#[test]
fn production_acl_lets_the_dialog_ask_why_wipe_is_unavailable() {
    use crate::agents::tests::{fixture, invoke};
    let (_dir, _host, _app, view) = fixture();
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
    use crate::agents::tests::{fixture, invoke};
    let (_dir, _host, _app, view) = fixture();
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
    // Refused before any agent was stopped.
    assert!(invoke(&view, "agent_control_snapshot", serde_json::json!({})).is_ok());
}
