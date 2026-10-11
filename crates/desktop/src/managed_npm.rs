//! Pinned, app-owned Node and npm tools. No user-global installs.
use crate::harness_setup::HarnessSetup;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Old Buzz: desktop/src-tauri/src/commands/agent_discovery/managed_node.rs:8-43,
// :300-323, :362-424. Checksums are pinned to the official Node v24.18.0 tarballs.
const VERSION: &str = "v24.18.0";
const MAX_ARCHIVE: u64 = 90 * 1024 * 1024;
// Native steering needs Pi's steer disposition, added in 0.99.0.
const PI: &str = "@earendil-works/pi-coding-agent@>=0.99.0";
const ADAPTER: &str = "git+https://github.com/salman1993/buzz-pi-acp.git#72015de";

// Both packages are pinned to versions available through the configured npm registry.
const CLAUDE: &str = "@anthropic-ai/claude-code@2.1.289";
const CLAUDE_ADAPTER: &str = "@agentclientprotocol/claude-agent-acp@0.85.1";
// Adapter only: Codex binds to the user's installed CLI and its login.
const CODEX_ADAPTER: &str = "@agentclientprotocol/codex-acp@2.1.1";

#[derive(Clone, Copy)]
pub(crate) enum Harness {
    Pi,
    Claude,
    Codex,
}
impl Harness {
    fn prefix(self) -> &'static str {
        match self {
            Self::Pi => "node-tools",
            Self::Claude => "claude-tools",
            Self::Codex => "codex-tools",
        }
    }
    fn revision(self) -> &'static str {
        match self {
            Self::Pi => adapter_rev(),
            Self::Claude => "claude-2.1.289-acp-0.85.1",
            Self::Codex => "codex-acp-2.1.1",
        }
    }
    fn binaries(self) -> &'static [&'static str] {
        match self {
            Self::Pi => &["pi", "buzz-pi-acp"],
            Self::Claude => &["claude", "claude-agent-acp"],
            Self::Codex => &["codex-acp"],
        }
    }
    fn packages(self) -> &'static [(&'static str, bool, &'static str)] {
        match self {
            Self::Pi => &[
                (PI, false, "Installing Pi failed"),
                (ADAPTER, true, "Installing the Pi ACP adapter failed"),
            ],
            Self::Claude => &[
                (CLAUDE, false, "Installing Claude Code failed"),
                (
                    CLAUDE_ADAPTER,
                    false,
                    "Installing the Claude Code ACP adapter failed",
                ),
            ],
            Self::Codex => &[(
                CODEX_ADAPTER,
                false,
                "Installing the Codex ACP adapter failed",
            )],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Artifact {
    platform: &'static str,
    filename: &'static str,
    sha256: &'static str,
}

fn artifact(os: &str, arch: &str) -> Option<Artifact> {
    let (platform, sha256) = match (os, arch) {
        ("macos", "aarch64") => (
            "darwin-arm64",
            "e1a97e14c99c803e96c7339403282ea05a499c32f8d83defe9ef5ec66f979ed1",
        ),
        ("macos", "x86_64") => (
            "darwin-x64",
            "dfd0dbd3e721503434df7b7205e719f61b3a3a31b2bcf9729b8b91fea240f080",
        ),
        ("linux", "x86_64") => (
            "linux-x64",
            "783130984963db7ba9cbd01089eaf2c2efb055c7c1693c943174b967b3050cb8",
        ),
        ("linux", "aarch64") => (
            "linux-arm64",
            "6b4484c2190274175df9aa8f28e2d758a819cb1c1fe6ab481e2f95b463ab8508",
        ),
        _ => return None,
    };
    Some(Artifact {
        platform,
        filename: match platform {
            "darwin-arm64" => "node-v24.18.0-darwin-arm64.tar.gz",
            "darwin-x64" => "node-v24.18.0-darwin-x64.tar.gz",
            "linux-x64" => "node-v24.18.0-linux-x64.tar.gz",
            _ => "node-v24.18.0-linux-arm64.tar.gz",
        },
        sha256,
    })
}

fn refuse_linked_prefix(prefix: &Path) -> Result<(), String> {
    for path in [
        prefix.to_path_buf(),
        prefix.join("bin"),
        prefix.join("lib"),
        prefix.join("lib/node_modules"),
        prefix.join("lib/node_modules/@earendil-works"),
        prefix.join("lib/node_modules/@earendil-works/pi-coding-agent"),
        prefix.join("lib/node_modules/buzz-pi-acp"),
        prefix.join("lib/node_modules/@anthropic-ai"),
        prefix.join("lib/node_modules/@agentclientprotocol"),
        prefix.join("cache"),
        prefix.join("etc"),
        prefix.join("releases"),
    ] {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err("App-owned npm prefix contains a link or non-directory".into())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Could not inspect app-owned npm prefix".into()),
            _ => {}
        }
    }
    Ok(())
}

fn node_dir(app_data: &Path, spec: Artifact) -> PathBuf {
    app_data
        .join("runtimes/node")
        .join(VERSION)
        .join(spec.platform)
}

fn verified(archive: &[u8], spec: Artifact) -> Result<(), String> {
    if archive.len() as u64 > MAX_ARCHIVE || format!("{:x}", Sha256::digest(archive)) != spec.sha256
    {
        return Err("Managed Node archive checksum or size mismatch".into());
    }
    Ok(())
}

async fn download(spec: Artifact) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not prepare managed Node download")?;
    let url = format!("https://nodejs.org/dist/{VERSION}/{}", spec.filename);
    let mut response =
        client.get(url).send().await.map_err(|_| {
            "Could not download Node.js from nodejs.org; check your network or proxy"
        })?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|size| size > MAX_ARCHIVE)
    {
        return Err("Managed Node download was unavailable or too large".into());
    }
    let mut archive = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Managed Node download interrupted")?
    {
        if archive.len() as u64 + chunk.len() as u64 > MAX_ARCHIVE {
            return Err("Managed Node download exceeded size limit".into());
        }
        archive.extend_from_slice(&chunk);
    }
    verified(&archive, spec)?;
    Ok(archive)
}

fn scrub(
    command: &mut tokio::process::Command,
    home: &Path,
    node_bin: &Path,
    app_data: &Path,
) -> Result<(), String> {
    let path = std::env::join_paths([
        node_bin,
        Path::new("/usr/bin"),
        Path::new("/bin"),
        Path::new("/usr/sbin"),
        Path::new("/sbin"),
    ])
    .map_err(|_| "Invalid managed Node PATH")?;
    command.env_clear();
    // npm reads registry, auth, CA and proxy settings from ~/.npmrc (real HOME)
    // and from npm_config_* variables. Keep those so a mirror still works, but
    // never let them move the app-owned prefix, cache or global config.
    for (name, value) in std::env::vars_os() {
        let Some(key) = name.to_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        if key.starts_with("npm_config_")
            && !matches!(
                key.as_str(),
                "npm_config_prefix" | "npm_config_cache" | "npm_config_globalconfig"
            )
        {
            command.env(name, value);
        }
    }
    for name in [
        "TMPDIR",
        "USER",
        "LOGNAME",
        "LANG",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "http_proxy",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("HOME", home)
        .env("PATH", path)
        .env("npm_config_cache", app_data.join("node-tools/cache"))
        .env("npm_config_prefix", app_data.join("node-tools"))
        .env(
            "npm_config_globalconfig",
            app_data.join("node-tools/etc/npmrc"),
        )
        .current_dir(home)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    Ok(())
}

async fn run_step(
    setup: &HarnessSetup,
    command: &mut tokio::process::Command,
    log: &File,
    failure: &str,
) -> Result<(), String> {
    let start = log
        .metadata()
        .map_err(|_| "Could not inspect install log")?
        .len();
    command
        .stdout(Stdio::from(
            log.try_clone().map_err(|_| "Could not copy install log")?,
        ))
        .stderr(Stdio::from(
            log.try_clone().map_err(|_| "Could not copy install log")?,
        ));
    let mut child = setup
        .spawn(|| command.spawn())
        .map_err(|error| format!("{failure}: {error}"))?;
    let status = tokio::time::timeout(Duration::from_secs(300), child.child.wait())
        .await
        .map_err(|_| format!("{failure}: timed out. See the install log."))?
        .map_err(|_| format!("{failure}: could not finish. See the install log."))?;
    child.reaped = true;
    drop(child);
    if !status.success() {
        let exit = status.code().map_or_else(
            || "process terminated".to_owned(),
            |code| format!("exit code {code}"),
        );
        let detail =
            npm_error(log, start).unwrap_or_else(|| "See the install log for details.".into());
        return Err(format!("{failure} ({exit}).\n{detail}"));
    }
    Ok(())
}

// Keep the visible diagnostic short; the private log retains full output.
// Only read this step's output so an earlier npm invocation cannot supply its error.
fn npm_error(log: &File, start: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = log.try_clone().ok()?;
    let end = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(start.max(end.saturating_sub(8192))))
        .ok()?;
    let mut tail = Vec::new();
    file.take(8192).read_to_end(&mut tail).ok()?;
    let output = String::from_utf8_lossy(&tail);
    let lines: Vec<_> = output
        .lines()
        .filter(|line| line.starts_with("npm error ") || line.starts_with("npm ERR! "))
        .filter(|line| !line.contains("A complete log of this run"))
        .take(6)
        .map(|line| line.chars().take(200).collect::<String>())
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The system's `tar`: `/usr/bin/tar` where it exists, else the system
/// profile's on NixOS, which has no `/usr/bin`. A `tar` found on `PATH` is
/// the last resort.
fn system_tar() -> Option<PathBuf> {
    ["/usr/bin/tar", "/bin/tar", "/run/current-system/sw/bin/tar"]
        .into_iter()
        .map(PathBuf::from)
        .chain(
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|dir| dir.join("tar")),
        )
        .find(|tar| tar.is_file())
}

async fn install_node(
    setup: &HarnessSetup,
    app_data: &Path,
    home: &Path,
    log: &File,
    spec: Artifact,
) -> Result<PathBuf, String> {
    let node_dir = node_dir(app_data, spec);
    let node = node_dir.join("bin/node");
    let npm = node_dir.join("lib/node_modules/npm/bin/npm-cli.js");
    if node.is_file() && npm.is_file() {
        return Ok(node);
    }
    let archive = download(spec).await?;
    let root = app_data.join("runtimes/node");
    std::fs::create_dir_all(&root).map_err(|_| "Could not create managed Node directory")?;
    let stage = root.join(format!("{}.{}.tmp", VERSION, spec.platform));
    if stage.exists() {
        std::fs::remove_dir_all(&stage)
            .map_err(|_| "Could not remove stale Node staging directory")?;
    }
    std::fs::create_dir_all(&stage).map_err(|_| "Could not stage managed Node")?;
    let archive_path = stage.join("node.tar.gz");
    let result = async {
        std::fs::write(&archive_path, archive).map_err(|_| "Could not stage Node archive")?;
        let tar_path = system_tar().ok_or("Could not find tar to unpack Node.js")?;
        let mut tar = tokio::process::Command::new(&tar_path);
        // Its own folder is the PATH, for the gzip `-z` runs.
        scrub(
            &mut tar,
            home,
            tar_path.parent().unwrap_or(Path::new("/usr/bin")),
            app_data,
        )?;
        tar.arg("-xzf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&stage)
            .arg("--strip-components=1");
        run_step(setup, &mut tar, log, "Unpacking Node.js failed").await?;
        std::fs::remove_file(&archive_path).map_err(|_| "Could not remove Node archive")?;
        if !stage.join("bin/node").is_file()
            || !stage.join("lib/node_modules/npm/bin/npm-cli.js").is_file()
        {
            return Err("Managed Node archive was incomplete".into());
        }
        std::fs::create_dir_all(node_dir.parent().ok_or("Invalid Node path")?)
            .map_err(|_| "Could not create Node version directory")?;
        if node_dir.exists() {
            std::fs::remove_dir_all(&node_dir)
                .map_err(|_| "Could not replace incomplete Node runtime")?;
        }
        std::fs::rename(&stage, &node_dir).map_err(|_| "Could not activate managed Node")?;
        Ok(node)
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(stage);
    }
    result
}

fn npm_command(
    node: &Path,
    app_data: &Path,
    home: &Path,
    prefix: &Path,
    package: &str,
    install_links: bool,
) -> Result<tokio::process::Command, String> {
    let bin = node.parent().ok_or("Invalid managed Node path")?;
    let npm = bin
        .parent()
        .ok_or("Invalid managed Node path")?
        .join("lib/node_modules/npm/bin/npm-cli.js");
    let mut command = tokio::process::Command::new(node);
    scrub(&mut command, home, bin, app_data)?;
    command
        .arg(npm)
        .args(["install", "--global", "--prefix"])
        .arg(prefix);
    if install_links {
        command.arg("--install-links=true");
    }
    command.arg(package);
    Ok(command)
}

fn adapter_rev() -> &'static str {
    ADAPTER.rsplit_once('#').map_or(ADAPTER, |(_, rev)| rev)
}

/// The release an app-owned shim points at: `../releases/<id>/bin/<name>`.
fn release_id(shim: &Path) -> Option<String> {
    let target = std::fs::read_link(shim).ok()?;
    let rest = target.strip_prefix("../releases").ok()?;
    rest.components()
        .next()?
        .as_os_str()
        .to_str()
        .map(str::to_owned)
}

/// Whether both app-owned shims point at a release of the pinned adapter.
pub fn current(app_data: &Path) -> bool {
    let bin = app_data.join("node-tools/bin");
    ["pi", "buzz-pi-acp"].iter().all(|name| {
        release_id(&bin.join(name))
            .is_some_and(|id| id.split_once('.').map(|(rev, _)| rev) == Some(adapter_rev()))
    })
}

// Each shim moves by rename, so a starting agent sees an old or a new complete
// release. Pi moves first because the previous adapter also runs on newer Pi.
fn activate(tools: &Path, id: &str, harness: Harness) -> Result<(), String> {
    let bin = tools.join("bin");
    std::fs::create_dir_all(&bin).map_err(|_| "Could not create app-owned tools directory")?;
    for name in harness.binaries() {
        let staged = bin.join(format!(".{name}.{id}"));
        let _ = std::fs::remove_file(&staged);
        std::os::unix::fs::symlink(
            Path::new("../releases").join(id).join("bin").join(name),
            &staged,
        )
        .and_then(|()| std::fs::rename(&staged, bin.join(name)))
        .map_err(|_| "Could not activate the new Harness install")?;
    }
    Ok(())
}

// Keep the previous release for agents still running from it.
fn prune(releases: &Path, keep: &[&str]) {
    let Ok(entries) = std::fs::read_dir(releases) else {
        return;
    };
    for entry in entries.flatten() {
        if !keep.iter().any(|id| entry.file_name() == *id) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

pub(crate) async fn install(
    setup: &HarnessSetup,
    app_data: &Path,
    log: File,
    harness: Harness,
) -> Result<bool, String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("Harness install requires HOME")?);
    let spec = artifact(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or("Managed Node is unavailable on this platform")?;
    let node = install_node(setup, app_data, &home, &log, spec).await?;
    let tools = app_data.join(harness.prefix());
    refuse_linked_prefix(&tools)?;
    let releases = tools.join("releases");
    std::fs::create_dir_all(&releases).map_err(|_| "Could not create app-owned npm prefix")?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let id = format!("{}.{millis}", harness.revision());
    let prefix = releases.join(&id);
    std::fs::create_dir(&prefix).map_err(|_| "Could not create app-owned npm prefix")?;
    let result = async {
        for &(package, install_links, failure) in harness.packages() {
            refuse_linked_prefix(&tools)?;
            refuse_linked_prefix(&prefix)?;
            let mut command = npm_command(&node, app_data, &home, &prefix, package, install_links)?;
            run_step(setup, &mut command, &log, failure).await?;
        }
        if !matches!(harness, Harness::Pi) {
            for name in harness.binaries() {
                let mut command = tokio::process::Command::new(prefix.join("bin").join(name));
                scrub(
                    &mut command,
                    &home,
                    node.parent().ok_or("Invalid managed Node path")?,
                    app_data,
                )?;
                command.arg("--version");
                run_step(
                    setup,
                    &mut command,
                    &log,
                    &format!("Verifying {name} failed"),
                )
                .await?;
            }
        }
        Ok::<_, String>(())
    }
    .await;
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&prefix);
        return Err(error);
    }
    let previous = harness
        .binaries()
        .last()
        .and_then(|name| release_id(&tools.join("bin").join(name)));
    activate(&tools, &id, harness)?;
    let mut keep = vec![id.as_str()];
    keep.extend(previous.as_deref());
    prune(&releases, &keep);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn claude_install_rejects_native_stub_before_activation_and_keeps_pi_releases() {
        let dir = tempfile::tempdir().unwrap();
        let app_data = dir.path();
        let setup = HarnessSetup::default();
        let spec = artifact(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let node_root = node_dir(app_data, spec);
        std::fs::create_dir_all(node_root.join("bin")).unwrap();
        std::fs::create_dir_all(node_root.join("lib/node_modules/npm/bin")).unwrap();
        std::fs::write(node_root.join("lib/node_modules/npm/bin/npm-cli.js"), "").unwrap();
        use crate::test_executable::write_executable;
        let node = node_root.join("bin/node");
        // A local npm boundary fixture creates the requested package's launcher.
        // Claude's native package can leave a failing stub despite npm success.
        let npm = |cli_exit| {
            format!(
                r#"#!/bin/sh
set -eu
prefix="$5"
case "$6" in
  @anthropic-ai/claude-code@*) name=claude; code={cli_exit} ;;
  @agentclientprotocol/claude-agent-acp@*) name=claude-agent-acp; code=0 ;;
  *) exit 2 ;;
esac
mkdir -p "$prefix/bin"
printf '#!/bin/sh\nexit %s\n' "$code" > "$prefix/bin/$name"
chmod 755 "$prefix/bin/$name"
"#
            )
        };
        let tools = app_data.join("claude-tools");
        let old = tools.join("releases/previous/bin");
        std::fs::create_dir_all(&old).unwrap();
        for name in ["claude", "claude-agent-acp"] {
            write_executable(&old.join(name), "#!/bin/sh\nexit 0\n");
        }
        activate(&tools, "previous", Harness::Claude).unwrap();
        let pi_release = app_data.join("node-tools/releases/pi-existing/bin");
        std::fs::create_dir_all(&pi_release).unwrap();
        std::fs::write(pi_release.join("pi"), "existing Pi").unwrap();
        let log_path = app_data.join("install.log");
        write_executable(&node, npm(1));
        let error = install(
            &setup,
            app_data,
            File::create(&log_path).unwrap(),
            Harness::Claude,
        )
        .await
        .unwrap_err();
        assert!(error.contains("Verifying claude failed"), "{error}");
        assert_eq!(
            release_id(&tools.join("bin/claude")),
            Some("previous".into())
        );
        assert_eq!(
            std::fs::read_dir(tools.join("releases")).unwrap().count(),
            1
        );
        write_executable(&node, npm(0));
        assert!(install(
            &setup,
            app_data,
            File::create(&log_path).unwrap(),
            Harness::Claude
        )
        .await
        .unwrap());
        assert_ne!(
            release_id(&tools.join("bin/claude")),
            Some("previous".into())
        );
        assert!(old.join("claude").is_file());
        assert_eq!(
            std::fs::read_to_string(pi_release.join("pi")).unwrap(),
            "existing Pi"
        );
        // A later Pi prune cannot reach the Claude release directory either.
        prune(&app_data.join("node-tools/releases"), &["pi-existing"]);
        assert!(tools.join("bin/claude").is_file());
        assert!(tools.join("bin/claude-agent-acp").is_file());
    }
    #[tokio::test]
    async fn failed_steps_report_their_own_npm_error_and_preserve_the_log() {
        use std::io::Write;
        let setup = HarnessSetup::default();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install.log");
        let mut log = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        writeln!(
            log,
            "npm error code EACCES\nstale output from an earlier step"
        )
        .unwrap();
        for (step, output) in [
            (
                "Installing Pi failed",
                "npm error code E404\nnpm error 404 No match found for version >=0.99.0",
            ),
            (
                "Installing the Pi ACP adapter failed",
                "npm error code ENOENT\nnpm error spawn git ENOENT",
            ),
            ("Installing Pi failed", ""),
        ] {
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .args(["-c", "printf '%s\\n' \"$1\" >&2; exit 1", "fixture", output])
                .process_group(0)
                .kill_on_drop(true);
            let error = run_step(&setup, &mut command, &log, step)
                .await
                .unwrap_err();
            assert!(
                error.starts_with(&format!("{step} (exit code 1).")),
                "{error}"
            );
            if !output.is_empty() {
                assert!(error.contains(output), "{error}");
                assert!(std::fs::read_to_string(&path).unwrap().contains(output));
            }
            assert!(!error.contains("EACCES"), "{error}");
            if step.contains("adapter") || output.is_empty() {
                assert!(!error.contains("E404"), "{error}");
            }
        }
    }
    #[test]
    fn pinned_artifacts_are_platform_specific_and_reject_unsupported_os() {
        for (os, arch, platform) in [
            ("macos", "aarch64", "darwin-arm64"),
            ("macos", "x86_64", "darwin-x64"),
            ("linux", "x86_64", "linux-x64"),
            ("linux", "aarch64", "linux-arm64"),
        ] {
            let spec = artifact(os, arch).unwrap();
            assert_eq!(spec.platform, platform);
            assert_eq!(spec.sha256.len(), 64);
            assert!(spec.filename.ends_with(&format!("{platform}.tar.gz")));
        }
        assert!(artifact("windows", "x86_64").is_none());
        assert!(artifact("linux", "riscv64").is_none());
    }
    #[test]
    fn mismatch_rejects_download_before_extraction() {
        assert!(verified(b"untrusted bytes", artifact("macos", "aarch64").unwrap()).is_err());
    }
    #[test]
    fn scrubbed_npm_environment_has_only_managed_node_and_system_tools() {
        let mut cmd = tokio::process::Command::new("/usr/bin/env");
        scrub(
            &mut cmd,
            Path::new("/temporary/home"),
            Path::new("/managed/node/bin"),
            Path::new("/app/data"),
        )
        .unwrap();
        let env: Vec<_> = cmd.as_std().get_envs().collect();
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| *value)
        };
        assert_eq!(
            get("PATH"),
            Some(std::ffi::OsStr::new(
                "/managed/node/bin:/usr/bin:/bin:/usr/sbin:/sbin"
            ))
        );
        assert_eq!(get("HOME"), Some(std::ffi::OsStr::new("/temporary/home")));
        assert_eq!(
            get("npm_config_prefix"),
            Some(std::ffi::OsStr::new("/app/data/node-tools"))
        );
        if let Some(ca) = std::env::var_os("NODE_EXTRA_CA_CERTS") {
            assert_eq!(get("NODE_EXTRA_CA_CERTS"), Some(ca.as_os_str()));
        }
        assert!(env.iter().all(|(name, _)| *name != "BUZZ_PRIVATE_KEY"));
        assert_eq!(
            get("npm_config_globalconfig"),
            Some(std::ffi::OsStr::new("/app/data/node-tools/etc/npmrc"))
        );
    }
    #[test]
    fn npm_mirror_settings_pass_through_but_cannot_move_the_prefix() {
        // Process env is shared; use names no other test reads.
        unsafe {
            std::env::set_var("NPM_CONFIG_REGISTRY", "https://mirror.example/");
            std::env::set_var("npm_config_userconfig", "/home/me/.npmrc");
            std::env::set_var("NPM_CONFIG_PREFIX", "/usr/local");
            std::env::set_var("NPM_CONFIG_GLOBALCONFIG", "/etc/npmrc");
        }
        let mut cmd = tokio::process::Command::new("/usr/bin/env");
        scrub(
            &mut cmd,
            Path::new("/home/me"),
            Path::new("/managed/node/bin"),
            Path::new("/app/data"),
        )
        .unwrap();
        let env: std::collections::BTreeMap<_, _> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v?.to_str()?.to_owned())))
            .collect();
        assert_eq!(env["NPM_CONFIG_REGISTRY"], "https://mirror.example/");
        assert_eq!(env["npm_config_userconfig"], "/home/me/.npmrc");
        assert_eq!(env["HOME"], "/home/me");
        assert!(!env.contains_key("NPM_CONFIG_PREFIX"));
        assert!(!env.contains_key("NPM_CONFIG_GLOBALCONFIG"));
        assert_eq!(env["npm_config_prefix"], "/app/data/node-tools");
    }
    #[cfg(unix)]
    #[test]
    fn linked_npm_prefix_cannot_redirect_install_to_user_global_files() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("node-tools");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&other, &prefix).unwrap();
        assert!(refuse_linked_prefix(&prefix).is_err());
        std::fs::remove_file(&prefix).unwrap();
        std::fs::create_dir_all(&prefix).unwrap();
        for destination in [
            "lib",
            "lib/node_modules",
            "lib/node_modules/@earendil-works",
            "lib/node_modules/@earendil-works/pi-coding-agent",
            "lib/node_modules/buzz-pi-acp",
            "lib/node_modules/@anthropic-ai",
            "lib/node_modules/@agentclientprotocol",
            "etc",
            "releases",
        ] {
            let path = prefix.join(destination);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&other, &path).unwrap();
            assert!(refuse_linked_prefix(&prefix).is_err(), "{destination}");
            std::fs::remove_file(path).unwrap();
        }
        assert!(refuse_linked_prefix(&prefix).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn activation_swaps_both_shims_to_a_complete_release_and_prunes_older_ones() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("node-tools");
        let release = |id: &str| {
            let bin = tools.join("releases").join(id).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            for name in ["pi", "buzz-pi-acp"] {
                std::fs::write(bin.join(name), id).unwrap();
                std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        };
        let adapter = || std::fs::read_to_string(tools.join("bin/buzz-pi-acp")).unwrap();
        // An install from before releases: npm's own shim into node-tools/lib.
        std::fs::create_dir_all(tools.join("bin")).unwrap();
        std::os::unix::fs::symlink(
            "../lib/node_modules/buzz-pi-acp/dist/index.js",
            tools.join("bin/buzz-pi-acp"),
        )
        .unwrap();
        assert!(!current(dir.path()));

        let old = "86b201e.1";
        let pinned = format!("{}.2", adapter_rev());
        let newer = format!("{}.3", adapter_rev());
        release(old);
        activate(&tools, old, Harness::Pi).unwrap();
        assert_eq!(adapter(), old);
        assert!(!current(dir.path()));

        release(&pinned);
        activate(&tools, &pinned, Harness::Pi).unwrap();
        assert!(current(dir.path()));
        assert_eq!(adapter(), pinned);
        assert_eq!(
            std::fs::read_to_string(tools.join("bin/pi")).unwrap(),
            pinned
        );
        assert!(buzz_agent_controller::managed_tool(dir.path(), "buzz-pi-acp").is_some());

        release(&newer);
        std::fs::create_dir_all(tools.join("releases/failed.4")).unwrap();
        prune(&tools.join("releases"), &[newer.as_str(), pinned.as_str()]);
        let mut left: Vec<_> = std::fs::read_dir(tools.join("releases"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [pinned, newer]);
    }
    #[tokio::test]
    async fn codex_install_activates_only_a_verified_pinned_adapter() {
        use crate::test_executable::write_executable;
        let dir = tempfile::tempdir().unwrap();
        let app_data = dir.path();
        let setup = HarnessSetup::default();
        let spec = artifact(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let node_root = node_dir(app_data, spec);
        std::fs::create_dir_all(node_root.join("bin")).unwrap();
        std::fs::create_dir_all(node_root.join("lib/node_modules/npm/bin")).unwrap();
        std::fs::write(node_root.join("lib/node_modules/npm/bin/npm-cli.js"), "").unwrap();
        // A local npm boundary fixture accepts only the pinned adapter package.
        let npm = |version_exit| {
            format!(
                r#"#!/bin/sh
set -eu
[ "$6" = "{CODEX_ADAPTER}" ] || exit 2
mkdir -p "$5/bin"
printf '#!/bin/sh\nexit {version_exit}\n' > "$5/bin/codex-acp"
chmod 755 "$5/bin/codex-acp"
"#
            )
        };
        let node = node_root.join("bin/node");
        let log_path = app_data.join("install.log");
        write_executable(&node, npm(1));
        let error = install(
            &setup,
            app_data,
            File::create(&log_path).unwrap(),
            Harness::Codex,
        )
        .await
        .unwrap_err();
        assert!(error.contains("Verifying codex-acp failed"), "{error}");
        assert!(buzz_agent_controller::managed_tool(app_data, "codex-acp").is_none());
        write_executable(&node, npm(0));
        assert!(install(
            &setup,
            app_data,
            File::create(&log_path).unwrap(),
            Harness::Codex
        )
        .await
        .unwrap());
        let shim = buzz_agent_controller::managed_tool(app_data, "codex-acp").unwrap();
        assert_eq!(shim, app_data.join("codex-tools/bin/codex-acp"));
        assert!(release_id(&shim).unwrap().starts_with("codex-acp-2.1.1."));
        assert!(!app_data.join("claude-tools").exists());
    }
    #[test]
    fn npm_installs_only_the_two_approved_packages_into_the_app_owned_prefix() {
        let node = Path::new("/app/data/runtimes/node/v24.18.0/darwin-arm64/bin/node");
        let prefix = Path::new("/app/data/node-tools/releases/72015de.1");
        let pi = npm_command(
            node,
            Path::new("/app/data"),
            Path::new("/temporary/home"),
            prefix,
            PI,
            false,
        )
        .unwrap();
        let adapter = npm_command(
            node,
            Path::new("/app/data"),
            Path::new("/temporary/home"),
            prefix,
            ADAPTER,
            true,
        )
        .unwrap();
        let args = |cmd: &tokio::process::Command| {
            cmd.as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            args(&pi),
            [
                "/app/data/runtimes/node/v24.18.0/darwin-arm64/lib/node_modules/npm/bin/npm-cli.js",
                "install",
                "--global",
                "--prefix",
                "/app/data/node-tools/releases/72015de.1",
                PI
            ]
        );
        assert_eq!(
            args(&adapter),
            [
                "/app/data/runtimes/node/v24.18.0/darwin-arm64/lib/node_modules/npm/bin/npm-cli.js",
                "install",
                "--global",
                "--prefix",
                "/app/data/node-tools/releases/72015de.1",
                "--install-links=true",
                ADAPTER
            ]
        );
    }
}
