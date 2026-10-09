//! Native custody for agents the app runs through a plugin (Agents2). The key
//! and owner attestation never enter the WebView; it asks for one bounded
//! event, read, upload or memory write at a time, and native signs and sends it
//! to the agent's community.
use crate::agents::profile_http::authorization;
use buzz_agent_controller::{
    AppAgent, AppAgents, Credentials, PlatformCredentials, Published, Secret, CREDENTIALS_BUSY,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const BUSY_WAITS: u32 = 50;
const BUSY_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

/// The identity file (which locks its own writes across app instances) and its
/// own credential handle, so Agents2 never waits on, or fails with, the harness
/// agent controller (whose storage lock another app may hold).
#[derive(Clone)]
pub(crate) struct AppAgentHost {
    agents: Result<AppAgents, String>,
    credentials: Arc<dyn Credentials>,
    /// Each key after its first read, so only create and first use reach the
    /// credential store, whose lock refuses rather than waits. Waiting on this
    /// lock instead lets concurrent publications queue for that first read.
    keys: Arc<Mutex<HashMap<String, Arc<Secret>>>>,
    /// One profile publication per agent at a time, so each is newer than the
    /// last; Delete takes it too, so no publication outlives the key.
    profiles: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    hosting: Arc<Hosting>,
}
impl AppAgentHost {
    /// `packaged` is false for a dev build, which steps aside for a packaged app.
    pub(crate) fn new(path: Result<PathBuf, String>, packaged: bool) -> Self {
        let hosts = path
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|path| Ok(path.parent().ok_or("Invalid agent storage")?.join("hosts")));
        Self {
            agents: path.map(AppAgents::open),
            credentials: Arc::new(PlatformCredentials::default()),
            keys: Arc::default(),
            profiles: Arc::default(),
            hosting: Arc::new(Hosting {
                dir: hosts,
                packaged,
                held: Mutex::default(),
            }),
        }
    }
    fn keys(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Secret>>> {
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn profile_turn(&self, pubkey: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(pubkey.into())
            .or_default()
            .clone()
    }
    /// The agent's key; read from the credential store only on first use. A
    /// busy store (a harness start or another app holding it) is waited out for
    /// a few seconds, without holding up agents whose keys are already read.
    async fn key(&self, agent: AppAgent) -> Result<(AppAgent, Arc<Secret>), String> {
        let host = self.clone();
        blocking(move || {
            let mut waits = 0;
            loop {
                let mut keys = host.keys();
                if let Some(key) = keys.get(&agent.pubkey) {
                    return Ok((agent, key.clone()));
                }
                match agent.read_key(host.credentials.as_ref()) {
                    Err(error) if error == CREDENTIALS_BUSY && waits < BUSY_WAITS => {
                        drop(keys);
                        waits += 1;
                        std::thread::sleep(BUSY_WAIT);
                    }
                    read => {
                        let key = Arc::new(read?);
                        keys.insert(agent.pubkey.clone(), key.clone());
                        return Ok((agent, key));
                    }
                }
            }
        })
        .await
    }
    /// Deletes the key once any profile publication in flight has settled, so
    /// the community's cleanup sees every profile it will ever have. The row
    /// stays, marked deleted, until the WebView has cleaned up the community and
    /// calls `app_agent_forget`.
    async fn delete(&self, pubkey: String) -> Result<(), String> {
        let turn = self.profile_turn(&pubkey);
        let _turn = turn.lock().await;
        let host = self.clone();
        blocking(move || {
            // Held across the deletion, so no publication re-reads the key meanwhile.
            let mut keys = host.keys();
            keys.remove(&pubkey);
            host.hosting.held().remove(&pubkey);
            host.agents
                .clone()?
                .remove(&pubkey, host.credentials.as_ref())
        })
        .await
    }
    /// The agent, if it still has its key. One deleted elsewhere loses its cached key.
    async fn agent(&self, pubkey: String) -> Result<AppAgent, String> {
        let host = self.clone();
        blocking(move || {
            let agent = host.agents.clone()?.get(&pubkey);
            if agent.is_err() {
                host.keys().remove(&pubkey);
            }
            agent
        })
        .await
    }
}

/// Which running copy of the app runs each agent. Every copy on this machine
/// (the packaged app and any dev builds) shares the agent list and hears the
/// same events. The packaged app always runs what it can and holds
/// `<pubkey>.packaged` while it does; there is only ever one, since release
/// builds are single-instance. Dev builds step aside while that is held, and
/// otherwise run each agent from whichever holds `<pubkey>.lock`. The OS lets go
/// of both when a copy exits, however it exits.
struct Hosting {
    dir: Result<PathBuf, String>,
    packaged: bool,
    /// The lock file this copy holds per agent: `.packaged` or `.lock`.
    held: Mutex<HashMap<String, File>>,
}
impl Hosting {
    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, File>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Whether this copy runs the agent. The packaged app always does, so a
    /// stuck dev build can never silence it. A dev build takes the agent if no
    /// other copy runs it, and lets go while the packaged app does.
    #[allow(clippy::incompatible_msrv)] // `File` locking; see `sign_out::Instance`.
    fn claim(&self, pubkey: &str) -> Result<bool, String> {
        if pubkey.len() != 64
            || !pubkey
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err("Invalid agent".into());
        }
        let dir = self.dir.as_ref().map_err(Clone::clone)?;
        std::fs::create_dir_all(dir).map_err(|_| "Could not open agent hosting")?;
        let packaged = dir.join(format!("{pubkey}.packaged"));
        let mut held = self.held();
        if self.packaged {
            if !held.contains_key(pubkey) {
                let file = open_lock(&packaged)?;
                // A dev build checking for it holds it briefly; this tries again next time.
                if lock(file.try_lock())? {
                    held.insert(pubkey.into(), file);
                }
            }
            return Ok(true);
        }
        if !lock(open_lock(&packaged)?.try_lock_shared())? {
            held.remove(pubkey);
            return Ok(false);
        }
        if !held.contains_key(pubkey) {
            let file = open_lock(&dir.join(format!("{pubkey}.lock")))?;
            if lock(file.try_lock())? {
                held.insert(pubkey.into(), file);
            }
        }
        Ok(held.contains_key(pubkey))
    }
}
/// Whether a try at a lock took it; closing the file lets go.
#[allow(clippy::incompatible_msrv)]
fn lock(attempt: Result<(), TryLockError>) -> Result<bool, String> {
    match attempt {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(_)) => Err("Could not read agent hosting".into()),
    }
}
fn open_lock(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| "Could not open agent hosting lock".to_owned())
}

/// Runs blocking file and credential work off the async runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| "Native credential operation failed".to_owned())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppAgentSummary {
    pubkey: String,
    relay: String,
    owner: String,
    #[serde(rename = "type")]
    agent_type: String,
    name: String,
    deleted: bool,
}
impl From<AppAgent> for AppAgentSummary {
    fn from(agent: AppAgent) -> Self {
        Self {
            pubkey: agent.pubkey,
            relay: agent.relay,
            owner: agent.owner,
            agent_type: agent.agent_type,
            name: agent.name,
            deleted: agent.deleted,
        }
    }
}

#[tauri::command]
pub(crate) async fn app_agent_list(
    state: tauri::State<'_, AppAgentHost>,
) -> Result<Vec<AppAgentSummary>, String> {
    let agents = state.agents.clone()?;
    blocking(move || Ok(agents.list()?.into_iter().map(Into::into).collect())).await
}

/// Generates a key for `owner` in `destination`, has the signed-in owner attest
/// it, and saves both. Nothing is saved unless all three succeed.
#[tauri::command]
pub(crate) async fn app_agent_create(
    state: tauri::State<'_, AppAgentHost>,
    identity: tauri::State<'_, crate::identity::IdentityHost>,
    destination: String,
    owner: String,
    agent_type: String,
    name: String,
) -> Result<AppAgentSummary, String> {
    let agents = state.agents.clone()?;
    let prepared = AppAgents::prepare(&destination, &owner, &agent_type, &name)?;
    let auth = identity
        .inner()
        .authorize_agent(owner, prepared.pubkey().to_owned())
        .await?;
    let host = state.inner().clone();
    blocking(move || {
        // Held so no first key read meets this write in the credential store.
        let mut keys = host.keys();
        host.credentials.retry();
        let (agent, key) = agents.commit(prepared, &auth, host.credentials.as_ref())?;
        keys.insert(agent.pubkey.clone(), Arc::new(key));
        Ok(agent.into())
    })
    .await
}

#[tauri::command]
pub(crate) async fn app_agent_rename(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
    name: String,
) -> Result<(), String> {
    let agents = state.agents.clone()?;
    blocking(move || agents.rename(&pubkey, &name)).await
}

#[tauri::command]
pub(crate) async fn app_agent_delete(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
) -> Result<(), String> {
    state.delete(pubkey).await
}

#[tauri::command]
pub(crate) async fn app_agent_forget(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
) -> Result<(), String> {
    let agents = state.agents.clone()?;
    blocking(move || agents.forget(&pubkey)).await
}

/// Whether this copy of the app runs the agent; see `Hosting`.
#[tauri::command]
pub(crate) async fn app_agent_claim(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
) -> Result<bool, String> {
    let hosting = state.hosting.clone();
    blocking(move || hosting.claim(&pubkey)).await
}

/// Lets another copy of the app run the agent, as when this one cannot.
#[tauri::command]
pub(crate) fn app_agent_release(state: tauri::State<'_, AppAgentHost>, pubkey: String) {
    state.hosting.held().remove(&pubkey);
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppAgentEvent {
    kind: u16,
    content: String,
    #[serde(default)]
    tags: Vec<Vec<String>>,
}

/// Signs one event as the agent and posts it to the agent's own community.
#[tauri::command]
pub(crate) async fn app_agent_publish(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
    event: AppAgentEvent,
) -> Result<Value, String> {
    let (agent, key) = state.key(state.agent(pubkey).await?).await?;
    let signed = agent.sign(&key, event.kind, event.content, event.tags)?;
    post(&agent, &key, signed).await
}

/// Publishes the agent's profile if its name differs from the last one its
/// community accepted. Safe to call any time; one runs per agent at a time.
#[tauri::command]
pub(crate) async fn app_agent_publish_profile(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
) -> Result<(), String> {
    let turn = state.profile_turn(&pubkey);
    let _turn = turn.lock().await;
    let agent = state.agent(pubkey).await?;
    if agent.profile.as_ref().map(|last| &last.name) == Some(&agent.name) {
        return Ok(());
    }
    let (agent, key) = state.key(agent).await?;
    let signed = post(&agent, &key, agent.profile_event(&key)?).await?;
    let created_at = signed
        .get("created_at")
        .and_then(Value::as_u64)
        .ok_or("Invalid agent event")?;
    let agents = state.agents.clone()?;
    blocking(move || {
        agents.published(
            &agent.pubkey,
            Published {
                name: agent.name,
                created_at,
            },
        )
    })
    .await
}

/// Reads from the agent's community as the agent, so it sees what the agent
/// may see rather than what the owner may.
#[tauri::command]
pub(crate) async fn app_agent_query(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
    filters: Vec<Value>,
) -> Result<Value, String> {
    if filters.is_empty() || filters.len() > 8 {
        return Err("Agents read with one to eight filters".into());
    }
    let (agent, key) = state.key(state.agent(pubkey).await?).await?;
    let bytes = serde_json::to_vec(&filters).map_err(|_| "Could not encode agent read")?;
    let response = client(20)?
        .post(agent.query_url())
        .header("Content-Type", "application/json")
        .header(
            "Authorization",
            authorization(agent.http_auth(&key, &agent.query_url(), &bytes)?)?,
        )
        .header("x-auth-tag", &agent.auth)
        .body(bytes)
        .send()
        .await
        .map_err(|_| "Agent read failed")?;
    let status = response.status();
    let body = bounded(response, 8 * 1024 * 1024).await?;
    if !status.is_success() {
        return Err(format!(
            "The community refused the agent read: {}",
            reason(status, &body)
        ));
    }
    serde_json::from_slice(&body).map_err(|_| "Invalid agent read".into())
}

/// Media an agent may upload, as the community accepts them.
const UPLOAD_TYPES: [&str; 5] = [
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "video/mp4",
];
const UPLOAD_LIMIT: usize = 50 * 1024 * 1024;

/// Uploads one file (base64 `data` of type `mime`) to the agent's community
/// as the agent, and returns the community's description of it.
#[tauri::command]
pub(crate) async fn app_agent_upload(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
    data: String,
    mime: String,
) -> Result<Value, String> {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    if !UPLOAD_TYPES.contains(&mime.as_str()) || data.len() > UPLOAD_LIMIT / 3 * 4 + 4 {
        return Err("Agents upload images or MP4 video up to 50 MB".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "Invalid upload data")?;
    let (agent, key) = state.key(state.agent(pubkey).await?).await?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let auth = serde_json::to_vec(&agent.upload_auth(&key, &sha256)?)
        .map_err(|_| "Could not authorize upload")?;
    let response = client(120)?
        .put(agent.upload_url())
        .header("Content-Type", &mime)
        .header("X-SHA-256", &sha256)
        .header(
            "Authorization",
            format!(
                "Nostr {}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(auth)
            ),
        )
        .header("x-auth-tag", &agent.auth)
        .body(bytes)
        .send()
        .await
        .map_err(|_| "Agent upload unconfirmed")?;
    let status = response.status();
    let body = bounded(response, 64 * 1024).await?;
    if !status.is_success() {
        return Err(format!(
            "The community refused the upload: {}",
            reason(status, &body)
        ));
    }
    serde_json::from_slice(&body).map_err(|_| "Invalid upload receipt".into())
}

/// Writes one memory entry, encrypted to the agent's owner. `after` is the
/// `created_at` of the entry it replaces, or 0.
#[tauri::command]
pub(crate) async fn app_agent_remember(
    state: tauri::State<'_, AppAgentHost>,
    pubkey: String,
    slug: String,
    body: String,
    after: u64,
) -> Result<Value, String> {
    let (agent, key) = state.key(state.agent(pubkey).await?).await?;
    let signed = agent.memory(&key, &slug, &body, after)?;
    post(&agent, &key, signed).await
}

fn client(seconds: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(seconds))
        .build()
        .map_err(|_| "Agent client unavailable".into())
}

async fn bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "Agent request failed")? {
        if body.len() + chunk.len() > limit {
            return Err("Agent response is too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Why the community refused, from its JSON body when it gives one.
fn reason(status: reqwest::StatusCode, body: &[u8]) -> String {
    let receipt: Value =
        serde_json::from_slice(&body[..body.len().min(16 * 1024)]).unwrap_or(Value::Null);
    receipt
        .get("message")
        .or_else(|| receipt.get("error"))
        .and_then(Value::as_str)
        .map(|text| text.chars().take(300).collect::<String>())
        .unwrap_or_else(|| format!("status {}", status.as_u16()))
}

async fn post(agent: &AppAgent, key: &Secret, signed: Value) -> Result<Value, String> {
    let event_id = signed
        .get("id")
        .and_then(Value::as_str)
        .ok_or("Invalid agent event")?
        .to_owned();
    let bytes = serde_json::to_vec(&signed).map_err(|_| "Could not encode agent event")?;
    let response = client(20)?
        .post(agent.events_url())
        .header("Content-Type", "application/json")
        .header(
            "Authorization",
            authorization(agent.http_auth(key, &agent.events_url(), &bytes)?)?,
        )
        .header("x-auth-tag", &agent.auth)
        .body(bytes)
        .send()
        .await
        .map_err(|_| "Agent event unconfirmed; it may or may not have been accepted")?;
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    let receipt: Value =
        serde_json::from_slice(&body[..body.len().min(16 * 1024)]).unwrap_or(Value::Null);
    if !status.is_success()
        || receipt.get("accepted").and_then(Value::as_bool) != Some(true)
        || receipt.get("event_id").and_then(Value::as_str) != Some(event_id.as_str())
    {
        return Err(format!(
            "The community refused the agent event: {}",
            reason(status, &body)
        ));
    }
    Ok(signed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// One saved key, behind a store that is busy for the first few reads.
    struct Busy(String, AtomicU32);
    impl Credentials for Busy {
        fn read_legacy(
            &self,
            _: buzz_agent_controller::LegacySource,
            _: &str,
        ) -> Result<Secret, String> {
            unreachable!()
        }
        fn read(&self, _: &str, pubkey: &str) -> Result<Option<Secret>, String> {
            if self.1.load(Ordering::SeqCst) > 0 {
                self.1.fetch_sub(1, Ordering::SeqCst);
                return Err(CREDENTIALS_BUSY.into());
            }
            Secret::parse(&self.0, pubkey).map(Some)
        }
        fn add(&self, _: &str, _: &Secret) -> Result<(), String> {
            unreachable!()
        }
        fn delete(&self, _: &str, _: &str) -> Result<(), String> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn a_first_key_read_waits_out_a_busy_store() {
        let key = Secret::generate().unwrap();
        let host = AppAgentHost {
            agents: Err("unused".into()),
            credentials: Arc::new(Busy(key.hex().to_string(), AtomicU32::new(3))),
            keys: Arc::default(),
            profiles: Arc::default(),
            hosting: Arc::new(hosting(Err("unused".into()), true)),
        };
        let agent = AppAgent {
            pubkey: key.pubkey().into(),
            relay: "wss://relay.example".into(),
            owner: "a".repeat(64),
            auth: "[]".into(),
            agent_type: "p/t".into(),
            name: "Ada".into(),
            profile: None,
            deleted: false,
        };
        let (_, read) = host.key(agent).await.unwrap();
        assert_eq!(read.pubkey(), key.pubkey());
    }

    #[tokio::test]
    async fn delete_waits_for_a_profile_publication_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let host = AppAgentHost::new(Ok(dir.path().join("identities.json")), true);
        let pubkey = "c".repeat(64);
        let turn = host.profile_turn(&pubkey);
        let publishing = turn.lock().await;
        let mut deleting = tokio::spawn({
            let host = host.clone();
            let pubkey = pubkey.clone();
            async move { host.delete(pubkey).await }
        });
        // Unblocked, deleting an absent agent takes well under this.
        let waited = std::time::Duration::from_millis(300);
        assert!(tokio::time::timeout(waited, &mut deleting).await.is_err());
        drop(publishing);
        deleting.await.unwrap().unwrap();
    }

    fn hosting(dir: Result<PathBuf, String>, packaged: bool) -> Hosting {
        Hosting {
            dir,
            packaged,
            held: Mutex::default(),
        }
    }

    #[test]
    fn one_copy_runs_each_agent_until_it_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = ("a".repeat(64), "b".repeat(64));
        let one = hosting(Ok(dir.path().join("hosts")), false);
        let other = hosting(Ok(dir.path().join("hosts")), false);
        assert!(one.claim(&first).unwrap());
        assert!(one.claim(&first).unwrap());
        assert!(!other.claim(&first).unwrap());
        assert!(other.claim(&second).unwrap());
        // As when that copy quits.
        drop(one);
        assert!(other.claim(&first).unwrap());
        assert!(other.claim("../x").is_err());
    }

    #[test]
    fn a_packaged_app_runs_what_it_can_and_dev_builds_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, other) = ("a".repeat(64), "b".repeat(64));
        let dev = hosting(Ok(dir.path().join("hosts")), false);
        let packaged = hosting(Ok(dir.path().join("hosts")), true);
        assert!(dev.claim(&agent).unwrap());
        assert!(dev.claim(&other).unwrap());
        // The packaged app runs it at once, though the dev build had it first,
        assert!(packaged.claim(&agent).unwrap());
        // and the dev build lets go at its next claim.
        assert!(!dev.claim(&agent).unwrap());
        assert!(packaged.claim(&agent).unwrap());
        // A second dev build cannot take it either.
        let another = hosting(Ok(dir.path().join("hosts")), false);
        assert!(!another.claim(&agent).unwrap());
        // One the packaged app does not ask for stays with the dev build.
        assert!(dev.claim(&other).unwrap());
        // As when it can no longer run it.
        packaged.held().remove(&agent);
        assert!(dev.claim(&agent).unwrap());
        assert!(!another.claim(&agent).unwrap());
    }
}
