//! Unsigned Nostr events and the signer that owns the user's key.
use serde::{Deserialize, Serialize};
use std::future::Future;

/// An event before signing. The signer adds `pubkey`, `id` and `sig`.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventTemplate {
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}

/// Signs events with the user's identity. Services here never see the key;
/// they hand a template to the signer and attach the signed event.
pub trait Signer: Send + Sync + 'static {
    /// The signed event as Nostr JSON.
    fn sign(
        &self,
        event: EventTemplate,
    ) -> impl Future<Output = Result<serde_json::Value, String>> + Send;
}
