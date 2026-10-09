use tauri::{
    ipc::{CallbackFn, InvokeBody},
    test::{get_ipc_response, mock_builder, MockRuntime, INVOKE_KEY},
    webview::InvokeRequest,
    WebviewBuilder, WebviewUrl, WindowBuilder,
};

fn invoke(
    webview: &tauri::Webview<MockRuntime>,
    command: &str,
    origin: &str,
) -> Result<tauri::ipc::InvokeResponseBody, serde_json::Value> {
    struct BorrowedWebview<'a>(&'a tauri::Webview<MockRuntime>);
    impl AsRef<tauri::Webview<MockRuntime>> for BorrowedWebview<'_> {
        fn as_ref(&self) -> &tauri::Webview<MockRuntime> {
            self.0
        }
    }
    get_ipc_response(
        &BorrowedWebview(webview),
        InvokeRequest {
            cmd: command.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: origin.parse().unwrap(),
            body: InvokeBody::default(),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.into(),
        },
    )
}

#[test]
fn native_command_permissions_allow_only_main_webview() {
    let app = mock_builder()
        // A marker handler proves which requests pass the real generated ACL,
        // without starting terminals, importing plugins or showing notifications.
        .invoke_handler(|request| {
            request.resolver.resolve("authorized");
            true
        })
        .build(super::app_context())
        .unwrap();
    let main_window = WindowBuilder::new(&app, "main").build().unwrap();
    let main = main_window
        .add_child(
            WebviewBuilder::new("main", WebviewUrl::default()),
            tauri::LogicalPosition::new(0, 0),
            tauri::LogicalSize::new(800, 600),
        )
        .unwrap();
    // Production uses a raw Wry guest with no IPC. A simulated Tauri sibling
    // catches accidental window-wide grants in the main window's capability.
    let guest = main_window
        .add_child(
            WebviewBuilder::new("browser-content", WebviewUrl::default()),
            tauri::LogicalPosition::new(500, 80),
            tauri::LogicalSize::new(300, 500),
        )
        .unwrap();

    let application_commands = [
        "identity_restore",
        "identity_import",
        "identity_create",
        "identity_export",
        "relay_sign",
        "identity_prepare_remote_agent_authorization",
        "identity_sign_builderlab_binding",
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
        "agent_control_save",
        "agent_control_save_defaults",
        "agent_control_start_on_app_launch",
        "agent_control_delete",
        "agent_control_action",
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
        #[cfg(target_os = "macos")]
        "notification_permission_state",
        #[cfg(target_os = "macos")]
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
    ];
    let local_origin = if cfg!(windows) {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    };
    // Window fallback belongs to trusted app UI, never a sibling or remote page.
    for origin in [local_origin, "https://example.org", "http://localhost:1430"] {
        assert!(invoke(&guest, "plugin:window|close", origin).is_err());
    }
    assert!(invoke(&main, "plugin:window|close", "https://example.org").is_err());
    // The removed owner attestation cannot acquire a main-webview grant.
    assert!(invoke(&main, "relay_agent_authorize", local_origin).is_err());
    assert!(invoke(&main, "relay_agent_history_decode", local_origin).is_err());
    for command in application_commands {
        assert!(invoke(&main, command, local_origin).is_ok(), "{command}");
        for origin in [local_origin, "https://example.org", "http://localhost:1430"] {
            assert!(
                invoke(&guest, command, origin).is_err(),
                "guest must reject {command} from {origin}"
            );
        }
        assert!(
            invoke(&main, command, "https://example.org").is_err(),
            "remote content must not use main grants: {command}"
        );
    }
    assert!(invoke(&main, "plugin:window|close", local_origin).is_ok());
}
