//! Ranged `buzz-media` reads served from aligned, shared blocks.
//!
//! WebKit hands custom-scheme `<video>` loads to AVFoundation, which reads the
//! file in exact, mostly 8-byte to 4 KiB, ranges. One signed relay GET per read
//! kept every newly mounted player (such as the review viewer) on a black, 00:00
//! frame for many seconds. Each aligned block is one signed `Range` GET, shared
//! by every read and player of the same URL.

use super::{buffer_media, send_media, spool as media_spool, upstream_type};
use crate::event::Signer;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::OnceCell;
use url::Url;

pub(crate) const BLOCK: u64 = 1024 * 1024;
/// Retains at most 32 blocks (32 MiB); a reader still holding an evicted block
/// keeps it until it finishes. Identity never changes within a process, and the
/// webview may already keep these bytes under `Cache-Control: private, max-age=3600`.
const CAPACITY: usize = 32;

struct Block {
    start: u64,
    total: u64,
    headers: http::HeaderMap,
    bytes: Vec<u8>,
}

type Slot = Arc<OnceCell<Arc<Block>>>;

static BLOCKS: Mutex<VecDeque<((Url, u64), Slot)>> = Mutex::new(VecDeque::new());

fn slot(url: &Url, index: u64) -> Slot {
    let mut blocks = BLOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let position = blocks
        .iter()
        .position(|((cached, at), _)| *at == index && cached == url);
    let entry = match position.and_then(|position| blocks.remove(position)) {
        Some(entry) => entry,
        None => ((url.clone(), index), Slot::default()),
    };
    let slot = entry.1.clone();
    blocks.push_back(entry);
    if blocks.len() > CAPACITY {
        blocks.pop_front();
    }
    slot
}

/// `bytes START-END/TOTAL`, the only form a 206 to a single range carries.
pub(crate) fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?, total.parse().ok()?))
}

/// Why a block fetch left nothing to cache.
enum Uncached {
    Status(u16),
    /// The upstream ignored `Range` for something other than video: its bounded
    /// 200 goes to the player as is.
    Whole(Box<http::Response<Vec<u8>>>),
    /// The upstream ignored `Range` for a video, which is spooled to disk.
    Video(Box<reqwest::Response>),
}

async fn fetch_block<S: Signer>(
    host: &S,
    url: &Url,
    index: u64,
) -> std::result::Result<Arc<Block>, Uncached> {
    let start = index * BLOCK;
    let upstream = send_media(
        host,
        url.clone(),
        Some(&format!("bytes={start}-{}", start + BLOCK - 1)),
    )
    .await
    .map_err(Uncached::Status)?;
    if upstream.status() == 200 && upstream_type(&upstream).0.starts_with("video/") {
        return Err(Uncached::Video(Box::new(upstream)));
    }
    let response = buffer_media(upstream, super::MAX_MEDIA)
        .await
        .map_err(Uncached::Status)?;
    if response.status() != 206 {
        return Err(Uncached::Whole(Box::new(response)));
    }
    let (headers, bytes) = (response.headers().clone(), response.into_body());
    // Only exactly the requested block, or its EOF-truncated part, is cached.
    let exact = headers
        .get("content-range")
        .and_then(|value| content_range(value.to_str().ok()?))
        .filter(|&(first, last, total)| {
            first == start
                && total > start
                && last == (start + BLOCK).min(total) - 1
                && bytes.len() as u64 == last - first + 1
        });
    let (_, _, total) = exact.ok_or(Uncached::Status(502))?;
    Ok(Arc::new(Block {
        start,
        total,
        headers,
        bytes,
    }))
}

/// Answers `bytes=START-END` (already bounded by `media_range`) exactly, ending
/// early only at the end of the blob, or with an upstream 200 that ignored `Range`.
/// A video sent whole is spooled to disk, up to `video_limit` bytes.
pub(crate) async fn read<S: Signer>(
    host: &S,
    url: &Url,
    start: u64,
    end: u64,
    video_limit: u64,
) -> std::result::Result<http::Response<Vec<u8>>, u16> {
    if let Some(spool) = media_spool::cached(url) {
        return media_spool::read(spool, start, end).await;
    }
    let mut body = Vec::new();
    let mut first: Option<Arc<Block>> = None;
    let mut at = start;
    let (first, last) = loop {
        let index = at / BLOCK;
        let block = match slot(url, index)
            .get_or_try_init(|| fetch_block(host, url, index))
            .await
        {
            Ok(block) => block.clone(),
            Err(Uncached::Status(status)) => return Err(status),
            Err(Uncached::Whole(response)) => return Ok(*response),
            Err(Uncached::Video(upstream)) => {
                let spool = media_spool::open(url, *upstream, video_limit).await?;
                return media_spool::read(spool, start, end).await;
            }
        };
        let last = end.min(block.total.checked_sub(1).ok_or(416u16)?);
        if at > last {
            return Err(416);
        }
        // Content addressing makes every block of a URL agree on the blob.
        let to = (last + 1).min(block.start + block.bytes.len() as u64);
        if at < block.start || to <= at {
            return Err(502);
        }
        body.extend_from_slice(
            &block.bytes[(at - block.start) as usize..(to - block.start) as usize],
        );
        at = to;
        if at > last {
            break (first.unwrap_or(block), last);
        }
        first.get_or_insert(block);
    };
    let mut response = http::Response::builder().status(206);
    for (name, value) in &first.headers {
        if name != "content-range" {
            response = response.header(name, value);
        }
    }
    response
        .header(
            "Content-Range",
            format!("bytes {start}-{last}/{}", first.total),
        )
        .body(body)
        .map_err(|_| 502)
}
