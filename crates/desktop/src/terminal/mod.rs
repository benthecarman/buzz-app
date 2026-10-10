//! Native terminal capability. Owner IDs are lifecycle fences, not a plugin sandbox.
//! No remote shell endpoint, signing capability or inherited host credentials.
//!
//! Every [`Terminals`] method may block on process teardown, so async callers
//! should run them on a blocking thread.
mod context;
#[cfg(all(test, unix))]
mod tests;
#[cfg(unix)]
mod unix;

pub use context::TerminalContext;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const MAX_OWNERS: usize = 32;
#[cfg(unix)]
const MAX_SESSIONS: usize = 20;
#[cfg(unix)]
const MAX_BYTES: usize = 64 * 1024;
/// Largest input a session queues for the shell. Callers can reject bigger
/// writes early, before moving them to a blocking thread.
pub const MAX_PENDING_INPUT: usize = 1024 * 1024;
const UNSUPPORTED: &str = "Local terminals require macOS or Linux";

/// Output drained by one [`Terminals::read`], capped at 64 KiB per call.
#[derive(Debug, Serialize)]
pub struct ReadResult {
    /// Raw PTY bytes, not necessarily ending on a UTF-8 boundary.
    pub data: Vec<u8>,
    /// Set once all output is drained and the shell has exited.
    pub exited: bool,
}

#[cfg(unix)]
type Session = unix::Session;
#[cfg(not(unix))]
struct Session;

#[derive(Default)]
struct Registry {
    stopped: bool,
    owners: HashMap<String, Owner>,
}

#[derive(Default)]
struct Owner {
    closed: bool,
    sessions: HashMap<String, Session>,
}

/// Registry of PTY sessions grouped by owner. Clones share one registry.
#[derive(Clone, Default)]
pub struct Terminals(Arc<Mutex<Registry>>);
impl Terminals {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Registry>, String> {
        self.0
            .lock()
            .map_err(|_| "Terminal registry unavailable".into())
    }

    /// Registers an owner, such as one plugin view, and returns its ID.
    /// [`Terminals::close_owner`] later ends all of its sessions at once.
    pub fn create_owner(&self) -> Result<String, String> {
        if !cfg!(unix) {
            return Err(UNSUPPORTED.into());
        }
        let mut state = self.lock()?;
        if state.stopped {
            return Err("Terminal service has stopped".into());
        }
        if state.owners.len() >= MAX_OWNERS {
            return Err("Too many terminal owners".into());
        }
        let id = Uuid::new_v4().to_string();
        state.owners.insert(id.clone(), Owner::default());
        Ok(id)
    }

    /// Starts the user's login shell for `owner` with only `context` and a
    /// fixed environment, and returns the session ID.
    pub fn spawn(
        &self,
        owner: &str,
        context: TerminalContext,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        context.validate()?;
        dimensions(cols, rows)?;
        #[cfg(unix)]
        {
            self.spawn_with(owner, |id| unix::command(&context, id), cols, rows)
        }
        #[cfg(not(unix))]
        {
            let _ = owner;
            Err(UNSUPPORTED.into())
        }
    }

    /// Flushes queued input, then drains the output that is ready without
    /// blocking.
    pub fn read(&self, owner: &str, id: &str) -> Result<ReadResult, String> {
        #[cfg(unix)]
        {
            self.with_session(owner, id, Session::read)
        }
        #[cfg(not(unix))]
        {
            let _ = (owner, id);
            Err(UNSUPPORTED.into())
        }
    }

    /// Queues `data` for the shell and writes as much as it accepts now.
    /// [`Terminals::read`] flushes the rest.
    pub fn write(&self, owner: &str, id: &str, data: &[u8]) -> Result<(), String> {
        #[cfg(unix)]
        {
            self.with_session(owner, id, |session| session.write(data))
        }
        #[cfg(not(unix))]
        {
            let _ = (owner, id, data);
            Err(UNSUPPORTED.into())
        }
    }

    /// Resizes the session's PTY, which signals the shell with SIGWINCH.
    pub fn resize(&self, owner: &str, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        #[cfg(unix)]
        {
            self.with_session(owner, id, |session| session.resize(cols, rows))
        }
        #[cfg(not(unix))]
        {
            let _ = (owner, id, cols, rows);
            Err(UNSUPPORTED.into())
        }
    }

    #[cfg(unix)]
    fn spawn_with(
        &self,
        owner: &str,
        command: impl FnOnce(&str) -> Result<portable_pty::CommandBuilder, String>,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        dimensions(cols, rows)?;
        // The lock covers admission, creation and insertion. close_owner cannot
        // return before an admitted spawn is accounted for and terminated, and
        // an already revoked owner cannot create another process.
        let mut state = self.lock()?;
        if !state.owners.get(owner).is_some_and(|owner| !owner.closed) {
            return Err("Terminal owner has closed".into());
        }
        if state
            .owners
            .values()
            .map(|owner| owner.sessions.len())
            .sum::<usize>()
            >= MAX_SESSIONS
        {
            return Err("Too many terminal sessions (maximum 20)".into());
        }
        let id = Uuid::new_v4().to_string();
        let session = Session::spawn(command(&id)?, cols, rows)?;
        state
            .owners
            .get_mut(owner)
            .ok_or("Terminal owner has closed")?
            .sessions
            .insert(id.clone(), session);
        Ok(id)
    }

    #[cfg(unix)]
    fn with_session<T>(
        &self,
        owner: &str,
        id: &str,
        run: impl FnOnce(&mut Session) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self.lock()?;
        let session = state
            .owners
            .get_mut(owner)
            .filter(|owner| !owner.closed)
            .and_then(|owner| owner.sessions.get_mut(id))
            .ok_or("Terminal session is unavailable for this owner")?;
        run(session)
    }

    /// Terminates one session and its process groups. Unknown IDs succeed.
    pub fn close(&self, owner: &str, id: &str) -> Result<(), String> {
        let mut state = self.lock()?;
        if let Some(owner) = state.owners.get_mut(owner) {
            // Keep the handle on failure so an explicit retry still owns cleanup.
            if let Some(session) = owner.sessions.get_mut(id) {
                stop(session)?;
            }
            owner.sessions.remove(id);
        }
        Ok(())
    }

    /// Revokes `owner`, then terminates all of its sessions. Unknown owners
    /// succeed.
    pub fn close_owner(&self, owner: &str) -> Result<(), String> {
        let mut state = self.lock()?;
        if let Some(entry) = state.owners.get_mut(owner) {
            entry.closed = true; // revoke before cleanup, even when cleanup fails
            shutdown_sessions(&mut entry.sessions)?;
            state.owners.remove(owner);
        }
        Ok(())
    }

    /// Stops the service on app exit: refuses new owners and terminates every
    /// session.
    pub fn shutdown(&self) -> Result<(), String> {
        let mut state = self.lock()?;
        state.stopped = true;
        let mut result = Ok(());
        for owner in state.owners.values_mut() {
            owner.closed = true;
            if let Err(error) = shutdown_sessions(&mut owner.sessions) {
                result = Err(error);
            }
        }
        state.owners.retain(|_, owner| !owner.sessions.is_empty());
        result
    }
}

fn stop(session: &mut Session) -> Result<(), String> {
    #[cfg(unix)]
    {
        session.shutdown()
    }
    #[cfg(not(unix))]
    {
        let _ = session;
        Ok(())
    }
}

fn shutdown_sessions(sessions: &mut HashMap<String, Session>) -> Result<(), String> {
    let mut result = Ok(());
    sessions.retain(|_, session| match stop(session) {
        Ok(()) => false,
        Err(error) => {
            result = Err(error);
            true
        }
    });
    result
}

/// Checks a PTY size. Callers can reject bad sizes early, before moving the
/// request to a blocking thread.
pub fn dimensions(cols: u16, rows: u16) -> Result<(), String> {
    if !(2..=500).contains(&cols) || !(1..=300).contains(&rows) {
        Err("Terminal dimensions must be 2–500 columns and 1–300 rows".into())
    } else {
        Ok(())
    }
}
