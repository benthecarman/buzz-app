//! Packaged repository reads with the development broker's `project-git` contract
//! (`browser-host/project-git.mjs`). The [`Signer`] signs the NIP-98 header; system Git reads into
//! an isolated bare repository that is removed after every outcome. No checkout, hooks,
//! submodules, user Git configuration, arbitrary URL or write RPC.
use super::{hex_key, origin, RelayResponse};
use crate::event::{EventTemplate, Signer};
use crate::upload::{upload_id, Uploads};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{oneshot, watch, Semaphore},
    time::Instant,
};

type Result<T> = std::result::Result<T, String>;

const READ_BYTES: usize = 4 * 1024 * 1024;
const TEXT_BYTES: u64 = 1024 * 1024;
/// Fetched objects are bounded separately: rendering limits apply only after ingestion.
const STORE_BYTES: u64 = 128 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(12);
/// Status of a read the renderer abandoned; the renderer has stopped listening.
const CANCELLED: u16 = 499;
static READS: Semaphore = Semaphore::const_new(2);

/// The upload registry's semantics: a cancel may overtake its read IPC.
fn cancels() -> &'static Uploads {
    static CANCELS: OnceLock<Uploads> = OnceLock::new();
    CANCELS.get_or_init(Uploads::default)
}

/// One repository read: the repository's owner and `d` tag, and optionally
/// a commit to show and a file to read.
#[derive(serde::Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitRead {
    pub owner: String,
    pub dtag: String,
    pub commit: Option<String>,
    pub path: Option<String>,
}

fn git_hash(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Mirrors `parseGitRead` in `src/features/projects/git.ts`.
fn parse_read(read: GitRead) -> Result<GitRead> {
    let dtag = read.dtag.as_bytes();
    let valid = hex_key(&read.owner.to_ascii_lowercase())
        && !dtag.is_empty()
        && dtag.len() <= 64
        && dtag[0] != b'.'
        && dtag
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !read.dtag.contains("..")
        && read.commit.as_deref().map_or(true, git_hash)
        && read.path.as_deref().map_or(true, |path| {
            path.encode_utf16().count() <= 4096
                && !path.chars().any(|c| (c as u32) < 32 || c as u32 == 127)
                && path
                    .split('/')
                    .all(|p| !p.is_empty() && p != "." && p != "..")
        });
    if !valid {
        return Err("Invalid Git read".into());
    }
    Ok(GitRead {
        owner: read.owner.to_ascii_lowercase(),
        commit: read.commit.map(|commit| commit.to_ascii_lowercase()),
        ..read
    })
}

fn reply(status: u16, body: &Value) -> RelayResponse {
    RelayResponse {
        status,
        headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
        body: body.to_string(),
    }
}

fn failure(status: u16) -> RelayResponse {
    reply(
        status,
        &json!({ "error": "Repository content could not be read" }),
    )
}

/// Reads a community repository into a fresh bare repository with system
/// Git and returns the snapshot as a relay-shaped JSON response. `id` names
/// the read for [`cancel`]; a cancelled read answers status 499.
pub async fn read<S: Signer>(
    host: &S,
    community: &str,
    id: &str,
    read: GitRead,
) -> Result<RelayResponse> {
    let read = parse_read(read)?;
    let id = upload_id(Some(id)).map_err(|_| "Invalid Git read")?;
    let url = format!("{}git/{}/{}.git", origin(community)?, read.owner, read.dtag);
    let Some(cancelled) = cancels()
        .start(id)
        .map_err(|_| "Repository reads are busy")?
    else {
        return Ok(failure(CANCELLED));
    };
    let response = serve(&url, &read, cancelled, sign(host, &url)).await;
    cancels().finish(id);
    response
}

/// Cancels read `id`, even if it has not started yet.
pub fn cancel(id: &str) -> Result<()> {
    cancels().cancel(upload_id(Some(id)).map_err(|_| "Invalid Git read")?);
    Ok(())
}

async fn sign<S: Signer>(host: &S, url: &str) -> Result<String> {
    let auth = host
        .sign(EventTemplate {
            kind: 27235,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "System clock is unavailable")?
                .as_secs(),
            content: String::new(),
            tags: vec![
                vec!["u".into(), url.into()],
                vec!["method".into(), "GET".into()],
            ],
        })
        .await?;
    Ok(format!(
        "Authorization: Nostr {}",
        STANDARD.encode(
            serde_json::to_vec(&auth).map_err(|_| "Could not encode relay authentication")?
        )
    ))
}

/// Cancellation and the deadline stop Git inside `Git::run`, which reaps its process tree,
/// so the read slot and temporary repository are released only after Git is gone.
async fn serve(
    url: &str,
    read: &GitRead,
    mut cancelled: oneshot::Receiver<()>,
    auth: impl Future<Output = Result<String>>,
) -> Result<RelayResponse> {
    let deadline = Instant::now() + DEADLINE;
    let (cancel, stop) = watch::channel(false);
    let work = async {
        let _slot = tokio::select! {
            slot = READS.acquire() => slot.map_err(|_| "Repository read failed")?,
            status = stopped(deadline, stop.clone()) => return Ok(failure(status)),
        };
        let header = auth.await?;
        let directory = tempfile::Builder::new()
            .prefix("buzz-project-read-")
            .tempdir()
            .map_err(|_| "Repository read failed")?;
        let git = Git::new(directory.path(), &header, deadline, stop);
        let response = match fetch_snapshot(&git, url, read).await {
            Ok(snapshot) => reply(200, &snapshot),
            Err(status) => failure(status),
        };
        remove(directory).await;
        Ok(response)
    };
    tokio::pin!(work);
    tokio::select! {
        response = &mut work => response,
        _ = &mut cancelled => {
            let _ = cancel.send(true);
            work.await
        }
    }
}

async fn stopped(deadline: Instant, mut stop: watch::Receiver<bool>) -> u16 {
    tokio::select! {
        _ = tokio::time::sleep_until(deadline) => 503,
        _ = stop.wait_for(|stopped| *stopped) => CANCELLED,
    }
}

/// `TempDir`'s drop discards removal errors. Windows can refuse removal briefly while a
/// killed helper's handles close, so removal is retried and a final failure is reported.
async fn remove(directory: tempfile::TempDir) {
    let path = directory.keep();
    for _ in 0..20 {
        match std::fs::remove_dir_all(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            _ => return,
        }
    }
    eprintln!("Could not remove repository read {}", path.display());
}

/// Bytes stored under `path`, the read's temporary repository.
fn stored(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => stored(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Failures are HTTP-like statuses; Git's argv and stderr never leave this module.
type Read<T> = std::result::Result<T, u16>;

struct Git {
    program: Option<PathBuf>,
    directory: PathBuf,
    env: Vec<(OsString, OsString)>,
    deadline: Instant,
    stop: watch::Receiver<bool>,
    store_bytes: u64,
}

/// Only the Git read has this macOS availability rule. The Apple shim is an
/// executable file but can present an installation dialog when tools are missing.
#[cfg(target_os = "macos")]
fn git_program(path: &std::ffi::OsStr) -> Option<PathBuf> {
    git_program_with_tools(path, || {
        std::process::Command::new("/usr/bin/xcode-select")
            .arg("-p")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

#[cfg(target_os = "macos")]
fn git_program_with_tools(
    path: &std::ffi::OsStr,
    tools_available: impl FnOnce() -> bool,
) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let shim = Path::new("/usr/bin/git");
    let mut tools_available = Some(tools_available);
    std::env::split_paths(path)
        .map(|directory| directory.join("git"))
        .find(|candidate| {
            if !std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            {
                return false;
            }
            candidate != shim || tools_available.take().is_some_and(|probe| probe())
        })
}

#[cfg(not(target_os = "macos"))]
fn git_program(path: &std::ffi::OsStr) -> Option<PathBuf> {
    Some(crate::process::resolve_program("git", path))
}

impl Git {
    fn new(directory: &Path, header: &str, deadline: Instant, stop: watch::Receiver<bool>) -> Self {
        // `ls-remote` runs before `init`. A ceiling at the read directory's
        // parent prevents discovery of a repository there or above it.
        let ceiling = directory
            .parent()
            .expect("temporary read directory has a parent");
        let path = crate::process::effective_path();
        let settings = [
            ("http.extraHeader", header),
            ("http.followRedirects", "false"),
            ("credential.helper", ""),
            ("core.hooksPath", "/dev/null"),
            ("protocol.allow", "never"),
            ("protocol.http.allow", "always"),
            ("protocol.https.allow", "always"),
            ("fetch.fsckObjects", "true"),
            ("transfer.fsckObjects", "true"),
        ];
        let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| !name.to_string_lossy().starts_with("GIT_"))
            .filter(|(name, _)| !name.eq_ignore_ascii_case("PATH"))
            .collect();
        env.extend([
            ("PATH".into(), path.clone()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CEILING_DIRECTORIES".into(), ceiling.as_os_str().into()),
            // Git for Windows maps `/dev/null` to `NUL`.
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_CONFIG_COUNT".into(), settings.len().to_string().into()),
        ]);
        for (index, (name, value)) in settings.iter().enumerate() {
            env.push((format!("GIT_CONFIG_KEY_{index}").into(), (*name).into()));
            env.push((format!("GIT_CONFIG_VALUE_{index}").into(), (*value).into()));
        }
        Self {
            program: git_program(&path),
            directory: directory.into(),
            env,
            deadline,
            stop,
            store_bytes: STORE_BYTES,
        }
    }

    /// Stdout and stored objects are bounded. On failure, timeout or cancellation the whole
    /// process tree (remote helpers, index-pack) is killed and Git is reaped before returning.
    async fn run(&self, args: &[&str]) -> Read<Vec<u8>> {
        let mut command = Command::new(self.program.as_ref().ok_or(503u16)?);
        command
            .args(args)
            .current_dir(&self.directory)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.as_std_mut().process_group(0);
        #[cfg(windows)]
        let job = crate::process::windows_job::WindowsJob::new().ok_or(502u16)?;
        #[cfg(windows)]
        let mut child = job.spawn_hidden(&mut command).ok_or(502u16)?;
        #[cfg(not(windows))]
        let mut child = command.spawn().map_err(|_| 502u16)?;
        #[cfg(unix)]
        let mut group = crate::process::ProcessGroupGuard {
            process_id: child.id().ok_or(502u16)? as i32,
            armed: true,
        };
        let stdout = child.stdout.take().ok_or(502u16)?;
        let finished = async {
            let mut output = Vec::new();
            stdout
                .take(READ_BYTES as u64 + 1)
                .read_to_end(&mut output)
                .await
                .map_err(|_| 502u16)?;
            if output.len() > READ_BYTES
                || !child.wait().await.map_err(|_| 502u16)?.success()
                || stored(&self.directory) > self.store_bytes
            {
                return Err(502);
            }
            Ok(output)
        };
        let outcome = tokio::select! {
            outcome = finished => outcome,
            status = stopped(self.deadline, self.stop.clone()) => Err(status),
            status = self.over_budget() => Err(status),
        };
        if outcome.is_err() {
            #[cfg(unix)]
            group.kill();
            #[cfg(windows)]
            job.terminate(Duration::from_secs(2)).await;
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        #[cfg(unix)]
        {
            group.armed = false;
        }
        outcome
    }

    async fn over_budget(&self) -> u16 {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if stored(&self.directory) > self.store_bytes {
                return 502;
            }
        }
    }
}

async fn fetch_snapshot(git: &Git, url: &str, read: &GitRead) -> Read<Value> {
    // An authenticated empty advertisement is an empty repository, not a failed
    // HEAD fetch. Discover object format before initializing the isolated repo.
    let refs = String::from_utf8_lossy(&git.run(&["ls-remote", "--symref", "--", url]).await?)
        .into_owned();
    if refs.trim().is_empty() {
        if read.commit.is_some() || read.path.is_some() {
            return Err(404);
        }
        return Ok(json!({
            "head": null, "commits": [], "files": [], "readme": null, "file": null, "diff": null
        }));
    }
    let sha256 = read.commit.as_ref().is_some_and(|c| c.len() == 64)
        || refs.lines().any(|line| {
            line.split_once('\t')
                .is_some_and(|(hash, _)| hash.len() == 64 && git_hash(hash))
        });
    let mut init = vec!["init", "--bare"];
    if sha256 {
        init.push("--object-format=sha256");
    }
    git.run(&init).await?;
    let fetch = |revision| ["fetch", "--no-tags", "--depth=101", "--", url, revision];
    let tip = match (
        git.run(&fetch(read.commit.as_deref().unwrap_or("HEAD")))
            .await,
        &read.commit,
    ) {
        (Ok(_), _) => "FETCH_HEAD",
        // Relays refuse unadvertised commits; listed history comes from HEAD.
        (Err(502), Some(commit)) => {
            git.run(&fetch("HEAD")).await?;
            commit
        }
        (Err(status), _) => return Err(status),
    };
    snapshot(git, read, tip).await
}

struct File {
    hash: String,
    size: u64,
    path: String,
}

/// Git plumbing uses NUL record boundaries; filenames never become arguments.
async fn snapshot(git: &Git, read: &GitRead, tip: &str) -> Read<Value> {
    let head = String::from_utf8_lossy(
        &git.run(&["rev-parse", "--verify", &format!("{tip}^{{commit}}")])
            .await?,
    )
    .trim()
    .to_string();
    if read.commit.as_ref().is_some_and(|commit| *commit != head) {
        return Err(502);
    }
    // HEAD's fetched history ends at shallow commits, which `git show` would render as roots.
    let shallow = std::fs::read_to_string(git.directory.join("shallow")).unwrap_or_default();
    if read.commit.is_some() && shallow.lines().any(|line| line == head) {
        return Err(502);
    }
    let tree = git.run(&["ls-tree", "-r", "-l", "-z", &head]).await?;
    let files: Vec<File> = String::from_utf8_lossy(&tree)
        .split('\0')
        .filter_map(|record| {
            let (meta, path) = record.split_once('\t')?;
            let mut meta = meta.split_whitespace();
            let (_mode, kind, hash, size) =
                (meta.next()?, meta.next()?, meta.next()?, meta.next()?);
            (kind == "blob" && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(())?;
            Some(File {
                hash: hash.into(),
                size: size.parse().ok()?,
                path: path.into(),
            })
        })
        .collect();
    if files.len() > 20_000 {
        return Err(502);
    }
    let history = git
        .run(&[
            "log",
            "-100",
            "--format=%H%x00%an%x00%at%x00%s%x00",
            &head,
            "--",
        ])
        .await?;
    let history = String::from_utf8_lossy(&history);
    let parts: Vec<&str> = history.split('\0').collect();
    let commits: Vec<Value> = parts
        .chunks_exact(4)
        .map(|c| {
            json!({
                "hash": c[0].trim(),
                "author": c[1],
                "date": c[2].parse::<u64>().unwrap_or_default(),
                "subject": c[3],
            })
        })
        .collect();
    let selected = match &read.path {
        Some(path) => Some(files.iter().find(|f| f.path == *path).ok_or(404u16)?),
        None => None,
    };
    let readme = files.iter().find(|f| {
        let name = f.path.to_ascii_lowercase();
        ["readme", "readme.md", "readme.txt", "readme.rst"].contains(&name.as_str())
    });
    let file = match selected {
        Some(f) => {
            json!({ "path": f.path, "content": content(git, Some(f)).await?, "size": f.size })
        }
        None => Value::Null,
    };
    let diff = match read.commit {
        Some(_) => Value::String(
            String::from_utf8_lossy(
                &git.run(&[
                    "show",
                    "--format=fuller",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    &head,
                    "--",
                ])
                .await?,
            )
            .into_owned(),
        ),
        None => Value::Null,
    };
    let snapshot = json!({
        "head": head,
        "commits": commits,
        "files": files.iter().map(|f| json!({ "path": f.path, "hash": f.hash, "size": f.size })).collect::<Vec<_>>(),
        "readme": content(git, readme).await?,
        "file": file,
        "diff": diff,
    });
    if snapshot.to_string().len() > READ_BYTES {
        return Err(502);
    }
    Ok(snapshot)
}

async fn content(git: &Git, file: Option<&File>) -> Read<Option<String>> {
    let Some(file) = file.filter(|f| f.size <= TEXT_BYTES) else {
        return Ok(None);
    };
    let bytes = git.run(&["cat-file", "blob", &file.hash]).await?;
    Ok((!bytes.contains(&0))
        .then(|| String::from_utf8(bytes).ok())
        .flatten())
}

#[cfg(test)]
#[path = "project_git_tests.rs"]
mod tests;
