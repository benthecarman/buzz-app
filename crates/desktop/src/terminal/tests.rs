use super::*;
use portable_pty::CommandBuilder;
use std::time::{Duration, Instant};

fn context() -> TerminalContext {
    TerminalContext {
        channel_id: "d785e4a9-a24e-44a0-bb9f-7c07d31a3c5c".into(),
        channel_name: "terminal-plugin".into(),
        thread_id: Some("a".repeat(64)),
        npub: "npub1u9l940mmrk7nv0cw6maa5fz4vztj0vj42s5dagj38zx9gtxj7qls94fpux".into(),
        relay_url: "wss://buzz.example.com".into(),
    }
}
fn shell() -> CommandBuilder {
    // portable-pty 0.9 uses non-reentrant getpwuid when constructing a command
    // without inherited SHELL, and when spawning without explicit HOME/SHELL.
    // Initialize once, then clone a complete fixture environment so parallel
    // tests never share those password-database buffers or inherit user config.
    static SHELL: std::sync::OnceLock<CommandBuilder> = std::sync::OnceLock::new();
    SHELL
        .get_or_init(|| {
            let mut cmd = CommandBuilder::new("/bin/sh");
            cmd.env_clear();
            cmd.env("PATH", "/usr/bin:/bin");
            cmd.env("HOME", "/");
            cmd.env("SHELL", "/bin/sh");
            cmd
        })
        .clone()
}
fn script(script: &str) -> CommandBuilder {
    let mut cmd = shell();
    cmd.args(["-c", script]);
    cmd
}
fn read_all(state: &Terminals, owner: &str, id: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    loop {
        let result = state.with_session(owner, id, Session::read).unwrap();
        assert!(result.data.len() <= MAX_BYTES);
        output.extend(result.data);
        if result.exited {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "PTY did not exit: {}",
            String::from_utf8_lossy(&output)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn parallel_fixture_environment_probe() {
    if std::env::var("BUZZ_TERMINAL_FIXTURE_PROBE").as_deref() != Ok("1") {
        return;
    }
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                let cmd = script("printf '%s|%s|%s' \"$HOME\" \"$SHELL\" \"$PWD\"");
                assert_eq!(cmd.get_env("HOME"), Some(std::ffi::OsStr::new("/")));
                assert_eq!(cmd.get_env("SHELL"), Some(std::ffi::OsStr::new("/bin/sh")));
                let state = Terminals::default();
                let owner = state.create_owner().unwrap();
                let id = state.spawn_with(&owner, |_| Ok(cmd), 80, 24).unwrap();
                let output = read_all(&state, &owner, &id);
                state.close_owner(&owner).unwrap();
                assert_eq!(output, b"/|/bin/sh|/");
            });
        }
    });
}

#[test]
fn parallel_fixtures_work_without_parent_home_or_shell() {
    // A fresh process exercises first-use construction without mutating the
    // environment shared by the other tests or pre-initializing the template.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "terminal::tests::parallel_fixture_environment_probe",
            "--nocapture",
        ])
        .env("BUZZ_TERMINAL_FIXTURE_PROBE", "1")
        .env_remove("HOME")
        .env_remove("SHELL")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn production_environment_probe() {
    if std::env::var("BUZZ_TERMINAL_PROBE").as_deref() != Ok("1") {
        return;
    }
    let state = Terminals::default();
    let owner = state.create_owner().unwrap();
    let mut context = context();
    context.channel_name = "$(touch bad); fake".into();
    context.thread_id = None;
    let id = state.spawn(&owner, context.clone(), 80, 24).unwrap();
    // The real login-shell command and real spawn path above must fence parent
    // credentials. Do not rebuild/fence a separate command in this regression.
    state
        .with_session(&owner, &id, |s| {
            s.write(b"stty -echo; printf '\nLOGIN=%s\n' \"$0\"; /usr/bin/env; exit\n")
        })
        .unwrap();
    let output = String::from_utf8(read_all(&state, &owner, &id)).unwrap();
    assert!(output.contains("LOGIN=-sh"), "{output}");
    for secret in [
        "TOP_SECRET_PRIVATE",
        "TOP_SECRET_AUTH",
        "FUTURE_SECRET",
        "BAD_HERMIT_PATH",
    ] {
        assert!(!output.contains(secret), "{output}");
    }
    for value in [
        "TERM=xterm-256color",
        "TERM_PROGRAM=Buzz",
        "COLORTERM=truecolor",
        "SHELL=/bin/sh",
        "BUZZ_NPUB=npub1",
        "BUZZ_RELAY_URL=wss://buzz.example.com",
        "BUZZ_TERM_VERSION=",
    ] {
        assert!(output.contains(value), "missing {value}: {output}");
    }
    assert!(output.contains(&format!("BUZZ_CHANNEL_ID={}", context.channel_id)));
    assert!(output.contains(&format!("BUZZ_CHANNEL={}", context.channel_id)));
    assert!(output.contains(&format!("BUZZ_TERM_SESSION={id}")));
    assert!(!output.contains("BUZZ_THREAD_ID="));
    state.close_owner(&owner).unwrap();
}

#[test]
fn real_spawn_fences_secrets_and_preserves_login_context() {
    let dir = std::env::temp_dir().join(format!("buzz-terminal-env-{}", Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "terminal::tests::production_environment_probe",
            "--nocapture",
        ])
        .env("BUZZ_TERMINAL_PROBE", "1")
        .env("BUZZ_PRIVATE_KEY", "TOP_SECRET_PRIVATE")
        .env("BUZZ_AUTH_TAG", "TOP_SECRET_AUTH")
        .env("UNKNOWN_NEW_SECRET", "FUTURE_SECRET")
        .env("PATH", "BAD_HERMIT_PATH")
        .env("SHELL", "/bin/sh")
        .env("HOME", &dir)
        .output()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn context_and_dimensions_reject_bad_boundary_inputs() {
    assert!(context().validate().is_ok());
    for cols in [0, 1, 501, u16::MAX] {
        assert!(dimensions(cols, 24).is_err());
    }
    for rows in [0, 301, u16::MAX] {
        assert!(dimensions(80, rows).is_err());
    }
    let mut ctx = context();
    ctx.channel_id = "$(echo forged)".into();
    assert!(ctx.validate().is_err());
    let mut ctx = context();
    ctx.thread_id = Some(String::new());
    assert!(ctx.validate().is_err());
    for url in [
        "https://example.com",
        "wss://user:secret@example.com",
        "wss://example.com/?token=secret",
        "wss://example.com/#secret",
    ] {
        let mut ctx = context();
        ctx.relay_url = url.into();
        assert!(ctx.validate().is_err());
    }
    assert_ne!(unix::resolve_shell(Some("/tmp")), "/tmp");
}

#[test]
fn large_final_output_is_fully_drained_before_exited() {
    let state = Terminals::default();
    let owner = state.create_owner().unwrap();
    let id = state
        .spawn_with(
            &owner,
            |_| Ok(script("head -c 300000 /dev/zero | tr '\\000' x")),
            80,
            24,
        )
        .unwrap();
    let output = read_all(&state, &owner, &id);
    assert_eq!(output.len(), 300000);
    assert!(output.iter().all(|b| *b == b'x'));
    state.close(&owner, &id).unwrap();
    state.close(&owner, &id).unwrap();
    state.close_owner(&owner).unwrap();
    state.close_owner(&owner).unwrap();
}

#[test]
fn interactive_input_resize_and_owner_isolation() {
    let state = Terminals::default();
    let owner = state.create_owner().unwrap();
    let other = state.create_owner().unwrap();
    let id = state
        .spawn_with(
            &owner,
            |_| {
                Ok(script(
                    "read answer; stty size; printf 'answer=%s' \"$answer\"",
                ))
            },
            80,
            24,
        )
        .unwrap();
    assert!(state.with_session(&other, &id, Session::read).is_err());
    state.close(&other, &id).unwrap();
    state
        .with_session(&owner, &id, |s| s.resize(100, 40))
        .unwrap();
    state
        .with_session(&owner, &id, |s| s.write(b"hello\n"))
        .unwrap();
    let output = String::from_utf8(read_all(&state, &owner, &id)).unwrap();
    assert!(output.contains("40 100"), "{output}");
    assert!(output.contains("answer=hello"), "{output}");
    state.shutdown().unwrap();
}

#[test]
fn close_revokes_pending_and_future_spawns() {
    let state = Terminals::default();
    let owner = state.create_owner().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let spawn_state = state.clone();
    let spawn_owner = owner.clone();
    let spawn = std::thread::spawn(move || {
        spawn_state.spawn_with(
            &spawn_owner,
            |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(script("exec sleep 30"))
            },
            80,
            24,
        )
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let close_state = state.clone();
    let close_owner = owner.clone();
    let close = std::thread::spawn(move || close_state.close_owner(&close_owner));
    release_tx.send(()).unwrap();
    let id = spawn.join().unwrap().unwrap();
    close.join().unwrap().unwrap();
    assert!(state.with_session(&owner, &id, Session::read).is_err());
    assert!(state.spawn(&owner, context(), 80, 24).is_err());
    assert!(state.lock().unwrap().owners.is_empty());
    state.shutdown().unwrap();
    assert!(state.create_owner().is_err());
}

#[test]
fn bounded_input_and_session_admission() {
    let state = Terminals::default();
    let owner = state.create_owner().unwrap();
    let id = state
        .spawn_with(
            &owner,
            |_| Ok(script("stty raw -echo; echo READY; exec sleep 30")),
            80,
            24,
        )
        .unwrap();
    assert!(state
        .with_session(&owner, &id, |s| s.write(&vec![b'x'; MAX_PENDING_INPUT + 1]))
        .is_err());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut ready = Vec::new();
    while !String::from_utf8_lossy(&ready).contains("READY") {
        ready.extend(state.with_session(&owner, &id, Session::read).unwrap().data);
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut rejected = false;
    for _ in 0..100 {
        if state
            .with_session(&owner, &id, |s| s.write(&vec![b'x'; MAX_BYTES]))
            .is_err()
        {
            rejected = true;
            break;
        }
    }
    assert!(rejected);
    for _ in 1..MAX_SESSIONS {
        state
            .spawn_with(&owner, |_| Ok(script("exec sleep 30")), 80, 24)
            .unwrap();
    }
    assert!(state
        .spawn_with(
            &owner,
            |_| panic!("over-cap spawn reached process creation"),
            80,
            24
        )
        .is_err());
    state.close_owner(&owner).unwrap();
}

#[test]
fn shutdown_kills_a_signal_ignoring_group_and_reaps_the_leader() {
    let state = Terminals::default();
    let owner = state.create_owner().unwrap();
    let id = state
        .spawn_with(
            &owner,
            |_| Ok(script("trap '' TERM; sleep 30 & echo READY:$!; wait")),
            80,
            24,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut text = String::new();
    while !text.contains("READY:") || !text.contains('\n') {
        text.push_str(&String::from_utf8_lossy(
            &state.with_session(&owner, &id, Session::read).unwrap().data,
        ));
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let child: i32 = text.trim().strip_prefix("READY:").unwrap().parse().unwrap();
    let start = Instant::now();
    state.close_owner(&owner).unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    // Grandchildren can briefly remain zombies until init reaps them.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: signal zero is an existence check, never delivers a signal.
        if unsafe { libc::kill(child, 0) } != 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "grandchild survived terminal close"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn foreground_job_receives_ctrl_c_and_close_stops_job_control_group() {
    let state = Terminals::default();
    let owner = state.create_owner().unwrap();
    let id = state
        .spawn_with(
            &owner,
            |_| {
                let mut cmd = shell();
                cmd.arg("-i");
                Ok(cmd)
            },
            80,
            24,
        )
        .unwrap();
    state
        .with_session(&owner, &id, |s| s.write(b"sleep 30\n"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let foreground = loop {
        let group = state
            .with_session(&owner, &id, |s| Ok(s.foreground_job()))
            .unwrap();
        if let Some(group) = group {
            break group;
        }
        state.with_session(&owner, &id, Session::read).unwrap();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    };
    state.with_session(&owner, &id, |s| s.write(&[3])).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(foreground, 0) } == 0 {
        state.with_session(&owner, &id, Session::read).unwrap();
        assert!(
            Instant::now() < deadline,
            "Ctrl+C did not reach foreground job"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    state
        .with_session(&owner, &id, |s| s.write(b"sleep 30\n"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let foreground = loop {
        let group = state
            .with_session(&owner, &id, |s| Ok(s.foreground_job()))
            .unwrap();
        if let Some(group) = group {
            break group;
        }
        state.with_session(&owner, &id, Session::read).unwrap();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    };
    state.close_owner(&owner).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(foreground, 0) } == 0 {
        assert!(Instant::now() < deadline, "foreground job survived close");
        std::thread::sleep(Duration::from_millis(2));
    }
}
