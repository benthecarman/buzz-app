//! App lifetime, not page/plugin lifetime. Native startup uses app-owned resources.
pub(crate) mod owner;
use buzz_agent_controller::{
    Action, AgentEdit, ControlSnapshot, Controller, Credentials, ImportPreview, Imports,
    LegacySource, NewAgent, PlatformCredentials, ProcessStatus, RuntimeBundle, Store,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    #[serde(flatten)]
    pub(crate) data: ControlSnapshot,
    inventory_warnings: Vec<String>,
    import_available: bool,
    create_available: bool,
    avatar_editing_available: bool,
    local_inventory_actions: bool,
    default_workspace: String,
    harness_options: Vec<HarnessOption>,
    claude_setup: ClaudeSetup,
    databricks_defaults: crate::agent_models::Defaults,
    agent_defaults: buzz_agent_controller::BuildDefaults,
    /// Running agents restarted by this save; absent on other responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    restarted: Option<usize>,
    /// Agents whose automatic restart after this save failed (not skipped).
    #[serde(skip_serializing_if = "Option::is_none")]
    restart_failures: Option<usize>,
}
impl Snapshot {
    fn from(
        data: ControlSnapshot,
        import_available: bool,
        workspace: &std::path::Path,
        app_data: &std::path::Path,
    ) -> Self {
        Self {
            data,
            inventory_warnings: Vec::new(),
            import_available,
            // Platforms with a native credential store for the new identity.
            create_available: cfg!(any(
                target_os = "macos",
                target_os = "windows",
                target_os = "linux"
            )),
            avatar_editing_available: true,
            local_inventory_actions: true,
            default_workspace: workspace.to_string_lossy().into_owned(),
            harness_options: harness_options(app_data),
            claude_setup: claude_setup(app_data),
            databricks_defaults: crate::agent_models::defaults(),
            agent_defaults: buzz_agent_controller::build_defaults(),
            restarted: None,
            restart_failures: None,
        }
    }
}
// Editing suggestions and executable presence only. Availability does not
// establish provider credentials or an ACP session.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessOption {
    command: String,
    label: &'static str,
    available: bool,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_supported: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_supported: Option<bool>,
    default_args: Vec<String>,
    providers: &'static [ProviderOption],
    configuration_policy: buzz_agent_controller::HarnessConfigurationPolicy,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSetup {
    status: &'static str,
    install_supported: bool,
    login_command: Option<String>,
    #[serde(skip)]
    cli: Option<PathBuf>,
    #[serde(skip)]
    node: Option<PathBuf>,
    #[serde(skip)]
    adapter: Option<PathBuf>,
}

fn claude_setup(app_data: &std::path::Path) -> ClaudeSetup {
    let cli = buzz_agent_controller::installed_npm_tool("claude");
    let managed_cli = buzz_agent_controller::managed_tool(app_data, "claude");
    let managed_node = buzz_agent_controller::managed_tool(app_data, "node");
    let (adapter, status, managed) = npm_choice(
        NpmTools {
            cli: cli.clone(),
            adapter: buzz_agent_controller::installed_npm_tool("claude-agent-acp"),
            node: buzz_agent_controller::installed_npm_tool("node"),
        },
        NpmTools {
            cli: managed_cli.clone(),
            adapter: buzz_agent_controller::managed_tool(app_data, "claude-agent-acp"),
            node: managed_node.clone(),
        },
    );
    let quote = |path: &std::path::Path| {
        format!(
            "'{}'",
            path.to_string_lossy()
                .replace('\'', if cfg!(windows) { "''" } else { "'\\''" })
        )
    };
    let cli = if managed { managed_cli.or(cli) } else { cli };
    let node = managed.then_some(managed_node).flatten();
    let login_command = cli.as_ref().map(|cli| {
        if let Some(node_bin) = node.as_ref().and_then(|node| node.parent()) {
            format!(
                "PATH={}:\"$PATH\" {} auth login",
                quote(node_bin),
                quote(cli)
            )
        } else {
            format!(
                "{}{} auth login",
                if cfg!(windows) { "& " } else { "" },
                quote(cli)
            )
        }
    });
    ClaudeSetup {
        status,
        install_supported: cfg!(all(
            any(target_os = "macos", target_os = "linux"),
            any(target_arch = "x86_64", target_arch = "aarch64")
        )),
        login_command,
        cli,
        node,
        adapter,
    }
}
/// Explicit Settings read, never part of periodic snapshots or controller writes.
#[tauri::command]
pub(crate) async fn claude_auth_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Option<bool> {
    use tauri::Manager as _;
    let app_data = app.path().app_data_dir().ok()?;
    prepare_tools_path().await;
    let setup = claude_setup(&app_data);
    let path = claude_auth_path(&setup)?;
    probe_claude_auth(setup.cli.as_ref()?, &path).await
}

fn claude_auth_path(setup: &ClaudeSetup) -> Option<std::ffi::OsString> {
    let mut path = buzz_agent_controller::tools_path().ok()?;
    if let Some(node_bin) = setup.node.as_ref().and_then(|node| node.parent()) {
        path = std::env::join_paths(
            std::iter::once(node_bin.to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .ok()?;
    }
    Some(path)
}

async fn probe_claude_auth(cli: &std::path::Path, path: &std::ffi::OsStr) -> Option<bool> {
    let (output, status) = crate::host_command::run_output(
        cli,
        &["auth".into(), "status".into()],
        std::time::Duration::from_secs(5),
        path,
        4096,
    )
    .await?;
    let logged_in = serde_json::from_str::<serde_json::Value>(&output)
        .ok()?
        .get("loggedIn")?
        .as_bool()?;
    match (status.code(), logged_in) {
        (Some(0), true) => Some(true),
        (Some(1), false) => Some(false),
        _ => None,
    }
}

#[derive(Serialize)]
struct ProviderOption {
    value: &'static str,
    label: &'static str,
}
// Common IDs checked against Goose's provider registry (crates/goose/src/providers/init.rs)
// and declarative provider definitions. Custom IDs remain editable.
const GOOSE_PROVIDERS: &[ProviderOption] = &[
    ProviderOption {
        value: "anthropic",
        label: "Anthropic",
    },
    ProviderOption {
        value: "openai",
        label: "OpenAI",
    },
    ProviderOption {
        value: "openrouter",
        label: "OpenRouter",
    },
    ProviderOption {
        value: "google",
        label: "Google Gemini",
    },
    ProviderOption {
        value: "github_copilot",
        label: "GitHub Copilot",
    },
    ProviderOption {
        value: "databricks",
        label: "Databricks",
    },
    ProviderOption {
        value: "databricks_v2",
        label: "Databricks v2",
    },
    ProviderOption {
        value: "ollama",
        label: "Ollama",
    },
    ProviderOption {
        value: "groq",
        label: "Groq",
    },
    ProviderOption {
        value: "mistral",
        label: "Mistral AI",
    },
    ProviderOption {
        value: "together",
        label: "Together AI",
    },
    ProviderOption {
        value: "perplexity",
        label: "Perplexity",
    },
    ProviderOption {
        value: "cerebras",
        label: "Cerebras",
    },
    ProviderOption {
        value: "custom_deepseek",
        label: "DeepSeek",
    },
];

fn npm_status(cli: bool, adapter: bool, node: bool) -> &'static str {
    if !cli || !node {
        "cli-needed"
    } else if !adapter {
        "adapter-needed"
    } else {
        "ready"
    }
}

struct NpmTools {
    cli: Option<PathBuf>,
    adapter: Option<PathBuf>,
    node: Option<PathBuf>,
}

fn npm_choice(user: NpmTools, managed: NpmTools) -> (Option<PathBuf>, &'static str, bool) {
    // An existing, complete user install always wins. Otherwise use the
    // app-owned pair only when its pinned Node can run its npm shims.
    let user_ready = user.cli.is_some() && user.adapter.is_some() && user.node.is_some();
    let managed_selected = !user_ready && managed.adapter.is_some() && managed.node.is_some();
    let selected = if user_ready {
        user
    } else if managed_selected {
        NpmTools {
            cli: managed.cli.or(user.cli),
            ..managed
        }
    } else {
        user
    };
    let status = npm_status(
        selected.cli.is_some(),
        selected.adapter.is_some(),
        selected.node.is_some(),
    );
    (selected.adapter, status, managed_selected)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn pi_current(app_data: &std::path::Path) -> bool {
    buzz_desktop::managed_npm::current(app_data)
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn pi_current(_: &std::path::Path) -> bool {
    true
}

fn harness_options(app_data: &std::path::Path) -> Vec<HarnessOption> {
    let (pi, pi_status, pi_managed) = npm_choice(
        NpmTools {
            cli: buzz_agent_controller::installed("pi"),
            adapter: buzz_agent_controller::installed("buzz-pi-acp"),
            node: buzz_agent_controller::installed("node"),
        },
        NpmTools {
            cli: buzz_agent_controller::managed_tool(app_data, "pi"),
            adapter: buzz_agent_controller::managed_tool(app_data, "buzz-pi-acp"),
            node: buzz_agent_controller::managed_tool(app_data, "node"),
        },
    );
    let mut options =
        vec![
            HarnessOption {
                command: "buzz-agent".into(),
                configuration_policy:
                    buzz_agent_controller::HarnessConfigurationPolicy::for_command("buzz-agent"),
                label: "Buzz Agent",
                available: true,
                status: "ready",
                install_supported: None,
                update_supported: None,
                default_args: vec![],
                // Windows refuses Databricks sign-in (DATABRICKS_WINDOWS): omit it.
                providers: &[
                    ProviderOption {
                        value: "databricks_v2",
                        label: "Databricks v2",
                    },
                    ProviderOption {
                        value: "openai",
                        label: "OpenAI",
                    },
                ][usize::from(cfg!(windows))..],
            },
            HarnessOption {
                command: "goose".into(),
                configuration_policy:
                    buzz_agent_controller::HarnessConfigurationPolicy::for_command("goose"),
                label: "Goose",
                available: true,
                status: "ready",
                install_supported: None,
                update_supported: None,
                default_args: vec![],
                providers: GOOSE_PROVIDERS,
            },
            HarnessOption {
                configuration_policy:
                    buzz_agent_controller::HarnessConfigurationPolicy::for_command("buzz-pi-acp"),
                command: pi.map_or_else(
                    || "buzz-pi-acp".into(),
                    |p| p.to_string_lossy().into_owned(),
                ),
                label: "Pi",
                available: pi_status == "ready",
                status: pi_status,
                install_supported: Some(cfg!(all(
                    any(target_os = "macos", target_os = "linux"),
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ))),
                update_supported: Some(pi_managed && pi_status == "ready" && !pi_current(app_data)),
                default_args: vec![],
                // Pi reports signed-in providers through its model catalog.
                providers: &[],
            },
        ];
    options.extend(
        buzz_agent_controller::harness_presets()
            .iter()
            .filter(|preset| preset.id != "claude")
            .map(|preset| preset_option(preset, buzz_agent_controller::installed(&preset.command))),
    );
    let claude = claude_setup(app_data);
    options.push(HarnessOption {
        configuration_policy: buzz_agent_controller::HarnessConfigurationPolicy::for_command(
            "claude-agent-acp",
        ),
        command: claude.adapter.map_or_else(
            || "claude-agent-acp".into(),
            |path| path.to_string_lossy().into_owned(),
        ),
        label: "Claude Code",
        available: claude.status == "ready",
        status: claude.status,
        install_supported: Some(claude.install_supported),
        update_supported: Some(false),
        default_args: vec![],
        providers: &[],
    });
    options
}

fn preset_option(
    preset: &'static buzz_agent_controller::HarnessPreset,
    command: Option<PathBuf>,
) -> HarnessOption {
    HarnessOption {
        configuration_policy: buzz_agent_controller::HarnessConfigurationPolicy::for_command(
            &preset.command,
        ),
        available: command.is_some(),
        status: if command.is_some() {
            "ready"
        } else {
            "cli-needed"
        },
        command: command.map_or_else(
            || preset.command.clone(),
            |p| p.to_string_lossy().into_owned(),
        ),
        label: &preset.label,
        install_supported: Some(false),
        update_supported: Some(false),
        default_args: preset.args.clone(),
        providers: &[],
    }
}

struct LogChallenge {
    id: String,
    pubkey: String,
    relay_url: String,
    nonce: String,
    issued: std::time::Instant,
}

struct PendingStart {
    ticket: u64,
    workspace: Option<String>,
    status: ProcessStatus,
    revision: u64,
    replay_floor: Option<u64>,
}
struct MentionReplay {
    revision: u64,
    floor: u64,
}

pub(crate) struct Host {
    inventory_warnings: Vec<String>,
    pub(crate) controller: Controller,
    imports: Imports,
    legacy_parent: PathBuf,
    workspace: PathBuf,
    app_data: PathBuf,
    closed: bool,
    credentials: Arc<dyn Credentials>,
    starts: BTreeMap<String, PendingStart>,
    queued: BTreeMap<String, Option<MentionReplay>>,
    next_start: u64,
    /// Agents with an explicit Start/Stop since open; queued restore skips them.
    acted: BTreeSet<String>,
    profiles: BTreeMap<String, Arc<tokio::sync::Mutex<()>>>,
    log_challenges: BTreeMap<String, LogChallenge>,
    creating: Option<(String, Arc<NewAgent>)>,
    legacy_check: fn() -> Result<(), String>,
}
impl Host {
    fn open(
        root: PathBuf,
        legacy_parent: PathBuf,
        workspace: PathBuf,
        bundle: Result<RuntimeBundle, String>,
        credentials: Arc<dyn Credentials>,
    ) -> Result<Self, String> {
        // Provision before restoring agents; failures remain nonfatal, as in old Buzz.
        if let Err(error) = buzz_agent_controller::ensure_buzz_cli_skill(&workspace) {
            eprintln!("buzz: failed to install CLI skill: {error}");
        }
        let app_data = root
            .parent()
            .ok_or("Invalid local agent storage")?
            .to_path_buf();
        let mut store = Store::open(root)?;
        let mut inventory_warnings = store.migrate_legacy(&legacy_parent);
        inventory_warnings.extend(store.clean_imported_prompts()?);
        let queued = store
            .snapshot()?
            .agents
            .into_iter()
            .filter_map(|agent| {
                startup_trace(serde_json::json!({
                    "phase": "selection", "agent": agent.id,
                    "enabled": agent.enabled, "startOnAppLaunch": agent.start_on_app_launch,
                    "selected": agent.start_on_app_launch,
                }));
                agent.start_on_app_launch.then_some((agent.id, None))
            })
            .collect();
        let mut controller = Controller::new(
            store,
            credentials.clone(),
            bundle,
            legacy_parent.join(crate::sign_out::AGENT_OWNERSHIP),
        );
        controller.protect_control_paths(
            crate::Manager::from_env().map(|manager| vec![manager.storage_root().to_path_buf()]),
        );
        Ok(Self {
            inventory_warnings,
            controller,
            imports: Imports::default(),
            legacy_parent,
            workspace,
            app_data,
            closed: false,
            credentials,
            starts: BTreeMap::new(),
            queued,
            next_start: 0,
            acted: BTreeSet::new(),
            profiles: BTreeMap::new(),
            log_challenges: BTreeMap::new(),
            creating: None,
            legacy_check: refuse_legacy,
        })
    }
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        let mut data = self.controller.snapshot()?;
        for agent in &mut data.agents {
            if let Some(pending) = self.starts.get(&agent.id) {
                agent.status = pending.status;
                agent.error = None;
            } else if self.queued.contains_key(&agent.id) {
                agent.status = ProcessStatus::Waiting;
                agent.error = None;
            }
        }
        let mut snapshot = Snapshot::from(
            data,
            cfg!(target_os = "macos"),
            &self.workspace,
            &self.app_data,
        );
        snapshot.inventory_warnings = self.inventory_warnings.clone();
        Ok(snapshot)
    }
    fn action(&mut self, id: &str, action: Action) -> Result<Snapshot, String> {
        self.starts.remove(id);
        self.queued.remove(id);
        self.acted.insert(id.to_owned());
        self.controller.action(id, action)?;
        self.snapshot()
    }
    fn take_start(&mut self, id: &str, ticket: u64) -> Result<PendingStart, String> {
        if self.starts.get(id).map(|pending| pending.ticket) != Some(ticket) {
            return Err(START_CANCELLED.into());
        }
        self.starts.remove(id).ok_or_else(|| START_CANCELLED.into())
    }
    fn attach_mention(&mut self, id: &str, revision: u64, floor: u64) -> Result<(), String> {
        let current = self.controller.snapshot()?;
        if !current
            .agents
            .iter()
            .any(|a| a.id == id && a.revision == revision)
        {
            return Err("Saved settings changed; mention replay was not attached".into());
        }
        if let Some(pending) = self.starts.get_mut(id) {
            if pending.revision != revision {
                return Err("Saved settings changed; mention replay was not attached".into());
            }
            pending.replay_floor = Some(pending.replay_floor.map_or(floor, |old| old.min(floor)));
        } else if let Some(replay) = self.queued.get_mut(id) {
            if replay.as_ref().is_some_and(|old| old.revision != revision) {
                return Err("Saved settings changed; mention replay was not attached".into());
            }
            let floor = replay.as_ref().map_or(floor, |old| old.floor.min(floor));
            *replay = Some(MentionReplay { revision, floor });
        } else {
            return Err(
                "Launch already finished or was cancelled; mention replay could not be confirmed"
                    .into(),
            );
        }
        Ok(())
    }
    fn refuse_legacy(&self, id: &str) -> Result<(), String> {
        if self.controller.requires_legacy_handover(id)? {
            (self.legacy_check)()
        } else {
            Ok(())
        }
    }
    fn shutdown(&mut self) -> Result<(), String> {
        self.closed = true; // Fence queued commands before shutdown starts.
        self.controller.shutdown()
    }
    fn log_challenge(
        &mut self,
        id: String,
        pubkey: String,
        relay_url: String,
    ) -> Result<String, String> {
        self.controller.log_target(&id, &pubkey, &relay_url)?;
        let nonce = uuid::Uuid::new_v4().to_string();
        self.log_challenges
            .retain(|_, pending| pending.issued.elapsed() <= std::time::Duration::from_secs(20));
        if self.log_challenges.len() >= 4 {
            return Err("Too many pending log authorizations".into());
        }
        self.log_challenges.insert(
            nonce.clone(),
            LogChallenge {
                id,
                pubkey,
                relay_url,
                nonce: nonce.clone(),
                issued: std::time::Instant::now(),
            },
        );
        Ok(nonce)
    }
    fn read_log(
        &mut self,
        id: &str,
        pubkey: &str,
        relay_url: &str,
        nonce: &str,
        signature: &str,
    ) -> Result<String, String> {
        // Consume before comparison or I/O; even a failed proof cannot be replayed.
        let challenge = self
            .log_challenges
            .remove(nonce)
            .ok_or("Log authorization expired")?;
        if challenge.issued.elapsed() > std::time::Duration::from_secs(20)
            || challenge.id != id
            || challenge.pubkey != pubkey
            || challenge.relay_url != relay_url
            || challenge.nonce != nonce
        {
            return Err("Log authorization expired".into());
        }
        self.controller
            .read_log(id, pubkey, relay_url, nonce, signature)
    }
}

type ProfilePublication = (
    tokio::sync::OwnedMutexGuard<()>,
    buzz_agent_controller::CreationProfile,
    Arc<dyn Credentials>,
);

#[derive(Clone)]
pub(crate) struct AgentHost(
    Arc<Mutex<Result<Host, String>>>,
    Arc<AtomicBool>,
    Arc<tokio::sync::Mutex<()>>,
    /// Starts compare each agent's attested owner with the signed-in human.
    owner::Owner,
);
impl AgentHost {
    pub(crate) fn initialize(
        paths: Result<(PathBuf, PathBuf, PathBuf), String>,
        resources: Result<PathBuf, String>,
        identity: owner::Owner,
    ) -> Self {
        buzz_agent_controller::warm_tools_path();
        Self::initialize_with(identity, move || {
            let bundle = resources.and_then(RuntimeBundle::new);
            paths.and_then(|(root, legacy, workspace)| {
                Host::open(
                    root,
                    legacy,
                    workspace,
                    bundle,
                    Arc::new(PlatformCredentials::default()),
                )
            })
        })
    }
    fn initialize_with(
        identity: owner::Owner,
        open: impl FnOnce() -> Result<Host, String> + Send + 'static,
    ) -> Self {
        let state = Arc::new(Mutex::new(Err(
            "Agent runtime is initializing; retry shortly".into(),
        )));
        let closed = Arc::new(AtomicBool::new(false));
        let admission = Arc::new(tokio::sync::Mutex::new(()));
        // Initialization is the first admitted operation. Callers wait for its
        // real outcome rather than treating the placeholder as a permanent error.
        let initializing = admission
            .clone()
            .try_lock_owned()
            .expect("new admission mutex");
        let owner = Self(
            state.clone(),
            closed.clone(),
            admission.clone(),
            identity.clone(),
        );
        tauri::async_runtime::spawn(async move {
            let opened = tauri::async_runtime::spawn_blocking(open)
                .await
                .unwrap_or_else(|_| Err("Agent runtime initialization failed".into()));
            if closed.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(mut state) = state.lock() {
                *state = opened;
            }
            drop(initializing); // restore itself enters through native admission.
            Self(state, closed, admission, identity).restore().await;
        });
        owner
    }
    // Synchronous admission belongs on a blocking worker; external waits release it.
    fn with<T>(&self, operation: impl FnOnce(&mut Host) -> Result<T, String>) -> Result<T, String> {
        if self.1.load(Ordering::SeqCst) {
            return Err("Agent host is shutting down".into());
        }
        let mut state = self.0.lock().map_err(|_| {
            "Native agent state is unavailable after an operation failed; restart the app"
        })?;
        let host = state.as_mut().map_err(|message| message.clone())?;
        if self.1.load(Ordering::SeqCst) || host.closed {
            return Err("Agent host is shutting down".into());
        }
        operation(host)
    }
    async fn begin_profile(&self, id: &str) -> Result<ProfilePublication, String> {
        let id = id.to_owned();
        run(self.clone(), move |host| {
            let profile = host.controller.creation_profile(&id)?;
            let guard = host
                .profiles
                .entry(id.to_owned())
                .or_default()
                .clone()
                .try_lock_owned()
                .map_err(|_| {
                    "Profile publication is already in progress; refresh status before retrying"
                })?;
            Ok((guard, profile, host.credentials.clone()))
        })
        .await
    }
    pub(crate) async fn restore(&self) {
        let ids = run(self.clone(), |host| {
            let ids = match host.controller.launch_ids() {
                Ok(ids) => ids,
                Err(error) => {
                    for (id, _) in std::mem::take(&mut host.queued) {
                        host.controller.record_error(&id, error.clone());
                    }
                    return Err(error);
                }
            };
            let mut queued = std::mem::take(&mut host.queued);
            host.queued = ids
                .into_iter()
                .filter(|id| !host.acted.contains(id))
                .map(|id| {
                    let replay = queued.remove(&id).flatten();
                    (id, replay)
                })
                .collect();
            Ok(host.queued.keys().cloned().collect::<Vec<_>>())
        })
        .await
        .unwrap_or_default();
        for id in ids {
            let begin = std::time::Instant::now();
            startup_trace(serde_json::json!({"phase": "start", "agent": id}));
            let result = start(self.clone(), id.clone(), Action::Start, true, None, None).await;
            let agent = result
                .as_ref()
                .ok()
                .and_then(|snapshot| snapshot.data.agents.iter().find(|agent| agent.id == id));
            startup_trace(serde_json::json!({
                "phase": "end", "agent": id, "status": agent.map(|agent| agent.status),
                "returnedError": result.is_err(), "agentError": agent.map(|agent| agent.error.is_some()),
                "elapsedMs": begin.elapsed().as_millis(),
            }));
        }
    }
    pub(crate) async fn ensure_open(&self) -> Result<(), String> {
        run(self.clone(), |_| Ok(())).await
    }
    pub(crate) async fn inherited_workspace(&self) -> Result<Option<String>, String> {
        run(self.clone(), |host| host.controller.inherited_workspace()).await
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) async fn waiting_for_pi(&self) -> Result<Vec<String>, String> {
        self.waiting_for(crate::harness_setup::waiting_for_pi).await
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    async fn waiting_for(
        &self,
        predicate: fn(&buzz_agent_controller::AgentView) -> bool,
    ) -> Result<Vec<String>, String> {
        run(self.clone(), move |host| {
            Ok(host
                .controller
                .snapshot()?
                .agents
                .iter()
                .filter(|agent| predicate(agent))
                .map(|agent| agent.id.clone())
                .collect())
        })
        .await
    }
    pub(crate) async fn disconnect(&self, workspace: &str) -> Result<(), String> {
        let workspace = buzz_agent_controller::connection::origin(workspace)?;
        run(self.clone(), move |host| {
            host.controller.disconnect(&workspace)?;
            // A successful Disconnect also retires pre-existing credential waits.
            // Otherwise their late completion could start against the removed cache.
            let cancelled: Vec<_> = host
                .starts
                .iter()
                .filter(|(_, pending)| pending.workspace.as_deref() == Some(&workspace))
                .map(|(id, _)| id.clone())
                .collect();
            for id in cancelled {
                host.starts.remove(&id);
                host.controller.record_error(
                    &id,
                    "Start cancelled by Disconnect; reconnect and retry Start".into(),
                );
            }
            // Queued restore has not acquired a ticket yet; fence it too.
            let queued: Vec<_> = host.queued.keys().cloned().collect();
            for id in queued {
                if host
                    .controller
                    .credential_request(&id)
                    .is_ok_and(|request| request.3.as_deref() == Some(&workspace))
                {
                    host.queued.remove(&id);
                    host.acted.insert(id.clone());
                    host.controller.record_error(
                        &id,
                        "Start cancelled by Disconnect; reconnect and retry Start".into(),
                    );
                }
            }
            Ok(())
        })
        .await
    }
    pub(crate) async fn model_context(
        &self,
        id: Option<&str>,
        revision: Option<u64>,
        edit: AgentEdit,
    ) -> Result<buzz_agent_controller::ModelContext, String> {
        let id = id.map(str::to_owned);
        run(self.clone(), move |host| match (id.as_deref(), revision) {
            (Some(id), Some(revision)) => host.controller.model_context(id, revision, edit),
            (None, None) => Controller::draft_model_context(host.controller.effective_draft(edit)?),
            _ => Err("Invalid agent model context".into()),
        })
        .await
    }
    pub(crate) async fn goose_model_context(
        &self,
        id: Option<&str>,
        revision: Option<u64>,
        edit: AgentEdit,
    ) -> Result<buzz_agent_controller::GooseModelContext, String> {
        prepare_tools_path().await;
        let id = id.map(str::to_owned);
        run(self.clone(), move |host| match (id.as_deref(), revision) {
            (Some(id), Some(revision)) => host.controller.goose_model_context(id, revision, edit),
            (None, None) => host
                .controller
                .draft_goose_model_context(host.controller.effective_draft(edit)?),
            _ => Err("Invalid agent model context".into()),
        })
        .await
    }
    pub(crate) async fn pi_model_context(
        &self,
        id: Option<&str>,
        revision: Option<u64>,
        edit: AgentEdit,
    ) -> Result<buzz_agent_controller::pi::PiContext, String> {
        prepare_tools_path().await;
        let id = id.map(str::to_owned);
        run(self.clone(), move |host| match (id.as_deref(), revision) {
            (Some(id), Some(revision)) => host.controller.pi_model_context(id, revision, edit),
            (None, None) => {
                Controller::draft_pi_model_context(host.controller.effective_draft(edit)?)
            }
            _ => Err("Invalid agent model context".into()),
        })
        .await
    }
    /// Sign out's "Also remove my agents"; runs before shutdown fences the host.
    pub(crate) fn shutdown(&self) -> Result<(), String> {
        self.1.store(true, Ordering::SeqCst);
        let mut state = self
            .0
            .lock()
            .map_err(|_| "Agent host shutdown could not be confirmed")?;
        if let Ok(host) = state.as_mut() {
            host.shutdown()?;
        }
        Ok(())
    }
}
async fn prepare_tools_path() {
    // Shell startup may take seconds; neither the controller nor FIFO admission
    // is held while this shared, bounded attempt runs.
    let _ = tauri::async_runtime::spawn_blocking(buzz_agent_controller::prepare_tools_path).await;
}
pub(crate) async fn run<T: Send + 'static>(
    state: AgentHost,
    operation: impl FnOnce(&mut Host) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    // FIFO admission keeps a queued Start preparation ahead of a later recovery
    // Stop. The worker owns admission through completion, even if its caller drops.
    // Credential/network waits happen between runs, so Stop can still fence them.
    let admission = state.2.clone().lock_owned().await;
    tauri::async_runtime::spawn_blocking(move || {
        let _admission = admission;
        state.with(operation)
    })
    .await
    .map_err(|_| "Native agent operation failed; refresh status before retrying")?
}
#[tauri::command]
pub(crate) async fn agent_control_log_challenge(
    state: tauri::State<'_, AgentHost>,
    id: String,
    pubkey: String,
    relay_url: String,
) -> Result<String, String> {
    run(state.inner().clone(), move |host| {
        host.log_challenge(id, pubkey, relay_url)
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_read_log(
    state: tauri::State<'_, AgentHost>,
    id: String,
    pubkey: String,
    relay_url: String,
    nonce: String,
    signature: String,
) -> Result<String, String> {
    run(state.inner().clone(), move |host| {
        host.read_log(&id, &pubkey, &relay_url, &nonce, &signature)
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_snapshot(
    state: tauri::State<'_, AgentHost>,
) -> Result<Snapshot, String> {
    prepare_tools_path().await;
    run(state.inner().clone(), |host| host.snapshot()).await
}
#[tauri::command]
pub(crate) async fn agent_control_save(
    state: tauri::State<'_, AgentHost>,
    id: String,
    expected_revision: u64,
    edit: AgentEdit,
) -> Result<Snapshot, String> {
    save_and_restart(state.inner().clone(), move |host| {
        host.controller.save(&id, expected_revision, edit).map(drop)
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_save_defaults(
    state: tauri::State<'_, AgentHost>,
    edit: buzz_agent_controller::AgentDefaultsEdit,
) -> Result<Snapshot, String> {
    save_and_restart(state.inner().clone(), move |host| {
        host.controller.save_defaults(edit).map(drop)
    })
    .await
}
/// Save, then restart only agents that were running before and after it and whose
/// effective settings changed. Stopped or disabled agents are never started.
async fn save_and_restart(
    owner: AgentHost,
    save: impl FnOnce(&mut Host) -> Result<(), String> + Send + 'static,
) -> Result<Snapshot, String> {
    let changed = run(owner.clone(), move |host| {
        let before = host.controller.running_settings()?;
        save(host)?;
        let after = host.controller.running_settings()?;
        Ok(changed_running(before, after))
    })
    .await?;
    let (mut restarted, mut failures) = (0, 0);
    for id in changed {
        // Re-checked under the lock: Stop wins, and a subsequent user Start
        // may already have launched the saved settings.
        let result = start_guarded(
            owner.clone(),
            id.clone(),
            Action::Restart,
            false,
            None,
            Some((needs_save_restart, NO_SAVE_RESTART)),
        )
        .await;
        match restart_outcome(&id, result) {
            RestartOutcome::Restarted => restarted += 1,
            RestartOutcome::Skipped => {}
            RestartOutcome::Failed => failures += 1,
        }
    }
    let mut snapshot = run(owner, |host| host.snapshot()).await?;
    snapshot.restarted = Some(restarted);
    snapshot.restart_failures = Some(failures);
    Ok(snapshot)
}
#[derive(Debug, PartialEq)]
enum RestartOutcome {
    Restarted,
    /// No longer needed, or an explicit Stop/newer action won: not a failure.
    Skipped,
    Failed,
}
const NO_SAVE_RESTART: &str = "Agent no longer needs a save restart";
fn restart_outcome(id: &str, result: Result<Snapshot, String>) -> RestartOutcome {
    match result {
        Err(error) if error == NO_SAVE_RESTART || error == START_CANCELLED => {
            RestartOutcome::Skipped
        }
        Err(_) => RestartOutcome::Failed,
        Ok(snapshot) => match snapshot.data.agents.iter().find(|agent| agent.id == id) {
            // A denied credential prompt or failed stop leaves the old process
            // running; only a launch of the saved settings counts as a restart.
            Some(agent) if is_running(agent) && agent.restart_diff.is_empty() => {
                RestartOutcome::Restarted
            }
            // Stop disabled it while the restart was in flight.
            Some(agent) if !agent.enabled => RestartOutcome::Skipped,
            _ => RestartOutcome::Failed,
        },
    }
}
/// Agents live both before and after a save whose effective settings differ.
fn changed_running(
    before: BTreeMap<String, serde_json::Value>,
    after: BTreeMap<String, serde_json::Value>,
) -> Vec<String> {
    after
        .into_iter()
        .filter(|(id, settings)| before.get(id).is_some_and(|old| old != settings))
        .map(|(id, _)| id)
        .collect()
}
fn is_running(agent: &buzz_agent_controller::AgentView) -> bool {
    agent.enabled && agent.status == buzz_agent_controller::ProcessStatus::Running
}
fn needs_save_restart(agent: &buzz_agent_controller::AgentView) -> bool {
    is_running(agent) && !agent.restart_diff.is_empty()
}
#[tauri::command]
pub(crate) async fn agent_control_start_on_app_launch(
    state: tauri::State<'_, AgentHost>,
    id: String,
    enabled: bool,
) -> Result<Snapshot, String> {
    run(state.inner().clone(), move |host| {
        host.controller.set_start_on_app_launch(&id, enabled)?;
        if !enabled {
            host.queued.remove(&id);
        }
        host.snapshot()
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_delete(
    state: tauri::State<'_, AgentHost>,
    id: String,
    expected_revision: u64,
) -> Result<Snapshot, String> {
    run(state.inner().clone(), move |host| {
        host.starts.remove(&id);
        host.queued.remove(&id);
        host.acted.insert(id.clone());
        host.controller.delete(&id, expected_revision)?;
        host.snapshot()
    })
    .await
}
// This only annotates an admitted launch; it cannot start or resurrect an agent.
#[tauri::command]
pub(crate) async fn agent_control_attach_mention(
    state: tauri::State<'_, AgentHost>,
    id: String,
    expected_revision: u64,
    replay_floor: u64,
) -> Result<(), String> {
    run(state.inner().clone(), move |host| {
        host.attach_mention(&id, expected_revision, replay_floor)
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_action(
    state: tauri::State<'_, AgentHost>,
    id: String,
    action: Action,
    replay_floor: Option<u64>,
) -> Result<Snapshot, String> {
    let owner = state.inner().clone();
    if matches!(action, Action::Stop) {
        return run(owner, move |host| host.action(&id, action)).await;
    }
    start(owner, id, action, false, replay_floor, None).await
}
// Only explicit nonsecret fields belong here; never pass snapshots/config/errors.
fn startup_trace(value: serde_json::Value) {
    if cfg!(debug_assertions) {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr().lock(),
            "[agent-startup] pid={} {value}",
            std::process::id()
        );
    }
}
pub(crate) const NOT_WAITING_FOR_PI: &str = "Agent no longer waiting for Pi";
#[derive(Clone, Copy)]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    expect(
        dead_code,
        reason = "Harness installation is unavailable on this platform"
    )
)]
pub(crate) enum InstallRestart {
    Pi,
}
pub(crate) async fn start(
    owner: AgentHost,
    id: String,
    action: Action,
    restore: bool,
    replay_floor: Option<u64>,
    install_restart: Option<InstallRestart>,
) -> Result<Snapshot, String> {
    let guard = install_restart.map(|harness| -> StartGuard {
        match harness {
            InstallRestart::Pi => (crate::harness_setup::waiting_for_pi, NOT_WAITING_FOR_PI),
        }
    });
    start_guarded(owner, id, action, restore, replay_floor, guard).await
}
const START_CANCELLED: &str = "Start cancelled by a newer action";
type StartGuard = (fn(&buzz_agent_controller::AgentView) -> bool, &'static str);
fn check_guard(host: &mut Host, id: &str, guard: Option<StartGuard>) -> Result<(), String> {
    let Some((eligible, refusal)) = guard else {
        return Ok(());
    };
    if host
        .controller
        .snapshot()?
        .agents
        .iter()
        .any(|agent| agent.id == id && eligible(agent))
    {
        Ok(())
    } else {
        Err(refusal.into())
    }
}
async fn start_guarded(
    owner: AgentHost,
    id: String,
    action: Action,
    restore: bool,
    replay_floor: Option<u64>,
    guard: Option<StartGuard>,
) -> Result<Snapshot, String> {
    let target = id.clone();
    let prepared = run(owner.clone(), move |host| {
        let id = target;
        let queued_replay = host.queued.remove(&id).flatten();
        if restore && (host.acted.contains(&id) || !host.controller.launch_ids()?.contains(&id)) {
            return Err("Agent disabled before restore".into());
        }
        // Re-check while holding the controller, not just when the caller
        // chose this agent: Stop or Edit may have changed it since.
        check_guard(host, &id, guard)?;
        if host.starts.contains_key(&id) {
            return Err("Agent start already in progress; use Stop to cancel".into());
        }
        if !restore {
            host.acted.insert(id.clone());
        }
        let request = match host.controller.credential_request(&id) {
            Ok(request) => request,
            Err(error) => {
                host.controller.record_error(&id, error.clone());
                return Err(error);
            }
        };
        if queued_replay
            .as_ref()
            .is_some_and(|replay| replay.revision != request.2)
        {
            let error = "Saved settings changed; retry Start for pending mentions".to_owned();
            host.controller.record_error(&id, error.clone());
            return Err(error);
        }
        let replay_floor = queued_replay.map_or(replay_floor, |replay| {
            Some(replay_floor.map_or(replay.floor, |floor| floor.min(replay.floor)))
        });
        if let Err(error) = host.refuse_legacy(&id) {
            host.controller.record_error(&id, error.clone());
            return Err(error);
        }
        host.next_start = host
            .next_start
            .checked_add(1)
            .ok_or("Start sequence exhausted")?;
        let ticket = host.next_start;
        host.starts.insert(
            id.clone(),
            PendingStart {
                ticket,
                workspace: request.3.clone(),
                status: ProcessStatus::Waiting,
                revision: request.2,
                replay_floor,
            },
        );
        let attested = host.controller.attested_owner(&id)?;
        Ok((request, ticket, host.credentials.clone(), attested))
    })
    .await?;
    let ((credential, pubkey, revision, _workspace), ticket, credentials, attested) = prepared;
    prepare_tools_path().await;
    let target = id.clone();
    let pi = run(owner.clone(), move |host| {
        host.starts
            .get(&target)
            .filter(|pending| pending.ticket == ticket)
            .ok_or(START_CANCELLED)?;
        Ok(host.controller.pi_launch_context(&target, revision))
    })
    .await?;
    let probed_pi = matches!(&pi, Ok(Some(_)));
    let preflight = match pi {
        Ok(Some(pi)) => buzz_desktop::pi_models::verify(pi)
            .await
            .map(|pi| buzz_agent_controller::pi::LaunchPreflight::new(Some(pi))),
        Ok(None) => Ok(buzz_agent_controller::pi::LaunchPreflight::new(None)),
        Err(error) => Err(error),
    };
    if probed_pi && preflight.is_ok() {
        let target = id.clone();
        run(owner.clone(), move |host| {
            host.starts
                .get(&target)
                .filter(|pending| pending.ticket == ticket)
                .ok_or(START_CANCELLED)?;
            Ok(())
        })
        .await?;
    }
    // OS permission prompts never hold the controller. Stop/quit invalidate the
    // ticket while the OS owns its dialog; a late key cannot start a listener.
    // Kept agents answer to the owner who authorized them, not whoever signs in
    // next: a mismatch is refused before the agent's key is read.
    let ready = match owner.3.viewer().await {
        Ok(signed_in) => buzz_agent_controller::check_owner(attested.as_deref(), Some(&signed_in)),
        Err(error) => Err(error),
    }
    .and(preflight.as_ref().map(|_| ()).map_err(Clone::clone));
    let target = id.clone();
    run(owner.clone(), move |host| {
        host.starts
            .get(&target)
            .filter(|pending| pending.ticket == ticket)
            .ok_or(START_CANCELLED)?;
        Ok(())
    })
    .await?;
    let acquired = if let Err(error) = ready {
        Err(error)
    } else {
        tauri::async_runtime::spawn_blocking(move || {
            if !restore && replay_floor.is_none() && guard.is_none() {
                credentials.retry();
            }
            credentials.read(&credential, &pubkey)
        })
        .await
        .map_err(|_| "Native credential operation failed".to_owned())
        .and_then(|v| v)
        .and_then(|v| v.ok_or("Saved agent key is unavailable; nothing was started".into()))
    };
    let target = id.clone();
    if acquired.is_ok() {
        run(owner.clone(), move |host| {
            let pending = host
                .starts
                .get_mut(&target)
                .filter(|pending| pending.ticket == ticket)
                .ok_or(START_CANCELLED)?;
            pending.status = ProcessStatus::Starting;
            Ok(())
        })
        .await?;
    }
    run(owner, move |host| {
        let replay_floor = host.take_start(&id, ticket)?.replay_floor;
        let key = match acquired {
            Ok(key) => key,
            Err(error) => {
                host.controller.record_error(&id, error);
                return host.snapshot();
            }
        };
        if let Err(error) = host.refuse_legacy(&id) {
            host.controller.record_error(&id, error);
            return host.snapshot();
        }
        // The OS credential prompt can outlast the agent (e.g. its listener
        // exited); eligibility must still hold right before Restart enables it.
        check_guard(host, &id, guard)?;
        if let Err(error) = host.controller.action_with_preflight(
            &id,
            action,
            revision,
            &key,
            replay_floor,
            &preflight?,
        ) {
            host.controller.record_error(&id, error);
        }
        host.snapshot()
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_use_here(
    state: tauri::State<'_, AgentHost>,
    id: String,
    resolution: buzz_agent_controller::CommunityResolution,
) -> Result<Snapshot, String> {
    run(state.inner().clone(), move |host| {
        host.controller.use_here(&id, resolution)?;
        host.snapshot()
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_local_clone_settings(
    state: tauri::State<'_, AgentHost>,
    id: String,
) -> Result<buzz_agent_controller::CloneSettings, String> {
    run(state.inner().clone(), move |host| {
        host.controller.local_clone_settings(&id)
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_clone_settings(
    state: tauri::State<'_, AgentHost>,
    source: LegacySource,
    pubkey: String,
) -> Result<buzz_agent_controller::CloneSettings, String> {
    run(state.inner().clone(), move |host| {
        Imports::clone_settings(source, host.legacy_parent.clone(), &pubkey)
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_import_preview(
    state: tauri::State<'_, AgentHost>,
    source: LegacySource,
    destination: String,
) -> Result<ImportPreview, String> {
    run(state.inner().clone(), move |host| {
        host.imports.preview(
            source,
            host.legacy_parent.clone(),
            host.workspace.clone(),
            &destination,
        )
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_import_commit(
    state: tauri::State<'_, AgentHost>,
    token: String,
    ids: Vec<String>,
) -> Result<Snapshot, String> {
    let owner = state.inner().clone();
    let (prepared, credentials) = run(owner.clone(), move |host| {
        let prepared = host
            .controller
            .prepare_import(&mut host.imports, &token, &ids)?;
        // Consume the preview so concurrent IPC cannot import it twice.
        host.imports.discard();
        Ok((prepared, host.credentials.clone()))
    })
    .await?;
    let imported = tauri::async_runtime::spawn_blocking(move || {
        credentials.retry();
        prepared.acquire(credentials.as_ref())
    })
    .await
    .map_err(|_| "Native import credential operation failed")??;
    run(owner, move |host| {
        host.controller.commit_import(imported)?;
        host.snapshot()
    })
    .await
}

#[tauri::command]
pub(crate) async fn agent_control_create_prepare(
    state: tauri::State<'_, AgentHost>,
    request_id: String,
    destination: String,
    owner: String,
) -> Result<serde_json::Value, String> {
    run(state.inner().clone(), move |host| {
        if uuid::Uuid::parse_str(&request_id).is_err() {
            return Err("Invalid create request".into());
        }
        if let Some(agent) = host
            .controller
            .created_request(&request_id, &destination, &owner)?
        {
            return Ok(serde_json::json!({"id": agent.id, "pubkey": agent.pubkey, "saved": true}));
        }
        if host.creating.as_ref().map(|(id, _)| id) != Some(&request_id) {
            host.creating = Some((
                request_id,
                Arc::new(NewAgent::prepare(&destination, &owner)?),
            ));
        }
        let agent = &host.creating.as_ref().ok_or("Create request expired")?.1;
        if !agent.matches(&destination, &owner)? {
            return Err("Create destination or owner changed".into());
        }
        Ok(serde_json::json!({"id": agent.id, "pubkey": agent.key.pubkey()}))
    })
    .await
}
/// Owner attestation for the pending create's generated key only. The
/// identity may read OS credentials, so the agent host stays unlocked.
#[tauri::command]
pub(crate) async fn agent_control_create_authorize(
    state: tauri::State<'_, AgentHost>,
    identity: tauri::State<'_, crate::identity::IdentityHost>,
    destination: String,
    owner: String,
    pubkey: String,
) -> Result<Vec<String>, String> {
    let (owner, pubkey) = run(state.inner().clone(), move |host| {
        let (_, prepared) = host
            .creating
            .as_ref()
            .ok_or("Create request expired; reopen Add agent")?;
        if prepared.key.pubkey() != pubkey || !prepared.matches(&destination, &owner)? {
            return Err("Authorization does not match the pending create request".into());
        }
        Ok((owner, pubkey))
    })
    .await?;
    identity.inner().authorize_agent(owner, pubkey).await
}
#[tauri::command]
pub(crate) async fn agent_control_create_commit(
    state: tauri::State<'_, AgentHost>,
    request_id: String,
    edit: AgentEdit,
    auth: String,
    bundle: Option<buzz_agent_controller::BundleMember>,
) -> Result<Snapshot, String> {
    if let Some(bundle) = &bundle {
        bundle.validate()?;
    }
    let owner = state.inner().clone();
    let (prepared, credentials, request_id, edit, auth) = run(owner.clone(), move |host| {
        let (_, prepared) = host
            .creating
            .as_ref()
            .filter(|(id, _)| id == &request_id)
            .ok_or("Create request expired; reopen Add agent")?;
        prepared.validate(edit.clone(), &auth)?;
        Ok((
            prepared.clone(),
            host.credentials.clone(),
            request_id,
            edit,
            auth,
        ))
    })
    .await?;
    let saved = prepared.clone();
    tauri::async_runtime::spawn_blocking(move || {
        credentials.retry();
        saved.save_key(credentials.as_ref())
    })
    .await
    .map_err(|_| "Native credential operation failed")??;
    run(owner, move |host| {
        if host.creating.as_ref().map(|(id, _)| id) != Some(&request_id) {
            return Err("Create request was replaced".into());
        }
        if let Some(bundle) = bundle {
            host.controller
                .create_bundle_member(&prepared, edit, &auth, &request_id, &bundle)?;
        } else {
            host.controller.create(&prepared, edit, &auth)?;
        }
        host.snapshot()
    })
    .await
}
#[tauri::command]
pub(crate) async fn agent_control_creation_profile(
    state: tauri::State<'_, AgentHost>,
    id: String,
) -> Result<Snapshot, String> {
    publish_profile(state.inner().clone(), id).await
}

async fn publish_profile(owner: AgentHost, id: String) -> Result<Snapshot, String> {
    // Native ownership survives renderer reloads. Refuse overlapping publication,
    // while allowing settings Save to advance the revision and retain pending.
    let (publication, profile, credentials) = owner.begin_profile(&id).await?;
    let (profile, key) = tauri::async_runtime::spawn_blocking(move || {
        credentials.retry();
        credentials
            .read(&profile.credential_id, &profile.pubkey)
            .map(|key| (profile, key))
    })
    .await
    .map_err(|_| "Native credential operation failed")??;
    let key = key.ok_or("Agent key unavailable")?;
    owner.ensure_open().await?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| "Profile client unavailable")?;
    publish_acquired(&owner, &id, &profile, &key, &client, publication).await
}

async fn publish_acquired(
    owner: &AgentHost,
    id: &str,
    profile: &buzz_agent_controller::CreationProfile,
    key: &buzz_agent_controller::Secret,
    client: &reqwest::Client,
    _publication: tokio::sync::OwnedMutexGuard<()>,
) -> Result<Snapshot, String> {
    let target = id.to_owned();
    let revision = profile.revision;
    profile_http::publish(client, profile, key, || async {
        run(owner.clone(), move |host| {
            let current = host.controller.creation_profile(&target)?;
            if current.revision != revision {
                return Err("Saved profile changed; retry publication".into());
            }
            Ok(())
        })
        .await
    })
    .await?;
    let id = id.to_owned();
    run(owner.clone(), move |host| {
        host.controller.profile_published(&id, revision)?;
        host.snapshot()
    })
    .await
}

/// Restore only owner-addressed snapshot memory to this saved native identity.
#[tauri::command]
pub(crate) async fn agent_control_snapshot_memory_write(
    state: tauri::State<'_, AgentHost>,
    identity: tauri::State<'_, crate::identity::IdentityHost>,
    id: String,
    entries: Vec<SnapshotMemoryEntry>,
) -> Result<MemoryWriteResult, String> {
    snapshot_memory::restore(state.inner().clone(), identity, id, entries).await
}
pub(crate) use snapshot_memory::{MemoryWriteResult, SnapshotMemoryEntry};
mod snapshot_memory;

pub(crate) mod profile_http;

#[cfg(test)]
pub(crate) mod tests;

// Advisory handover guard only: unmodified old Buzz does not share our lock and
// can be launched afterward. Never inspect process environments or terminate it.
fn refuse_legacy_listing(listing: &str) -> Result<(), String> {
    for line in listing.lines() {
        let executable = line
            .trim()
            .split_once(char::is_whitespace)
            .map(|(_, exe)| exe.trim())
            .unwrap_or("");
        let name = std::path::Path::new(executable)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if name == "buzz-desktop"
            || executable.contains("/Buzz.app/Contents/MacOS/")
            || executable.contains("/Buzz Dev.app/Contents/MacOS/")
        {
            return Err(
                "Stop old Buzz before starting agents here; simultaneous ownership is unsupported"
                    .into(),
            );
        }
    }
    Ok(())
}
fn refuse_legacy() -> Result<(), String> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,comm="])
        .env_clear()
        .output()
        .map_err(|_| "Could not check old Buzz processes; Start refused")?;
    if !output.status.success() || output.stdout.len() > 4 * 1024 * 1024 {
        return Err("Could not check old Buzz processes; Start refused".into());
    }
    refuse_legacy_listing(&String::from_utf8_lossy(&output.stdout))
}

/// Trusted plugin service: native state remains authoritative for all start paths.
#[tauri::command]
pub(crate) async fn agent_security(
    state: tauri::State<'_, AgentHost>,
    request: buzz_agent_controller::security::Request,
) -> Result<serde_json::Value, String> {
    let owner = state.inner().clone();
    let provider = match &request {
        buzz_agent_controller::security::Request::Register { provider, .. } => {
            Some(provider.clone())
        }
        _ => None,
    };
    let result = run(owner.clone(), move |host| host.controller.security(request)).await?;
    if let Some(provider) = provider {
        tauri::async_runtime::spawn(async move {
            let ids = owner
                .with(|host| host.controller.security_restore_ids(&provider))
                .unwrap_or_default();
            for id in ids {
                let _ = start(owner.clone(), id, Action::Start, true, None, None).await;
            }
        });
    }
    Ok(result)
}

#[tauri::command]
pub(crate) async fn agent_control_team_instructions(
    state: tauri::State<'_, AgentHost>,
    identity: tauri::State<'_, crate::identity::IdentityHost>,
    id: String,
    revision: u64,
    instructions: String,
    team: String,
    community: String,
) -> Result<ControlSnapshot, String> {
    let (owner, teams) = crate::relay::current_team_members(identity.inner(), &community).await?;
    run(state.inner().clone(), move |host| {
        let agent = host
            .controller
            .snapshot()?
            .agents
            .into_iter()
            .find(|agent| agent.id == id)
            .ok_or("Agent no longer exists")?;
        if !teams
            .get(&team)
            .is_some_and(|head| head.members.contains(&agent.pubkey))
        {
            return Err("Team no longer contains this member; refresh before deploying".into());
        }
        host.controller
            .reconcile_team_bindings(&community, &owner, &teams)?;
        host.controller.apply_team_instructions(
            &id,
            revision,
            &instructions,
            &owner,
            (&team, &community),
        )?;
        Ok(host.snapshot()?.data)
    })
    .await
}

#[tauri::command]
pub(crate) async fn agent_control_team_capture(
    state: tauri::State<'_, AgentHost>,
    team: buzz_agent_controller::TeamMeta,
    members: Vec<String>,
    community: String,
) -> Result<buzz_agent_controller::TeamSnapshot, String> {
    let relay = community
        .trim_end_matches('/')
        .replacen("https://", "wss://", 1);
    run(state.inner().clone(), move |host| {
        host.controller.export_team(team, &members, &relay)
    })
    .await
}

#[tauri::command]
pub(crate) async fn agent_control_team_export(
    state: tauri::State<'_, AgentHost>,
    identity: tauri::State<'_, crate::identity::IdentityHost>,
    snapshot: buzz_agent_controller::TeamSnapshot,
    members: Vec<String>,
    community: String,
    memory_level: Option<String>,
) -> Result<buzz_agent_controller::TeamSnapshot, String> {
    let level = memory_level.unwrap_or_else(|| "none".into());
    if !matches!(level.as_str(), "none" | "core" | "everything") {
        return Err("Invalid memory selection".into());
    }
    let viewer = if level == "none" {
        None
    } else {
        Some(identity.with_key(|_, viewer| Ok(viewer.to_owned())).await?)
    };
    snapshot.validate()?;
    if snapshot.members.len() != members.len() {
        return Err("Portable team members do not match their definitions".into());
    }
    let relay = community
        .trim_end_matches('/')
        .replacen("https://", "wss://", 1);
    let (mut snapshot, targets) = run(state.inner().clone(), move |host| {
        let agents = host.snapshot()?.data.agents;
        let targets = members
            .iter()
            .map(|pubkey| {
                let agent = agents
                    .iter()
                    .find(|agent| &agent.pubkey == pubkey && agent.relay_url == relay)
                    .ok_or("A team member is unavailable")?;
                Ok((agent.pubkey.clone(), community.clone()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        // Export the members' current saved settings; the portable team owns metadata.
        let snapshot = host
            .controller
            .export_team(snapshot.team, &members, &relay)?;
        Ok((snapshot, targets))
    })
    .await?;
    if level != "none" {
        for (member, (agent, community)) in snapshot.members.iter_mut().zip(targets) {
            let listing =
                crate::relay::agent::relay_agent_memories_read(identity.clone(), community, agent)
                    .await?;
            if listing["partial"].as_bool() != Some(false) {
                return Err("Memory listing is incomplete; retry or export Team only".into());
            }
            let entries = listing["entries"]
                .as_array()
                .ok_or("Invalid memory listing")?;
            member.memory.level = level.clone();
            member.memory.entries = entries
                .iter()
                .filter(|entry| level == "everything" || entry["slug"].as_str() == Some("core"))
                .map(|entry| {
                    Ok(buzz_agent_controller::MemoryEntry {
                        slug: entry["slug"]
                            .as_str()
                            .ok_or("Invalid memory listing")?
                            .into(),
                        body: entry["body"]
                            .as_str()
                            .ok_or("Invalid memory listing")?
                            .into(),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
        }
    }
    if let Some(expected) = viewer {
        identity
            .with_key(move |_, current| {
                if expected != current {
                    return Err("Identity changed during memory export".into());
                }
                Ok(())
            })
            .await?;
    }
    snapshot.validate()?;
    Ok(snapshot)
}

#[tauri::command]
pub(crate) fn agent_control_team_preview(
    content: String,
) -> Result<buzz_agent_controller::TeamSnapshot, String> {
    buzz_agent_controller::TeamSnapshot::decode(content.as_bytes())
}
