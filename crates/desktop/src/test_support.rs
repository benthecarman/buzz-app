//! Fixtures shared by this crate's tests.
use crate::event::{EventTemplate, Signer};
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;
use url::Url;

/// Signs as the fixed test key, like the app's identity fixture.
#[derive(Clone, Copy, Default)]
pub(crate) struct TestSigner;

impl TestSigner {
    fn keypair() -> secp256k1::Keypair {
        let secret = secp256k1::SecretKey::from_byte_array([1; 32]).unwrap();
        secp256k1::Keypair::from_secret_key(&secp256k1::Secp256k1::signing_only(), &secret)
    }
}

impl Signer for TestSigner {
    async fn sign(&self, event: EventTemplate) -> Result<serde_json::Value, String> {
        let pair = Self::keypair();
        let pubkey = pair.x_only_public_key().0.to_string();
        let serialized = serde_json::to_vec(&serde_json::json!([
            0,
            pubkey,
            event.created_at,
            event.kind,
            event.tags,
            event.content
        ]))
        .unwrap();
        if serialized.len() > 64 * 1024 {
            return Err("Relay event is too large".into());
        }
        let hash = Sha256::digest(serialized);
        let signature = secp256k1::Secp256k1::signing_only().sign_schnorr_no_aux_rand(&hash, &pair);
        Ok(serde_json::json!({
            "id": format!("{hash:x}"), "pubkey": pubkey,
            "created_at": event.created_at, "kind": event.kind,
            "tags": event.tags, "content": event.content, "sig": signature.to_string()
        }))
    }
}

impl<S: Signer> Signer for std::sync::Arc<S> {
    fn sign(
        &self,
        event: EventTemplate,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, String>> + Send {
        S::sign(self, event)
    }
}

/// Asserts the event's ID and Schnorr signature.
pub(crate) fn verify(event: &serde_json::Value) {
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

pub(crate) fn fixture_server(response: String) -> (Url, std::thread::JoinHandle<(String, String)>) {
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

pub(crate) fn blossom_event(headers: &str) -> serde_json::Value {
    let encoded = headers
        .lines()
        .find_map(|line| line.strip_prefix("authorization: Nostr "))
        .unwrap();
    let event: serde_json::Value =
        serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
    verify(&event);
    event
}

pub(crate) fn tag<'a>(event: &'a serde_json::Value, name: &str) -> Vec<&'a str> {
    event["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tag| tag[0] == name)
        .map(|tag| tag[1].as_str().unwrap())
        .collect()
}

/// The relay's strict NIP-FI rules: one each of `t`, `server`, `expiration`,
/// expiry within 60 s of creation, non-empty content.
pub(crate) fn assert_strict(event: &serde_json::Value, verb: &str, server: &str) {
    assert_eq!(event["kind"], 24242);
    assert_ne!(event["content"], "");
    assert_eq!(tag(event, "t"), [verb]);
    assert_eq!(tag(event, "server"), [server]);
    let expiration: u64 = tag(event, "expiration")[0].parse().unwrap();
    assert_eq!(tag(event, "expiration").len(), 1);
    assert_eq!(expiration, event["created_at"].as_u64().unwrap() + 60);
}

/// Runs a future to completion on a fresh current-thread runtime.
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}
