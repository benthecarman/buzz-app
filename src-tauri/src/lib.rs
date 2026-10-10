mod archive;
use archive::relay_archive;
mod browser;
#[cfg(target_os = "macos")]
mod close_menu;
mod oauth_callback;
use oauth_callback::{
    oauth_callback_begin, oauth_callback_cancel, oauth_callback_wait, OAuthCallbackHost,
};
#[cfg(test)]
mod browser_permissions_tests;
mod pairing;
use browser::{
    browser_action, browser_attach, browser_detach, browser_navigate, browser_set_bounds,
    browser_status,
};
mod app_agents;
mod deep_links;
mod dock;
#[cfg(test)]
#[path = "enterprise_adapter_url.rs"]
mod enterprise_adapter_url;
#[cfg(test)]
#[path = "enterprise_auth_build.rs"]
mod enterprise_auth_build;
mod enterprise_login_gate;
mod enterprise_relay_url;
mod host_command;
mod host_process;
mod host_request;
mod identity;
mod image_clipboard;

mod notifications;
mod os_idle;
mod window_controls;
mod window_state;
use os_idle::get_os_idle_seconds;
mod relay;
mod sign_out;
use identity::{
    identity_create, identity_export, identity_import, identity_prepare_remote_agent_authorization,
    identity_restore, identity_sign_builderlab_binding, IdentityHost,
};
use relay::{
    media_copy_image, media_download, media_stream_base, relay_agent_library,
    relay_agent_log_proof, relay_agent_memories_read, relay_agent_observer, relay_agent_resolve,
    relay_channel_publish, relay_channel_sign, relay_decode_read_state, relay_decode_reminders,
    relay_decode_sidebar, relay_direct_message, relay_git_authorization, relay_http,
    relay_kit_decode, relay_kit_prepare, relay_kit_sign, relay_project_git,
    relay_project_git_cancel, relay_publish_read_state, relay_sign, relay_sign_read_state,
    relay_sign_reminder, relay_sign_sidebar, relay_upload, relay_upload_begin, relay_upload_cancel,
    relay_upload_chunk, relay_workflow_runs,
};
mod terminal;
#[cfg(test)]
use buzz_desktop::test_executable;
use buzzodz_plugins::{
    imports::{prepare_folder, prepare_git, PreparedImport, Preview},
    Catalog, InstallationResult, Manager,
};
use deep_links::{deep_link_take, deep_link_watch, DeepLinks};
use dock::{dock_permission, unread_indicator_set};
use enterprise_login_gate::enterprise_login_gate;
use host_command::plugin_host_run_command;
use host_process::{
    plugin_host_process_kill, plugin_host_process_spawn, plugin_host_process_write, HostProcesses,
};
use host_request::plugin_host_request;
use notifications::{notification_show, Notifications};
#[cfg(target_os = "macos")]
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::Manager as _;
use tauri_plugin_dialog::DialogExt;
use terminal::{
    terminal_close, terminal_close_owner, terminal_create_owner, terminal_read, terminal_resize,
    terminal_spawn, terminal_write, Terminals,
};

#[derive(Clone, Default)]
struct Imports(Arc<Mutex<Option<PreparedImport>>>);

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
enum TitleBarDoubleClickAction {
    Fill,
    Zoom,
    Minimize,
    None,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct TitleBarFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[cfg(any(target_os = "macos", test))]
impl TitleBarFrame {
    fn approximately_equals(self, other: Self) -> bool {
        const TOLERANCE: f64 = 0.5;
        (self.x - other.x).abs() <= TOLERANCE
            && (self.y - other.y).abs() <= TOLERANCE
            && (self.width - other.width).abs() <= TOLERANCE
            && (self.height - other.height).abs() <= TOLERANCE
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct TitleBarFillFrame {
    restore: TitleBarFrame,
    filled: TitleBarFrame,
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct TitleBarFillFrames(Mutex<HashMap<String, TitleBarFillFrame>>);

#[cfg(any(target_os = "macos", test))]
fn title_bar_fill_target(
    current: TitleBarFrame,
    visible: TitleBarFrame,
    saved: Option<TitleBarFillFrame>,
) -> (TitleBarFrame, Option<TitleBarFillFrame>) {
    if let Some(saved) = saved.filter(|saved| current.approximately_equals(saved.filled)) {
        (saved.restore, None)
    } else {
        (
            visible,
            Some(TitleBarFillFrame {
                restore: current,
                filled: visible,
            }),
        )
    }
}

#[cfg(any(target_os = "macos", test))]
fn title_bar_double_click_action(preference: Option<&str>) -> TitleBarDoubleClickAction {
    match preference {
        Some("Maximize" | "Fill") => TitleBarDoubleClickAction::Fill,
        Some("Zoom") => TitleBarDoubleClickAction::Zoom,
        Some("Minimize") => TitleBarDoubleClickAction::Minimize,
        _ => TitleBarDoubleClickAction::None,
    }
}

#[cfg(target_os = "macos")]
fn title_bar_frame(rect: objc2_foundation::NSRect) -> TitleBarFrame {
    TitleBarFrame {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

#[cfg(target_os = "macos")]
fn ns_rect(frame: TitleBarFrame) -> objc2_foundation::NSRect {
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    NSRect::new(
        NSPoint::new(frame.x, frame.y),
        NSSize::new(frame.width, frame.height),
    )
}

#[cfg(target_os = "macos")]
fn toggle_title_bar_fill<R: tauri::Runtime>(
    window: &tauri::Window<R>,
    frames: &TitleBarFillFrames,
) -> Result<(), String> {
    let ns_window = window.ns_window().map_err(|error| error.to_string())?;
    let ns_window: &objc2_app_kit::NSWindow = unsafe { &*ns_window.cast() };
    let current = title_bar_frame(ns_window.frame());
    let visible = title_bar_frame(
        ns_window
            .screen()
            .ok_or("Could not resolve the window's current screen")?
            .visibleFrame(),
    );
    let mut frames = frames
        .0
        .lock()
        .map_err(|_| "Title-bar Fill state is unavailable")?;
    let (target, saved) = title_bar_fill_target(current, visible, frames.remove(window.label()));
    if let Some(saved) = saved {
        frames.insert(window.label().to_owned(), saved);
    }
    drop(frames);
    ns_window.setFrame_display_animate(ns_rect(target), true, true);
    Ok(())
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn title_bar_double_click<R: tauri::Runtime>(
    window: tauri::Window<R>,
    fill_frames: tauri::State<'_, TitleBarFillFrames>,
) -> Result<(), String> {
    use objc2_foundation::{ns_string, NSUserDefaults};

    let preference = NSUserDefaults::standardUserDefaults()
        .stringForKey(ns_string!("AppleActionOnDoubleClick"))
        .map(|value| value.to_string());
    match title_bar_double_click_action(preference.as_deref()) {
        TitleBarDoubleClickAction::Fill => toggle_title_bar_fill(&window, &fill_frames)?,
        TitleBarDoubleClickAction::Zoom => {
            let ns_window = window.ns_window().map_err(|error| error.to_string())?;
            let ns_window: &objc2_app_kit::NSWindow = unsafe { &*ns_window.cast() };
            ns_window.performZoom(None);
        }
        TitleBarDoubleClickAction::Minimize => {
            window.minimize().map_err(|error| error.to_string())?;
        }
        TitleBarDoubleClickAction::None => {}
    }

    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn title_bar_double_click<R: tauri::Runtime>(_window: tauri::Window<R>) {}

async fn prepare_import(
    imports: Imports,
    operation: impl FnOnce() -> Result<Option<PreparedImport>, String> + Send + 'static,
) -> Result<Option<Preview>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut pending = imports
            .0
            .try_lock()
            .map_err(|_| "Another import is in progress")?;
        *pending = None;
        *pending = operation()?;
        Ok(pending.as_ref().map(|p| p.preview.clone()))
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn workspace_pick_folder<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title("Choose a source repository")
            .blocking_pick_folder()
            .map(|folder| {
                folder
                    .into_path()
                    .map(|path| path.to_string_lossy().into_owned())
                    .map_err(|error| error.to_string())
            })
            .transpose()
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn plugin_import_folder<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    imports: tauri::State<'_, Imports>,
) -> Result<Option<Preview>, String> {
    prepare_import(imports.inner().clone(), move || {
        app.dialog()
            .file()
            .set_title("Choose a plugin folder")
            .blocking_pick_folder()
            .map(|folder| prepare_folder(&folder.into_path().map_err(|e| e.to_string())?))
            .transpose()
    })
    .await
}
#[tauri::command]
async fn plugin_import_git(
    imports: tauri::State<'_, Imports>,
    repository: String,
    reference: String,
    authorization: Option<String>,
) -> Result<Option<Preview>, String> {
    prepare_import(imports.inner().clone(), move || {
        prepare_git(&repository, &reference, authorization.as_deref()).map(Some)
    })
    .await
}
#[tauri::command]
async fn plugin_import_discard(
    imports: tauri::State<'_, Imports>,
    token: String,
) -> Result<(), String> {
    let mut pending = imports
        .0
        .try_lock()
        .map_err(|_| "Another import is in progress")?;
    if pending.as_ref().is_some_and(|p| p.preview.token == token) {
        *pending = None;
    }
    Ok(())
}
#[tauri::command]
async fn plugin_import_install(
    manager: tauri::State<'_, PluginManager>,
    imports: tauri::State<'_, Imports>,
    token: String,
    path: String,
) -> Result<InstallationResult, String> {
    let imports = imports.inner().clone();
    with_manager(manager, move |m| {
        let pending = imports
            .0
            .try_lock()
            .map_err(|_| "Another import is in progress")?;
        pending
            .as_ref()
            .ok_or("This import preview expired. Choose the source again.")?
            .install(&m, &token, &path)
            .map(|catalog| ready(&m, catalog))
    })
    .await
}

fn ready(manager: &Manager, catalog: Catalog) -> InstallationResult {
    InstallationResult::Ready {
        catalog,
        external_plugins_paused: manager.external_plugins_paused(),
    }
}

// Invalid environment configuration must not prevent the recovery shell from opening.
struct PluginManager(Result<Manager, String>);
async fn with_manager<T: Send + 'static>(
    state: tauri::State<'_, PluginManager>,
    operation: impl FnOnce(Manager) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let manager = state.0.clone()?;
    tauri::async_runtime::spawn_blocking(move || operation(manager))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn plugin_catalog(
    manager: tauri::State<'_, PluginManager>,
) -> Result<InstallationResult, String> {
    with_manager(manager, |m| {
        Ok(match m.catalog() {
            Ok(catalog) => ready(&m, catalog),
            Err(reason) => InstallationResult::Recovery {
                reason,
                can_reset: true,
            },
        })
    })
    .await
}
#[tauri::command]
async fn plugin_change(
    manager: tauri::State<'_, PluginManager>,
    action: String,
    id: String,
) -> Result<InstallationResult, String> {
    with_manager(manager, move |m| {
        m.change(&action, &id).map(|catalog| ready(&m, catalog))
    })
    .await
}
#[tauri::command]
async fn plugin_reload(
    manager: tauri::State<'_, PluginManager>,
    id: String,
) -> Result<InstallationResult, String> {
    with_manager(manager, move |m| {
        m.reload(&id).map(|catalog| ready(&m, catalog))
    })
    .await
}
#[tauri::command]
async fn plugin_module(
    manager: tauri::State<'_, PluginManager>,
    id: String,
    revision: String,
) -> Result<String, String> {
    with_manager(manager, move |m| m.module(&id, &revision)).await
}
#[tauri::command]
async fn plugin_recover(
    manager: tauri::State<'_, PluginManager>,
) -> Result<InstallationResult, String> {
    with_manager(manager, |m| m.recover().map(|catalog| ready(&m, catalog))).await
}
#[tauri::command]
async fn update_restart<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    app.request_restart();
    Ok(())
}
fn commands<R: tauri::Runtime>() -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        pairing::pairing_account,
        pairing::pairing_start,
        pairing::pairing_status,
        pairing::pairing_confirm,
        pairing::pairing_deny,
        pairing::pairing_cancel,
        identity_restore,
        identity_import,
        identity_create,
        identity_export,
        identity_prepare_remote_agent_authorization,
        identity_sign_builderlab_binding,
        enterprise_login_gate,
        relay_sign,
        relay_decode_read_state,
        relay_sign_read_state,
        relay_publish_read_state,
        relay_http,
        relay_workflow_runs,
        relay_project_git,
        relay_project_git_cancel,
        relay_git_authorization,
        relay_channel_sign,
        relay_channel_publish,
        relay_kit_sign,
        relay_kit_prepare,
        relay_kit_decode,
        relay_direct_message,
        relay_decode_sidebar,
        relay_sign_sidebar,
        relay_decode_reminders,
        relay_sign_reminder,
        relay_agent_resolve,
        relay_agent_log_proof,
        relay_archive,
        relay_agent_observer,
        relay_agent_memories_read,
        relay_agent_library,
        relay_upload_begin,
        relay_upload_chunk,
        relay_upload,
        relay_upload_cancel,
        media_download,
        media_copy_image,
        media_stream_base,
        get_os_idle_seconds,
        workspace_pick_folder,
        plugin_import_folder,
        plugin_import_git,
        plugin_import_install,
        plugin_import_discard,
        plugin_catalog,
        plugin_change,
        plugin_reload,
        plugin_module,
        plugin_recover,
        plugin_host_run_command,
        plugin_host_request,
        plugin_host_process_spawn,
        plugin_host_process_write,
        plugin_host_process_kill,
        oauth_callback_begin,
        oauth_callback_wait,
        oauth_callback_cancel,
        app_agents::app_agent_list,
        app_agents::app_agent_create,
        app_agents::app_agent_portable,
        app_agents::app_agent_port,
        app_agents::app_agent_rename,
        app_agents::app_agent_set_picture,
        app_agents::app_agent_set_type,
        app_agents::app_agent_delete,
        app_agents::app_agent_forget,
        app_agents::app_agent_publish,
        app_agents::app_agent_publish_profile,
        app_agents::app_agent_query,
        app_agents::app_agent_upload,
        app_agents::app_agent_media,
        app_agents::app_agent_remember,
        app_agents::app_agent_claim,
        app_agents::app_agent_release,
        title_bar_double_click,
        notification_show,
        #[cfg(target_os = "macos")]
        notifications::macos::notification_permission_state,
        #[cfg(target_os = "macos")]
        notifications::macos::request_notification_access,
        deep_link_take,
        deep_link_watch,
        dock_permission,
        unread_indicator_set,
        terminal_create_owner,
        terminal_spawn,
        terminal_read,
        terminal_write,
        terminal_resize,
        terminal_close,
        terminal_close_owner,
        update_restart,
        sign_out::sign_out,
        sign_out::sign_out_wipe_refusal
    ]
}
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = app_context();
    // A pending Sign out finishes before any window, webview storage, service or
    // identity read; if it can't, Buzz explains and exits without opening.
    let instance = sign_out::Paths::resolve(&context.config().identifier).map(|paths| {
        sign_out::boot(
            &paths,
            |app_data| {
                let credentials = buzz_agent_controller::PlatformCredentials::default();
                let old = app_data.join(sign_out::OLD_AGENTS);
                if old
                    .try_exists()
                    .map_err(|error| format!("check agent registry: {error}"))?
                {
                    buzz_agent_controller::delete_local_agent_keys(old, &credentials)?;
                }
                // The pending erase runs before the temporary startup migration.
                for folder in [sign_out::AGENTS, sign_out::migrate_agents::LEGACY] {
                    buzz_agent_controller::AppAgents::open(
                        app_data.join(folder).join("identities.json"),
                    )
                    .delete_keys(&credentials)?;
                }
                Ok(())
            },
            identity::remove_saved_key,
        )
        .unwrap_or_else(|message| sign_out::exit_with(&message))
    });
    let identity = IdentityHost::default();
    let builder = tauri::Builder::default();
    let builder = if !tauri::is_dev() {
        // Single instance comes first, as its documentation requires. Its deep-link
        // feature forwards deep-link argv on Windows/Linux. macOS OS URLs reach
        // the registered bundle directly; cross-copy URL handoff is unsupported.
        // This callback only foregrounds the running window. Development launches
        // skip this so parallel worktrees can run side by side.
        builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            deep_links::focus_main(app);
        }))
    } else {
        builder
    };
    let builder = builder
        .plugin(window_controls::init())
        .plugin(window_state::builder().build())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            #[cfg(target_os = "linux")]
            {
                use gtk::prelude::GtkSettingsExt;

                // Avoid medium/full hinting distorting Linux WebKit font spacing.
                // Process-local only; preserve the user's hinting on/off preference.
                if let Some(settings) = gtk::Settings::default() {
                    settings.set_gtk_xft_hintstyle(Some("hintslight"));
                }
            }
            if let Some(window) = app.get_window("main") {
                if let Err(error) = window_state::restore(&window) {
                    eprintln!("Could not restore Buzz window: {error}");
                }
            }
            #[cfg(target_os = "macos")]
            notifications::macos::init();
            deep_links::setup(app.handle());
            app.manage(relay::Spools::new(
                app.path()
                    .app_cache_dir()
                    .map(|path| path.join("upload-spools"))
                    .map_err(|_| "Media preparation could not access temporary storage".to_owned()),
            ));
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
                loop {
                    interval.tick().await;
                    handle
                        .state::<relay::Spools>()
                        .reap(&handle.state::<relay::Uploads>());
                }
            });
            // Relay `<video>` and `<audio>` load from this listener; without it
            // they show as unavailable.
            match relay::MediaStream::start(
                app.state::<IdentityHost>().inner().clone(),
                tauri::async_runtime::handle().inner(),
            ) {
                Ok(stream) => {
                    app.manage(stream);
                }
                Err(error) => eprintln!("Could not start the media listener: {error}"),
            }
            // Only app-owned storage is created. Preview uses the OS-resolved legacy
            // parent, never a browser-supplied path or a different environment source.
            let paths = (|| {
                let root = app
                    .path()
                    .app_data_dir()
                    .map_err(|_| "Could not resolve local agent storage")?
                    .join("agent-controller");
                let legacy = app
                    .path()
                    .data_dir()
                    .map_err(|_| "Could not resolve legacy library directory")?;
                Ok((root, legacy))
            })();
            app.manage(app_agents::AppAgentHost::new(
                paths
                    .as_ref()
                    .map(|(root, _)| {
                        root.with_file_name(sign_out::AGENTS)
                            .join("identities.json")
                    })
                    .map_err(Clone::clone),
                paths
                    .as_ref()
                    .map(|(root, legacy)| buzz_agent_controller::OldStores {
                        harness: root.join("agents.json"),
                        data_dir: legacy.clone(),
                    })
                    .map_err(Clone::clone),
                // `just desktop` builds step aside for a packaged app.
                !tauri::is_dev(),
            ));
            Ok(())
        });
    #[cfg(target_os = "macos")]
    let builder = builder
        .manage(TitleBarFillFrames::default())
        .menu(close_menu::menu)
        .on_menu_event(close_menu::handle);
    // Register the updater only in configured release builds; omit it locally.
    #[cfg(buzz_updater_enabled)]
    let builder = if tauri::is_dev() {
        builder
    } else {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    };
    let builder = match instance {
        Some(instance) => builder.manage(instance),
        None => builder,
    };
    builder
        .manage(image_clipboard::ImageClipboard::default())
        .manage(identity)
        .manage(archive::ArchiveHost::default())
        .manage(pairing::Pairing::default())
        .manage(relay::Uploads::default())
        .register_asynchronous_uri_scheme_protocol("buzz-media", relay::media_protocol)
        .manage(Imports::default())
        .manage(Terminals::default())
        .manage(HostProcesses::default())
        .manage(OAuthCallbackHost::default())
        .manage(Notifications::default())
        .manage(DeepLinks::default())
        .manage(PluginManager(Manager::from_env()))
        .invoke_handler({
            let application_commands = commands::<tauri::Wry>();
            let browser_commands: fn(tauri::ipc::Invoke<tauri::Wry>) -> bool = tauri::generate_handler![
                browser_attach,
                browser_set_bounds,
                browser_detach,
                browser_navigate,
                browser_action,
                browser_status
            ];
            // Browser embeds a real native view; existing commands also support MockRuntime.
            move |request: tauri::ipc::Invoke<tauri::Wry>| {
                if request.message.command().starts_with("browser_") {
                    browser_commands(request)
                } else {
                    application_commands(request)
                }
            }
        })
        .on_page_load(|webview, payload| {
            if let Err(error) = webview
                .state::<OAuthCallbackHost>()
                .document_load(webview.label(), payload.event())
            {
                eprintln!("OAuth callback cleanup failed: {error}");
            }
            if webview.label() == "main" && matches!(payload.event(), tauri::webview::PageLoadEvent::Started) {
                webview.state::<pairing::Pairing>().cancel_all();
                webview.state::<relay::Spools>().cancel_all(&webview.state::<relay::Uploads>());
                // Processes belong to the page that started them.
                webview.state::<HostProcesses>().stop_all();
            }
            browser::page_load(webview, payload);
        })
        .on_window_event(|window, event| {
            if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed | tauri::WindowEvent::CloseRequested { .. }) { window.state::<pairing::Pairing>().cancel_all(); }
            if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed) { window.state::<relay::Spools>().cancel_all(&window.state::<relay::Uploads>()); }
            #[cfg(target_os = "macos")]
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    // Keep the webview and running agents alive until explicit Quit.
                    api.prevent_close();
                    if let Err(error) = window.hide() {
                        eprintln!("Could not close Buzz window: {error}");
                    }
                    return;
                }
            }
            browser::window_event(window, event);
        })
        .build(context)
        .expect("failed to build Buzz Foundation")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if matches!(event, tauri::RunEvent::Reopen { .. }) {
                deep_links::focus_main(app);
            }
            if matches!(event, tauri::RunEvent::Exit) {
                shut_down(app);
            }
        });
}

/// Best-effort native teardown when Buzz exits, from Quit or a fenced sign-out.
pub(crate) fn shut_down<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    app.state::<relay::Spools>()
        .cancel_all(&app.state::<relay::Uploads>());
    app.state::<image_clipboard::ImageClipboard>().release();
    app.state::<HostProcesses>().shutdown();
    browser::shutdown();
    if let Err(error) = app.state::<Terminals>().shutdown() {
        eprintln!("Terminal shutdown failed: {error}");
    }
}

fn app_context<R: tauri::Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

#[cfg(test)]
mod tests {
    use super::{
        title_bar_double_click_action, title_bar_fill_target, TitleBarDoubleClickAction,
        TitleBarFillFrame, TitleBarFrame,
    };

    fn frame(x: f64, y: f64, width: f64, height: f64) -> TitleBarFrame {
        TitleBarFrame {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn title_bar_double_click_preferences_map_to_native_actions() {
        assert_eq!(
            title_bar_double_click_action(Some("Maximize")),
            TitleBarDoubleClickAction::Fill
        );
        assert_eq!(
            title_bar_double_click_action(Some("Fill")),
            TitleBarDoubleClickAction::Fill
        );
        assert_eq!(
            title_bar_double_click_action(Some("Zoom")),
            TitleBarDoubleClickAction::Zoom
        );
        assert_eq!(
            title_bar_double_click_action(Some("Minimize")),
            TitleBarDoubleClickAction::Minimize
        );
        assert_eq!(
            title_bar_double_click_action(Some("None")),
            TitleBarDoubleClickAction::None
        );
        assert_eq!(
            title_bar_double_click_action(None),
            TitleBarDoubleClickAction::None
        );
    }

    #[test]
    fn title_bar_fill_restores_only_an_unchanged_filled_window() {
        let original = frame(100.0, 100.0, 900.0, 700.0);
        let visible = frame(0.0, 25.0, 1512.0, 920.0);
        let saved = TitleBarFillFrame {
            restore: original,
            filled: visible,
        };

        assert_eq!(
            title_bar_fill_target(visible, visible, Some(saved)),
            (original, None)
        );

        let moved = frame(20.0, 25.0, 1492.0, 920.0);
        assert_eq!(
            title_bar_fill_target(moved, visible, Some(saved)),
            (
                visible,
                Some(TitleBarFillFrame {
                    restore: moved,
                    filled: visible,
                })
            )
        );
    }
}
