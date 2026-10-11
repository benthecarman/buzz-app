use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
struct Memory(Mutex<BTreeMap<String, String>>, Faults);
/// Credential-store operations to refuse, standing in for a denied or busy store.
#[derive(Default)]
struct Faults {
    add: std::sync::atomic::AtomicBool,
    read: std::sync::atomic::AtomicBool,
    delete: std::sync::atomic::AtomicBool,
}
fn set(fault: &std::sync::atomic::AtomicBool, on: bool) {
    fault.store(on, std::sync::atomic::Ordering::SeqCst);
}
fn failing(fault: &std::sync::atomic::AtomicBool) -> Result<()> {
    if fault.load(std::sync::atomic::Ordering::SeqCst) {
        Err("Credential store refused".into())
    } else {
        Ok(())
    }
}
impl Credentials for Memory {
    fn read_legacy(&self, _: crate::LegacySource, _: &str) -> Result<Secret> {
        panic!("App agents never import")
    }
    fn read(&self, id: &str, pubkey: &str) -> Result<Option<Secret>> {
        failing(&self.1.read)?;
        self.0
            .lock()
            .unwrap()
            .get(id)
            .map(|hex| Secret::parse(hex, pubkey))
            .transpose()
    }
    fn add(&self, id: &str, key: &Secret) -> Result<()> {
        failing(&self.1.add)?;
        self.0
            .lock()
            .unwrap()
            .insert(id.into(), key.hex().to_string());
        Ok(())
    }
    fn delete(&self, id: &str, _: &str) -> Result<()> {
        failing(&self.1.delete)?;
        self.0.lock().unwrap().remove(id);
        Ok(())
    }
}

/// The owner `test_attestation` signs with.
fn owner() -> String {
    let auth: Vec<String> =
        serde_json::from_str(&crate::secret::test_attestation(&"a".repeat(64))).unwrap();
    auth[1].clone()
}
fn created(dir: &std::path::Path, credentials: &Memory) -> (AppAgents, AppAgent) {
    let agents = AppAgents::open(dir.join("agents/identities.json"));
    let prepared = AppAgents::prepare("https://relay.example/", &owner(), "p/t", "Ada").unwrap();
    let auth: Vec<String> =
        serde_json::from_str(&crate::secret::test_attestation(prepared.pubkey())).unwrap();
    let (agent, _) = agents.commit(prepared, &auth, credentials).unwrap();
    (agents, agent)
}

/// The rows, as `(pubkey, deleted)`.
fn rows(agents: &AppAgents) -> Vec<(String, bool)> {
    let list = agents.list().unwrap();
    list.into_iter()
        .map(|agent| (agent.pubkey, agent.deleted))
        .collect()
}

#[test]
fn creates_saves_and_removes_an_identity_with_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, agent) = created(dir.path(), &credentials);
    assert_eq!(agent.relay, "wss://relay.example");
    assert_eq!(
        (agent.agent_type.as_str(), agent.name.as_str()),
        ("p/t", "Ada")
    );
    assert_eq!(agent.events_url(), "https://relay.example/events");
    assert_eq!(agents.list().unwrap(), std::slice::from_ref(&agent));
    assert_eq!(agents.get(&agent.pubkey).unwrap(), agent);
    assert!(agent.read_key(&credentials).is_ok());
    // The key goes at once; the row stays, marked, for its community's cleanup.
    agents.remove(&agent.pubkey, &credentials).unwrap();
    assert_eq!(rows(&agents), [(agent.pubkey.clone(), true)]);
    assert!(agents.get(&agent.pubkey).is_err());
    assert!(agents.rename(&agent.pubkey, "Bea").is_err());
    assert!(agent.read_key(&credentials).is_err());
    // Removing again is success.
    agents.remove(&agent.pubkey, &credentials).unwrap();
    agents.forget(&agent.pubkey).unwrap();
    assert!(agents.list().unwrap().is_empty());
}

#[test]
fn forgets_only_deleted_rows() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, agent) = created(dir.path(), &credentials);
    agents.forget(&agent.pubkey).unwrap();
    assert_eq!(agents.get(&agent.pubkey).unwrap(), agent);
}

#[test]
fn changes_only_a_live_agents_type() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, agent) = created(dir.path(), &credentials);
    agents.set_type(&agent.pubkey, "q/u").unwrap();
    assert!(agents.set_type(&agent.pubkey, "").is_err());
    let changed = agents.get(&agent.pubkey).unwrap();
    assert_eq!(
        changed,
        AppAgent {
            agent_type: "q/u".into(),
            ..agent.clone()
        }
    );
    agents.remove(&agent.pubkey, &credentials).unwrap();
    assert!(agents.set_type(&agent.pubkey, "p/t").is_err());
}

#[test]
fn renames_and_profiles_replace_the_last_accepted_profile() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, agent) = created(dir.path(), &credentials);
    let key = agent.read_key(&credentials).unwrap();
    let first = agent.profile_event(&key).unwrap();
    assert_eq!(first["kind"], 0);
    assert_eq!(first["content"], r#"{"bot":true,"name":"Ada"}"#);
    let saved: Vec<String> = serde_json::from_str(&agent.auth).unwrap();
    assert_eq!(first["tags"], serde_json::json!([saved]));
    // An accepted profile from the future: the next must still be newer.
    let future = first["created_at"].as_u64().unwrap() + 100;
    let published = |created_at| Published {
        name: "Ada".into(),
        picture: None,
        about: None,
        created_at,
    };
    agents.published(&agent.pubkey, published(future)).unwrap();
    agents
        .published(&agent.pubkey, published(future - 1))
        .unwrap();
    agents.rename(&agent.pubkey, "  Bea ").unwrap();
    assert!(agents.rename(&agent.pubkey, " ").is_err());
    let renamed = agents.get(&agent.pubkey).unwrap();
    assert_eq!(renamed.name, "Bea");
    assert_eq!(renamed.profile, Some(published(future)));
    let next = renamed.profile_event(&key).unwrap();
    assert_eq!(next["created_at"], future + 1);
    assert_eq!(next["content"], r#"{"bot":true,"name":"Bea"}"#);
}

#[test]
fn a_set_picture_is_published_and_can_be_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, agent) = created(dir.path(), &credentials);
    let key = agent.read_key(&credentials).unwrap();
    let url = "https://relay.example/media/abc";
    for refused in [
        "http://relay.example/a",
        "https://u:p@relay.example/a",
        "nope",
    ] {
        assert!(agents
            .set_picture(&agent.pubkey, Some(refused.into()))
            .is_err());
    }
    agents.set_picture(&agent.pubkey, Some(url.into())).unwrap();
    let pictured = agents.get(&agent.pubkey).unwrap();
    assert_eq!(pictured.picture.as_deref(), Some(url));
    assert!(!pictured.profile_is_current());
    assert_eq!(
        pictured.profile_event(&key).unwrap()["content"],
        format!(r#"{{"bot":true,"name":"Ada","picture":"{url}"}}"#)
    );
    agents.set_picture(&agent.pubkey, None).unwrap();
    assert_eq!(agents.get(&agent.pubkey).unwrap().picture, None);
}

#[test]
fn a_failed_key_deletion_keeps_the_identity_for_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, agent) = created(dir.path(), &credentials);
    set(&credentials.1.delete, true);
    assert!(agents.remove(&agent.pubkey, &credentials).is_err());
    assert_eq!(agents.list().unwrap(), std::slice::from_ref(&agent));
    assert!(agent.read_key(&credentials).is_ok());
    set(&credentials.1.delete, false);
    agents.remove(&agent.pubkey, &credentials).unwrap();
    assert_eq!(rows(&agents), [(agent.pubkey, true)]);
    assert!(credentials.0.lock().unwrap().is_empty());
}

#[test]
fn a_failed_key_save_leaves_no_orphaned_key() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let agents = AppAgents::open(dir.path().join("identities.json"));
    let commit = |credentials: &Memory| {
        let prepared = AppAgents::prepare("wss://relay.example", &owner(), "p/t", "Ada").unwrap();
        let auth: Vec<String> =
            serde_json::from_str(&crate::secret::test_attestation(prepared.pubkey())).unwrap();
        agents.commit(prepared, &auth, credentials).map(drop)
    };
    // The store refuses the key: nothing is left behind.
    set(&credentials.1.add, true);
    assert!(commit(&credentials).is_err());
    assert!(agents.list().unwrap().is_empty());
    // ...even when it refuses everything, as an unavailable store does.
    set(&credentials.1.delete, true);
    assert!(commit(&credentials).is_err());
    assert!(agents.list().unwrap().is_empty());
    set(&credentials.1.delete, false);
    set(&credentials.1.add, false);
    // The key is saved but cannot be read back: it is deleted with its identity.
    set(&credentials.1.read, true);
    assert!(commit(&credentials).is_err());
    assert!(agents.list().unwrap().is_empty());
    assert!(credentials.0.lock().unwrap().is_empty());
    // ...and if it cannot be deleted either, its identity stays so `remove` can.
    set(&credentials.1.delete, true);
    assert!(commit(&credentials).is_err());
    let [stranded] = agents.list().unwrap().try_into().unwrap();
    set(&credentials.1.read, false);
    set(&credentials.1.delete, false);
    assert!(stranded.read_key(&credentials).is_ok());
    agents.remove(&stranded.pubkey, &credentials).unwrap();
    assert_eq!(rows(&agents), [(stranded.pubkey, true)]);
    assert!(credentials.0.lock().unwrap().is_empty());
}

#[test]
fn refuses_an_attestation_for_another_key_or_owner() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let agents = AppAgents::open(dir.path().join("identities.json"));
    let prepared = AppAgents::prepare("wss://relay.example", &owner(), "p/t", "Ada").unwrap();
    let other: Vec<String> =
        serde_json::from_str(&crate::secret::test_attestation(&"b".repeat(64))).unwrap();
    assert!(agents.commit(prepared, &other, &credentials).is_err());
    let prepared =
        AppAgents::prepare("wss://relay.example", &"c".repeat(64), "p/t", "Ada").unwrap();
    let auth: Vec<String> =
        serde_json::from_str(&crate::secret::test_attestation(prepared.pubkey())).unwrap();
    assert!(agents.commit(prepared, &auth, &credentials).is_err());
    assert!(agents.list().unwrap().is_empty());
    assert!(credentials.0.lock().unwrap().is_empty());
}

#[test]
fn signs_only_bounded_kinds_with_the_saved_attestation() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (_, agent) = created(dir.path(), &credentials);
    let key = agent.read_key(&credentials).unwrap();
    let saved: Vec<String> = serde_json::from_str(&agent.auth).unwrap();
    for kind in [
        5, 7, 9, 9000, 9001, 9002, 9007, 9008, 9021, 9022, 20002, 40003, 40100, 41010,
    ] {
        let tags = vec![
            vec!["auth".to_owned(), "forged".to_owned()],
            vec!["h".to_owned(), "channel".to_owned()],
            vec!["p".to_owned(), agent.owner.clone(), "admin".to_owned()],
        ];
        let event = agent.sign(&key, kind, "hi".into(), tags).unwrap();
        assert_eq!(event["kind"], kind);
        assert_eq!(event["pubkey"], agent.pubkey);
        let mut tags: Vec<Vec<String>> = serde_json::from_value(event["tags"].clone()).unwrap();
        let ms = tags.remove(tags.iter().position(|tag| tag[0] == "ms").unwrap());
        assert_eq!(ms[0], "ms");
        assert!(ms[1].parse::<u16>().unwrap() < 1000);
        assert_eq!(
            tags,
            [
                vec!["h".to_owned(), "channel".to_owned()],
                vec!["p".to_owned(), agent.owner.clone(), "admin".to_owned()],
                saved.clone()
            ]
        );
    }
    // A plugin cannot pick its own `ms`, and events outside a channel get none.
    let forged = vec![
        vec!["h".to_owned(), "channel".to_owned()],
        vec!["ms".to_owned(), "999".to_owned()],
    ];
    let event = agent.sign(&key, 9, "hi".into(), forged).unwrap();
    let tags: Vec<Vec<String>> = serde_json::from_value(event["tags"].clone()).unwrap();
    assert_eq!(tags.iter().filter(|tag| tag[0] == "ms").count(), 1);
    let event = agent.sign(&key, 7, "+".into(), vec![]).unwrap();
    let tags: Vec<Vec<String>> = serde_json::from_value(event["tags"].clone()).unwrap();
    assert!(tags.iter().all(|tag| tag[0] != "ms"));
    for kind in [0, 1, 3, 9003, 10100, 24242, 27235, 30078, 30174, 40002] {
        assert!(agent.sign(&key, kind, String::new(), vec![]).is_err());
    }
    assert!(agent
        .sign(&key, 9, "x".repeat(64 * 1024 + 1), vec![])
        .is_err());
    assert!(agent.sign(&key, 9, String::new(), vec![vec![]]).is_err());
    let other = Secret::generate().unwrap();
    assert!(agent.sign(&other, 9, String::new(), vec![]).is_err());
    assert!(agent.http_auth(&other, &agent.events_url(), b"{}").is_err());
    for url in [agent.events_url(), agent.query_url()] {
        let auth = agent.http_auth(&key, &url, b"{}").unwrap();
        assert_eq!(auth["kind"], 27235);
        assert_eq!(auth["tags"][0], serde_json::json!(["u", url]));
    }
    assert!(agent
        .http_auth(&key, "https://elsewhere.test/events", b"{}")
        .is_err());
}

#[test]
fn signs_uploads_and_memory_for_its_own_community_and_owner() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (_, agent) = created(dir.path(), &credentials);
    let key = agent.read_key(&credentials).unwrap();
    use sha2::{Digest, Sha256};
    let bytes = b"media body";
    let sha = format!("{:x}", Sha256::digest(bytes));
    let auth = agent.upload_auth(&key, &sha).unwrap();
    assert_eq!(auth["kind"], 24242);
    let tags: Vec<Vec<String>> = serde_json::from_value(auth["tags"].clone()).unwrap();
    assert_eq!(tags[1], ["x", sha.as_str()]);
    assert_eq!(
        tags[3],
        ["server", agent.relay.trim_start_matches("wss://")]
    );
    assert!(agent.upload_auth(&key, "not-a-hash").is_err());
    let get = agent.media_auth(&key).unwrap();
    assert_eq!(get["kind"], 24242);
    let tags: Vec<Vec<String>> = serde_json::from_value(get["tags"].clone()).unwrap();
    assert_eq!(tags[0], ["t", "get"]);
    assert_eq!(
        tags[2],
        ["server", agent.relay.trim_start_matches("wss://")]
    );
    let media = format!(
        "{}/media/{sha}.png",
        agent.relay.replacen("wss", "https", 1)
    );
    assert_eq!(agent.media_url(&media).unwrap(), media);
    assert_eq!(agent.media_url(&format!("{sha}.png")).unwrap(), media);
    assert!(agent.verify_media_hash(&media, bytes).is_ok());
    assert!(agent.verify_media_hash(&media, b"different body").is_err());
    let thumbnail = format!("{}/media/{sha}.thumb.jpg", agent.endpoint(""));
    assert!(agent
        .verify_media_hash(&thumbnail, b"derived thumbnail")
        .is_ok());
    for input in [
        format!("https://elsewhere.test/media/{sha}.png"),
        format!("{media}?x=1"),
        format!("{sha}/../x"),
        format!("{sha}.PNG"),
        "not-a-hash".into(),
    ] {
        assert!(agent.media_url(&input).is_err(), "{input}");
    }
    let future = u64::MAX / 2;
    let memory = agent.memory(&key, "mem/notes", "hi", future).unwrap();
    assert_eq!(memory["kind"], 30174);
    assert_eq!(memory["created_at"], future + 1);
    assert_eq!(memory["tags"][1], serde_json::json!(["p", agent.owner]));
    assert!(agent.memory(&key, "../notes", "hi", 0).is_err());
    let other = Secret::generate().unwrap();
    assert!(agent.upload_auth(&other, &sha).is_err());
    assert!(agent.media_auth(&other).is_err());
    assert!(agent.memory(&other, "core", "hi", 0).is_err());
}

#[test]
fn concurrent_writers_on_one_file_keep_every_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agents/identities.json");
    let credentials = std::sync::Arc::new(Memory::default());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            // Separate handles stand in for separate app instances.
            let agents = AppAgents::open(path.clone());
            let credentials = credentials.clone();
            std::thread::spawn(move || {
                let prepared =
                    AppAgents::prepare("https://relay.example/", &owner(), "p/t", "Ada").unwrap();
                let auth: Vec<String> =
                    serde_json::from_str(&crate::secret::test_attestation(prepared.pubkey()))
                        .unwrap();
                agents
                    .commit(prepared, &auth, credentials.as_ref())
                    .unwrap()
                    .0
                    .pubkey
            })
        })
        .collect();
    let mut made: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    let mut saved: Vec<_> = AppAgents::open(path)
        .list()
        .unwrap()
        .into_iter()
        .map(|agent| agent.pubkey)
        .collect();
    made.sort();
    saved.sort();
    assert_eq!(saved, made);
}

#[test]
fn an_adopted_key_keeps_its_identity_picture_and_shared_saved_key() {
    let credentials = Memory::default();
    let key = Secret::generate().unwrap();
    let pubkey = key.pubkey().to_owned();
    // A harness agent already keeps this key under the same community's ID.
    let id = agent_id(&pubkey, "wss://relay.example");
    credentials.add(&id, &key).unwrap();
    set(&credentials.1.add, true);
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let store = |n: usize| AppAgents::open(dirs[n].path().join("agents/identities.json"));
    let adopt = |picture: &str| {
        AppAgents::adopt(
            "https://relay.example/",
            &owner(),
            "p/t",
            Secret::parse(&key.hex(), &pubkey).unwrap(),
            "Ada",
            Some(picture.into()),
            Some("Reviews pull requests".into()),
        )
        .unwrap()
    };
    let auth: Vec<String> =
        serde_json::from_str(&crate::secret::test_attestation(&pubkey)).unwrap();
    let agents = store(0);
    let (agent, _) = agents
        .commit(
            adopt("https://relay.example/media/a.png"),
            &auth,
            &credentials,
        )
        .unwrap();
    assert_eq!(agent.pubkey, pubkey);
    assert!(!serde_json::to_value(&agent)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("adopted"));
    assert_eq!(
        agents.get(&pubkey).unwrap().about.as_deref(),
        Some("Reviews pull requests")
    );
    let profile = agent
        .profile_event(&agent.read_key(&credentials).unwrap())
        .unwrap();
    assert_eq!(
        profile["content"],
        r#"{"about":"Reviews pull requests","bot":true,"name":"Ada","picture":"https://relay.example/media/a.png"}"#
    );
    // Once here, it is not taken over again, nor while it is being removed.
    let again = adopt("https://a.example/b.png");
    assert!(agents.commit(again, &auth, &credentials).is_err());
    agents.edit(&pubkey, |agent| agent.deleted = true).unwrap();
    let again = adopt("https://a.example/b.png");
    assert!(agents.commit(again, &auth, &credentials).is_err());
    // A shared key that fails to verify is left where it was.
    set(&credentials.1.read, true);
    assert!(store(1)
        .commit(adopt("https://a.example/b.png"), &auth, &credentials)
        .is_err());
    set(&credentials.1.read, false);
    assert!(credentials.read(&id, &pubkey).unwrap().is_some());
    assert!(rows(&store(1)).is_empty());
    // Inline pictures are not profile pictures.
    let (agent, _) = store(2)
        .commit(adopt("data:image/png;base64,AA"), &auth, &credentials)
        .unwrap();
    assert_eq!(agent.picture, None);
}

#[test]
fn an_adopted_key_that_cannot_be_saved_withdraws_only_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let agents = AppAgents::open(dir.path().join("agents/identities.json"));
    let key = Secret::generate().unwrap();
    let pubkey = key.pubkey().to_owned();
    let auth: Vec<String> =
        serde_json::from_str(&crate::secret::test_attestation(&pubkey)).unwrap();
    let adopt = || {
        AppAgents::adopt(
            "https://relay.example/",
            &owner(),
            "p/t",
            Secret::parse(&key.hex(), &pubkey).unwrap(),
            "Ada",
            None,
            None,
        )
        .unwrap()
    };
    set(&credentials.1.add, true);
    assert!(agents.commit(adopt(), &auth, &credentials).is_err());
    assert!(rows(&agents).is_empty());
    set(&credentials.1.add, false);
    let (agent, _) = agents.commit(adopt(), &auth, &credentials).unwrap();
    assert_eq!(agent.read_key(&credentials).unwrap().pubkey(), pubkey);
}

#[test]
fn deleting_keys_removes_every_agents_key_and_repeats_safely() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, first) = created(dir.path(), &credentials);
    let prepared = AppAgents::prepare("https://relay.example/", &owner(), "p/t", "Bea").unwrap();
    let auth: Vec<String> =
        serde_json::from_str(&crate::secret::test_attestation(prepared.pubkey())).unwrap();
    let (second, _) = agents.commit(prepared, &auth, &credentials).unwrap();
    agents.delete_keys(&credentials).unwrap();
    assert!(first.read_key(&credentials).is_err());
    assert!(second.read_key(&credentials).is_err());
    agents.delete_keys(&credentials).unwrap();
    // No saved identities is nothing to delete.
    AppAgents::open(dir.path().join("absent/identities.json"))
        .delete_keys(&credentials)
        .unwrap();
}

#[test]
fn saved_public_profile_survives_updates_and_is_signed() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, agent) = created(dir.path(), &credentials);
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&agents.path).unwrap()).unwrap();
    saved[0]["picture"] = "https://relay.example/media/a.png".into();
    saved[0]["about"] = "Reviews pull requests.\nKeeps changes small.".into();
    // Existing receipts predate picture/about tracking.
    saved[0]["profile"] = serde_json::json!({"name": "Ada", "createdAt": 500});
    std::fs::write(&agents.path, saved.to_string()).unwrap();
    agents.rename(&agent.pubkey, "Bea").unwrap();
    let reopened = AppAgents::open(agents.path.clone());
    let agent = reopened.get(&agent.pubkey).unwrap();
    let event = agent
        .profile_event(&agent.read_key(&credentials).unwrap())
        .unwrap();
    let content: serde_json::Value =
        serde_json::from_str(event["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        content,
        serde_json::json!({
            "name": "Bea", "bot": true,
            "picture": "https://relay.example/media/a.png",
            "about": "Reviews pull requests.\nKeeps changes small.",
        })
    );
    assert!(!agent.profile_is_current());
    reopened
        .published(
            &agent.pubkey,
            Published {
                name: agent.name.clone(),
                picture: agent.picture.clone(),
                about: agent.about.clone(),
                created_at: event["created_at"].as_u64().unwrap(),
            },
        )
        .unwrap();
    assert!(reopened.get(&agent.pubkey).unwrap().profile_is_current());
    let accepted = std::fs::read(&reopened.path).unwrap();
    // Each metadata-only change/clear must publish, even without a rename.
    for field in ["picture", "about"] {
        for replacement in [Some("https://relay.example/changed"), None] {
            let mut changed: serde_json::Value = serde_json::from_slice(&accepted).unwrap();
            changed[0][field] = serde_json::json!(replacement);
            std::fs::write(&reopened.path, changed.to_string()).unwrap();
            let changed = reopened.get(&agent.pubkey).unwrap();
            assert!(!changed.profile_is_current());
            let event = changed
                .profile_event(&agent.read_key(&credentials).unwrap())
                .unwrap();
            let content: serde_json::Value =
                serde_json::from_str(event["content"].as_str().unwrap()).unwrap();
            assert_eq!(
                content.get(field).and_then(serde_json::Value::as_str),
                replacement
            );
        }
    }
    let event: nostr::event::Event = serde_json::from_value(event).unwrap();
    event.verify().unwrap();
}

#[test]
fn old_adopted_flags_are_read_then_dropped_without_relaxing_the_schema() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = Memory::default();
    let (agents, agent) = created(dir.path(), &credentials);
    let original = serde_json::to_value(agents.list().unwrap()).unwrap();
    for adopted in [false, true] {
        let mut saved = original.clone();
        saved[0]["adopted"] = adopted.into();
        std::fs::write(&agents.path, saved.to_string()).unwrap();
        assert_eq!(agents.get(&agent.pubkey).unwrap(), agent);
        agents.rename(&agent.pubkey, "Bea").unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&agents.path).unwrap()).unwrap();
        assert!(!rewritten[0].as_object().unwrap().contains_key("adopted"));
    }
    for (field, value) in [
        ("adopted", serde_json::json!("true")),
        ("unknown", serde_json::json!(true)),
    ] {
        let mut saved = original.clone();
        saved[0][field] = value;
        std::fs::write(&agents.path, saved.to_string()).unwrap();
        assert!(agents.list().is_err());
    }
}
