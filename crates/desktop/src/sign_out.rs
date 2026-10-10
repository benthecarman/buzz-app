//! Sign out of Buzz. The command only records intent in a marker outside app
//! data and restarts, which stops agents. The next launch, holding alone the lock for
//! its key (and, for a wipe, the lock every Buzz shares), does every deletion before any window, service or identity read:
//! agent keys (when asked), the wipe, then the human key. Each step is safe to
//! repeat; any failure keeps the marker and Buzz exits, so the next launch retries.
//! An erase also waits for every agent's supervisor to let go of its ownership lock.
//! Finishing first advances a record beside the marker that is never removed, so
//! a running copy can tell a sign-out finished while it let go of its locks.
//!
//! The caller supplies the key store and what needs app state: removing keys,
//! closing signing, stopping agents, and quitting.
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FAILED: &str = "Sign out of Buzz didn't finish. Open Buzz again to retry.";
const BUSY: &str = "Another Buzz window using this sign-in is still open. Quit it, then open Buzz again to finish signing out.";
const BUSY_ALL: &str =
    "Another Buzz window is still open. Quit every Buzz window, then open Buzz again to finish erasing.";
const OTHERS: &str =
    "Quit every other Buzz window using this sign-in, then sign out again. Nothing was removed.";
const OTHERS_ALL: &str =
    "Quit every other Buzz window, then sign out and erase again. Nothing was removed.";
const SIGNING_OUT: &str = "Buzz is signing out in another window. Open Buzz again in a moment.";
const NOT_PREPARED: &str = "Couldn't prepare sign out; nothing was removed. Try again.";
/// Shown when Buzz quits because a sign-out left it unable to continue.
pub const FENCED: &str = "Buzz had to quit to sign out safely. Open Buzz again to continue.";
const AGENT_STOPPING: &str =
    "Erasing didn't finish because an agent is still stopping; nothing was removed. Open Buzz again to retry.";
const ALREADY: &str = "Buzz is already signing out.";
const LINKED: &str = "Erasing is unavailable because a Buzz storage folder is a link or couldn't be checked; nothing was removed";
const DEV_WIPE: &str = "Erasing is unavailable in development builds because they share agent keys and plugin storage with the installed Buzz";
// TEMPORARY agents rename: remove after 2026-10-18; see the migration module.
pub mod migrate_agents;
/// App data folder holding the current Agents identities.
pub const AGENTS: &str = "agents";
pub const OLD_AGENTS: &str = "agent-controller";
/// One lock per running agent, held by its supervisor until the agent has
/// stopped, even after Buzz exits. Shared by every Buzz, under the user data folder.
pub const AGENT_OWNERSHIP: &str = "dev.local.buzz.agent-ownership";
/// What kept agents need to be identified and start again, by app data folder:
/// the Agents list, and the earlier system's list and defaults to port from.
/// Their keys live in the keychain. Anything else in these folders (hosting
/// locks, saved logins, logs, run folders, and whatever is added later) is wiped.
const KEPT: Kept = &[
    (AGENTS, &["identities.json"]),
    (migrate_agents::LEGACY, &["identities.json"]),
    (OLD_AGENTS, &["agents.json", "defaults.json"]),
];
/// The children a wiped folder keeps, each with the files it keeps.
type Kept = &'static [(&'static str, &'static [&'static str])];
/// Held shared by every Buzz and alone only to wipe. Named for the storage every
/// Buzz shares whatever its identifier: the default plugin folder and the fixed
/// agent key service. It is the release app's name too.
const LOCK: &str = ".dev.local.buzz.foundation.instance.lock";
/// How long a launch waits for an exiting or signing-out instance to let go.
const WAIT: Duration = if cfg!(test) {
    Duration::from_secs(1)
} else {
    Duration::from_secs(10)
};

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Choices {
    wipe: bool,
    remove_agents: bool,
}

/// The marker, the shared instance lock and the folders a wipe clears, resolved like Tauri's app paths.
pub struct Paths {
    marker: PathBuf,
    /// Grows by one byte each time a sign-out of this key store finishes. Named,
    /// like the key lock, for the key store alone: copies with any identifier
    /// share the key, and the record carries no choices.
    finished: PathBuf,
    lock: PathBuf,
    /// Held shared by every Buzz using this human key store, alone to sign out of it.
    key_lock: PathBuf,
    /// Agent ownership locks; an erase waits until none is held.
    ownership: PathBuf,
    app_data: PathBuf,
    /// Other app-owned folders: local data and WebView storage, caches.
    others: Vec<PathBuf>,
}
/// One marker per key store, so a debug launch never acts on a release sign-out.
fn marker_name(identifier: &str, service: &str) -> String {
    format!(".{identifier}--{service}.sign-out-pending")
}
fn finished_name(service: &str) -> String {
    format!(".{service}.sign-outs-finished")
}
/// How many sign-outs of this key store have finished; absent is none.
fn finished(path: &Path) -> std::io::Result<u64> {
    match fs::metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        other => other.map(|meta| meta.len()),
    }
}
/// Named for the key store alone: copies with different identifiers can share a key.
fn key_lock_name(service: &str) -> String {
    format!(".{service}.instance.lock")
}
impl Paths {
    /// The paths for the app `identifier` signed in with the key store
    /// `service`; `None` when the platform has no user data folders.
    pub fn resolve(identifier: &str, service: &str) -> Option<Self> {
        let data = dirs::data_dir()?;
        let app_data = data.join(identifier);
        let mut others = vec![
            dirs::data_local_dir()?.join(identifier),
            dirs::cache_dir()?.join(identifier),
        ];
        #[cfg(target_os = "macos")]
        others.push(dirs::home_dir()?.join("Library/WebKit").join(identifier));
        others.retain(|path| *path != app_data);
        others.dedup();
        Some(Self {
            marker: data.join(marker_name(identifier, service)),
            finished: data.join(finished_name(service)),
            lock: data.join(LOCK),
            key_lock: data.join(key_lock_name(service)),
            ownership: data.join(AGENT_OWNERSHIP),
            app_data,
            others,
        })
    }
    /// Each wiped folder, and the children it keeps (the agent registries) unless agents go too.
    fn targets(&self, choices: Choices) -> Vec<(&Path, Kept)> {
        let kept = if choices.remove_agents { &[] } else { KEPT };
        std::iter::once((self.app_data.as_path(), kept))
            .chain(self.others.iter().map(|path| (path.as_path(), &[] as Kept)))
            .collect()
    }
    /// The wipe never deletes through a link: every folder it moves, opens or
    /// removes, its trash and the kept registries must be a real folder or absent.
    fn unlinked(&self, choices: Choices) -> std::io::Result<()> {
        if choices.remove_agents {
            for (registry, _) in KEPT {
                real_dir(&self.app_data.join(registry))?;
            }
        }
        if !choices.wipe {
            return Ok(());
        }
        for (path, kept) in self.targets(choices) {
            for path in [path.to_owned(), trash(path)] {
                if real_dir(&path)? {
                    for (child, _) in kept {
                        real_dir(&path.join(child))?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Whether `path` is a real folder; absent is `false`. A link, anything else
/// or failing to look is an error, never absence.
fn real_dir(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(std::io::Error::other(format!(
            "{} is a link or not a folder",
            path.display()
        ))),
    }
}

/// Every running Buzz holds both locks shared. Signing out needs this key's lock
/// alone, so no copy using the key runs; a wipe also needs the all-Buzz lock
/// alone, so no copy using shared storage runs. Locks are taken key first, then
/// all-Buzz, and released in reverse, never waiting while holding one alone.
/// `started` admits one sign-out per instance: after it commits, Buzz restarts
/// or exits, so it is never cleared. `seen` is the finished record at launch: a
/// change means a sign-out finished while this instance let go of its locks.
pub struct Instance {
    locks: Mutex<Locks>,
    marker: PathBuf,
    finished: PathBuf,
    seen: u64,
    started: AtomicBool,
}
struct Locks {
    key: File,
    all: File,
}
// `File` locking is Rust 1.89; credential-store and plugin-manager already need it.
#[allow(clippy::incompatible_msrv)]
impl Instance {
    /// Admit this instance's one sign-out, holding the locks it needs alone.
    fn begin(&self, choices: Choices) -> Result<(), Failure> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(refuse(ALREADY));
        }
        let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = locks.key.unlock() {
            return Err(lost(error));
        }
        if locks.key.try_lock().is_err() {
            handoff();
            return Err(self.share(&locks, false, OTHERS));
        }
        if !choices.wipe {
            return Ok(());
        }
        if let Err(error) = locks.all.unlock() {
            return Err(lost(error));
        }
        if locks.all.try_lock().is_ok() {
            return Ok(());
        }
        if let Err(error) = locks.key.unlock() {
            return Err(lost(error));
        }
        handoff();
        Err(self.share(&locks, true, OTHERS_ALL))
    }
    /// Withdraw a sign-out that committed nothing, returning why.
    fn abort(&self, choices: Choices, failure: Failure) -> Failure {
        let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        let released = if choices.wipe {
            locks.all.unlock()
        } else {
            Ok(())
        };
        match released.and_then(|()| locks.key.unlock()) {
            Ok(()) => self.share(&locks, choices.wipe, &failure.message),
            Err(error) => lost(error),
        }
    }
    /// Share the unlocked key lock again, and the all-Buzz lock when `all` was
    /// let go too. Another process may have committed, or even finished, a
    /// sign-out meanwhile; only with neither can this instance carry on.
    fn share(&self, locks: &Locks, all: bool, message: &str) -> Failure {
        // Bounded: a stuck owner must not hang the refusal; not sharing again means exiting.
        if !wait_for(|| locks.key.try_lock_shared())
            || (all && !wait_for(|| locks.all.try_lock_shared()))
        {
            return fenced();
        }
        let changed = self
            .marker
            .try_exists()
            .and_then(|pending| Ok(pending || finished(&self.finished)? != self.seen));
        match changed {
            Ok(false) => {
                self.started.store(false, Ordering::SeqCst);
                refuse(message)
            }
            Ok(true) => fenced(),
            Err(error) => lost(error),
        }
    }
}
fn lost(error: std::io::Error) -> Failure {
    eprintln!("buzz: instance lock: {error}");
    fenced()
}
// Test seam: runs wherever ownership is briefly let go, and after the first
// failed attempt to take it while waiting.
#[cfg(test)]
thread_local!(static HANDOFF: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default());
fn handoff() {
    #[cfg(test)]
    if let Some(gap) = HANDOFF.take() {
        gap();
    }
}
fn wait_for<E>(mut attempt: impl FnMut() -> Result<(), E>) -> bool {
    let start = Instant::now();
    loop {
        if attempt().is_ok() {
            return true;
        }
        if start.elapsed() >= WAIT {
            return false;
        }
        handoff();
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run at launch, before anything else. Returns this instance's lock, or the
/// message to show before exiting.
/// `remove_agent_keys` deletes the keys of the agents in the registry folder
/// it is given, and `remove_key` the human key; both succeed when absent.
#[allow(clippy::incompatible_msrv)] // `File` locking; see `Instance`.
pub fn boot(
    paths: &Paths,
    mut remove_agent_keys: impl FnMut(&Path) -> Result<(), String>,
    mut remove_key: impl FnMut() -> Result<(), String>,
) -> Result<Instance, String> {
    let open = |path: &Path| {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|error| {
                eprintln!("buzz: could not open instance lock: {error}");
                FAILED.to_owned()
            })
    };
    // A first launch may find no user-data folder yet; the locks live in it.
    if let Some(data) = paths.lock.parent() {
        fs::create_dir_all(data).map_err(|error| {
            eprintln!("buzz: could not create user data folder: {error}");
            FAILED.to_owned()
        })?;
    }
    let locks = Locks {
        key: open(&paths.key_lock)?,
        all: open(&paths.lock)?,
    };
    let failed = |error: std::io::Error| {
        eprintln!("buzz: instance lock: {error}");
        FAILED.to_owned()
    };
    // Shared first, then look: a sign-out that commits after this sees our locks.
    // Every time they are taken again, look again.
    loop {
        if !wait_for(|| locks.key.try_lock_shared()) || !wait_for(|| locks.all.try_lock_shared()) {
            return Err(SIGNING_OUT.into());
        }
        let Some(wipe) = pending_wipe(&paths.marker).map_err(failed)? else {
            break;
        };
        // A restarting instance may still be exiting; wait for it to let go.
        locks.all.unlock().map_err(failed)?;
        locks.key.unlock().map_err(failed)?;
        let mut busy = BUSY;
        let mut stuck = None;
        let taken = wait_for(|| {
            busy = BUSY;
            locks.key.try_lock().map_err(drop)?;
            if wipe && locks.all.try_lock().is_err() {
                busy = BUSY_ALL;
                // Never wait holding the key lock exclusively: if it can't be
                // let go, stop trying (`Ok` ends the wait) and fail below.
                return match locks.key.unlock() {
                    Ok(()) => Err(()),
                    Err(error) => {
                        stuck = Some(error);
                        Ok(())
                    }
                };
            }
            Ok(())
        });
        if let Some(error) = stuck {
            return Err(failed(error));
        }
        if !taken {
            return Err(busy.into());
        }
        // Read again under exclusive ownership and run only what these locks
        // cover: another launch may have finished it or committed a wipe since.
        match pending(&paths.marker)? {
            // Now a wipe: let go and loop to take every lock.
            Some(choices) if choices.wipe && !wipe => {}
            Some(choices) => {
                if choices.wipe {
                    agents_stopped(&paths.ownership)?;
                }
                finish(paths, choices, &mut remove_agent_keys, &mut remove_key).map_err(
                    |error| {
                        eprintln!("buzz: sign out did not finish: {error}");
                        FAILED.to_owned()
                    },
                )?
            }
            None => {}
        }
        if wipe {
            locks.all.unlock().map_err(failed)?;
        }
        locks.key.unlock().map_err(failed)?;
        handoff();
    }
    migrate_agents::migrate(paths, &locks)?;
    Ok(Instance {
        locks: Mutex::new(locks),
        marker: paths.marker.clone(),
        finished: paths.finished.clone(),
        seen: finished(&paths.finished).map_err(failed)?,
        started: AtomicBool::new(false),
    })
}

/// An erase deletes storage agents use, so it waits until no supervisor holds an
/// agent's ownership lock. A plain sign-out removes only the human key and skips
/// this: the folder is shared with every Buzz, so another copy's agents would block it.
#[allow(clippy::incompatible_msrv)] // `File` locking; see `Instance`.
fn agents_stopped(ownership: &Path) -> Result<(), String> {
    let failed = |error: std::io::Error| {
        eprintln!("buzz: agent ownership: {error}");
        FAILED.to_owned()
    };
    let entries = match fs::read_dir(ownership) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        other => other.map_err(failed)?,
    };
    for entry in entries {
        let path = entry.map_err(failed)?.path();
        if path.extension() != Some("lock".as_ref()) {
            continue;
        }
        // Closing the file releases a lock taken here.
        let file = File::open(&path).map_err(failed)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(AGENT_STOPPING.into()),
            Err(fs::TryLockError::Error(error)) => return Err(failed(error)),
        }
    }
    Ok(())
}

/// Whether a pending sign-out wipes, to choose the locks; `None` when none is
/// pending. Malformed JSON counts as a wipe, so every lock is taken before
/// `pending` reports it; a read error returns at once and nothing is deleted.
fn pending_wipe(marker: &Path) -> std::io::Result<Option<bool>> {
    match fs::read(marker) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(raw) => Ok(Some(
            serde_json::from_slice::<Choices>(&raw).map_or(true, |choices| choices.wipe),
        )),
    }
}

fn trash(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".sign-out-trash");
    path.with_file_name(name)
}

fn missing(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Move `path` to its trash name, then return its kept children. Resumes an
/// earlier attempt that stopped part-way.
fn move_aside(path: &Path, kept: Kept) -> std::io::Result<()> {
    let moved = trash(path);
    if !real_dir(&moved)? {
        real_dir(path)?;
        missing(fs::rename(path, &moved))?;
    }
    for (child, _) in kept {
        if real_dir(&moved.join(child))? && !(real_dir(path)? && real_dir(&path.join(child))?) {
            fs::create_dir_all(path)?;
            fs::rename(moved.join(child), path.join(child))?;
        }
    }
    Ok(())
}

fn move_back(path: &Path, kept: Kept) -> std::io::Result<()> {
    let moved = trash(path);
    if !real_dir(&moved)? {
        return Ok(());
    }
    for (child, _) in kept {
        if real_dir(path)? && real_dir(&path.join(child))? {
            real_dir(&moved.join(child))?;
            fs::rename(path.join(child), moved.join(child))?;
        }
    }
    // Only the kept children were returned, so anything else here is unexpected; keep it.
    if real_dir(path)? {
        missing(fs::remove_dir(path))?;
    }
    fs::rename(moved, path)
}

/// Delete `path` except its kept children's kept files, whether it is the
/// original or was recreated.
fn clear(path: &Path, kept: Kept) -> std::io::Result<()> {
    if kept.is_empty() {
        return remove_real_dir(path);
    }
    let children: Vec<&str> = kept.iter().map(|(child, _)| *child).collect();
    clear_except(path, &children)?;
    for (child, files) in kept {
        clear_except(&path.join(child), files)?;
    }
    Ok(())
}

/// Delete a real folder; absent is done, and a link is refused, never followed or unlinked.
fn remove_real_dir(path: &Path) -> std::io::Result<()> {
    if !real_dir(path)? {
        return Ok(());
    }
    missing(fs::remove_dir_all(path))
}

/// Delete `path`'s trash, refusing a link at the trash or a kept child.
fn remove_trash(path: &Path, kept: Kept) -> std::io::Result<()> {
    let moved = trash(path);
    if real_dir(&moved)? {
        for (child, _) in kept {
            real_dir(&moved.join(child))?;
        }
    }
    remove_real_dir(&moved)
}

fn clear_except(path: &Path, keep: &[&str]) -> std::io::Result<()> {
    if !real_dir(path)? {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if keep.iter().any(|name| entry.file_name() == *name) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The pending sign-out's choices, if any. Unreadable or malformed is the
/// message shown before exiting, never absence.
fn pending(marker: &Path) -> Result<Option<Choices>, String> {
    let raw = match fs::read(marker) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        other => other,
    };
    raw.map_err(|error| error.to_string())
        .and_then(|raw| serde_json::from_slice(&raw).map_err(|e| e.to_string()))
        .map(Some)
        .map_err(|error| {
            eprintln!("buzz: sign out did not finish: {error}");
            FAILED.to_owned()
        })
}

fn finish(
    paths: &Paths,
    choices: Choices,
    remove_agent_keys: impl FnOnce(&Path) -> Result<(), String>,
    remove_key: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    // Agent keys go first, while their registries in app data are still in
    // place; nothing is moved until every key is gone, so a registry confirmed
    // absent means it's done. Failing to look is not absence: stop before moving anything.
    paths
        .unlinked(choices)
        .map_err(|error| format!("check wipe folders: {error}"))?;
    if choices.remove_agents {
        remove_agent_keys(&paths.app_data)
            .map_err(|error| format!("remove agent keys: {error}"))?;
    }
    let targets = if choices.wipe {
        paths.targets(choices)
    } else {
        Vec::new()
    };
    let mut moved = Vec::new();
    let mut outcome = Ok(());
    for &(path, kept) in &targets {
        moved.push((path, kept));
        if let Err(error) = move_aside(path, kept) {
            outcome = Err(format!("move {} aside: {error}", path.display()));
            break;
        }
    }
    if outcome.is_ok() {
        // Idempotent: an absent key is success, so a resumed attempt passes here.
        outcome = remove_key();
    }
    if let Err(error) = outcome {
        // Partially moved folders go back; the marker stays so the next launch retries.
        for &(path, kept) in moved.iter().rev() {
            if let Err(undo) = move_back(path, kept) {
                eprintln!("buzz: could not restore {}: {undo}", path.display());
            }
        }
        return Err(error);
    }
    // The key is gone, so nothing is rolled back from here: clear what was moved
    // aside and anything in place now, then the marker.
    for (path, kept) in targets {
        remove_trash(path, kept)
            .and_then(|()| clear(path, kept))
            .map_err(|error| format!("delete wiped {}: {error}", path.display()))?;
    }
    // Record it before the marker goes; a resumed attempt records it again.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.finished)
        .and_then(|mut record| std::io::Write::write_all(&mut record, b"."))
        .map_err(|error| format!("record finished: {error}"))?;
    fs::remove_file(&paths.marker).map_err(|error| format!("remove marker: {error}"))
}

fn write_marker(path: &Path, choices: Choices) -> std::io::Result<()> {
    let staged = path.with_extension("staged");
    fs::write(&staged, serde_json::to_vec(&choices)?)?;
    fs::rename(&staged, path)
}

/// A refusal the dialog shows. `fenced` means this instance can't continue (its
/// locks or agents are in an unknown state): Buzz exits natively instead, and
/// it never reaches the dialog.
#[derive(Debug, PartialEq, Serialize)]
pub struct Failure {
    message: String,
    #[serde(skip)]
    fenced: bool,
}
fn refuse(message: &str) -> Failure {
    Failure {
        message: message.into(),
        fenced: false,
    }
}
fn fenced() -> Failure {
    Failure {
        message: FENCED.into(),
        fenced: true,
    }
}

/// Commit intent, close signing, then run `shutdown`. Deleting is left to the next launch.
async fn prepare<S>(
    paths: &Paths,
    choices: Choices,
    close: impl FnOnce(),
    shutdown: impl FnOnce() -> S,
) -> Result<(), Failure>
where
    S: Future<Output = Result<(), String>>,
{
    if let Err(error) = paths.unlinked(choices) {
        eprintln!("buzz: sign out refused: {error}");
        return Err(refuse(LINKED));
    }
    write_marker(&paths.marker, choices).map_err(|error| {
        eprintln!("buzz: could not write sign-out marker: {error}");
        refuse(NOT_PREPARED)
    })?;
    // Committed: the key is removed at the next launch, so it signs or pairs nothing more.
    close();
    shutdown().await.map_err(|error| {
        eprintln!("buzz: agents did not stop: {error}");
        fenced()
    })
}

/// Admit, commit and shut down. A refusal before the marker withdraws; any
/// fenced failure, before or after it, closes signing and goes to `exit`, never
/// to the dialog.
async fn attempt<S, E>(
    instance: &Instance,
    paths: &Paths,
    choices: Choices,
    close: impl Fn(),
    shutdown: impl FnOnce() -> S,
    exit: impl FnOnce() -> E,
) -> Result<(), Failure>
where
    S: Future<Output = Result<(), String>>,
    E: Future<Output = Result<(), Failure>>,
{
    let result = match instance.begin(choices) {
        Ok(()) => prepare(paths, choices, &close, shutdown)
            .await
            .map_err(|failure| {
                if failure.fenced {
                    failure
                } else {
                    instance.abort(choices, failure)
                }
            }),
        Err(failure) => Err(failure),
    };
    match result {
        Err(failure) if failure.fenced => {
            close();
            exit().await
        }
        other => other,
    }
}

/// Why this build can't sign out, if it can't.
fn refusal(
    debug: bool,
    dev_viewer: bool,
    plugin_home: bool,
    wipe: bool,
    remove_agents: bool,
) -> Option<&'static str> {
    if dev_viewer {
        Some("Sign out is unavailable while the development broker supplies your identity")
    } else if remove_agents && !wipe {
        Some("Removing agents is part of erasing this device")
    } else if wipe && debug {
        // Debug builds have their own human key, but share agent keys and plugin
        // storage with the installed app, so only plain sign-out is safe.
        Some(DEV_WIPE)
    } else if wipe && plugin_home {
        Some("Erasing is unavailable while BUZZODZ_HOME moves plugin storage")
    } else {
        None
    }
}

/// Why this build can't sign out with these choices, from its build and environment.
fn current_refusal(wipe: bool, remove_agents: bool) -> Option<&'static str> {
    let set = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    refusal(
        cfg!(debug_assertions),
        set("BUZZ_DEV_VIEWER"),
        set("BUZZODZ_HOME"),
        wipe,
        remove_agents,
    )
}

/// Why wipe is unavailable here, if it is, so the dialog can say so up front;
/// `sign_out` still enforces it.
pub fn wipe_refusal() -> Option<&'static str> {
    current_refusal(true, false)
}

/// Records the choices, closes signing and stops agents; the caller restarts
/// on `Ok`, and the next launch deletes. `storage` is `None` when this app's
/// paths or [`Instance`] are unknown. `close` stops signing and pairing with
/// the human key, `shutdown` stops agents and model work, and `exit` quits as
/// Quit does after showing [`FENCED`]; it is called when this instance cannot
/// continue.
pub async fn sign_out<S, E>(
    storage: Option<(&Paths, &Instance)>,
    wipe: bool,
    remove_agents: bool,
    close: impl Fn(),
    shutdown: impl FnOnce() -> S,
    exit: impl FnOnce() -> E,
) -> Result<(), Failure>
where
    S: Future<Output = Result<(), String>>,
    E: Future<Output = Result<(), Failure>>,
{
    if let Some(message) = current_refusal(wipe, remove_agents) {
        return Err(refuse(message));
    }
    let Some((paths, instance)) = storage else {
        return Err(refuse(
            "Couldn't find this app's local storage; nothing was removed",
        ));
    };
    let choices = Choices {
        wipe,
        remove_agents,
    };
    attempt(instance, paths, choices, close, shutdown, exit).await
}

#[cfg(test)]
mod tests;
