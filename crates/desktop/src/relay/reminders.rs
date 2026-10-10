//! NIP-ER reminders (kind 30300). Self-encrypted only; never a general NIP-44 or signing primitive.
use crate::event::EventTemplate;
use nostr::{
    event::Event,
    key::{Keys, SecretKey},
    nips::nip44,
};
use serde::Deserialize;
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, String>;

/// The reminder event kind.
pub const KIND: u16 = 30300;
const MAX_PLAINTEXT: usize = 16 * 1024;
const DAY: u64 = 86_400;

fn hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn text(value: &Value, max: usize) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.encode_utf16().count() <= max)
}

/// The Buzz target dialect shared with beta desktop and mobile: `{eventId, channelId, preview, authorPubkey}`.
fn validate_content(content: &Value, status: &str) -> Result<()> {
    let invalid = || "Invalid reminder".to_owned();
    let data = content.as_object().ok_or_else(invalid)?;
    if data
        .keys()
        .any(|key| !["target", "note", "status"].contains(&key.as_str()))
        || data.get("status").and_then(Value::as_str) != Some(status)
        || data.get("note").is_some_and(|note| !text(note, 4096))
    {
        return Err(invalid());
    }
    match data.get("target") {
        Some(target) => {
            let target = target.as_object().ok_or_else(invalid)?;
            let field = |name: &str| target.get(name).and_then(Value::as_str).unwrap_or_default();
            if target.len() != 4
                || !hex(field("eventId"), 64)
                || !hex(field("authorPubkey"), 64)
                || field("channelId").is_empty()
                || field("channelId").len() > 256
                || !target
                    .get("preview")
                    .is_some_and(|preview| text(preview, 1024))
            {
                return Err(invalid());
            }
        }
        None if data
            .get("note")
            .and_then(Value::as_str)
            .is_some_and(|n| !n.is_empty()) => {}
        None => return Err(invalid()),
    }
    Ok(())
}

/// Exactly the tags `relay_sign_reminder` writes; the tag selects the status it may carry.
fn tag_status(tags: &[Vec<String>]) -> Option<&'static str> {
    match tags {
        [d, schedule] if d.len() == 2 && d[0] == "d" && hex(&d[1], 32) && schedule.len() == 2 => {
            let digits = !schedule[1].is_empty()
                && schedule[1].bytes().all(|b| b.is_ascii_digit())
                && (schedule[1] == "0" || !schedule[1].starts_with('0'))
                && schedule[1].parse::<u64>().is_ok_and(|n| n < (1 << 53));
            match schedule[0].as_str() {
                "not_before" if digits => Some("pending"),
                "expiration" if digits => Some("closed"),
                _ => None,
            }
        }
        _ => None,
    }
}

fn status_matches(tag: &str, content: &Value) -> Option<&'static str> {
    let status = content.get("status").and_then(Value::as_str)?;
    match (tag, status) {
        ("pending", "pending") => Some("pending"),
        ("closed", "done") => Some("done"),
        ("closed", "cancelled") => Some("cancelled"),
        _ => None,
    }
}

/// One verified, self-authored reminder for `viewer`, decrypted, or `None`
/// if this client could not have saved it.
pub fn decrypt(secret: &[u8; 32], viewer: &str, raw: &Value) -> Option<Value> {
    let event: Event = serde_json::from_value(raw.clone()).ok()?;
    event.verify().ok()?;
    let secret = SecretKey::from_slice(secret).ok()?;
    let public = Keys::new(secret.clone()).public_key();
    if event.kind.as_u16() != KIND || event.pubkey.to_hex() != viewer || event.pubkey != public {
        return None;
    }
    // Inspect raw tags: deserialization must not hide duplicate `d` selectors.
    let ds = raw["tags"]
        .as_array()?
        .iter()
        .filter(|tag| tag[0] == "d")
        .count();
    if ds != 1 || event.content.len() > 4 * MAX_PLAINTEXT {
        return None;
    }
    let plaintext = nip44::decrypt(&secret, &public, &event.content).ok()?;
    if plaintext.len() > MAX_PLAINTEXT {
        return None;
    }
    let content: Value = serde_json::from_str(&plaintext).ok()?;
    // Read and write share one rule: show only reminders this client could save again.
    let status = content.get("status").and_then(Value::as_str)?;
    validate_content(&content, status).ok()?;
    Some(json!({ "eventId": event.id.to_hex(), "content": content }))
}

/// Refuses a decode batch over 256 events or 4 MiB.
pub fn decode_capacity(events: &[Value]) -> Result<()> {
    if events.len() > 256
        || serde_json::to_vec(events)
            .map_err(|_| "Invalid reminders")?
            .len()
            > 4 * 1024 * 1024
    {
        return Err("Reminder decode capacity exceeded".into());
    }
    Ok(())
}

/// Decrypts verified, self-authored reminders. Other clients may write records this
/// client cannot read or save, so invalid entries are skipped, not fatal to the list.
pub fn decode(secret: &[u8; 32], viewer: &str, events: &[Value]) -> Value {
    Value::Array(
        events
            .iter()
            .filter_map(|raw| decrypt(secret, viewer, raw))
            .collect(),
    )
}

/// What the renderer may ask to save: the content and schedule, never
/// ciphertext or tags.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReminderIntent {
    pub d: String,
    pub created_at: u64,
    pub not_before: Option<u64>,
    pub expiration: Option<u64>,
    pub content: Value,
}

/// A validated reminder, ready to be encrypted to its author.
pub struct Prepared {
    d: String,
    created_at: u64,
    schedule: (&'static str, u64),
    plaintext: String,
}

/// Validates `intent` at time `now` (seconds).
pub fn prepare(intent: ReminderIntent, now: u64) -> Result<Prepared> {
    let invalid = || "Invalid reminder".to_owned();
    let schedule = match (intent.not_before, intent.expiration) {
        (Some(at), None) if at < (1 << 53) => ("not_before", at, "pending"),
        (None, Some(at)) if (now + 29 * DAY..=now + 91 * DAY).contains(&at) => {
            let status = intent.content.get("status").and_then(Value::as_str);
            (
                "expiration",
                at,
                if status == Some("done") {
                    "done"
                } else {
                    "cancelled"
                },
            )
        }
        _ => return Err(invalid()),
    };
    // created_at = max(now, previous + 1); allow a small lead for a future-dated predecessor.
    if !hex(&intent.d, 32) || intent.created_at + 60 < now || intent.created_at > now + 3600 {
        return Err(invalid());
    }
    validate_content(&intent.content, schedule.2)?;
    let plaintext = serde_json::to_string(&intent.content).map_err(|_| invalid())?;
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(invalid());
    }
    Ok(Prepared {
        d: intent.d,
        created_at: intent.created_at,
        schedule: (schedule.0, schedule.1),
        plaintext,
    })
}

impl Prepared {
    /// The unsigned reminder, encrypted to the key owner (`secret`).
    pub fn seal(self, secret: &[u8; 32]) -> Result<EventTemplate> {
        let secret = SecretKey::from_slice(secret).map_err(|_| "Invalid reminder")?;
        let public = Keys::new(secret.clone()).public_key();
        let content = nip44::encrypt(&secret, &public, &self.plaintext, nip44::Version::V2)
            .map_err(|_| "Could not encrypt reminder")?;
        Ok(EventTemplate {
            kind: KIND,
            created_at: self.created_at,
            content,
            tags: vec![
                vec!["d".into(), self.d],
                vec![self.schedule.0.into(), self.schedule.1.to_string()],
            ],
        })
    }
}

/// The status a signed reminder's tags select, if they are exactly the tags
/// [`Prepared::seal`] writes.
pub fn admission_tag(event: &Value) -> Result<&'static str> {
    let invalid = || "Invalid reminder".to_owned();
    let tags: Vec<Vec<String>> =
        serde_json::from_value(event["tags"].clone()).map_err(|_| invalid())?;
    tag_status(&tags).ok_or_else(invalid)
}

/// Admits a reminder for publication only if the viewer can decrypt it and
/// its content's status matches `tag` (from [`admission_tag`]).
pub fn admit(secret: &[u8; 32], viewer: &str, event: &Value, tag: &str) -> Result<()> {
    let invalid = || "Invalid reminder".to_owned();
    let decoded = decrypt(secret, viewer, event).ok_or_else(invalid)?;
    // `decrypt` already ran `validate_content`; the tag must select the content's status.
    status_matches(tag, &decoded["content"])
        .map(|_| ())
        .ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Value {
        json!({"eventId": "a".repeat(64), "channelId": "c1", "preview": "hi", "authorPubkey": "b".repeat(64)})
    }

    #[test]
    fn content_accepts_the_shared_target_dialect_only() {
        assert!(
            validate_content(&json!({"target": target(), "status": "pending"}), "pending").is_ok()
        );
        assert!(validate_content(&json!({"note": "call", "status": "done"}), "done").is_ok());
        // NIP-ER spec shape, unknown keys, status mismatch and empty records stay rejected.
        for bad in [
            json!({"target": {"id": "x", "a": "y", "relays": [], "preview": "p"}, "status": "pending"}),
            json!({"target": target(), "status": "pending", "extra": 1}),
            json!({"target": target(), "status": "done"}),
            json!({"note": "", "status": "pending"}),
            json!("text"),
        ] {
            assert!(validate_content(&bad, "pending").is_err(), "{bad}");
        }
    }

    #[test]
    fn tags_mirror_the_relay_not_before_validator() {
        let tags = |name: &str, value: &str| {
            vec![
                vec![
                    "d".to_string(),
                    "0123456789abcdef0123456789abcdef".to_string(),
                ],
                vec![name.to_string(), value.to_string()],
            ]
        };
        assert_eq!(
            tag_status(&tags("not_before", "1700000000")),
            Some("pending")
        );
        assert_eq!(
            tag_status(&tags("expiration", "1700000000")),
            Some("closed")
        );
        for value in ["", "01", "1e9", "-1", "9007199254740992"] {
            assert_eq!(tag_status(&tags("not_before", value)), None, "{value}");
        }
        assert_eq!(tag_status(&tags("alt", "1")), None);
        let mut extra = tags("not_before", "1");
        extra.push(vec!["p".into(), "x".into()]);
        assert_eq!(tag_status(&extra), None);
    }
}
