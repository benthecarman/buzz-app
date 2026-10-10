//! Unix PTY ownership. Nonblocking I/O keeps both output and queued input bounded.
use super::{ReadResult, TerminalContext, MAX_BYTES, MAX_PENDING_INPUT};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

fn error() -> String {
    io::Error::last_os_error().to_string()
}

pub(super) fn command(context: &TerminalContext, id: &str) -> Result<CommandBuilder, String> {
    context.validate()?;
    let shell = resolve_shell(std::env::var("SHELL").ok().as_deref());
    let mut cmd = CommandBuilder::new_default_prog();
    // This is the production spawn fence, not a blacklist of known credentials.
    cmd.env_clear();
    for key in [
        "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("SHELL", shell);
    cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("TERM", "xterm-256color");
    cmd.env("TERM_PROGRAM", "Buzz");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("BUZZ_CHANNEL_ID", &context.channel_id);
    cmd.env("BUZZ_CHANNEL", context.display());
    cmd.env("BUZZ_NPUB", &context.npub);
    cmd.env("BUZZ_RELAY_URL", &context.relay_url);
    cmd.env("BUZZ_TERM_SESSION", id);
    cmd.env("BUZZ_TERM_VERSION", env!("CARGO_PKG_VERSION"));
    if let Some(thread) = &context.thread_id {
        cmd.env("BUZZ_THREAD_ID", thread);
    }
    Ok(cmd)
}

fn executable(path: &str) -> bool {
    let path = Path::new(path);
    if !path.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a valid NUL-terminated path, read only by access.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

pub(super) fn resolve_shell(candidate: Option<&str>) -> String {
    candidate
        .filter(|shell| executable(shell))
        .map(str::to_owned)
        .or_else(passwd_shell)
        .filter(|shell| executable(shell))
        .unwrap_or_else(|| "/bin/sh".into())
}

fn passwd_shell() -> Option<String> {
    // Reentrant lookup: native commands can resolve shells on concurrent threads.
    let mut buffer = vec![0u8; 16 * 1024];
    let mut result = std::ptr::null_mut();
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    // SAFETY: entry/buffer/result are writable for the stated lengths. Returned
    // pointers refer to buffer; copy the shell before buffer is dropped.
    unsafe {
        if libc::getpwuid_r(
            libc::getuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        ) != 0
            || result.is_null()
        {
            return None;
        }
        let shell = (*result).pw_shell;
        if shell.is_null() {
            return None;
        }
        CStr::from_ptr(shell).to_str().ok().map(str::to_owned)
    }
}

pub(super) struct Session {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pid: libc::pid_t,
    input: VecDeque<u8>,
    eof: bool,
    reaped: bool,
}
impl Session {
    pub(super) fn spawn(command: CommandBuilder, cols: u16, rows: u16) -> Result<Self, String> {
        let pair = native_pty_system()
            .openpty(size(cols, rows))
            .map_err(|e| e.to_string())?;
        let fd = pair
            .master
            .as_raw_fd()
            .ok_or("PTY has no Unix descriptor")?;
        // SAFETY: fd is owned by the live master. O_NONBLOCK applies to the
        // master's file description only, not the slave's interactive streams.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(error());
            }
        }
        // All fallible fd setup precedes spawn. From here the session owns reap.
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| e.to_string())?;
        drop(pair.slave);
        let Some(pid) = child
            .process_id()
            .and_then(|p| i32::try_from(p).ok())
            .filter(|p| *p > 1)
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("PTY child has no process identity".into());
        };
        Ok(Self {
            master: pair.master,
            child,
            pid,
            input: VecDeque::new(),
            eof: false,
            reaped: false,
        })
    }

    fn fd(&self) -> Result<i32, String> {
        self.master
            .as_raw_fd()
            .ok_or_else(|| "PTY is closed".into())
    }

    fn drain(&mut self) -> Result<Vec<u8>, String> {
        let mut data = Vec::new();
        if self.eof {
            return Ok(data);
        }
        let fd = self.fd()?;
        let mut buffer = [0u8; 8192];
        while data.len() < MAX_BYTES {
            let amount = buffer.len().min(MAX_BYTES - data.len());
            // SAFETY: live owned fd, writable buffer with amount <= its length.
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), amount) };
            if count > 0 {
                data.extend_from_slice(&buffer[..count as usize]);
            } else if count == 0 {
                self.eof = true;
                break;
            } else {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Linux signals the closed slave with EIO; Darwin returns EOF.
                if err.raw_os_error() == Some(libc::EIO) {
                    self.eof = true;
                    break;
                }
                return Err(err.to_string());
            }
        }
        Ok(data)
    }

    fn flush_input(&mut self) -> Result<(), String> {
        if self.input.is_empty() {
            return Ok(());
        }
        let fd = self.fd()?;
        // Limit work in any one read/write IPC, even for a queued large paste.
        let mut budget = MAX_BYTES;
        while !self.input.is_empty() && budget > 0 {
            let bytes = self.input.as_slices().0;
            let amount = bytes.len().min(budget);
            // SAFETY: bytes is a valid slice; fd remains owned throughout the call.
            let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), amount) };
            if count > 0 {
                self.input.drain(..count as usize);
                budget -= count as usize;
            } else {
                let err = io::Error::last_os_error();
                if count < 0 && err.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if count < 0 && err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("Terminal input failed: {err}"));
            }
        }
        Ok(())
    }

    pub(super) fn write(&mut self, data: &[u8]) -> Result<(), String> {
        if data.len() > MAX_PENDING_INPUT {
            return Err("Terminal input exceeds 1 MiB".into());
        }
        if self.eof || self.reaped {
            return Err("Terminal has exited".into());
        }
        if self.input.len() + data.len() > MAX_PENDING_INPUT {
            return Err("Terminal input is full; wait for the shell to read it".into());
        }
        self.input.extend(data);
        self.flush_input()
    }

    pub(super) fn read(&mut self) -> Result<ReadResult, String> {
        // Flush before consuming output, so an input error cannot discard a
        // successfully read output chunk. A shell exiting mid-paste cancels the
        // remaining input but still has its final output drained normally.
        if exited(self.pid)? {
            self.input.clear();
        } else if let Err(error) = self.flush_input() {
            if exited(self.pid)? {
                self.input.clear();
            } else {
                return Err(error);
            }
        }
        let data = self.drain()?;
        let exited = self.eof && exited(self.pid)?;
        // Never report child status alone: a capped read may leave a large final
        // output behind. EOF and the unreaped child's exit are both required.
        Ok(ReadResult { data, exited })
    }

    pub(super) fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        super::dimensions(cols, rows)?;
        self.master
            .resize(size(cols, rows))
            .map_err(|e| e.to_string())
    }

    pub(super) fn foreground_job(&self) -> Option<libc::pid_t> {
        self.master
            .process_group_leader()
            .filter(|group| *group > 1 && *group != self.pid)
    }

    pub(super) fn shutdown(&mut self) -> Result<(), String> {
        if self.reaped {
            return Ok(());
        }
        self.input.clear();
        // Job-control shells put their foreground command in another group.
        // The tty owns that identity; signal it in addition to the shell group.
        let foreground = self.foreground_job();
        let pid = self.pid;
        let signal = |sig| -> Result<(), String> {
            if let Some(group) = foreground {
                // SAFETY: getsid only inspects a PID. Refuse a recycled/unrelated
                // foreground group; the unreaped session leader pins its session ID.
                if unsafe { libc::getsid(group) } == pid {
                    signal_group(group, sig).map_err(|e| e.to_string())?;
                }
            }
            match signal_group(pid, sig) {
                // Darwin returns EPERM for a group containing only an exited
                // zombie. We deliberately have not reaped it yet. A live leader
                // must never have its permission failure hidden by this case.
                Err(error) if error.raw_os_error() == Some(libc::EPERM) && exited(pid)? => Ok(()),
                result => result.map_err(|e| e.to_string()),
            }
        };
        signal(libc::SIGTERM)?;
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            // Drain while terminating: a full tty can delay the child's teardown.
            let _ = self.drain();
            if exited(self.pid)? {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Sweep even if the leader exited; its children may ignore TERM. Reap
        // only after this sweep, so the group ID cannot be reused beforehand.
        signal(libc::SIGKILL)?;
        let reap_deadline = Instant::now() + Duration::from_secs(2);
        while !exited(self.pid)? {
            if Instant::now() >= reap_deadline {
                return Err("Terminal termination has not completed; retry End session".into());
            }
            let _ = self.drain();
            std::thread::sleep(Duration::from_millis(1));
        }
        self.child.wait().map_err(|e| e.to_string())?;
        self.reaped = true;
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("Terminal cleanup failed: {error}");
        }
    }
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn signal_group(group: libc::pid_t, signal: i32) -> io::Result<()> {
    // SAFETY: positive owned group identity; negative PID targets the whole group.
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

fn exited(pid: libc::pid_t) -> Result<bool, String> {
    // WNOWAIT is essential: keep the child/group identity reserved until cleanup.
    // SAFETY: fully initialized writable siginfo; waitid only observes our child.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == 0
        {
            Ok(info.si_pid() == pid)
        } else {
            Err(error())
        }
    }
}
