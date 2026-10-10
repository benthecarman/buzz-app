//! Relay `<video>` and `<audio>` over loopback HTTP, on every desktop platform.
//!
//! WebKitGTK's GStreamer player loads only http(s), blob and file URLs
//! (`isProtocolAllowed`, `webKitWebSrcGetProtocols`), so `buzz-media` cannot
//! play on Linux; over HTTP, a player's closed connection also tells native
//! code it is done with a spool. This listener serves the same relay blobs
//! `buzz-media` does, through the same block cache, spool and signed GETs, at
//! `http://127.0.0.1:<port>/<token>/<percent-encoded relay URL>`.
//!
//! - It binds only `127.0.0.1`. A random 256-bit token per process, given
//!   only to the main window over IPC, authorizes every request; `Host` must
//!   name the bound address and `Origin`, when sent, the app. Nothing is
//!   logged.
//! - At most `CONNECTIONS` are accepted at once, before any per-connection
//!   work, and a request head must arrive within `HEAD_TIMEOUT`.
//! - Each connection answers one request and closes. Bodies are written a
//!   block at a time from `media_blocks`, so a full or open-ended response
//!   holds one block, never the blob. A response that cannot be finished is
//!   cut short of its `Content-Length`, which the player sees as a failure.
//! - A connection holds the URL's spool, if it has one, until it closes; a
//!   read waiting for spooled bytes holds it too. When the client closes, its
//!   response, its hold and the read it waits on are dropped. Another
//!   player's connection to the same URL keeps its own hold, and an unheld
//!   spool expires after `media_spool`'s idle time.

use super::{blocks as media_blocks, media_url, spool as media_spool, WHOLE_VIDEO_MAX};
use crate::event::Signer;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::{net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;
use url::Url;

/// Open connections, each holding at most one block of a response. A paused
/// player may keep its connection, so this is well above the players one
/// window shows; a connection over it waits to be accepted.
const CONNECTIONS: usize = 32;
const HEAD_LIMIT: usize = 8192;
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Page origins a media request may name: the app on macOS and Linux, on
/// Windows, and the development server in debug builds.
fn app_origin(origin: &str) -> bool {
    matches!(origin, "tauri://localhost" | "http://tauri.localhost")
        || (cfg!(debug_assertions) && origin == "http://localhost:1430")
}

/// The running listener; dropping it stops the listener and its connections.
pub struct MediaStream {
    base: String,
    stop: CancellationToken,
}

impl Drop for MediaStream {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl MediaStream {
    /// Binds the listener and starts serving on `runtime`. Binding is
    /// synchronous, so the address is known before any page loads.
    pub fn start<S: Signer>(host: S, runtime: &tokio::runtime::Handle) -> std::io::Result<Self> {
        Self::serve(host, media_url, runtime)
    }

    /// `target` validates the decoded relay URL; it is `media_url` outside tests.
    fn serve<S: Signer>(
        host: S,
        target: fn(&str) -> Option<Url>,
        runtime: &tokio::runtime::Handle,
    ) -> std::io::Result<Self> {
        // Bound synchronously, so the address is known before any page loads.
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let authority = listener.local_addr()?.to_string();
        let mut secret = [0; 32];
        getrandom::fill(&mut secret).map_err(std::io::Error::other)?;
        let token = URL_SAFE_NO_PAD.encode(secret);
        let base = format!("http://{authority}/{token}/");
        let stop = CancellationToken::new();
        let server = Arc::new(Server {
            host,
            authority,
            token,
            target,
        });
        runtime.spawn(accept(listener, server, stop.clone()));
        Ok(Self { base, stop })
    }

    /// `http://127.0.0.1:<port>/<token>/`, to which the page appends the
    /// encoded relay URL of a `buzz-media` source.
    pub fn base(&self) -> &str {
        &self.base
    }
}

struct Server<S> {
    host: S,
    authority: String,
    token: String,
    target: fn(&str) -> Option<Url>,
}

async fn accept<S: Signer>(
    listener: std::net::TcpListener,
    server: Arc<Server<S>>,
    stop: CancellationToken,
) {
    let Ok(listener) = TcpListener::from_std(listener) else {
        return;
    };
    let connections = Arc::new(Semaphore::new(CONNECTIONS));
    loop {
        // A connection is accepted only with a free slot; until then the
        // kernel's bounded backlog holds it, and nothing is spawned for it.
        let accepted = tokio::select! {
            accepted = async {
                let permit = connections.clone().acquire_owned().await.ok()?;
                Some((permit, listener.accept().await))
            } => accepted,
            () = stop.cancelled() => return,
        };
        let Some((permit, accepted)) = accepted else {
            return;
        };
        let Ok((socket, _)) = accepted else {
            // Such as running out of descriptors; let connections finish.
            drop(permit);
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let (server, stop) = (server.clone(), stop.clone());
        tokio::spawn(async move {
            tokio::select! {
                () = connection(&server, socket) => {}
                () = stop.cancelled() => {}
            }
            drop(permit);
        });
    }
}

async fn connection<S: Signer>(server: &Server<S>, socket: TcpStream) {
    let (mut reader, mut writer) = socket.into_split();
    let Ok(Some(head)) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut reader)).await else {
        return;
    };
    let reply = async {
        match parse(server, &head) {
            Ok(request) => respond(server, &mut writer, request).await,
            Err(status) => write_status(&mut writer, status, None).await,
        }
    };
    // Whatever the reply waits on is dropped once the client closes.
    tokio::select! {
        _ = reply => {}
        () = closed(&mut reader) => {}
    }
}

/// The request head, without its terminating blank line, if the head and
/// that line fit in `HEAD_LIMIT` bytes.
async fn read_head(reader: &mut OwnedReadHalf) -> Option<Vec<u8>> {
    let mut head = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        if let Some(end) = head.windows(4).position(|part| part == b"\r\n\r\n") {
            if end + 4 > HEAD_LIMIT {
                return None;
            }
            head.truncate(end);
            return Some(head);
        }
        if head.len() >= HEAD_LIMIT {
            return None;
        }
        let count = reader.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..count]);
    }
}

/// Resolves when the client closes its connection; it sends nothing else.
async fn closed(reader: &mut OwnedReadHalf) {
    let mut chunk = [0; 1024];
    let mut extra = 0;
    while extra <= HEAD_LIMIT {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(count) => extra += count,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Range {
    From(u64),
    Between(u64, u64),
    Suffix(u64),
}

#[derive(Debug)]
struct Request {
    head: bool,
    url: Url,
    range: Option<Range>,
}

fn parse<S>(server: &Server<S>, head: &[u8]) -> std::result::Result<Request, u16> {
    let text = std::str::from_utf8(head).map_err(|_| 400u16)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some("HTTP/1.1"), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Err(400);
    };
    let head = match method {
        "GET" => false,
        "HEAD" => true,
        _ => return Err(405),
    };
    let (mut host, mut range) = (None, None);
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        if name.is_empty() || name.ends_with([' ', '\t']) || line.starts_with([' ', '\t']) {
            return Err(400);
        }
        let value = value.trim_matches([' ', '\t']);
        match name.to_ascii_lowercase().as_str() {
            "host" if host.replace(value).is_some() => return Err(400),
            "origin" if !app_origin(value) => return Err(403),
            // A request body has no meaning here and would desynchronize framing.
            "content-length" if value != "0" => return Err(400),
            "transfer-encoding" => return Err(400),
            "range" => range = Some(value),
            _ => {}
        }
    }
    if host != Some(server.authority.as_str()) {
        return Err(403);
    }
    let rest = target.strip_prefix('/').ok_or(400u16)?;
    let (token, encoded) = rest.split_once('/').ok_or(404u16)?;
    if !same(token.as_bytes(), server.token.as_bytes()) {
        return Err(404);
    }
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .map_err(|_| 400u16)?;
    let url = (server.target)(&decoded).ok_or(403u16)?;
    Ok(Request {
        head,
        url,
        range: range.and_then(byte_range),
    })
}

/// Compares the token without exposing how much of it matched.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// One byte range; anything else is ignored and the whole blob sent, as
/// RFC 9110 §14.2 allows.
fn byte_range(value: &str) -> Option<Range> {
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    match (start, end) {
        ("", suffix) => Some(Range::Suffix(number(suffix)?)),
        (start, "") => Some(Range::From(number(start)?)),
        (start, end) => {
            let (start, end) = (number(start)?, number(end)?);
            (start <= end).then_some(Range::Between(start, end))
        }
    }
}

/// One block-aligned read: its body and the blob's total length, or the
/// buffered 200 an upstream sent instead for something other than video.
enum Part {
    Range(Vec<u8>, u64),
    Whole(http::Response<Vec<u8>>),
}

async fn read<S: Signer>(
    server: &Server<S>,
    url: &Url,
    start: u64,
    end: u64,
) -> std::result::Result<(Part, http::HeaderMap), u16> {
    let end = end.min(start.saturating_add(media_blocks::BLOCK - 1));
    let response = media_blocks::read(&server.host, url, start, end, WHOLE_VIDEO_MAX).await?;
    if response.status() != 206 {
        let headers = response.headers().clone();
        return Ok((Part::Whole(response), headers));
    }
    let (parts, body) = response.into_parts();
    let total = parts
        .headers
        .get("content-range")
        .and_then(|value| media_blocks::content_range(value.to_str().ok()?))
        .filter(|&(first, last, _)| first == start && last - first + 1 == body.len() as u64)
        .ok_or(502u16)?
        .2;
    Ok((Part::Range(body, total), parts.headers))
}

async fn respond<S: Signer>(
    server: &Server<S>,
    writer: &mut OwnedWriteHalf,
    request: Request,
) -> std::io::Result<()> {
    let url = &request.url;
    let opening = match request.range {
        Some(Range::From(start) | Range::Between(start, _)) => start,
        // A suffix needs the length first, which any read reports.
        None | Some(Range::Suffix(_)) => 0,
    };
    let last = match request.range {
        Some(Range::Between(_, end)) => end,
        _ => u64::MAX,
    };
    let (part, headers) = match read(server, url, opening, last).await {
        Ok(read) => read,
        Err(status) => return write_status(writer, status, None).await,
    };
    // Until this connection closes, its player may still read the spool.
    let _hold = media_spool::hold(url);
    let (mut body, total) = match part {
        Part::Range(body, total) => (Some(body), total),
        Part::Whole(response) => return write_whole(writer, response, request.head).await,
    };
    let (start, end) = match request.range {
        None | Some(Range::From(_)) => (opening, total - 1),
        Some(Range::Between(start, end)) => (start, end.min(total - 1)),
        Some(Range::Suffix(0)) => return write_status(writer, 416, Some(total)).await,
        Some(Range::Suffix(length)) => (total - length.min(total), total - 1),
    };
    if start != opening {
        body = None;
    }
    let status = if request.range.is_some() { 206 } else { 200 };
    let mut head = status_line(status);
    for (name, value) in &headers {
        if !matches!(name.as_str(), "content-range" | "content-length") {
            head.push_str(&format!(
                "{name}: {}\r\n",
                value.to_str().unwrap_or_default()
            ));
        }
    }
    if status == 206 {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        end - start + 1
    ));
    writer.write_all(head.as_bytes()).await?;
    if request.head {
        return Ok(());
    }
    let mut at = start;
    while at <= end {
        let bytes = match body.take() {
            Some(bytes) => bytes,
            None => match read(server, url, at, end).await {
                Ok((Part::Range(bytes, length), _)) if length == total => bytes,
                // Ending short of `Content-Length` tells the player it failed.
                _ => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            },
        };
        if bytes.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        writer.write_all(&bytes).await?;
        at += bytes.len() as u64;
    }
    Ok(())
}

/// The bounded 200 `media_blocks` passes through for a non-video upstream
/// that ignored `Range`.
async fn write_whole(
    writer: &mut OwnedWriteHalf,
    response: http::Response<Vec<u8>>,
    head_only: bool,
) -> std::io::Result<()> {
    let mut head = status_line(response.status().as_u16());
    for (name, value) in response.headers() {
        if !matches!(name.as_str(), "content-range" | "content-length") {
            head.push_str(&format!(
                "{name}: {}\r\n",
                value.to_str().unwrap_or_default()
            ));
        }
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body().len()
    ));
    writer.write_all(head.as_bytes()).await?;
    if !head_only {
        writer.write_all(response.body()).await?;
    }
    Ok(())
}

async fn write_status(
    writer: &mut OwnedWriteHalf,
    status: u16,
    total: Option<u64>,
) -> std::io::Result<()> {
    let mut head = status_line(status);
    if let Some(total) = total {
        head.push_str(&format!("Content-Range: bytes */{total}\r\n"));
    }
    head.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
    writer.write_all(head.as_bytes()).await
}

fn status_line(status: u16) -> String {
    let reason = http::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or_default();
    format!("HTTP/1.1 {status} {reason}\r\n")
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
