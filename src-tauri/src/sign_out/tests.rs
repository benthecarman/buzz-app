/// The real command handler and generated ACL behind a main webview.
fn production_app() -> (
    tauri::App<tauri::test::MockRuntime>,
    tauri::WebviewWindow<tauri::test::MockRuntime>,
) {
    let app = tauri::test::mock_builder()
        .manage(crate::identity::IdentityHost::fixture())
        .invoke_handler(crate::commands())
        .build(crate::app_context())
        .unwrap();
    let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    (app, view)
}
fn invoke(
    view: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    cmd: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    tauri::test::get_ipc_response(
        view,
        tauri::webview::InvokeRequest {
            cmd: cmd.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: view.url().unwrap(),
            body: tauri::ipc::InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: tauri::test::INVOKE_KEY.into(),
        },
    )
    .map(|body| body.deserialize().unwrap())
}

#[test]
fn production_acl_lets_the_dialog_ask_why_wipe_is_unavailable() {
    let (_app, view) = production_app();
    // Tests are debug builds, where wipe is refused.
    assert_eq!(
        invoke(&view, "sign_out_wipe_refusal", serde_json::json!({})).unwrap(),
        serde_json::json!(
            "Erasing is unavailable in development builds because they share agent keys and plugin storage with the installed Buzz"
        )
    );
}

#[test]
fn production_acl_lets_sign_out_reach_native_validation() {
    let (_app, view) = production_app();
    let error = invoke(
        &view,
        "sign_out",
        serde_json::json!({"wipe": false, "removeAgents": true}),
    )
    .unwrap_err();
    assert_eq!(
        error,
        serde_json::json!({
            "message": "Removing agents is part of erasing this device"
        })
    );
}
