//! Channel recipe encryption. The record shapes are checked by the shared
//! policy in buzz-desktop.
use super::*;
use buzz_desktop::relay::kit::admission;
pub(super) use buzz_desktop::relay::kit::prepare;
#[cfg(test)]
use buzz_desktop::relay::kit::{coordinate, KIT_TAG, MANIFEST_TAG, PAYLOAD_TAG, TEXT_TAG};
use serde_json::Value;

pub(super) async fn validate_ciphertext(
    host: &IdentityHost,
    event: &Value,
    community: &str,
) -> Result<Value> {
    if event.get("kind") != Some(&Value::from(30078)) {
        return Err("Invalid channel recipe".into());
    }
    let ciphertext = event
        .get("content")
        .and_then(Value::as_str)
        .ok_or("Invalid channel recipe")?;
    if ciphertext.len() > 64 * 1024 {
        return Err("Invalid channel recipe".into());
    }
    let plaintext = host.kit_cipher(ciphertext.to_owned(), false).await?;
    if plaintext.len() > 48 * 1024 {
        return Err("Invalid channel recipe".into());
    }
    let raw: Value = serde_json::from_str(&plaintext).map_err(|_| "Invalid channel recipe")?;
    admission(event, &raw, community)?;
    if matches!(
        raw["value"]["type"].as_str(),
        Some("team-payload" | "team-text")
    ) || raw["version"] == 2
    {
        let owner = if raw["version"] == 2 {
            &raw["value"]["portable"]["owner"]
        } else {
            &raw["value"]["owner"]
        };
        if owner.as_str() != Some(&host.viewer().await?) {
            return Err("Portable team belongs to another viewer".into());
        }
    }
    Ok(raw)
}

#[tauri::command]
pub(crate) async fn relay_kit_sign(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    event: crate::identity::EventTemplate,
) -> Result<Value> {
    origin(&community)?;
    let raw = serde_json::to_value(&event).map_err(|_| "Invalid channel recipe")?;
    validate_ciphertext(host.inner(), &raw, &community).await?;
    host.sign(event).await
}

#[cfg(test)]
mod crypto_tests {
    use super::*;
    #[tokio::test]
    async fn kit_crypto_round_trips_and_enforces_coordinate_and_community() {
        let host = IdentityHost::fixture();
        let community = "https://relay.test";
        let raw = serde_json::json!({"version":1,"community":community,"deleted":false,
            "value":{"type":"team","id":"team-one","name":"Team","agents":[]}});
        let ciphertext = host.kit_cipher(raw.to_string(), true).await.unwrap();
        let coordinate = coordinate(&raw, community).unwrap();
        let event = serde_json::json!({"kind":30078,"content":ciphertext,"tags":[["d",coordinate],["t",KIT_TAG]]});
        validate_ciphertext(&host, &event, community).await.unwrap();
        assert!(validate_ciphertext(&host, &event, "https://other.test")
            .await
            .is_err());
        let mut wrong = event.clone();
        wrong["tags"][0][1] = serde_json::json!("wrong");
        assert!(validate_ciphertext(&host, &wrong, community).await.is_err());
        wrong["tags"] = serde_json::json!([["d", coordinate], ["t", KIT_TAG], ["h", "injected"]]);
        assert!(validate_ciphertext(&host, &wrong, community).await.is_err());
    }
}

#[cfg(test)]
mod portable_tests {
    use super::*;
    #[tokio::test]
    async fn manifests_and_payloads_use_distinct_verified_owner_coordinates() {
        use base64::Engine;
        let host = IdentityHost::fixture();
        let owner = host.viewer().await.unwrap();
        let community = "https://relay.test";
        let revision = "11111111-1111-4111-8111-111111111111";
        let manifest = serde_json::json!({"version":2,"community":community,"deleted":false,
            "value":{"type":"team","id":"portable","name":"Portable","agents":["ab".repeat(32)],
            "portable":{"version":1,"owner":owner,"revision":revision,"digest":"cd".repeat(32),"bytes":4,"chunks":1}}});
        let chunk = serde_json::json!({"version":1,"community":community,"deleted":false,
            "value":{"type":"team-payload","id":format!("{revision}-0"),"version":1,"community":community,
            "owner":owner,"teamId":"portable","revision":revision,"index":0,
            "data":base64::engine::general_purpose::STANDARD.encode(b"test")}});
        for (raw, tag) in [(&manifest, MANIFEST_TAG), (&chunk, PAYLOAD_TAG)] {
            prepare(raw, community).unwrap();
            let coordinate = coordinate(raw, community).unwrap();
            let ciphertext = host.kit_cipher(raw.to_string(), true).await.unwrap();
            let mut event = serde_json::json!({"kind":30078,"content":ciphertext,"tags":[["d",coordinate],["t",tag]]});
            assert_eq!(
                validate_ciphertext(&host, &event, community).await.unwrap(),
                *raw
            );
            event["tags"][1][1] = serde_json::json!(KIT_TAG);
            assert!(validate_ciphertext(&host, &event, community).await.is_err());
        }
        let mut wrong = manifest.clone();
        wrong["value"]["portable"]["owner"] = serde_json::json!("ef".repeat(32));
        let ciphertext = host.kit_cipher(wrong.to_string(), true).await.unwrap();
        let event = serde_json::json!({"kind":30078,"content":ciphertext,"tags":[["d",coordinate(&wrong,community).unwrap()],["t",MANIFEST_TAG]]});
        assert!(validate_ciphertext(&host, &event, community).await.is_err());
        wrong = manifest;
        wrong["version"] = serde_json::json!(1);
        assert!(prepare(&wrong, community).is_err());
    }
}

#[cfg(test)]
mod team_text_tests {
    use super::*;
    fn text(owner: &str, community: &str, bytes: u64) -> Value {
        serde_json::json!({"version":1,"community":community,"deleted":false,
            "value":{"type":"team-text","id":"crew","owner":owner,
            "manifest":{"version":1,"owner":owner,"revision":"11111111-1111-4111-8111-111111111111",
            "digest":"cd".repeat(32),"bytes":bytes,"chunks":bytes.div_ceil(16 * 1024)}}})
    }
    #[tokio::test]
    async fn team_text_heads_bind_owner_tag_and_coordinate() {
        let host = IdentityHost::fixture();
        let owner = host.viewer().await.unwrap();
        let community = "https://relay.test";
        let live = text(&owner, community, 3);
        let mut tombstone = live.clone();
        tombstone["deleted"] = serde_json::json!(true);
        tombstone["value"]["manifest"] = Value::Null;
        for raw in [&live, &tombstone] {
            prepare(raw, community).unwrap();
            let coordinate = coordinate(raw, community).unwrap();
            assert_eq!(
                coordinate,
                format!("{TEXT_TAG}:https%3A%2F%2Frelay.test:{owner}:crew")
            );
            let ciphertext = host.kit_cipher(raw.to_string(), true).await.unwrap();
            let mut event = serde_json::json!({"kind":30078,"content":ciphertext,
                "tags":[["d",coordinate],["t",TEXT_TAG]]});
            assert_eq!(
                validate_ciphertext(&host, &event, community).await.unwrap(),
                *raw
            );
            // Never admitted under either team-record tag.
            for tag in [KIT_TAG, MANIFEST_TAG] {
                event["tags"][1][1] = serde_json::json!(tag);
                assert!(validate_ciphertext(&host, &event, community).await.is_err());
            }
        }
        // Another owner's head, even self-consistent, is refused.
        let other = text(&"ef".repeat(32), community, 3);
        let ciphertext = host.kit_cipher(other.to_string(), true).await.unwrap();
        let event = serde_json::json!({"kind":30078,"content":ciphertext,
            "tags":[["d",coordinate(&other, community).unwrap()],["t",TEXT_TAG]]});
        assert!(validate_ciphertext(&host, &event, community).await.is_err());
    }
    /// The same corpus runs against browser admission in
    /// `browser-host/channel-kit.test.mjs`.
    #[test]
    fn team_text_heads_match_the_shared_admission_corpus() {
        let owner = "ab".repeat(32);
        let corpus = include_str!(
            "../../../src/features/channel-templates/team-text-admission-contract.json"
        )
        .replace("OWNER", &owner);
        let cases: Vec<Value> = serde_json::from_str(&corpus).unwrap();
        assert!(cases.len() > 10);
        for case in cases {
            let event = serde_json::json!({"tags": case["tags"]});
            assert_eq!(
                admission(&event, &case["record"], "https://relay.test").is_ok(),
                case["accepted"] == true,
                "{}",
                case["name"]
            );
        }
    }
    #[test]
    fn team_text_heads_reject_bad_shapes() {
        let owner = "ab".repeat(32);
        let community = "https://relay.test";
        assert!(prepare(&text(&owner, community, 3 + 128 * 1024), community).is_ok());
        let mut bad = vec![
            text(&owner, community, 2),
            text(&owner, community, 4 + 128 * 1024),
            text(&owner, "https://other.test", 3),
        ];
        let mut extra = text(&owner, community, 3);
        extra["value"]["text"] = serde_json::json!("inline");
        bad.push(extra);
        let mut mismatched = text(&owner, community, 3);
        mismatched["value"]["manifest"]["owner"] = serde_json::json!("ef".repeat(32));
        bad.push(mismatched);
        let mut live_null = text(&owner, community, 3);
        live_null["value"]["manifest"] = Value::Null;
        bad.push(live_null);
        let mut dead_manifest = text(&owner, community, 3);
        dead_manifest["deleted"] = serde_json::json!(true);
        bad.push(dead_manifest);
        let mut v2 = text(&owner, community, 3);
        v2["version"] = serde_json::json!(2);
        bad.push(v2);
        for raw in bad {
            assert!(prepare(&raw, community).is_err(), "{raw}");
        }
    }
}
