use crate::event::EventTemplate;
use crate::relay::events::validate_event;

#[test]
fn catalog_publications_admit_only_the_nip_ap_envelope() {
    let agent = |tags: serde_json::Value, content: &str| EventTemplate {
        kind: 30175,
        created_at: 123,
        content: content.into(),
        tags: serde_json::from_value(tags).unwrap(),
    };
    let body = r#"{"display_name":"Scout","system_prompt":"Help."}"#;
    let slug = "02".repeat(32);
    for tags in [
        serde_json::json!([["d", slug]]),
        serde_json::json!([["d", slug], ["shared", "true"]]),
        serde_json::json!([["d", "scout_1-a"], ["shared", "true"], ["client-id", "x"]]),
    ] {
        assert!(validate_event("https://relay.test", &agent(tags, body)).is_ok());
    }
    for tags in [
        serde_json::json!([]),
        serde_json::json!([["d"]]),
        serde_json::json!([["d", ""]]),
        serde_json::json!([["d", "Scout"]]),
        serde_json::json!([["d", "-scout"]]),
        serde_json::json!([["d", "a".repeat(65)]]),
        serde_json::json!([["d", "builtin-team:welcome"]]),
        serde_json::json!([["d", slug], ["d", "other"]]),
        serde_json::json!([["d"], ["d", slug]]),
        serde_json::json!([["d", slug], ["shared", "false"]]),
        serde_json::json!([["d", slug], ["shared"]]),
        serde_json::json!([["d", slug], ["shared", "true", "extra"]]),
        serde_json::json!([["d", slug], ["shared", "true"], ["shared", "true"]]),
        serde_json::json!([["d", slug], ["h", "channel"]]),
        serde_json::json!([["d", slug], ["p", "a".repeat(64)]]),
        serde_json::json!([["d", slug], ["client-id", "a"], ["client-id", "b"]]),
    ] {
        let event = agent(tags, body);
        assert!(
            validate_event("https://relay.test", &event).is_err(),
            "{:?}",
            event.tags
        );
    }
    for content in [
        "",
        "[]",
        "not json",
        r#"{"system_prompt":"no name"}"#,
        r#"{"display_name":"  "}"#,
        r#"{"display_name":"Scout","env_vars":{"KEY":"secret"}}"#,
    ] {
        let event = agent(serde_json::json!([["d", slug]]), content);
        assert!(
            validate_event("https://relay.test", &event).is_err(),
            "{content}"
        );
    }
    let oversized = format!(
        r#"{{"display_name":"Scout","system_prompt":"{}"}}"#,
        "a".repeat(65_536)
    );
    assert!(validate_event(
        "https://relay.test",
        &agent(serde_json::json!([["d", slug]]), &oversized)
    )
    .is_err());

    let team = |tags: serde_json::Value, content: &str| EventTemplate {
        kind: 30178,
        created_at: 123,
        content: content.into(),
        tags: serde_json::from_value(tags).unwrap(),
    };
    let body = r#"{"v":1,"name":"Crew","members":[]}"#;
    for d in [
        "builtin-team:welcome",
        "6f1c2c0e-0d3c-4b8e-9a51-3f0d2a1e4b7c",
        "kit_team-1",
    ] {
        assert!(validate_event(
            "https://relay.test",
            &team(serde_json::json!([["d", d], ["shared", "true"]]), body)
        )
        .is_ok());
    }
    for d in ["", "has space", "tab\tid", &"a".repeat(65)] {
        assert!(
            validate_event(
                "https://relay.test",
                &team(serde_json::json!([["d", d]]), body)
            )
            .is_err(),
            "{d:?}"
        );
    }
    for content in [
        r#"{"v":1,"members":[]}"#,
        r#"{"v":1,"name":"Crew"}"#,
        r#"{"v":1,"name":"Crew","members":[],"env_vars":{}}"#,
    ] {
        assert!(
            validate_event(
                "https://relay.test",
                &team(serde_json::json!([["d", "t"]]), content)
            )
            .is_err(),
            "{content}"
        );
    }
    let mut late = team(serde_json::json!([["d", "t"]]), body);
    late.created_at = 9_007_199_254_740_992;
    assert!(validate_event("https://relay.test", &late).is_err());
}
