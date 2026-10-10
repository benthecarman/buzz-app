//! Purpose-bound agent operations. Never return key material or unscoped plaintext.
use super::{hex_key, origin, send, Result};
use crate::identity::IdentityHost;
use buzz_desktop::relay::agent::{self as agent, AgentTarget, LogTarget};
pub(crate) use buzz_desktop::relay::agent::{decode_archive, slug, AgentEvent};
use serde_json::Value;
use tauri::Manager;

#[tauri::command]
pub(crate) async fn relay_agent_resolve(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    target: AgentTarget,
) -> Result<Value> {
    let relay_url = agent::relay(&community)?;
    host.with_key(move |secret, viewer| agent::resolve(secret, viewer, relay_url, target))
        .await
}

#[tauri::command]
pub(crate) async fn relay_agent_log_proof(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    target: LogTarget,
) -> Result<String> {
    let relays = agent::log_relays(&community, &target)?;
    host.with_key(move |secret, viewer| agent::log_proof(secret, viewer, relays, target))
        .await
}

#[tauri::command]
pub(crate) async fn relay_agent_observer(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    event: AgentEvent,
) -> Result<Value> {
    origin(&community)?;
    host.with_key(move |secret, viewer| agent::decode_observer(secret, viewer, &event))
        .await
}

#[tauri::command]
pub(crate) async fn relay_agent_memories_read(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    agent: String,
) -> Result<Value> {
    let url = origin(&community)?
        .join("/query")
        .map_err(|_| "Invalid community")?;
    if !hex_key(&agent) {
        return Err("Invalid memory target".into());
    }
    let viewer = host.with_key(|_, viewer| Ok(viewer.to_owned())).await?;
    let body = agent::memory_query(&viewer, &agent)?;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        send(host.inner(), url, "POST", Some(body), true, 2 * 1024 * 1024),
    )
    .await
    .map_err(|_| "Memory read failed")??;
    if response.status == 401 || response.status == 403 {
        return Err("MemoryDenied".into());
    }
    if response.status != 200 || response.body.len() > 2 * 1024 * 1024 {
        return Err("Memory read failed".into());
    }
    let events = agent::memory_wire_records(&response.body)?;
    origin(&community)?;
    host.with_key(move |secret, current| {
        agent::decode_memories(secret, current, &viewer, &agent, &events)
    })
    .await
}

#[tauri::command]
pub(crate) async fn relay_agent_library<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Value> {
    let parent = app
        .path()
        .data_dir()
        .map_err(|_| "Current Buzz library unavailable")?;
    let path = parent.join("xyz.block.buzz.app/agents/managed-agents.json");
    tauri::async_runtime::spawn_blocking(move || agent::read_agent_library(path))
        .await
        .map_err(|_| "Current Buzz library unavailable".to_owned())?
}
