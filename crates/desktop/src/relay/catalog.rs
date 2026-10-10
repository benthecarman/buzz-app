//! NIP-AP catalog envelopes: agent definitions (30175) and team projections (30178).
//! The signer admits only the owner-to-self envelope; content parsing stays in JS,
//! but secrets-bearing fields are refused here as a last line of defense.
use crate::event::EventTemplate;
use serde_json::Value;

const MAX_CONTENT: usize = 65_535;
/// Serialized event bound: JSON escaping at most doubles JSON-text content, plus
/// the bounded tags and fields. Mirrors `MAX_EVENT_BYTES` in `catalog-envelope.ts`.
pub const MAX_EVENT_BYTES: usize = 2 * MAX_CONTENT + 2048;

/// Whether `kind` is a NIP-AP catalog kind.
pub fn is_catalog(kind: u16) -> bool {
    matches!(kind, 30175 | 30178)
}

fn persona_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

fn team_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 64
        && value.chars().all(|c| !c.is_control() && !c.is_whitespace())
}

/** Exactly one `d`, at most one exact `["shared","true"]`, at most one `client-id`. */
pub(crate) fn valid(event: &EventTemplate) -> bool {
    if !is_catalog(event.kind)
        // Signed events return to JavaScript; timestamps must fit Number.MAX_SAFE_INTEGER.
        || event.created_at > 9_007_199_254_740_991
        || event.content.len() > MAX_CONTENT
    {
        return false;
    }
    let count = |name: &str| {
        event
            .tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some(name))
            .count()
    };
    if count("d") != 1 || count("shared") > 1 || count("client-id") > 1 {
        return false;
    }
    let tags_ok = event.tags.iter().all(|tag| {
        tag.len() == 2
            && match tag[0].as_str() {
                "d" if event.kind == 30175 => persona_slug(&tag[1]),
                "d" => team_id(&tag[1]),
                "shared" => tag[1] == "true",
                "client-id" => tag[1].encode_utf16().count() <= 128,
                _ => false,
            }
    });
    if !tags_ok {
        return false;
    }
    let Ok(Value::Object(body)) = serde_json::from_str::<Value>(&event.content) else {
        return false;
    };
    // NIP-AP: catalog content is public plaintext and MUST NOT carry environment secrets.
    if body.contains_key("env_vars") {
        return false;
    }
    match event.kind {
        30175 => body
            .get("display_name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.trim().is_empty()),
        _ => {
            body.get("members").is_some_and(Value::is_array)
                && body
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !name.trim().is_empty())
        }
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
