#[path = "src/enterprise_adapter_url.rs"]
mod enterprise_adapter_url;
#[path = "src/enterprise_auth_build.rs"]
mod enterprise_auth_build;
#[path = "src/enterprise_relay_url.rs"]
mod enterprise_relay_url;

fn main() {
    configure_enterprise_auth();
    // Release builds enable the updater only when both values are supplied; its
    // `plugins.updater` config comes from the same values via `tauri build --config`.
    println!("cargo:rerun-if-env-changed=BUZZ_UPDATER_PUBLIC_KEY");
    println!("cargo:rerun-if-env-changed=BUZZ_UPDATER_ENDPOINT");
    println!("cargo:rustc-check-cfg=cfg(buzz_updater_enabled)");
    let configured = |name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty());
    if configured("BUZZ_UPDATER_PUBLIC_KEY") && configured("BUZZ_UPDATER_ENDPOINT") {
        println!("cargo:rustc-cfg=buzz_updater_enabled");
    }
    let mut attributes = tauri_build::Attributes::new();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        // Tauri's resource linker covers bin targets, not the lib unit-test EXE.
        // Link the same manifest into both without changing icons/version resources.
        // https://github.com/tauri-apps/tauri/issues/13419
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
        let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
    tauri_build::try_build(
        attributes.app_manifest(tauri_build::AppManifest::new().commands(&[
            "pairing_account",
            "pairing_start",
            "pairing_status",
            "pairing_confirm",
            "pairing_deny",
            "pairing_cancel",
            "identity_restore",
            "identity_import",
            "identity_create",
            "identity_export",
            "sign_out",
            "sign_out_wipe_refusal",
            "identity_prepare_remote_agent_authorization",
            "identity_sign_builderlab_binding",
            "enterprise_login_gate",
            "relay_sign",
            "relay_decode_read_state",
            "relay_sign_read_state",
            "relay_publish_read_state",
            "relay_http",
            "relay_workflow_runs",
            "relay_project_git",
            "relay_project_git_cancel",
            "relay_channel_sign",
            "relay_channel_publish",
            "relay_kit_sign",
            "relay_kit_prepare",
            "relay_kit_decode",
            "relay_direct_message",
            "relay_decode_sidebar",
            "relay_sign_sidebar",
            "relay_decode_reminders",
            "relay_sign_reminder",
            "relay_agent_resolve",
            "relay_git_authorization",
            "relay_agent_log_proof",
            "relay_agent_observer",
            "relay_archive",
            "relay_agent_memories_read",
            "relay_agent_library",
            "relay_upload_begin",
            "relay_upload_chunk",
            "relay_upload",
            "relay_upload_cancel",
            "media_download",
            "media_copy_image",
            "media_stream_base",
            "media_snapshot_read",
            "get_os_idle_seconds",
            "workspace_pick_folder",
            "plugin_import_folder",
            "plugin_import_git",
            "plugin_import_install",
            "plugin_import_discard",
            "plugin_catalog",
            "plugin_change",
            "plugin_reload",
            "plugin_module",
            "plugin_recover",
            "plugin_host_run_command",
            "plugin_host_request",
            "plugin_host_process_spawn",
            "plugin_host_process_write",
            "plugin_host_process_kill",
            "oauth_callback_begin",
            "oauth_callback_wait",
            "oauth_callback_cancel",
            "agent_control_create_prepare",
            "app_agent_list",
            "app_agent_create",
            "app_agent_rename",
            "app_agent_delete",
            "app_agent_forget",
            "app_agent_publish",
            "app_agent_publish_profile",
            "app_agent_query",
            "app_agent_upload",
            "app_agent_remember",
            "app_agent_claim",
            "app_agent_release",
            "agent_control_create_authorize",
            "agent_control_create_commit",
            "agent_control_creation_profile",
            "agent_control_snapshot_memory_write",
            "agent_control_team_preview",
            "agent_control_team_capture",
            "agent_control_team_export",
            "agent_control_team_instructions",
            "agent_control_snapshot",
            "agent_control_log_challenge",
            "agent_control_read_log",
            "pi_install",
            "claude_install",
            "claude_auth_status",
            "agent_security",
            "agent_control_save",
            "agent_control_save_defaults",
            "agent_control_start_on_app_launch",
            "agent_control_delete",
            "agent_control_action",
            "agent_control_attach_mention",
            "agent_control_import_preview",
            "agent_control_import_commit",
            "agent_control_local_clone_settings",
            "agent_control_clone_settings",
            "agent_control_use_here",
            "agent_models_begin",
            "agent_models_cancel",
            "agent_models_run",
            "title_bar_double_click",
            "notification_show",
            "notification_permission_state",
            "request_notification_access",
            "dock_permission",
            "unread_indicator_set",
            "deep_link_take",
            "deep_link_watch",
            "terminal_create_owner",
            "terminal_spawn",
            "terminal_read",
            "terminal_write",
            "terminal_resize",
            "terminal_close",
            "terminal_close_owner",
            "update_restart",
            "browser_attach",
            "browser_set_bounds",
            "browser_detach",
            "browser_navigate",
            "browser_action",
            "browser_status",
        ])),
    )
    .expect("Could not build Tauri resources")
}

fn configure_enterprise_auth() {
    const RELAYS: &str = "BUZZ_BUILD_ENTERPRISE_AUTH_RELAYS";
    const ADAPTER: &str = "BUZZ_BUILD_ENTERPRISE_AUTH_ADAPTER_BASE_URL";
    println!("cargo:rerun-if-env-changed={RELAYS}");
    println!("cargo:rerun-if-env-changed={ADAPTER}");

    let relays = std::env::var(RELAYS).ok();
    let adapter = std::env::var(ADAPTER).ok();
    let configured = enterprise_auth_build::validate_enterprise_auth_build_config(
        relays.as_deref(),
        adapter.as_deref(),
    )
    .unwrap_or_else(|error| panic!("{error}"));

    if let Some(relays) = configured.relays.as_deref() {
        println!("cargo:rustc-env={RELAYS}={relays}");
    }
    if let Some(adapter) = configured.adapter.as_deref() {
        println!("cargo:rustc-env={ADAPTER}={adapter}");
    }
}
