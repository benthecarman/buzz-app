//! Bounded child processes and the `PATH` that desktop launches resolve
//! programs on.
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Windows job objects, which own a child and all of its descendants.
#[cfg(windows)]
pub mod windows_job {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use tokio::process::{Child, Command};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
    };

    /// A job that kills its processes when closed.
    pub struct WindowsJob(OwnedHandle);

    impl WindowsJob {
        pub fn new() -> Option<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    job.0.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            if configured == 0 {
                return None;
            }
            Some(job)
        }

        /// Termination is asynchronous; waits, bounded by `wait`, until no job process remains.
        pub async fn terminate(&self, wait: std::time::Duration) {
            unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
            let until = tokio::time::Instant::now() + wait;
            while self.active_processes() != Some(0) && tokio::time::Instant::now() < until {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }

        fn active_processes(&self) -> Option<u32> {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            let queried = unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            (queried != 0).then_some(info.ActiveProcesses)
        }

        pub fn assign(&self, child: &Child) -> bool {
            child.raw_handle().is_some_and(|handle| unsafe {
                AssignProcessToJobObject(self.0.as_raw_handle(), handle) != 0
            })
        }

        pub fn spawn(&self, command: &mut Command) -> Option<Child> {
            self.spawn_with_check(command, 0, |_, _| {})
        }

        pub fn spawn_hidden(&self, command: &mut Command) -> Option<Child> {
            self.spawn_with_check(command, CREATE_NO_WINDOW, |_, _| {})
        }

        pub fn spawn_with_check(
            &self,
            command: &mut Command,
            flags: u32,
            check: impl FnOnce(&Child, &OwnedHandle),
        ) -> Option<Child> {
            command
                .creation_flags(CREATE_SUSPENDED | flags)
                .kill_on_drop(true);
            let child = command.spawn().ok()?;
            let primary_thread = primary_thread(child.id()?)?;
            check(&child, &primary_thread);
            if !self.assign(&child) {
                return None;
            }
            if unsafe { ResumeThread(primary_thread.as_raw_handle()) } != 1 {
                return None;
            }
            Some(child)
        }
    }

    fn primary_thread(process_id: u32) -> Option<OwnedHandle> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        if unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } == 0 {
            return None;
        }
        let mut thread_id = None;
        loop {
            if entry.th32OwnerProcessID == process_id
                && thread_id.replace(entry.th32ThreadID).is_some()
            {
                return None;
            }
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            if unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } == 0 {
                break;
            }
        }
        if unsafe { GetLastError() } != ERROR_NO_MORE_FILES {
            return None;
        }
        let handle = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id?) };
        if handle.is_null() {
            return None;
        }
        Some(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

/// Kills a child's process group on drop while `armed`. Tokio kills only the
/// direct child on future cancellation; the group also owns descendants.
#[cfg(unix)]
pub struct ProcessGroupGuard {
    /// The group leader, started with `process_group(0)`.
    pub process_id: i32,
    /// Disarm once the child has been reaped, so a reused ID is never signalled.
    pub armed: bool,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    /// Sends `SIGKILL` to the whole group.
    pub fn kill(&self) {
        unsafe { libc::kill(-self.process_id, libc::SIGKILL) };
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
    }
}

/// The `PATH` for launching user tools. On macOS a GUI app inherits a
/// minimal `PATH`, so per-user and Homebrew directories are added.
pub fn effective_path() -> OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    #[cfg(target_os = "macos")]
    {
        let path = if path.is_empty() {
            OsString::from("/usr/bin:/bin")
        } else {
            path
        };
        // Per-user installs, such as Claude Code's official installer, come first:
        // an older system-wide copy in /usr/local/bin would otherwise run instead,
        // and installing again could not replace it.
        let mut directories = std::env::var_os("HOME")
            .map(|home| std::path::Path::new(&home).join(".local/bin"))
            .into_iter()
            .collect::<Vec<_>>();
        directories.extend(
            std::env::split_paths(&path).filter(|directory| !directory.as_os_str().is_empty()),
        );
        directories.extend(["/opt/homebrew/bin".into(), "/usr/local/bin".into()]);
        std::env::join_paths(directories).unwrap_or(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        path
    }
}

/// The program to execute: on macOS, its first executable match on
/// `effective_path`; elsewhere `program` unchanged, which the OS resolves.
pub fn resolve_program(program: &str, effective_path: &OsStr) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Some(executable) = std::env::split_paths(effective_path)
            .map(|path| path.join(program))
            .find(|path| {
                std::fs::metadata(path).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
        {
            return executable;
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = effective_path;
    PathBuf::from(program)
}
/// Bounded stdout and exit status; callers must project away private command output.
pub async fn run_output(
    executable: &Path,
    args: &[String],
    deadline: Duration,
    path: &OsStr,
    max_output_bytes: u64,
) -> Option<(String, std::process::ExitStatus)> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if !path.is_empty() {
        command.env("PATH", path);
    }
    #[cfg(unix)]
    command.as_std_mut().process_group(0);

    #[cfg(windows)]
    let _job = windows_job::WindowsJob::new()?;
    #[cfg(windows)]
    let mut child = _job.spawn(&mut command)?;
    #[cfg(not(windows))]
    let mut child = command.spawn().ok()?;
    #[cfg(unix)]
    let mut process_group = ProcessGroupGuard {
        process_id: child.id()? as i32,
        armed: true,
    };
    let stdout = child.stdout.take()?;
    let output = tokio::time::timeout(deadline, async {
        let mut bytes = Vec::new();
        stdout
            .take(max_output_bytes + 1)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() as u64 > max_output_bytes {
            return None;
        }
        let status = child.wait().await.ok()?;
        #[cfg(unix)]
        {
            process_group.armed = false;
        }
        Some((bytes, status))
    })
    .await;

    if let Ok(Some((bytes, status))) = output {
        return Some((String::from_utf8(bytes).ok()?, status));
    }

    #[cfg(unix)]
    process_group.kill();
    let _ = child.start_kill();
    let _ = child.wait().await;
    #[cfg(unix)]
    {
        process_group.armed = false;
    }
    None
}
