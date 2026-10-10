//! Purpose-bound channel commands: details, lifecycle, identity archive,
//! member administration and stream creation, each in the broker's shape.
use super::events::verify_signature;
use super::hex_key;
use crate::event::EventTemplate;
use serde_json::Value;

type Result<T> = std::result::Result<T, String>;

/// Whether `value` is a canonical lowercase UUID, as channel IDs are.
pub fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
        && value.len() == 36
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
/// The value of the tag at `index` if it is exactly `[name, value]`.
pub fn tag<'a>(event: &'a EventTemplate, index: usize, name: &str) -> Option<&'a str> {
    let t = event.tags.get(index)?;
    match t.as_slice() {
        [key, value] if key == name => Some(value.as_str()),
        _ => None,
    }
}
fn common(event: &EventTemplate) -> bool {
    event.content.is_empty() && event.created_at <= i64::MAX as u64
}
fn details(event: &EventTemplate) -> bool {
    if event.kind == 9002 && common(event) && event.tags.len() == 2 {
        return tag(event, 0, "h").is_some_and(uuid)
            && tag(event, 1, "name").is_some_and(|name| {
                !name.is_empty()
                    && name.chars().count() <= 120
                    && name
                        .trim_start_matches(|c: char| c == '#' || c.is_whitespace())
                        .trim_end_matches(char::is_whitespace)
                        == name
            });
    }
    if event.kind != 9002 || !common(event) || !(3..=5).contains(&event.tags.len()) {
        return false;
    }
    let (Some(id), Some(name), Some(about)) = (
        tag(event, 0, "h"),
        tag(event, 1, "name"),
        tag(event, 2, "about"),
    ) else {
        return false;
    };
    let mut index = 3;
    if let Some(visibility) = tag(event, index, "visibility") {
        if !matches!(visibility, "private" | "open") {
            return false;
        }
        index += 1;
    }
    if let Some(ttl) = tag(event, index, "ttl") {
        if !ttl.is_empty()
            && (ttl.starts_with('0')
                || !ttl.bytes().all(|b| b.is_ascii_digit())
                || !ttl.parse::<i32>().is_ok_and(|n| n > 0))
        {
            return false;
        }
        index += 1;
    }
    uuid(id)
        && !name.is_empty()
        && name.chars().count() <= 120
        && name
            .trim_start_matches(|c: char| c == '#' || c.is_whitespace())
            .trim_end_matches(char::is_whitespace)
            == name
        && about.chars().count() <= 1000
        && !about.contains("Buzz session (")
        && index == event.tags.len()
}
fn lifecycle(event: &EventTemplate) -> bool {
    if !matches!(event.kind, 9002 | 9008 | 9021 | 9022 | 41012)
        || !common(event)
        || event.tags.len() != if event.kind == 9002 { 2 } else { 1 }
    {
        return false;
    }
    tag(event, 0, "h").is_some_and(uuid)
        && (event.kind != 9002 || matches!(tag(event, 1, "archived"), Some("true" | "false")))
}
fn archive(event: &EventTemplate) -> bool {
    if !matches!(event.kind, 9035 | 9036)
        || !common(event)
        || !(2..=3).contains(&event.tags.len())
        || event.tags[0] != ["-"]
    {
        return false;
    }
    let Some(target) = tag(event, 1, "p") else {
        return false;
    };
    if !hex_key(target) {
        return false;
    }
    if event.tags.len() == 2 {
        return true;
    }
    let auth = &event.tags[2];
    auth.len() == 4
        && auth[0] == "auth"
        && hex_key(&auth[1])
        && auth[1] != target
        && auth[3].len() == 128
        && auth[3]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn member_administration(event: &EventTemplate) -> bool {
    matches!(event.kind, 9000 | 9001)
        && event.content.is_empty()
        && event.created_at <= 9_007_199_254_740_991
        && event.tags.len() == if event.kind == 9000 { 3 } else { 2 }
        && tag(event, 0, "h").is_some_and(uuid)
        && tag(event, 1, "p").is_some_and(hex_key)
        && (event.kind == 9001
            || matches!(tag(event, 2, "role"), Some("admin" | "member" | "guest")))
}
fn valid(route: &str, event: &EventTemplate) -> bool {
    match route {
        "channel-details" => details(event),
        "channel-lifecycle" => lifecycle(event),
        "identity-archive" => archive(event),
        "member-administration" => member_administration(event),
        _ => false,
    }
}
fn template(event: &Value) -> Result<EventTemplate> {
    super::events::template(event, "Invalid channel lifecycle command")
}
/// The template of a channel command for `route`. With `viewer`, `event` is
/// a signed event to publish and must be the viewer's, correctly signed.
pub fn validate(route: &str, event: &Value, viewer: Option<&str>) -> Result<EventTemplate> {
    let parsed = template(event)?;
    if !valid(route, &parsed)
        || viewer.is_some_and(|v| event.get("pubkey").and_then(Value::as_str) != Some(v))
        || (route == "member-administration"
            && viewer.is_some_and(|v| tag(&parsed, 1, "p") == Some(v)))
    {
        return Err("Invalid channel lifecycle command".into());
    }
    if viewer.is_some() {
        verify_signature(event)?;
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_rename_only_admits_a_canonical_name() {
        let mut event = EventTemplate {
            kind: 9002,
            created_at: 12,
            content: String::new(),
            tags: vec![
                vec!["h".into(), uuid::Uuid::nil().to_string()],
                vec!["name".into(), "Session title".into()],
            ],
        };
        assert!(valid("channel-details", &event));
        for name in ["", "#name", " name", "name "] {
            event.tags[1][1] = name.into();
            assert!(!valid("channel-details", &event));
        }
        event.tags[1] = vec!["visibility".into(), "open".into()];
        assert!(!valid("channel-details", &event));
    }
    #[test]
    fn purpose_bound_templates() {
        let mut event = EventTemplate {
            kind: 9002,
            created_at: 12,
            content: String::new(),
            tags: vec![
                vec!["h".into(), uuid::Uuid::nil().to_string()],
                vec!["archived".into(), "true".into()],
            ],
        };
        assert!(valid("channel-lifecycle", &event));
        assert!(!valid("channel-details", &event));
        event.tags.push(vec!["name".into(), "injected".into()]);
        assert!(!valid("channel-lifecycle", &event));
        event.kind = 9035;
        assert!(!valid("identity-archive", &event));
        event.kind = 9002;
        event.tags = vec![
            vec!["h".into(), uuid::Uuid::nil().to_string()],
            vec!["name".into(), "C#".into()],
            vec!["about".into(), "".into()],
        ];
        assert!(valid("channel-details", &event));
        event.tags[1][1] = "#C".into();
        assert!(!valid("channel-details", &event));
    }
}

/// Whether `event` is the exact stream-channel creation command.
pub fn creation(event: &EventTemplate) -> bool {
    if event.kind != 9007 || !event.content.is_empty() || !(4..=7).contains(&event.tags.len()) {
        return false;
    }
    let (Some(id), Some(name), Some(visibility), Some(channel_type)) = (
        tag(event, 0, "h"),
        tag(event, 1, "name"),
        tag(event, 2, "visibility"),
        tag(event, 3, "channel_type"),
    ) else {
        return false;
    };
    if !uuid(id)
        || name.trim().is_empty()
        || name.chars().count() > 120
        || !matches!(visibility, "open" | "private")
        || channel_type != "stream"
    {
        return false;
    }
    let mut index = 4;
    if event
        .tags
        .get(index)
        .is_some_and(|t| t.first().is_some_and(|s| s == "about"))
    {
        let Some(about) = tag(event, index, "about") else {
            return false;
        };
        if about.chars().count() > 1000
            || (about.contains("Buzz session (") && !session_metadata(about))
        {
            return false;
        }
        index += 1;
    }
    if event
        .tags
        .get(index)
        .is_some_and(|t| t.first().is_some_and(|s| s == "ttl"))
    {
        let Some(ttl) = tag(event, index, "ttl") else {
            return false;
        };
        if event.tags.get(4).is_some_and(|t| {
            t.first().is_some_and(|s| s == "about") && session_metadata(&event.tags[4][1])
        }) || ttl.starts_with('0')
            || !ttl.parse::<u32>().is_ok_and(|n| n <= 2_147_483_647)
        {
            return false;
        }
        index += 1;
    }
    if event
        .tags
        .get(index)
        .is_some_and(|t| t.first().is_some_and(|s| s == "client-id"))
    {
        let Some(client) = tag(event, index, "client-id") else {
            return false;
        };
        if !uuid(client) {
            return false;
        }
        index += 1;
    }
    index == event.tags.len()
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn lifecycle_accepts_a_join_request_for_one_channel_only() {
        let channel = uuid::Uuid::nil().to_string();
        let join = serde_json::json!({
            "kind": 9021,
            "created_at": 1700000010,
            "content": "",
            "tags": [["h", channel]]
        });
        assert!(validate("channel-lifecycle", &join, None).is_ok());
        assert!(validate("channel-details", &join, None).is_err());
        for tags in [
            serde_json::json!([]),
            serde_json::json!([["h", "open"]]),
            serde_json::json!([["h", channel], ["h", channel]]),
            serde_json::json!([["h", channel], ["p", "00".repeat(32)]]),
        ] {
            let mut command = join.clone();
            command["tags"] = tags;
            assert!(validate("channel-lifecycle", &command, None).is_err());
        }
    }
    #[test]
    fn lifecycle_accepts_only_exact_archive_values() {
        let event = serde_json::json!({
            "kind": 9002,
            "created_at": 1700000010,
            "content": "",
            "tags": [["h", uuid::Uuid::nil().to_string()], ["archived", "false"]]
        });
        for value in ["true", "false"] {
            let mut command = event.clone();
            command["tags"][1][1] = serde_json::json!(value);
            assert!(validate("channel-lifecycle", &command, None).is_ok());
            assert!(validate("channel-details", &command, None).is_err());
        }
        for value in [
            serde_json::json!(""),
            serde_json::json!("False"),
            serde_json::json!("TRUE"),
            serde_json::json!("false "),
            serde_json::json!("0"),
            serde_json::json!("anything"),
            serde_json::json!(false),
            serde_json::json!(0),
            Value::Null,
        ] {
            let mut command = event.clone();
            command["tags"][1][1] = value;
            assert!(validate("channel-lifecycle", &command, None).is_err());
        }
        for tags in [
            serde_json::json!([["h", uuid::Uuid::nil().to_string()]]),
            serde_json::json!([["h", uuid::Uuid::nil().to_string()], ["archived"]]),
            serde_json::json!([
                ["h", uuid::Uuid::nil().to_string()],
                ["archived", "false", "extra"]
            ]),
            serde_json::json!([
                ["h", uuid::Uuid::nil().to_string()],
                ["archived", "false"],
                ["archived", "true"]
            ]),
            serde_json::json!([
                ["h", uuid::Uuid::nil().to_string()],
                ["archived", "false"],
                ["name", "injected"]
            ]),
            serde_json::json!([["archived", "false"], ["h", uuid::Uuid::nil().to_string()]]),
            serde_json::json!([["h", "not-a-channel"], ["archived", "false"]]),
        ] {
            let mut command = event.clone();
            command["tags"] = tags;
            assert!(validate("channel-lifecycle", &command, None).is_err());
        }
    }

    #[test]
    fn commands_reject_cross_route_and_foreign_signatures() {
        let host = crate::test_support::TestSigner;
        for (value, tampered) in [("true", "false"), ("false", "true")] {
            let template = EventTemplate {
                kind: 9002,
                created_at: 1700000010,
                content: String::new(),
                tags: vec![
                    vec!["h".into(), uuid::Uuid::new_v4().to_string()],
                    vec!["archived".into(), value.into()],
                ],
            };
            let event =
                crate::test_support::block_on(crate::Signer::sign(&host, template)).unwrap();
            assert!(validate("channel-lifecycle", &event, event["pubkey"].as_str()).is_ok());
            assert!(validate("channel-details", &event, event["pubkey"].as_str()).is_err());
            assert!(validate("channel-lifecycle", &event, Some(&"f".repeat(64))).is_err());
            let mut altered = event.clone();
            altered["tags"][1][1] = Value::String(tampered.into());
            assert!(validate("channel-lifecycle", &altered, event["pubkey"].as_str()).is_err());
        }
    }
    fn details_event(extra: Vec<Vec<String>>) -> EventTemplate {
        EventTemplate {
            kind: 9002,
            created_at: 1700000010,
            content: String::new(),
            tags: vec![
                vec!["h".into(), uuid::Uuid::nil().to_string()],
                vec!["name".into(), "Team".into()],
                vec!["about".into(), "Description".into()],
            ]
            .into_iter()
            .chain(extra)
            .collect(),
        }
    }

    #[test]
    fn details_admit_visibility_and_duration_at_sign_and_publish_boundaries() {
        let host = crate::test_support::TestSigner;
        for visibility in [None, Some("private"), Some("open")] {
            for ttl in [None, Some(""), Some("1"), Some("86400"), Some("2147483647")] {
                let extra = [("visibility", visibility), ("ttl", ttl)]
                    .into_iter()
                    .filter_map(|(key, value)| value.map(|value| vec![key.into(), value.into()]))
                    .collect();
                let event = serde_json::to_value(details_event(extra)).unwrap();
                let parsed = validate("channel-details", &event, None)
                    .unwrap_or_else(|error| panic!("{visibility:?}/{ttl:?}: {error}"));
                assert!(validate("channel-lifecycle", &event, None).is_err());
                let signed =
                    crate::test_support::block_on(crate::Signer::sign(&host, parsed)).unwrap();
                assert!(validate("channel-details", &signed, signed["pubkey"].as_str()).is_ok());
                assert!(validate("channel-details", &signed, Some(&"f".repeat(64))).is_err());
                let mut altered = signed;
                altered["tags"][1][1] = Value::String("Tampered".into());
                assert!(validate("channel-details", &altered, altered["pubkey"].as_str()).is_err());
            }
        }
    }

    #[test]
    fn details_reject_malformed_optional_tags() {
        for extra in [
            vec![vec![]],
            vec![vec!["ttl"]],
            vec![vec!["ttl", "1", "extra"]],
            vec![vec!["visibility"]],
            vec![vec!["visibility", "open", "extra"]],
            vec![vec!["visibility", "public"]],
            vec![vec!["visibility", ""]],
            vec![vec!["archived", "true"]],
            vec![vec!["p", "injected"]],
            vec![vec!["ttl", "0"]],
            vec![vec!["ttl", "01"]],
            vec![vec!["ttl", "-1"]],
            vec![vec!["ttl", "+1"]],
            vec![vec!["ttl", " 1"]],
            vec![vec!["ttl", "1\n"]],
            vec![vec!["ttl", "1.5"]],
            vec![vec!["ttl", "1e3"]],
            vec![vec!["ttl", "2147483648"]],
            vec![vec!["ttl", "18446744073709551616"]],
            vec![vec!["ttl", "86400"], vec!["visibility", "open"]],
            vec![vec!["visibility", "private"], vec!["visibility", "open"]],
            vec![vec!["ttl", "1"], vec!["ttl", ""]],
            vec![vec!["visibility", "open"], vec!["ttl", "0"]],
            vec![
                vec!["visibility", "open"],
                vec!["ttl", "1"],
                vec!["ttl", ""],
            ],
        ] {
            let event = details_event(
                extra
                    .iter()
                    .map(|tag| tag.iter().map(|s| (*s).into()).collect())
                    .collect(),
            );
            assert!(!valid("channel-details", &event), "{extra:?}");
        }
    }

    #[test]
    fn creation_requires_the_exact_stream_command() {
        let mut event = EventTemplate {
            kind: 9007,
            created_at: 1700000010,
            content: String::new(),
            tags: vec![
                vec!["h".into(), uuid::Uuid::new_v4().to_string()],
                vec!["name".into(), "Team".into()],
                vec!["visibility".into(), "private".into()],
                vec!["channel_type".into(), "stream".into()],
            ],
        };
        assert!(creation(&event));
        for malformed in [
            vec![],
            vec!["h".into()],
            vec!["h".into(), "x".into(), "extra".into()],
        ] {
            event.tags[0] = malformed;
            assert!(!creation(&event));
        }
        event.tags[0] = vec!["h".into(), uuid::Uuid::new_v4().to_string()];
        event.tags.swap(0, 1);
        assert!(!creation(&event));
        event.tags.swap(0, 1);
        event.tags[1] = event.tags[0].clone();
        assert!(!creation(&event));
        event.tags[1] = vec!["name".into(), "Team".into()];
        event.tags.push(vec!["p".into(), "attacker".into()]);
        assert!(!creation(&event));
        event.tags.pop();
        event.tags[3][1] = "dm".into();
        assert!(!creation(&event));
    }
}

fn session_metadata(description: &str) -> bool {
    const PREFIX: &str = "Buzz session (buzz.sessions/v1)";
    description == PREFIX
        || description
            .strip_prefix(&format!("{PREFIX}\nparent:"))
            .is_some_and(uuid)
}
