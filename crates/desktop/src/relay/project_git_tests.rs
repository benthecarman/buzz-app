use super::*;
use std::io::Read as _;
use std::process::Command as StdCommand;

const OWNER: &str = "ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB";

fn read(value: Value) -> std::result::Result<GitRead, String> {
    parse_read(serde_json::from_value(value).map_err(|e| e.to_string())?)
}

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.test")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.test")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

/// Production configuration; the shared sender is never signalled.
fn production_git(directory: &Path) -> Git {
    static STOP: OnceLock<watch::Sender<bool>> = OnceLock::new();
    Git::new(
        directory,
        "Authorization: Nostr test",
        Instant::now() + DEADLINE,
        STOP.get_or_init(|| watch::channel(false).0).subscribe(),
    )
}

/// Production configuration with only file transport re-enabled, so the full read runs offline.
fn local_git(directory: &Path) -> Git {
    let mut git = production_git(directory);
    let key = git
        .env
        .iter()
        .find(|(_, v)| v == "protocol.allow")
        .map(|(k, _)| k.to_string_lossy().replace("KEY", "VALUE"))
        .unwrap();
    let key = OsString::from(key);
    git.env.iter_mut().find(|(k, _)| *k == key).unwrap().1 = "always".into();
    git
}

fn source() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    sh(path, &["init", "-q", "-b", "main"]);
    std::fs::write(path.join("README.md"), "# Hello\n").unwrap();
    std::fs::create_dir(path.join("src")).unwrap();
    std::fs::write(path.join("src/main.rs"), "fn main() {}\n").unwrap();
    sh(path, &["add", "."]);
    sh(path, &["commit", "-q", "-m", "first"]);
    let first = sh(path, &["rev-parse", "HEAD"]);
    std::fs::write(path.join("logo.bin"), [0u8, 1, 2]).unwrap();
    std::fs::write(path.join("src/main.rs"), "fn main() { println!(); }\n").unwrap();
    sh(path, &["add", "."]);
    sh(path, &["commit", "-q", "-m", "second"]);
    let url = url::Url::from_directory_path(path).unwrap().to_string();
    (dir, url, first)
}

fn go(git: &Git, url: &str, read: &GitRead) -> Read<Value> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(fetch_snapshot(git, url, read))
}

#[test]
fn first_advertisement_does_not_discover_parent_git_config() {
    let parent = tempfile::tempdir().unwrap();
    check_discovery_ceiling(parent.path());
}

#[cfg(unix)]
#[test]
fn discovery_ceiling_handles_symlinked_temp_parent() {
    let parent = tempfile::tempdir().unwrap();
    let alias = tempfile::tempdir().unwrap();
    let link = alias.path().join("linked-temp");
    std::os::unix::fs::symlink(parent.path(), &link).unwrap();
    check_discovery_ceiling(&link);
}

fn check_discovery_ceiling(parent: &Path) {
    let nested = parent.join("nested");
    std::fs::create_dir(&nested).unwrap();
    sh(&nested, &["init", "-q"]);
    let work = nested.join("read");
    std::fs::create_dir(&work).unwrap();
    let original = "https://example.test/repo.git";
    let replacement = "file:///offline-only/";
    sh(
        &nested,
        &[
            "config",
            "--local",
            &format!("url.{replacement}.insteadOf"),
            "https://example.test/",
        ],
    );
    let git = production_git(&work);
    // Prove the fixture rewrites when the discovery boundary is removed.
    let mut without_ceiling = production_git(&work);
    without_ceiling
        .env
        .retain(|(key, _)| key != "GIT_CEILING_DIRECTORIES");
    let probe = |git: &Git| {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(git.run(&["ls-remote", "--get-url", original]))
            .unwrap()
    };
    assert_eq!(
        String::from_utf8(probe(&without_ceiling)).unwrap().trim(),
        "file:///offline-only/repo.git"
    );
    assert_eq!(String::from_utf8(probe(&git)).unwrap().trim(), original);
    assert!(git
        .env
        .iter()
        .any(|(key, value)| key == "GIT_CEILING_DIRECTORIES"
            && value == work.parent().unwrap().as_os_str()));
    let (_source, url, _) = source();
    let base = read(json!({ "owner": OWNER, "dtag": "repo" })).unwrap();
    assert_eq!(
        go(&local_git(&work), &url, &base).unwrap()["commits"][0]["subject"],
        "second"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_git_selection_skips_unavailable_shim_without_launching_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let other = tempfile::tempdir().unwrap();
    let alternative = other.path().join("git");
    std::fs::write(&alternative, "fixture, never executed").unwrap();
    std::fs::set_permissions(&alternative, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths([Path::new("/usr/bin"), other.path()]).unwrap();
    let mut calls = 0;
    let selected = git_program_with_tools(&path, || {
        calls += 1;
        false
    });
    assert_eq!(selected, Some(alternative.clone()));
    assert_eq!(calls, 1);
    assert_eq!(
        git_program_with_tools(Path::new("/usr/bin").as_os_str(), || false),
        None
    );
    assert_eq!(
        git_program_with_tools(Path::new("/usr/bin").as_os_str(), || true),
        Some("/usr/bin/git".into())
    );
    let path = std::env::join_paths([other.path(), Path::new("/usr/bin")]).unwrap();
    assert_eq!(
        git_program_with_tools(&path, || panic!("shim probe should not run")),
        Some(alternative)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn unavailable_git_fails_before_spawning_or_fetching() {
    let work = tempfile::tempdir().unwrap();
    let mut git = production_git(work.path());
    git.program = None;
    let base = read(json!({ "owner": OWNER, "dtag": "repo" })).unwrap();
    assert_eq!(go(&git, "https://example.test/repo.git", &base), Err(503));
}

#[test]
fn read_input_matches_the_shared_contract() {
    let parsed = read(json!({ "owner": OWNER, "dtag": "buzz.app", "commit": "A".repeat(40), "path": "src/main.rs" })).unwrap();
    assert_eq!(parsed.owner, OWNER.to_ascii_lowercase());
    assert_eq!(parsed.commit, Some("a".repeat(40)));
    for bad in [
        json!({ "owner": "ab", "dtag": "repo" }),
        json!({ "owner": OWNER, "dtag": ".repo" }),
        json!({ "owner": OWNER, "dtag": "a..b" }),
        json!({ "owner": OWNER, "dtag": "a/b" }),
        json!({ "owner": OWNER, "dtag": "a".repeat(65) }),
        json!({ "owner": OWNER, "dtag": "repo", "commit": "abc" }),
        json!({ "owner": OWNER, "dtag": "repo", "path": "../etc" }),
        json!({ "owner": OWNER, "dtag": "repo", "path": "a//b" }),
        json!({ "owner": OWNER, "dtag": "repo", "path": "a\nb" }),
        json!({ "owner": OWNER, "dtag": "repo", "url": "https://elsewhere.test" }),
    ] {
        assert!(read(bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn reads_history_tree_readme_files_and_diffs() {
    let (_source, url, first) = source();
    let base = read(json!({ "owner": OWNER, "dtag": "repo" })).unwrap();
    let work = tempfile::tempdir().unwrap();
    let snapshot = go(&local_git(work.path()), &url, &base).unwrap();
    assert_eq!(snapshot["commits"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot["commits"][0]["subject"], "second");
    assert_eq!(snapshot["commits"][0]["author"], "Ada");
    assert_eq!(snapshot["commits"][0]["hash"], snapshot["head"]);
    assert_eq!(snapshot["commits"][1]["hash"], first);
    assert_eq!(snapshot["readme"], "# Hello\n");
    assert_eq!(snapshot["diff"], Value::Null);
    assert_eq!(snapshot["file"], Value::Null);
    let paths: Vec<_> = snapshot["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["README.md", "logo.bin", "src/main.rs"]);

    let work = tempfile::tempdir().unwrap();
    let file = read(json!({ "owner": OWNER, "dtag": "repo", "path": "src/main.rs" })).unwrap();
    let snapshot = go(&local_git(work.path()), &url, &file).unwrap();
    assert_eq!(
        snapshot["file"],
        json!({ "path": "src/main.rs", "content": "fn main() { println!(); }\n", "size": 26 })
    );

    let work = tempfile::tempdir().unwrap();
    let binary = read(json!({ "owner": OWNER, "dtag": "repo", "path": "logo.bin" })).unwrap();
    assert_eq!(
        go(&local_git(work.path()), &url, &binary).unwrap()["file"]["content"],
        Value::Null
    );

    let work = tempfile::tempdir().unwrap();
    let absent = read(json!({ "owner": OWNER, "dtag": "repo", "path": "missing.txt" })).unwrap();
    assert_eq!(go(&local_git(work.path()), &url, &absent), Err(404));

    let work = tempfile::tempdir().unwrap();
    let commit = read(json!({ "owner": OWNER, "dtag": "repo", "commit": first })).unwrap();
    let snapshot = go(&local_git(work.path()), &url, &commit).unwrap();
    assert_eq!(snapshot["head"], first);
    assert_eq!(snapshot["commits"].as_array().unwrap().len(), 1);
    let diff = snapshot["diff"].as_str().unwrap();
    assert!(
        diff.contains("first") && diff.contains("+fn main() {}"),
        "{diff}"
    );
}

#[test]
fn unadvertised_commits_are_read_from_head_history() {
    let (_source, url, first) = source();
    let work = tempfile::tempdir().unwrap();
    let commit = read(json!({ "owner": OWNER, "dtag": "repo", "commit": first })).unwrap();
    let snapshot = go(&relay_like(work.path()), &url, &commit).unwrap();
    assert_eq!(snapshot["head"], first);
    assert!(snapshot["diff"].as_str().unwrap().contains("+fn main() {}"));
    let work = tempfile::tempdir().unwrap();
    let absent = read(json!({ "owner": OWNER, "dtag": "repo", "commit": "b".repeat(40) })).unwrap();
    assert_eq!(go(&local_git(work.path()), &url, &absent), Err(502));
}

/// Protocol v0 refuses unadvertised wants, as the relay's smart HTTP does.
fn relay_like(directory: &Path) -> Git {
    let mut git = local_git(directory);
    let count = git
        .env
        .iter()
        .position(|(k, _)| k == "GIT_CONFIG_COUNT")
        .unwrap();
    let index: usize = git.env[count].1.to_str().unwrap().parse().unwrap();
    git.env[count].1 = (index + 1).to_string().into();
    git.env.push((
        format!("GIT_CONFIG_KEY_{index}").into(),
        "protocol.version".into(),
    ));
    git.env
        .push((format!("GIT_CONFIG_VALUE_{index}").into(), "0".into()));
    git
}

#[test]
fn head_history_never_serves_its_shallow_boundary() {
    let (source, url, _) = source();
    let mut stream = String::from("reset refs/heads/main\nfrom refs/heads/main^0\n");
    for n in 0..100 {
        stream += &format!(
            "commit refs/heads/main\ncommitter Ada <ada@example.test> 0 +0000\ndata 1\n.\nM 644 inline count.txt\ndata {}\n{n}\n",
            n.to_string().len()
        );
    }
    let mut import = StdCommand::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(source.path())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut import.stdin.take().unwrap(), stream.as_bytes()).unwrap();
    assert!(import.wait().unwrap().success());
    for (rev, expected) in [("HEAD~100", Err(502)), ("HEAD~99", Ok(()))] {
        let commit = sh(source.path(), &["rev-parse", rev]);
        let work = tempfile::tempdir().unwrap();
        let commit = read(json!({ "owner": OWNER, "dtag": "repo", "commit": commit })).unwrap();
        let snapshot = go(&relay_like(work.path()), &url, &commit);
        assert_eq!(snapshot.map(|_| ()), expected, "{rev}");
    }
}

#[test]
fn fetched_objects_are_bounded_during_ingestion() {
    let (source, url, _) = source();
    // Incompressible, so the pack stays above the budget.
    let mut state = 0x9e3779b97f4a7c15u64;
    let noise: Vec<u8> = (0..256 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    std::fs::write(source.path().join("noise.bin"), noise).unwrap();
    sh(source.path(), &["add", "."]);
    sh(source.path(), &["commit", "-qm", "noise"]);
    let base = read(json!({ "owner": OWNER, "dtag": "repo" })).unwrap();
    let work = tempfile::tempdir().unwrap();
    assert!(go(&local_git(work.path()), &url, &base).is_ok());
    let work = tempfile::tempdir().unwrap();
    let mut git = local_git(work.path());
    git.store_bytes = 128 * 1024;
    assert_eq!(go(&git, &url, &base), Err(502));
}

#[test]
fn empty_repositories_have_no_content_and_no_revisions() {
    let empty = tempfile::tempdir().unwrap();
    sh(empty.path(), &["init", "-q", "--bare"]);
    let url = url::Url::from_directory_path(empty.path())
        .unwrap()
        .to_string();
    let work = tempfile::tempdir().unwrap();
    let base = read(json!({ "owner": OWNER, "dtag": "repo" })).unwrap();
    assert_eq!(
        go(&local_git(work.path()), &url, &base).unwrap(),
        json!({ "head": null, "commits": [], "files": [], "readme": null, "file": null, "diff": null })
    );
    let path = read(json!({ "owner": OWNER, "dtag": "repo", "path": "README.md" })).unwrap();
    assert_eq!(go(&local_git(work.path()), &url, &path), Err(404));
}

#[test]
fn production_configuration_refuses_non_http_transport() {
    let (_source, url, _) = source();
    let work = tempfile::tempdir().unwrap();
    let base = read(json!({ "owner": OWNER, "dtag": "repo" })).unwrap();
    assert_eq!(go(&production_git(work.path()), &url, &base), Err(502));
}

/// A remote that accepts connections and never answers, so reads stay inside Git's helpers.
fn silent_remote() -> (String, std::sync::mpsc::Receiver<std::net::TcpStream>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/git/{OWNER}/repo.git",
        listener.local_addr().unwrap()
    );
    let (connections, accepted) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if connections.send(stream).is_err() {
                break;
            }
        }
    });
    (url, accepted)
}

fn closes(mut stream: std::net::TcpStream) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut buffer = [0; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return true,
            Ok(_) => continue,
            // Terminating a process that owns a TCP connection may reset it
            // rather than finish a graceful shutdown. Both end the connection.
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return true,
            Err(_) => return false,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_reads_release_slots_after_killing_remote_helpers() {
    let (url, accepted) = silent_remote();
    let start = |url: String| {
        let base = read(json!({ "owner": OWNER, "dtag": "repo" })).unwrap();
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(async move {
            serve(&url, &base, cancelled, async {
                Ok("Authorization: Nostr test".into())
            })
            .await
        });
        (cancel, task)
    };
    let connected = || accepted.recv_timeout(Duration::from_secs(5)).unwrap();
    let reads = [start(url.clone()), start(url.clone())];
    let streams = [connected(), connected()];
    for (cancel, task) in reads {
        cancel.send(()).unwrap();
        assert_eq!(task.await.unwrap().unwrap().status, CANCELLED);
    }
    // `git-remote-http`, not the direct child, holds each connection.
    for stream in streams {
        assert!(
            closes(stream),
            "a cancelled read left its remote helper running"
        );
    }
    // Both slots are free: a third read reaches the remote instead of waiting.
    let (cancel, task) = start(url);
    let stream = connected();
    cancel.send(()).unwrap();
    assert_eq!(task.await.unwrap().unwrap().status, CANCELLED);
    assert!(closes(stream));
}

/// Windows job termination is asynchronous: a killed descendant can still hold a handle in
/// the read's repository when the direct child has been reaped.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_reads_remove_repositories_held_by_descendants() {
    let scripts = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let held = directory.path().join("held");
    let ready = directory.path().join("ready");
    let literal = |path: &Path| path.to_string_lossy().replace('\'', "''");
    let inner = scripts.path().join("hold.ps1");
    std::fs::write(
        &inner,
        format!(
            "$file = [System.IO.File]::Open('{}', 'OpenOrCreate', 'ReadWrite', 'None')\n[System.IO.File]::WriteAllText('{}', '1')\nStart-Sleep -Seconds 30\n",
            literal(&held),
            literal(&ready)
        ),
    )
    .unwrap();
    let outer = scripts.path().join("launch.ps1");
    std::fs::write(
        &outer,
        format!(
            "Start-Process -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File','\"{}\"' -WindowStyle Hidden\nStart-Sleep -Seconds 30\n",
            literal(&inner)
        ),
    )
    .unwrap();
    let (stop, stopped) = watch::channel(false);
    let mut git = production_git(directory.path());
    git.program = Some("powershell.exe".into());
    git.stop = stopped;
    let outer = outer.to_string_lossy().into_owned();
    let run = tokio::spawn(async move {
        git.run(&[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &outer,
        ])
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant did not open the repository file");
    stop.send(true).unwrap();
    assert_eq!(run.await.unwrap(), Err(CANCELLED));
    let path = directory.path().to_path_buf();
    remove(directory).await;
    assert!(
        !path.exists(),
        "a cancelled read left its repository behind"
    );
}
