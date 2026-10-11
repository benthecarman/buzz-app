//! Identities for agents the app runs through a plugin (Agents). They are not
//! harness agents: there is no process, harness setting or controller record.
//! Native keeps each key and its owner attestation and signs only bounded kinds,
//! bounded requests to its community, and its memory.
//! The saved identities are the agent list: each row names its type and name.
use crate::config::{agent_id, canonical_key, canonical_relay};
use crate::secret::validate_attestation;
use crate::{Credentials, Result, Secret};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a plugin agent may publish: a deletion (5), a reaction (7), a message
/// (9), a channel member change (9000, 9001), edit (9002), creation (9007),
/// deletion (9008), join (9021) or leave (9022), typing (20002), an edit of its
/// own message (40003), a channel canvas (40100) or a request to open a DM
/// (41010). The relay decides which channel changes it may make. Its profile
/// (0) is the app's, and its memory (30174) is written by `memory`.
const KINDS: [u16; 14] = [
    5, 7, 9, 9000, 9001, 9002, 9007, 9008, 9021, 9022, 20002, 40003, 40100, 41010,
];

/// One saved identity. The key itself stays in the OS credential store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppAgent {
    pub pubkey: String,
    /// Canonical `wss://` community origin the agent was created for.
    pub relay: String,
    pub owner: String,
    /// The owner's NIP-OA `auth` tag as a JSON array string.
    pub auth: String,
    /// The agent type's key, `pluginId/typeId`, set at create and changed by
    /// `set_type`. Rows saved before it was recorded read as an unavailable
    /// type with no name.
    #[serde(rename = "type", default)]
    pub agent_type: String,
    #[serde(default)]
    pub name: String,
    /// Public profile metadata, published as kind-0 picture and about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// The last profile its community accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Published>,
    /// Its key is gone. The row stays until its community confirms it left its
    /// channels and was archived, which the owner signs, not the agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Published {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    pub created_at: u64,
}
impl AppAgent {
    fn credential_id(&self) -> String {
        agent_id(&self.pubkey, &self.relay)
    }
    /// The community's HTTP endpoints for writes, reads, uploads and media;
    /// nothing else is ever sent to.
    pub fn events_url(&self) -> String {
        self.endpoint("/events")
    }
    pub fn query_url(&self) -> String {
        self.endpoint("/query")
    }
    pub fn upload_url(&self) -> String {
        self.endpoint("/upload")
    }
    /// Its community's URL for one media blob, from that URL or its
    /// `sha256[.ext]` name. Media elsewhere is refused.
    pub fn media_url(&self, input: &str) -> Result<String> {
        let base = self.endpoint("/media/");
        let name = input.trim();
        let name = name.strip_prefix(base.as_str()).unwrap_or(name);
        let parts: Vec<&str> = name.split('.').collect();
        let ext = |part: &str| {
            !part.is_empty()
                && part.len() <= 8
                && part
                    .bytes()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
        };
        let valid = match parts.as_slice() {
            [hash] => crate::config::canonical_key(hash),
            [hash, part] => crate::config::canonical_key(hash) && ext(part),
            [hash, "thumb", "jpg"] => crate::config::canonical_key(hash),
            _ => false,
        };
        if !valid {
            return Err("Agents fetch only their community's media".into());
        }
        Ok(format!("{base}{name}"))
    }
    /// Confirms a downloaded media body matches the hash in its community URL.
    pub fn verify_media_hash(&self, url: &str, bytes: &[u8]) -> Result<()> {
        use sha2::{Digest, Sha256};
        let canonical = self.media_url(url)?;
        let expected = canonical
            .rsplit('/')
            .next()
            .and_then(|name| name.get(..64))
            .ok_or("Invalid media hash")?;
        if canonical.ends_with(".thumb.jpg") {
            // Thumbnails are derived JPEGs, but keep the source blob's name.
            return Ok(());
        }
        let actual = format!("{:x}", Sha256::digest(bytes));
        if actual != expected {
            return Err("Media did not match its SHA-256".into());
        }
        Ok(())
    }
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.relay.replacen("wss://", "https://", 1))
    }
    pub fn read_key(&self, credentials: &dyn Credentials) -> Result<Secret> {
        credentials
            .read(&self.credential_id(), &self.pubkey)?
            .ok_or_else(|| "Agent key unavailable".into())
    }
    /// Signs one bounded event as this agent with its owner attestation attached.
    pub fn sign(
        &self,
        key: &Secret,
        kind: u16,
        content: String,
        tags: Vec<Vec<String>>,
    ) -> Result<serde_json::Value> {
        if !KINDS.contains(&kind) {
            return Err(
                "Agents can sign messages, edits, reactions, deletions and channel changes only"
                    .into(),
            );
        }
        if content.len() > 64 * 1024
            || tags.len() > 256
            || tags
                .iter()
                .any(|tag| tag.is_empty() || tag.iter().map(String::len).sum::<usize>() > 4096)
        {
            return Err("Agent event is too large".into());
        }
        let tags = self.attested(key, tags)?;
        key.signed(kind, content, tags)
    }
    /// Whether the community accepted this row's current public metadata.
    pub fn profile_is_current(&self) -> bool {
        self.profile.as_ref().is_some_and(|last| {
            last.name == self.name && last.picture == self.picture && last.about == self.about
        })
    }
    /// Its profile as this row describes it, replacing the last one accepted.
    pub fn profile_event(&self, key: &Secret) -> Result<serde_json::Value> {
        valid_name(&self.name)?;
        let mut content = serde_json::json!({ "name": self.name, "bot": true });
        if let Some(picture) = &self.picture {
            content["picture"] = picture.as_str().into();
        }
        if let Some(about) = &self.about {
            content["about"] = about.as_str().into();
        }
        let content = content.to_string();
        let tags = self.attested(key, vec![])?;
        key.sign_event_after(
            0,
            content,
            tags,
            self.profile.as_ref().map(|p| p.created_at),
        )
    }
    fn attested(&self, key: &Secret, mut tags: Vec<Vec<String>>) -> Result<Vec<Vec<String>>> {
        if key.pubkey() != self.pubkey {
            return Err("Agent identity changed".into());
        }
        let auth: Vec<String> =
            serde_json::from_str(&self.auth).map_err(|_| "Invalid owner authorization")?;
        tags.retain(|tag| tag.first().map(String::as_str) != Some("auth"));
        tags.push(auth);
        Ok(tags)
    }
    /// NIP-98 authorization for posting `body` to this agent's community, as
    /// a write (`/events`) or a read (`/query`).
    pub fn http_auth(&self, key: &Secret, url: &str, body: &[u8]) -> Result<serde_json::Value> {
        if key.pubkey() != self.pubkey {
            return Err("Agent identity changed".into());
        }
        if url != self.events_url() && url != self.query_url() {
            return Err("Agents post only to their community".into());
        }
        key.profile_auth(url, body)
    }
    /// Blossom authorization (24242) to upload the blob with this SHA-256 to
    /// its community, valid for a minute.
    pub fn upload_auth(&self, key: &Secret, sha256: &str) -> Result<serde_json::Value> {
        if !crate::config::canonical_key(sha256) {
            return Err("Invalid upload hash".into());
        }
        self.blossom_auth(
            key,
            "Upload file",
            vec![
                vec!["t".into(), "upload".into()],
                vec!["x".into(), sha256.into()],
            ],
        )
    }
    /// Blossom authorization (24242) to fetch media from its community, valid
    /// for a minute.
    pub fn media_auth(&self, key: &Secret) -> Result<serde_json::Value> {
        self.blossom_auth(key, "Get media", vec![vec!["t".into(), "get".into()]])
    }
    fn blossom_auth(
        &self,
        key: &Secret,
        content: &str,
        mut tags: Vec<Vec<String>>,
    ) -> Result<serde_json::Value> {
        if key.pubkey() != self.pubkey {
            return Err("Agent identity changed".into());
        }
        let expiration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "System clock is unavailable")?
            .as_secs()
            + 60;
        let server = self.relay.trim_start_matches("wss://");
        tags.push(vec!["expiration".into(), expiration.to_string()]);
        tags.push(vec!["server".into(), server.into()]);
        key.sign_event_after(24242, content.into(), tags, None)
    }
    /// Its memory entry `slug`, encrypted to its owner and newer than the
    /// entry it replaces (`after`, that entry's `created_at`).
    pub fn memory(
        &self,
        key: &Secret,
        slug: &str,
        body: &str,
        after: u64,
    ) -> Result<serde_json::Value> {
        if key.pubkey() != self.pubkey {
            return Err("Agent identity changed".into());
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "System clock is unavailable")?
            .as_secs();
        key.memory_event(&self.owner, slug, body, now.max(after.saturating_add(1)))
    }
}

/// A key awaiting its owner attestation. Dropping a generated one discards it.
pub struct NewAppAgent {
    key: Secret,
    relay: String,
    owner: String,
    agent_type: String,
    name: String,
    picture: Option<String>,
    about: Option<String>,
    /// An earlier agent system's key, which may already be saved under this ID.
    adopted: bool,
}
impl NewAppAgent {
    pub fn pubkey(&self) -> &str {
        self.key.pubkey()
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
}

/// The saved identities, one JSON file under app data. Reads see a whole file
/// (writes rename into place); each read-modify-write holds a file lock beside it,
/// since more than one app instance can share the directory.
#[derive(Clone)]
pub struct AppAgents {
    path: PathBuf,
}
impl AppAgents {
    pub fn open(path: PathBuf) -> Self {
        Self { path }
    }
    /// Every row, deleted ones included.
    pub fn list(&self) -> Result<Vec<AppAgent>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                // Older development builds persisted this boolean. Ignore only
                // that obsolete field; all other row/schema checks still apply.
                let malformed = || "Saved agent identities are malformed".to_owned();
                let mut rows: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|_| malformed())?;
                for row in rows.as_array_mut().ok_or_else(malformed)? {
                    if let Some(adopted) = row.as_object_mut().and_then(|row| row.remove("adopted"))
                    {
                        if !adopted.is_boolean() {
                            return Err(malformed());
                        }
                    }
                }
                serde_json::from_value(rows).map_err(|_| malformed())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(_) => Err("Could not read saved agent identities".into()),
        }
    }
    /// An agent that still has its key.
    pub fn get(&self, pubkey: &str) -> Result<AppAgent> {
        self.list()?
            .into_iter()
            .find(|agent| agent.pubkey == pubkey && !agent.deleted)
            .ok_or_else(|| "No such agent on this device".into())
    }
    /// Runs `change` on the saved list while holding the cross-process lock, and
    /// saves what it returns.
    fn update<T>(&self, change: impl FnOnce(&mut Vec<AppAgent>) -> Result<T>) -> Result<T> {
        let dir = self.path.parent().ok_or("Invalid agent identity storage")?;
        std::fs::create_dir_all(dir).map_err(|_| "Could not create agent identity storage")?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(self.path.with_extension("lock"))
            .map_err(|_| "Could not open agent identity lock")?;
        lock.lock().map_err(|_| "Could not lock agent identities")?;
        let mut agents = self.list()?;
        let result = change(&mut agents)?;
        self.write(&agents)?;
        Ok(result)
    }
    fn write(&self, agents: &[AppAgent]) -> Result<()> {
        let bytes =
            serde_json::to_vec_pretty(agents).map_err(|_| "Could not encode agent identities")?;
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, bytes).map_err(|_| "Could not save agent identities")?;
        std::fs::rename(&temp, &self.path).map_err(|_| "Could not save agent identities".into())
    }
    /// Changes one agent that still has its key.
    fn edit(&self, pubkey: &str, change: impl FnOnce(&mut AppAgent)) -> Result<()> {
        self.update(|agents| {
            let agent = agents
                .iter_mut()
                .find(|agent| agent.pubkey == pubkey && !agent.deleted)
                .ok_or("No such agent on this device")?;
            change(agent);
            Ok(())
        })
    }
    pub fn prepare(
        destination: &str,
        owner: &str,
        agent_type: &str,
        name: &str,
    ) -> Result<NewAppAgent> {
        let mut agent = Self::adopt(
            destination,
            owner,
            agent_type,
            Secret::generate()?,
            name,
            None,
            None,
        )?;
        agent.adopted = false;
        Ok(agent)
    }
    /// As `prepare`, for a key an earlier agent system saved, so the agent keeps
    /// its identity and public profile metadata.
    pub fn adopt(
        destination: &str,
        owner: &str,
        agent_type: &str,
        key: Secret,
        name: &str,
        picture: Option<String>,
        about: Option<String>,
    ) -> Result<NewAppAgent> {
        if !canonical_key(owner) {
            return Err("Choose a signed-in owner".into());
        }
        valid_type(agent_type)?;
        Ok(NewAppAgent {
            relay: canonical_relay(destination)?,
            key,
            owner: owner.into(),
            agent_type: agent_type.into(),
            name: valid_name(name)?,
            picture: picture.filter(|url| crate::port::public_picture(url)),
            about,
            adopted: true,
        })
    }
    /// Saves the identity, then the key, so a saved key always has an identity to
    /// delete it by. If the key cannot be saved, the identity is withdrawn once
    /// its key is known to be gone; otherwise it stays for `remove` to retry.
    /// An attestation for another key or owner is refused. Returns the key too,
    /// so its first use needs no credential read.
    pub fn commit(
        &self,
        prepared: NewAppAgent,
        auth: &[String],
        credentials: &dyn Credentials,
    ) -> Result<(AppAgent, Secret)> {
        let auth = serde_json::to_string(auth).map_err(|_| "Invalid owner authorization")?;
        validate_attestation(&auth, prepared.key.pubkey())?;
        let tag: Vec<String> =
            serde_json::from_str(&auth).map_err(|_| "Invalid owner authorization")?;
        if tag[1] != prepared.owner {
            return Err("Agent authorization belongs to another owner".into());
        }
        let agent = AppAgent {
            pubkey: prepared.key.pubkey().into(),
            relay: prepared.relay,
            owner: prepared.owner,
            auth,
            agent_type: prepared.agent_type,
            name: prepared.name,
            picture: prepared.picture,
            about: prepared.about,
            profile: None,
            deleted: false,
        };
        let id = agent.credential_id();
        // A harness agent saved for the same community keeps this key under the
        // same ID. It is shared, never added again or deleted on failure here.
        let shared = prepared.adopted && credentials.read(&id, &agent.pubkey)?.is_some();
        self.update(|agents| {
            if prepared.adopted {
                if let Some(saved) = agents.iter().find(|saved| saved.pubkey == agent.pubkey) {
                    return Err(if saved.deleted {
                        "That agent is still being removed here; try again shortly".into()
                    } else {
                        "That agent is already here".into()
                    });
                }
            }
            agents.retain(|saved| saved.pubkey != agent.pubkey);
            agents.push(agent.clone());
            Ok(())
        })?;
        if shared {
            return Ok((agent, prepared.key));
        }
        let added = credentials.add(&id, &prepared.key);
        let refused = added.is_err();
        let saved = added.and_then(|()| {
            agent
                .read_key(credentials)
                .map(drop)
                .map_err(|_| "New agent key could not be verified".into())
        });
        if let Err(error) = saved {
            // Another writer may have saved the same adopted key meanwhile; that
            // copy is theirs as much as this agent's.
            if prepared.adopted && matches!(credentials.read(&id, &agent.pubkey), Ok(Some(_))) {
                return Ok((agent, prepared.key));
            }
            // A refused save stored no key, so nothing can be lost: the identity
            // goes even when the store refuses everything, as an unavailable one
            // does, rather than staying behind without a key.
            if refused || prepared.adopted || credentials.delete(&id, &agent.pubkey).is_ok() {
                let _ = self.update(|agents| {
                    agents.retain(|saved| saved.pubkey != agent.pubkey);
                    Ok(())
                });
            }
            return Err(error);
        }
        Ok((agent, prepared.key))
    }
    pub fn rename(&self, pubkey: &str, name: &str) -> Result<()> {
        let name = valid_name(name)?;
        self.edit(pubkey, |agent| agent.name = name)
    }
    /// Makes it an agent of another type; its identity and profile stay.
    pub fn set_type(&self, pubkey: &str, agent_type: &str) -> Result<()> {
        valid_type(agent_type)?;
        self.edit(pubkey, |agent| agent.agent_type = agent_type.into())
    }
    /// Sets or, with `None`, clears the profile picture: an `https` URL without
    /// credentials.
    pub fn set_picture(&self, pubkey: &str, picture: Option<String>) -> Result<()> {
        if picture
            .as_deref()
            .is_some_and(|url| !crate::port::public_picture(url))
        {
            return Err("Use an HTTPS image URL without credentials".into());
        }
        self.edit(pubkey, |agent| agent.picture = picture)
    }
    /// Records a profile its community accepted, unless a newer one already is.
    pub fn published(&self, pubkey: &str, profile: Published) -> Result<()> {
        self.edit(pubkey, |agent| {
            if agent
                .profile
                .as_ref()
                .is_none_or(|last| last.created_at < profile.created_at)
            {
                agent.profile = Some(profile);
            }
        })
    }
    /// Deletes the key, then marks the row deleted, so a failed deletion can be
    /// retried. Absence is success.
    pub fn remove(&self, pubkey: &str, credentials: &dyn Credentials) -> Result<()> {
        let Some(agent) = self
            .list()?
            .into_iter()
            .find(|agent| agent.pubkey == pubkey && !agent.deleted)
        else {
            return Ok(());
        };
        credentials.delete(&agent.credential_id(), &agent.pubkey)?;
        self.edit(pubkey, |agent| agent.deleted = true)
    }
    /// Deletes every key it still has, as an erase that removes agents does
    /// before the rows go with app data. Absence is success, so it repeats safely.
    pub fn delete_keys(&self, credentials: &dyn Credentials) -> Result<()> {
        for agent in self.list()?.into_iter().filter(|agent| !agent.deleted) {
            credentials.delete(&agent.credential_id(), &agent.pubkey)?;
        }
        Ok(())
    }
    /// Drops a deleted row once its community has been cleaned up.
    pub fn forget(&self, pubkey: &str) -> Result<()> {
        self.update(|agents| {
            agents.retain(|agent| agent.pubkey != pubkey || !agent.deleted);
            Ok(())
        })
    }
}

fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err("Name the agent in one line".into());
    }
    Ok(name.into())
}
fn valid_type(agent_type: &str) -> Result<()> {
    if agent_type.is_empty() || agent_type.len() > 256 {
        return Err("Choose an agent type".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
