//! Installation and settings never execute plugin code. Both desktop and CLI use this crate.
use nostr::{
    event::{Event, EventBuilder, FinalizeEvent, Kind, Tag},
    key::{Keys, SecretKey},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub mod imports;

pub type Result<T> = std::result::Result<T, String>;
const LIMIT: u64 = 8 * 1024 * 1024;
pub const DEFAULT_HOST_COMMAND_OUTPUT_BYTES: u64 = 4096;
pub const MAX_HOST_COMMAND_OUTPUT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub api_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostGrants>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostGrants {
    #[serde(default)]
    pub commands: Vec<HostCommand>,
    #[serde(default)]
    pub network_origins: Vec<String>,
    /// Programs the plugin may start as long-lived processes. Unlike a command,
    /// a process takes stdin and the caller appends its own arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<HostProcess>,
    /// Lists, installs, enables, disables and removes plugins through the app.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plugins: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCommand {
    pub id: String,
    pub program: String,
    pub args: Vec<String>,
    #[serde(
        default,
        rename = "maxOutputBytes",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_bytes: Option<u64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcess {
    pub id: String,
    pub program: String,
    /// Fixed leading arguments; the caller's arguments follow them.
    #[serde(default)]
    pub args: Vec<String>,
}
fn valid_program(program: &str, args: &[String]) -> bool {
    !program.is_empty()
        && program.len() <= 80
        && program
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && args.len() <= 16
        && args
            .iter()
            .all(|argument| argument.len() <= 1024 && !argument.contains('\0'))
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        valid_id(&self.id)?;
        if self.name.trim().is_empty() || self.name.len() > 80 {
            return Err("Name must contain 1–80 bytes of text".into());
        }
        if self.api_version != 1 {
            return Err("Only page plugin API version 1 is supported".into());
        }
        if let Some(host) = &self.host {
            if host.commands.len() > 16
                || host.network_origins.len() > 16
                || host.processes.len() > 16
            {
                return Err("Too many host declarations".into());
            }
            let mut command_ids = std::collections::HashSet::new();
            for command in &host.commands {
                valid_id(&command.id)?;
                if !command_ids.insert(&command.id)
                    || !valid_program(&command.program, &command.args)
                    || command
                        .max_output_bytes
                        .is_some_and(|limit| !(1..=MAX_HOST_COMMAND_OUTPUT_BYTES).contains(&limit))
                {
                    return Err("Invalid host command declaration".into());
                }
            }
            let mut process_ids = std::collections::HashSet::new();
            for process in &host.processes {
                valid_id(&process.id)?;
                if !process_ids.insert(&process.id)
                    || !valid_program(&process.program, &process.args)
                {
                    return Err("Invalid host process declaration".into());
                }
            }
            let mut origins = std::collections::HashSet::new();
            for origin in &host.network_origins {
                let url = url::Url::parse(origin).map_err(err)?;
                if url.scheme() != "https"
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.origin().ascii_serialization() != *origin
                    || !origins.insert(origin)
                {
                    return Err("Invalid HTTPS origin declaration".into());
                }
            }
        }
        Ok(())
    }
}
pub fn valid_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 80
        || !id.as_bytes()[0].is_ascii_lowercase()
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
        || id.split(['.', '-']).any(str::is_empty)
    {
        return Err("Invalid identifier (use lowercase letters, digits, dots or hyphens)".into());
    }
    Ok(())
}
pub fn bundled_manifests() -> Vec<Manifest> {
    let mut builderlab: Manifest = serde_json::from_str(include_str!(
        "../../../src/bundled/builderlab/manifest.json"
    ))
    .expect("builderlab manifest");
    // The build script derives this compile-time constant from BUZZ_BUILDERLAB_URL.
    let origin = env!("BUZZ_BUILDERLAB_ORIGIN");
    builderlab.host = Some(HostGrants {
        network_origins: if origin.is_empty() {
            vec![]
        } else {
            vec![origin.into()]
        },
        ..HostGrants::default()
    });
    vec![
        builderlab,
        serde_json::from_str(include_str!("../../../src/bundled/pairing/manifest.json"))
            .expect("valid pairing manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/todos/manifest.json"))
            .expect("todos manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/diffs/manifest.json"))
            .expect("bundled diffs manifest"),
        serde_json::from_str(include_str!(
            "../../../src/bundled/channel-templates/manifest.json"
        ))
        .expect("channel templates manifest"),
        serde_json::from_str(include_str!(
            "../../../src/bundled/identity-naming/manifest.json"
        ))
        .expect("identity naming manifest"),
        serde_json::from_str(include_str!(
            "../../../src/bundled/agent-activity/manifest.json"
        ))
        .expect("agent activity manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/terminal/manifest.json"))
            .expect("terminal manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/profiles/manifest.json"))
            .expect("valid bundled Profiles manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/links/manifest.json"))
            .expect("links manifest"),
        serde_json::from_str(include_str!(
            "../../../src/bundled/voice-notes/manifest.json"
        ))
        .expect("voice notes manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/mentions/manifest.json"))
            .expect("mentions manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/emoji/manifest.json"))
            .expect("emoji manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/channels/manifest.json"))
            .expect("channels manifest"),
        serde_json::from_str(include_str!(
            "../../../src/bundled/channel-usage/manifest.json"
        ))
        .expect("channel usage manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/github/manifest.json"))
            .expect("github manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/me/manifest.json"))
            .expect("Me manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/inbox/manifest.json"))
            .expect("valid Inbox manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/reminders/manifest.json"))
            .expect("reminders manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/bestie/manifest.json"))
            .expect("bestie manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/projects/manifest.json"))
            .expect("projects manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/agents/manifest.json"))
            .expect("agents manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/agents2/manifest.json"))
            .expect("agents2 manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/workflows/manifest.json"))
            .expect("workflows manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/feedback/manifest.json"))
            .expect("feedback manifest"),
        serde_json::from_str(include_str!("../../../src/bundled/sessions/manifest.json"))
            .expect("sessions manifest"),
        serde_json::from_str(include_str!(
            "../../../src/bundled/hosted-communities/manifest.json"
        ))
        .expect("hosted communities manifest"),
        serde_json::from_str(include_str!(
            "../../../src/bundled/moderation/manifest.json"
        ))
        .expect("moderation manifest"),
    ]
}
/// New bundles must opt in to the default-on policy.
fn enabled_by_default(id: &str) -> bool {
    matches!(
        id,
        "buzz.channels"
            | "buzz.feedback"
            | "buzz.diffs"
            | "buzz.identity-naming"
            | "buzz.agent-activity"
            | "buzz.channel-usage"
            | "buzz.terminal"
            | "buzz.profiles"
            | "buzz.links"
            | "buzz.mentions"
            | "buzz.voice-notes"
            | "buzz.emoji"
            | "buzz.github"
            | "buzz.pairing"
            | "buzz.me"
            | "buzz.inbox"
            | "buzz.reminders"
            | "buzz.projects"
            | "buzz.agents"
            | "buzz.agents2"
            | "buzz.workflows"
            | "buzz.sessions"
            | "block.hosted-communities"
            | "block.builderlab"
            | "buzz.moderation"
    )
}
fn is_bundled(id: &str) -> bool {
    bundled_manifests().iter().any(|manifest| manifest.id == id)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Installed {
    manifest: Manifest,
    current: String,
    #[serde(default)]
    current_signature: Option<Event>,
    #[serde(default)]
    current_source: Option<ReloadSource>,
    previous: Option<String>,
    #[serde(default)]
    previous_signature: Option<Event>,
    #[serde(default)]
    previous_source: Option<ReloadSource>,
    enabled: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReloadSource {
    root: PathBuf,
    path: String,
}
impl ReloadSource {
    pub fn folder(root: PathBuf, path: String) -> Result<Self> {
        if !root.is_absolute() {
            return Err("Reload source root must be absolute".into());
        }
        validate_candidate_path(&path)?;
        Ok(Self { root, path })
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registry {
    version: u32,
    bundled_enabled: bool,
    #[serde(default)]
    bundled_overrides: BTreeMap<String, bool>,
    installed: BTreeMap<String, Installed>,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            version: 1,
            bundled_enabled: true,
            bundled_overrides: BTreeMap::new(),
            installed: BTreeMap::new(),
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    manifest: Manifest,
    code: String,
}
#[derive(Clone)]
struct Release {
    bytes: Vec<u8>,
    signature: Option<Event>,
}
const RELEASE_MARKER: &str = "buzz-plugin-release-v1";
const RELEASE_KIND: Kind = Kind::Custom(1064);
const SIGNED_ROLLBACK_ERROR: &str = "Cannot roll back a signed plugin to an unsigned revision";

fn publisher(bytes: &[u8], signature: &Event) -> Result<String> {
    publisher_for_hash(&hash(bytes), signature)
}
fn publisher_for_hash(digest: &str, signature: &Event) -> Result<String> {
    signature
        .verify()
        .map_err(|_| "Invalid plugin release event ID or signature")?;
    if signature.kind != RELEASE_KIND || !signature.content.is_empty() {
        return Err("Plugin release must be a NIP-PS event with empty content".into());
    }
    for (key, expected) in [("x", digest.to_string()), ("t", RELEASE_MARKER.into())] {
        let values: Vec<&str> = signature
            .tags
            .iter()
            .filter_map(|tag| {
                let parts = tag.as_slice();
                (parts.first().map(String::as_str) == Some(key))
                    .then(|| (parts.len() == 2).then(|| parts[1].as_str()))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or("Incomplete plugin release tag")?;
        if values.len() != 1 || values[0] != expected {
            return Err(format!("Invalid or ambiguous plugin release {key} tag"));
        }
    }
    Ok(signature.pubkey.to_hex())
}

pub fn sign_release(directory: &Path, key_text: &str) -> Result<String> {
    let bytes = artifact_from_text(
        &read_limited(&directory.join("manifest.json"))?,
        read_limited(&directory.join("plugin.js"))?,
    )?;
    let secret = SecretKey::parse(key_text.trim()).map_err(|_| "Invalid signing key")?;
    let tags = [
        Tag::custom("x", [hash(&bytes).as_str()]),
        Tag::custom("t", [RELEASE_MARKER]),
    ];
    let event = EventBuilder::new(RELEASE_KIND, "")
        .tags(tags)
        .finalize(&Keys::new(secret))
        .map_err(err)?;
    let publisher = publisher(&bytes, &event)?;
    atomic_write(&directory.join("plugin.artifact.json"), &bytes)?;
    atomic_write(
        &directory.join("plugin.signature.json"),
        &serde_json::to_vec_pretty(&event).map_err(err)?,
    )?;
    Ok(publisher)
}

/// Sign with the desktop human identity without creating or exporting a credential.
pub fn sign_release_saved(directory: &Path) -> Result<String> {
    sign_release_from_store(directory, buzz_credential_store::read_human)
}

fn sign_release_from_store<K: AsRef<[u8]>>(
    directory: &Path,
    read: impl FnOnce() -> std::result::Result<K, buzz_credential_store::Error>,
) -> Result<String> {
    use bech32::{primitives::decode::CheckedHrpstring, Bech32};
    use buzz_credential_store::Error;
    let bytes = read().map_err(|error| match error {
        Error::Absent => "Set up your Buzz human identity in the desktop app before signing",
        Error::Denied => "Secure storage access was denied; unlock it and retry signing",
        Error::Busy => "Secure storage is busy; retry signing shortly",
        Error::Corrupt => "Saved Buzz human identity is malformed; nothing was changed",
        _ => "Saved Buzz human identity could not be read from secure storage",
    })?;
    let text = std::str::from_utf8(bytes.as_ref())
        .map_err(|_| "Saved Buzz human identity is malformed; nothing was changed")?
        .trim();
    if text.len() != 63
        || !(text.starts_with("nsec1") || text.starts_with("NSEC1"))
        || CheckedHrpstring::new::<Bech32>(text).is_err()
        || SecretKey::parse(text).is_err()
    {
        return Err("Saved Buzz human identity is malformed; nothing was changed".into());
    }
    sign_release(directory, text)
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub manifest: Manifest,
    pub source: &'static str,
    pub enabled: bool,
    pub revision: String,
    pub previous: Option<String>,
    pub has_signature: bool,
    pub rollback_blocked_reason: Option<&'static str>,
    pub reloadable: bool,
    pub error: Option<String>,
    pub publisher: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub profile: String,
    pub location: String,
    pub plugins: Vec<PluginInfo>,
}
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum InstallationResult {
    Ready {
        catalog: Catalog,
        #[serde(rename = "externalPluginsPaused")]
        external_plugins_paused: bool,
    },
    Recovery {
        reason: String,
        #[serde(rename = "canReset")]
        can_reset: bool,
    },
}
#[derive(Clone)]
pub struct Manager {
    root: PathBuf,
    profile: String,
    safe_mode: bool,
}
impl Manager {
    /// Host control-plane location that protected workers must not modify.
    pub fn storage_root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn open(home: Option<PathBuf>, profile: &str, safe_mode: bool) -> Result<Self> {
        valid_id(profile)?;
        let home = home
            .or_else(|| std::env::var_os("BUZZODZ_HOME").map(PathBuf::from))
            .or_else(|| dirs::data_dir().map(|p| p.join("dev.local.buzz.foundation")))
            .ok_or("Cannot determine application data directory; set BUZZODZ_HOME")?;
        if !home.is_absolute() {
            return Err("Plugin home must be an absolute path".into());
        }
        Ok(Self {
            root: home.join("profiles").join(profile),
            profile: profile.into(),
            safe_mode,
        })
    }
    pub fn from_env() -> Result<Self> {
        Self::open(
            None,
            &std::env::var("BUZZODZ_PROFILE").unwrap_or_else(|_| "default".into()),
            std::env::var("BUZZODZ_SAFE_MODE").as_deref() == Ok("1"),
        )
    }
    fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.root).map_err(err)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("registry.lock"))
            .map_err(err)?;
        file.lock().map_err(err)?;
        Ok(file)
    }
    fn read(&self) -> Result<Registry> {
        let path = self.root.join("registry.json");
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Registry::default())
            }
            Err(error) => return Err(err(error)),
        };
        let text = read_file_limited(file)?;
        let r: Registry = serde_json::from_str(&text).map_err(err)?;
        if r.version != 1 {
            return Err(
                "Unsupported settings version; use a compatible Buzz or recover settings".into(),
            );
        }
        for (id, p) in &r.installed {
            p.manifest.validate()?;
            if *id != p.manifest.id || is_bundled(id) {
                return Err("Invalid installed plugin identity".into());
            }
            valid_hash(&p.current)?;
            if let Some(h) = &p.previous {
                valid_hash(h)?;
            }
        }
        Ok(r)
    }
    fn save(&self, r: &Registry) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(r).map_err(err)?;
        if bytes.len() as u64 > LIMIT {
            return Err(
                "Plugin registry exceeds 8 MiB; reduce release metadata or remove plugins".into(),
            );
        }
        atomic_write(&self.root.join("registry.json"), &bytes)
    }
    fn artifact_path(&self, id: &str, revision: &str) -> PathBuf {
        self.root
            .join("artifacts")
            .join(id)
            .join(format!("{revision}.json"))
    }
    fn artifact(&self, id: &str, revision: &str, signature: Option<&Event>) -> Result<Artifact> {
        valid_id(id)?;
        valid_hash(revision)?;
        let text = read_limited(&self.artifact_path(id, revision))?;
        if hash(text.as_bytes()) != revision {
            return Err(
                "Installed artifact failed its integrity check; reinstall or roll back".into(),
            );
        }
        if let Some(signature) = signature {
            publisher(text.as_bytes(), signature)?;
        }
        let a: Artifact = serde_json::from_str(&text).map_err(err)?;
        a.manifest.validate()?;
        if a.manifest.id != id {
            return Err("Artifact identity mismatch".into());
        }
        Ok(a)
    }
    pub fn external_plugins_paused(&self) -> bool {
        self.safe_mode
    }
    pub fn catalog(&self) -> Result<Catalog> {
        let registry = self.lock().and_then(|_lock| self.read())?;
        let mut plugins: Vec<PluginInfo> = bundled_manifests()
            .into_iter()
            .map(|manifest| {
                // Required even when an older profile saved a disabled override.
                let enabled = manifest.id == "buzz.channels"
                    || registry
                        .bundled_overrides
                        .get(&manifest.id)
                        .copied()
                        .unwrap_or_else(|| enabled_by_default(&manifest.id));
                PluginInfo {
                    manifest,
                    source: "bundled",
                    enabled,
                    revision: "bundled".into(),
                    previous: None,
                    has_signature: false,
                    rollback_blocked_reason: None,
                    reloadable: false,
                    error: None,
                    publisher: None,
                }
            })
            .collect();
        for (id, p) in registry.installed {
            // Catalog polling stays cheap; verify content hashes before enabling/loading.
            let has_signature = p.current_signature.is_some();
            let rollback_blocked_reason =
                (has_signature && p.previous.is_some() && p.previous_signature.is_none())
                    .then_some(SIGNED_ROLLBACK_ERROR);
            let mut error = fs::metadata(self.artifact_path(&id, &p.current))
                .map_err(err)
                .err();
            let publisher = if error.is_none() {
                p.current_signature.as_ref().and_then(|signature| {
                    match publisher_for_hash(&p.current, signature) {
                        Ok(key) => Some(key),
                        Err(reason) => {
                            error = Some(reason);
                            None
                        }
                    }
                })
            } else {
                None
            };
            plugins.push(PluginInfo {
                manifest: p.manifest,
                source: "external",
                enabled: p.enabled,
                revision: p.current,
                previous: p.previous,
                has_signature,
                rollback_blocked_reason,
                reloadable: p.current_source.is_some(),
                error,
                publisher,
            });
        }
        Ok(Catalog {
            profile: self.profile.clone(),
            location: self.root.display().to_string(),
            plugins,
        })
    }
    pub fn install(&self, directory: &Path) -> Result<Catalog> {
        let source = ReloadSource::folder(directory.canonicalize().map_err(err)?, ".".into())?;
        self.install_artifact(&prepare_artifact(directory)?, Some(source))
    }
    fn install_artifact(&self, release: &Release, source: Option<ReloadSource>) -> Result<Catalog> {
        let bytes = &release.bytes;
        let manifest = release.validate()?;
        let revision = hash(bytes);
        {
            let _lock = self.lock()?;
            let mut registry = self.read()?;
            let old = registry.installed.get(&manifest.id);
            if let Some(old) = old {
                if let Some(previous) = &old.current_signature {
                    let previous_publisher = publisher_for_hash(&old.current, previous)?;
                    if release.publisher().as_deref() != Some(previous_publisher.as_str()) {
                        return Err("Publisher changed or signed plugin became unsigned; remove and reinstall to change publisher".into());
                    }
                }
            }
            let (previous, previous_source) = old.map_or((None, None), |p| {
                if p.current == revision {
                    (p.previous.clone(), p.previous_source.clone())
                } else {
                    (Some(p.current.clone()), p.current_source.clone())
                }
            });
            let enabled = old.is_some_and(|p| p.enabled);
            let previous_signature = old.and_then(|p| {
                if p.current == revision {
                    p.previous_signature.clone()
                } else {
                    p.current_signature.clone()
                }
            });
            atomic_write(&self.artifact_path(&manifest.id, &revision), bytes)?;
            registry.installed.insert(
                manifest.id.clone(),
                Installed {
                    manifest,
                    current: revision,
                    current_signature: release.signature.clone(),
                    current_source: source,
                    previous,
                    previous_signature,
                    previous_source,
                    enabled,
                },
            );
            self.save(&registry)?;
        }
        self.catalog()
    }
    pub fn change(&self, action: &str, id: &str) -> Result<Catalog> {
        valid_id(id)?;
        {
            let _lock = self.lock()?;
            let mut registry = self.read()?;
            if is_bundled(id) {
                match action {
                    "enable" => {
                        registry.bundled_overrides.insert(id.into(), true);
                    }
                    "disable" => {
                        if id == "buzz.channels" {
                            return Err("Channels is required and cannot be disabled".into());
                        }
                        registry.bundled_overrides.insert(id.into(), false);
                    }
                    _ => return Err("Bundled pages can only be enabled or disabled".into()),
                }
            } else {
                let p = registry
                    .installed
                    .get_mut(id)
                    .ok_or("Plugin is not installed")?;
                match action {
                    "enable" => {
                        self.artifact(id, &p.current, p.current_signature.as_ref())?;
                        p.enabled = true;
                    }
                    "disable" => p.enabled = false,
                    "remove" => {
                        registry.installed.remove(id);
                    }
                    "rollback" => {
                        let previous = p.previous.clone().ok_or("No previous revision")?;
                        if p.current_signature.is_some() && p.previous_signature.is_none() {
                            return Err(SIGNED_ROLLBACK_ERROR.into());
                        }
                        let a = self.artifact(id, &previous, p.previous_signature.as_ref())?;
                        let previous_source = p.previous_source.clone();
                        p.previous = Some(p.current.clone());
                        p.previous_source = p.current_source.clone();
                        p.current = previous;
                        std::mem::swap(&mut p.current_signature, &mut p.previous_signature);
                        p.current_source = previous_source;
                        p.manifest = a.manifest;
                    }
                    _ => return Err("Unknown management action".into()),
                }
            }
            self.save(&registry)?;
            if action == "remove" {
                // Commit removal first. A cleanup failure cannot re-enable the plugin.
                let path = self.root.join("artifacts").join(id);
                if path.exists() {
                    fs::remove_dir_all(path)
                        .map_err(|e| format!("Plugin removed, but artifact cleanup failed: {e}"))?;
                }
            }
        }
        self.catalog()
    }
    pub fn reload(&self, id: &str) -> Result<Catalog> {
        self.reload_with_commit_hook(id, || {})
    }
    fn reload_with_commit_hook(&self, id: &str, before_commit: impl FnOnce()) -> Result<Catalog> {
        valid_id(id)?;
        if is_bundled(id) {
            return Err("Bundled plugins cannot be reloaded from disk".into());
        }
        let snapshot = {
            let _lock = self.lock()?;
            let registry = self.read()?;
            let plugin = registry
                .installed
                .get(id)
                .ok_or("Plugin is not installed")?;
            if plugin.enabled {
                return Err("Disable the plugin before reloading it from disk".into());
            }
            let source = plugin
                .current_source
                .clone()
                .ok_or("Plugin was not installed from a reloadable folder")?;
            (
                plugin.current.clone(),
                source,
                plugin.manifest.id.clone(),
                plugin.manifest.host.clone().unwrap_or_default(),
            )
        };
        let release = prepare_reload_artifact(&snapshot.1)?;
        let manifest = release.validate()?;
        if manifest.id != snapshot.2 {
            return Err("Reloaded plugin manifest ID changed; import it as a new plugin".into());
        }
        // Compare effective access without changing the stored manifest representation.
        let effective_grants = |mut grants: HostGrants| {
            for command in &mut grants.commands {
                command.max_output_bytes = Some(
                    command
                        .max_output_bytes
                        .unwrap_or(DEFAULT_HOST_COMMAND_OUTPUT_BYTES),
                );
            }
            grants
        };
        if effective_grants(manifest.host.clone().unwrap_or_default())
            != effective_grants(snapshot.3)
        {
            return Err("Host access changed; use Load from folder to review it".into());
        }
        let revision = hash(&release.bytes);
        before_commit();
        {
            let _lock = self.lock()?;
            let mut registry = self.read()?;
            let plugin = registry
                .installed
                .get_mut(id)
                .ok_or("Plugin is not installed")?;
            if plugin.enabled {
                return Err("Disable the plugin before reloading it from disk".into());
            }
            if plugin.current != snapshot.0 || plugin.current_source.as_ref() != Some(&snapshot.1) {
                return Err("Plugin changed while reload was reading from disk; try again".into());
            }
            if let Some(previous) = &plugin.current_signature {
                if release.publisher().as_deref()
                    != Some(publisher_for_hash(&plugin.current, previous)?.as_str())
                {
                    return Err("Publisher changed or signed plugin became unsigned; remove and reinstall to change publisher".into());
                }
            }
            let (previous, previous_source) = if plugin.current == revision {
                (plugin.previous.clone(), plugin.previous_source.clone())
            } else {
                (Some(plugin.current.clone()), plugin.current_source.clone())
            };
            let previous_signature = if plugin.current == revision {
                plugin.previous_signature.clone()
            } else {
                plugin.current_signature.clone()
            };
            atomic_write(&self.artifact_path(&manifest.id, &revision), &release.bytes)?;
            plugin.manifest = manifest;
            plugin.current = revision;
            plugin.current_signature = release.signature;
            plugin.current_source = Some(snapshot.1);
            plugin.previous = previous;
            plugin.previous_signature = previous_signature;
            plugin.previous_source = previous_source;
            self.save(&registry)?;
        }
        self.catalog()
    }
    pub fn module(&self, id: &str, revision: &str) -> Result<String> {
        Ok(self.current_artifact(id, revision)?.code)
    }
    pub fn host_grants(&self, id: &str, revision: &str) -> Result<HostGrants> {
        if revision == "bundled" {
            let plugin = self
                .catalog()?
                .plugins
                .into_iter()
                .find(|plugin| {
                    plugin.source == "bundled" && plugin.manifest.id == id && plugin.enabled
                })
                .ok_or("Bundled plugin is disabled or unavailable")?;
            return Ok(plugin.manifest.host.unwrap_or_default());
        }
        Ok(self
            .current_artifact(id, revision)?
            .manifest
            .host
            .unwrap_or_default())
    }
    fn current_artifact(&self, id: &str, revision: &str) -> Result<Artifact> {
        if self.safe_mode {
            return Err("External plugins are disabled in safe mode".into());
        }
        let _lock = self.lock()?;
        let r = self.read()?;
        let p = r.installed.get(id).ok_or("Plugin is not installed")?;
        if !p.enabled || p.current != revision {
            return Err("Plugin was disabled or updated; refresh the catalog".into());
        }
        self.artifact(id, revision, p.current_signature.as_ref())
    }
    pub fn recover(&self) -> Result<Catalog> {
        {
            let _lock = self.lock()?;
            let path = self.root.join("registry.json");
            if path.exists() {
                let mut backup = tempfile::Builder::new()
                    .prefix("registry-backup-")
                    .suffix(".json")
                    .tempfile_in(&self.root)
                    .map_err(err)?;
                std::io::copy(&mut File::open(&path).map_err(err)?, &mut backup).map_err(err)?;
                backup.as_file().sync_all().map_err(err)?;
                backup.keep().map_err(err)?;
            }
            self.save(&Registry::default())?;
        }
        self.catalog()
    }
}
impl Release {
    fn validate(&self) -> Result<Manifest> {
        if self.bytes.len() as u64 > LIMIT {
            return Err("Plugin artifact exceeds 8 MiB".into());
        }
        let artifact: Artifact = serde_json::from_slice(&self.bytes).map_err(err)?;
        artifact.manifest.validate()?;
        if is_bundled(&artifact.manifest.id) || artifact.code.trim().is_empty() {
            return Err("Invalid plugin artifact".into());
        }
        // IMPORTANT: We allow unsigned plugins currently. Before release, we should add UI to restrict the public keys we trust; and also allowlist Block plugins.
        if let Some(signature) = &self.signature {
            if serde_json::to_vec(signature).map_err(err)?.len() > 64 * 1024 {
                return Err("Plugin release signature exceeds 64 KiB".into());
            }
            publisher(&self.bytes, signature)?;
        }
        Ok(artifact.manifest)
    }
    fn publisher(&self) -> Option<String> {
        self.signature.as_ref().map(|event| event.pubkey.to_hex())
    }
}
fn prepare_artifact(directory: &Path) -> Result<Release> {
    let root =
        cap_std::fs::Dir::open_ambient_dir(directory, cap_std::ambient_authority()).map_err(err)?;
    imports::read_package(&root, Path::new("."))
}
pub fn release_manifest(directory: &Path) -> Result<Manifest> {
    prepare_artifact(directory)?.validate()
}
fn prepare_reload_artifact(source: &ReloadSource) -> Result<Release> {
    let relative = validate_candidate_path(&source.path)?;
    if !fs::symlink_metadata(&source.root)
        .map_err(err)?
        .file_type()
        .is_dir()
    {
        return Err("Reload source root must be a regular folder".into());
    }
    let directory = cap_std::fs::Dir::open_ambient_dir(&source.root, cap_std::ambient_authority())
        .map_err(err)?;
    imports::read_package(&directory, &relative)
}
fn validate_candidate_path(path: &str) -> Result<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        return Err("Reload source path is empty".into());
    }
    if path == "." {
        return Ok(PathBuf::new());
    }
    let path = Path::new(path);
    if !path.is_relative() {
        return Err("Reload source path must be relative".into());
    }
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => relative.push(part),
            _ => return Err("Reload source path must stay inside its folder".into()),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err("Reload source path is empty".into());
    }
    Ok(relative)
}
fn artifact_from_text(manifest: &str, code: String) -> Result<Vec<u8>> {
    let manifest: Manifest = serde_json::from_str(manifest).map_err(err)?;
    manifest.validate()?;
    if is_bundled(&manifest.id) {
        return Err("Cannot replace a bundled plugin".into());
    }
    if code.trim().is_empty() {
        return Err("Plugin module is empty".into());
    }
    let bytes = serde_json::to_vec(&Artifact { manifest, code }).map_err(err)?;
    if bytes.len() as u64 > LIMIT {
        return Err("Plugin artifact exceeds 8 MiB".into());
    }
    Ok(bytes)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn valid_hash(s: &str) -> Result<()> {
    if s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        Ok(())
    } else {
        Err("Invalid artifact revision".into())
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn read_limited(path: &Path) -> Result<String> {
    if !fs::symlink_metadata(path)
        .map_err(err)?
        .file_type()
        .is_file()
    {
        return Err("Plugin files must be regular files, not symbolic links".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(err)?;
    if !file.metadata().map_err(err)?.is_file() {
        return Err("Plugin files must be regular files".into());
    }
    read_file_limited(file)
}
fn read_file_limited(file: File) -> Result<String> {
    let mut text = String::new();
    file.take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(err)?;
    if text.len() as u64 > LIMIT {
        return Err("File exceeds 8 MiB".into());
    }
    Ok(text)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("Missing parent directory")?;
    fs::create_dir_all(parent).map_err(err)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(err)?;
    temp.write_all(bytes).map_err(err)?;
    temp.as_file().sync_all().map_err(err)?;
    temp.persist(path).map_err(err)?;
    #[cfg(unix)]
    File::open(parent).map_err(err)?.sync_all().map_err(err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        artifact_from_text, bundled_manifests, enabled_by_default, Manager, Manifest,
        MAX_HOST_COMMAND_OUTPUT_BYTES,
    };
    use std::collections::BTreeMap;
    use std::fs;

    #[test]
    fn native_catalog_matches_desktop_bundles() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../src/bundled");
        let index = fs::read_to_string(format!("{root}/index.ts")).unwrap();
        let mut dirs = BTreeMap::new();
        for line in index.lines() {
            if let Some(rest) = line.strip_prefix("import ") {
                if let Some((name, path)) = rest.split_once(" from \"./") {
                    if let Some(dir) = path.strip_suffix("/manifest.json\";") {
                        dirs.insert(name.to_string(), dir.to_string());
                    }
                }
            }
        }
        let mut expected = BTreeMap::new();
        for entry in index.split("manifest: { ...").skip(1) {
            let name = entry.split(',').next().unwrap();
            let default = entry
                .split("enabledByDefault: ")
                .nth(1)
                .unwrap()
                .starts_with("true");
            let manifest =
                fs::read_to_string(format!("{root}/{}/manifest.json", dirs[name])).unwrap();
            let manifest: Manifest = serde_json::from_str(&manifest).unwrap();
            expected.insert(manifest.id, default);
        }
        let native: BTreeMap<String, bool> = bundled_manifests()
            .into_iter()
            .map(|manifest| {
                let default = enabled_by_default(&manifest.id);
                (manifest.id, default)
            })
            .collect();
        assert_eq!(native, expected);
    }

    #[test]
    fn saved_human_identity_signs_importable_release_without_storage_fallback() {
        use buzz_credential_store::Error;
        use nostr::{
            key::{Keys, SecretKey},
            nips::nip19::ToBech32,
        };

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("dist");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"id":"example.saved","name":"Saved","apiVersion":1}"#,
        )
        .unwrap();
        fs::write(source.join("plugin.js"), "export const saved = true;").unwrap();
        for (error, expected) in [
            (Error::Absent, "Set up your Buzz human identity"),
            (Error::Denied, "access was denied"),
            (Error::Busy, "busy"),
            (Error::Corrupt, "malformed"),
            (Error::Unavailable, "could not be read"),
        ] {
            assert!(
                super::sign_release_from_store(&source, || Err::<Vec<u8>, _>(error))
                    .unwrap_err()
                    .contains(expected)
            );
            assert!(!source.join("plugin.artifact.json").exists());
        }
        for invalid in [
            b"not-an-nsec".to_vec(),
            vec![0xff],
            SecretKey::generate().to_secret_hex().into_bytes(),
        ] {
            assert!(super::sign_release_from_store(&source, || Ok(invalid))
                .unwrap_err()
                .contains("malformed"));
            assert!(!source.join("plugin.artifact.json").exists());
        }

        let key = SecretKey::generate();
        let nsec = key.to_bech32().unwrap();
        let publisher = super::sign_release_from_store(&source, || Ok(nsec.into_bytes())).unwrap();
        assert_eq!(publisher, Keys::new(key).public_key().to_hex());
        let event: nostr::event::Event =
            serde_json::from_slice(&fs::read(source.join("plugin.signature.json")).unwrap())
                .unwrap();
        assert_eq!(event.kind, super::RELEASE_KIND);
        assert_eq!(event.content, "");
        assert_eq!(event.tags.len(), 2);
        let prepared = crate::imports::prepare_folder(&source).unwrap();
        assert_eq!(
            prepared.preview.candidates[0].publisher.as_deref(),
            Some(publisher.as_str())
        );
        let home = tempfile::tempdir().unwrap();
        let manager = Manager::open(Some(home.path().into()), "test", false).unwrap();
        let path = prepared.preview.candidates[0].path.clone();
        let installed = prepared
            .install(&manager, &prepared.preview.token, &path)
            .unwrap();
        let plugin = installed
            .plugins
            .iter()
            .find(|p| p.manifest.id == "example.saved")
            .unwrap();
        assert_eq!(plugin.publisher.as_deref(), Some(publisher.as_str()));
        manager.change("enable", "example.saved").unwrap();
        assert!(manager
            .module("example.saved", &plugin.revision)
            .unwrap()
            .contains("saved = true"));
    }

    #[test]
    fn release_requires_valid_event_and_exact_artifact_bytes() {
        use super::{publisher, RELEASE_KIND, RELEASE_MARKER};
        use nostr::{
            event::{EventBuilder, FinalizeEvent, Kind, Tag},
            key::Keys,
        };
        let bytes = artifact_from_text(
            r#"{"id":"example.page","name":"Example","apiVersion":1}"#,
            "export const code = 1".into(),
        )
        .unwrap();
        let key = Keys::generate();
        let tags = || {
            vec![
                Tag::custom("x", [super::hash(&bytes)]),
                Tag::custom("t", [RELEASE_MARKER]),
            ]
        };
        let sign = |tags| {
            EventBuilder::new(RELEASE_KIND, "")
                .tags(tags)
                .finalize(&key)
                .unwrap()
        };
        let good = sign(tags());
        assert_eq!(publisher(&bytes, &good).unwrap(), good.pubkey.to_hex());
        let mut extra = tags();
        extra.push(Tag::custom("note", ["not authoritative"]));
        assert_eq!(
            publisher(&bytes, &sign(extra)).unwrap(),
            good.pubkey.to_hex()
        );
        for kind in [Kind::FileMetadata, Kind::TextNote] {
            let wrong_kind = EventBuilder::new(kind, "")
                .tags(tags())
                .finalize(&key)
                .unwrap();
            assert!(publisher(&bytes, &wrong_kind).is_err());
        }
        let nonempty = EventBuilder::new(RELEASE_KIND, "not empty")
            .tags(tags())
            .finalize(&key)
            .unwrap();
        assert!(publisher(&bytes, &nonempty).is_err());
        for altered in [
            artifact_from_text(
                r#"{"id":"example.page","name":"Changed","apiVersion":1}"#,
                "export const code = 1".into(),
            )
            .unwrap(),
            artifact_from_text(
                r#"{"id":"example.page","name":"Example","apiVersion":1}"#,
                "export const code = 2".into(),
            )
            .unwrap(),
        ] {
            assert!(publisher(&altered, &good).is_err());
        }
        let mut bad_id = good.clone();
        bad_id.id = "00".repeat(32).parse().unwrap();
        assert!(publisher(&bytes, &bad_id).is_err());
        let other = EventBuilder::new(RELEASE_KIND, "")
            .tags(tags())
            .finalize(&Keys::generate())
            .unwrap();
        let mut bad_sig = good.clone();
        bad_sig.sig = other.sig;
        assert!(publisher(&bytes, &bad_sig).is_err());
        for invalid in [
            {
                let mut t = tags();
                t.pop();
                t
            },
            {
                let mut t = tags();
                t.remove(0);
                t
            },
            {
                let mut t = tags();
                t[0] = Tag::custom("x", ["00".repeat(32)]);
                t
            },
            {
                let mut t = tags();
                t[0] = Tag::custom("x", [super::hash(&bytes).to_uppercase()]);
                t
            },
            {
                let mut t = tags();
                t[1] = Tag::custom("t", ["wrong-purpose"]);
                t
            },
            {
                let mut t = tags();
                t.push(t[0].clone());
                t
            },
            {
                let mut t = tags();
                t.push(t[1].clone());
                t
            },
            {
                let mut t = tags();
                t[0] = Tag::custom("x", [super::hash(&bytes), String::new()]);
                t
            },
            {
                let mut t = tags();
                t[1] = Tag::custom("t", Vec::<String>::new());
                t
            },
        ] {
            assert!(publisher(&bytes, &sign(invalid)).is_err());
        }
    }

    #[test]
    fn validates_host_declarations_and_old_manifests() {
        let old: Manifest =
            serde_json::from_str(r#"{"id":"example.page","name":"Example","apiVersion":1}"#)
                .unwrap();
        assert_eq!(old.host, None);
        assert!(old.validate().is_ok());
        assert!(!serde_json::to_string(&old).unwrap().contains("host"));
        let manifest = serde_json::json!({
            "id": "example.page", "name": "Example", "apiVersion": 1,
            "host": {
                "commands": [{"id":"status","program":"example-cli","args":["status"]}],
                "networkOrigins": ["https://api.example.com"]
            }
        });
        assert!(artifact_from_text(&manifest.to_string(), "export const x = 1".into()).is_ok());
        let old: Manifest = serde_json::from_value(manifest.clone()).unwrap();
        assert_eq!(
            old.host.as_ref().unwrap().commands[0].max_output_bytes,
            None
        );
        assert_eq!(serde_json::to_value(&old).unwrap(), manifest);
        for limit in [1, 4096, MAX_HOST_COMMAND_OUTPUT_BYTES] {
            let mut valid = manifest.clone();
            valid["host"]["commands"][0]["maxOutputBytes"] = serde_json::json!(limit);
            assert!(artifact_from_text(&valid.to_string(), "export const x = 1".into()).is_ok());
        }
        for limit in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(MAX_HOST_COMMAND_OUTPUT_BYTES + 1),
        ] {
            let mut invalid = manifest.clone();
            invalid["host"]["commands"][0]["maxOutputBytes"] = limit;
            assert!(artifact_from_text(&invalid.to_string(), "export const x = 1".into()).is_err());
        }
        let parsed: Manifest = serde_json::from_value(manifest.clone()).unwrap();
        assert!(!parsed.host.as_ref().unwrap().plugins);
        assert!(!serde_json::to_string(&parsed).unwrap().contains("plugins"));
        let mut managing = manifest.clone();
        managing["host"]["plugins"] = serde_json::json!(true);
        let parsed: Manifest = serde_json::from_value(managing.clone()).unwrap();
        assert!(parsed.host.as_ref().unwrap().plugins);
        assert!(serde_json::to_string(&parsed)
            .unwrap()
            .contains(r#""plugins":true"#));
        assert!(artifact_from_text(&managing.to_string(), "export const x = 1".into()).is_ok());
        managing["host"]["plugins"] = serde_json::json!("all");
        assert!(artifact_from_text(&managing.to_string(), "export const x = 1".into()).is_err());
        for invalid_origin in [
            "http://api.example.com",
            "https://api.example.com/path",
            "https://user@api.example.com",
            "https://api.example.com:443",
        ] {
            let mut invalid = manifest.clone();
            invalid["host"]["networkOrigins"] = serde_json::json!([invalid_origin]);
            assert!(artifact_from_text(&invalid.to_string(), "export const x = 1".into()).is_err());
        }
        let mut invalid = manifest.clone();
        invalid["host"]["commands"][0]["program"] = serde_json::json!("/bin/sh");
        assert!(artifact_from_text(&invalid.to_string(), "export const x = 1".into()).is_err());
        let mut invalid = manifest.clone();
        invalid["host"]["commands"]
            .as_array_mut()
            .unwrap()
            .push(manifest["host"]["commands"][0].clone());
        assert!(artifact_from_text(&invalid.to_string(), "export const x = 1".into()).is_err());

        let mut processes = manifest.clone();
        processes["host"]["processes"] = serde_json::json!([
            {"id":"agent","program":"example-cli"},
            {"id":"install","program":"bash","args":["-c","echo install"]}
        ]);
        assert!(artifact_from_text(&processes.to_string(), "export const x = 1".into()).is_ok());
        let parsed: Manifest = serde_json::from_value(processes.clone()).unwrap();
        assert_eq!(
            parsed.host.as_ref().unwrap().processes[0].args,
            Vec::<String>::new()
        );
        for (field, value) in [
            ("program", serde_json::json!("/bin/sh")),
            ("program", serde_json::json!("")),
            ("id", serde_json::json!("Agent")),
            ("args", serde_json::json!(["a\0b"])),
            ("shell", serde_json::json!(true)),
        ] {
            let mut invalid = processes.clone();
            invalid["host"]["processes"][0][field] = value;
            assert!(artifact_from_text(&invalid.to_string(), "export const x = 1".into()).is_err());
        }
        let mut duplicate = processes.clone();
        duplicate["host"]["processes"][1]["id"] = serde_json::json!("agent");
        assert!(artifact_from_text(&duplicate.to_string(), "export const x = 1".into()).is_err());
    }

    #[test]
    fn host_grants_follow_enabled_current_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let manager = Manager::open(Some(temp.path().into()), "test", false).unwrap();
        let source = temp.path().join("build");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("plugin.js"), "export function apply() {}").unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"id":"example.page","name":"Example","apiVersion":1,"host":{"commands":[{"id":"first","program":"example-cli","args":["status"]}],"networkOrigins":["https://one.example"]}}"#,
        )
        .unwrap();
        let first = manager
            .install(&source)
            .unwrap()
            .plugins
            .into_iter()
            .find(|plugin| plugin.manifest.id == "example.page")
            .unwrap()
            .revision;
        assert!(manager.host_grants("example.page", &first).is_err());
        manager.change("enable", "example.page").unwrap();
        assert!(manager.host_grants("example.page", "bundled").is_err());
        let paused = Manager::open(Some(temp.path().into()), "test", true).unwrap();
        assert!(paused.host_grants("example.page", &first).is_err());
        assert_eq!(
            manager
                .host_grants("example.page", &first)
                .unwrap()
                .commands[0]
                .id,
            "first"
        );

        fs::write(
            source.join("manifest.json"),
            r#"{"id":"example.page","name":"Example","apiVersion":1,"host":{"commands":[{"id":"second","program":"example-cli","args":["status","--json"],"maxOutputBytes":65536}],"networkOrigins":["https://two.example"]}}"#,
        )
        .unwrap();
        let second = manager
            .install(&source)
            .unwrap()
            .plugins
            .into_iter()
            .find(|plugin| plugin.manifest.id == "example.page")
            .unwrap()
            .revision;
        assert!(manager.host_grants("example.page", &first).is_err());
        let grants = manager.host_grants("example.page", &second).unwrap();
        assert_eq!(grants.commands[0].id, "second");
        assert_eq!(grants.commands[0].max_output_bytes, Some(65536));
        let reopened = Manager::open(Some(temp.path().into()), "test", false).unwrap();
        assert_eq!(
            reopened.host_grants("example.page", &second).unwrap(),
            grants
        );
        assert_eq!(grants.network_origins, ["https://two.example"]);
        manager.change("disable", "example.page").unwrap();
        assert!(manager.host_grants("example.page", &second).is_err());
    }

    #[test]
    fn host_grants_follow_enabled_bundled_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let manager = Manager::open(Some(temp.path().into()), "test", false).unwrap();
        let grants = manager.host_grants("block.builderlab", "bundled").unwrap();
        assert_eq!(
            grants.network_origins,
            if env!("BUZZ_BUILDERLAB_ORIGIN").is_empty() {
                vec![]
            } else {
                vec![env!("BUZZ_BUILDERLAB_ORIGIN").to_owned()]
            }
        );
        assert!(grants.commands.is_empty());
        assert!(manager
            .host_grants("block.builderlab", "wrong-revision")
            .is_err());
        assert!(manager.host_grants("unknown.plugin", "bundled").is_err());
        assert_eq!(
            manager.host_grants("buzz.channels", "bundled").unwrap(),
            super::HostGrants::default()
        );

        manager.change("disable", "block.builderlab").unwrap();
        let reopened = Manager::open(Some(temp.path().into()), "test", false).unwrap();
        assert!(reopened.host_grants("block.builderlab", "bundled").is_err());
        manager.change("enable", "block.builderlab").unwrap();
        assert_eq!(
            reopened.host_grants("block.builderlab", "bundled").unwrap(),
            grants
        );

        // Safe mode pauses external plugins; enabled bundled plugins remain usable.
        let paused = Manager::open(Some(temp.path().into()), "test", true).unwrap();
        assert_eq!(
            paused.host_grants("block.builderlab", "bundled").unwrap(),
            grants
        );
    }

    #[test]
    fn reload_rejects_changed_host_grants_before_enable() {
        let temp = tempfile::tempdir().unwrap();
        let manager = Manager::open(Some(temp.path().into()), "test", false).unwrap();
        let source = temp.path().join("build");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("plugin.js"), "export function apply() {}").unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"id":"example.page","name":"Example","apiVersion":1}"#,
        )
        .unwrap();
        let first = manager
            .install(&source)
            .unwrap()
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "example.page")
            .unwrap()
            .revision
            .clone();

        fs::write(
            source.join("manifest.json"),
            r#"{"id":"example.page","name":"Example","apiVersion":1,"host":{"commands":[{"id":"status","program":"example-cli","args":["status"]}]}}"#,
        )
        .unwrap();
        let error = match manager.reload("example.page") {
            Ok(_) => panic!("reload should reject changed host grants"),
            Err(error) => error,
        };
        assert!(error.contains("Load from folder"));

        let catalog = manager.catalog().unwrap();
        let plugin = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "example.page")
            .unwrap();
        assert_eq!(plugin.revision, first);
        assert!(plugin.previous.is_none());
        manager.change("enable", "example.page").unwrap();
        assert!(manager
            .host_grants("example.page", &first)
            .unwrap()
            .commands
            .is_empty());
    }

    #[test]
    fn optional_plugins_default_off_and_preserve_explicit_overrides() {
        for id in ["buzz.todos", "buzz.channel-templates"] {
            let temp = tempfile::tempdir().unwrap();
            let manager = Manager::open(Some(temp.path().into()), "optional-test", false).unwrap();
            let enabled = |manager: &Manager| {
                manager
                    .catalog()
                    .unwrap()
                    .plugins
                    .into_iter()
                    .find(|plugin| plugin.manifest.id == id)
                    .unwrap()
                    .enabled
            };
            assert!(!enabled(&manager), "{id} defaults off");
            manager.change("enable", id).unwrap();
            let reopened = Manager::open(Some(temp.path().into()), "optional-test", false).unwrap();
            assert!(enabled(&reopened), "{id} retains enable after reopening");
            reopened.change("disable", id).unwrap();
            assert!(!enabled(&manager), "{id} exposes persisted disable");
        }
    }

    #[test]
    fn reload_rejects_enable_between_disk_read_and_commit() {
        let temp = tempfile::tempdir().unwrap();
        let manager = Manager::open(Some(temp.path().into()), "test", false).unwrap();
        let source = temp.path().join("build");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("manifest.json"),
            r#"{"id":"example.page","name":"Example","apiVersion":1}"#,
        )
        .unwrap();
        fs::write(source.join("plugin.js"), "export function apply() {}").unwrap();
        let first = manager
            .install(&source)
            .unwrap()
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "example.page")
            .unwrap()
            .revision
            .clone();
        fs::write(source.join("plugin.js"), "export const reloaded = true;").unwrap();

        let error = match manager.reload_with_commit_hook("example.page", || {
            manager.change("enable", "example.page").unwrap();
        }) {
            Ok(_) => panic!("reload should reject an enabled plugin at commit"),
            Err(error) => error,
        };

        assert!(error.contains("Disable the plugin before reloading"));
        let catalog = manager.catalog().unwrap();
        let plugin = catalog
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == "example.page")
            .unwrap();
        assert!(plugin.enabled);
        assert_eq!(plugin.revision, first);
        assert_eq!(plugin.previous, None);
    }
}
