use super::*;
use crate::test_support::*;

#[tokio::test]
async fn native_http_signs_exact_bytes_and_never_follows_redirects() {
    // HTTP is test-transport-only; the IPC boundary always requires HTTPS.
    let (url, task) = fixture_server("HTTP/1.1 302 Found\r\nLocation: https://other.test/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into());
    let body = r#"[{"kinds":[0],"limit":5}]"#;
    let result = send(
        &TestSigner,
        url.clone(),
        "POST",
        Some(body.into()),
        true,
        MAX_RESPONSE,
    )
    .await
    .unwrap();
    assert_eq!(result.status, 302);
    let (headers, sent) = task.join().unwrap();
    assert_eq!(sent, body);
    let encoded = headers
        .lines()
        .find_map(|line| line.strip_prefix("authorization: Nostr "))
        .unwrap();
    let event: serde_json::Value =
        serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
    assert_eq!(event["kind"], 27235);
    assert_eq!(event["content"], "");
    assert_eq!(event["tags"][0], serde_json::json!(["u", url.as_str()]));
    assert_eq!(event["tags"][1], serde_json::json!(["method", "POST"]));
    assert_eq!(
        event["tags"][2],
        serde_json::json!(["payload", format!("{:x}", Sha256::digest(body.as_bytes()))])
    );
    // A fresh credential per dispatched request: a nonce and the current time.
    assert_eq!(event["tags"][3][0], "nonce");
    assert!(event["tags"][3][1]
        .as_str()
        .is_some_and(|nonce| !nonce.is_empty()));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(event["created_at"]
        .as_u64()
        .is_some_and(|at| now.abs_diff(at) <= 5));
    verify(&event);
}

#[tokio::test]
async fn memory_response_limit_rejects_before_generic_transport_budget() {
    // The advertised size alone must be rejected. A multi-megabyte server write
    // blocks the fixture thread after the client closes on this header.
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        2 * 1024 * 1024 + 1
    );
    let (url, task) = fixture_server(response);
    let result = send(
        &TestSigner,
        url,
        "POST",
        Some("[]".into()),
        true,
        2 * 1024 * 1024,
    )
    .await;
    assert!(matches!(result, Err(ref message) if message == "Relay response is too large"));
    task.join().unwrap();
}
