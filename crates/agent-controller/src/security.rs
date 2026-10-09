//! Generic, native-owned launch protection. Provider code and policy schemas are external.
pub use crate::store::Binding;
use crate::{config::Agent, runtime::Controller, store::PROTECTION_KEY as KEY, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone)]
pub(crate) struct Provider {
    executable: PathBuf,
    digest: Vec<u8>,
    lease: String,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Request {
    Snapshot,
    Register {
        provider: String,
        executable: PathBuf,
    },
    Unregister {
        provider: String,
        lease: String,
    },
    Defaults {
        revision: u64,
        binding: Option<Binding>,
    },
    Agent {
        id: String,
        revision: u64,
        binding: Option<Binding>,
    },
}
fn executable_bytes(path: &Path) -> Result<Vec<u8>> {
    if !path.is_absolute() {
        return Err("Choose an absolute protection launcher path".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| "Protection launcher is unavailable")?;
    let meta = file
        .metadata()
        .map_err(|_| "Cannot inspect protection launcher")?;
    if !meta.is_file() || meta.len() > 256 * 1024 * 1024 {
        return Err("Protection launcher must be a bounded regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err("Protection launcher is not executable".into());
        }
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(256 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read protection launcher")?;
    if bytes.len() > 256 * 1024 * 1024 {
        return Err("Protection launcher exceeds size limit".into());
    }
    Ok(bytes)
}
impl Controller {
    pub fn protect_control_paths(&mut self, paths: Result<Vec<PathBuf>>) {
        self.protection_paths = paths;
    }

    pub fn security(&mut self, request: Request) -> Result<Value> {
        match request {
            Request::Snapshot => self.security_snapshot(),
            Request::Register {
                provider,
                executable,
            } => {
                if !Binding::valid_provider(&provider) {
                    return Err("Invalid protection provider".into());
                }
                let digest = Sha256::digest(executable_bytes(&executable)?).to_vec();
                let mut bytes = [0u8; 24];
                getrandom::fill(&mut bytes).map_err(|_| "Cannot create provider lease")?;
                let lease: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                self.security_providers.insert(
                    provider,
                    Provider {
                        executable,
                        digest,
                        lease: lease.clone(),
                    },
                );
                Ok(json!({"lease": lease}))
            }
            Request::Unregister { provider, lease } => {
                if self
                    .security_providers
                    .get(&provider)
                    .is_some_and(|p| p.lease == lease)
                {
                    self.security_providers.remove(&provider);
                }
                self.security_snapshot()
            }
            Request::Defaults { revision, binding } => {
                self.require_provider(&binding)?;
                self.store
                    .set_launch_protection_defaults(revision, binding)?;
                self.security_snapshot()
            }
            Request::Agent {
                id,
                revision: expected,
                binding,
            } => {
                self.require_provider(&binding)?;
                self.store.set_launch_protection(&id, expected, binding)?;
                self.security_snapshot()
            }
        }
    }
    fn require_provider(&self, binding: &Option<Binding>) -> Result<()> {
        if binding
            .as_ref()
            .is_some_and(|b| !self.security_providers.contains_key(&b.provider))
        {
            return Err("Enable the protection provider before saving".into());
        }
        Ok(())
    }
    fn security_snapshot(&self) -> Result<Value> {
        let mut snapshot = self.store.launch_protection_snapshot()?;
        snapshot["availableProviders"] = json!(self.security_providers.keys().collect::<Vec<_>>());
        Ok(snapshot)
    }
    pub fn security_restore_ids(&mut self, provider: &str) -> Result<Vec<String>> {
        let running: Vec<_> = self
            .snapshot()?
            .agents
            .into_iter()
            .filter(|a| a.status == crate::ProcessStatus::Running)
            .map(|a| a.id)
            .collect();
        Ok(self
            .store
            .agents()?
            .iter()
            .filter(|a| a.starts_on_launch() && !running.contains(&a.id))
            .filter_map(|a| {
                Binding::decode(a.extra.get(KEY))
                    .ok()
                    .flatten()
                    .filter(|b| b.provider == provider)
                    .map(|_| a.id.clone())
            })
            .collect())
    }
    pub(crate) fn wrap_protected_worker(
        &self,
        agent: &Agent,
        command: &mut Command,
    ) -> Result<Option<tempfile::TempDir>> {
        let Some(binding) = Binding::decode(agent.extra.get(KEY))? else {
            return Ok(None);
        };
        let provider = self
            .security_providers
            .get(&binding.provider)
            .ok_or("Required security plugin is unavailable; agent was not started")?;
        if !cfg!(unix) {
            return Err("Protected worker launch requires Unix process containment".into());
        }
        // Hash and stage the SAME bytes: delayed starts and worker replacement
        // must never execute the mutable registration path.
        let bytes = executable_bytes(&provider.executable)?;
        if Sha256::digest(&bytes).as_slice() != provider.digest {
            return Err("Protection launcher changed; re-enable the plugin before starting".into());
        }
        // Every policy covers this namespace, including runs created later.
        let controls = self.store.root().join("run-controls");
        crate::connection::private_directory(&controls)?;
        let snapshot = tempfile::Builder::new()
            .prefix("agent-")
            .tempdir_in(&controls)
            .map_err(|_| "Cannot create private protection directory")?;
        let control = snapshot.path();
        let directory = provider
            .executable
            .parent()
            .ok_or("Invalid protection launcher")?;
        let mut protected_paths = self.protection_paths.clone()?;
        protected_paths.extend(self.store.protected_control_paths());
        protected_paths.push(self.ownership_root.clone());
        protected_paths.push(directory.to_path_buf());
        protected_paths.push(controls);
        if let Some(directory) = self.bundle.as_ref().ok().and_then(|b| b.directory()) {
            protected_paths.push(directory.to_path_buf());
        }
        protected_paths.push(std::env::current_exe().map_err(|_| "Cannot locate host executable")?);
        let context = json!({"version":2, "providerDirectory":directory,
            "policy":binding.policy,"relayUrl":agent.relay_url,"workspace":agent.workspace,
            "protectedPaths":protected_paths});
        let path = control.join("launch-protection.json");
        let launcher = control.join("launcher");
        write_snapshot(&launcher, &bytes, 0o500)?;
        write_snapshot(
            &path,
            &serde_json::to_vec(&context).map_err(|_| "Invalid launch context")?,
            0o400,
        )?;
        command.env(
            "BUZZ_ACP_LAUNCH_PREFIX",
            serde_json::to_string(&json!([launcher, "--launch", path, "--"]))
                .map_err(|_| "Invalid launch prefix")?,
        );
        Ok(Some(snapshot))
    }
}

fn write_snapshot(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    options
        .open(path)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .map_err(|_| "Cannot create private protection launch snapshot".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn nonregular_launchers_are_rejected_without_waiting_for_a_writer() {
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("launcher");
        assert!(Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert!(executable_bytes(&fifo)
            .unwrap_err()
            .contains("regular file"));
        let link = root.path().join("link");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &link).unwrap();
        assert!(executable_bytes(&link).is_err());
    }
}
