//! Which events the app may sign or publish for the user. Each kind has one
//! purpose-bound shape, mirroring the relay broker's admission; anything else
//! is refused, so a script cannot borrow the user's signature.
use super::{catalog, channel_writes, hex_key, origin};
use crate::event::EventTemplate;
use url::Url;

type Result<T> = std::result::Result<T, String>;

/// The unsigned template of a JSON event, or `error`.
pub fn template(event: &serde_json::Value, error: &str) -> Result<EventTemplate> {
    serde_json::from_value(serde_json::json!({
        "kind": event.get("kind"), "created_at": event.get("created_at"),
        "tags": event.get("tags"), "content": event.get("content"),
    }))
    .map_err(|_| error.into())
}

/// Whether `value` is a canonical lowercase UUID, as workflow IDs are.
pub fn workflow_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
        && value.len() == 36
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Admits `event` for signing in `community`, or says why not.
pub fn validate_event(community: &str, event: &EventTemplate) -> Result<()> {
    let mut relay = origin(community)?;
    if event.kind == 22242 {
        relay.set_scheme("wss").map_err(|_| "Invalid relay")?;
        let relay_tags: Vec<_> = event
            .tags
            .iter()
            .filter(|t| t.first().map(String::as_str) == Some("relay"))
            .collect();
        let challenges: Vec<_> = event
            .tags
            .iter()
            .filter(|t| t.first().map(String::as_str) == Some("challenge"))
            .collect();
        if !event.content.is_empty()
            || event.tags.len() != 2
            || relay_tags.len() != 1
            || relay_tags[0].len() != 2
            || Url::parse(&relay_tags[0][1]).ok().as_ref() != Some(&relay)
            || challenges.len() != 1
            || challenges[0].len() != 2
            || challenges[0][1].is_empty()
            || challenges[0][1].len() > 4096
        {
            return Err("Relay authentication does not match this community".into());
        }
    } else if event.kind == 5 {
        if !valid_message_deletion(event) && !valid_managed_agent_deletion(event) {
            validate_workflow_template(event)?;
        }
    } else if matches!(event.kind, 30620 | 46020) {
        validate_workflow_template(event)?;
    } else if event.kind == 9007 {
        if !channel_writes::creation(event) {
            return Err("Agent enrollment or channel operation unavailable or invalid".into());
        }
    } else if matches!(event.kind, 30175 | 30178) {
        if !catalog::valid(event) {
            return Err("Malformed catalog publication".into());
        }
    } else if event.kind == 40100 {
        if !valid_canvas(event) {
            return Err("Malformed Canvas save".into());
        }
    } else if event.kind == 28936 {
        // A NIP-43 leave request revokes the signer's own membership: empty
        // content and exactly the NIP-70 protected tag, nothing else.
        if !event.content.is_empty() || event.tags != vec![vec!["-".to_string()]] {
            return Err("A leave request carries no content or other tags".into());
        }
    } else if matches!(event.kind, 9030..=9032) {
        if !valid_member_command(event) {
            return Err("Invalid member change".into());
        }
    } else if !matches!(
        event.kind,
        0 | 7 | 9 | 1984 | 9000 | 9001 | 20001 | 30030 | 30177 | 30315 | 40003 | 42000 | 45010
    ) {
        return Err("This event is not supported by the packaged relay connection".into());
    }
    Ok(())
}

/** Match the broker's NIP-43 member command shape (`memberCommand` in
 * `src/features/communities/admin-protocol.ts`); the relay decides authority. */
fn valid_member_command(event: &EventTemplate) -> bool {
    let target = |tag: &[String]| matches!(tag, [name, key] if name == "p" && hex_key(key));
    let role = |tag: &[String]| matches!(tag, [name, role] if name == "role" && (role == "admin" || role == "member"));
    event.content.is_empty()
        && match (event.kind, event.tags.as_slice()) {
            (9031, [p]) => target(p),
            (9030 | 9032, [p, r]) => target(p) && role(r),
            _ => false,
        }
}

// Match the broker's purpose-bound Canvas admission, including legacy untagged retries.
fn valid_canvas(event: &EventTemplate) -> bool {
    event.content.len() <= 24 * 1024
        && event
            .tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some("h"))
            .count()
            == 1
        && event
            .tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some("expected-revision"))
            .count()
            <= 1
        && event.tags.iter().all(|tag| {
            if tag.len() != 2 {
                return false;
            }
            match tag[0].as_str() {
                "h" => channel_writes::uuid(&tag[1]),
                "client-id" => tag[1].encode_utf16().count() <= 128,
                "expected-revision" => {
                    tag[1] == "none"
                        || (tag[1].len() == 64
                            && tag[1]
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
                }
                _ => false,
            }
        })
}

/** Only the owner's managed-agent coordinate may use this global kind-5 shape. */
fn valid_managed_agent_deletion(event: &EventTemplate) -> bool {
    if !event.content.is_empty()
        // Signed events return to JavaScript; timestamps must fit Number.MAX_SAFE_INTEGER.
        || event.created_at > 9_007_199_254_740_991
        || event.tags.iter().any(|tag| {
            tag.len() != 2
                || tag[1].len() > 256
                || !matches!(tag[0].as_str(), "a" | "k" | "client-id")
        })
        || event
            .tags
            .iter()
            .enumerate()
            .any(|(i, tag)| event.tags[..i].iter().any(|prior| prior[0] == tag[0]))
        || event
            .tags
            .iter()
            .any(|tag| tag[0] == "k" && tag[1] != "30177")
    {
        return false;
    }
    let Some(coordinate) = event.tags.iter().find(|tag| tag[0] == "a") else {
        return false;
    };
    let mut parts = coordinate[1].split(':');
    parts.next() == Some("30177")
        && parts.next().is_some_and(hex_key)
        && parts.next().is_some_and(hex_key)
        && parts.next().is_none()
}

/** Keep the shared kind-5 writer aligned with the broker's channel-local deletion shape. */
fn valid_message_deletion(event: &EventTemplate) -> bool {
    if event.kind != 5
        || event.created_at > 9_007_199_254_740_991
        || !event.content.is_empty()
        || event.tags.len() > 106
        || event
            .tags
            .iter()
            .any(|tag| tag.len() != 2 || !matches!(tag[0].as_str(), "h" | "e" | "k" | "client-id"))
    {
        return false;
    }
    let channels: Vec<_> = event.tags.iter().filter(|tag| tag[0] == "h").collect();
    let targets: Vec<_> = event.tags.iter().filter(|tag| tag[0] == "e").collect();
    let kinds: Vec<_> = event.tags.iter().filter(|tag| tag[0] == "k").collect();
    channels.len() == 1
        && !channels[0][1].is_empty()
        && channels[0][1].encode_utf16().count() <= 256
        && (1..=100).contains(&targets.len())
        && targets.iter().all(|tag| {
            tag[1].len() == 64
                && tag[1]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
        && targets
            .iter()
            .enumerate()
            .all(|(i, tag)| targets[..i].iter().all(|prior| prior[1] != tag[1]))
        && (1..=3).contains(&kinds.len())
        && kinds
            .iter()
            .all(|tag| matches!(tag[1].as_str(), "7" | "9" | "40002"))
}

fn validate_workflow_template(event: &EventTemplate) -> Result<()> {
    let expected = if event.kind == 5 { "a" } else { "d" };
    if event.tags.len() > 8
        || event.content.len() > 24_000
        || (event.kind != 30620 && !event.content.is_empty())
        || event.tags.iter().any(|tag| {
            tag.len() != 2
                || tag.iter().any(|value| value.len() > 256)
                || !matches!(
                    tag[0].as_str(),
                    "h" | "d" | "a" | "expected-revision" | "client-id"
                )
        })
        || event.tags.iter().filter(|tag| tag[0] == "h").count() != 1
        || event.tags.iter().filter(|tag| tag[0] == expected).count() != 1
        || event.tags.iter().any(|tag| {
            (tag[0] == "d" && expected != "d")
                || (tag[0] == "a" && expected != "a")
                || (tag[0] == "expected-revision" && event.kind != 30620)
        })
        || event
            .tags
            .iter()
            .enumerate()
            .any(|(i, tag)| event.tags[..i].iter().any(|prior| prior[0] == tag[0]))
    {
        return Err("Malformed workflow command".into());
    }
    let coordinate = &event.tags.iter().find(|tag| tag[0] == expected).unwrap()[1];
    let id = if event.kind == 5 {
        let Some(("30620", owner, id)) = coordinate
            .split_once(':')
            .and_then(|(kind, rest)| rest.split_once(':').map(|(owner, id)| (kind, owner, id)))
        else {
            return Err("Malformed workflow command".into());
        };
        if owner.len() != 64
            || !owner
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("Malformed workflow command".into());
        }
        id
    } else {
        coordinate.as_str()
    };
    if !workflow_uuid(id)
        || !workflow_uuid(&event.tags.iter().find(|tag| tag[0] == "h").unwrap()[1])
        || event.tags.iter().any(|tag| {
            tag[0] == "expected-revision"
                && (tag[1].len() != 64
                    || !tag[1]
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
        })
    {
        return Err("Malformed workflow command".into());
    }
    Ok(())
}

/// Checks the ID and signature of a signed JSON event.
pub fn verify_signature(event: &serde_json::Value) -> Result<()> {
    let parsed: nostr::event::Event =
        serde_json::from_value(event.clone()).map_err(|_| "Invalid outgoing signature")?;
    parsed
        .verify()
        .map_err(|_| "Invalid outgoing signature".into())
}

#[cfg(test)]
mod tests;
