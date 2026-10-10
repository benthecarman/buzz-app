//! Authenticated HTTP to a Buzz relay: NIP-98 requests and Blossom tokens.
//! Credentials stay with the [`Signer`]; redirects never carry auth.
pub mod agent;
pub mod catalog;
pub mod channel_writes;
pub mod events;
pub mod kit;
pub mod project_git;
pub mod reminders;

use crate::event::{EventTemplate, Signer};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::OnceLock, time::Duration};
use url::Url;

type Result<T> = std::result::Result<T, String>;
/// Largest request body sent to a relay.
pub const MAX_BODY: usize = 1024 * 1024;
/// Largest relay response read into memory by default.
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;

/// Whether `value` is a lowercase hex 32-byte key or event ID.
pub fn hex_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A relay origin: HTTPS, with a host and no credentials, path, query or fragment.
pub fn origin(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| "Invalid relay origin")?;
    if value.len() > 2048
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Relay access requires an HTTPS origin without credentials or a path".into());
    }
    Ok(url)
}

/// A relay reply as the webview sees it: status, the headers it may read,
/// and a UTF-8 body.
#[derive(Debug, Serialize)]
pub struct RelayResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// The shared relay client. It never follows redirects, so a redirect cannot
/// carry an `Authorization` header to another origin.
pub fn client() -> Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<std::result::Result<reqwest::Client, reqwest::Error>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
        })
        .as_ref()
        .map_err(|_| "Relay network client is unavailable".into())
}

/// Sends one relay request. With `authenticated`, signs a fresh NIP-98 event
/// over the URL, method and exact body; otherwise asks for relay information.
pub async fn send<S: Signer>(
    host: &S,
    url: Url,
    method: &str,
    body: Option<String>,
    authenticated: bool,
    response_limit: usize,
) -> Result<RelayResponse> {
    let mut request = client()?.request(
        reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| "Invalid relay method")?,
        url.clone(),
    );
    if authenticated {
        let payload = body
            .as_ref()
            .map(|body| format!("{:x}", Sha256::digest(body.as_bytes())));
        let auth = host
            .sign(EventTemplate {
                kind: 27235,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "System clock is unavailable")?
                    .as_secs(),
                content: String::new(),
                tags: [
                    vec![
                        vec!["u".into(), url.to_string()],
                        vec!["method".into(), method.into()],
                    ],
                    payload
                        .map(|hash| vec![vec!["payload".into(), hash]])
                        .unwrap_or_default(),
                    vec![vec!["nonce".into(), uuid::Uuid::new_v4().to_string()]],
                ]
                .concat(),
            })
            .await?;
        request = request
            .header(
                "Authorization",
                format!(
                    "Nostr {}",
                    STANDARD.encode(
                        serde_json::to_vec(&auth)
                            .map_err(|_| "Could not encode relay authentication")?
                    )
                ),
            )
            .header("Content-Type", "application/json");
        if let Some(body) = body {
            request = request.body(body);
        }
    } else {
        request = request.header("Accept", "application/nostr+json");
    }
    // Never replay a write after a transport error: it may already have reached the relay.
    let response = request
        .send()
        .await
        .map_err(|_| "Relay request could not be confirmed")?;
    read_response(response, response_limit).await
}

/// Reads a bounded UTF-8 response with its readable headers.
pub async fn read_response(mut response: reqwest::Response, limit: usize) -> Result<RelayResponse> {
    let status = response.status().as_u16();
    let mut headers = BTreeMap::new();
    for name in ["content-type", "retry-after", "server-timing"] {
        if let Some(value) = response.headers().get(name).and_then(|v| v.to_str().ok()) {
            headers.insert(name.into(), value.into());
        }
    }

    let bytes = read_bounded(
        &mut response,
        limit,
        "Relay response was interrupted",
        "Relay response is too large",
    )
    .await?;

    let body = String::from_utf8(bytes).map_err(|_| "Relay response is not UTF-8")?;
    Ok(RelayResponse {
        status,
        headers,
        body,
    })
}

/// Reads a body of at most `limit` bytes, with the given error messages.
pub async fn read_bounded(
    response: &mut reqwest::Response,
    limit: usize,
    interrupted: &str,
    oversized: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(oversized.into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| interrupted)? {
        if chunk.len() > limit - bytes.len() {
            return Err(oversized.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// A Blossom `Authorization` value. JS never signs these: `relay_sign` rejects
/// kind 24242, so a script cannot mint a reusable read or a `delete` proof.
pub async fn blossom_auth<S: Signer>(
    host: &S,
    url: &Url,
    verb: &str,
    content: &str,
    mut tags: Vec<Vec<String>>,
) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "System clock is unavailable")?
        .as_secs();
    // `server` matches the dev broker's `URL.host` (host plus any explicit port).
    let server = &url[url::Position::BeforeHost..url::Position::AfterPort];
    tags.extend([
        vec!["t".into(), verb.into()],
        vec!["server".into(), server.into()],
        // The relay's strict NIP-FI window is 60 seconds.
        vec!["expiration".into(), (now + 60).to_string()],
    ]);
    let event = host
        .sign(EventTemplate {
            kind: 24242,
            created_at: now,
            content: content.into(),
            tags,
        })
        .await?;
    Ok(format!(
        "Nostr {}",
        STANDARD.encode(
            serde_json::to_vec(&event).map_err(|_| "Could not encode media authentication")?
        )
    ))
}

#[cfg(test)]
mod tests;
