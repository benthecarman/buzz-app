//! Sign out commands. `buzz_desktop::sign_out` records intent and erases at
//! the next launch; this supplies the app's identity, pairing, agents and Quit.
pub(crate) use buzz_desktop::sign_out::{
    boot, migrate_agents, Instance, Paths, AGENTS, OLD_AGENTS,
};
use buzz_desktop::sign_out::{Failure, FENCED};
use tauri::Manager;

/// Shown when `boot` fails: no window opens, so nothing recreates wiped storage.
pub(crate) fn exit_with(message: &str) -> ! {
    eprintln!("buzz: {message}");
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Buzz")
        .set_description(message)
        .show();
    std::process::exit(1)
}

/// Tear down as Quit does, however that goes, then explain and exit. Agent
/// processes stop when Buzz exits; a pending erase waits for any ownership lock at launch.
async fn exit_fenced<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<(), Failure> {
    let handle = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        crate::shut_down(&handle);
        exit_with(FENCED)
    }) {
        eprintln!("buzz: could not reach the main thread: {error}");
        exit_with(FENCED)
    }
    // The dialog waits while the main thread tears down and exits.
    std::future::pending().await
}

/// Why wipe is unavailable here, if it is, so the dialog can say so up front;
/// `sign_out` still enforces it.
#[tauri::command]
pub(crate) fn sign_out_wipe_refusal() -> Option<&'static str> {
    buzz_desktop::sign_out::wipe_refusal()
}

/// Records the choices and restarts; the next launch deletes.
#[tauri::command]
pub(crate) async fn sign_out<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    wipe: bool,
    remove_agents: bool,
) -> Result<(), Failure> {
    let paths = Paths::resolve(
        &app.config().identifier,
        buzz_credential_store::HUMAN_SERVICE,
    );
    let instance = app.try_state::<Instance>();
    // Agents run as plugin host processes, which stop when Buzz exits.
    let shutdown = || async { Ok(()) };
    // Close the signer and any pairing that already holds a copy of the key.
    // Looked up on use, so a refusal touches no app state.
    let close = || {
        app.state::<crate::identity::IdentityHost>().close();
        app.state::<crate::pairing::Pairing>().close();
    };
    buzz_desktop::sign_out::sign_out(
        paths.as_ref().zip(instance.as_deref()),
        wipe,
        remove_agents,
        close,
        shutdown,
        || exit_fenced(app.clone()),
    )
    .await?;
    app.request_restart();
    Ok(())
}

#[cfg(test)]
mod tests;
