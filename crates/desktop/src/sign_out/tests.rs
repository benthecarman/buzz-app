use super::*;
use std::cell::Cell;
use std::future::Future;

fn paths_for(root: &Path, service: &str) -> Paths {
    Paths {
        marker: root.join(marker_name("app", service)),
        finished: root.join(finished_name(service)),
        lock: root.join(LOCK),
        key_lock: root.join(key_lock_name(service)),
        ownership: root.join(AGENT_OWNERSHIP),
        app_data: root.join("app"),
        others: vec![root.join("webkit")],
    }
}
fn fixture() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let paths = paths_for(root, "identity");
    fs::create_dir_all(paths.app_data.join(KEPT)).unwrap();
    fs::write(paths.app_data.join(KEPT).join("agents.json"), "agents").unwrap();
    fs::write(paths.app_data.join("archive.sqlite3"), "archive").unwrap();
    fs::create_dir_all(&paths.others[0]).unwrap();
    fs::write(paths.others[0].join("localstorage"), "prefs").unwrap();
    (dir, paths)
}
/// What a launch runs once it owns the locks: the pending sign-out, if any.
fn finish_pending(
    paths: &Paths,
    remove_agent_keys: impl FnOnce(&Path) -> Result<(), String>,
    remove_key: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let Some(choices) = pending(&paths.marker)? else {
        return Ok(());
    };
    finish(paths, choices, remove_agent_keys, remove_key).map_err(|error| {
        eprintln!("buzz: sign out did not finish: {error}");
        FAILED.to_owned()
    })
}
fn no_agents(_: &Path) -> Result<(), String> {
    panic!("agent keys must not be touched")
}
fn mark(paths: &Paths, wipe: bool, remove_agents: bool) {
    write_marker(
        &paths.marker,
        Choices {
            wipe,
            remove_agents,
        },
    )
    .unwrap();
}
/// Listing without the instance lock file, which boot creates.
fn data(root: &Path) -> Vec<String> {
    listing(root)
        .into_iter()
        .filter(|path| !path.ends_with("instance.lock"))
        .collect()
}
/// Everything under `root` but the finished record, which is never removed.
fn listing(root: &Path) -> Vec<String> {
    let mut found: Vec<String> = walk(root, root);
    found.retain(|path| !path.ends_with("sign-outs-finished"));
    found.sort();
    found
}
fn walk(root: &Path, dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            let name = path.strip_prefix(root).unwrap().display().to_string();
            if path.is_dir() {
                let mut nested = walk(root, &path);
                nested.push(format!("{name}/"));
                nested
            } else {
                vec![name]
            }
        })
        .collect()
}

#[test]
fn no_marker_leaves_everything_alone() {
    let (dir, paths) = fixture();
    let before = listing(dir.path());
    assert_eq!(
        finish_pending(&paths, |_| Ok(()), || panic!("key must not be touched")),
        Ok(())
    );
    assert_eq!(listing(dir.path()), before);
}

#[test]
fn sign_out_without_wipe_removes_only_the_key_and_marker() {
    let (dir, paths) = fixture();
    let before = listing(dir.path());
    mark(&paths, false, false);
    let removed = Cell::new(false);
    assert_eq!(
        finish_pending(
            &paths,
            |_| Ok(()),
            || {
                removed.set(true);
                Ok(())
            }
        ),
        Ok(())
    );
    assert!(removed.get());
    assert_eq!(listing(dir.path()), before);
}

#[test]
fn wipe_keeps_agents_unless_they_are_removed_too() {
    for remove_agents in [false, true] {
        let (dir, paths) = fixture();
        mark(&paths, true, remove_agents);
        assert_eq!(finish_pending(&paths, |_| Ok(()), || Ok(())), Ok(()));
        let expected: &[&str] = if remove_agents {
            &[]
        } else {
            &[
                "app/",
                "app/agent-controller/",
                "app/agent-controller/agents.json",
            ]
        };
        assert_eq!(listing(dir.path()), expected);
    }
}

#[test]
fn key_removal_failure_restores_data_keeps_marker_and_the_retry_succeeds() {
    for remove_agents in [false, true] {
        let (dir, paths) = fixture();
        mark(&paths, true, remove_agents);
        let mut before = listing(dir.path());
        assert_eq!(
            finish_pending(&paths, |_| Ok(()), || Err("keychain denied".into())),
            Err(FAILED.into())
        );
        // Everything, including the marker, is exactly where it was.
        assert_eq!(listing(dir.path()), before);
        assert!(paths.marker.exists());

        assert_eq!(finish_pending(&paths, |_| Ok(()), || Ok(())), Ok(()));
        before = listing(dir.path());
        assert!(!before.iter().any(|path| path.contains("archive")));
        assert!(!paths.marker.exists());
    }
}

#[test]
fn a_wipe_interrupted_part_way_resumes_on_the_next_launch() {
    let (dir, paths) = fixture();
    mark(&paths, true, false);
    // Crash after moving app data aside, before returning the agent registry.
    fs::rename(&paths.app_data, trash(&paths.app_data)).unwrap();
    assert_eq!(finish_pending(&paths, |_| Ok(()), || Ok(())), Ok(()));
    assert_eq!(
        listing(dir.path()),
        [
            "app/",
            "app/agent-controller/",
            "app/agent-controller/agents.json"
        ]
    );
}

#[test]
fn an_interrupted_wipe_rolls_back_fully_when_the_key_cannot_be_removed() {
    let (dir, paths) = fixture();
    mark(&paths, true, false);
    let before = listing(dir.path());
    fs::rename(&paths.app_data, trash(&paths.app_data)).unwrap();
    assert!(finish_pending(&paths, |_| Ok(()), || Err("locked".into())).is_err());
    assert_eq!(listing(dir.path()), before);
}

#[test]
fn an_unreadable_marker_blocks_without_touching_the_key_or_data() {
    let (dir, paths) = fixture();
    fs::write(&paths.marker, "{\"wipe\":true}").unwrap();
    let before = listing(dir.path());
    assert!(finish_pending(&paths, |_| Ok(()), || panic!("key must not be touched")).is_err());
    assert_eq!(listing(dir.path()), before);
}

#[test]
fn a_debug_sign_out_never_touches_the_release_key_or_data() {
    let (dir, debug) = fixture();
    let debug = Paths {
        marker: dir.path().join(marker_name("app", "identity.debug")),
        ..debug
    };
    let release = paths_for(dir.path(), "identity");
    assert_ne!(debug.marker, release.marker);
    mark(&debug, true, true);
    let before = data(dir.path());
    // The release build launches first: its key and the shared folders stay put.
    assert!(boot(&release, no_agents, || panic!(
        "release key must not be touched"
    ))
    .is_ok());
    assert_eq!(data(dir.path()), before);
    assert!(debug.marker.exists());
}

#[test]
fn a_retry_clears_storage_recreated_after_the_key_was_removed() {
    let (dir, paths) = fixture();
    mark(&paths, true, false);
    // Trash cleanup fails once (a file where its folder should be).
    fs::write(trash(&paths.others[0]), "stuck").unwrap();
    assert!(finish_pending(&paths, |_| Ok(()), || Ok(())).is_err());
    assert!(paths.marker.exists());
    // Something recreates storage before the next launch.
    fs::create_dir_all(&paths.others[0]).unwrap();
    fs::write(paths.others[0].join("recreated"), "new").unwrap();
    fs::write(paths.app_data.join("recreated.sqlite3"), "new").unwrap();
    fs::remove_file(trash(&paths.others[0])).unwrap();
    // The key is already gone; the retry must still succeed and leave nothing.
    assert_eq!(finish_pending(&paths, |_| Ok(()), || Ok(())), Ok(()));
    assert_eq!(
        listing(dir.path()),
        [
            "app/",
            "app/agent-controller/",
            "app/agent-controller/agents.json"
        ]
    );
}

/// A child process another test is spawning holds inherited lock handles until
/// it execs, so a dropped instance's lock can take a moment to free.
fn admitted_once_free(instance: &Instance, choices: Choices) -> bool {
    wait_for(|| instance.begin(choices))
}
const PLAIN: Choices = Choices {
    wipe: false,
    remove_agents: false,
};
const WIPE: Choices = Choices {
    wipe: true,
    remove_agents: false,
};
#[test]
fn instances_share_the_lock_and_sign_out_needs_it_alone() {
    let (_dir, paths) = fixture();
    let first = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    let second = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    assert!(first.begin(WIPE).is_err());
    drop(second);
    assert!(admitted_once_free(&first, WIPE));
    // While one instance signs out, a new launch neither runs nor waits forever.
    assert_eq!(
        boot(&paths, no_agents, || panic!("no marker")).err(),
        Some(SIGNING_OUT.into())
    );
    assert_eq!(first.abort(WIPE, refuse("withdrawn")), refuse("withdrawn"));
    assert!(boot(&paths, no_agents, || panic!("no marker")).is_ok());
}

#[test]
fn a_second_sign_out_in_the_same_instance_is_refused() {
    let (_dir, paths) = fixture();
    let instance = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    instance.begin(WIPE).unwrap();
    assert_eq!(instance.begin(WIPE), Err(refuse(ALREADY)));
    // Concurrent calls race for the same guard; exactly one is admitted.
    instance.abort(WIPE, refuse("withdrawn"));
    let admitted = std::thread::scope(|scope| {
        let calls: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| instance.begin(WIPE).is_ok()))
            .collect();
        calls
            .into_iter()
            .map(|call| call.join().unwrap())
            .filter(|&admitted| admitted)
            .count()
    });
    assert_eq!(admitted, 1);
}

#[test]
fn a_pending_sign_out_waits_for_the_restarting_instance_to_exit() {
    let (dir, paths) = fixture();
    let exiting = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    exiting.begin(WIPE).unwrap();
    mark(&paths, false, false);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        drop(exiting);
    });
    let before = data(dir.path());
    assert!(boot(&paths, no_agents, || Ok(())).is_ok());
    release.join().unwrap();
    assert!(!paths.marker.exists());
    assert_eq!(
        data(dir.path()),
        before
            .into_iter()
            .filter(|path| !path.ends_with("sign-out-pending"))
            .collect::<Vec<_>>()
    );
}

const CHILD: &str = "BUZZ_SIGN_OUT_TEST_CHILD";
/// Set when the child signs out without wiping.
const CHILD_PLAIN: &str = "BUZZ_SIGN_OUT_TEST_CHILD_PLAIN";
/// The child's key service; `identity` when unset.
const CHILD_SERVICE: &str = "BUZZ_SIGN_OUT_TEST_CHILD_SERVICE";
/// Another Buzz process: boots, then on "go" tries to sign out and, if admitted,
/// holds on until the parent says (or closes its input). Closing its input
/// before "go" just exits, so it is a copy that only runs.
#[test]
fn child_instance() {
    let Some(root) = std::env::var_os(CHILD) else {
        return;
    };
    let service = std::env::var(CHILD_SERVICE).unwrap_or_else(|_| "identity".into());
    let paths = paths_for(Path::new(&root), &service);
    let instance = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    println!("child: booted");
    let mut input = std::io::stdin().lines();
    if input.next().is_none() {
        return;
    }
    let choices = if std::env::var_os(CHILD_PLAIN).is_some() {
        PLAIN
    } else {
        WIPE
    };
    if admitted_once_free(&instance, choices) {
        mark(&paths, choices.wipe, false);
        println!("child: signing out");
        input.next();
    } else {
        println!("child: refused");
    }
}
/// `child_instance` in a separate process, driven line by line.
struct Child {
    process: std::process::Child,
    input: Option<std::process::ChildStdin>,
    output: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
}
impl Child {
    fn spawn(root: &Path) -> Self {
        Self::spawn_with(root, WIPE)
    }
    fn spawn_with(root: &Path, choices: Choices) -> Self {
        Self::spawn_as(root, "identity", choices)
    }
    /// A copy that only runs, signed in with `service`'s key.
    fn running(root: &Path, service: &str) -> Self {
        Self::spawn_as(root, service, WIPE)
    }
    fn spawn_as(root: &Path, service: &str, choices: Choices) -> Self {
        use std::io::BufRead;
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.env(CHILD_SERVICE, service);
        if !choices.wipe {
            command.env(CHILD_PLAIN, "1");
        }
        let mut process = command
            .args(["sign_out::tests::child_instance", "--exact", "--nocapture"])
            .env(CHILD, root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let input = process.stdin.take();
        let output = std::io::BufReader::new(process.stdout.take().unwrap()).lines();
        let mut child = Self {
            process,
            input,
            output,
        };
        assert_eq!(child.report(), "child: booted");
        child
    }
    fn report(&mut self) -> String {
        self.output
            .by_ref()
            .map(Result::unwrap)
            .find(|line| line.starts_with("child: "))
            .unwrap()
    }
    /// Let it try to sign out and return what it reports.
    fn go(&mut self) -> String {
        use std::io::Write;
        writeln!(self.input.as_mut().unwrap(), "go").unwrap();
        self.report()
    }
    /// Let it exit, reading the rest of its output so it never fails writing.
    fn finish(mut self) {
        drop(self.input.take());
        self.output.by_ref().for_each(drop);
        assert!(self.process.wait().unwrap().success());
    }
}

#[test]
fn across_processes_a_running_instance_blocks_sign_out_and_a_later_launch_finishes_it() {
    let (dir, paths) = fixture();
    // A launch that is already running refuses the other process's sign-out.
    let running = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    let mut child = Child::spawn(dir.path());
    assert_eq!(child.go(), "child: refused");
    child.finish();
    assert!(!paths.marker.exists());
    drop(running);
    // A launch that starts while the other process signs out waits until the
    // parent lets the child go, then finishes it.
    let mut child = Child::spawn(dir.path());
    assert_eq!(child.go(), "child: signing out");
    // Runs only once the launch has tried for the lock and is still waiting:
    // it lets the child go and waits for it to exit before the launch retries.
    let released = std::rc::Rc::new(Cell::new(false));
    let flag = released.clone();
    HANDOFF.set(Some(Box::new(move || {
        child.finish();
        flag.set(true);
    })));
    let removed = Cell::new(false);
    assert!(boot(&paths, no_agents, || {
        assert!(released.get(), "finished only after the child let go");
        removed.set(true);
        Ok(())
    })
    .is_ok());
    assert!(released.get());
    assert!(removed.get());
    assert!(!paths.marker.exists());
    assert_eq!(
        data(dir.path()),
        [
            "app/",
            "app/agent-controller/",
            "app/agent-controller/agents.json"
        ]
    );
}

#[test]
fn a_sign_out_committed_while_recovery_hands_the_lock_back_is_finished_too() {
    let (dir, paths) = fixture();
    mark(&paths, false, false);
    // In the gap after recovery lets go of the lock, another process launches
    // and commits a new sign-out.
    let root = dir.path().to_path_buf();
    HANDOFF.set(Some(Box::new(move || {
        let mut child = Child::spawn(&root);
        assert_eq!(child.go(), "child: signing out");
        child.finish();
    })));
    let removed = Cell::new(0);
    assert!(boot(&paths, no_agents, || {
        removed.set(removed.get() + 1);
        Ok(())
    })
    .is_ok());
    assert_eq!(removed.get(), 2);
    assert!(!paths.marker.exists());
}

#[test]
fn a_sign_out_committed_while_a_refused_one_lets_go_exits_natively() {
    let (dir, paths) = fixture();
    let instance = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    let mut child = Child::spawn_with(dir.path(), PLAIN);
    // Our upgrade is refused (the child shares); while we let go, it commits.
    HANDOFF.set(Some(Box::new(move || {
        assert_eq!(child.go(), "child: signing out");
        child.finish();
    })));
    let (closes, exited) = (Cell::new(0), Cell::new(false));
    let result = run(attempt(
        &instance,
        &paths,
        PLAIN,
        || closes.set(closes.get() + 1),
        || -> std::future::Ready<Result<(), String>> { panic!("agents must not stop") },
        || {
            assert_eq!(closes.get(), 1, "signing closes before the exit");
            exited.set(true);
            async { Err(refuse("exited")) }
        },
    ));
    assert!(exited.get());
    assert_eq!(result, Err(refuse("exited")));
    assert!(paths.marker.exists());
    // Not a usable retry: the guard stays taken.
    assert_eq!(instance.begin(WIPE), Err(refuse(ALREADY)));
}

/// Reading the kept registry's details fails while traversing and deleting it
/// stay allowed: the wipe must stop, not treat it as absent and delete it.
#[cfg(target_os = "macos")]
#[test]
fn an_uninspectable_kept_registry_stops_the_wipe_and_the_retry_keeps_it() {
    let (dir, paths) = fixture();
    mark(&paths, true, false);
    let before = listing(dir.path());
    let registry = paths.app_data.join(KEPT);
    let chmod = |args: &[&str]| {
        let status = std::process::Command::new("chmod")
            .args(args)
            .arg(&registry)
            .status()
            .unwrap();
        assert!(status.success());
    };
    chmod(&["+a", "everyone deny readattr"]);
    assert!(registry.try_exists().is_err());
    let result = finish_pending(&paths, no_agents, || panic!("the human key stays"));
    // Removing every ACL entry needs no attribute read, unlike `-a`.
    chmod(&["-N"]);
    assert_eq!(result, Err(FAILED.into()));
    assert_eq!(listing(dir.path()), before);

    assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
    assert_eq!(
        listing(dir.path()),
        [
            "app/",
            "app/agent-controller/",
            "app/agent-controller/agents.json"
        ]
    );
}

#[test]
fn a_first_launch_creates_a_missing_user_data_folder() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_for(&dir.path().join("missing/share"), "identity");
    assert!(boot(&paths, no_agents, || panic!("no marker")).is_ok());
    assert!(paths.lock.is_file() && paths.key_lock.is_file());
}

#[test]
fn every_identifier_shares_the_all_buzz_lock_and_each_key_has_its_own() {
    let (debug, release) = (
        Paths::resolve("dev.local.buzz.custom", "identity").unwrap(),
        Paths::resolve("dev.local.buzz.foundation", "identity").unwrap(),
    );
    assert_eq!(debug.lock, release.lock);
    assert_eq!(debug.key_lock, release.key_lock);
    assert_ne!(debug.marker, release.marker);
    assert_ne!(key_lock_name("identity"), key_lock_name("identity.debug"));
}

/// Another build signed in with a different key, e.g. a dev build beside the installed app.
const OTHER_KEY: &str = "identity.debug";

#[test]
fn plain_sign_out_needs_only_copies_using_the_same_key_closed() {
    let (dir, paths) = fixture();
    let other = Child::running(dir.path(), OTHER_KEY);
    let signing_out = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    assert_eq!(signing_out.begin(PLAIN), Ok(()));
    other.finish();
}

#[test]
fn plain_sign_out_is_refused_while_a_copy_using_the_same_key_runs() {
    let (dir, paths) = fixture();
    let same = Child::running(dir.path(), "identity");
    let signing_out = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    assert_eq!(signing_out.begin(PLAIN), Err(refuse(OTHERS)));
    drop(signing_out);
    // A pending plain sign-out waits for that copy too.
    mark(&paths, false, false);
    assert_eq!(
        boot(&paths, no_agents, || panic!("the key stays")).err(),
        Some(BUSY.into())
    );
    same.finish();
    assert!(boot(&paths, no_agents, || Ok(())).is_ok());
    assert!(!paths.marker.exists());
}

#[test]
fn wipe_is_refused_while_any_copy_runs() {
    let (dir, paths) = fixture();
    let other = Child::running(dir.path(), OTHER_KEY);
    let signing_out = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    assert_eq!(signing_out.begin(WIPE), Err(refuse(OTHERS_ALL)));
    drop(signing_out);
    // A pending wipe waits for every copy; a pending plain sign-out doesn't.
    mark(&paths, true, false);
    assert_eq!(
        boot(&paths, no_agents, || panic!("the key stays")).err(),
        Some(BUSY_ALL.into())
    );
    mark(&paths, false, false);
    assert!(boot(&paths, no_agents, || Ok(())).is_ok());
    assert!(!paths.marker.exists());
    other.finish();
}

/// A launch chose its locks for a plain sign-out; while it let go, another
/// process replaced it with a wipe and a copy with another key started. The
/// launch must not wipe holding only its key lock.
#[test]
fn a_sign_out_escalated_to_a_wipe_during_recovery_waits_for_every_copy() {
    let (dir, paths) = fixture();
    let mut wiping = Child::spawn(dir.path());
    mark(&paths, false, false);
    let before = data(dir.path());
    // The launch's first exclusive try fails (the child shares the key), then:
    let other = std::rc::Rc::new(Cell::new(None));
    let started = other.clone();
    let root = dir.path().to_path_buf();
    HANDOFF.set(Some(Box::new(move || {
        assert_eq!(wiping.go(), "child: signing out");
        wiping.finish();
        started.set(Some(Child::running(&root, OTHER_KEY)));
    })));
    assert_eq!(
        boot(&paths, no_agents, || panic!("the key stays")).err(),
        Some(BUSY_ALL.into())
    );
    assert_eq!(data(dir.path()), before);
    assert_eq!(pending(&paths.marker), Ok(Some(WIPE)));
    other.take().unwrap().finish();
    assert!(boot(&paths, no_agents, || Ok(())).is_ok());
    assert!(!paths.marker.exists());
    assert_eq!(
        data(dir.path()),
        [
            "app/",
            "app/agent-controller/",
            "app/agent-controller/agents.json"
        ]
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_registry_moves_nothing_and_the_retry_removes_agent_keys() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, paths) = fixture();
    mark(&paths, true, true);
    let before = listing(dir.path());
    let mode =
        |mode| fs::set_permissions(&paths.app_data, fs::Permissions::from_mode(mode)).unwrap();
    mode(0o000);
    let result = finish_pending(&paths, no_agents, || panic!("the human key stays"));
    mode(0o755);
    assert_eq!(result, Err(FAILED.into()));
    assert_eq!(listing(dir.path()), before);

    let removed = Cell::new(false);
    assert_eq!(
        finish_pending(
            &paths,
            |_| {
                removed.set(true);
                Ok(())
            },
            || Ok(())
        ),
        Ok(())
    );
    assert!(removed.get());
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn agent_keys_go_first_and_an_interruption_retries_without_moving_anything() {
    let (dir, paths) = fixture();
    mark(&paths, true, true);
    let before = listing(dir.path());
    assert_eq!(
        finish_pending(
            &paths,
            |registry| {
                assert!(registry.join("agents.json").exists());
                Err("keychain interrupted".into())
            },
            || panic!("the human key waits for the agent keys")
        ),
        Err(FAILED.into())
    );
    assert_eq!(listing(dir.path()), before);
    let removed = Cell::new(false);
    assert_eq!(
        finish_pending(
            &paths,
            |_| {
                removed.set(true);
                Ok(())
            },
            || Ok(())
        ),
        Ok(())
    );
    assert!(removed.get());
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn kept_agents_keep_only_their_list_settings_and_keys() {
    let (dir, paths) = fixture();
    let controller = paths.app_data.join(KEPT);
    for file in [
        "defaults.json",
        "agents.previous.json",
        "controller.lock",
        "buzz-agent/oauth/databricks/0123abcd.json",
        "logs/agent.log",
        "runs/agent-1/tmp/scratch",
        "run-controls/agent-1/launch-protection.json",
        "control-write/staged",
        "something-new",
    ] {
        let path = controller.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }
    mark(&paths, true, false);
    assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
    assert_eq!(
        listing(dir.path()),
        [
            "app/",
            "app/agent-controller/",
            "app/agent-controller/agents.json",
            "app/agent-controller/defaults.json"
        ]
    );
    assert_eq!(fs::read(controller.join("agents.json")).unwrap(), b"agents");
    assert_eq!(fs::read(controller.join("defaults.json")).unwrap(), b"x");
}

#[cfg(unix)]
#[test]
fn a_linked_kept_registry_stops_the_wipe_and_deletes_nothing_through_it() {
    let (dir, paths) = fixture();
    let controller = paths.app_data.join(KEPT);
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let files = [
        ("agents.json", "a"),
        ("defaults.json", "d"),
        ("sentinel", "s"),
    ];
    for (name, body) in files {
        fs::write(outside.join(name), body).unwrap();
    }
    fs::remove_dir_all(&controller).unwrap();
    std::os::unix::fs::symlink(&outside, &controller).unwrap();
    mark(&paths, true, false);
    let result = finish_pending(&paths, no_agents, || panic!("the human key stays"));
    assert_eq!(result, Err(FAILED.to_owned()));
    assert!(paths.marker.exists());
    for (name, body) in files {
        assert_eq!(fs::read(outside.join(name)).unwrap(), body.as_bytes());
    }
    // A real registry in its place lets the retry finish.
    fs::remove_file(&controller).unwrap();
    fs::create_dir(&controller).unwrap();
    fs::write(controller.join("agents.json"), "agents").unwrap();
    assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
    assert!(!paths.marker.exists());
    assert_eq!(fs::read(controller.join("agents.json")).unwrap(), b"agents");
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"s");
}

#[cfg(unix)]
#[test]
fn a_linked_wipe_folder_or_trash_stops_the_wipe() {
    for linked in ["webkit", "webkit.sign-out-trash", "app.sign-out-trash"] {
        let (dir, paths) = fixture();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "s").unwrap();
        let link = dir.path().join(linked);
        let _ = fs::remove_dir_all(&link);
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        mark(&paths, true, false);
        let result = finish_pending(&paths, no_agents, || panic!("the human key stays"));
        assert_eq!(result, Err(FAILED.to_owned()), "{linked}");
        assert!(paths.marker.exists());
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"s");
    }
}

#[test]
fn kept_agents_keep_their_keys() {
    let (_dir, paths) = fixture();
    mark(&paths, true, false);
    assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
}

#[test]
fn debug_builds_sign_out_without_wipe_and_wipe_needs_default_plugin_storage() {
    assert_eq!(refusal(true, false, false, false, false), None);
    assert_eq!(refusal(true, false, false, true, false), Some(DEV_WIPE));
    assert_eq!(refusal(true, false, false, true, true), Some(DEV_WIPE));
    assert!(refusal(false, true, false, false, false)
        .unwrap()
        .contains("development broker"));
    assert!(refusal(false, false, true, true, false)
        .unwrap()
        .contains("BUZZODZ_HOME"));
    assert_eq!(refusal(false, false, true, false, false), None);
    assert_eq!(refusal(false, false, false, true, true), None);
}

fn choices(remove_agents: bool) -> Choices {
    Choices {
        wipe: true,
        remove_agents,
    }
}
fn run<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn preparation_commits_the_marker_before_stopping_agents() {
    let (_dir, paths) = fixture();
    let marker = paths.marker.clone();
    let stopped = Cell::new(false);
    let closed = Cell::new(false);
    run(prepare(
        &paths,
        choices(true),
        || closed.set(true),
        || {
            assert!(marker.exists());
            assert!(closed.get(), "signing closes before agents stop");
            stopped.set(true);
            async { Ok(()) }
        },
    ))
    .unwrap();
    assert!(stopped.get());
}

#[test]
fn a_marker_write_failure_stops_nothing() {
    let (dir, mut paths) = fixture();
    paths.marker = dir.path().join("missing-folder/marker");
    let failure = run(prepare(
        &paths,
        choices(true),
        || panic!("nothing was committed"),
        || -> std::future::Ready<Result<(), String>> { panic!("agents must not stop") },
    ))
    .unwrap_err();
    assert_eq!(failure, refuse(NOT_PREPARED));
}

#[test]
fn a_shutdown_failure_keeps_the_marker_and_exits_natively() {
    let (_dir, paths) = fixture();
    let instance = boot(&paths, no_agents, || panic!("no marker"))
        .ok()
        .unwrap();
    let (closes, exited) = (Cell::new(0), Cell::new(false));
    let result = run(attempt(
        &instance,
        &paths,
        choices(false),
        || closes.set(closes.get() + 1),
        || async { Err("controller stuck".to_owned()) },
        || {
            // Once on commit, and again on the fenced path before the exit.
            assert_eq!(closes.get(), 2, "signing closes before the exit");
            exited.set(true);
            async { Err(refuse("exited")) }
        },
    ));
    assert!(exited.get());
    assert_eq!(result, Err(refuse("exited")));
    assert!(paths.marker.exists());
}

#[test]
fn a_pending_erase_waits_for_every_agent_supervisor_to_let_go() {
    let (_dir, paths) = fixture();
    mark(&paths, true, true);
    fs::create_dir_all(&paths.ownership).unwrap();
    let held = File::create(paths.ownership.join("agent-community.lock")).unwrap();
    #[allow(clippy::incompatible_msrv)]
    held.lock().unwrap();
    let before = data(paths.app_data.parent().unwrap());
    let refused = boot(
        &paths,
        |_| panic!("agent keys must stay"),
        || panic!("the key must stay"),
    );
    assert_eq!(refused.err(), Some(AGENT_STOPPING.to_owned()));
    assert_eq!(data(paths.app_data.parent().unwrap()), before);
    drop(held);
    let removed = Cell::new(0);
    let finished = boot(
        &paths,
        |_| {
            removed.set(removed.get() + 1);
            Ok(())
        },
        || {
            removed.set(removed.get() + 1);
            Ok(())
        },
    );
    assert_eq!(finished.err(), None);
    assert_eq!(removed.get(), 2);
    assert!(!paths.marker.exists());
}

#[test]
fn a_plain_sign_out_does_not_wait_for_agent_supervisors() {
    let (_dir, paths) = fixture();
    mark(&paths, false, false);
    fs::create_dir_all(&paths.ownership).unwrap();
    let held = File::create(paths.ownership.join("agent-community.lock")).unwrap();
    #[allow(clippy::incompatible_msrv)]
    held.lock().unwrap();
    assert!(boot(&paths, no_agents, || Ok(())).is_ok());
    assert!(!paths.marker.exists());
}

/// An outside folder holding `files`, for links to point at.
#[cfg(unix)]
fn outside(dir: &Path, files: &[&str]) -> PathBuf {
    let outside = dir.join("outside");
    for file in files {
        let path = outside.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, file.as_bytes()).unwrap();
    }
    outside
}
#[cfg(unix)]
fn unchanged(outside: &Path, files: &[&str]) {
    for file in files {
        assert_eq!(fs::read(outside.join(file)).unwrap(), file.as_bytes());
    }
}

#[cfg(unix)]
#[test]
fn a_linked_storage_parent_is_trusted_and_only_buzz_folder_under_it_goes() {
    let (dir, mut paths) = fixture();
    let files = ["sentinel", "app/localstorage"];
    let outside = outside(dir.path(), &files);
    let parent = dir.path().join("Library-WebKit");
    std::os::unix::fs::symlink(&outside, &parent).unwrap();
    paths.others = vec![parent.join("app")];
    mark(&paths, true, false);
    assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
    assert!(fs::symlink_metadata(&parent).unwrap().is_symlink());
    assert!(!outside.join("app").exists());
    unchanged(&outside, &["sentinel"]);
}

#[cfg(unix)]
#[test]
fn rollback_never_moves_through_an_app_data_swapped_for_a_link() {
    let (dir, paths) = fixture();
    let files = ["agent-controller/sentinel"];
    let outside = outside(dir.path(), &files);
    mark(&paths, true, false);
    let result = finish_pending(&paths, no_agents, || {
        fs::rename(&paths.app_data, dir.path().join("displaced")).unwrap();
        std::os::unix::fs::symlink(&outside, &paths.app_data).unwrap();
        Err("keychain busy".into())
    });
    assert_eq!(result, Err(FAILED.to_owned()));
    assert!(paths.marker.exists());
    assert_eq!(
        listing(&outside),
        ["agent-controller/", "agent-controller/sentinel"]
    );
    unchanged(&outside, &files);
    // With the real app data back, the retry finishes and leaves outside alone.
    fs::remove_file(&paths.app_data).unwrap();
    fs::rename(dir.path().join("displaced"), &paths.app_data).unwrap();
    assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
    assert!(!paths.marker.exists());
    unchanged(&outside, &files);
}

#[cfg(unix)]
#[test]
fn a_kept_registry_recreated_as_a_link_in_trash_fails_and_the_retry_finishes() {
    let (dir, paths) = fixture();
    let files = ["sentinel"];
    let outside = outside(dir.path(), &files);
    let link = trash(&paths.app_data).join(KEPT);
    mark(&paths, true, false);
    let result = finish_pending(&paths, no_agents, || {
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        Ok(())
    });
    assert_eq!(result, Err(FAILED.to_owned()));
    assert!(paths.marker.exists());
    assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    unchanged(&outside, &files);
    fs::remove_file(&link).unwrap();
    assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
    assert!(!paths.marker.exists());
    assert_eq!(
        fs::read(paths.app_data.join(KEPT).join("agents.json")).unwrap(),
        b"agents"
    );
    unchanged(&outside, &files);
}

#[cfg(unix)]
#[test]
fn a_wipe_folder_or_trash_recreated_as_a_link_fails_and_the_retry_finishes() {
    for linked in ["webkit", "webkit.sign-out-trash"] {
        let (dir, paths) = fixture();
        let files = ["sentinel"];
        let outside = outside(dir.path(), &files);
        let link = dir.path().join(linked);
        mark(&paths, true, false);
        let result = finish_pending(&paths, no_agents, || {
            let _ = fs::rename(&link, dir.path().join("displaced"));
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            Ok(())
        });
        assert_eq!(result, Err(FAILED.to_owned()), "{linked}");
        assert!(paths.marker.exists());
        unchanged(&outside, &files);
        fs::remove_file(&link).unwrap();
        fs::create_dir(&link).unwrap();
        assert_eq!(finish_pending(&paths, no_agents, || Ok(())), Ok(()));
        assert!(!paths.marker.exists());
        assert!(!link.exists());
        unchanged(&outside, &files);
    }
}

#[cfg(unix)]
#[test]
fn a_linked_wipe_folder_refuses_before_any_marker() {
    let (dir, paths) = fixture();
    let controller = paths.app_data.join(KEPT);
    fs::remove_dir_all(&controller).unwrap();
    std::os::unix::fs::symlink(dir.path(), &controller).unwrap();
    let failure = run(prepare(
        &paths,
        choices(false),
        || panic!("nothing was committed"),
        || -> std::future::Ready<Result<(), String>> { panic!("agents must not stop") },
    ))
    .unwrap_err();
    assert_eq!(failure, refuse(LINKED));
    assert!(!paths.marker.exists());
}

/// While this instance lets go of its locks, another sign-out is committed and
/// a restarted launch finishes it, removing the marker. Returns the fenced attempt.
/// `identifier` names the app that finishes it; it shares this instance's key.
fn finished_in_the_gap(
    dir: &Path,
    paths: &Paths,
    choices: Choices,
    identifier: &str,
) -> Result<(), Failure> {
    let instance = boot(paths, no_agents, || panic!("no marker")).ok().unwrap();
    // Plain: a copy sharing the key refuses the upgrade. Wipe: a copy with
    // another key refuses the all-Buzz lock.
    let running = if choices.wipe {
        Child::running(dir, OTHER_KEY)
    } else {
        Child::running(dir, "identity")
    };
    let mut gap = paths_for(dir, "identity");
    gap.marker = dir.join(marker_name(identifier, "identity"));
    HANDOFF.set(Some(Box::new(move || {
        running.finish();
        mark(&gap, choices.wipe, false);
        drop(boot(&gap, no_agents, || Ok(())).ok().unwrap());
        assert!(!gap.marker.exists(), "the sign-out finished");
    })));
    let closes = Cell::new(0);
    let result = run(attempt(
        &instance,
        paths,
        choices,
        || closes.set(closes.get() + 1),
        || -> std::future::Ready<Result<(), String>> { panic!("agents must not stop") },
        || {
            assert_eq!(closes.get(), 1, "signing closes before the exit");
            async { Err(refuse("exited")) }
        },
    ));
    assert_eq!(instance.begin(WIPE), Err(refuse(ALREADY)));
    result
}

#[test]
fn a_sign_out_finished_while_a_refused_one_lets_go_exits_natively() {
    let (dir, paths) = fixture();
    assert_eq!(
        finished_in_the_gap(dir.path(), &paths, PLAIN, "app"),
        Err(refuse("exited"))
    );
}

#[test]
fn a_wipe_finished_while_a_refused_one_lets_go_exits_natively() {
    let (dir, paths) = fixture();
    assert_eq!(
        finished_in_the_gap(dir.path(), &paths, WIPE, "app"),
        Err(refuse("exited"))
    );
}

#[test]
fn a_sign_out_finished_by_another_identifier_using_the_key_exits_natively() {
    let (dir, paths) = fixture();
    assert_eq!(
        finished_in_the_gap(dir.path(), &paths, PLAIN, "other"),
        Err(refuse("exited"))
    );
}

#[test]
fn a_sign_out_finished_before_launch_does_not_fence_a_later_refusal() {
    let (dir, paths) = fixture();
    mark(&paths, false, false);
    let instance = boot(&paths, no_agents, || Ok(())).ok().unwrap();
    assert!(paths.finished.exists());
    let running = Child::running(dir.path(), "identity");
    assert_eq!(instance.begin(PLAIN), Err(refuse(OTHERS)));
    running.finish();
}
