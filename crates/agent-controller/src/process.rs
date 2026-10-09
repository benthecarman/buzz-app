//! Every listener gets its own Unix session. ACP's worker process groups stay
//! inside that session, so teardown is not limited to the listener's group.
//! On Windows, a kill-on-close Job Object contains the listener before it runs.
use crate::Result;
#[cfg(unix)]
use std::process::ExitStatus;
#[cfg(all(unix, not(target_os = "linux")))]
use std::process::Stdio;
use std::process::{Child, Command};
#[cfg(unix)]
use std::time::{Duration, Instant};

pub(crate) struct Process {
    child: Child,
    #[cfg(unix)]
    session: u32,
    #[cfg(windows)]
    job: job::Job,
    stopped: bool,
}
impl Process {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // setsid is async-signal-safe and performs no allocation in pre_exec.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            let job = job::Job::create()?;
            let child = job.spawn(command)?;
            Ok(Self {
                child,
                job,
                stopped: false,
            })
        }
        #[cfg(unix)]
        {
            let child = command.spawn().map_err(|_| {
                "Could not start bundled agent listener; check the runtime installation"
            })?;
            let session = child.id();
            Ok(Self {
                child,
                session,
                stopped: false,
            })
        }
    }
    pub fn alive(&mut self) -> Result<bool> {
        if self.stopped {
            return Ok(false);
        }
        #[cfg(windows)]
        self.job.sweep();
        match self
            .child
            .try_wait()
            .map_err(|_| "Could not inspect agent process")?
        {
            None => Ok(true),
            Some(_) => {
                self.stop()?;
                Ok(false)
            }
        }
    }
    #[cfg(unix)]
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .try_wait()
            .map_err(|_| "Could not inspect owned process".into())
    }
    /// Short-lived discovery has no cooperative listener shutdown to wait for.
    #[cfg(unix)]
    pub fn kill(&mut self) -> Result<()> {
        self.stop_with_grace(Duration::ZERO)
    }
    pub fn stop(&mut self) -> Result<()> {
        #[cfg(unix)]
        return self.stop_with_grace(Duration::from_secs(2));
        #[cfg(windows)]
        self.stop_with_grace()
    }
    fn stop_with_grace(&mut self, #[cfg(unix)] grace: Duration) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        #[cfg(unix)]
        {
            // First let ACP cancel turns and shut down its workers itself.
            signal_in_session(self.session, self.session, libc::SIGTERM)?;
            let deadline = Instant::now() + grace;
            loop {
                let members = session_members(self.session)?;
                if members.is_empty() {
                    break;
                }
                if Instant::now() >= deadline {
                    // Freeze first so a terminating child cannot create a fresh
                    // descendant between the enumeration and the kill pass.
                    for pid in &members {
                        signal_in_session(*pid, self.session, libc::SIGSTOP)?;
                    }
                    let frozen = session_members(self.session)?;
                    for pid in frozen {
                        signal_in_session(pid, self.session, libc::SIGKILL)?;
                    }
                    break;
                }
                // Reap the leader if it exited; session ID is retained by living members.
                self.child
                    .try_wait()
                    .map_err(|_| "Could not reap agent listener")?;
                std::thread::sleep(Duration::from_millis(25));
            }
            self.child
                .wait()
                .map_err(|_| "Could not reap agent listener")?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !session_members(self.session)?.is_empty() {
                if Instant::now() >= deadline {
                    return Err("Agent descendants have not exited; shutdown is incomplete".into());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            self.stopped = true;
            Ok(())
        }
        #[cfg(windows)]
        {
            // A windowless listener has no cooperative stop signal.
            self.job.stop()?;
            self.child
                .wait()
                .map_err(|_| "Could not reap agent listener")?;
            self.stopped = true;
            Ok(())
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
#[cfg(unix)]
fn signal(pid: u32, value: i32) -> Result<()> {
    if pid <= 1 || pid > i32::MAX as u32 {
        return Err("Invalid owned process identifier".into());
    }
    if unsafe { libc::kill(pid as i32, value) } == -1
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    {
        return Err("Could not signal owned agent process".into());
    }
    Ok(())
}
#[cfg(unix)]
fn signal_in_session(pid: u32, session: u32, value: i32) -> Result<()> {
    if unsafe { libc::getsid(pid as i32) } == session as i32 {
        signal(pid, value)?;
    }
    Ok(())
}
/// Linux reads `/proc` directly rather than running `ps`, whose location
/// varies (NixOS has no `/bin/ps`). Only IDs and states are read, as with
/// `ps`; getsid stays the authority for session membership.
#[cfg(target_os = "linux")]
fn session_members(session: u32) -> Result<Vec<u32>> {
    let entries = std::fs::read_dir("/proc").map_err(|_| "Could not inspect agent descendants")?;
    let mut members = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
        else {
            continue;
        };
        // A process that exits meanwhile has no stat file; it is no member.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The state follows the parenthesised command name, which may
        // itself contain spaces or parentheses.
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next());
        if state == Some("Z") {
            continue;
        } // exited; its parent/init owns reaping
        if unsafe { libc::getsid(pid as i32) } == session as i32 {
            members.push(pid);
        }
    }
    Ok(members)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn session_members(session: u32) -> Result<Vec<u32>> {
    // ps exposes only IDs/state, never environment or command lines. getsid is
    // the authority, not platform-dependent ps SID formatting (macOS differs).
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,stat="])
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "Could not inspect agent descendants")?;
    if !output.status.success() || output.stdout.len() > 4 * 1024 * 1024 {
        return Err("Could not inspect agent descendants".into());
    }
    let mut members = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let pid = fields.next().and_then(|p| p.parse::<u32>().ok());
        let status = fields.next().unwrap_or("");
        if status.starts_with('Z') {
            continue;
        } // exited; its parent/init owns reaping
        if let Some(pid) = pid.filter(|p| *p > 1 && *p <= i32::MAX as u32) {
            if unsafe { libc::getsid(pid as i32) } == session as i32 {
                members.push(pid);
            }
        }
    }
    Ok(members)
}

#[cfg(windows)]
mod job {
    use crate::Result;
    use std::collections::HashSet;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_NO_MORE_FILES, FILETIME, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JobObjectAssociateCompletionPortInformation, JobObjectBasicAccountingInformation,
        JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::SystemServices::JOB_OBJECT_MSG_NEW_PROCESS;
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, OpenThread, ResumeThread, WaitForSingleObject,
        CREATE_NO_WINDOW, CREATE_SUSPENDED, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, THREAD_SUSPEND_RESUME,
    };
    use windows_sys::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus,
    };

    /// Completion key of job messages; the watcher stops on any other.
    const JOB: usize = 1;
    /// A member's process object, keyed by ID and creation time.
    type Member = ((u32, u64), OwnedHandle);

    /// Only a member's own signaled process object proves that it has exited.
    /// A watcher opens each process as it joins; `seen` counts the processes
    /// opened and `held` keeps those not yet signaled, across Stop attempts.
    pub(super) struct Job {
        handle: Arc<OwnedHandle>,
        port: Arc<OwnedHandle>,
        joined: mpsc::Receiver<Member>,
        held: Vec<OwnedHandle>,
        seen: HashSet<(u32, u64)>,
    }

    impl Job {
        /// Unnamed and without breakaway: members cannot leave, and closing the
        /// last handle (including on owner death) terminates every member.
        pub(super) fn create() -> Result<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err("Could not create agent process container".into());
            }
            let handle = Arc::new(unsafe { OwnedHandle::from_raw_handle(handle) });
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    handle.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            } == 0
            {
                return Err("Could not configure agent process container".into());
            }
            // Associated before the listener joins, so no member predates the port.
            let port =
                unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
            if port.is_null() {
                return Err("Could not configure agent process container".into());
            }
            let port = Arc::new(unsafe { OwnedHandle::from_raw_handle(port) });
            let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: JOB as *mut _,
                CompletionPort: port.as_raw_handle(),
            };
            if unsafe {
                SetInformationJobObject(
                    handle.as_raw_handle(),
                    JobObjectAssociateCompletionPortInformation,
                    (&association as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
                    std::mem::size_of_val(&association) as u32,
                )
            } == 0
            {
                return Err("Could not configure agent process container".into());
            }
            let (found, joined) = mpsc::channel();
            let (job, queue) = (handle.clone(), port.clone());
            std::thread::Builder::new()
                .spawn(move || watch(&job, &queue, &found))
                .map_err(|_| "Could not configure agent process container")?;
            Ok(Self {
                handle,
                port,
                joined,
                held: Vec::new(),
                seen: HashSet::new(),
            })
        }

        pub(super) fn spawn(&self, command: &mut Command) -> Result<Child> {
            spawn(&self.handle, command)
        }

        /// Collect opened members and retire only signaled handles.
        pub(super) fn sweep(&mut self) {
            while let Ok(member) = self.joined.try_recv() {
                self.add(member);
            }
            self.held.retain(
                |held| unsafe { WaitForSingleObject(held.as_raw_handle(), 0) } != WAIT_OBJECT_0,
            );
        }

        fn add(&mut self, (key, handle): Member) {
            if self.seen.insert(key) {
                self.held.push(handle);
            }
        }

        /// Terminating only requests exit. Succeed once every process that ever
        /// joined was opened and each opened process is signaled; a lost
        /// message or a member gone before it was opened fails closed.
        pub(super) fn stop(&mut self) -> Result<()> {
            // Open live members directly in case their messages are still queued.
            for id in listed(&self.handle) {
                if let Some(member) = member(&self.handle, id) {
                    self.add(member);
                }
            }
            if unsafe { TerminateJobObject(self.handle.as_raw_handle(), 1) } == 0 {
                return Err("Could not stop agent processes".into());
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.sweep();
                if self.held.is_empty() && self.seen.len() == joined(&self.handle)? as usize {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("Agent descendants have not exited; shutdown is incomplete".into());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }

    impl Drop for Job {
        /// Wake the watcher so it releases its job handle.
        fn drop(&mut self) {
            unsafe {
                PostQueuedCompletionStatus(self.port.as_raw_handle(), 0, 0, std::ptr::null())
            };
        }
    }

    /// Try to open each process as it joins. Delivery is not guaranteed and a
    /// fast member can exit first; a missed process stays uncounted, never inferred.
    fn watch(job: &OwnedHandle, port: &OwnedHandle, found: &mpsc::Sender<Member>) {
        loop {
            let (mut message, mut key, mut id) = (0, 0, std::ptr::null_mut());
            if unsafe {
                GetQueuedCompletionStatus(
                    port.as_raw_handle(),
                    &mut message,
                    &mut key,
                    &mut id,
                    INFINITE,
                )
            } == 0
                || key != JOB
            {
                return;
            }
            if message == JOB_OBJECT_MSG_NEW_PROCESS {
                // For job messages the overlapped pointer carries the process ID.
                if let Some(member) = member(job, id as usize as u32) {
                    if found.send(member).is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// A reused ID names a process outside the job. Creation time tells a
    /// member reopened for a later message from one that is not yet counted.
    fn member(job: &OwnedHandle, id: u32) -> Option<Member> {
        let access = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
        let handle = unsafe { OpenProcess(access, 0, id) };
        if handle.is_null() {
            return None;
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut inside = 0;
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        if unsafe { IsProcessInJob(handle.as_raw_handle(), job.as_raw_handle(), &mut inside) } == 0
            || inside == 0
            || unsafe { GetProcessTimes(handle.as_raw_handle(), created, exited, kernel, user) }
                == 0
        {
            return None;
        }
        let created = (created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64;
        Some(((id, created), handle))
    }

    /// Fail closed: a child that is not contained never runs its first instruction.
    fn spawn(job: &OwnedHandle, command: &mut Command) -> Result<Child> {
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = command.spawn().map_err(|_| {
            "Could not start bundled agent listener; check the runtime installation"
        })?;
        if let Err(error) = contain(job, &child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(child)
    }

    /// Assign a suspended child, then resume its only thread.
    fn contain(job: &OwnedHandle, child: &Child) -> Result<()> {
        let thread = primary_thread(child.id()).ok_or("Could not contain agent listener")?;
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err("Could not contain agent listener".into());
        }
        if unsafe { ResumeThread(thread.as_raw_handle()) } != 1 {
            return Err("Could not resume contained agent listener".into());
        }
        Ok(())
    }

    /// Every process ever associated with the job, including exited ones.
    fn joined(job: &OwnedHandle) -> Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        if unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err("Could not inspect agent descendants".into());
        }
        Ok(info.TotalProcesses)
    }

    /// IDs of the active members. Any it misses is left to the joined count.
    fn listed(job: &OwnedHandle) -> Vec<u32> {
        // JOBOBJECT_BASIC_PROCESS_ID_LIST with room for any listener tree.
        #[repr(C)]
        struct List {
            assigned: u32,
            listed: u32,
            ids: [usize; 1024],
        }
        let mut list = List {
            assigned: 0,
            listed: 0,
            ids: [0; 1024],
        };
        let queried = unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicProcessIdList,
                (&mut list as *mut List).cast(),
                std::mem::size_of_val(&list) as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Vec::new();
        }
        list.ids
            .iter()
            .take(list.listed as usize)
            .map(|&id| id as u32)
            .collect()
    }

    // Same exactly-one-thread check as the host command container.
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

    #[cfg(test)]
    #[test]
    fn uncontained_child_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut command = Command::new(std::env::var_os("ComSpec").unwrap());
        command.raw_arg(format!("/d /c type nul > \"{}\"", marker.display()));
        // A handle that is not a job: assignment fails after the suspended spawn.
        let not_a_job = std::fs::File::create(dir.path().join("not-a-job"))
            .unwrap()
            .into();
        assert_eq!(
            spawn(&not_a_job, &mut command).unwrap_err(),
            "Could not contain agent listener"
        );
        assert!(!marker.exists(), "uncontained child ran");
        let job = Job::create().unwrap();
        let mut child = job.spawn(&mut command).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(marker.exists(), "contained child was not resumed");
    }

    #[cfg(test)]
    #[test]
    fn member_gone_before_it_was_opened_fails_stop() {
        let shell = std::env::var_os("ComSpec").unwrap();
        let mut job = Job::create().unwrap();
        let mut root = job
            .spawn(Command::new(&shell).raw_arg("/d /c ping -n 600 127.0.0.1 >nul"))
            .unwrap();
        let mut gone = job
            .spawn(Command::new(&shell).raw_arg("/d /c exit"))
            .unwrap();
        // Its own signaled handle proves the exit; holding it pins the ID.
        assert!(gone.wait().unwrap().success());
        let deadline = Instant::now() + Duration::from_secs(30);
        // Once received, a sweep after the exit also retires the watcher's handle.
        let key = loop {
            job.sweep();
            if let Some(&key) = job.seen.iter().find(|&&(id, _)| id == gone.id()) {
                break key;
            }
            assert!(Instant::now() < deadline, "watcher did not open the member");
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(gone);
        // Wait until Stop could no longer reopen that exact process.
        while member(&job.handle, key.0).is_some_and(|(found, _)| found == key) {
            assert!(Instant::now() < deadline, "exited member was not released");
            std::thread::sleep(Duration::from_millis(10));
        }
        // As if its notification were lost.
        job.seen.remove(&key);
        assert_eq!(
            job.stop().unwrap_err(),
            "Agent descendants have not exited; shutdown is incomplete"
        );
        assert!(root.try_wait().unwrap().is_some(), "root survived Stop");
        assert!(job.held.is_empty(), "an opened member survived Stop");
        assert!(
            !job.seen.contains(&key),
            "Stop recaptured the missing member"
        );
    }
}
