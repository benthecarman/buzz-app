//! Uploads to a relay's Blossom endpoint from host-owned spool files.
//!
//! The caller streams bounded chunks into a [`Spools`] entry, never a path;
//! [`process_spool`] then optionally converts the file with ffmpeg and
//! streams it upstream with a signed
//! `upload` token. An upload ID can be cancelled at any point, even before
//! its upload begins.
pub(crate) mod spool;

pub use spool::{Spool, Spools};

use crate::event::Signer;
use crate::media::valid_type;
use crate::relay::{blossom_auth, client, read_response, RelayResponse};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use url::Url;

type Result<T> = std::result::Result<T, String>;

/// The relay's largest accepted upload (videos).
const MAX_UPLOAD: usize = 500 * 1024 * 1024;

/// A cancel may overtake its upload IPC. Pending IDs expire because a cancel
/// can also arrive after completion or after an upload was rejected before start.
const PENDING_CANCEL_LIFETIME: Duration = Duration::from_secs(60);
#[derive(Default)]
struct UploadState {
    active: HashMap<String, oneshot::Sender<()>>,
    pending: HashMap<String, Instant>,
}
/// Upload IDs in flight and cancellations that arrived before their upload.
#[derive(Default)]
pub struct Uploads(std::sync::Mutex<UploadState>);

impl Uploads {
    fn lock(&self) -> std::sync::MutexGuard<'_, UploadState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers `id`, or returns `None` when it was already cancelled. The
    /// receiver fires when `id` is cancelled.
    pub fn start(&self, id: &str) -> Result<Option<oneshot::Receiver<()>>> {
        let mut uploads = self.lock();
        if uploads.pending.remove(id).is_some() {
            return Ok(None);
        }
        if uploads.active.contains_key(id) {
            return Err("Upload is already in progress".into());
        }
        if uploads.active.len() >= 64 {
            return Err("Uploads are busy".into());
        }
        let (sender, receiver) = oneshot::channel();
        uploads.active.insert(id.into(), sender);
        Ok(Some(receiver))
    }

    /// Cancels `id` if running; otherwise remembers it for a while, as a
    /// cancel may arrive before its upload starts.
    pub fn cancel(&self, id: &str) {
        let mut uploads = self.lock();
        if let Some(sender) = uploads.active.remove(id) {
            let _ = sender.send(());
            return;
        }
        let now = Instant::now();
        uploads
            .pending
            .retain(|_, inserted| now.duration_since(*inserted) < PENDING_CANCEL_LIFETIME);
        if uploads.pending.contains_key(id) {
            uploads.pending.insert(id.into(), now);
            return;
        }
        if uploads.pending.len() >= 64 {
            // With independent IPC calls we cannot distinguish an old cancel
            // from one that overtook its upload. Retire an arbitrary old pending
            // ID so a new cancellation still has a bounded chance to win.
            if let Some(old) = uploads.pending.keys().next().cloned() {
                uploads.pending.remove(&old);
            }
        }
        uploads.pending.insert(id.into(), now);
    }

    /// Whether no upload is running.
    pub fn is_empty(&self) -> bool {
        self.lock().active.is_empty()
    }

    /// Cancels every running upload.
    pub fn cancel_all(&self) {
        for (_, sender) in self.lock().active.drain() {
            let _ = sender.send(());
        }
    }

    /// Forgets a settled upload.
    pub fn finish(&self, id: &str) {
        self.lock().active.remove(id);
    }
}

/// A valid upload ID: 1 to 64 ASCII letters, digits or hyphens.
pub fn upload_id(value: Option<&str>) -> Result<&str> {
    value
        .filter(|id| {
            (1..=64).contains(&id.len())
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .ok_or_else(|| "Invalid upload ID".into())
}

/// Converts the spool when `preparation` names a mode, then streams it to
/// `url` with a signed `upload` token. A conversion failure is a relay-shaped
/// `{"code": ...}` response; `progress` sees bytes handed to the connection.
pub async fn process_spool<S: Signer>(
    host: &S,
    url: Url,
    spool: Spool,
    kind: Option<&str>,
    preparation: Option<&str>,
    progress: impl Fn(UploadSent) + Send + Sync + 'static,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<RelayResponse> {
    if cancelled.try_recv().is_ok() {
        return Err("Upload cancelled".into());
    }
    let (path, kind) = if let Some(mode) = preparation {
        match crate::media::preparation::prepare_file(
            &spool.source(),
            spool.size,
            spool.directory.path(),
            mode,
            cancelled,
        )
        .await
        {
            Ok(value) => (value.0, Some(value.1)),
            Err(crate::media::preparation::PreparationError::Cancelled) => {
                return Err("Upload cancelled".into())
            }
            Err(error) => {
                return Ok(RelayResponse {
                    status: error.status(),
                    headers: BTreeMap::new(),
                    body: serde_json::json!({"code": error.code()}).to_string(),
                })
            }
        }
    } else {
        (spool.source(), kind)
    };
    let hash = if path == spool.source() {
        (spool.size, spool.hash.clone())
    } else {
        // Hash in bounded blocks; the spool remains owned until it settles.
        let path = path.clone();
        tokio::task::spawn_blocking(move || spool::hash_file(&path))
            .await
            .map_err(|_| "Upload hashing could not complete")??
    };
    tokio::select! {
        result = upload_file(host, url, kind, path, hash, progress) => result,
        _ = &mut *cancelled => Err("Upload cancelled".into()),
    }
}

fn validate_upload_size(size: usize) -> Result<()> {
    if size == 0 || size > MAX_UPLOAD {
        return Err("File exceeds the supported upload limit".into());
    }
    Ok(())
}

/// Upload progress: bytes handed to the connection, of `total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct UploadSent {
    pub sent: u64,
    pub total: u64,
}
/// The largest chunk [`Spools::append`] accepts, and the upstream write size.
pub const UPLOAD_CHUNK: usize = 64 * 1024;

fn file_chunks(
    file: std::fs::File,
    total: u64,
    report: impl Fn(UploadSent) + Send + Sync + 'static,
) -> impl futures_util::Stream<Item = std::io::Result<bytes::Bytes>> + Send {
    use std::io::Read;
    let report = std::sync::Arc::new(report);
    futures_util::stream::try_unfold((file, 0u64, None), move |(mut file, sent, reported)| {
        let mut buffer = vec![0; UPLOAD_CHUNK];
        let report = report.clone();
        async move {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                if sent != total {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "Upload spool was truncated",
                    ));
                }
                return Ok(None);
            }
            let sent = sent + read as u64;
            if sent > total {
                return Err(std::io::Error::other("Upload spool changed"));
            }
            let percent = sent * 100 / total;
            if reported != Some(percent) {
                report(UploadSent { sent, total });
            }
            buffer.truncate(read);
            Ok(Some((
                bytes::Bytes::from(buffer),
                (file, sent, Some(percent)),
            )))
        }
    })
}

async fn upload_file<S: Signer>(
    host: &S,
    url: Url,
    kind: Option<&str>,
    path: std::path::PathBuf,
    (size, hash): (usize, String),
    progress: impl Fn(UploadSent) + Send + Sync + 'static,
) -> Result<RelayResponse> {
    validate_upload_size(size)?;
    let kind = kind
        .filter(|kind| valid_type(kind))
        .unwrap_or("application/octet-stream");
    let file = std::fs::File::open(path)
        .map_err(|_| "Media preparation could not access temporary storage")?;
    let auth = blossom_auth(
        host,
        &url,
        "upload",
        "Upload attachment",
        vec![vec!["x".into(), hash.clone()]],
    )
    .await?;
    let response = client()?
        .put(url)
        .timeout(Duration::from_secs(600))
        .header("Authorization", auth)
        .header("Content-Type", kind)
        .header("X-SHA-256", hash)
        .header(reqwest::header::CONTENT_LENGTH, size)
        .body(reqwest::Body::wrap_stream(file_chunks(
            file,
            size as u64,
            progress,
        )))
        .send()
        .await
        .map_err(|_| "Upload did not finish")?;
    read_response(response, 8192).await
}

#[cfg(test)]
mod tests;
