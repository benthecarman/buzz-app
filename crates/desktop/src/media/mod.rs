//! Relay media: Blossom-authenticated reads of relay blobs, for players,
//! downloads, the clipboard and snapshots, and media conversion for uploads.
//!
//! Players cannot send the required `Authorization` header, so a host serves
//! relay media to them, through a custom scheme ([`respond`]) or a loopback
//! listener ([`stream::MediaStream`]). Each request gets a fresh `get` token.
pub(crate) mod blocks;
pub(crate) mod preparation;
pub(crate) mod spool;
pub mod stream;

use crate::event::Signer;
use crate::relay::{blossom_auth, client, origin};
use std::{path::PathBuf, time::Duration};
use url::Url;

type Result<T> = std::result::Result<T, String>;

/// Largest whole-file media response buffered in memory: the relay's document
/// limit, which also covers images. Video is read by `Range`; an origin that
/// ignores it is spooled to disk up to `WHOLE_VIDEO_MAX` instead.
pub(crate) const MAX_MEDIA: usize = 100 * 1024 * 1024;
/// Largest video an origin that ignores `Range` may send whole: 500 MiB, the
/// development broker's video limit. A video of exactly this size plays; a
/// larger `Content-Length` gets 413 before any download, and a response
/// without one is stopped and rejected with 413 once it passes this size.
/// Edit this value to change the limit; it is compiled into the binary.
pub(crate) const WHOLE_VIDEO_MAX: u64 = 500 * 1024 * 1024;
/// Open-ended ranges are shortened so playback starts after one small chunk;
/// the media element requests the next range itself.
const MEDIA_CHUNK: u64 = 4 * 1024 * 1024;
/// Only `blocks` sends `Range`, always for one block, so a 206 is never
/// buffered past it.
const MAX_MEDIA_RANGE: usize = blocks::BLOCK as usize;

/// Answers one `buzz-media://localhost/<percent-encoded relay media URL>`
/// request: a ranged read through the shared block cache, or the whole blob.
/// Only relay-blob URLs are reached; this checks only the URL shape, not
/// saved-community membership. Errors are a bare status.
pub async fn respond<S: Signer>(
    host: &S,
    request: &http::Request<Vec<u8>>,
) -> http::Response<Vec<u8>> {
    let response = match media_request(request) {
        Ok((url, Some((start, end)))) => {
            blocks::read(host, &url, start, end, WHOLE_VIDEO_MAX).await
        }
        Ok((url, None)) => fetch_media(host, url, None).await,
        Err(status) => Err(status),
    };
    response.unwrap_or_else(|status| {
        http::Response::builder()
            .status(status)
            .body(Vec::new())
            .expect("static response")
    })
}

/// The decoded RGBA pixels of a relay image, for the OS clipboard:
/// `(width, height, rgba)`. `source` is a `buzz-media` URL.
pub async fn copy_image<S: Signer>(host: &S, source: &str) -> Result<(usize, usize, Vec<u8>)> {
    let url = download_target(source).ok_or("Invalid media URL")?;
    let response = fetch_media(host, url, None)
        .await
        .map_err(|_| "Could not fetch image")?;
    if !response
        .headers()
        .get("Content-Type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.starts_with("image/") && kind != "image/svg+xml")
    {
        return Err("Could not decode image".into());
    }
    tokio::task::spawn_blocking(move || clipboard_pixels(response.body()))
        .await
        .map_err(|_| "Could not decode image".to_owned())?
}

/// Saves a relay blob into the directory `downloads` returns, never replacing
/// an existing file. The host, not the caller, owns the collision policy;
/// `name` is untrusted and falls back to the blob's basename.
pub async fn download<S: Signer>(
    host: &S,
    source: &str,
    name: &str,
    downloads: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf> {
    let url = download_target(source).ok_or("Invalid media URL")?;
    let filename = download_name(name, &url).to_owned();
    let directory = downloads().ok_or("Downloads unavailable")?;
    let response = fetch_media(host, url, None)
        .await
        .map_err(|_| "Media download failed")?;
    let body = response.into_body();
    tokio::task::spawn_blocking(move || save_download(&directory, &filename, &body))
        .await
        .map_err(|_| "Media download interrupted".to_owned())?
}

/// Reads a relay snapshot of at most `max_bytes`. The caller may tighten,
/// but never raise, the 16 MiB snapshot cap.
pub async fn snapshot<S: Signer>(host: &S, source: &str, max_bytes: usize) -> Result<Vec<u8>> {
    if max_bytes == 0 || max_bytes > 16 * 1024 * 1024 {
        return Err("Invalid snapshot size limit".into());
    }
    let url = download_target(source).ok_or("Invalid media URL")?;
    fetch_media_bounded(host, url, None, max_bytes)
        .await
        .map(http::Response::into_body)
        .map_err(|status| format!("Snapshot media read failed ({status})"))
}

/// Decode only the custom scheme URLs emitted by convertFileSrc. The media URL
/// is independently checked before any authenticated request is made.
fn download_target(source: &str) -> Option<Url> {
    let url = Url::parse(source).ok()?;
    let host_ok = match url.scheme() {
        "buzz-media" => url.host_str() == Some("localhost"),
        "http" => url.host_str() == Some("buzz-media.localhost"),
        _ => false,
    };
    if !host_ok
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let target = percent_encoding::percent_decode_str(url.path().strip_prefix('/')?)
        .decode_utf8()
        .ok()?;
    media_url(&target)
}

/// Never interpret untrusted attachment names as paths. A missing or invalid name
/// falls back to the validated blob's basename.
fn download_name<'a>(name: &'a str, url: &'a Url) -> &'a str {
    let stem = name.split('.').next().unwrap_or("");
    let reserved = stem.trim_end_matches(' ').to_ascii_uppercase();
    if !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !matches!(reserved.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !matches!(
            reserved.as_str(),
            "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
        && !name.chars().any(|c| {
            c.is_control()
                || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                    | '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}')
        })
    {
        name
    } else {
        url.path().rsplit('/').next().unwrap_or("media")
    }
}

/// Fail closed if the OS cannot mark the file as untrusted internet content.
#[cfg(target_os = "macos")]
fn mark_download(file: &std::fs::File, _path: &std::path::Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_secs();
    let value = format!("0081;{timestamp:x};Buzz;");
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            c"com.apple.quarantine".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn mark_download(_file: &std::fs::File, path: &std::path::Path) -> std::io::Result<()> {
    // Alternate data streams are attached to the same NTFS file, not a sibling.
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    std::fs::write(stream, b"[ZoneTransfer]\r\nZoneId=3\r\n")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn mark_download(_file: &std::fs::File, _path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

fn save_download(
    directory: &std::path::Path,
    filename: &str,
    body: &[u8],
) -> Result<std::path::PathBuf> {
    use std::io::Write as _;
    let (stem, ext) = filename
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or((filename, ""), |(stem, ext)| (stem, ext));
    for index in 0..1000 {
        let candidate = if index == 0 {
            filename.to_owned()
        } else if ext.is_empty() {
            format!("{stem} ({index})")
        } else {
            format!("{stem} ({index}).{ext}")
        };
        let path = directory.join(candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(error) = mark_download(&file, &path)
                    .and_then(|_| file.write_all(body))
                    .and_then(|_| file.sync_all())
                {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(format!("Could not save media: {error}"));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Could not save media: {error}")),
        }
    }
    Err("Too many files with this name".into())
}

/// `image` bounds the WebP canvas but its VP8 decoder allocates Y/U/V planes
/// from the embedded frame header before comparing that frame to the canvas.
/// Inspect every embedded lossy frame (including ANMF subchunks) before decode.
fn check_webp_vp8_frames(bytes: &[u8], max_bytes: usize) -> Result<()> {
    fn chunks(
        mut bytes: &[u8],
        max_bytes: usize,
        animated_frame: bool,
        mut expected: Option<(usize, usize)>,
    ) -> Result<()> {
        let mut alpha_successor = false;
        let mut frame_chunk_seen = false;
        while !bytes.is_empty() {
            let header = bytes.get(..8).ok_or("Could not decode image")?;
            let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            let end = 8usize.checked_add(size).ok_or("Could not decode image")?;
            let next = end.checked_add(size & 1).ok_or("Could not decode image")?;
            let payload = bytes.get(8..end).ok_or("Could not decode image")?;
            if bytes.get(..next).is_none() {
                return Err("Could not decode image".into());
            }
            if animated_frame && alpha_successor && &header[..4] != b"VP8 " {
                return Err("Could not decode image".into());
            }
            if animated_frame && frame_chunk_seen {
                return Err("Could not decode image".into());
            }
            if animated_frame
                && !alpha_successor
                && &header[..4] != b"ALPH"
                && &header[..4] != b"VP8 "
                && &header[..4] != b"VP8L"
            {
                return Err("Could not decode image".into());
            }
            match &header[..4] {
                b"VP8 " => {
                    let frame = payload.get(..10).ok_or("Could not decode image")?;
                    if frame[0] & 1 != 0 || &frame[3..6] != b"\x9d\x01\x2a" {
                        return Err("Could not decode image".into());
                    }
                    let width = usize::from(u16::from_le_bytes([frame[6], frame[7]]) & 0x3fff);
                    let height = usize::from(u16::from_le_bytes([frame[8], frame[9]]) & 0x3fff);
                    // The locked VP8 decoder allocates three padded Y/U/V planes
                    // before comparing the embedded frame with its canvas.
                    let plane_bytes = width
                        .div_ceil(16)
                        .checked_mul(height.div_ceil(16))
                        .and_then(|blocks| blocks.checked_mul(16 * 16 + 2 * 8 * 8))
                        .ok_or("Image too large to copy")?;
                    if width == 0 || height == 0 || plane_bytes > max_bytes {
                        return Err("Image too large to copy".into());
                    }
                    if expected.is_some_and(|dimensions| dimensions != (width, height)) {
                        return Err("Could not decode image".into());
                    }
                    frame_chunk_seen = true;
                }
                b"VP8L" if animated_frame => {
                    frame_chunk_seen = true;
                }
                b"ALPH" if animated_frame => {
                    // `image-webp` decodes the successor payload as VP8 even when
                    // its chunk tag is not VP8; require the tag we validated.
                    alpha_successor = true;
                    if bytes.len().saturating_sub(next) < 8 {
                        return Err("Could not decode image".into());
                    }
                }
                b"VP8X" if !animated_frame => {
                    let canvas = payload.get(..10).ok_or("Could not decode image")?;
                    let width = usize::from(canvas[4])
                        | (usize::from(canvas[5]) << 8)
                        | (usize::from(canvas[6]) << 16);
                    let height = usize::from(canvas[7])
                        | (usize::from(canvas[8]) << 8)
                        | (usize::from(canvas[9]) << 16);
                    expected = Some((width + 1, height + 1));
                }
                b"ANMF" if !animated_frame => {
                    let frame = payload.get(..16).ok_or("Could not decode image")?;
                    let width = usize::from(frame[6])
                        | (usize::from(frame[7]) << 8)
                        | (usize::from(frame[8]) << 16);
                    let height = usize::from(frame[9])
                        | (usize::from(frame[10]) << 8)
                        | (usize::from(frame[11]) << 16);
                    chunks(
                        &payload[16..],
                        max_bytes,
                        true,
                        Some((width + 1, height + 1)),
                    )?;
                }
                _ => {}
            }
            bytes = &bytes[next..];
        }
        if animated_frame && !frame_chunk_seen {
            return Err("Could not decode image".into());
        }
        Ok(())
    }

    let header = bytes.get(..12).ok_or("Could not decode image")?;
    if &header[..4] != b"RIFF" || &header[8..12] != b"WEBP" {
        return Err("Could not decode image".into());
    }
    let riff_size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    let end = riff_size.checked_add(8).ok_or("Could not decode image")?;
    if end != bytes.len() {
        return Err("Could not decode image".into());
    }
    let body = bytes.get(12..end).ok_or("Could not decode image")?;
    chunks(body, max_bytes, false, None)
}

/// Decode only formats the gallery can paste, with a bound on both encoded and
/// expanded bytes. The URL extension and remote Content-Type are not evidence.
fn clipboard_pixels(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>)> {
    const MAX_IMAGE_BYTES: usize = 50 * 1024 * 1024;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image too large to copy".into());
    }
    let format = image::guess_format(bytes).map_err(|_| "Could not decode image")?;
    if format == image::ImageFormat::WebP {
        check_webp_vp8_frames(bytes, MAX_IMAGE_BYTES)?;
    }
    if !matches!(
        format,
        image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::WebP
            | image::ImageFormat::Gif
    ) {
        return Err("Could not decode image".into());
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
    reader.limits(limits);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| "Could not decode image")?;
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|size| *size <= MAX_IMAGE_BYTES)
        .ok_or("Image too large to copy")?;
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
    reader.limits(limits);
    let rgba = reader
        .decode()
        .map_err(|_| "Could not decode image")?
        .to_rgba8()
        .into_raw();
    if rgba.len() != pixels {
        return Err("Could not decode image".into());
    }
    Ok((width as usize, height as usize, rgba))
}

/// `buzz-media://localhost/<percent-encoded relay media URL>`, the shape of
/// `convertFileSrc(url, "buzz-media")` on every desktop platform.
fn media_request(
    request: &http::Request<Vec<u8>>,
) -> std::result::Result<(Url, Option<(u64, u64)>), u16> {
    if request.method() != http::Method::GET {
        return Err(405);
    }
    let encoded = request.uri().path().strip_prefix('/').ok_or(400u16)?;
    let target = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .map_err(|_| 400u16)?;
    let url = media_url(&target).ok_or(403u16)?;
    let range = match request.headers().get("range") {
        None => None,
        Some(value) => Some(media_range(value.to_str().map_err(|_| 416u16)?).ok_or(416u16)?),
    };
    Ok((url, range))
}

/// The relay blob a media source names: only relay-hosted blobs on an HTTPS
/// origin; never an arbitrary URL.
pub fn media_url(target: &str) -> Option<Url> {
    let url = Url::parse(target).ok()?;
    let name = url.path().strip_prefix("/media/")?;
    let (hash, extension) = name.split_once('.').unwrap_or((name, ""));
    let extension_ok = extension.is_empty()
        || extension == "thumb.jpg"
        || (extension.len() <= 8
            && extension
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    let mut base = url.clone();
    base.set_path("/");
    (origin(base.as_str()).is_ok()
        && url.query().is_none()
        && url.fragment().is_none()
        && hash.len() == 64
        && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && extension_ok)
        .then_some(url)
}

/// One `bytes=START-[END]` range, bounded so a response fits one buffer.
fn media_range(value: &str) -> Option<(u64, u64)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let last = start.checked_add(MEDIA_CHUNK - 1)?;
    let end = if end.is_empty() {
        last
    } else {
        end.parse::<u64>().ok()?.min(last)
    };
    (end >= start).then_some((start, end))
}

pub(crate) async fn fetch_media<S: Signer>(
    host: &S,
    url: Url,
    range: Option<String>,
) -> std::result::Result<http::Response<Vec<u8>>, u16> {
    fetch_media_bounded(host, url, range, MAX_MEDIA).await
}

pub(crate) async fn fetch_media_bounded<S: Signer>(
    host: &S,
    url: Url,
    range: Option<String>,
    max_bytes: usize,
) -> std::result::Result<http::Response<Vec<u8>>, u16> {
    buffer_media(send_media(host, url, range.as_deref()).await?, max_bytes).await
}

/// One freshly signed upstream GET; only a 200 or 206 is returned.
pub(crate) async fn send_media<S: Signer>(
    host: &S,
    url: Url,
    range: Option<&str>,
) -> std::result::Result<reqwest::Response, u16> {
    let auth = blossom_auth(host, &url, "get", "Get buzz-media", Vec::new())
        .await
        .map_err(|_| 401u16)?;
    let mut request = client()
        .map_err(|_| 502u16)?
        .get(url)
        // Match the broker's whole-media deadline; large documents may take minutes.
        .timeout(Duration::from_secs(600))
        .header("Authorization", auth);
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    let upstream = request.send().await.map_err(|_| 502u16)?;
    let status = upstream.status().as_u16();
    if !matches!(status, 200 | 206) {
        return Err(status);
    }
    Ok(upstream)
}

/// The response type of an upstream media response, as `media_type` decides it.
pub(crate) fn upstream_type(upstream: &reqwest::Response) -> (String, bool) {
    media_type(
        upstream
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
    )
}

/// Headers every `buzz-media` response carries, for an upstream `Content-Type`.
pub(crate) fn media_headers(kind: String, disposition: bool) -> http::response::Builder {
    let response = http::Response::builder()
        .header("Content-Type", kind)
        .header("Accept-Ranges", "bytes")
        .header("Cache-Control", "private, max-age=3600")
        .header("X-Content-Type-Options", "nosniff");
    if disposition {
        response.header("Content-Disposition", "attachment")
    } else {
        response
    }
}

pub(crate) async fn buffer_media(
    mut upstream: reqwest::Response,
    max_bytes: usize,
) -> std::result::Result<http::Response<Vec<u8>>, u16> {
    let status = upstream.status().as_u16();
    let limit = if status == 206 {
        MAX_MEDIA_RANGE.min(max_bytes)
    } else {
        max_bytes
    };
    if upstream
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(413);
    }
    let (kind, disposition) = upstream_type(&upstream);
    let mut response = media_headers(kind, disposition).status(status);
    if let Some(value) = upstream
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
    {
        response = response.header("Content-Range", value);
    }
    let mut body = Vec::new();
    while let Some(chunk) = upstream.chunk().await.map_err(|_| 502u16)? {
        if body.len() + chunk.len() > limit {
            return Err(413);
        }
        body.extend_from_slice(&chunk);
    }
    response.body(body).map_err(|_| 502)
}

pub(crate) fn valid_type(kind: &str) -> bool {
    let token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
    };
    kind.split_once('/')
        .is_some_and(|(a, b)| token(a) && token(b))
}

/// Mirrors the dev broker's `/api/relay/media`: render images (never SVG),
/// video and audio; everything else is a download.
fn media_type(value: Option<&str>) -> (String, bool) {
    let kind = value
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let inline = valid_type(&kind)
        && ((kind.starts_with("image/") && kind != "image/svg+xml")
            || kind.starts_with("video/")
            || kind.starts_with("audio/"));
    if inline {
        (kind, false)
    } else {
        ("application/octet-stream".into(), true)
    }
}

#[cfg(test)]
pub(crate) mod tests;
