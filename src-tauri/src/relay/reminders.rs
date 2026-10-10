//! NIP-ER reminders (kind 30300). Self-encrypted only; never a general NIP-44 or signing primitive.
use super::{verify_owned_event, Result};
use crate::identity::IdentityHost;
use buzz_desktop::relay::reminders::{self, ReminderIntent};
use serde_json::Value;

/// Decrypt verified, self-authored reminders. Other clients may write records this
/// client cannot read or save, so invalid entries are skipped, not fatal to the list.
#[tauri::command]
pub(crate) async fn relay_decode_reminders(
    host: tauri::State<'_, IdentityHost>,
    events: Vec<Value>,
) -> Result<Value> {
    reminders::decode_capacity(&events)?;
    host.with_key(move |secret, viewer| Ok(reminders::decode(secret, viewer, &events)))
        .await
}

/// Sign one self-encrypted reminder built from bounded intent, never caller ciphertext or tags.
#[tauri::command]
pub(crate) async fn relay_sign_reminder(
    host: tauri::State<'_, IdentityHost>,
    intent: ReminderIntent,
) -> Result<Value> {
    sign_reminder(host.inner(), intent, now()?).await
}

fn now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "System clock is unavailable")?
        .as_secs())
}

async fn sign_reminder(host: &IdentityHost, intent: ReminderIntent, now: u64) -> Result<Value> {
    let prepared = reminders::prepare(intent, now)?;
    let template = host
        .with_key(move |secret, _| prepared.seal(secret))
        .await?;
    host.sign(template).await
}

/// Admit for publication only a record shaped exactly like `relay_sign_reminder` output.
pub(super) async fn admit_reminder(host: &IdentityHost, event: &Value) -> Result<()> {
    if event["kind"].as_u64() != Some(u64::from(reminders::KIND)) {
        return Err("Invalid reminder".into());
    }
    verify_owned_event(host, event).await?;
    let tag = reminders::admission_tag(event)?;
    let raw = event.clone();
    host.with_key(move |secret, viewer| reminders::admit(secret, viewer, &raw, tag))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::EventTemplate;
    use buzz_desktop::relay::reminders::{decrypt, KIND};
    use nostr::{
        key::{Keys, SecretKey},
        nips::nip44,
    };

    const DAY: u64 = 86_400;
    use serde_json::json;

    fn target() -> Value {
        json!({"eventId": "a".repeat(64), "channelId": "c1", "preview": "hi", "authorPubkey": "b".repeat(64)})
    }

    fn intent(not_before: Option<u64>, expiration: Option<u64>, status: &str) -> ReminderIntent {
        ReminderIntent {
            d: "0123456789abcdef0123456789abcdef".into(),
            created_at: 1_000_000,
            not_before,
            expiration,
            content: json!({"target": target(), "status": status}),
        }
    }

    #[tokio::test]
    async fn signs_decodes_and_admits_only_own_reminders() {
        let host = IdentityHost::fixture();
        let now = 1_000_000;
        let pending = sign_reminder(&host, intent(Some(now + 60), None, "pending"), now)
            .await
            .unwrap();
        assert_eq!(pending["kind"], 30300);
        assert_eq!(pending["tags"][1], json!(["not_before", "1000060"]));
        admit_reminder(&host, &pending).await.unwrap();
        let viewer = host.viewer().await.unwrap();
        let decoded = host
            .with_key({
                let pending = pending.clone();
                move |secret, viewer| Ok(decrypt(secret, viewer, &pending))
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decoded["content"]["target"], target());
        assert_eq!(pending["pubkey"], viewer);

        let done = sign_reminder(&host, intent(None, Some(now + 40 * DAY), "done"), now)
            .await
            .unwrap();
        assert_eq!(done["tags"][1][0], "expiration");
        admit_reminder(&host, &done).await.unwrap();

        // Wrong schedule/status pairing, out-of-window expiration, stale clocks and bad d tags.
        for bad in [
            intent(Some(now), None, "done"),
            intent(None, Some(now + DAY), "done"),
            intent(Some(now), Some(now + 40 * DAY), "pending"),
            ReminderIntent {
                created_at: now - 3600,
                ..intent(Some(now), None, "pending")
            },
            ReminderIntent {
                d: "not-hex".into(),
                ..intent(Some(now), None, "pending")
            },
        ] {
            assert!(sign_reminder(&host, bad, now).await.is_err());
        }

        // A tampered record fails signature verification and is skipped, not admitted.
        let mut tampered = pending.clone();
        tampered["tags"][1][1] = json!("1");
        assert!(admit_reminder(&host, &tampered).await.is_err());
        let skipped = host
            .with_key(move |secret, viewer| Ok(decrypt(secret, viewer, &tampered)))
            .await
            .unwrap();
        assert!(skipped.is_none());
    }

    /// Encrypt and sign arbitrary content, as beta or mobile may have written it.
    async fn seal(host: &IdentityHost, content: Value) -> Value {
        let plaintext = content.to_string();
        let sealed = host
            .with_key(move |secret, _| {
                let secret = SecretKey::from_slice(secret).unwrap();
                let public = Keys::new(secret.clone()).public_key();
                Ok(nip44::encrypt(&secret, &public, &plaintext, nip44::Version::V2).unwrap())
            })
            .await
            .unwrap();
        host.sign(EventTemplate {
            kind: KIND,
            created_at: 1_000_000,
            content: sealed,
            tags: vec![
                vec!["d".into(), "0123456789abcdef0123456789abcdef".into()],
                vec!["not_before".into(), "1000060".into()],
            ],
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn decode_skips_reminders_this_client_could_not_save() {
        let host = IdentityHost::fixture();
        let decode =
            |event: Value| host.with_key(move |secret, viewer| Ok(decrypt(secret, viewer, &event)));
        let normal = seal(&host, json!({"target": target(), "status": "pending"})).await;
        assert!(decode(normal).await.unwrap().is_some());
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}".repeat(100);
        let mut emoji = target();
        emoji["preview"] = json!(family);
        let mut empty = target();
        empty["channelId"] = json!("");
        empty["authorPubkey"] = json!("");
        for (name, content) in [
            (
                "4,097-char note",
                json!({"target": target(), "note": "n".repeat(4097), "status": "pending"}),
            ),
            (
                "100 family emoji preview",
                json!({"target": emoji, "status": "pending"}),
            ),
            (
                "empty channel and author",
                json!({"target": empty, "status": "pending"}),
            ),
            (
                "17,000-char note",
                json!({"note": "n".repeat(17_000), "status": "pending"}),
            ),
        ] {
            let event = seal(&host, content).await;
            assert!(decode(event).await.unwrap().is_none(), "{name}");
        }
    }

    #[tokio::test]
    async fn admit_rejects_foreign_pubkeys_and_other_kinds() {
        let host = IdentityHost::fixture();
        let now = 1_000_000;
        let own = sign_reminder(&host, intent(Some(now + 60), None, "pending"), now)
            .await
            .unwrap();

        // Another author's reminder; the pubkey check runs before signature verification.
        let mut foreign = own.clone();
        foreign["pubkey"] = json!("b".repeat(64));
        assert!(admit_reminder(&host, &foreign).await.is_err());

        // Our own pubkey but not a reminder kind.
        let mut wrong_kind = own.clone();
        wrong_kind["kind"] = json!(30078);
        assert!(admit_reminder(&host, &wrong_kind).await.is_err());
    }
}
