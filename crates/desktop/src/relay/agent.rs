//! Purpose-bound agent operations: proofs, observer and archive frames, and
//! memories, decoded with the viewer's key. Never return key material or
//! unscoped plaintext. The key holder passes `secret` and `viewer` in.
use super::origin;
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use nostr::key::{PublicKey as NostrPublicKey, SecretKey as NostrSecretKey};
use nostr::nips::nip44::{self, v2::ConversationKey};
use secp256k1::{schnorr::Signature, Keypair, Secp256k1, XOnlyPublicKey};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

type Result<T> = std::result::Result<T, String>;

const HEX: &str = "0123456789abcdef";
fn key(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| HEX.as_bytes().contains(&b))
}
fn signature(secret: &[u8; 32], text: &str) -> Result<String> {
    let secp = Secp256k1::signing_only();
    let pair = Keypair::from_secret_key(
        &secp,
        &secp256k1::SecretKey::from_byte_array(*secret).map_err(|_| "Invalid identity")?,
    );
    Ok(secp
        .sign_schnorr_no_aux_rand(&Sha256::digest(text), &pair)
        .to_string())
}
/// The `wss://` relay URL of a community origin, without a trailing slash.
pub fn relay(community: &str) -> Result<String> {
    let mut url = origin(community)?;
    url.set_scheme("wss").map_err(|_| "Invalid community")?;
    Ok(url.to_string().trim_end_matches('/').to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
/// An agent whose community membership the owner confirms.
pub struct AgentTarget {
    pub pubkey: String,
    pub owner: String,
    pub confirmed: Option<bool>,
}
/// The owner's signed proof that `target` belongs to the community at
/// `relay_url` (see [`relay`]).
pub fn resolve(
    secret: &[u8; 32],
    viewer: &str,
    relay_url: String,
    target: AgentTarget,
) -> Result<Value> {
    if target.owner != viewer
        || !key(&target.pubkey)
        || target.pubkey == viewer
        || target.confirmed != Some(true)
    {
        return Err("Explicit owner community resolution required".into());
    }
    let proof = signature(
        secret,
        &format!("nostr:agent-community:{}:{relay_url}", target.pubkey),
    )?;
    Ok(json!({"pubkey": target.pubkey, "relayUrl": relay_url, "owner": viewer, "signature": proof}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// A harness log an agent asks its owner to authorize.
pub struct LogTarget {
    pub id: String,
    pub pubkey: String,
    pub relay_url: String,
    pub nonce: String,
}
/// The community relay and the target's own relay URL, both normalized, or
/// an error before the key is consulted.
pub fn log_relays(community: &str, target: &LogTarget) -> Result<(String, String)> {
    let expected = relay(community)?;
    if !target.relay_url.starts_with("wss://") {
        return Err("Log authorization unavailable".into());
    }
    let actual = relay(&target.relay_url.replacen("wss://", "https://", 1))?;
    if target.relay_url != actual {
        return Err("Log authorization unavailable".into());
    }
    Ok((expected, actual))
}

/// The owner's signature authorizing one harness log session, given the
/// relays from [`log_relays`].
pub fn log_proof(
    secret: &[u8; 32],
    viewer: &str,
    (expected, actual): (String, String),
    target: LogTarget,
) -> Result<String> {
    if actual != expected
        || !key(&target.pubkey)
        || target.pubkey == viewer
        || target.id
            != format!(
                "{}-{:x}",
                target.pubkey,
                Sha256::digest(expected.as_bytes())
            )
        || target.nonce.len() != 36
        || target.nonce.bytes().enumerate().any(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b != b'-'
            } else {
                !b.is_ascii_hexdigit() || b.is_ascii_uppercase()
            }
        })
    {
        return Err("Log authorization unavailable".into());
    }
    signature(
        secret,
        &format!(
            "buzz-app:harness-log:v1:{}:{}:{}:{}",
            target.id, target.pubkey, expected, target.nonce
        ),
    )
}

/// A signed event from an agent, as received from the relay.
#[derive(Deserialize, serde::Serialize, Clone)]
pub struct AgentEvent {
    pub id: String,
    pub pubkey: String,
    pub sig: String,
    pub created_at: u64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}
fn verified(event: &AgentEvent) -> Result<()> {
    if !key(&event.id)
        || !key(&event.pubkey)
        || event.sig.len() != 128
        || event.content.len() > 87472
        || event.tags.len() > 64
        || event
            .tags
            .iter()
            .any(|tag| tag.len() > 16 || tag.iter().any(|item| item.len() > 4096))
    {
        return Err("Invalid agent envelope".into());
    }
    let bytes = serde_json::to_vec(&(
        0,
        &event.pubkey,
        event.created_at,
        event.kind,
        &event.tags,
        &event.content,
    ))
    .map_err(|_| "Invalid agent envelope")?;
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    if hex_digest(&digest) != event.id {
        return Err("Invalid agent envelope".into());
    }
    let pubkey = event
        .pubkey
        .parse::<XOnlyPublicKey>()
        .map_err(|_| "Invalid agent envelope")?;
    let sig = event
        .sig
        .parse::<Signature>()
        .map_err(|_| "Invalid agent envelope")?;
    Secp256k1::verification_only()
        .verify_schnorr(&sig, &digest, &pubkey)
        .map_err(|_| "Invalid agent envelope".into())
}
fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn exact(event: &AgentEvent, name: &str, value: &str) -> bool {
    let tags: Vec<_> = event
        .tags
        .iter()
        .filter(|tag| tag.first().map(String::as_str) == Some(name))
        .collect();
    tags.len() == 1 && tags[0].len() == 2 && tags[0][1] == value
}
fn single_memory_tag(event: &AgentEvent, name: &str, value: &str) -> bool {
    let mut tags = event
        .tags
        .iter()
        .filter(|tag| tag.first().map(String::as_str) == Some(name));
    tags.next()
        .is_some_and(|tag| tag.get(1).is_some_and(|item| item == value))
        && tags.next().is_none()
}
fn decrypt(secret: &[u8; 32], agent: &str, content: &str) -> Result<String> {
    let secret = NostrSecretKey::from_slice(secret).map_err(|_| "Invalid identity")?;
    let agent = NostrPublicKey::from_hex(agent).map_err(|_| "Invalid agent envelope")?;
    nip44::decrypt(&secret, &agent, content).map_err(|_| "Invalid encrypted agent envelope".into())
}
/// The plaintext of a live observer frame (kind 24200) sent to `viewer`.
pub fn decode_observer(secret: &[u8; 32], viewer: &str, event: &AgentEvent) -> Result<Value> {
    if event.kind != 24200 {
        return Err("Invalid observer envelope".into());
    }
    decode_archive(secret, viewer, event, false)
}
/// The plaintext of an observer or archived frame sent to `viewer`. With
/// `history`, frames up to 90 days old are accepted instead of 5 minutes.
pub fn decode_archive(
    secret: &[u8; 32],
    viewer: &str,
    event: &AgentEvent,
    history: bool,
) -> Result<Value> {
    verified(event)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "Invalid clock")?
        .as_secs();
    if !matches!(event.kind, 24200 | 44200)
        || !exact(event, "p", viewer)
        || !exact(event, "agent", &event.pubkey)
        || (event.kind == 24200 && !exact(event, "frame", "telemetry"))
        || event.created_at > now + 300
        || now.saturating_sub(event.created_at) > if history { 90 * 24 * 60 * 60 } else { 300 }
        || event.content.len() < 132
        || event.content.len() > 87472
    {
        return Err("Invalid observer envelope".into());
    }
    let text = decrypt(secret, &event.pubkey, &event.content)?;
    if text.len() > 65535 {
        return Err("Invalid observer frame".into());
    }
    serde_json::from_str::<Value>(&text).map_err(|_| "Invalid observer frame")?;
    Ok(
        json!({"id": event.id, "agent": event.pubkey, "createdAt": event.created_at, "plaintext": text}),
    )
}

// Preserve duplicate-key evidence before serde_json's Value map discards it.
// This visitor is recursive so unknown nested fields cannot hide ambiguous names.
struct Unique(Value);
impl<'de> serde::Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, SeqAccess, Visitor};
        struct Visit;
        impl<'de> Visitor<'de> for Visit {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "unique JSON")
            }
            fn visit_bool<E: Error>(self, v: bool) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_i64<E: Error>(self, v: i64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_u64<E: Error>(self, v: u64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_f64<E: Error>(self, v: f64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_str<E: Error>(self, v: &str) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_none<E: Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_unit<E: Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut result = Vec::new();
                while let Some(Unique(value)) = seq.next_element::<Unique>()? {
                    result.push(value);
                }
                Ok(Unique(Value::Array(result)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut result = serde_json::Map::new();
                while let Some((name, Unique(value))) = map.next_entry::<String, Unique>()? {
                    if result.insert(name, value).is_some() {
                        return Err(A::Error::custom("duplicate memory field"));
                    }
                }
                Ok(Unique(Value::Object(result)))
            }
        }
        deserializer.deserialize_any(Visit)
    }
}
fn unique_json(text: &str) -> Result<Value> {
    serde_json::from_str::<Unique>(text)
        .map(|value| value.0)
        .map_err(|_| "Invalid memory body".into())
}

/// Whether `s` is a memory slug: `core` or a bounded `mem/...` path.
pub fn slug(s: &str) -> bool {
    if s == "core" {
        return true;
    }
    if s.len() > 255 || !s.starts_with("mem/") {
        return false;
    }
    s[4..].split('/').all(|part| {
        !part.is_empty()
            && part.len() <= 64
            && part.bytes().enumerate().all(|(i, b)| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || (i > 0 && (b == b'-' || b == b'_'))
            })
    })
}
/// The relay query for `agent`'s memories addressed to `viewer`, or an
/// error for an invalid target.
pub fn memory_query(viewer: &str, agent: &str) -> Result<String> {
    if !key(agent) || viewer == agent {
        return Err("Invalid memory target".into());
    }
    Ok(json!([{"kinds":[30174], "authors":[agent], "#p":[viewer], "limit":256}]).to_string())
}

// Validate the complete array grammar before projecting members. RawValue
// preserves syntactically valid members that Value cannot represent (for
// example lone surrogates, overflowing numbers or deep unknown fields).
/// The records of a memory query response, each `None` if unreadable.
pub fn memory_wire_records(body: &str) -> Result<Vec<Option<Value>>> {
    let records: Vec<Box<serde_json::value::RawValue>> =
        serde_json::from_str(body).map_err(|_| "Memory read failed")?;
    if records.len() > 256 {
        return Err("Memory read failed".into());
    }
    Ok(records
        .iter()
        .map(|raw| serde_json::from_str(raw.get()).ok())
        .collect())
}
/// The newest memory per slug, decrypted for `viewer`. `captured_viewer` is
/// the viewer the query was made for; a changed identity is rejected.
pub fn decode_memories(
    secret: &[u8; 32],
    viewer: &str,
    captured_viewer: &str,
    agent: &str,
    events: &[Option<Value>],
) -> Result<Value> {
    if !key(agent) || agent == viewer || viewer != captured_viewer || events.len() > 256 {
        return Err("Invalid memory target".into());
    }

    let secret_key = NostrSecretKey::from_slice(secret).map_err(|_| "Invalid identity")?;
    let public_key = NostrPublicKey::from_hex(agent).map_err(|_| "Invalid memory target")?;
    let conversation =
        ConversationKey::derive(&secret_key, &public_key).map_err(|_| "Invalid memory target")?;
    let mut heads: BTreeMap<String, (u64, String, Value)> = BTreeMap::new();
    let mut decoded_bytes = 0usize;
    let mut partial = events.len() == 256;
    for raw in events {
        let candidate = (|| -> Result<(String, u64, String, Value)> {
            let event = AgentEvent::deserialize(raw.as_ref().ok_or("Invalid memory envelope")?)
                .map_err(|_| "Invalid memory envelope")?;
            verified(&event)?;
            let d = event
                .tags
                .iter()
                .filter(|tag| tag.first().map(String::as_str) == Some("d"))
                .collect::<Vec<_>>();
            if event.kind != 30174
                || event.pubkey != agent
                || d.len() != 1
                || d[0].len() < 2
                || !single_memory_tag(&event, "p", viewer)
                || !key(&d[0][1])
            {
                return Err("Invalid memory envelope".into());
            }
            let payload = STANDARD
                .decode(&event.content)
                .map_err(|_| "Invalid memory envelope")?;
            // Reuse the batch's ECDH result and decode base64 only once.
            // v2::decrypt_to_bytes authenticates but does not check the version byte.
            if payload.first() != Some(&2) {
                return Err("Invalid memory envelope".into());
            }
            let text = String::from_utf8(
                nip44::v2::decrypt_to_bytes(&conversation, &payload)
                    .map_err(|_| "Invalid memory envelope")?,
            )
            .map_err(|_| "Invalid memory envelope")?;
            if STANDARD.encode(&payload) != event.content
                || nip44::v2::encrypt_to_bytes_with_nonce(
                    &conversation,
                    text.as_bytes(),
                    payload[1..33]
                        .try_into()
                        .map_err(|_| "Invalid memory envelope")?,
                )
                .map_err(|_| "Invalid memory envelope")?
                    != payload
            {
                return Err("Invalid memory envelope".into());
            }
            let value = unique_json(&text)?;
            let name = value
                .get("slug")
                .and_then(Value::as_str)
                .ok_or("Invalid memory slug")?;
            if !slug(name) {
                return Err("Invalid memory slug".into());
            }
            let mut mac = Hmac::<Sha256>::new_from_slice(conversation.as_bytes())
                .map_err(|_| "Invalid memory address")?;
            mac.update(b"agent-memory/v1/d-tag\0");
            mac.update(name.as_bytes());
            if hex_digest(&mac.finalize().into_bytes()) != d[0][1] {
                return Err("Invalid memory address".into());
            }
            let body = if name == "core" {
                value.get("profile").filter(|v| v.is_string())
            } else {
                value.get("value").filter(|v| v.is_string() || v.is_null())
            }
            .ok_or("Invalid memory body")?;
            Ok((
                name.to_owned(),
                event.created_at,
                event.id.clone(),
                json!({"slug": name, "body": body, "eventId": event.id, "createdAt": event.created_at}),
            ))
        })();
        match candidate {
            Ok((name, timestamp, id, entry)) => {
                let previous = heads.get(&name);
                if previous.map_or(true, |(time, previous_id, _)| {
                    timestamp > *time || (timestamp == *time && id < *previous_id)
                }) {
                    let added = serde_json::to_vec(&entry)
                        .map_err(|_| "Invalid memory listing")?
                        .len();
                    let removed = previous.map_or(0, |(_, _, old)| {
                        serde_json::to_vec(old).map_or(0, |bytes| bytes.len())
                    });
                    decoded_bytes = decoded_bytes.saturating_add(added).saturating_sub(removed);
                    if decoded_bytes > 1024 * 1024 {
                        return Err("Decoded memory budget exceeded".into());
                    }
                    heads.insert(name, (timestamp, id, entry));
                }
            }
            Err(_) => partial = true,
        }
    }
    let entries: Vec<_> = heads
        .into_values()
        .map(|(_, _, entry)| entry)
        .filter(|entry| !entry["body"].is_null())
        .collect();
    Ok(json!({"entries": entries, "partial": partial}))
}

fn library_field(row: &Value, name: &str, max: usize) -> Result<String> {
    let value = row
        .get(name)
        .and_then(Value::as_str)
        .ok_or("Current Buzz library unavailable")?;
    if value.encode_utf16().count() > max {
        return Err("Current Buzz library unavailable".into());
    }
    Ok(value.to_owned())
}

/// The agent definitions and identities in the Buzz app's managed-agent
/// library file, bounded and validated.
pub fn read_agent_library(path: PathBuf) -> Result<Value> {
    use std::{fs::OpenOptions, io::Read};
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| "Current Buzz library unavailable")?;
    let meta = file
        .metadata()
        .map_err(|_| "Current Buzz library unavailable")?;
    if !meta.is_file() || meta.len() > 8 * 1024 * 1024 {
        return Err("Current Buzz library unavailable".into());
    }
    let mut bytes = Vec::new();
    file.take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Current Buzz library unavailable")?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("Current Buzz library unavailable".into());
    }
    let rows: Vec<Value> =
        serde_json::from_slice(&bytes).map_err(|_| "Current Buzz library unavailable")?;
    if rows.len() > 2000 {
        return Err("Current Buzz library unavailable".into());
    }
    let mut definitions = Vec::new();
    let mut identities = Vec::new();
    let mut slugs = HashSet::new();
    let mut keys = HashSet::new();
    for row in rows {
        let pubkey = library_field(&row, "pubkey", 64)?;
        let name = library_field(&row, "name", 256)?;
        let avatar = row
            .get("avatar_url")
            .and_then(Value::as_str)
            .and_then(|source| {
                let source = source.trim();
                let data_image = ["png", "jpeg", "webp", "gif"].iter().any(|format| {
                    source
                        .strip_prefix(&format!("data:image/{format};base64,"))
                        .is_some_and(|encoded| {
                            let unpadded = encoded.trim_end_matches('=');
                            !unpadded.is_empty()
                                && unpadded
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
                        })
                });
                if source.len() <= 512 * 1024 && data_image {
                    return Some(source.to_owned());
                }
                if source.len() > 2048 {
                    return None;
                }
                let url = url::Url::parse(source).ok()?;
                if url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
                {
                    Some(url.to_string())
                } else {
                    None
                }
            });
        if pubkey.is_empty() {
            if row.get("slug").map_or(true, Value::is_null) {
                continue;
            }
            let id = library_field(&row, "slug", 256)?;
            if id.is_empty() || !slugs.insert(id.clone()) {
                return Err("Current Buzz library unavailable".into());
            }
            if row.get("is_active") == Some(&Value::Bool(false)) {
                continue;
            }
            if row.get("is_active").is_some_and(|v| !v.is_boolean()) {
                return Err("Current Buzz library unavailable".into());
            }
            let display = row
                .get("display_name")
                .filter(|v| !v.is_null())
                .map(|_| library_field(&row, "display_name", 256))
                .transpose()?
                .unwrap_or(name);
            let mut item = json!({"id":id,"name":display});
            if let Some(avatar) = avatar {
                item["avatar"] = json!(avatar);
            }
            definitions.push(item);
        } else {
            if !key(&pubkey) || !keys.insert(pubkey.clone()) {
                return Err("Current Buzz library unavailable".into());
            }
            let definition_id = row
                .get("persona_id")
                .filter(|v| !v.is_null())
                .map(|_| library_field(&row, "persona_id", 256))
                .transpose()?;
            let mut item = json!({"pubkey":pubkey,"name":name});
            if let Some(avatar) = avatar {
                item["avatar"] = json!(avatar);
            }
            if let Some(id) = definition_id.filter(|s| !s.is_empty()) {
                item["definitionId"] = json!(id);
            }
            identities.push(item);
        }
    }
    Ok(json!({"definitions":definitions,"identities":identities}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ([u8; 32], String) {
        let secret = [1; 32];
        let secp = Secp256k1::signing_only();
        let pair = Keypair::from_secret_key(
            &secp,
            &secp256k1::SecretKey::from_byte_array(secret).unwrap(),
        );
        (secret, pair.x_only_public_key().0.to_string())
    }
    fn signed_event(
        secret: &[u8; 32],
        kind: u16,
        tags: Vec<Vec<String>>,
        content: String,
    ) -> AgentEvent {
        let pair = Keypair::from_secret_key(
            &Secp256k1::signing_only(),
            &secp256k1::SecretKey::from_byte_array(*secret).unwrap(),
        );
        let pubkey = pair.x_only_public_key().0.to_string();
        let created_at = 1_700_000_000;
        let bytes =
            serde_json::to_vec(&json!([0, pubkey, created_at, kind, tags, content])).unwrap();
        let id = hex_digest(&Sha256::digest(bytes));
        let pair = Keypair::from_secret_key(
            &Secp256k1::signing_only(),
            &secp256k1::SecretKey::from_byte_array(*secret).unwrap(),
        );
        let sig = Secp256k1::signing_only()
            .sign_schnorr_no_aux_rand(
                &Sha256::digest(
                    serde_json::to_vec(&json!([0, pubkey, created_at, kind, tags, content]))
                        .unwrap(),
                ),
                &pair,
            )
            .to_string();
        AgentEvent {
            id,
            pubkey,
            sig,
            created_at,
            kind,
            tags,
            content,
        }
    }

    #[test]
    fn exact_agent_envelopes_and_unique_memory_fields() {
        let (secret, viewer) = fixture();
        let event = signed_event(
            &secret,
            24200,
            vec![vec!["p".into(), viewer.clone()]],
            "cipher".into(),
        );
        assert!(verified(&event).is_ok());
        let mut forged = event.clone();
        forged.tags.push(vec!["p".into(), viewer.clone()]);
        assert!(verified(&forged).is_err());
        assert!(!exact(&forged, "p", &viewer));
        assert!(unique_json(r#"{"slug":"core","nested":{"a":1,"a":2}}"#).is_err());
        assert!(unique_json(r#"{"slug":"core","nested":{"a":1,"b":2}}"#).is_ok());
        assert!(!slug("mem/a//b"));
        assert!(slug("mem/a/b_2"));
    }
    #[test]
    fn snapshot_memory_events_round_trip_through_native_owner_reader() {
        let agent_key = buzz_agent_controller::Secret::generate().unwrap();
        let agent = agent_key.pubkey().to_owned();
        let owner_bytes = [2; 32];
        let owner = signed_event(&owner_bytes, 1, vec![], String::new()).pubkey;
        let events = [
            ("core", "private fixture core"),
            ("mem/one", "private fixture entry"),
        ]
        .into_iter()
        .map(|(slug, body)| {
            Some(
                agent_key
                    .memory_event(&owner, slug, body, 1_700_000_001)
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();
        let listing = decode_memories(&owner_bytes, &owner, &owner, &agent, &events).unwrap();
        assert_eq!(listing["partial"], false);
        assert_eq!(listing["entries"][0]["body"], "private fixture core");
        assert_eq!(listing["entries"][1]["body"], "private fixture entry");
        assert_ne!(agent, owner);
    }

    #[test]
    fn encrypted_memory_and_observer_fixtures() {
        let (viewer_secret, viewer) = fixture();
        let agent_secret = [2; 32];
        let agent_key = NostrSecretKey::from_slice(&agent_secret).unwrap();
        let agent = signed_event(&agent_secret, 1, vec![], String::new()).pubkey;
        let conversation =
            ConversationKey::derive(&agent_key, &NostrPublicKey::from_hex(&viewer).unwrap())
                .unwrap();
        let encrypt = |text: &str| {
            STANDARD.encode(
                nip44::v2::encrypt_to_bytes_with_nonce(&conversation, text.as_bytes(), [7; 32])
                    .unwrap(),
            )
        };
        let address = |name: &str| {
            let mut mac = Hmac::<Sha256>::new_from_slice(conversation.as_bytes()).unwrap();
            mac.update(b"agent-memory/v1/d-tag\0");
            mac.update(name.as_bytes());
            hex_digest(&mac.finalize().into_bytes())
        };
        let memory = signed_event(
            &agent_secret,
            30174,
            vec![
                vec!["d".into(), address("core")],
                vec!["p".into(), viewer.clone()],
            ],
            encrypt(r#"{"slug":"core","profile":"remember this"}"#),
        );
        assert_eq!(memory.pubkey, agent);
        let result = decode_memories(
            &viewer_secret,
            &viewer,
            &viewer,
            &agent,
            &[Some(json!(memory))],
        )
        .unwrap();
        assert_eq!(result["entries"][0]["body"], "remember this");
        assert_eq!(result["partial"], false);
        // The low-level decoder must not admit unsupported versions, bad MACs,
        // or non-UTF-8 plaintext after switching to a batch-local key.
        let payload = STANDARD.decode(&memory.content).unwrap();
        let mut wrong_version = payload.clone();
        wrong_version[0] = 3;
        let mut bad_mac = payload;
        *bad_mac.last_mut().unwrap() ^= 1;
        let invalid_utf8 =
            nip44::v2::encrypt_to_bytes_with_nonce(&conversation, &[0xff], [7; 32]).unwrap();
        for content in [
            STANDARD.encode(wrong_version),
            STANDARD.encode(bad_mac),
            STANDARD.encode(invalid_utf8),
            "invalid base64".into(),
        ] {
            let invalid = signed_event(&agent_secret, 30174, memory.tags.clone(), content);
            let decoded = decode_memories(
                &viewer_secret,
                &viewer,
                &viewer,
                &agent,
                &[Some(json!(memory)), Some(json!(invalid))],
            )
            .unwrap();
            assert_eq!(decoded["entries"].as_array().unwrap().len(), 1);
            assert_eq!(decoded["entries"][0]["body"], "remember this");
            assert_eq!(decoded["partial"], true);
        }
        assert!(library_field(&json!({"name": "é".repeat(256)}), "name", 256).is_ok());
        assert!(library_field(&json!({"name": "é".repeat(257)}), "name", 256).is_err());
        let listing_wire = format!("[{},{{\"unknown\":\"\\ud800\"}}]", json!(memory));
        let wire = memory_wire_records(&listing_wire).unwrap();
        assert!(wire[1].is_none());
        let with_invalid =
            decode_memories(&viewer_secret, &viewer, &viewer, &agent, &wire).unwrap();
        assert_eq!(with_invalid["entries"][0]["body"], "remember this");
        assert_eq!(with_invalid["partial"], true);
        let reverse_wire = format!("[{{\"unknown\":1e400}},{}]", json!(memory));
        let reverse = decode_memories(
            &viewer_secret,
            &viewer,
            &viewer,
            &agent,
            &memory_wire_records(&reverse_wire).unwrap(),
        )
        .unwrap();
        assert_eq!(reverse["entries"][0]["body"], "remember this");
        assert_eq!(reverse["partial"], true);
        for broken in [
            "{}",
            "[{},]",
            "[1,,2]",
            "[1]tail",
            "[1,2",
            "[1}]",
            "[{\"valid\":1} {\"valid\":2}]",
            "[1,abc]",
            "[1,{\"a\":1,}]",
            "[1,{'a':1}]",
            "[1,{\"a\":\"\\q\"}]",
            "[1,{\"a\":[1,]}]",
            "\u{000c}[]",
        ] {
            assert!(memory_wire_records(broken).is_err(), "{broken}");
        }
        let deep = format!(
            "[{},{{\"unknown\":{}}}]",
            json!(memory),
            "[".repeat(200) + "0" + &"]".repeat(200)
        );
        let deep_records = memory_wire_records(&deep).unwrap();
        assert!(deep_records[1].is_none());
        let deep_listing =
            decode_memories(&viewer_secret, &viewer, &viewer, &agent, &deep_records).unwrap();
        assert_eq!(deep_listing["entries"][0]["body"], "remember this");
        assert_eq!(deep_listing["partial"], true);
        let hinted = signed_event(
            &agent_secret,
            30174,
            vec![
                vec!["d".into(), address("core"), "extra".into()],
                vec!["p".into(), viewer.clone(), "wss://relay.test".into()],
            ],
            encrypt(r#"{"slug":"core","profile":"hinted"}"#),
        );
        assert_eq!(
            decode_memories(
                &viewer_secret,
                &viewer,
                &viewer,
                &agent,
                &[Some(json!(hinted))]
            )
            .unwrap()["entries"][0]["body"],
            "hinted"
        );
        let tombstone = signed_event(
            &agent_secret,
            30174,
            vec![
                vec!["d".into(), address("mem/note")],
                vec!["p".into(), viewer.clone()],
            ],
            encrypt(r#"{"slug":"mem/note","value":null}"#),
        );
        let deleted = decode_memories(
            &viewer_secret,
            &viewer,
            &viewer,
            &agent,
            &[Some(json!(tombstone))],
        )
        .unwrap();
        assert_eq!(deleted["entries"].as_array().unwrap().len(), 0);
        let bad_address = signed_event(
            &agent_secret,
            30174,
            vec![
                vec!["d".into(), address("mem/other")],
                vec!["p".into(), viewer.clone()],
            ],
            encrypt(r#"{"slug":"core","profile":"wrong address"}"#),
        );
        let partial = decode_memories(
            &viewer_secret,
            &viewer,
            &viewer,
            &agent,
            &[Some(json!(bad_address))],
        )
        .unwrap();
        assert_eq!(partial["partial"], true);
        let mut tampered = memory.clone();
        let swapped = if &tampered.content[100..101] == "A" {
            "B"
        } else {
            "A"
        };
        tampered.content.replace_range(100..101, swapped);
        resign(&mut tampered, &agent_secret);
        let rejected = decode_memories(
            &viewer_secret,
            &viewer,
            &viewer,
            &agent,
            &[Some(json!(tampered))],
        )
        .unwrap();
        assert_eq!(rejected["entries"].as_array().unwrap().len(), 0);
        assert_eq!(rejected["partial"], true);
        assert!(decode_memories(&viewer_secret, &agent, &viewer, &agent, &[]).is_err());

        let observer = signed_event(
            &agent_secret,
            24200,
            vec![
                vec!["p".into(), viewer.clone()],
                vec!["agent".into(), agent.clone()],
                vec!["frame".into(), "telemetry".into()],
            ],
            encrypt(r#"{"type":"status","message":"active"}"#),
        );
        // A historical fixture is deliberately stale; make a freshly signed envelope.
        let mut observer = observer;
        observer.created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        resign(&mut observer, &agent_secret);
        assert_eq!(
            decode_observer(&viewer_secret, &viewer, &observer).unwrap()["agent"],
            agent
        );
        let mut historical = observer.clone();
        historical.created_at -= 3600;
        resign(&mut historical, &agent_secret);
        assert!(decode_observer(&viewer_secret, &viewer, &historical).is_err());
        assert_eq!(
            decode_archive(&viewer_secret, &viewer, &historical, true).unwrap()["plaintext"],
            r#"{"type":"status","message":"active"}"#
        );
        assert!(decode_archive(&viewer_secret, &agent, &historical, true).is_err());
        historical.created_at -= 90 * 24 * 60 * 60;
        resign(&mut historical, &agent_secret);
        assert!(decode_archive(&viewer_secret, &viewer, &historical, true).is_err());
        observer.tags.push(vec!["p".into(), viewer.clone()]);
        resign(&mut observer, &agent_secret);
        assert!(decode_observer(&viewer_secret, &viewer, &observer).is_err());
        assert!(decode_archive(&viewer_secret, &viewer, &observer, true).is_err());
    }

    #[test]
    #[ignore = "local performance measurement; run with --ignored --nocapture"]
    fn measure_memory_batch_decode() {
        let (secret, viewer) = fixture();
        let agent_secret = [2; 32];
        let agent = signed_event(&agent_secret, 1, vec![], String::new()).pubkey;
        let conversation = ConversationKey::derive(
            &NostrSecretKey::from_slice(&agent_secret).unwrap(),
            &NostrPublicKey::from_hex(&viewer).unwrap(),
        )
        .unwrap();
        let events: Vec<_> = (0..256)
            .map(|index| {
                let name = format!("mem/note-{index}");
                let mut mac = Hmac::<Sha256>::new_from_slice(conversation.as_bytes()).unwrap();
                mac.update(b"agent-memory/v1/d-tag\0");
                mac.update(name.as_bytes());
                let content = json!({"slug": name, "value": "x".repeat(1024)}).to_string();
                let encrypted = STANDARD.encode(
                    nip44::v2::encrypt_to_bytes_with_nonce(
                        &conversation,
                        content.as_bytes(),
                        [7; 32],
                    )
                    .unwrap(),
                );
                Some(json!(signed_event(
                    &agent_secret,
                    30174,
                    vec![
                        vec!["d".into(), hex_digest(&mac.finalize().into_bytes())],
                        vec!["p".into(), viewer.clone()],
                    ],
                    encrypted
                )))
            })
            .collect();
        for _ in 0..2 {
            decode_memories(&secret, &viewer, &viewer, &agent, &events).unwrap();
        }
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let result = decode_memories(&secret, &viewer, &viewer, &agent, &events).unwrap();
            assert_eq!(result["entries"].as_array().unwrap().len(), 256);
            assert_eq!(result["partial"], true);
            std::hint::black_box(result);
        }
        eprintln!(
            "memory batch: 256 x 1 KiB, 20 iterations: {:?}",
            started.elapsed()
        );
    }

    fn resign(event: &mut AgentEvent, secret: &[u8; 32]) {
        let bytes = serde_json::to_vec(&json!([
            0,
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags,
            event.content
        ]))
        .unwrap();
        let digest = Sha256::digest(bytes);
        event.id = hex_digest(&digest);
        let pair = Keypair::from_secret_key(
            &Secp256k1::signing_only(),
            &secp256k1::SecretKey::from_byte_array(*secret).unwrap(),
        );
        event.sig = Secp256k1::signing_only()
            .sign_schnorr_no_aux_rand(&digest, &pair)
            .to_string();
    }

    #[test]
    fn proof_domains_are_not_retargetable() {
        let (secret, viewer) = fixture();
        let auth = signature(&secret, &format!("nostr:agent-auth:{viewer}:")).unwrap();
        let scoped = signature(
            &secret,
            &format!("nostr:agent-community:{viewer}:wss://relay.test"),
        )
        .unwrap();
        assert_ne!(auth, scoped);
        assert_eq!(relay("https://relay.test").unwrap(), "wss://relay.test");
        assert!(relay("https://relay.test/path").is_err());
    }
}
