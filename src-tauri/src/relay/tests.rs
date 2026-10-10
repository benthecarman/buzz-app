use super::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[test]
fn routes_cannot_retarget_credentials_or_expand_http_access() {
    for community in [
        "http://relay.test",
        "https://u:p@relay.test",
        "https://relay.test/path",
        "https://relay.test/?q",
        "https://relay.test/#x",
    ] {
        assert!(request_url(community, "/query", "POST").is_err());
    }
    for path in [
        "//other.test/query",
        "/query?target=x",
        "/api/admin",
        "/../query",
        "https://other.test/query",
    ] {
        assert!(request_url("https://relay.test", path, "POST").is_err());
    }
    for path in ["/api/invites", "/gifs/search"] {
        assert!(request_url("https://relay.test", path, "POST").is_ok());
        assert!(request_url("https://relay.test", path, "GET").is_err());
    }
    assert!(request_url("https://relay.test", "/gifs/other", "POST").is_err());
    assert!(request_url("https://relay.test", "/events", "GET").is_err());
    assert_eq!(
        request_url("https://relay.test", "/query", "POST")
            .unwrap()
            .as_str(),
        "https://relay.test/query"
    );
}

fn fixture_server(response: String) -> (Url, std::thread::JoinHandle<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/query", listener.local_addr().unwrap())).unwrap();
    let task = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = socket.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            let text = String::from_utf8_lossy(&bytes);
            if let Some((headers, body)) = text.split_once("\r\n\r\n") {
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .map(|value| value.parse().unwrap())
                    .unwrap_or(0);
                if body.len() == length {
                    let result = (headers.into(), body.into());
                    socket.write_all(response.as_bytes()).unwrap();
                    return result;
                }
            }
        }
    });
    (url, task)
}

fn verify(event: &serde_json::Value) {
    let serialized = serde_json::to_vec(&serde_json::json!([
        0,
        event["pubkey"],
        event["created_at"],
        event["kind"],
        event["tags"],
        event["content"]
    ]))
    .unwrap();
    let hash = Sha256::digest(serialized);
    assert_eq!(event["id"], format!("{hash:x}"));
    let signature: secp256k1::schnorr::Signature = event["sig"].as_str().unwrap().parse().unwrap();
    let public: secp256k1::XOnlyPublicKey = event["pubkey"].as_str().unwrap().parse().unwrap();
    secp256k1::Secp256k1::verification_only()
        .verify_schnorr(&signature, &hash, &public)
        .unwrap();
}

#[test]
fn managed_agent_deletion_is_owner_only_through_existing_ipc() {
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
    let app = mock_builder()
        .manage(IdentityHost::fixture())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    let sign = |event: serde_json::Value| {
        get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: "relay_sign".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body: tauri::ipc::InvokeBody::Json(serde_json::json!({
                    "community": "https://relay.test", "event": event
                })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        )
        .map(|body| body.deserialize::<serde_json::Value>().unwrap())
    };
    let owner = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f";
    let agent = "02".repeat(32);
    let coordinate = format!("30177:{owner}:{agent}");
    let deletion = serde_json::json!({
        "kind": 5, "created_at": 123, "content": "", "tags": [["a", coordinate]]
    });
    for tags in [
        serde_json::json!([["a", coordinate]]),
        serde_json::json!([
            ["a", coordinate],
            ["k", "30177"],
            ["client-id", "unregister"]
        ]),
    ] {
        let mut event = deletion.clone();
        event["tags"] = tags;
        let signed = sign(event.clone()).unwrap();
        assert_eq!(signed["pubkey"], owner);
        for field in ["kind", "created_at", "content", "tags"] {
            assert_eq!(signed[field], event[field]);
        }
        verify(&signed);
    }
    for tags in [
        serde_json::json!([]),
        serde_json::json!([["a"]]),
        serde_json::json!([["a", coordinate, "extra"]]),
        serde_json::json!([["a", format!("30177:{}:{agent}", "a".repeat(64))]]),
        serde_json::json!([["a", format!("30175:{owner}:{agent}")]]),
        serde_json::json!([["a", format!("30177:{owner}:")]]),
        serde_json::json!([["a", format!("30177:{owner}:{}", "A".repeat(64))]]),
        serde_json::json!([["a", format!("30177:{owner}:{agent}:extra")]]),
        serde_json::json!([["a", coordinate], ["a", coordinate]]),
        serde_json::json!([["a", coordinate], ["e", "b".repeat(64)]]),
        serde_json::json!([["a", coordinate], ["h", "channel"]]),
        serde_json::json!([["a", coordinate], ["k", "30175"]]),
        serde_json::json!([["a", coordinate], ["k", "30177"], ["k", "30177"]]),
        serde_json::json!([
            ["a", coordinate],
            ["client-id", "one"],
            ["client-id", "two"]
        ]),
    ] {
        let mut event = deletion.clone();
        event["tags"] = tags;
        assert!(sign(event.clone()).is_err(), "accepted {event}");
    }
    let mut event = deletion;
    event["content"] = serde_json::json!("unexpected content");
    assert!(sign(event).is_err());
}

#[tokio::test]
async fn catalog_signing_admits_maximum_escaped_content_through_ipc() {
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
    let app = mock_builder()
        .manage(IdentityHost::fixture())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    let sign = |event: &EventTemplate| {
        get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: "relay_sign".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body: tauri::ipc::InvokeBody::Json(serde_json::json!({
                    "community": "https://relay.test", "event": event
                })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        )
        .map(|body| body.deserialize::<serde_json::Value>().unwrap())
    };
    // Each `"` is two content bytes (`\"`) and four serialized bytes (`\\\"`).
    let agent = |size: usize| {
        let room = size - r#"{"display_name":"Big","system_prompt":""}"#.len();
        let pad = format!("{}{}", r#"\""#.repeat(room / 2), "a".repeat(room % 2));
        let content = format!(r#"{{"display_name":"Big","system_prompt":"{pad}"}}"#);
        assert_eq!(content.len(), size);
        EventTemplate {
            kind: 30175,
            created_at: 123,
            tags: vec![
                vec!["d".into(), "big".into()],
                vec!["shared".into(), "true".into()],
                vec!["client-id".into(), "a".repeat(36)],
            ],
            content,
        }
    };
    let largest = agent(65_535);
    let signed = sign(&largest).unwrap();
    verify(&signed);
    assert_eq!(signed["content"], largest.content);
    assert!(signed.to_string().len() > 128 * 1024);
    // The ordinary bound would refuse it, so the catalog route is load-bearing.
    assert!(IdentityHost::fixture().sign(largest).await.is_err());
    assert!(sign(&agent(65_536)).is_err());
}

#[test]
fn real_ipc_restores_identity_signs_and_rejects_invalid_requests() {
    // The path resolver reads HOME at runtime. Isolate it in a child rather
    // than changing process-global HOME under the parallel test runner.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args([
        "--exact",
        "relay::tests::isolated_agent_ipc_probe",
        "--nocapture",
    ]);
    #[cfg(unix)]
    {
        let home = tempfile::tempdir().unwrap();
        #[cfg(target_os = "macos")]
        let path = home
            .path()
            .join("Library/Application Support/xyz.block.buzz.app/agents/managed-agents.json");
        #[cfg(target_os = "linux")]
        let path = home
            .path()
            .join(".local/share/xyz.block.buzz.app/agents/managed-agents.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "[]").unwrap();
        child.env("HOME", home.path()).env_remove("XDG_DATA_HOME");
        let output = child.env("BUZZ_AGENT_IPC_PROBE", "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let output = child.env("BUZZ_ARCHIVE_RESTART", "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(target_os = "windows")]
    {
        let output = child.env("BUZZ_AGENT_IPC_PROBE", "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn isolated_agent_ipc_probe() {
    if std::env::var("BUZZ_AGENT_IPC_PROBE").as_deref() != Ok("1") {
        return;
    }
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
    use tauri::Manager;
    let app = mock_builder()
        .manage(IdentityHost::fixture())
        .manage(Uploads::default())
        .manage(Spools::new(Ok(
            std::env::temp_dir().join(format!("buzz-spool-ipc-{}", uuid::Uuid::new_v4()))
        )))
        .manage(crate::archive::ArchiveHost::default())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    let invoke = |cmd: &str, body: serde_json::Value| {
        get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: cmd.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body: tauri::ipc::InvokeBody::Json(body),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        )
        .map(|body| body.deserialize::<serde_json::Value>().unwrap())
    };
    // Empty uploads fail before allocating storage or networking.
    assert_eq!(
        invoke(
            "relay_upload_begin",
            serde_json::json!({"id":"empty", "size":0})
        )
        .unwrap_err(),
        serde_json::json!("File exceeds the supported upload limit")
    );
    invoke(
        "relay_upload_begin",
        serde_json::json!({"id":"chunked", "size":3}),
    )
    .unwrap();
    let mut headers = tauri::http::HeaderMap::new();
    headers.insert("x-buzz-upload-id", "chunked".parse().unwrap());
    headers.insert("x-buzz-upload-offset", "0".parse().unwrap());
    get_ipc_response(
        &view,
        tauri::webview::InvokeRequest {
            cmd: "relay_upload_chunk".into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: view.url().unwrap(),
            body: tauri::ipc::InvokeBody::Raw(b"abc".to_vec()),
            headers: headers.clone(),
            invoke_key: INVOKE_KEY.into(),
        },
    )
    .unwrap();
    headers.insert("x-buzz-community", "https://relay.test".parse().unwrap());
    // Fixed-mode rejection proves finalization/cleanup without an external server.
    headers.insert("x-buzz-preparation", "video:not-allowed".parse().unwrap());
    let response = get_ipc_response(
        &view,
        tauri::webview::InvokeRequest {
            cmd: "relay_upload".into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: view.url().unwrap(),
            body: tauri::ipc::InvokeBody::Json(serde_json::json!({})),
            headers,
            invoke_key: INVOKE_KEY.into(),
        },
    )
    .unwrap()
    .deserialize::<serde_json::Value>()
    .unwrap();
    assert_eq!(response["body"], r#"{"code":"video"}"#);
    assert!(app.state::<Uploads>().is_empty());
    invoke(
        "relay_upload_begin",
        serde_json::json!({"id":"chunked", "size":3}),
    )
    .unwrap();
    invoke("relay_upload_cancel", serde_json::json!({"id":"chunked"})).unwrap();
    let public = invoke("identity_restore", serde_json::json!({})).unwrap();
    #[cfg(unix)]
    {
        let archive = |request| {
            invoke(
                "relay_archive",
                serde_json::json!({
                    "community":"https://relay.test", "viewer":public, "request":request
                }),
            )
        };
        let settings = archive(serde_json::json!({"action":"settings"})).unwrap();
        let path = std::path::PathBuf::from(settings["path"].as_str().unwrap());
        assert!(path.starts_with(std::env::var("HOME").unwrap()));
        if std::env::var("BUZZ_ARCHIVE_RESTART").as_deref() != Ok("1") {
            for kind in [24200, 44200] {
                let event = crate::archive::tests::envelope(public.as_str().unwrap(), kind, 0);
                for _ in 0..2 {
                    let saved = archive(serde_json::json!({"action":"ingest","event":event,"revision":settings["revision"]})).unwrap();
                    assert_eq!(saved, serde_json::json!({"saved":true}));
                }
            }
        }
        for kind in [24200, 44200] {
            let fresh = crate::archive::tests::envelope(public.as_str().unwrap(), kind, 2);
            let stale = crate::archive::tests::envelope_at(
                public.as_str().unwrap(),
                kind,
                3,
                fresh.created_at - 301,
            );
            let wrong_viewer = crate::archive::tests::envelope(&fresh.pubkey, kind, 4);
            for invalid in [stale, wrong_viewer] {
                assert!(archive(serde_json::json!({"action":"ingest","event":invalid,"revision":settings["revision"]})).is_err());
            }
            let page = archive(serde_json::json!({"action":"read","kind":kind})).unwrap();
            assert_eq!(page["records"].as_array().unwrap().len(), 1);
            assert!(page["records"][0]["plaintext"]
                .as_str()
                .unwrap()
                .contains("archive-secret-marker"));
            assert_eq!(page["skipped"], 0);
        }
        let stored = archive(serde_json::json!({"action":"read","kind":24200})).unwrap();
        let event_id = stored["records"][0]["id"].as_str().unwrap().as_bytes();
        let mut found_event = false;
        for file in std::fs::read_dir(path.parent().unwrap()).unwrap() {
            let raw = std::fs::read(file.unwrap().path()).unwrap();
            found_event |= raw.windows(event_id.len()).any(|bytes| bytes == event_id);
            assert!(!raw
                .windows(b"archive-secret-marker".len())
                .any(|s| s == b"archive-secret-marker"));
            assert!(!raw.windows(b"channelId".len()).any(|s| s == b"channelId"));
        }
        assert!(found_event, "ciphertext scan must include a saved envelope");
        let isolated = invoke("relay_archive",serde_json::json!({"community":"https://other.test","viewer":public,"request":{"action":"read","kind":24200}})).unwrap();
        assert!(isolated["records"].as_array().unwrap().is_empty());
        if std::env::var("BUZZ_ARCHIVE_RESTART").as_deref() == Ok("1") {
            archive(serde_json::json!({"action":"clear","kind":24200})).unwrap();
            assert!(
                archive(serde_json::json!({"action":"read","kind":24200})).unwrap()["records"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                archive(serde_json::json!({"action":"read","kind":44200})).unwrap()["records"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    let event = invoke(
        "relay_sign",
        serde_json::json!({
            "community": "https://relay.test", "event": {
                "kind": 9, "created_at": 123, "tags": [["h", "channel"]], "content": "IPC message"
            }
        }),
    )
    .unwrap();
    assert_eq!(event["pubkey"], public);
    verify(&event);

    let repository = format!("https://relay.test/git/{}/plugins", "a".repeat(64));
    let token = invoke(
        "relay_git_authorization",
        serde_json::json!({"community": "https://relay.test", "repository": repository}),
    )
    .unwrap();
    let auth: serde_json::Value =
        serde_json::from_slice(&STANDARD.decode(token.as_str().unwrap()).unwrap()).unwrap();
    verify(&auth);
    assert_eq!(auth["pubkey"], public);
    assert_eq!(auth["kind"], 27235);
    assert_eq!(
        auth["tags"],
        serde_json::json!([["u", repository], ["method", "GET"]])
    );
    assert!(invoke(
        "relay_git_authorization",
        serde_json::json!({"community": "https://relay.test", "repository": "https://other.test/git/x/y"}),
    )
    .is_err());

    // The old direct attestation IPC must be absent, not merely unused by the UI.
    assert!(invoke(
        "relay_agent_authorize",
        serde_json::json!({
            "community": "https://relay.test",
            "target": {"owner": public, "pubkey": "02".repeat(32)}
        }),
    )
    .is_err());
    let resolved = invoke("relay_agent_resolve", serde_json::json!({
        "community": "https://relay.test", "target": {"owner": public, "pubkey": "02".repeat(32), "confirmed": true}
    })).unwrap();
    assert_eq!(resolved["relayUrl"], "wss://relay.test");
    #[cfg(unix)]
    {
        let library = invoke("relay_agent_library", serde_json::json!({})).unwrap();
        assert_eq!(
            library,
            serde_json::json!({"definitions": [], "identities": []})
        );
        assert!(app
            .path()
            .data_dir()
            .unwrap()
            .starts_with(std::env::var("HOME").unwrap()));
    }
    // Library IPC success is exercised against an isolated HOME on Unix.
    // Windows known-folder inventory is not isolated here; do not invoke its
    // reader until a test-only fixture can control that path.
    // Each must reach the command: a handler refusal is fine; an ACL refusal is not.
    for (command, input) in [
        (
            "relay_archive",
            serde_json::json!({"community":"https://relay.test", "viewer":"bad", "request":{"action":"settings"}}),
        ),
        (
            "relay_agent_memories_read",
            serde_json::json!({"community": "https://relay.test", "agent": public}),
        ),
        (
            "relay_agent_observer",
            serde_json::json!({"community": "https://relay.test", "event": {"id": "bad"}}),
        ),
        (
            "relay_agent_log_proof",
            serde_json::json!({"community": "https://relay.test", "target": {"id": "bad", "pubkey": public, "relayUrl": "wss://relay.test", "nonce": "bad"}}),
        ),
    ] {
        let result = invoke(command, input);
        assert!(
            !format!("{result:?}").contains(&format!("{command} not allowed")),
            "ACL blocked {command}"
        );
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let read = invoke(
        "relay_sign_read_state",
        serde_json::json!({
            "community": "https://relay.test", "intent": {"slot": "a".repeat(32), "createdAt": now,
                "blob": {"v": 1, "client_id": "fixture", "contexts": {"channel": now}}}
        }),
    )
    .unwrap();
    verify(&read);
    let decoded = invoke(
        "relay_decode_read_state",
        serde_json::json!({
            "community": "https://relay.test", "events": [read.clone()]
        }),
    )
    .unwrap();
    assert_eq!(decoded[0]["eventId"], read["id"]);
    assert!(invoke(
        "relay_publish_read_state",
        serde_json::json!({
            "community": "https://relay.test", "event": event
        })
    )
    .is_err());
    assert!(invoke("relay_http", serde_json::json!({
        "community": "https://relay.test", "path": "//other.test/query", "method": "POST", "body": "[]"
    })).is_err());
    assert!(invoke("relay_sign", serde_json::json!({
        "community": "https://relay.test", "event": {
            "kind": 22242, "created_at": 123, "tags": [["relay", "wss://other.test"], ["challenge", "nonce"]], "content": ""
        }
    })).is_err());
    let id = "11111111-1111-4111-8111-111111111111";
    assert!(invoke(
        "relay_sign",
        serde_json::json!({
            "community": "https://relay.test", "event": {
                "kind": 5, "created_at": 123,
                "tags": [["h", id], ["a", format!("30620:{}:{id}", "a".repeat(64))]], "content": ""
            }
        })
    )
    .is_err());
    let deletion = invoke("relay_sign", serde_json::json!({
        "community": "https://relay.test", "event": {
            "kind": 5, "created_at": 123,
            "tags": [["h", id], ["a", format!("30620:{}:{id}", public.as_str().unwrap())]], "content": ""
        }
    })).unwrap();
    verify(&deletion);
    assert!(invoke(
        "relay_workflow_runs",
        serde_json::json!({
            "community": "https://relay.test", "id": "../query", "cursor": null
        })
    )
    .unwrap_err()
    .to_string()
    .contains("Invalid workflow read"));
    assert!(invoke(
        "relay_project_git",
        serde_json::json!({
            "community": "https://relay.test",
            "id": "11111111-1111-4111-8111-111111111111",
            "read": { "owner": "a".repeat(64), "dtag": "../query" }
        })
    )
    .unwrap_err()
    .to_string()
    .contains("Invalid Git read"));
}

#[test]
fn managed_agent_registration_signs_as_owner_through_existing_ipc() {
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
    let app = mock_builder()
        .manage(IdentityHost::fixture())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    let invoke = |cmd: &str, body: serde_json::Value| {
        get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: cmd.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body: tauri::ipc::InvokeBody::Json(body),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        )
        .map(|body| body.deserialize::<serde_json::Value>().unwrap())
    };
    let public = invoke("identity_restore", serde_json::json!({})).unwrap();
    let registration = serde_json::json!({
        "kind": 30177, "created_at": 123, "tags": [["d", "02".repeat(32)]],
        "content": r#"{"name":"Remote agent","parallelism":1,"respond_to":"owner-only"}"#
    });
    let registered = invoke(
        "relay_sign",
        serde_json::json!({
            "community": "https://relay.test", "event": registration
        }),
    )
    .unwrap();
    assert_eq!(registered["pubkey"], public);
    for field in ["kind", "created_at", "tags", "content"] {
        assert_eq!(registered[field], registration[field]);
    }
    verify(&registered);
}

#[tokio::test]
async fn signing_is_verifiable_and_does_not_export_a_key() {
    let event = IdentityHost::fixture()
        .sign(EventTemplate {
            kind: 9,
            created_at: 123,
            tags: vec![vec!["h".into(), "channel".into()]],
            content: "Hello\nfrom native".into(),
        })
        .await
        .unwrap();
    verify(&event);
    assert_eq!(
        event["pubkey"],
        "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
    );
    assert!(!event.to_string().contains("nsec"));
    assert!(IdentityHost::default()
        .sign(EventTemplate {
            kind: 9,
            created_at: 0,
            tags: vec![],
            content: "".into()
        })
        .await
        .is_err());
}

#[test]
fn workflow_history_is_fixed_and_cannot_retarget_native_http() {
    let id = "11111111-1111-4111-8111-111111111111";
    let cursor = WorkflowCursor {
        before: "2026-09-29T20:00:00Z".into(),
        before_id: id.into(),
    };
    assert_eq!(workflow_runs_url("https://relay.test", id, Some(&cursor)).unwrap().as_str(),
        "https://relay.test/workflows/11111111-1111-4111-8111-111111111111/runs?limit=20&before=2026-09-29T20%3A00%3A00Z&before_id=11111111-1111-4111-8111-111111111111");
    for invalid in [
        "../query",
        "11111111-1111-4111-8111-111111111111?target=x",
        "11111111-1111-4111-8111-111111111111/../query",
    ] {
        assert!(workflow_runs_url("https://relay.test", invalid, None).is_err());
    }
    assert!(workflow_runs_url(
        "https://relay.test",
        id,
        Some(&WorkflowCursor {
            before: "2026-09-29T20:00:00Z&target=x".into(),
            before_id: id.into()
        })
    )
    .is_err());
    assert!(request_url(
        "https://relay.test",
        "/workflows/11111111-1111-4111-8111-111111111111/runs",
        "GET"
    )
    .is_err());
}

#[tokio::test]
async fn workflow_get_is_authenticated_without_payload_and_never_redirects() {
    let (mut url, task) = fixture_server("HTTP/1.1 302 Found\r\nLocation: https://other.test/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into());
    url.set_path("/workflows/11111111-1111-4111-8111-111111111111/runs");
    url.set_query(Some("limit=20"));
    let response = send(
        &IdentityHost::fixture(),
        url.clone(),
        "GET",
        None,
        true,
        1024 * 1024,
    )
    .await
    .unwrap();
    assert_eq!(response.status, 302);
    let (headers, body) = task.join().unwrap();
    assert!(headers
        .starts_with("GET /workflows/11111111-1111-4111-8111-111111111111/runs?limit=20 HTTP/1.1"));
    assert!(body.is_empty());
    let encoded = headers
        .lines()
        .find_map(|line| line.strip_prefix("authorization: Nostr "))
        .unwrap();
    let event: serde_json::Value =
        serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
    assert_eq!(event["tags"][0], serde_json::json!(["u", url.as_str()]));
    assert_eq!(event["tags"][1], serde_json::json!(["method", "GET"]));
    assert_eq!(event["tags"].as_array().unwrap().len(), 3);
    verify(&event);
}

#[test]
fn native_write_commands_reach_handlers_through_production_ipc() {
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
    let app = mock_builder()
        .manage(IdentityHost::fixture())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    let requests = [
        (
            "relay_channel_sign",
            serde_json::json!({"community":"https://relay.test","route":"channel-lifecycle","event":{"kind":9002,"created_at":1700000010,"content":"","tags":[["h","11111111-1111-4111-8111-111111111111"],["archived","true"]]}}),
        ),
        (
            "relay_kit_prepare",
            serde_json::json!({"community":"https://relay.test","record":{"version":1,"community":"https://relay.test","deleted":false,"value":{"type":"team","id":"mine","name":"Mine","agents":[]}}}),
        ),
        (
            "relay_kit_decode",
            serde_json::json!({"community":"https://relay.test","events":[]}),
        ),
        (
            "relay_kit_sign",
            serde_json::json!({"community":"invalid","event":{"kind":30078,"created_at":1,"content":"","tags":[]}}),
        ),
        (
            "relay_channel_publish",
            serde_json::json!({"community":"invalid","route":"channel-lifecycle","event":{}}),
        ),
        (
            "relay_direct_message",
            serde_json::json!({"community":"invalid","pubkeys":[]}),
        ),
    ];
    for (command, body) in requests {
        let result = get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: command.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body: tauri::ipc::InvokeBody::Json(body),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        );
        if let Err(error) = result {
            assert!(
                !error.to_string().contains("not allowed"),
                "{command} blocked by ACL: {error}"
            );
        }
    }
}

#[test]
fn channel_commands_sign_archive_and_unarchive_and_reject_malformed_tags_through_ipc() {
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
    let app = mock_builder()
        .manage(IdentityHost::fixture())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    let invoke = |tags: Vec<Vec<String>>| {
        get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: "relay_channel_sign".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body: tauri::ipc::InvokeBody::Json(serde_json::json!({
                    "community": "https://relay.test", "route": "channel-lifecycle",
                    "event": { "kind": 9002, "created_at": 123, "content": "", "tags": tags }
                })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        )
    };
    let h = vec!["h".into(), uuid::Uuid::nil().to_string()];
    let archived = vec!["archived".into(), "true".into()];
    for value in ["true", "false"] {
        let tags = vec![h.clone(), vec!["archived".into(), value.into()]];
        let event: serde_json::Value = invoke(tags.clone()).unwrap().deserialize().unwrap();
        verify(&event);
        assert_eq!(event["tags"], serde_json::json!(tags));
        assert_eq!(event["kind"], 9002);
        assert_eq!(event["content"], "");
    }
    for tags in [
        vec![vec![], archived.clone()],
        vec![vec!["h".into()], archived.clone()],
        vec![h.clone(), vec!["archived".into()]],
        vec![h.clone(), vec!["archived".into(), "invalid".into()]],
        vec![h.clone(), h.clone()],
        vec![archived.clone(), h.clone()],
    ] {
        let error = match invoke(tags) {
            Ok(_) => panic!("invalid tag must reject"),
            Err(error) => error,
        };
        assert!(
            !error.to_string().contains("not allowed"),
            "ACL blocked command: {error}"
        );
    }
}

#[test]
fn creation_rejects_truncated_tags_through_existing_ipc() {
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
    let app = mock_builder()
        .manage(IdentityHost::fixture())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    for tag in [vec![], vec!["h"]] {
        let response = get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: "relay_sign".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body: tauri::ipc::InvokeBody::Json(serde_json::json!({
                    "community": "https://relay.test",
                    "event": { "kind": 9007, "created_at": 123, "content": "", "tags": [
                        tag, ["name", "Team"], ["visibility", "private"], ["channel_type", "stream"]
                    ] }
                })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        );
        let error = match response {
            Ok(_) => panic!("malformed creation must reject before discovery"),
            Err(error) => error,
        };
        assert!(
            !error.to_string().contains("not allowed"),
            "ACL blocked command: {error}"
        );
    }
}

#[tokio::test]
async fn discovery_body_is_bounded_for_length_and_chunked_transfer() {
    for chunked in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let task = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 2048];
            loop {
                let count = socket.read(&mut buffer).unwrap();
                assert!(count > 0, "request ended before headers completed");
                bytes.extend_from_slice(&buffer[..count]);
                assert!(
                    bytes.len() <= 16 * 1024,
                    "request headers exceeded fixture limit"
                );
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            if chunked {
                socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
                // Valid chunked response, over the budget on the first chunk.
                socket
                    .write_all(format!("{:X}\r\n", MAX_BODY + 1).as_bytes())
                    .unwrap();
                socket.write_all(&vec![b'x'; MAX_BODY + 1]).unwrap();
                let _ = socket.write_all(b"\r\n0\r\n\r\n");
            } else {
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            MAX_BODY + 1
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                // Headers alone must suffice: do not wait for the advertised body.
            }
        });
        let mut response = client().unwrap().get(url).send().await.unwrap();
        assert_eq!(
            read_bounded(&mut response, MAX_BODY, "interrupted", "oversized")
                .await
                .unwrap_err(),
            "oversized"
        );
        task.join().unwrap();
    }
}

#[tokio::test]
async fn sidebar_ipc_only_decodes_verified_self_coordinates_and_signs_valid_payloads() {
    let host = IdentityHost::fixture();
    let payload =
        serde_json::json!({"version":1,"channels":{"channel":{"starred":true,"updatedAt":1}}});
    let event = host
        .sign_sidebar("channel-stars".into(), payload.clone(), 1)
        .await
        .unwrap();
    verify(&event);
    assert_eq!(
        host.decode_sidebar(vec![event.clone()]).await.unwrap()["channel-stars"],
        payload
    );
    assert!(host
        .decode_sidebar(vec![event.clone(), event.clone()])
        .await
        .is_err());
    assert!(host.decode_sidebar(vec![event.clone(); 5]).await.is_err());
    let mut tampered = event.clone();
    tampered["content"] = serde_json::json!("changed");
    assert!(host.decode_sidebar(vec![tampered]).await.is_err());
    let mut wrong_coordinate = event.clone();
    wrong_coordinate["tags"][0][1] = serde_json::json!("unknown");
    assert!(host.decode_sidebar(vec![wrong_coordinate]).await.is_err());
    assert!(host
        .sign_sidebar("other".into(), payload.clone(), 1)
        .await
        .is_err());
    assert!(host.sign_sidebar("channel-stars".into(), serde_json::json!({"version":1,"channels":{"c":{"starred":"not boolean","updatedAt":1}}}), 1).await.is_err());
    let other = IdentityHost::fixture()
        .sign_sidebar(
            "channel-sort".into(),
            serde_json::json!({"version":1,"groups":{}}),
            1,
        )
        .await
        .unwrap();
    assert_eq!(
        host.decode_sidebar(vec![other]).await.unwrap()["channel-sort"]["version"],
        1
    );
}

#[tokio::test]
async fn event_writer_admits_sidebar_records_and_recipes_by_their_own_coordinate() {
    let host = IdentityHost::fixture();
    let community = "https://relay.test";
    for (coordinate, payload) in [
        (
            "channel-sections",
            serde_json::json!({"version":1,"sections":[{"id":"group","name":"Group","order":0}],"assignments":{"channel":"group"}}),
        ),
        (
            "channel-stars",
            serde_json::json!({"version":1,"channels":{"channel":{"starred":true,"updatedAt":1}}}),
        ),
        (
            "channel-mutes",
            serde_json::json!({"version":1,"channels":{"channel":{"muted":true,"updatedAt":1}}}),
        ),
        (
            "channel-sort",
            serde_json::json!({"version":1,"groups":{"channels":"recent"}}),
        ),
    ] {
        let event = host
            .sign_sidebar(coordinate.into(), payload, 1)
            .await
            .unwrap();
        admit_app_data(&host, &event, community).await.unwrap();
    }
    let sidebar = host
        .sign_sidebar(
            "channel-sort".into(),
            serde_json::json!({"version":1,"groups":{}}),
            1,
        )
        .await
        .unwrap();
    let mut tampered = sidebar.clone();
    tampered["content"] = serde_json::json!("changed");
    assert!(admit_app_data(&host, &tampered, community).await.is_err());
    let mut foreign = sidebar.clone();
    foreign["pubkey"] = serde_json::json!("02".repeat(32));
    assert!(admit_app_data(&host, &foreign, community).await.is_err());

    // Anything outside the four sidebar coordinates is still held to the recipe contract.
    let sign = |tags: Vec<Vec<String>>, content: String| {
        host.sign(crate::identity::EventTemplate {
            kind: 30078,
            created_at: 1,
            content,
            tags,
        })
    };
    let recipe = serde_json::json!({"version":1,"community":community,"deleted":false,
        "value":{"type":"team","id":"team-one","name":"Team","agents":[]}});
    let ciphertext = host.kit_cipher(recipe.to_string(), true).await.unwrap();
    let coordinate = "buzz-channel-kit-v1:https%3A%2F%2Frelay.test:team:team-one";
    let kit_tags = |d: &str| {
        vec![
            vec!["d".to_owned(), d.to_owned()],
            vec!["t".to_owned(), "buzz-channel-kit-v1".to_owned()],
        ]
    };
    let valid = sign(kit_tags(coordinate), ciphertext.clone())
        .await
        .unwrap();
    admit_app_data(&host, &valid, community).await.unwrap();
    // A recipe cannot borrow a sidebar coordinate, and a sidebar record cannot borrow another.
    for d in ["channel-sections", "read-state:other", "unknown"] {
        let event = sign(kit_tags(d), ciphertext.clone()).await.unwrap();
        assert!(admit_app_data(&host, &event, community).await.is_err());
    }
    let mut duplicate = kit_tags(coordinate);
    duplicate.push(vec!["d".to_owned(), "channel-stars".to_owned()]);
    let event = sign(duplicate, ciphertext).await.unwrap();
    assert!(admit_app_data(&host, &event, community).await.is_err());
}

#[tokio::test]
async fn sidebar_decoder_interoperates_with_nostr_tools_nip44_v2() {
    // Produced with nostr-tools 2.25.2, private key [1; 32].
    let event: serde_json::Value = serde_json::from_str(r#"{"kind":30078,"created_at":1700000000,"tags":[["d","channel-mutes"],["t","channel-mutes"]],"content":"Ajhdtq+PsGhpLGhyo0hAN55quGVmOE8/p0id4UVmJPz/Aki5aZUHWBymErqORblF9uPjX6XD5DjFJDR18qIHIIullzAvPKE5z336CV5caxsevvNkXoeRk7U0xVpk+piVfM9z2+cgyuJoG3cGMzFp78/53XHDvsEUgtE9Wv8kgvj5vQc=","pubkey":"1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f","id":"e4471969b8f1b27fad968343db8deb4346c3e530c69b548f68a6adb31f99628c","sig":"14064569d6085363f32865b2204037c4a5769a09f116209fda3790feef12e6672d1806c7626c6857d0696cfe43f39ed4a8dc13d965989300ec757fb3a2fd44cd"}"#).unwrap();
    assert_eq!(
        IdentityHost::fixture()
            .decode_sidebar(vec![event])
            .await
            .unwrap()["channel-mutes"],
        serde_json::json!({"version":1,"channels":{"cross":{"muted":true,"updatedAt":1}}})
    );
}

#[tokio::test]
async fn sidebar_signer_matches_projection_lengths_and_preserves_unknown_sort_entries() {
    let host = IdentityHost::fixture();
    let unicode = "界".repeat(120);
    let groups = serde_json::json!({"version":1,"sections":[{"id":"group","name":unicode,"order":0}],"assignments":{"c":"group"}});
    let event = host
        .sign_sidebar("channel-sections".into(), groups.clone(), 1)
        .await
        .unwrap();
    assert_eq!(
        host.decode_sidebar(vec![event]).await.unwrap()["channel-sections"],
        groups
    );
    // A valid head from another client must remain writable after a native move.
    let mut existing = groups.clone();
    existing["assignments"]["other"] = serde_json::json!("group");
    let event = host
        .sign_sidebar("channel-sections".into(), existing.clone(), 2)
        .await
        .unwrap();
    assert_eq!(
        host.decode_sidebar(vec![event]).await.unwrap()["channel-sections"],
        existing
    );
    // Same preservation vector as browser-host/sidebar-sort.test.mjs: unrelated modes,
    // section keys and top-level metadata survive an override update.
    let sort = serde_json::json!({"version":1,"future":{"x":1},"groups":{
        "channels":"recent","section:elsewhere":"recent","future":"next-mode","section:work":"recent"
    }});
    let event = host
        .sign_sidebar("channel-sort".into(), sort.clone(), 1)
        .await
        .unwrap();
    assert_eq!(
        host.decode_sidebar(vec![event]).await.unwrap()["channel-sort"],
        sort
    );
    assert!(host.sign_sidebar("channel-sections".into(), serde_json::json!({"version":1,"sections":[{"id":"group","name":"界".repeat(257),"order":0}],"assignments":{}}), 1).await.is_err());
}

#[tokio::test]
async fn sidebar_signer_supports_large_records_without_expanding_general_signing() {
    let host = IdentityHost::fixture();
    let channels: serde_json::Map<String, serde_json::Value> = (0..500)
        .map(|i| {
            (
                format!("{i:08x}-1234-1234-1234-123456789abc"),
                serde_json::json!({"starred":true,"updatedAt":1700000000000_u64}),
            )
        })
        .collect();
    let event = host
        .sign_sidebar(
            "channel-stars".into(),
            serde_json::json!({"version":1,"channels":channels}),
            1,
        )
        .await
        .unwrap();
    verify(&event);
    assert!(event["content"].as_str().unwrap().len() > 64 * 1024);
    assert_eq!(
        host.decode_sidebar(vec![event]).await.unwrap()["channel-stars"]["channels"]
            .as_object()
            .unwrap()
            .len(),
        500
    );
    let muted: serde_json::Map<String, serde_json::Value> = (0..500)
        .map(|i| {
            (
                format!("{i:08x}-1234-1234-1234-123456789abc"),
                serde_json::json!({"muted":i % 2 == 0,"updatedAt":1700000000000_u64}),
            )
        })
        .collect();
    let event = host
        .sign_sidebar(
            "channel-mutes".into(),
            serde_json::json!({"version":1,"channels":muted}),
            1,
        )
        .await
        .unwrap();
    verify(&event);
    assert_eq!(
        host.decode_sidebar(vec![event]).await.unwrap()["channel-mutes"]["channels"]
            .as_object()
            .unwrap()
            .len(),
        500
    );
    let assignments: serde_json::Map<String, serde_json::Value> = (0..1000)
        .map(|i| {
            (
                format!("{i:08x}-1234-1234-1234-123456789abc"),
                serde_json::json!("group"),
            )
        })
        .collect();
    let event = host.sign_sidebar("channel-sections".into(), serde_json::json!({
        "version":1,"sections":[{"id":"group","name":"Work","order":0}],"assignments":assignments
    }), 1).await.unwrap();
    verify(&event);
    assert_eq!(
        host.decode_sidebar(vec![event]).await.unwrap()["channel-sections"]["assignments"]
            .as_object()
            .unwrap()
            .len(),
        1000
    );
    assert!(host
        .sign(EventTemplate {
            kind: 9,
            created_at: 1,
            tags: vec![],
            content: "x".repeat(65_536)
        })
        .await
        .is_err());
}

#[tokio::test]
async fn sidebar_signer_and_decoder_match_broker_plaintext_boundaries() {
    let host = IdentityHost::fixture();
    // Unknown string sort modes are preserved by both clients; use one to place
    // actual JSON exactly on each wire-format and application budget boundary.
    let prefix = r#"{"groups":{"future":""#;
    let suffix = r#""},"version":1}"#;
    for size in [65_408, 65_409, 65_535, 65_536, 128 * 1024] {
        let filler = "x".repeat(size - prefix.len() - suffix.len());
        let payload = serde_json::json!({"version":1,"groups":{"future":filler}});
        assert_eq!(serde_json::to_string(&payload).unwrap().len(), size);
        let event = host
            .sign_sidebar("channel-sort".into(), payload.clone(), 1)
            .await
            .unwrap();
        verify(&event);
        assert_eq!(
            host.decode_sidebar(vec![event]).await.unwrap()["channel-sort"],
            payload
        );
    }
    let oversized = serde_json::json!({
        "version":1,"groups":{"future":"x".repeat(128 * 1024 - prefix.len() - suffix.len() + 1)}
    });
    assert_eq!(
        serde_json::to_string(&oversized).unwrap().len(),
        128 * 1024 + 1
    );
    assert_eq!(
        host.sign_sidebar("channel-sort".into(), oversized, 1)
            .await
            .unwrap_err(),
        "Sidebar plaintext budget exceeded"
    );
}

#[tokio::test]
async fn sidebar_decoder_accepts_broker_extended_length_sections_alongside_other_preferences() {
    let host = IdentityHost::fixture();
    let section = "12345678-1234-1234-1234-123456789abc";
    let assignments: serde_json::Map<String, serde_json::Value> = (0..1000)
        .map(|i| {
            (
                format!("{i:08x}-1234-1234-1234-123456789abc"),
                serde_json::json!(section),
            )
        })
        .collect();
    let sections = serde_json::json!({"version":1,"sections":[{"id":section,"name":"Work","order":0}],"assignments":assignments});
    assert!(serde_json::to_vec(&sections).unwrap().len() > 65_535);
    let mut events = vec![host
        .sign_sidebar("channel-sections".into(), sections.clone(), 1)
        .await
        .unwrap()];
    for coordinate in ["channel-stars", "channel-mutes", "channel-sort"] {
        let payload = if coordinate == "channel-sort" {
            serde_json::json!({"version":1,"groups":{}})
        } else {
            serde_json::json!({"version":1,"channels":{}})
        };
        events.push(
            host.sign_sidebar(coordinate.into(), payload, 1)
                .await
                .unwrap(),
        );
    }
    let decoded = host.decode_sidebar(events).await.unwrap();
    assert_eq!(decoded["channel-sections"], sections);
    assert_eq!(decoded["channel-stars"]["version"], 1);
    assert_eq!(decoded["channel-mutes"]["version"], 1);
    assert_eq!(decoded["channel-sort"]["version"], 1);
}

#[tokio::test]
async fn sidebar_decoder_accepts_four_maximum_plaintext_records_and_bounds_total_upload() {
    let host = IdentityHost::fixture();
    let mut events = Vec::new();
    for coordinate in [
        "channel-sort",
        "channel-sections",
        "channel-stars",
        "channel-mutes",
    ] {
        // Preserved top-level data can fill the plaintext budget without
        // bypassing each coordinate's validated schema or the signer boundary.
        let mut value = match coordinate {
            "channel-sort" => serde_json::json!({"version":1,"groups":{},"future":""}),
            "channel-sections" => {
                serde_json::json!({"version":1,"sections":[],"assignments":{},"future":""})
            }
            _ => serde_json::json!({"version":1,"channels":{},"future":""}),
        };
        let overhead = serde_json::to_vec(&value).unwrap().len();
        value["future"] = serde_json::json!("x".repeat(128 * 1024 - overhead));
        assert_eq!(serde_json::to_vec(&value).unwrap().len(), 128 * 1024);
        events.push(
            host.sign_sidebar(coordinate.into(), value, 1)
                .await
                .unwrap(),
        );
    }
    let request_len = serde_json::to_vec(&events).unwrap().len();
    assert!(request_len > 512 * 1024 && request_len < 768 * 1024);
    assert_eq!(
        host.decode_sidebar(events)
            .await
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        4
    );
}

#[tokio::test]
async fn sidebar_decoder_loads_four_populated_bounded_coordinates() {
    let host = IdentityHost::fixture();
    let section = "00000000-1234-1234-1234-123456789abc";
    let assignments: serde_json::Map<String, serde_json::Value> = (0..1000)
        .map(|i| {
            (
                format!("{i:08x}-1234-1234-1234-123456789abc"),
                serde_json::json!(section),
            )
        })
        .collect();
    let sections: serde_json::Value =
        serde_json::json!({"version":1,"sections":[],"assignments":assignments});
    let named_sections: Vec<_> = (0..100)
        .map(|i| serde_json::json!({"id":format!("{i:08x}-1234-1234-1234-123456789abc"),"name":"N".repeat(198),"order":i}))
        .collect();
    let mut sections = sections;
    sections["sections"] = serde_json::json!(named_sections);
    let mut events = vec![host
        .sign_sidebar("channel-sections".into(), sections.clone(), 1)
        .await
        .unwrap()];
    for (coordinate, field) in [("channel-stars", "starred"), ("channel-mutes", "muted")] {
        let channels: serde_json::Map<String, serde_json::Value> = (0..500)
            .map(|i| {
                (
                    format!("{i:08x}-5678-1234-1234-123456789abc"),
                    serde_json::json!({field: true, "updatedAt": 1_700_000_000_000_u64}),
                )
            })
            .collect();
        let event = host
            .sign_sidebar(
                coordinate.into(),
                serde_json::json!({"version":1,"channels":channels}),
                1,
            )
            .await
            .unwrap();
        assert_eq!(
            host.decode_sidebar(vec![event.clone()]).await.unwrap()[coordinate]["channels"]
                .as_object()
                .unwrap()
                .len(),
            500
        );
        events.push(event);
    }
    let sort = serde_json::json!({"version":1,"groups":{"channels":"recent"}});
    events.push(
        host.sign_sidebar("channel-sort".into(), sort.clone(), 1)
            .await
            .unwrap(),
    );
    let request_bytes = serde_json::to_vec(&events).unwrap().len();
    assert!(
        request_bytes > 256 * 1024,
        "fixture must cross old aggregate budget: {request_bytes}"
    );
    let decoded = host.decode_sidebar(events).await.unwrap();
    assert_eq!(decoded["channel-sections"], sections);
    assert_eq!(
        decoded["channel-stars"]["channels"]
            .as_object()
            .unwrap()
            .len(),
        500
    );
    assert_eq!(
        decoded["channel-mutes"]["channels"]
            .as_object()
            .unwrap()
            .len(),
        500
    );
    assert_eq!(decoded["channel-sort"], sort);
}

#[tokio::test]
async fn read_state_codec_round_trips_and_rejects_foreign_intent() {
    let host = IdentityHost::fixture();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let blob = serde_json::json!({"v":1,"client_id":"fixture","contexts":{"channel":now}});
    let signed = host
        .sign_read_state("a".repeat(32), now, blob.clone())
        .await
        .unwrap();
    verify(&signed);
    let decoded = host.decode_read_state(vec![signed.clone()]).await.unwrap();
    assert_eq!(decoded[0]["eventId"], signed["id"]);
    assert_eq!(decoded[0]["blob"], blob);
    assert!(host
        .sign_read_state("x".repeat(32), now, blob.clone())
        .await
        .is_err());
    assert!(host
        .sign_read_state("a".repeat(32), now - 120, blob)
        .await
        .is_err());
    let mut tampered = signed.clone();
    tampered["tags"] = serde_json::json!([["d", "channel-sort"], ["t", "read-state"]]);
    assert!(host.decode_read_state(vec![tampered]).await.is_err());
    let mut duplicate = signed.clone();
    duplicate["tags"] = serde_json::json!([
        ["d", "read-state:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
        ["d", "channel-sort"],
        ["t", "read-state"]
    ]);
    assert!(host.decode_read_state(vec![duplicate]).await.is_err());
    assert!(host.decode_read_state(vec![signed; 17]).await.is_err());
}

#[test]
fn general_signing_never_accepts_read_state_kind() {
    let event = EventTemplate {
        kind: 30078,
        created_at: 0,
        tags: vec![],
        content: String::new(),
    };
    assert!(validate_event("https://relay.test", &event).is_err());
}

#[tokio::test]
#[ignore = "local performance measurement; run with --ignored --nocapture"]
async fn measure_preference_batch_decode() {
    let host = IdentityHost::fixture();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let blob = serde_json::json!({"v":1,"client_id":"fixture","contexts":{"channel":now}});
    let event = host
        .sign_read_state("a".repeat(32), now, blob)
        .await
        .unwrap();
    let events = vec![event; 16];
    host.decode_read_state(events.clone()).await.unwrap();
    let started = Instant::now();
    for _ in 0..100 {
        std::hint::black_box(host.decode_read_state(events.clone()).await.unwrap());
    }
    eprintln!(
        "read-state batch: 16 slots, 100 iterations: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn preference_batches_reject_invalid_ciphertext_after_signature_verification() {
    let host = IdentityHost::fixture();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let sidebar = host
        .sign_sidebar(
            "channel-stars".into(),
            serde_json::json!({"version":1,"channels":{}}),
            now,
        )
        .await
        .unwrap();
    let read = host
        .sign_read_state(
            "a".repeat(32),
            now,
            serde_json::json!({"v":1,"client_id":"fixture","contexts":{"channel":now}}),
        )
        .await
        .unwrap();
    for (event, sidebar_record) in [(sidebar, true), (read, false)] {
        let payload = STANDARD.decode(event["content"].as_str().unwrap()).unwrap();
        let mut version = payload.clone();
        version[0] = 3;
        let mut mac = payload;
        *mac.last_mut().unwrap() ^= 1;
        for content in [
            STANDARD.encode(version),
            STANDARD.encode(mac),
            "invalid base64".into(),
        ] {
            let invalid = host
                .sign(EventTemplate {
                    created_at: now,
                    kind: 30078,
                    tags: serde_json::from_value(event["tags"].clone()).unwrap(),
                    content,
                })
                .await
                .unwrap();
            verify(&invalid);
            let result = if sidebar_record {
                host.decode_sidebar(vec![invalid]).await
            } else {
                host.decode_read_state(vec![invalid]).await
            };
            assert!(result.is_err());
        }
    }
}

#[test]
fn member_commands_sign_through_production_ipc() {
    use tauri::test::{get_ipc_response, mock_builder, INVOKE_KEY};
    let app = mock_builder()
        .manage(IdentityHost::fixture())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    let sign = |event: &serde_json::Value| {
        get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: "relay_sign".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body: tauri::ipc::InvokeBody::Json(serde_json::json!({
                    "community": "https://relay.test", "event": event
                })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.into(),
            },
        )
        .map(|body| body.deserialize::<serde_json::Value>().unwrap())
    };
    let target = "a".repeat(64);
    let command = |kind: u16, tags: serde_json::Value| serde_json::json!({ "kind": kind, "created_at": 1, "content": "", "tags": tags });
    for event in [
        command(9030, serde_json::json!([["p", target], ["role", "member"]])),
        command(9030, serde_json::json!([["p", target], ["role", "admin"]])),
        command(9031, serde_json::json!([["p", target]])),
        command(9032, serde_json::json!([["p", target], ["role", "admin"]])),
        command(9032, serde_json::json!([["p", target], ["role", "member"]])),
    ] {
        let signed = sign(&event).unwrap();
        assert_eq!(
            signed["pubkey"],
            "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
        );
        for field in ["kind", "created_at", "content", "tags"] {
            assert_eq!(signed[field], event[field]);
        }
        verify(&signed);
    }
    // An owner grant is refused by the host before any key use.
    for event in [
        command(9030, serde_json::json!([["p", target], ["role", "owner"]])),
        command(9032, serde_json::json!([["p", target], ["role", "owner"]])),
    ] {
        assert!(sign(&event).is_err(), "signed {event}");
    }
}

#[test]
fn git_authorization_covers_only_this_communitys_repositories() {
    let owner = "a".repeat(64);
    for repository in [
        format!("https://relay.test/git/{owner}/plugins"),
        format!("https://relay.test/git/{owner}/plugins.git"),
        format!("https://relay.test/git/{owner}/plugins."),
        format!("https://relay.test/git/{owner}/plugins..git"),
        format!("https://relay.test/git/{owner}/{}", "n".repeat(64)),
        format!("https://relay.test/git/{owner}/{}.git", "n".repeat(64)),
    ] {
        assert_eq!(
            git_repository("https://relay.test/", &repository)
                .unwrap()
                .as_str(),
            repository
        );
    }
    for repository in [
        format!("https://other.test/git/{owner}/plugins"),
        format!("http://relay.test/git/{owner}/plugins"),
        format!("https://relay.test:444/git/{owner}/plugins"),
        format!("https://me@relay.test/git/{owner}/plugins"),
        format!("https://relay.test/git/{owner}/plugins/"),
        format!("https://relay.test/git/{owner}/plugins?service=git-receive-pack"),
        format!("https://relay.test/git/{owner}/plugins#x"),
        format!("https://relay.test/git/{owner}/.hidden"),
        format!("https://relay.test/git/{owner}/a..b"),
        format!("https://relay.test/git/{owner}/a..b.git"),
        format!("https://relay.test/git/{owner}/.git"),
        format!("https://relay.test/git/{owner}/..git"),
        format!("https://relay.test/git/{owner}/{}", "n".repeat(65)),
        format!("https://relay.test/git/{owner}/{}.git", "n".repeat(65)),
        format!("https://relay.test/git/{}/plugins", "A".repeat(64)),
        format!("https://relay.test/api/{owner}/plugins"),
        format!("https://relay.test/git/{owner}/plugins/info/refs"),
        format!("https://RELAY.test/git/{owner}/plugins"),
        "https://relay.test/query".into(),
    ] {
        assert!(
            git_repository("https://relay.test/", &repository).is_err(),
            "{repository}"
        );
    }
}

#[test]
fn upload_ingress_commands_validate_raw_chunks_and_finalize_owned_bytes() {
    use tauri::{
        test::{get_ipc_response, mock_builder, INVOKE_KEY},
        Manager as _,
    };
    let parent = tempfile::tempdir().unwrap();
    let app = mock_builder()
        .manage(Uploads::default())
        .manage(Spools::new(Ok(parent.path().to_owned())))
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::default())
        .build()
        .unwrap();
    let invoke = |command: &str, body, id: Option<&str>, offset: Option<&str>| {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(id) = id {
            headers.insert("x-buzz-upload-id", id.parse().unwrap());
        }
        if let Some(offset) = offset {
            headers.insert("x-buzz-upload-offset", offset.parse().unwrap());
        }
        get_ipc_response(
            &view,
            tauri::webview::InvokeRequest {
                cmd: command.into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                url: view.url().unwrap(),
                body,
                headers,
                invoke_key: INVOKE_KEY.into(),
            },
        )
    };
    for (id, offset, body) in [
        (
            "missing-offset",
            None,
            tauri::ipc::InvokeBody::Raw(b"abc".to_vec()),
        ),
        (
            "invalid-offset",
            Some("bad"),
            tauri::ipc::InvokeBody::Raw(b"abc".to_vec()),
        ),
        (
            "json",
            Some("0"),
            tauri::ipc::InvokeBody::Json(serde_json::json!([1, 2, 3])),
        ),
        (
            "large",
            Some("0"),
            tauri::ipc::InvokeBody::Raw(vec![0; UPLOAD_CHUNK + 1]),
        ),
        ("empty", Some("0"), tauri::ipc::InvokeBody::Raw(Vec::new())),
    ] {
        invoke(
            "relay_upload_begin",
            tauri::ipc::InvokeBody::Json(serde_json::json!({"id": id, "size": 3})),
            None,
            None,
        )
        .unwrap();
        assert!(invoke("relay_upload_chunk", body, Some(id), offset).is_err());
        assert!(app.state::<Spools>().take(id).is_err());
        assert!(app.state::<Uploads>().is_empty());
    }
    assert!(invoke(
        "relay_upload_chunk",
        tauri::ipc::InvokeBody::Raw(vec![1]),
        None,
        Some("0")
    )
    .is_err());
    invoke(
        "relay_upload_begin",
        tauri::ipc::InvokeBody::Json(serde_json::json!({"id": "valid", "size": 3})),
        None,
        None,
    )
    .unwrap();
    invoke(
        "relay_upload_chunk",
        tauri::ipc::InvokeBody::Raw(b"a".to_vec()),
        Some("valid"),
        Some("0"),
    )
    .unwrap();
    invoke(
        "relay_upload_chunk",
        tauri::ipc::InvokeBody::Raw(b"bc".to_vec()),
        Some("valid"),
        Some("1"),
    )
    .unwrap();
    let (spool, _) = app.state::<Spools>().take("valid").unwrap();
    assert_eq!(std::fs::read(spool.source()).unwrap(), b"abc");
    assert_eq!(spool.hash(), format!("{:x}", Sha256::digest(b"abc")));
    app.state::<Uploads>().finish("valid");
}
