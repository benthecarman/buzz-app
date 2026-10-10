//! Channel command signing and publication; the shapes are checked by the
//! shared policy in buzz-desktop.
use super::*;
use buzz_desktop::relay::channel_writes::{tag, uuid, validate};
use serde_json::Value;

#[tauri::command]
pub(crate) async fn relay_channel_sign(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    route: String,
    event: Value,
) -> Result<Value> {
    origin(&community)?;
    let parsed = validate(&route, &event, None)?;
    if route == "member-administration"
        && tag(&parsed, 1, "p") == Some(host.viewer().await?.as_str())
    {
        return Err("Invalid member administration command".into());
    }
    host.sign(parsed).await
}

#[tauri::command]
pub(crate) async fn relay_channel_publish(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    route: String,
    event: Value,
) -> Result<RelayResponse> {
    let url = request_url(&community, "/events", "POST")?;
    let viewer = host.viewer().await?;
    validate(&route, &event, Some(&viewer))?;
    let body = serde_json::to_string(&event).map_err(|_| "Invalid channel lifecycle command")?;
    if body.len() > MAX_BODY {
        return Err("Invalid relay request body".into());
    }
    send(host.inner(), url, "POST", Some(body), true, MAX_RESPONSE).await
}

#[tauri::command]
pub(crate) async fn relay_kit_prepare(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    record: Value,
) -> Result<String> {
    origin(&community)?;
    super::kit::prepare(&record, &community)?;
    let text = serde_json::to_string(&record).map_err(|_| "Invalid channel recipe")?;
    host.kit_cipher(text, true).await
}

#[tauri::command]
pub(crate) async fn relay_kit_decode(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    events: Vec<Value>,
) -> Result<Vec<Value>> {
    origin(&community)?;
    if events.len() > 16
        || serde_json::to_vec(&events)
            .map_err(|_| "Invalid channel recipe")?
            .len()
            > 1024 * 1024
    {
        return Err("Invalid channel recipe".into());
    }
    let viewer = host.viewer().await?;
    let mut decoded = Vec::with_capacity(events.len());
    for event in events {
        let raw: nostr::event::Event =
            serde_json::from_value(event.clone()).map_err(|_| "Invalid channel recipe")?;
        raw.verify().map_err(|_| "Invalid channel recipe")?;
        if event.get("pubkey").and_then(Value::as_str) != Some(&viewer)
            || event.get("kind") != Some(&serde_json::json!(30078))
        {
            return Err("Invalid channel recipe".into());
        }
        let record = super::kit::validate_ciphertext(host.inner(), &event, &community).await?;
        decoded.push(serde_json::json!({"eventId": event["id"], "record": record}));
    }
    Ok(decoded)
}

#[tauri::command]
pub(crate) async fn relay_direct_message(
    host: tauri::State<'_, IdentityHost>,
    community: String,
    pubkeys: Vec<String>,
) -> Result<String> {
    let url = request_url(&community, "/events", "POST")?;
    let viewer = host.viewer().await?;
    let unique: std::collections::HashSet<_> = pubkeys.iter().collect();
    if pubkeys.is_empty()
        || pubkeys.len() > 8
        || unique.len() != pubkeys.len()
        || pubkeys.iter().any(|p| !super::hex_key(p) || p == &viewer)
    {
        return Err("Choose between one and eight other people.".into());
    }
    let event = host
        .sign(EventTemplate {
            kind: 41010,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "System clock is unavailable")?
                .as_secs(),
            content: String::new(),
            tags: pubkeys
                .into_iter()
                .map(|p| vec!["p".into(), p])
                .chain(std::iter::once(vec![
                    "client".into(),
                    uuid::Uuid::new_v4().to_string(),
                ]))
                .collect(),
        })
        .await?;
    let body = serde_json::to_string(&event).map_err(|_| "Invalid channel lifecycle command")?;
    let response = send(host.inner(), url, "POST", Some(body), true, MAX_RESPONSE).await?;
    if response.status != 200 {
        return Err("The direct message could not be opened. Try again.".into());
    }
    let receipt: Value = serde_json::from_str(&response.body)
        .map_err(|_| "The direct message could not be opened. Try again.")?;
    if receipt.get("event_id") != event.get("id")
        || receipt.get("accepted") != Some(&Value::Bool(true))
    {
        return Err("The direct message could not be opened. Try again.".into());
    }
    let payload = receipt
        .get("message")
        .and_then(Value::as_str)
        .and_then(|m| m.strip_prefix("response:"))
        .ok_or("The direct message could not be opened. Try again.")?;
    let result: Value = serde_json::from_str(payload)
        .map_err(|_| "The relay returned an invalid direct message.")?;
    let id = result
        .get("channel_id")
        .and_then(Value::as_str)
        .ok_or("The relay returned an invalid direct message.")?;
    if !uuid(id) {
        return Err("The relay returned an invalid direct message.".into());
    }
    Ok(id.into())
}

#[cfg(test)]
mod member_administration_tests {
    use super::*;
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};

    #[test]
    fn member_commands_match_js_and_sign_through_production_ipc_and_acl() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../src/features/channel-members/administration-contract.json"
        ))
        .unwrap();
        let app = mock_builder()
            .manage(IdentityHost::fixture())
            .invoke_handler(crate::commands())
            .build(crate::app_context())
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
            .build()
            .unwrap();
        let invoke = |command: &str, event: Value| {
            get_ipc_response(
                &view,
                tauri::webview::InvokeRequest {
                    cmd: command.into(),
                    callback: tauri::ipc::CallbackFn(0),
                    error: tauri::ipc::CallbackFn(1),
                    url: view.url().unwrap(),
                    body: tauri::ipc::InvokeBody::Json(serde_json::json!({
                        "community": "https://relay.test", "route": "member-administration", "event": event
                    })),
                    headers: Default::default(),
                    invoke_key: INVOKE_KEY.into(),
                },
            )
        };
        for case in cases["accepted"].as_array().unwrap() {
            let template = case["event"].clone();
            let event: Value = invoke("relay_channel_sign", template.clone())
                .unwrap()
                .deserialize()
                .unwrap();
            super::super::verify_signature(&event).unwrap();
            assert_eq!(event["tags"], template["tags"]);
            assert_eq!(event["kind"], template["kind"]);
            assert_eq!(event["created_at"], template["created_at"]);
            assert_eq!(event["content"], template["content"]);
            let viewer = event["pubkey"].as_str().unwrap();
            assert!(validate("member-administration", &event, Some(viewer)).is_ok());
            for route in ["channel-details", "channel-lifecycle", "identity-archive"] {
                assert!(validate(route, &event, Some(viewer)).is_err());
            }
            assert!(validate("member-administration", &event, Some(&"f".repeat(64))).is_err());
            let mut tampered = event.clone();
            tampered["tags"][1][1] = Value::String("d".repeat(64));
            assert!(validate("member-administration", &tampered, Some(viewer)).is_err());
            // Publish reaches the registered handler/ACL and rejects before networking.
            let error = invoke("relay_channel_publish", tampered).unwrap_err();
            assert!(error.to_string().contains("signature"), "{error}");
            let mut self_target = template;
            self_target["tags"][1][1] = Value::String(viewer.into());
            let error = invoke("relay_channel_sign", self_target).unwrap_err();
            assert!(
                error.to_string().contains("Invalid member administration"),
                "{error}"
            );
        }
        for case in cases["rejected"].as_array().unwrap() {
            let event = case["event"].clone();
            assert!(
                validate("member-administration", &event, None).is_err(),
                "{}",
                case["name"]
            );
            let error = invoke("relay_channel_sign", event).unwrap_err();
            assert!(!error.to_string().contains("not allowed"), "{error}");
        }
    }
}
