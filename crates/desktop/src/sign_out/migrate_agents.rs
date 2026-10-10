//! TEMPORARY agents rename: remove after 2026-10-18, once development users
//! have upgraded, together with the boot call and legacy erase/keep entries.
use super::{real_dir, wait_for, Locks, Paths, AGENTS};

pub const LEGACY: &str = "agents2";

#[allow(clippy::incompatible_msrv)] // File locking, like the owning sign_out module.
pub(super) fn migrate(paths: &Paths, locks: &Locks) -> Result<(), String> {
    let old = paths.app_data.join(LEGACY);
    let new = paths.app_data.join(AGENTS);
    let failed = |error: std::io::Error| format!("Could not migrate agent storage: {error}");
    if !real_dir(&old).map_err(failed)? {
        return Ok(());
    }
    // Another running build may still write the old path. Reuse Buzz's shared
    // instance lock rather than moving files out from under it. Our key lock
    // stays shared so sign-out cannot start during the migration.
    locks.all.unlock().map_err(failed)?;
    if !wait_for(|| locks.all.try_lock()) {
        return Err("Close other Buzz copies, then reopen Buzz to migrate agent storage. Nothing was moved.".into());
    }
    // Recheck after acquiring the lock: another upgrading process may have
    // completed the move while we waited. Never replace an existing directory.
    if real_dir(&old).map_err(failed)? {
        if real_dir(&new).map_err(failed)? {
            return Err("Both agents and agents2 storage exist. Resolve the conflicting folders before reopening Buzz; nothing was moved.".into());
        }
        std::fs::rename(&old, &new).map_err(failed)?;
    }
    locks.all.unlock().map_err(failed)?;
    if !wait_for(|| locks.all.try_lock_shared()) {
        return Err(super::SIGNING_OUT.into());
    }
    Ok(())
}
