//! Canonical Codex CLI/adapter binding shared by discovery and launch.
//! Resolution never authenticates, reads a model catalog, or mutates Codex state.
use crate::{installed, Result};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const MAX_SHEBANG: usize = 512;
const PASSTHROUGH: &[&str] = &[
    "HOME",
    "CODEX_HOME",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "LANG",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Stable identity for the native-owned Codex integration.
pub const INTEGRATION_ID: &str = "codex";

/// A resolved executable and, for a script, its exact interpreter.
#[derive(Clone, PartialEq, Eq)]
struct BoundExecutable {
    selected: PathBuf,
    program: PathBuf,
    script: Option<PathBuf>,
}

/// Native-only binding. Config files and authentication can change without any
/// path or environment value changing, so equality never proves a working login.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexContext {
    /// Canonical selected ACP adapter path.
    pub adapter: PathBuf,
    /// Canonical selected Codex CLI path.
    pub cli: PathBuf,
    /// Exact adapter script interpreter, when the adapter is a script.
    pub interpreter: Option<PathBuf>,
    /// Exact CLI script interpreter, when the CLI is a script.
    pub cli_interpreter: Option<PathBuf>,
    /// Canonical workspace used by every command in this context.
    pub workspace: PathBuf,
    adapter_command: BoundExecutable,
    cli_command: BoundExecutable,
    environment: BTreeMap<OsString, OsString>,
}

impl CodexContext {
    /// Resolve the device-selected adapter and CLI without falling back to an
    /// adapter-bundled Codex engine.
    pub fn installed(workspace: &Path, app_data: Option<&Path>) -> Result<Self> {
        Self::installed_with(workspace, &BTreeMap::new(), app_data)
    }

    /// Resolve the installed pair with the agent's effective environment. Only
    /// the fixed Codex binding allowlist is admitted into child processes.
    pub fn installed_with(
        workspace: &Path,
        overrides: &BTreeMap<String, String>,
        app_data: Option<&Path>,
    ) -> Result<Self> {
        let adapter = installed_adapter(app_data).ok_or("Codex ACP adapter not found")?;
        let cli = installed("codex").ok_or("Codex CLI not found")?;
        Self::new(&adapter, &cli, workspace, overrides)
    }

    /// Resolve the agent's selected adapter without replacing it with a newly
    /// discovered global install. A missing saved adapter is a launch error.
    pub fn for_agent(
        adapter: &str,
        workspace: &Path,
        effective: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let cli = installed("codex").ok_or("Codex CLI not found")?;
        Self::new(
            Path::new(adapter),
            &cli,
            workspace,
            &agent_environment(effective),
        )
    }

    /// Resolve an explicit pair. This is the later per-agent binding seam and
    /// also keeps tests from depending on ambient tools.
    pub fn new(
        adapter: &Path,
        cli: &Path,
        workspace: &Path,
        overrides: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let workspace = workspace
            .canonicalize()
            .map_err(|_| "Codex workspace does not exist")?;
        if !workspace.is_dir() {
            return Err("Codex workspace is not a directory".into());
        }
        let mut directories = Vec::new();
        for path in [adapter, cli] {
            if let Some(parent) = path.parent().filter(|path| path.is_absolute()) {
                directories.push(parent.to_path_buf());
            }
        }
        directories.extend(default_directories());
        // An app-owned adapter runs on its pinned Node; the CLI keeps its own.
        let adapter_directories: Vec<_> = managed_node(adapter)
            .and_then(|node| node.parent().map(Path::to_path_buf))
            .into_iter()
            .chain(directories.iter().cloned())
            .collect();
        let adapter_command = bind(adapter, &adapter_directories, "Codex ACP adapter")?;
        let cli_command = bind(cli, &directories, "Codex CLI")?;
        if let Some(path) = &adapter_command.script {
            let value = path.as_os_str().to_string_lossy();
            if value.contains(',') || value.trim() != value {
                return Err(
                    "Codex ACP adapter path cannot contain a comma or surrounding whitespace"
                        .into(),
                );
            }
        }
        let adapter = adapter_command.selected.clone();
        let cli = cli_command.selected.clone();
        let interpreter = adapter_command
            .script
            .as_ref()
            .map(|_| adapter_command.program.clone());
        let cli_interpreter = cli_command
            .script
            .as_ref()
            .map(|_| cli_command.program.clone());

        if std::env::var_os("CODEX_CONFIG").is_some() || overrides.contains_key("CODEX_CONFIG") {
            return Err("CODEX_CONFIG is not supported by Codex agents yet".into());
        }
        let mut environment = BTreeMap::new();
        for key in PASSTHROUGH {
            if let Some(value) = std::env::var_os(key) {
                environment.insert(OsString::from(key), value);
            }
        }
        for (key, value) in overrides {
            if !PASSTHROUGH.contains(&key.as_str()) {
                return Err(format!("Codex binding does not permit {key}"));
            }
            environment.insert(key.into(), value.into());
        }
        environment.insert("CODEX_PATH".into(), cli.as_os_str().to_owned());
        environment.insert("INITIAL_AGENT_MODE".into(), "agent-full-access".into());
        let path = std::env::join_paths(
            [
                cli_interpreter.as_deref().and_then(Path::parent),
                interpreter.as_deref().and_then(Path::parent),
                cli.parent(),
                adapter.parent(),
                Some(Path::new("/usr/bin")),
                Some(Path::new("/bin")),
                Some(Path::new("/usr/sbin")),
                Some(Path::new("/sbin")),
            ]
            .into_iter()
            .flatten()
            .map(Path::to_path_buf)
            .chain(default_directories()),
        )
        .map_err(|_| "Invalid Codex tools path")?;
        environment.insert("PATH".into(), path);
        Ok(Self {
            adapter,
            cli,
            interpreter,
            cli_interpreter,
            workspace,
            adapter_command,
            cli_command,
            environment,
        })
    }

    /// Exact adapter command with isolated effective context.
    pub fn adapter_command(&self) -> Command {
        self.command(&self.adapter_command)
    }

    /// Exact installed CLI command with the same context as the adapter.
    pub fn cli_command(&self) -> Command {
        self.command(&self.cli_command)
    }

    /// Exact executable and comma-transport-safe arguments for bundled Buzz ACP.
    pub(crate) fn adapter_launch(&self, configured: &[String]) -> Result<(PathBuf, Vec<String>)> {
        if !configured.is_empty() {
            return Err("Native Codex does not accept custom adapter arguments".into());
        }
        let mut args = Vec::with_capacity(usize::from(self.adapter_command.script.is_some()));
        if let Some(script) = &self.adapter_command.script {
            args.push(script.to_string_lossy().into_owned());
        }
        if args
            .iter()
            .any(|arg| arg.is_empty() || arg.contains(',') || arg.trim() != arg)
        {
            return Err("Codex ACP adapter arguments must be nonempty, comma-free, and have no surrounding whitespace".into());
        }
        Ok((self.adapter_command.program.clone(), args))
    }

    /// Apply the isolated CLI, interpreter, full-access, and path binding.
    /// Host-owned identity and relay values are added later.
    pub(crate) fn apply_launch_environment(
        &self,
        command: &mut Command,
        runtime_directory: Option<&Path>,
    ) -> Result<()> {
        let path = self
            .environment
            .get(OsStr::new("PATH"))
            .ok_or("Codex binding has no tools path")?;
        let path = std::env::join_paths(
            runtime_directory
                .map(Path::to_path_buf)
                .into_iter()
                .chain(std::env::split_paths(path)),
        )
        .map_err(|_| "Invalid Codex runtime tools path")?;
        command
            .env_clear()
            .envs(&self.environment)
            .env("PATH", path)
            .current_dir(&self.workspace);
        // Like every other harness, agents reach the host SSH agent for git.
        if let Some(socket) = std::env::var_os("SSH_AUTH_SOCK") {
            command.env("SSH_AUTH_SOCK", socket);
        }
        Ok(())
    }

    fn command(&self, executable: &BoundExecutable) -> Command {
        let mut command = Command::new(&executable.program);
        if let Some(script) = &executable.script {
            command.arg(script);
        }
        command
            .env_clear()
            .envs(&self.environment)
            .current_dir(&self.workspace)
            .stdin(Stdio::null());
        command
    }
}

fn agent_environment(effective: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    effective
        .iter()
        .filter(|(key, _)| PASSTHROUGH.contains(&key.as_str()) || key.as_str() == "CODEX_CONFIG")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Native Codex records accept only binding inputs plus the existing host-owned
/// worker-count preference. Legacy records are not retroactively constrained.
pub(crate) fn validate_native_environment(effective: &BTreeMap<String, String>) -> Result<()> {
    if let Some(key) = effective
        .keys()
        .find(|key| !PASSTHROUGH.contains(&key.as_str()) && key.as_str() != "BUZZ_ACP_AGENTS")
    {
        return Err(format!("Codex binding does not permit {key}"));
    }
    Ok(())
}

/// The one adapter lookup shared by Settings, discovery, and new selections.
pub fn installed_adapter(app_data: Option<&Path>) -> Option<PathBuf> {
    adapter_choice(installed("codex-acp"), app_data)
}

/// As for Claude Code, a user install wins. Otherwise the app-owned adapter is
/// used only when its pinned Node is installed to run it.
fn adapter_choice(user: Option<PathBuf>, app_data: Option<&Path>) -> Option<PathBuf> {
    user.or_else(|| {
        let app_data = app_data?;
        crate::managed_tool(app_data, "node")?;
        crate::managed_tool(app_data, "codex-acp")
    })
}

/// The pinned Node beside an app-owned `<app_data>/codex-tools/bin` adapter.
fn managed_node(adapter: &Path) -> Option<PathBuf> {
    let bin = adapter
        .parent()
        .filter(|bin| bin.ends_with("codex-tools/bin"))?;
    crate::managed_tool(bin.parent()?.parent()?, "node")
}

fn bind(path: &Path, directories: &[PathBuf], label: &str) -> Result<BoundExecutable> {
    if !path.is_absolute() {
        return Err(format!("{label} path must be absolute"));
    }
    executable(path).map_err(|_| format!("{label} is missing or not executable"))?;
    let selected = path
        .canonicalize()
        .map_err(|_| format!("Could not canonicalize {label}"))?;
    let Some(interpreter) = shebang(&selected)? else {
        return Ok(BoundExecutable {
            selected: selected.clone(),
            program: selected,
            script: None,
        });
    };
    let program = if interpreter == Path::new("/usr/bin/env") {
        resolve("node", directories).ok_or("Codex adapter and CLI scripts require Node.js")?
    } else {
        if !interpreter.is_absolute() {
            return Err(format!("{label} uses a relative interpreter"));
        }
        resolve(interpreter.as_os_str(), directories)
            .ok_or_else(|| format!("{label} interpreter is missing"))?
    };
    Ok(BoundExecutable {
        selected: selected.clone(),
        program,
        script: Some(selected),
    })
}

fn shebang(path: &Path) -> Result<Option<PathBuf>> {
    let mut bytes = [0u8; MAX_SHEBANG + 1];
    let count = std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut bytes))
        .map_err(|_| "Could not inspect Codex executable")?;
    if !bytes[..count].starts_with(b"#!") {
        return Ok(None);
    }
    let end = bytes[..count]
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or("Codex executable has an oversized interpreter line")?;
    let line = std::str::from_utf8(&bytes[2..end])
        .map_err(|_| "Codex executable has an invalid interpreter line")?;
    let mut words = line.split_whitespace();
    let interpreter = words
        .next()
        .filter(|value| !value.is_empty())
        .ok_or("Codex executable has no interpreter")?;
    if interpreter == "/usr/bin/env" {
        if words.next() != Some("node") || words.next().is_some() {
            return Err("Codex executable uses an unsupported env interpreter".into());
        }
    } else if words.next().is_some() {
        return Err("Codex executable uses unsupported interpreter arguments".into());
    }
    Ok(Some(interpreter.into()))
}

fn resolve(name: impl AsRef<OsStr>, directories: &[PathBuf]) -> Option<PathBuf> {
    let name = Path::new(name.as_ref());
    let candidates: Vec<_> = if name.is_absolute() {
        vec![name.to_path_buf()]
    } else {
        directories
            .iter()
            .map(|directory| directory.join(name))
            .collect()
    };
    candidates.into_iter().find_map(|path| {
        executable(&path).ok()?;
        path.canonicalize().ok()
    })
}

fn executable(path: &Path) -> Result<()> {
    let metadata = path.metadata().map_err(|_| "Executable is missing")?;
    if !metadata.is_file() {
        return Err("Executable is not a file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("File is not executable".into());
        }
    }
    Ok(())
}

fn default_directories() -> Vec<PathBuf> {
    crate::tools_path()
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

#[cfg(all(test, unix))]
mod tests;
