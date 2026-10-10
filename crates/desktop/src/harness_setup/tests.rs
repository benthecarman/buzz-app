use super::*;
use std::io::Write as _;
#[cfg(unix)]
use std::process::Stdio;

#[tokio::test]
async fn one_install_at_a_time_and_success_log() {
    let state = std::sync::Arc::new(HarnessSetup::default());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-controller/pi-install.log");
    let (started, observed) = tokio::sync::oneshot::channel();
    let (release, done) = tokio::sync::oneshot::channel::<()>();
    let installing = state.clone();
    let output = path.clone();
    let task = tokio::spawn(async move {
        let _guard = installing.claim().unwrap();
        run_install(&output, |mut log| async move {
            started.send(()).unwrap();
            done.await.unwrap();
            writeln!(log, "CLI installed").unwrap();
            Ok(true)
        })
        .await
        .unwrap()
    });
    observed.await.unwrap();
    assert!(state.claim().is_err());
    release.send(()).unwrap();
    let report = task.await.unwrap();
    assert!(state.claim().is_ok());
    assert!(report.ready);
    assert_eq!(report.output.trim(), "CLI installed");
    assert_eq!(
        std::fs::read_to_string(path).unwrap().trim(),
        "CLI installed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("agent-controller/pi-install.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn failure_records_combined_output_and_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pi-install.log");
    let report = run_install(&path, |mut log| async move {
        writeln!(log, "stdout and stderr").unwrap();
        Err("Installer exited unexpectedly".into())
    })
    .await
    .unwrap();
    assert!(!report.ready);
    assert_eq!(
        report.error.as_deref(),
        Some("Installer exited unexpectedly")
    );
    assert!(report.output.contains("stdout and stderr"));
    assert!(std::fs::read_to_string(path)
        .unwrap()
        .contains("Installer exited unexpectedly"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn quit_kills_the_tracked_installer_and_its_helpers() {
    use tokio::io::AsyncBufReadExt as _;
    let state = HarnessSetup::default();
    let _guard = state.claim().unwrap();
    let mut active = state
        .spawn(|| {
            tokio::process::Command::new("/bin/sh")
                .args(["-c", "sleep 30 & printf 'ready\\n'; wait"])
                .process_group(0)
                .kill_on_drop(true)
                .stdout(Stdio::piped())
                .spawn()
        })
        .unwrap();
    let group = active.group.unwrap();
    let mut marker = String::new();
    tokio::io::BufReader::new(active.child.stdout.take().unwrap())
        .read_line(&mut marker)
        .await
        .unwrap();
    assert_eq!(marker, "ready\n", "the helper must exist before Quit");
    state.shutdown();
    assert!(!active.child.wait().await.unwrap().success());
    active.reaped = true;
    let gone = (0..50).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        (unsafe { libc::kill(-(group as i32), 0) }) == -1
    });
    assert!(gone, "installer helpers survived Quit");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn quit_serializes_with_spawn_and_kills_without_the_installer_resuming() {
    use std::sync::mpsc;
    use std::time::Duration;

    let state = HarnessSetup::default();
    let guard = state.claim().unwrap();
    let runtime = tokio::runtime::Handle::current();
    let (spawned, observe_spawn) = mpsc::channel();
    let (register, allow_registration) = mpsc::channel();
    let (registered, observe_registration) = mpsc::channel();
    let (resume, allow_resume) = mpsc::channel();
    let mut child = std::thread::scope(|scope| {
        let setup = &state;
        let worker = scope.spawn(move || {
            let _runtime = runtime.enter();
            let child = setup
                .spawn(|| {
                    let child = tokio::process::Command::new("/bin/sleep")
                        .arg("30")
                        .process_group(0)
                        .kill_on_drop(true)
                        .spawn()?;
                    spawned.send(child.id().unwrap()).unwrap();
                    // Hold the exact old gap: a child exists but is not registered.
                    allow_registration
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                    Ok(child)
                })
                .unwrap();
            registered.send(()).unwrap();
            // Quit must kill without the installer future being polled again.
            allow_resume.recv_timeout(Duration::from_secs(5)).unwrap();
            child
        });
        let _group = observe_spawn.recv_timeout(Duration::from_secs(5)).unwrap();
        let lock_held = matches!(state.1.try_lock(), Err(std::sync::TryLockError::WouldBlock));
        // Always release the worker before asserting so a regression cannot hang it.
        let (quitting, observe_quit) = mpsc::channel();
        let quit = scope.spawn(move || {
            quitting.send(()).unwrap();
            setup.shutdown();
        });
        observe_quit.recv_timeout(Duration::from_secs(5)).unwrap();
        register.send(()).unwrap();
        observe_registration
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        quit.join().unwrap();
        let untracked = state.1.lock().unwrap().group.is_none();
        resume.send(()).unwrap();
        let child = worker.join().unwrap();
        assert!(
            lock_held,
            "spawn and registration must hold the shutdown mutex"
        );
        assert!(untracked);
        child
    });
    let status = tokio::time::timeout(Duration::from_secs(5), child.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    child.reaped = true;
    assert_eq!(unsafe { libc::kill(-(child.group.unwrap() as i32), 0) }, -1);
    drop(guard);
    assert_eq!(state.claim().err().as_deref(), Some("Buzz is quitting"));
    assert_eq!(
        state
            .spawn(|| panic!("Quit must prevent a late spawn"))
            .err()
            .as_deref(),
        Some("Buzz is quitting")
    );
}

#[test]
fn a_finished_install_clears_its_group_without_killing_it_again_on_quit() {
    let state = HarnessSetup::default();
    let guard = state.claim().unwrap();
    // Above supported OS pid limits; even a failed assertion cannot signal a
    // real process group from this test.
    let absent_group = i32::MAX as u32;
    state.1.lock().unwrap().group = Some(absent_group);
    state.untrack(absent_group, false);
    assert!(state.1.lock().unwrap().group.is_none());
    state.shutdown();
    drop(guard);
    assert_eq!(state.claim().err().as_deref(), Some("Buzz is quitting"));
}

#[test]
fn install_claim_is_exclusive_and_shutdown_fences_it() {
    let setup = HarnessSetup::default();
    let first = setup.claim().unwrap();
    assert_eq!(
        setup.claim().err().as_deref(),
        Some("A Harness installation is already in progress")
    );
    drop(first);
    let pi = setup.claim().unwrap();
    assert!(setup.claim().is_err());
    drop(pi);
    setup.shutdown();
    assert_eq!(setup.claim().err().as_deref(), Some("Buzz is quitting"));
}

#[test]
fn only_failed_enabled_pi_waiting_for_its_missing_adapter_restarts() {
    use ProcessStatus::{Failed, Running, Stopped};
    assert!(waiting(
        true,
        Failed,
        "/managed/node-tools/bin/buzz-pi-acp",
        "buzz-pi-acp",
        Some("Required runtime executable is missing")
    ));
    assert!(!waiting(
        false,
        Failed,
        "/managed/node-tools/bin/buzz-pi-acp",
        "buzz-pi-acp",
        Some("Required runtime executable is missing")
    ));
    assert!(!waiting(true, Stopped, "buzz-pi-acp", "buzz-pi-acp", None));
    assert!(!waiting(true, Running, "buzz-pi-acp", "buzz-pi-acp", None));
    assert!(!waiting(
        true,
        Failed,
        "goose",
        "buzz-pi-acp",
        Some("Required runtime executable is missing")
    ));
    assert!(!waiting(
        true,
        Failed,
        "buzz-pi-acp",
        "buzz-pi-acp",
        Some("Agent listener exited; restart to retry")
    ));
}
