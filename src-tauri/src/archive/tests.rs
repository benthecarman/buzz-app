use super::*;
use crate::relay::agent::AgentEvent;
use base64::{engine::general_purpose::STANDARD, Engine};
use nostr::{
    key::{PublicKey, SecretKey},
    nips::nip44,
};
use secp256k1::{Keypair, Secp256k1};
use serde_json::json;
use sha2::{Digest, Sha256};

pub(crate) fn envelope(viewer: &str, kind: u16, serial: u64) -> AgentEvent {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    envelope_at(viewer, kind, serial, created)
}
pub(crate) fn envelope_at(viewer: &str, kind: u16, serial: u64, created: u64) -> AgentEvent {
    let secret = [2; 32];
    let pair = Keypair::from_secret_key(
        &Secp256k1::signing_only(),
        &secp256k1::SecretKey::from_byte_array(secret).unwrap(),
    );
    let agent = pair.x_only_public_key().0.to_string();
    let conversation = nip44::v2::ConversationKey::derive(
        &SecretKey::from_slice(&secret).unwrap(),
        &PublicKey::from_hex(viewer).unwrap(),
    )
    .unwrap();
    let plaintext = format!(
        r#"{{"channelId":"private-channel","kind":"turn_started","payload":"archive-secret-marker","serial":{serial}}}"#
    );
    let content = STANDARD.encode(
        nip44::v2::encrypt_to_bytes_with_nonce(&conversation, plaintext.as_bytes(), [7; 32])
            .unwrap(),
    );
    let tags = vec![
        vec!["p", viewer],
        vec!["agent", &agent],
        vec!["frame", "telemetry"],
    ];
    let hash = Sha256::digest(
        serde_json::to_vec(&json!([0, agent, created, kind, tags, content])).unwrap(),
    );
    serde_json::from_value(json!({"id":format!("{hash:x}"),"pubkey":agent,"sig":Secp256k1::signing_only().sign_schnorr_no_aux_rand(&hash,&pair).to_string(),"created_at":created,"kind":kind,"tags":tags,"content":content})).unwrap()
}
fn viewer() -> String {
    Keypair::from_secret_key(
        &Secp256k1::signing_only(),
        &secp256k1::SecretKey::from_byte_array([1; 32]).unwrap(),
    )
    .x_only_public_key()
    .0
    .to_string()
}
#[tokio::test]
async fn host_rejects_invalid_ingest_isolates_bad_rows_and_preserves_live_decode_on_disk_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("archive.sqlite3");
    let identity = IdentityHost::fixture();
    let host = ArchiveHost::default();
    let viewer = viewer();
    let request = |input| {
        execute(
            &identity,
            host.clone(),
            path.clone(),
            "https://a.test".into(),
            viewer.clone(),
            input,
        )
    };
    request(Request::Settings).await.unwrap();
    let event = envelope(&viewer, 24200, 0);
    request(Request::Ingest {
        event: event.clone(),
        revision: 0,
    })
    .await
    .unwrap();
    for kind in [24200, 44200] {
        let stale = envelope_at(&viewer, kind, 8, event.created_at - 301);
        let wrong_viewer = envelope(&event.pubkey, kind, 9);
        for invalid in [stale, wrong_viewer] {
            assert!(request(Request::Ingest {
                event: invalid,
                revision: 0
            })
            .await
            .is_err());
        }
    }
    assert_eq!(
        rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM archive_events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let other = envelope(&viewer, 24200, 1);
    request(Request::Ingest {
        event: other.clone(),
        revision: 0,
    })
    .await
    .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE archive_events SET envelope='broken' WHERE id=?1",
            [other.id],
        )
        .unwrap();
    let page = request(Request::Read {
        kind: 24200,
        agent: None,
        before: None,
    })
    .await
    .unwrap();
    assert_eq!(page["records"].as_array().unwrap().len(), 1);
    assert_eq!(page["skipped"], 1);
    assert!(execute(
        &identity,
        host.clone(),
        path.clone(),
        "https://a.test".into(),
        "f".repeat(64),
        Request::Settings
    )
    .await
    .is_err());
    connection.execute_batch("CREATE TRIGGER refuse_clear BEFORE DELETE ON archive_events BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(request(Request::Clear { kind: Some(24200) }).await.is_err());
    assert_eq!(
        request(Request::Read {
            kind: 24200,
            agent: None,
            before: None
        })
        .await
        .unwrap()["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let corrupt = directory.path().join("broken.sqlite3");
    std::fs::write(&corrupt, b"not a database").unwrap();
    assert!(execute(
        &identity,
        ArchiveHost::default(),
        corrupt,
        "https://a.test".into(),
        viewer.clone(),
        Request::Settings
    )
    .await
    .is_err());
    assert!(crate::relay::agent::decode_archive(&[1; 32], &viewer, &event, false).is_ok());
}

/// SQLite waits must not hold the identity lock that signing needs.
#[tokio::test]
async fn sqlite_wait_does_not_hold_identity_signing_lock() {
    // A busy callback is the explicit barrier: signing starts only once the
    // archive operation has entered SQLite and is blocked by another writer.
    static BUSY: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>> =
        std::sync::Mutex::new(None);
    static RELEASE: std::sync::Condvar = std::sync::Condvar::new();
    static WAITING: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
    fn blocked(_: i32) -> bool {
        let mut waiting = WAITING.lock().unwrap();
        *waiting = true;
        if let Some(sender) = BUSY.lock().unwrap().take() {
            let _ = sender.send(());
        }
        while *waiting {
            waiting = RELEASE.wait(waiting).unwrap();
        }
        false
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("busy.sqlite3");
    let identity = IdentityHost::fixture();
    let viewer = identity.viewer().await.unwrap();
    let host =
        ArchiveHost::with_busy_handler(path.clone(), &viewer, "https://a.test", blocked).unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (sender, entered) = tokio::sync::oneshot::channel();
    *BUSY.lock().unwrap() = Some(sender);
    let other = identity.clone();
    let work = tokio::spawn(async move {
        execute(
            &other,
            host,
            path,
            "https://a.test".into(),
            viewer,
            Request::Settings,
        )
        .await
    });
    entered.await.unwrap();
    let signed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        identity.sign(crate::identity::EventTemplate {
            kind: 1,
            created_at: 1,
            tags: vec![],
            content: "independent signing".into(),
        }),
    )
    .await;
    // Always release before asserting, including when the old lock scope times out.
    *WAITING.lock().unwrap() = false;
    RELEASE.notify_one();
    blocker.execute_batch("ROLLBACK").unwrap();
    let _ = work.await.unwrap();
    assert!(signed.expect("SQLite blocked identity signing").is_ok());
}
