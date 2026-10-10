//! Packaged human relay access. Credentials stay with IdentityHost; redirects never carry auth.
pub(crate) mod agent;
use crate::identity::{EventTemplate, IdentityHost};
pub(crate) use agent::{
    relay_agent_library, relay_agent_log_proof, relay_agent_memories_read, relay_agent_observer,
    relay_agent_resolve,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use buzz_desktop::media;
use buzz_desktop::relay::events::{template, validate_event, verify_signature, workflow_uuid};
pub(crate) use buzz_desktop::relay::origin;
use buzz_desktop::relay::{catalog, channel_writes as channel_policy, project_git};
use buzz_desktop::relay::{
    client, hex_key, read_bounded, send, RelayResponse, MAX_BODY, MAX_RESPONSE,
};
use buzz_desktop::upload::{process_spool, upload_id, UPLOAD_CHUNK};
use url::Url;

mod channel_writes;
mod kit;
pub(crate) use channel_writes::{
    relay_channel_publish, relay_channel_sign, relay_direct_message, relay_kit_decode,
    relay_kit_prepare,
};
pub(crate) use kit::relay_kit_sign;
mod reminders;
pub(crate) use buzz_desktop::media::stream::MediaStream;
pub(crate) use buzz_desktop::upload::{Spools, Uploads};
pub(crate) use reminders::{relay_decode_reminders, relay_sign_reminder};
type Result<T> = std::result::Result<T, String>;

fn request_url(community: &str, path: &str, method: &str) -> Result<Url> {
    let allowed = match method {
        "GET" => matches!(path, "/" | "/api/join-policy"),
        "POST" => matches!(
            path,
            "/query"
                | "/events"
                | "/api/invites"
                | "/api/invites/claim"
                | "/api/invites/accept-policy"
                | "/gifs/search"
        ),
        _ => false,
    };
    if !allowed {
        return Err("Unsupported relay request".into());
    }
    origin(community)?
        .join(path)
        .map_err(|_| "Invalid relay path".into())
}

fn valid_workflow_time(value: &str) -> bool {
    // Timestamp syntax is bounded here; relay owns interpretation of the cursor.
    value.bytes().all(|byte| {
        byte.is_ascii_digit() || matches!(byte, b'-' | b'T' | b':' | b'.' | b'Z' | b'+')
    })
}

fn workflow_runs_url(community: &str, id: &str, cursor: Option<&WorkflowCursor>) -> Result<Url> {
    if !workflow_uuid(id)
        || cursor.is_some_and(|c| {
            !workflow_uuid(&c.before_id)
                || c.before.len() > 40
                || !c.before.as_bytes().get(0..10).is_some_and(|prefix| {
                    prefix.iter().enumerate().all(|(i, b)| {
                        if i == 4 || i == 7 {
                            *b == b'-'
                        } else {
                            b.is_ascii_digit()
                        }
                    })
                })
                || c.before.as_bytes().get(10) != Some(&b'T')
                || !valid_workflow_time(&c.before)
        })
    {
        return Err("Invalid workflow read".into());
    }
    let mut url = origin(community)?
        .join(&format!("/workflows/{id}/runs"))
        .map_err(|_| "Invalid workflow read")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("limit", "20");
        if let Some(cursor) = cursor {
            query.append_pair("before", &cursor.before);
            query.append_pair("before_id", &cursor.before_id);
        }
    }
    Ok(url)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkflowCursor {
    before: String,
    before_id: String,
}

#[tauri::command]
pub(crate) async fn relay_workflow_runs(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    id: String,
    cursor: Option<WorkflowCursor>,
) -> Result<RelayResponse> {
    let url = workflow_runs_url(&community, &id, cursor.as_ref())?;
    send(host.inner(), url, "GET", None, true, 1024 * 1024).await
}

/// A Buzz git repository on this community: `<origin>/git/<owner hex>/<name>`.
fn git_repository(community: &str, repository: &str) -> Result<Url> {
    let origin = origin(community)?;
    let url = Url::parse(repository).map_err(|_| "Not a repository in this community")?;
    let segments: Vec<_> = url.path().split('/').skip(1).collect();
    // The relay's rule: strip one optional `.git`, then 1–64 of [A-Za-z0-9._-],
    // no leading dot and no "..".
    let name = |value: &str| {
        let value = value.strip_suffix(".git").unwrap_or(value);
        !value.is_empty()
            && value.len() <= 64
            && !value.starts_with('.')
            && !value.contains("..")
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if url.as_str() != repository
        || url.origin() != origin.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || segments.len() != 3
        || segments[0] != "git"
        || !hex_key(segments[1])
        || !name(segments[2])
    {
        return Err("Not a repository in this community".into());
    }
    Ok(url)
}

/// NIP-98 for one repository URL. The relay accepts it for 60 seconds on every Git route
/// of that repository, so one clone reuses it. Returns the token after `Authorization: Nostr `;
/// never a general signing capability.
#[tauri::command]
pub(crate) async fn relay_git_authorization(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    repository: String,
) -> Result<String> {
    let url = git_repository(&community, &repository)?;
    let auth = host
        .sign(EventTemplate {
            kind: 27235,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "System clock is unavailable")?
                .as_secs(),
            content: String::new(),
            tags: vec![
                vec!["u".into(), url.to_string()],
                vec!["method".into(), "GET".into()],
            ],
        })
        .await?;
    Ok(STANDARD.encode(
        serde_json::to_vec(&auth).map_err(|_| "Could not encode repository authorization")?,
    ))
}

#[tauri::command]
pub(crate) async fn relay_sign(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    event: EventTemplate,
) -> Result<serde_json::Value> {
    validate_event(&community, &event)?;
    if event.kind == 9007 && !channel_creation_supported(&community).await? {
        return Err("Channel creation is unavailable".into());
    }
    if event.kind == 30078 {
        return Err("Channel recipe or Canvas rejected".into());
    }
    let coordinate_delete = if event.kind == 5 {
        event
            .tags
            .iter()
            .find(|tag| tag[0] == "a")
            .map(|tag| tag[1].clone())
    } else {
        None
    };
    let signed = if catalog::is_catalog(event.kind) {
        host.sign_bounded(event, catalog::MAX_EVENT_BYTES).await?
    } else {
        host.sign(event).await?
    };
    if coordinate_delete
        .is_some_and(|coordinate| coordinate.split(':').nth(1) != signed["pubkey"].as_str())
    {
        return Err("Only the author can delete this event".into());
    }
    Ok(signed)
}

#[tauri::command]
pub(crate) async fn relay_decode_sidebar(
    host: tauri::State<'_, IdentityHost>,
    events: Vec<serde_json::Value>,
) -> Result<serde_json::Value> {
    host.decode_sidebar(events).await
}

#[tauri::command]
pub(crate) async fn relay_sign_sidebar(
    host: tauri::State<'_, IdentityHost>,
    coordinate: String,
    payload: serde_json::Value,
    created_at: u64,
) -> Result<serde_json::Value> {
    host.sign_sidebar(coordinate, payload, created_at).await
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReadStateIntent {
    slot: String,
    created_at: u64,
    blob: serde_json::Value,
}

#[tauri::command]
pub(crate) async fn relay_decode_read_state(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    events: Vec<serde_json::Value>,
) -> Result<serde_json::Value> {
    origin(&community)?;
    host.decode_read_state(events).await
}

#[tauri::command]
pub(crate) async fn relay_sign_read_state(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    intent: ReadStateIntent,
) -> Result<serde_json::Value> {
    origin(&community)?;
    host.sign_read_state(intent.slot, intent.created_at, intent.blob)
        .await
}

/// Publication cannot be routed through the general event writer: it must verify
/// the exact own, encrypted read-state coordinate before forwarding signed bytes.
#[tauri::command]
pub(crate) async fn relay_publish_read_state(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    event: serde_json::Value,
) -> Result<RelayResponse> {
    let url = request_url(&community, "/events", "POST")?;
    let body = serde_json::to_string(&event).map_err(|_| "Invalid read-state event")?;
    if body.len() > 64 * 1024 {
        return Err("Invalid read-state event".into());
    }
    host.decode_read_state(vec![event]).await?;
    send(host.inner(), url, "POST", Some(body), true, MAX_RESPONSE).await
}

async fn verify_owned_event(host: &IdentityHost, event: &serde_json::Value) -> Result<()> {
    if event.get("pubkey").and_then(serde_json::Value::as_str)
        != Some(host.viewer().await?.as_str())
    {
        return Err("Invalid outgoing signature".into());
    }
    verify_signature(event)
}

async fn admit_app_data(
    host: &IdentityHost,
    event: &serde_json::Value,
    community: &str,
) -> Result<()> {
    verify_owned_event(host, event).await?;
    // Sidebar preferences and channel recipes share kind 30078; the `d` coordinate selects the contract.
    let sidebar = event["tags"].as_array().is_some_and(|tags| {
        tags.iter().any(|tag| {
            tag[0] == "d"
                && tag[1]
                    .as_str()
                    .is_some_and(crate::identity::sidebar_coordinate)
        })
    });
    if sidebar {
        return host.admit_sidebar(event.clone()).await;
    }
    kit::validate_ciphertext(host, event, community).await?;
    Ok(())
}

async fn channel_creation_supported(community: &str) -> Result<bool> {
    let url = request_url(community, "/", "GET")?;
    let mut response = client()?
        .get(url)
        .header("Accept", "application/nostr+json")
        .send()
        .await
        .map_err(|_| "Community discovery failed")?;
    if !response.status().is_success() {
        return Err("Community discovery failed".into());
    }
    let body = read_bounded(
        &mut response,
        MAX_BODY,
        "Community discovery failed",
        "Invalid community information",
    )
    .await?;
    let info: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| "Invalid community information")?;
    Ok(info
        .get("self")
        .and_then(serde_json::Value::as_str)
        .is_some_and(hex_key)
        && info
            .get("supported_nips")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|nips| nips.contains(&serde_json::Value::from(29))))
}

#[tauri::command]
pub(crate) async fn relay_http(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    path: String,
    method: String,
    body: Option<String>,
) -> Result<RelayResponse> {
    let url = request_url(&community, &path, &method)?;
    if body.as_ref().is_some_and(|b| b.len() > MAX_BODY)
        || (method == "GET" && body.is_some())
        || (method == "POST" && body.is_none())
    {
        return Err("Invalid relay request body".into());
    }
    if path == "/events" {
        let event: serde_json::Value =
            serde_json::from_str(body.as_deref().ok_or("Invalid relay request body")?)
                .map_err(|_| "Invalid relay request body")?;
        let kind = event.get("kind").and_then(serde_json::Value::as_u64);
        if matches!(kind, Some(41010)) {
            return Err("Choose between one and eight other people.".into());
        }
        if kind == Some(30078) {
            admit_app_data(host.inner(), &event, &community).await?;
        }
        if kind == Some(30300) {
            reminders::admit_reminder(host.inner(), &event).await?;
        }
        if kind == Some(9007) {
            verify_owned_event(host.inner(), &event).await?;
            let template = template(
                &event,
                "Agent enrollment or channel operation unavailable or invalid",
            )?;
            if !channel_policy::creation(&template) {
                return Err("Agent enrollment or channel operation unavailable or invalid".into());
            }
        }
        if matches!(kind, Some(9002 | 9008 | 9021 | 9022 | 41012 | 9035 | 9036)) {
            return Err("Invalid outgoing signature".into());
        }
    }
    send(
        host.inner(),
        url,
        &method,
        body,
        method == "POST",
        MAX_RESPONSE,
    )
    .await
}

/// Allocate only host-owned storage; callers never supply filesystem paths.
#[tauri::command]
pub(crate) async fn relay_upload_begin<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: String,
    size: usize,
) -> Result<()> {
    use tauri::Manager as _;
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<Spools>()
            .begin(&app.state::<Uploads>(), &id, size)
    })
    .await
    .map_err(|_| "Upload receiving could not complete")?
}

/// One acknowledged IPC chunk at a time. Bound and validate in the host too.
#[tauri::command]
pub(crate) async fn relay_upload_chunk<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    request: tauri::ipc::Request<'_>,
) -> Result<()> {
    use tauri::Manager as _;
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let id = upload_id(header("x-buzz-upload-id"))?.to_owned();
    let chunk: Result<_> = (|| {
        let offset = header("x-buzz-upload-offset")
            .and_then(|v| v.parse::<usize>().ok())
            .ok_or("Invalid upload chunk")?;
        let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
            return Err("Upload body must be raw bytes".into());
        };
        if body.is_empty() || body.len() > UPLOAD_CHUNK {
            return Err("Invalid upload chunk".into());
        }
        Ok((offset, body.clone()))
    })();
    tauri::async_runtime::spawn_blocking(move || {
        let spools = app.state::<Spools>();
        let uploads = app.state::<Uploads>();
        let result = chunk.and_then(|(offset, bytes)| spools.append(&id, offset, &bytes));
        if result.is_err() {
            spools.cancel(&uploads, &id);
        }
        result
    })
    .await
    .map_err(|_| "Upload receiving could not complete")?
}

/// Finalize the received spool, convert on disk when requested, and stream it.
#[tauri::command]
pub(crate) async fn relay_upload<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    host: tauri::State<'_, IdentityHost>,
    uploads: tauri::State<'_, Uploads>,
    spools: tauri::State<'_, Spools>,
    request: tauri::ipc::Request<'_>,
) -> Result<RelayResponse> {
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let id = upload_id(header("x-buzz-upload-id"))?;
    let result = async {
        let progress = header("x-buzz-upload-progress")
            .map(|value| {
                value
                    .parse::<tauri::ipc::JavaScriptChannelId>()
                    .map(|channel| channel.channel_on(webview))
                    .map_err(|_| "Invalid upload progress channel".to_string())
            })
            .transpose()?;
        let url = origin(header("x-buzz-community").unwrap_or_default())?
            .join("/upload")
            .map_err(|_| "Invalid relay path")?;
        let (spool, mut cancelled) = spools.take(id)?;
        process_spool(
            host.inner(),
            url,
            spool,
            header("x-buzz-content-type"),
            header("x-buzz-preparation"),
            move |sent| {
                if let Some(channel) = &progress {
                    let _ = channel.send(sent);
                }
            },
            &mut cancelled,
        )
        .await
    }
    .await;
    spools.discard(id);
    uploads.finish(id);
    result
}

#[tauri::command]
pub(crate) fn relay_upload_cancel(
    uploads: tauri::State<'_, Uploads>,
    spools: tauri::State<'_, Spools>,
    id: String,
) -> Result<()> {
    let id = upload_id(Some(&id))?;
    // Never free a processing slot until the running future has stopped.
    spools.cancel(&uploads, id);
    Ok(())
}

/// `buzz-media://` serves relay `GET /media/*` to `<img>`, `<video>` and
/// `<audio>`, which cannot send the required Blossom `Authorization` header.
/// Each request gets a fresh `get` token and forwards only the player's
/// `Range`, so playback streams and seeks. Which URLs use it is decided in
/// shared TypeScript (`mediaUrl`); this checks only the relay-blob URL shape,
/// not saved-community membership (Rust keeps no community list).
pub(crate) fn media_protocol<R: tauri::Runtime>(
    ctx: tauri::UriSchemeContext<'_, R>,
    request: tauri::http::Request<Vec<u8>>,
    responder: tauri::UriSchemeResponder,
) {
    use tauri::Manager as _;
    let host = ctx.app_handle().state::<IdentityHost>().inner().clone();
    tauri::async_runtime::spawn(async move {
        responder.respond(media::respond(&host, &request).await);
    });
}

/// The host owns authenticated media access and the OS clipboard. Never hand
/// bearer tokens or privileged fetch capability to the webview.
#[tauri::command]
pub(crate) async fn media_copy_image<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    host: tauri::State<'_, IdentityHost>,
    source: String,
) -> Result<()> {
    let pixels = media::copy_image(host.inner(), &source).await?;
    crate::image_clipboard::write_image(&app, pixels.0, pixels.1, pixels.2).await
}

/// Persist an authenticated bounded response without replacing an existing file.
/// The host, not the webview, owns the save path and collision policy.
#[tauri::command]
pub(crate) async fn media_download<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    host: tauri::State<'_, IdentityHost>,
    source: String,
    name: String,
) -> Result<()> {
    use tauri::Manager as _;
    media::download(host.inner(), &source, &name, || {
        app.path().download_dir().ok()
    })
    .await?;
    Ok(())
}

/// Where media elements load relay media; `None` if the listener could not
/// start, and they show relay video and audio as unavailable. They never
/// fall back to `buzz-media`, whose reads a closed player cannot cancel.
#[tauri::command]
pub(crate) fn media_stream_base<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Option<String> {
    use tauri::Manager as _;
    app.try_state::<MediaStream>()
        .map(|stream| stream.base().to_owned())
}

/// Packaged repository reads with the development broker's `project-git`
/// contract; see `buzz_desktop::relay::project_git`.
#[tauri::command]
pub(crate) async fn relay_project_git(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    id: String,
    read: project_git::GitRead,
) -> Result<RelayResponse> {
    project_git::read(host.inner(), &community, &id, read).await
}

#[tauri::command]
pub(crate) fn relay_project_git_cancel(id: String) -> Result<()> {
    project_git::cancel(&id)
}

#[cfg(test)]
mod tests;
