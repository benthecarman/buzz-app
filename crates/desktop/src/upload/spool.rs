//! Host-owned upload files. The renderer supplies bounded bytes, never paths.
use super::{upload_id, validate_upload_size, Result, Uploads, UPLOAD_CHUNK};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};

const SLOTS: usize = 4;
const IDLE: Duration = Duration::from_secs(60);

/// One received upload on disk. Its files and resource permit live through
/// conversion and upstream sending, and are removed when it drops.
pub struct Spool {
    pub(crate) directory: tempfile::TempDir,
    pub(crate) size: usize,
    pub(crate) hash: String,
    _slot: OwnedSemaphorePermit,
}
impl Spool {
    /// The SHA-256 of the received bytes, in lowercase hex.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// The received (unconverted) file.
    pub fn source(&self) -> PathBuf {
        self.directory.path().join("source")
    }
}
struct Receiving {
    file: File,
    spool: Spool,
    hash: Sha256,
    received: usize,
    touched: Instant,
    cancelled: oneshot::Receiver<()>,
}
type Entry = Arc<Mutex<Option<Receiving>>>;

struct Root {
    // Declared first so the lock closes before TempDir removes it on Windows.
    _lock: File,
    directory: tempfile::TempDir,
}

/// Upload files being received, under one owned, locked temporary root.
/// Leftovers of earlier sessions are removed when no live process owns them.
pub struct Spools {
    receiving: Mutex<HashMap<String, Entry>>,
    slots: Arc<Semaphore>,
    parent: Result<PathBuf>,
    root: Mutex<Option<Root>>,
}

pub(crate) fn private_tempdir_in(parent: &Path) -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("upload-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir_in(parent)
}

fn root(parent: &Path) -> std::io::Result<Root> {
    std::fs::create_dir_all(parent)?;
    // Serialize creation/recovery so another app cannot reclaim a new directory
    // before its ownership lock is installed. Never touch another live process.
    let gate = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join("ownership"))?;
    fs2::FileExt::lock_exclusive(&gate)?;
    // Recovery is opportunistic: inaccessible leftovers must not prevent this
    // session from creating its own storage. Required gate/session locks still fail.
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("upload-")
                || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                continue;
            }
            let path = entry.path();
            let lock = File::options()
                .read(true)
                .write(true)
                .open(path.join("ownership"));
            match lock {
                Ok(lock) if fs2::FileExt::try_lock_exclusive(&lock).is_ok() => {
                    drop(lock);
                    let _ = std::fs::remove_dir_all(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let _ = std::fs::remove_dir_all(path);
                }
                _ => {}
            }
        }
    }
    let directory = private_tempdir_in(parent)?;
    let lock = File::options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(directory.path().join("ownership"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    Ok(Root {
        _lock: lock,
        directory,
    })
}

impl Spools {
    /// Spools under `parent`, such as the app cache directory; an error is
    /// reported by each upload that needs the directory.
    pub fn new(parent: Result<PathBuf>) -> Self {
        let initial_root = parent.as_ref().ok().and_then(|path| root(path).ok());
        Self {
            receiving: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(SLOTS)),
            parent,
            root: Mutex::new(initial_root),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.receiving
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    // No filesystem work or per-upload lock acquisition under the map lock.
    fn entry(&self, id: &str) -> Result<Entry> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| "Upload cancelled".into())
    }
    fn remove(&self, id: &str, entry: &Entry) {
        let removed = {
            let mut entries = self.lock();
            if entries
                .get(id)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
            {
                entries.remove(id)
            } else {
                None
            }
        };
        drop(removed);
    }
    /// Reserves storage for upload `id` of `size` bytes.
    pub fn begin(&self, uploads: &Uploads, id: &str, size: usize) -> Result<()> {
        upload_id(Some(id))?;
        validate_upload_size(size)?;
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "Uploads are busy")?;
        let mut owned_root = self
            .root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owned_root.is_none() {
            let parent = self.parent.as_ref().map_err(Clone::clone)?;
            *owned_root = Some(
                root(parent).map_err(|_| "Media preparation could not access temporary storage")?,
            );
        }
        let root_path = owned_root.as_ref().unwrap().directory.path().to_owned();
        drop(owned_root);
        let entry = Arc::new(Mutex::new(None));
        let mut cancelled = {
            let mut entries = self.lock();
            if entries.contains_key(id) {
                return Err("Upload is already in progress".into());
            }
            let cancelled = uploads.start(id)?.ok_or("Upload cancelled")?;
            entries.insert(id.into(), entry.clone());
            cancelled
        };
        let result = (|| {
            let mut receiving = entry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let directory = private_tempdir_in(&root_path)
                .map_err(|_| "Media preparation could not access temporary storage")?;
            let file = File::options()
                .create_new(true)
                .write(true)
                .open(directory.path().join("source"))
                .map_err(|_| "Media preparation could not access temporary storage")?;
            if cancelled.try_recv() != Err(oneshot::error::TryRecvError::Empty) {
                return Err("Upload cancelled".into());
            }
            *receiving = Some(Receiving {
                spool: Spool {
                    directory,
                    size,
                    hash: String::new(),
                    _slot: slot,
                },
                file,
                hash: Sha256::new(),
                received: 0,
                touched: Instant::now(),
                cancelled,
            });
            Ok(())
        })();
        if result.is_err() {
            // Finish while removing our reservation so a reused ID cannot have
            // its newer cancellation sender removed by this failed begin.
            let removed = {
                let mut entries = self.lock();
                if entries
                    .get(id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    uploads.finish(id);
                    entries.remove(id)
                } else {
                    None
                }
            };
            drop(removed);
        }
        result
    }
    /// Appends the chunk at `offset`; any failure discards the upload.
    pub fn append(&self, id: &str, offset: usize, bytes: &[u8]) -> Result<()> {
        let entry = self.entry(id)?;
        let mut receiving = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = (|| {
            let entry = receiving.as_mut().ok_or("Upload cancelled")?;
            if entry.cancelled.try_recv() != Err(oneshot::error::TryRecvError::Empty) {
                return Err("Upload cancelled".into());
            }
            if bytes.is_empty()
                || bytes.len() > UPLOAD_CHUNK
                || offset != entry.received
                || bytes.len() > entry.spool.size - entry.received
            {
                return Err("Invalid upload chunk".into());
            }
            entry
                .file
                .write_all(bytes)
                .map_err(|_| "Media preparation could not access temporary storage")?;
            if entry.cancelled.try_recv() != Err(oneshot::error::TryRecvError::Empty) {
                return Err("Upload cancelled".into());
            }
            entry.hash.update(bytes);
            entry.received += bytes.len();
            entry.touched = Instant::now();
            Ok(())
        })();
        if result.is_err() {
            receiving.take();
            self.remove(id, &entry);
        }
        result
    }
    /// The complete spool and its cancellation, removed from receiving.
    pub fn take(&self, id: &str) -> Result<(Spool, oneshot::Receiver<()>)> {
        let entry = self.lock().remove(id).ok_or("Upload cancelled")?;
        let mut entry = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or("Upload cancelled")?;
        if entry.received != entry.spool.size {
            return Err("Incomplete upload".into());
        }
        entry
            .file
            .flush()
            .map_err(|_| "Media preparation could not access temporary storage")?;
        entry.spool.hash = format!("{:x}", entry.hash.finalize());
        drop(entry.file);
        Ok((entry.spool, entry.cancelled))
    }
    /// Drops whatever remains of upload `id`.
    pub fn discard(&self, id: &str) {
        let removed = self.lock().remove(id);
        drop(removed);
    }
    /// Cancels upload `id`, received or still running.
    pub fn cancel(&self, uploads: &Uploads, id: &str) {
        // Registration and cancellation share the map lock. A begin that arrives
        // afterward consumes the pending cancel; an existing owner is signalled
        // before its receiving entry is removed.
        let removed = {
            let mut entries = self.lock();
            uploads.cancel(id);
            entries.remove(id)
        };
        drop(removed);
    }
    /// Cancels every upload, such as when the window closes.
    pub fn cancel_all(&self, uploads: &Uploads) {
        let removed = {
            let mut entries = self.lock();
            uploads.cancel_all();
            std::mem::take(&mut *entries)
        };
        drop(removed);
    }
    /// Cancels uploads that received nothing for a minute. Call periodically.
    pub fn reap(&self, uploads: &Uploads) {
        let entries: Vec<_> = self
            .lock()
            .iter()
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect();
        for (id, entry) in entries {
            // An in-flight disk operation owns cleanup and is not idle.
            let Ok(mut receiving) = entry.try_lock() else {
                continue;
            };
            if receiving
                .as_ref()
                .is_some_and(|entry| entry.touched.elapsed() >= IDLE)
            {
                let removed = {
                    let mut entries = self.lock();
                    if entries
                        .get(&id)
                        .is_some_and(|current| Arc::ptr_eq(current, &entry))
                    {
                        uploads.cancel(&id);
                        entries.remove(&id)
                    } else {
                        None
                    }
                };
                if removed.is_some() {
                    receiving.take();
                }
                drop(removed);
            }
        }
    }
}

pub(crate) fn hash_file(path: &Path) -> Result<(usize, String)> {
    let mut file =
        File::open(path).map_err(|_| "Media preparation could not access temporary storage")?;
    let mut hash = Sha256::new();
    let mut buffer = [0; UPLOAD_CHUNK];
    let mut size = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "Media preparation could not access temporary storage")?;
        if read == 0 {
            break;
        }
        size += read;
        validate_upload_size(size)?;
        hash.update(&buffer[..read]);
    }
    validate_upload_size(size)?;
    Ok((size, format!("{:x}", hash.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn receiving(spools: &Spools, id: &str) -> Entry {
        spools.entry(id).unwrap()
    }
    fn spool_path(spools: &Spools, id: &str) -> PathBuf {
        receiving(spools, id)
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .spool
            .directory
            .path()
            .to_owned()
    }
    fn fixture() -> (tempfile::TempDir, Spools, Uploads) {
        let parent = tempfile::tempdir().unwrap();
        let spools = Spools::new(Ok(parent.path().to_owned()));
        (parent, spools, Uploads::default())
    }
    #[test]
    fn simultaneous_files_have_exact_hashes_and_cleanup_holds_admission() {
        let (_parent, spools, uploads) = fixture();
        for id in ["a", "b", "c", "d"] {
            spools.begin(&uploads, id, 3).unwrap();
        }
        assert!(spools.begin(&uploads, "extra", 3).is_err());
        assert!(spools.begin(&uploads, "a", 3).is_err());
        spools.append("a", 0, b"abc").unwrap();
        spools.append("b", 0, b"xyz").unwrap();
        let (a, _cancel) = spools.take("a").unwrap();
        let (b, _) = spools.take("b").unwrap();
        let path = a.directory.path().to_owned();
        assert_eq!(std::fs::read(a.source()).unwrap(), b"abc");
        assert_eq!(a.hash, format!("{:x}", Sha256::digest(b"abc")));
        assert_eq!(std::fs::read(b.source()).unwrap(), b"xyz");
        assert!(spools.begin(&uploads, "extra", 3).is_err());
        drop(a);
        assert!(!path.exists());
        uploads.finish("a");
        spools.begin(&uploads, "extra", 3).unwrap();
    }
    #[test]
    fn cancellation_partial_and_invalid_chunks_remove_files() {
        let (_parent, spools, uploads) = fixture();
        for (id, offset, bytes) in [
            ("wrong-offset", 1, b"a".as_slice()),
            ("empty", 0, b""),
            ("overflow", 0, b"four"),
        ] {
            spools.begin(&uploads, id, 3).unwrap();
            let path = spool_path(&spools, id);
            assert!(spools.append(id, offset, bytes).is_err());
            assert!(!path.exists());
            uploads.finish(id);
        }
        spools.begin(&uploads, "partial", 3).unwrap();
        let path = spool_path(&spools, "partial");
        spools.append("partial", 0, b"a").unwrap();
        assert!(spools.take("partial").is_err());
        assert!(!path.exists());
        uploads.finish("partial");
        spools.begin(&uploads, "cancel", 3).unwrap();
        let path = spool_path(&spools, "cancel");
        spools.cancel(&uploads, "cancel");
        assert!(!path.exists());
        assert!(spools.append("cancel", 0, b"abc").is_err());
        uploads.cancel("before");
        assert!(spools.begin(&uploads, "before", 3).is_err());
    }
    #[test]
    fn idle_and_page_reload_release_receiving_and_running_work() {
        let (_parent, spools, uploads) = fixture();
        spools.begin(&uploads, "idle", 3).unwrap();
        let path = spool_path(&spools, "idle");
        receiving(&spools, "idle")
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .touched = Instant::now() - IDLE;
        spools.reap(&uploads);
        assert!(!path.exists());
        spools.begin(&uploads, "running", 3).unwrap();
        spools.append("running", 0, b"abc").unwrap();
        let (running, mut cancel) = spools.take("running").unwrap();
        let path = running.directory.path().to_owned();
        spools.cancel_all(&uploads);
        assert!(cancel.try_recv().is_ok());
        assert!(
            path.exists(),
            "running operation owns cleanup until stopped"
        );
        drop(running);
        assert!(!path.exists());
    }
    #[test]
    fn busy_upload_does_not_hold_map_or_other_uploads_and_cancel_wins() {
        let (_parent, spools, uploads) = fixture();
        spools.begin(&uploads, "busy", 3).unwrap();
        spools.begin(&uploads, "other", 3).unwrap();
        let busy = receiving(&spools, "busy");
        let path = spool_path(&spools, "busy");
        // Hold the exact mutex used across write_all. No timing/sleep required.
        let held = busy.lock().unwrap();
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    spools.append("other", 0, b"abc").unwrap();
                    let (other, _) = spools.take("other").unwrap();
                    assert_eq!(std::fs::read(other.source()).unwrap(), b"abc");
                    spools.begin(&uploads, "new", 3).unwrap();
                    spools.reap(&uploads);
                    spools.cancel(&uploads, "busy");
                    assert!(spools.entry("busy").is_err());
                })
                .join()
                .unwrap();
        });
        assert!(path.exists(), "in-flight owner must retain storage");
        drop(held);
        // Simulate an append that looked up its owner just before cancellation.
        let mut receiving = busy.lock().unwrap();
        assert!(receiving.as_mut().unwrap().cancelled.try_recv().is_ok());
        receiving.take();
        drop(receiving);
        drop(busy);
        assert!(!path.exists());
        assert_eq!(spools.slots.available_permits(), SLOTS - 1);
    }
    #[test]
    fn recovery_reclaims_interrupted_sessions_but_not_live_owners() {
        let parent = tempfile::tempdir().unwrap();
        let live = root(parent.path()).unwrap();
        let live_path = live.directory.path().to_owned();
        let stale = private_tempdir_in(parent.path()).unwrap();
        let stale_path = stale.keep();
        std::fs::write(stale_path.join("source"), b"abandoned bytes").unwrap();
        File::create(stale_path.join("ownership")).unwrap();
        let next = root(parent.path()).unwrap();
        assert!(!stale_path.exists());
        assert!(live_path.exists());
        drop(next);
        drop(live);
        assert!(!live_path.exists());
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn undeletable_stale_entry_does_not_block_new_uploads() {
        let parent = tempfile::tempdir().unwrap();
        let stale = private_tempdir_in(parent.path()).unwrap();
        let blocked = stale.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        let file = blocked.join("source");
        std::fs::write(&file, b"abandoned").unwrap();
        File::create(stale.path().join("ownership")).unwrap();
        #[cfg(unix)]
        let original = {
            use std::os::unix::fs::PermissionsExt;
            let original = std::fs::metadata(&blocked).unwrap().permissions();
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o500)).unwrap();
            original
        };
        #[cfg(windows)]
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            // Read-only attributes can be ignored by remove_dir_all. Denying
            // delete sharing models a stale file still held by another process.
            File::options()
                .read(true)
                .share_mode(0)
                .open(&file)
                .unwrap()
        };
        let removal = std::fs::remove_dir_all(stale.path());
        let spools = Spools::new(Ok(parent.path().to_owned()));
        let uploads = Uploads::default();
        let admission = spools.begin(&uploads, "new", 3);
        // Release the fixture's restriction before assertions, even on failure.
        #[cfg(unix)]
        std::fs::set_permissions(&blocked, original).unwrap();
        #[cfg(windows)]
        drop(held);
        assert!(removal.is_err(), "fixture must actually reject removal");
        assert!(spools.root.lock().unwrap().is_some());
        admission.unwrap();
        spools.append("new", 0, b"abc").unwrap();
        assert_eq!(
            std::fs::read(spools.take("new").unwrap().0.source()).unwrap(),
            b"abc"
        );
    }
    #[test]
    fn storage_failure_does_not_reserve_upload_capacity() {
        let parent = tempfile::tempdir().unwrap();
        let file = parent.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();
        let spools = Spools::new(Ok(file));
        let uploads = Uploads::default();
        assert!(spools
            .begin(&uploads, "failure", 3)
            .unwrap_err()
            .contains("temporary storage"));
        assert_eq!(spools.slots.available_permits(), SLOTS);
        assert!(uploads.lock().active.is_empty());
    }
    #[test]
    fn failed_root_initialization_recovers_on_same_instance_without_reclaiming_live_owner() {
        let parent = tempfile::tempdir().unwrap();
        let storage = parent.path().join("storage");
        std::fs::write(&storage, b"unavailable").unwrap();
        let spools = Spools::new(Ok(storage.clone()));
        let uploads = Uploads::default();
        assert!(spools.begin(&uploads, "failed", 3).is_err());
        assert_eq!(spools.slots.available_permits(), SLOTS);
        assert!(uploads.lock().active.is_empty());
        std::fs::remove_file(&storage).unwrap();
        let live = root(&storage).unwrap();
        let live_path = live.directory.path().to_owned();
        spools.begin(&uploads, "recovered", 3).unwrap();
        assert!(live_path.exists());
        spools.append("recovered", 0, b"abc").unwrap();
        let (spool, _) = spools.take("recovered").unwrap();
        assert_eq!(std::fs::read(spool.source()).unwrap(), b"abc");
        assert_eq!(spool.hash, format!("{:x}", Sha256::digest(b"abc")));
        assert_eq!(spools.slots.available_permits(), SLOTS - 1);
        drop(spool);
        uploads.finish("recovered");
        assert_eq!(spools.slots.available_permits(), SLOTS);
        assert!(live_path.exists());
    }
    #[test]
    fn cancellation_overtakes_begin_waiting_for_root_without_stranding_slot() {
        let (_parent, spools, uploads) = fixture();
        let root = spools.root.lock().unwrap();
        std::thread::scope(|scope| {
            let (started, entering) = std::sync::mpsc::channel();
            let spools_ref = &spools;
            let uploads_ref = &uploads;
            let begin = scope.spawn(move || {
                started.send(()).unwrap();
                spools_ref.begin(uploads_ref, "pending", 3)
            });
            entering.recv().unwrap();
            // Hold root initialization until the cancellation has been recorded.
            // The pending begin cannot register while this guard is held.
            spools.cancel(&uploads, "pending");
            drop(root);
            assert_eq!(begin.join().unwrap().unwrap_err(), "Upload cancelled");
        });
        assert_eq!(spools.slots.available_permits(), SLOTS);
        assert!(uploads.lock().active.is_empty());
        assert!(spools.entry("pending").is_err());
    }
    fn assert_write_failure_cleans_partial_file(file: impl FnOnce(&Path) -> File) {
        let (_parent, spools, uploads) = fixture();
        spools.begin(&uploads, "failure", 3).unwrap();
        spools.append("failure", 0, b"a").unwrap();
        let path = spool_path(&spools, "failure");
        receiving(&spools, "failure")
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .file = file(&path.join("source"));
        assert!(spools
            .append("failure", 1, b"bc")
            .unwrap_err()
            .contains("temporary storage"));
        assert!(!path.exists());
        assert_eq!(spools.slots.available_permits(), SLOTS);
        uploads.cancel("failure");
        spools.begin(&uploads, "retry", 3).unwrap();
        spools.append("retry", 0, b"abc").unwrap();
        assert_eq!(
            std::fs::read(spools.take("retry").unwrap().0.source()).unwrap(),
            b"abc"
        );
    }
    #[test]
    fn write_failure_cleans_partial_file_and_releases_slot() {
        // Read-only descriptors reject writes on every supported OS without
        // exhausting a disk, changing permissions or silently skipping coverage.
        assert_write_failure_cleans_partial_file(|path| File::open(path).unwrap());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn enospc_cleans_partial_file_and_releases_slot() {
        assert_write_failure_cleans_partial_file(|_| {
            File::options().write(true).open("/dev/full").unwrap()
        });
    }
}
