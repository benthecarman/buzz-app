//! Private channel recipes, teams and their portable payloads (kind 30078
//! records): the coordinate each record must use and its admission.
use super::{channel_writes, hex_key};
use serde_json::Value;

type Result<T> = std::result::Result<T, String>;

/// The `t` tag of channel recipes and teams.
pub const KIT_TAG: &str = "buzz-channel-kit-v1";
/// The `t` tag of the viewer's own section and group records.
pub const ME_KIT_TAG: &str = "buzz-me-kit-v1";
/// The `t` tag of portable team manifests.
pub const MANIFEST_TAG: &str = "buzz-channel-kit-v2";
/// The `t` tag of portable team payload chunks.
pub const PAYLOAD_TAG: &str = "buzz-team-payload-v1";
/// The `t` tag of shared team instructions heads.
pub const TEXT_TAG: &str = "buzz-team-text-v1";
/// `v1:` plus at most 128 KiB of UTF-8 team instructions.
const TEXT_MANIFEST_BYTES: std::ops::RangeInclusive<u64> = 3..=3 + 128 * 1024;
fn portable(value: &Value) -> bool {
    let Some(manifest) = value.as_object() else {
        return false;
    };
    let bytes = value["bytes"].as_u64().unwrap_or(0);
    only(
        value,
        &["version", "owner", "revision", "digest", "bytes", "chunks"],
    ) && manifest.len() == 6
        && value["version"] == 1
        && value["owner"].as_str().is_some_and(hex_key)
        && value["revision"].as_str().is_some_and(channel_writes::uuid)
        && value["digest"].as_str().is_some_and(hex_key)
        && (1..=8 * 1024 * 1024).contains(&bytes)
        && value["chunks"].as_u64() == Some(bytes.div_ceil(16 * 1024))
}
fn payload(value: &Value, community: &str) -> bool {
    use base64::Engine;
    let Some(index) = value["index"].as_u64() else {
        return false;
    };
    let Some(revision) = value["revision"].as_str() else {
        return false;
    };
    let Some(data) = value["data"].as_str() else {
        return false;
    };
    only(
        value,
        &[
            "type",
            "id",
            "version",
            "community",
            "owner",
            "teamId",
            "revision",
            "index",
            "data",
        ],
    ) && value["version"] == 1
        && value["community"] == community
        && value["owner"].as_str().is_some_and(hex_key)
        && value["teamId"].as_str().is_some_and(identifier)
        && channel_writes::uuid(revision)
        && index < 512
        && value["id"] == format!("{revision}-{index}")
        && data.len() <= (16usize * 1024).div_ceil(3) * 4
        && base64::engine::general_purpose::STANDARD
            .decode(data)
            .is_ok_and(|bytes| {
                bytes.len() <= 16 * 1024
                    && base64::engine::general_purpose::STANDARD.encode(bytes) == data
            })
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn text(value: Option<&Value>, max: usize, required: bool) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| s.chars().count() <= max && (!required || !s.trim().is_empty()))
}
fn keys(value: Option<&Value>, max: usize, ids: bool) -> bool {
    let Some(list) = value.and_then(Value::as_array) else {
        return false;
    };
    let mut unique = std::collections::HashSet::new();
    list.len() <= max
        && list.iter().all(|v| {
            v.as_str()
                .is_some_and(|s| (if ids { identifier(s) } else { hex_key(s) }) && unique.insert(s))
        })
}
fn lineup(value: &Value) -> bool {
    keys(value.get("teamIds"), 100, true)
        && keys(value.get("agents"), 200, false)
        && text(value.get("canvas"), 24 * 1024, false)
        && value["canvas"]
            .as_str()
            .is_some_and(|s| s.len() <= 24 * 1024)
}
fn only(value: &Value, allowed: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.keys().all(|key| allowed.contains(&key.as_str())))
}
fn exact_lineup(value: &Value, allowed: &[&str]) -> bool {
    only(value, allowed) && lineup(value)
}
/// The `d` coordinate `raw` must be stored under in `community`, after
/// validating the record.
pub fn coordinate(raw: &Value, community: &str) -> Result<String> {
    let bytes = serde_json::to_vec(raw).map_err(|_| "Invalid channel recipe")?;
    if !only(raw, &["version", "community", "deleted", "value"])
        || bytes.len() > 48 * 1024
        || !matches!(raw.get("version").and_then(Value::as_u64), Some(1 | 2))
        || raw.get("community").and_then(Value::as_str) != Some(community)
        || !raw.get("deleted").is_some_and(Value::is_boolean)
    {
        return Err("Invalid channel recipe".into());
    }
    let value = raw.get("value").ok_or("Invalid channel recipe")?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or("Invalid channel recipe")?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or("Invalid channel recipe")?;
    let version = raw["version"].as_u64().ok_or("Invalid channel recipe")?;
    if (kind != "team-payload" && bytes.len() > 16 * 1024) || (version == 2 && kind != "team") {
        return Err("Invalid channel recipe".into());
    }
    let valid = match kind {
        "team" => {
            identifier(id)
                && if version == 2 {
                    only(value, &["type", "id", "name", "agents", "portable"])
                        && text(value.get("name"), 256, true)
                        && keys(value.get("agents"), 32, false)
                        && value["agents"].as_array().is_some_and(|a| !a.is_empty())
                        && portable(&value["portable"])
                } else {
                    only(value, &["type", "id", "name", "agents"])
                        && text(value.get("name"), 120, true)
                        && keys(value.get("agents"), 200, false)
                }
        }
        "team-payload" => version == 1 && raw["deleted"] == false && payload(value, community),
        "team-text" => {
            let manifest = &value["manifest"];
            version == 1
                && only(value, &["type", "id", "owner", "manifest"])
                && value.as_object().is_some_and(|v| v.len() == 4)
                && identifier(id)
                && value["owner"].as_str().is_some_and(hex_key)
                && if raw["deleted"] == true {
                    manifest.is_null()
                } else {
                    portable(manifest)
                        && manifest["owner"] == value["owner"]
                        && manifest["bytes"]
                            .as_u64()
                            .is_some_and(|bytes| TEXT_MANIFEST_BYTES.contains(&bytes))
                }
        }
        "template" => {
            exact_lineup(
                value,
                &[
                    "type",
                    "id",
                    "name",
                    "description",
                    "teamIds",
                    "agents",
                    "canvas",
                ],
            ) && identifier(id)
                && text(value.get("name"), 120, true)
                && text(value.get("description"), 1000, false)
        }
        "groups" => {
            if id != "me" || !only(value, &["type", "id", "groups", "assignments", "channels"]) {
                return Err("Invalid channel recipe".into());
            }
            if let Some(channels) = value.get("channels") {
                let Some(channels) = channels.as_array() else {
                    return Err("Invalid Me placement".into());
                };
                let mut seen = std::collections::HashSet::new();
                if channels.len() > 1000
                    || channels.iter().any(|v| {
                        !v.as_str()
                            .is_some_and(|id| channel_writes::uuid(id) && seen.insert(id))
                    })
                {
                    return Err("Invalid Me placement".into());
                }
            }
            let Some(groups) = value.get("groups").and_then(Value::as_array) else {
                return Err("Invalid channel recipe".into());
            };
            let mut names = std::collections::HashSet::new();
            groups.len() <= 100
                && groups.iter().all(|g| {
                    only(g, &["id", "name", "defaultTemplateId"])
                        && g.get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|s| identifier(s) && names.insert(s))
                        && text(g.get("name"), 120, true)
                        && g.get("defaultTemplateId")
                            .and_then(Value::as_str)
                            .is_some_and(|s| s.is_empty() || identifier(s))
                })
                && value
                    .get("assignments")
                    .and_then(Value::as_object)
                    .is_some_and(|a| {
                        a.len() <= 1000
                            && a.iter().all(|(channel, group)| {
                                channel_writes::uuid(channel)
                                    && group.as_str().is_some_and(|s| names.contains(s))
                            })
                    })
        }
        _ => false,
    };
    if !valid {
        return Err("Invalid channel recipe".into());
    }
    // encodeURIComponent for an HTTPS origin leaves only alphanumeric, -_.!~*'() unescaped.
    let encoded: String = community
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    if kind == "team-text" {
        let owner = value["owner"].as_str().ok_or("Invalid team text owner")?;
        Ok(format!("{TEXT_TAG}:{encoded}:{owner}:{id}"))
    } else if kind == "team-payload" {
        Ok(format!(
            "{PAYLOAD_TAG}:{encoded}:{}:{}:{}:{}",
            value["owner"].as_str().ok_or("Invalid payload owner")?,
            value["teamId"].as_str().ok_or("Invalid payload team")?,
            value["revision"]
                .as_str()
                .ok_or("Invalid payload revision")?,
            value["index"]
        ))
    } else {
        let tag = record_tag(raw);
        Ok(format!("{tag}:{encoded}:{kind}:{id}"))
    }
}
fn record_tag(raw: &Value) -> &'static str {
    let value = &raw["value"];
    if value["type"] == "team-payload" {
        PAYLOAD_TAG
    } else if value["type"] == "team-text" {
        TEXT_TAG
    } else if raw["version"] == 2 {
        MANIFEST_TAG
    } else if (value["type"] == "groups" && value["id"] == "me")
        || (value["type"] == "template"
            && value["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("me-section-")))
    {
        ME_KIT_TAG
    } else {
        KIT_TAG
    }
}
/// Checks a signed record's tags against its decrypted `raw` record.
pub fn admission(event: &Value, raw: &Value, community: &str) -> Result<()> {
    let coordinate = coordinate(raw, community)?;
    let expected_tag = record_tag(raw);
    let tags = event
        .get("tags")
        .and_then(Value::as_array)
        .ok_or("Invalid channel recipe")?;
    let mut d = 0;
    let mut t = 0;
    for tag in tags {
        let items = tag.as_array().ok_or("Invalid channel recipe")?;
        if items.len() != 2 {
            return Err("Invalid channel recipe".into());
        }
        match items[0].as_str() {
            Some("d") if items[1].as_str() == Some(&coordinate) => d += 1,
            Some("t") if items[1].as_str() == Some(expected_tag) => t += 1,
            Some("client-id") if items[1].as_str().is_some_and(|s| s.len() <= 128) => (),
            _ => return Err("Invalid channel recipe".into()),
        }
    }
    if d != 1 || t != 1 {
        return Err("Invalid channel recipe".into());
    }
    Ok(())
}

/// Checks a record before it is encrypted for `community`.
pub fn prepare(raw: &Value, community: &str) -> Result<()> {
    coordinate(raw, community).map(|_| ())
}

#[cfg(test)]
mod tests {
    #[test]
    fn me_placement_is_private_recipe_data_not_messages_group_data() {
        let community = "https://community.test";
        let mut raw = serde_json::json!({"version":1,"community":community,"deleted":false,"value":{
            "type":"groups","id":"me","groups":[],"assignments":{},
            "channels":["11111111-1111-4111-8111-111111111111"]
        }});
        assert!(coordinate(&raw, community).unwrap().starts_with(ME_KIT_TAG));
        raw["value"]["id"] = serde_json::json!("personal");
        assert!(coordinate(&raw, community).is_err());
        raw["value"]["id"] = serde_json::json!("me");
        raw["value"]["channels"] = serde_json::json!(["bad"]);
        assert!(coordinate(&raw, community).is_err());
        raw["value"]["channels"] = serde_json::json!([
            "11111111-1111-4111-8111-111111111111",
            "11111111-1111-4111-8111-111111111111"
        ]);
        assert!(coordinate(&raw, community).is_err());
    }

    use super::*;
    #[test]
    fn recipe_boundary_rejects_cross_community_and_invalid_values() {
        let community = "https://relay.test";
        let mut record = serde_json::json!({"version":1,"community":community,"deleted":false,
          "value":{"type":"team","id":"team-one","name":"Team","agents":[]}});
        assert!(prepare(&record, community).is_ok());
        assert!(prepare(&record, "https://other.test").is_err());
        record["value"]["agents"] = serde_json::json!(["invalid"]);
        assert!(prepare(&record, community).is_err());
    }
}
