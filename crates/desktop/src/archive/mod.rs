//! Bounded local archive of agent observer frames. Only encrypted, signed
//! envelopes reach SQLite; frames are decrypted for the viewer on read.
//!
//! The caller owns the identity checks: it verifies an envelope before
//! [`Request::Ingest`] (see [`decode_archive`]) and that the viewer has not
//! changed, and it decrypts a [`Page`] with the viewer's key.
mod store;

use crate::relay::agent::{decode_archive, AgentEvent};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type Result<T> = std::result::Result<T, String>;

/// The archive database, opened on first use. Clones share it.
#[derive(Clone, Default)]
pub struct Archive(Arc<Mutex<Option<store::Store>>>);

/// One archive operation, as the renderer sends it.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
pub enum Request {
    Settings,
    Configure {
        observer: bool,
        metrics: bool,
        #[serde(rename = "observerDays")]
        observer_days: u32,
        revision: i64,
    },
    Read {
        kind: u16,
        agent: Option<String>,
        before: Option<i64>,
    },
    Clear {
        kind: Option<u16>,
    },
    Ingest {
        event: AgentEvent,
        revision: i64,
    },
}

/// The result of [`Archive::run`].
pub enum Reply {
    /// The finished JSON reply.
    Complete(Value),
    /// Stored envelopes that still need the viewer's key; see [`Page::decode`].
    Page(Page),
}

/// One page of encrypted records read from the archive.
pub struct Page {
    rows: Vec<(i64, i64, String)>,
    agents: Vec<String>,
    revision: i64,
}

impl Archive {
    /// Runs `request` for `viewer` in `community` against the database at
    /// `path`, opening it if needed. Blocking: SQLite work happens here, so
    /// call it off the async runtime and without holding the key owner's lock.
    /// An open failure stays retryable.
    pub fn run(
        &self,
        path: PathBuf,
        community: &str,
        viewer: &str,
        request: Request,
    ) -> Result<Reply> {
        let mut guard = self.0.lock().map_err(|_| "Archive unavailable")?;
        if guard.is_none() {
            *guard = Some(store::Store::open(path)?);
        }
        let store = guard.as_mut().ok_or("Archive unavailable")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "Invalid clock")?
            .as_secs() as i64;
        store.seed(viewer, community)?;
        store.prune(now)?;
        Ok(match request {
            Request::Settings => Reply::Complete(store.settings(viewer, community)?),
            Request::Configure {
                observer,
                metrics,
                observer_days,
                revision,
            } => Reply::Complete(store.configure(
                viewer,
                community,
                store::CaptureSettings {
                    observer,
                    metrics,
                    days: observer_days,
                    revision,
                },
                now,
            )?),
            Request::Clear { kind } => {
                store.clear(viewer, community, kind)?;
                Reply::Complete(json!({"cleared":true}))
            }
            Request::Ingest { event, revision } => {
                store.ingest(viewer, community, &event, revision, now)?;
                Reply::Complete(json!({"saved":true}))
            }
            Request::Read {
                kind,
                agent,
                before,
            } => {
                let (rows, revision) =
                    store.read(viewer, community, kind, agent.as_deref(), before)?;
                Reply::Page(Page {
                    rows,
                    revision,
                    agents: store.agents(viewer, community, kind)?,
                })
            }
        })
    }

    /// An archive over an already open database whose connection calls
    /// `busy` while another writer holds it. Lets tests block SQLite.
    #[doc(hidden)]
    pub fn with_busy_handler(
        path: PathBuf,
        viewer: &str,
        community: &str,
        busy: fn(i32) -> bool,
    ) -> Result<Self> {
        let store = store::Store::open(path)?;
        store.seed(viewer, community)?;
        store
            .busy_handler(busy)
            .map_err(|_| "Archive unavailable")?;
        Ok(Self(Arc::new(Mutex::new(Some(store)))))
    }
}

impl Page {
    /// The renderer's page: records decrypted for `viewer`, a count of
    /// unreadable ones, the agents, and the cursor of the next page.
    pub fn decode(self, secret: &[u8; 32], viewer: &str) -> Result<Value> {
        let Page {
            rows,
            agents,
            revision,
        } = self;
        let cursor = if rows.len() == 100 {
            rows.last().map(|row| row.0)
        } else {
            None
        };
        let mut records = Vec::new();
        let mut skipped = 0;
        for (_, received, raw) in rows {
            let decoded = serde_json::from_str(&raw)
                .map_err(|_| "Invalid saved archive record".into())
                .and_then(|event| decode_archive(secret, viewer, &event, true));
            match decoded {
                Ok(mut frame) => {
                    frame["receivedAt"] = json!(received * 1000);
                    records.push(frame);
                }
                Err(_) => skipped += 1,
            }
        }
        Ok(
            json!({"records":records,"skipped":skipped,"agents":agents,"before":cursor,"revision":revision}),
        )
    }
}

#[cfg(test)]
mod tests;
