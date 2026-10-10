//! Videos from origins that ignore `Range`, written once to a temporary file.
//!
//! Players seek by `Range`, which such an origin cannot answer. Instead the
//! 200 is copied to an anonymous temporary file, which the OS deletes when its
//! last handle closes (also on a crash), and each player range is answered
//! from it as an exact range once its bytes have arrived.
//!
//! A spool lives while it is held, by a read waiting for its bytes or by a
//! `media_stream` connection still sending them (`hold`), and for `IDLE`
//! after its last hold ends. A closed player's connection closes, so its holds
//! end. Once the spool is dropped, its download stops, waiting reads fail and
//! the file is deleted.

use super::{media_headers, upstream_type};
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    sync::{watch, Semaphore, SemaphorePermit},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use url::Url;

/// At most two spool files exist at once, so temporary disk use stays within
/// twice the configured limit. Each file owns a permit until it is deleted.
const CAPACITY: usize = 2;
static FILES: Semaphore = Semaphore::const_new(CAPACITY);
/// How long a new video waits for an evicted spool's file to be deleted.
const ADMIT: Duration = Duration::from_secs(5);
static ADMITTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// Upstream bytes are written in batches of this size, bounding the memory one
/// download holds.
const BATCH: usize = super::blocks::BLOCK as usize;
/// How long an unheld spool is kept, such as for a player that paused and
/// closed its connection, or between a player's reads.
#[cfg(not(test))]
const IDLE: Duration = Duration::from_secs(60);
#[cfg(test)]
const IDLE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
enum Progress {
    Writing(u64),
    Done(u64),
    Failed(u16),
}

struct SpoolFile {
    file: Mutex<File>,
    _permit: SemaphorePermit<'static>,
}

#[cfg(test)]
pub(super) fn open_files() -> usize {
    CAPACITY - FILES.available_permits()
}

pub(super) struct Spool {
    file: Arc<SpoolFile>,
    /// The upstream `Content-Length`, when it sent one.
    total: Option<u64>,
    headers: http::HeaderMap,
    progress: watch::Receiver<Progress>,
    usage: Arc<Usage>,
    stop: CancellationToken,
}

/// When a spool was last released, and how many holds it has now.
struct Usage {
    released: Mutex<Instant>,
    holds: AtomicUsize,
}

/// Keeps a spool from expiring until dropped.
pub(super) struct Hold(Arc<Usage>);

impl Drop for Hold {
    fn drop(&mut self) {
        // Released before the count drops, so `idle` never sees an unheld
        // spool with a stale release time.
        *lock(&self.0.released) = Instant::now();
        self.0.holds.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Spool {
    fn hold(&self) -> Hold {
        self.usage.holds.fetch_add(1, Ordering::SeqCst);
        Hold(self.usage.clone())
    }
}

static SPOOLS: Mutex<VecDeque<(Url, Arc<Spool>)>> = Mutex::new(VecDeque::new());

/// Moves a usable spool of `url` to the most recent position; a failed one is
/// dropped so the next read fetches the video again.
fn lookup(spools: &mut VecDeque<(Url, Arc<Spool>)>, url: &Url) -> Option<Arc<Spool>> {
    let position = spools.iter().position(|(cached, _)| cached == url)?;
    let entry = spools.remove(position)?;
    if matches!(*entry.1.progress.borrow(), Progress::Failed(_)) {
        return None;
    }
    let spool = entry.1.clone();
    spools.push_back(entry);
    Some(spool)
}

fn spools() -> std::sync::MutexGuard<'static, VecDeque<(Url, Arc<Spool>)>> {
    lock(&SPOOLS)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn cached(url: &Url) -> Option<Arc<Spool>> {
    lookup(&mut spools(), url)
}

/// Holds `url`'s spool, if it has one.
pub(super) fn hold(url: &Url) -> Option<Hold> {
    cached(url).map(|spool| spool.hold())
}

/// A file permit, evicting the least recently used spool if none is free.
async fn admit() -> std::result::Result<SemaphorePermit<'static>, u16> {
    if let Ok(permit) = FILES.try_acquire() {
        return Ok(permit);
    }
    let evicted = spools().pop_front();
    if let Some((_, spool)) = evicted {
        spool.stop.cancel();
    }
    match tokio::time::timeout(ADMIT, FILES.acquire()).await {
        Ok(Ok(permit)) => Ok(permit),
        _ => Err(503),
    }
}

/// Starts copying a video 200 of at most `limit` bytes to disk, or joins the
/// copy already running for `url` and drops `upstream`.
pub(super) async fn open(
    url: &Url,
    upstream: reqwest::Response,
    limit: u64,
) -> std::result::Result<Arc<Spool>, u16> {
    let total = upstream.content_length();
    if total.is_some_and(|total| total > limit) {
        return Err(413);
    }
    if let Some(spool) = cached(url) {
        return Ok(spool);
    }
    // Admissions run one at a time, so a concurrent open of the same URL
    // joins the spool created here instead of evicting another player's.
    let _admitting = ADMITTING.lock().await;
    if let Some(spool) = cached(url) {
        return Ok(spool);
    }
    let permit = admit().await?;
    let mut spools = spools();
    let file = Arc::new(SpoolFile {
        file: Mutex::new(tempfile::tempfile().map_err(|_| 500u16)?),
        _permit: permit,
    });
    let (kind, _) = upstream_type(&upstream);
    let headers = media_headers(kind, false)
        .body(())
        .map_err(|_| 502u16)?
        .into_parts()
        .0
        .headers;
    let (sender, progress) = watch::channel(Progress::Writing(0));
    let usage = Arc::new(Usage {
        released: Mutex::new(Instant::now()),
        holds: AtomicUsize::new(0),
    });
    let stop = CancellationToken::new();
    let spool = Arc::new(Spool {
        file: file.clone(),
        total,
        headers,
        progress,
        usage: usage.clone(),
        stop: stop.clone(),
    });
    spools.push_back((url.clone(), spool.clone()));
    tokio::spawn(download(file, upstream, limit, total, sender, usage, stop));
    Ok(spool)
}

/// Removes the spool used through `usage`, unless something replaced it.
fn forget(usage: &Arc<Usage>) {
    spools().retain(|(_, spool)| !Arc::ptr_eq(&spool.usage, usage));
}

/// Resolves once the spool has been unheld for `IDLE`.
async fn idle(usage: &Usage) {
    loop {
        let deadline = if usage.holds.load(Ordering::SeqCst) > 0 {
            // Checked again after `IDLE`; a release then restarts the wait.
            Instant::now() + IDLE
        } else {
            let deadline = *lock(&usage.released) + IDLE;
            if Instant::now() >= deadline {
                return;
            }
            deadline
        };
        tokio::time::sleep_until(deadline).await;
    }
}

async fn download(
    file: Arc<SpoolFile>,
    mut upstream: reqwest::Response,
    limit: u64,
    total: Option<u64>,
    progress: watch::Sender<Progress>,
    usage: Arc<Usage>,
    stop: CancellationToken,
) {
    let copied = tokio::select! {
        copied = copy(&file, &mut upstream, limit, &progress) => copied,
        () = stop.cancelled() => Err(503),
        () = idle(&usage) => Err(503),
    };
    drop((file, upstream));
    match copied {
        Ok(length) if total.map_or(true, |total| total == length) => {
            progress.send_replace(Progress::Done(length));
            tokio::select! {
                () = stop.cancelled() => {}
                () = idle(&usage) => {}
            }
        }
        Ok(_) => {
            progress.send_replace(Progress::Failed(502));
        }
        Err(status) => {
            progress.send_replace(Progress::Failed(status));
        }
    }
    forget(&usage);
}

async fn copy(
    file: &Arc<SpoolFile>,
    upstream: &mut reqwest::Response,
    limit: u64,
    progress: &watch::Sender<Progress>,
) -> std::result::Result<u64, u16> {
    let mut written = 0;
    let mut batch = Vec::with_capacity(BATCH);
    loop {
        let chunk = upstream.chunk().await.map_err(|_| 502u16)?;
        if let Some(chunk) = &chunk {
            if written + (batch.len() + chunk.len()) as u64 > limit {
                return Err(413);
            }
            batch.extend_from_slice(chunk);
        }
        if batch.len() >= BATCH || (chunk.is_none() && !batch.is_empty()) {
            let (file, at) = (file.clone(), written);
            batch = tokio::task::spawn_blocking(move || file.write_at(&batch, at).map(|()| batch))
                .await
                .map_err(|_| 500u16)?
                .map_err(|_| 500u16)?;
            written += batch.len() as u64;
            batch.clear();
            progress.send_replace(Progress::Writing(written));
        }
        if chunk.is_none() {
            return Ok(written);
        }
    }
}

/// Answers `bytes=START-END` (already bounded by `media_range`) once those
/// bytes are on disk; without a `Content-Length` the copy must finish first.
pub(super) async fn read(
    spool: Arc<Spool>,
    start: u64,
    end: u64,
) -> std::result::Result<http::Response<Vec<u8>>, u16> {
    if spool.total.is_some_and(|total| start >= total) {
        return Err(416);
    }
    // Dropped with this read, such as when its player's connection closes.
    let _hold = spool.hold();
    let state = *spool
        .progress
        .clone()
        .wait_for(|progress| match *progress {
            Progress::Writing(written) => spool
                .total
                .is_some_and(|total| written > end.min(total - 1)),
            Progress::Done(_) | Progress::Failed(_) => true,
        })
        .await
        .map_err(|_| 503u16)?;
    let total = match state {
        Progress::Failed(status) => return Err(status),
        Progress::Done(total) => total,
        Progress::Writing(_) => spool.total.ok_or(502u16)?,
    };
    if start >= total {
        return Err(416);
    }
    let last = end.min(total - 1);
    let file = spool.file.clone();
    let body = tokio::task::spawn_blocking(move || {
        let mut body = vec![0; (last - start + 1) as usize];
        file.read_at(&mut body, start).map(|()| body)
    })
    .await
    .map_err(|_| 500u16)?
    .map_err(|_| 500u16)?;
    let mut response = http::Response::builder().status(206);
    for (name, value) in &spool.headers {
        response = response.header(name, value);
    }
    response
        .header("Content-Range", format!("bytes {start}-{last}/{total}"))
        .body(body)
        .map_err(|_| 502)
}

impl SpoolFile {
    // The writer and concurrent readers share one handle, and so one cursor.
    fn write_at(&self, bytes: &[u8], at: u64) -> std::io::Result<()> {
        let mut file = lock(&self.file);
        file.seek(SeekFrom::Start(at))?;
        file.write_all(bytes)
    }

    fn read_at(&self, bytes: &mut [u8], at: u64) -> std::io::Result<()> {
        let mut file = lock(&self.file);
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(bytes)
    }
}
