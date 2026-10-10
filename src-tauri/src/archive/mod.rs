//! Bounded host-owned archive. Only encrypted signed envelopes reach SQLite.
use crate::{identity::IdentityHost, relay::agent::decode_archive};
use buzz_desktop::archive::Reply;
pub(crate) use buzz_desktop::archive::{Archive as ArchiveHost, Request};
use serde_json::Value;
use tauri::Manager;

type Result<T> = std::result::Result<T, String>;
#[tauri::command]
pub(crate) async fn relay_archive<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    identity: tauri::State<'_, IdentityHost>,
    archive: tauri::State<'_, ArchiveHost>,
    community: String,
    viewer: String,
    request: Request,
) -> Result<Value> {
    let community = crate::relay::origin(&community)?
        .origin()
        .ascii_serialization();
    // Initialization failure stays retryable and cannot disable unrelated app startup.
    let path = app
        .path()
        .app_data_dir()
        .map_err(|_| "Archive location unavailable")?
        .join(if cfg!(debug_assertions) {
            "archive-debug"
        } else {
            "archive"
        })
        .join("events.sqlite3");
    execute(
        identity.inner(),
        archive.inner().clone(),
        path,
        community,
        viewer,
        request,
    )
    .await
}
async fn execute(
    identity: &IdentityHost,
    archive: ArchiveHost,
    path: std::path::PathBuf,
    community: String,
    viewer: String,
    request: Request,
) -> Result<Value> {
    if !matches!(request, Request::Ingest { .. }) && identity.viewer().await? != viewer {
        return Err("Archive viewer changed".into());
    }
    let request = if let Request::Ingest { event, revision } = request {
        let expected = viewer.clone();
        identity
            .with_key(move |secret, current| {
                if current != expected {
                    return Err("Archive viewer changed".into());
                }
                // The trusted main renderer supplies fresh envelopes. This verifies
                // cryptography/recipient, not relay delivery or agent ownership.
                decode_archive(secret, current, &event, false)?;
                Ok(Request::Ingest { event, revision })
            })
            .await?
    } else {
        request
    };
    let read_only = matches!(request, Request::Settings);
    let partition = viewer.clone();
    // Neither waiting for the archive mutex nor any SQLite work holds the key
    // owner's lock. An admitted write stays in its original account partition.
    let reply = tauri::async_runtime::spawn_blocking(move || {
        archive.run(path, &community, &partition, request)
    })
    .await
    .map_err(|_| "Archive task failed")??;
    match reply {
        Reply::Complete(value) => {
            if read_only && identity.viewer().await? != viewer {
                return Err("Archive viewer changed".into());
            }
            Ok(value)
        }
        Reply::Page(page) => {
            identity
                .with_key(move |secret, current| {
                    // Recheck after the database wait, before decrypting for the renderer.
                    if current != viewer {
                        return Err("Archive viewer changed".into());
                    }
                    page.decode(secret, current)
                })
                .await
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
