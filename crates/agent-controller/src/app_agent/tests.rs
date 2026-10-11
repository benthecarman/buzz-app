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
    let agents = AppAgents::open(dir.join("agents2/identities.json"));
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
    for kind in [5, 7, 9, 40003, 40100, 41010] {
        let tags = vec![
            vec!["auth".to_owned(), "forged".to_owned()],
            vec!["h".to_owned(), "channel".to_owned()],
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
            [vec!["h".to_owned(), "channel".to_owned()], saved.clone()]
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
    for kind in [0, 1, 3, 9000, 24242, 27235, 30078, 30174, 40002] {
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
    let sha = "a".repeat(64);
    let auth = agent.upload_auth(&key, &sha).unwrap();
    assert_eq!(auth["kind"], 24242);
    let tags: Vec<Vec<String>> = serde_json::from_value(auth["tags"].clone()).unwrap();
    assert_eq!(tags[1], ["x", sha.as_str()]);
    assert_eq!(
        tags[3],
        ["server", agent.relay.trim_start_matches("wss://")]
    );
    assert!(agent.upload_auth(&key, "not-a-hash").is_err());
    let future = u64::MAX / 2;
    let memory = agent.memory(&key, "mem/notes", "hi", future).unwrap();
    assert_eq!(memory["kind"], 30174);
    assert_eq!(memory["created_at"], future + 1);
    assert_eq!(memory["tags"][1], serde_json::json!(["p", agent.owner]));
    assert!(agent.memory(&key, "../notes", "hi", 0).is_err());
    let other = Secret::generate().unwrap();
    assert!(agent.upload_auth(&other, &sha).is_err());
    assert!(agent.memory(&other, "core", "hi", 0).is_err());
}

#[test]
fn concurrent_writers_on_one_file_keep_every_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agents2/identities.json");
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
