use super::*;

#[test]
fn websocket_auth_is_bound_to_the_captured_community() {
    let mut event = EventTemplate {
        kind: 22242,
        created_at: 1,
        content: "".into(),
        tags: vec![
            vec!["relay".into(), "wss://relay.test".into()],
            vec!["challenge".into(), "nonce".into()],
        ],
    };
    assert!(validate_event("https://relay.test", &event).is_ok());
    assert!(validate_event("https://other.test", &event).is_err());
    event
        .tags
        .push(vec!["relay".into(), "wss://other.test".into()]);
    assert!(validate_event("https://relay.test", &event).is_err());
    event.kind = 27235;
    assert!(validate_event("https://relay.test", &event).is_err());
}

#[test]
fn leave_requests_sign_only_the_protected_empty_shape() {
    let leave = |content: &str, tags: Vec<Vec<String>>| EventTemplate {
        kind: 28936,
        created_at: 1,
        content: content.into(),
        tags,
    };
    let protected = || vec![vec!["-".to_string()]];
    assert!(validate_event("https://relay.test", &leave("", protected())).is_ok());
    for rejected in [
        leave("bye", protected()),
        leave("", vec![]),
        leave("", vec![vec!["-".into(), "x".into()]]),
        leave(
            "",
            vec![vec!["-".into()], vec!["h".into(), "channel".into()]],
        ),
        leave("", vec![vec!["p".into(), "a".repeat(64)]]),
    ] {
        assert!(validate_event("https://relay.test", &rejected).is_err());
    }
}

#[test]
fn member_commands_sign_only_the_broker_shape() {
    let command = |kind: u16, content: &str, tags: &[&[&str]]| EventTemplate {
        kind,
        created_at: 1,
        content: content.into(),
        tags: tags
            .iter()
            .map(|tag| tag.iter().map(|value| value.to_string()).collect())
            .collect(),
    };
    let key = "a".repeat(64);
    let p: &[&str] = &["p", &key];
    for accepted in [
        command(9030, "", &[p, &["role", "member"]]),
        command(9030, "", &[p, &["role", "admin"]]),
        command(9031, "", &[p]),
        command(9032, "", &[p, &["role", "admin"]]),
        command(9032, "", &[p, &["role", "member"]]),
    ] {
        assert!(validate_event("https://relay.test", &accepted).is_ok());
    }
    let upper = "A".repeat(64);
    for rejected in [
        // Owner is never granted, and add/role must name a role.
        command(9030, "", &[p, &["role", "owner"]]),
        command(9032, "", &[p, &["role", "owner"]]),
        command(9030, "", &[p]),
        command(9032, "", &[p]),
        // Remove carries the target only.
        command(9031, "", &[p, &["role", "member"]]),
        command(9030, "note", &[p, &["role", "member"]]),
        command(9031, "", &[&["p", &upper]]),
        command(9031, "", &[&["p", &key[1..]]]),
        command(9031, "", &[&["p", &key, "wss://relay.test"]]),
        command(9031, "", &[p, p]),
        command(9031, "", &[&["role", "member"], p]),
        command(9030, "", &[p, &["role", "member"], &["h", "channel"]]),
        command(9031, "", &[]),
        // Workspace profile edits stay outside this surface.
        command(9033, "", &[]),
    ] {
        assert!(validate_event("https://relay.test", &rejected).is_err());
    }
}

#[test]
fn workflow_signer_rejects_nonworkflow_deletes_and_invalid_commands() {
    let id = "11111111-1111-4111-8111-111111111111";
    let mut event = EventTemplate {
        kind: 5,
        created_at: 1,
        content: "".into(),
        tags: vec![
            vec!["h".into(), id.into()],
            vec!["a".into(), format!("30620:{}:{id}", "a".repeat(64))],
        ],
    };
    assert!(validate_event("https://relay.test", &event).is_ok());
    event.tags[1][1] = format!("30030:{}:{id}", "a".repeat(64));
    assert!(validate_event("https://relay.test", &event).is_err());
    event.tags[1][1] = format!("30620:{}:{id}", "a".repeat(64));
    event.tags.push(vec!["e".into(), "b".repeat(64)]);
    assert!(validate_event("https://relay.test", &event).is_err());
    event.tags.pop();
    event.kind = 46020;
    assert!(validate_event("https://relay.test", &event).is_err());
    event.tags[1] = vec!["d".into(), id.into()];
    assert!(validate_event("https://relay.test", &event).is_ok());
    event.content = "not empty".into();
    assert!(validate_event("https://relay.test", &event).is_err());
}

#[test]
fn shared_kind_five_signer_accepts_message_and_reaction_deletion_only_in_broker_shape() {
    let id = "a".repeat(64);
    let mut event = EventTemplate {
        kind: 5,
        created_at: 123,
        content: String::new(),
        tags: vec![
            vec!["h".into(), "room".into()],
            vec!["e".into(), id.clone()],
            vec!["k".into(), "9".into()],
            vec!["client-id".into(), "intent".into()],
        ],
    };
    assert!(validate_event("https://relay.test", &event).is_ok());
    event.created_at = 9_007_199_254_740_992;
    assert!(validate_event("https://relay.test", &event).is_err());
    event.created_at = 123;
    event.content = "not empty".into();
    assert!(validate_event("https://relay.test", &event).is_err());
    event.content.clear();
    event.tags[0][1].clear();
    assert!(validate_event("https://relay.test", &event).is_err());
    event.tags[0][1] = "😀".repeat(128);
    assert!(validate_event("https://relay.test", &event).is_ok());
    event.tags[0][1].push('😀');
    assert!(validate_event("https://relay.test", &event).is_err());
    event.tags[0][1] = "room".into();
    event.tags[2][1] = "7".into();
    assert!(validate_event("https://relay.test", &event).is_ok());
    event.tags[2][1] = "40002".into();
    assert!(validate_event("https://relay.test", &event).is_ok());
    event.tags[2][1] = "30620".into();
    assert!(validate_event("https://relay.test", &event).is_err());
    event.tags[2][1] = "9".into();
    event.tags.push(vec!["e".into(), id.clone()]);
    assert!(validate_event("https://relay.test", &event).is_err());
    event.tags.pop();
    event.tags[1][1] = "A".repeat(64);
    assert!(validate_event("https://relay.test", &event).is_err());
    event.tags[1][1] = id;
    event.tags.push(vec!["a".into(), "30620:other:id".into()]);
    assert!(validate_event("https://relay.test", &event).is_err());
}

#[test]
fn shared_kind_five_signer_accepts_only_one_agent_record_deletion() {
    let owner = "a".repeat(64);
    let agent = "b".repeat(64);
    let mut event = EventTemplate {
        kind: 5,
        created_at: 123,
        content: String::new(),
        tags: vec![
            vec!["a".into(), format!("30177:{owner}:{agent}")],
            vec!["client-id".into(), "intent".into()],
        ],
    };
    assert!(validate_event("https://relay.test", &event).is_ok());
    event.tags.pop();
    assert!(validate_event("https://relay.test", &event).is_ok());
    for coordinate in [
        format!("30175:{owner}:{agent}"),
        format!("30177:{owner}:{}", "B".repeat(64)),
        format!("30177:{owner}:{agent}:extra"),
        format!("30177:{owner}"),
    ] {
        event.tags[0][1] = coordinate;
        assert!(validate_event("https://relay.test", &event).is_err());
    }
    event.tags[0][1] = format!("30177:{owner}:{agent}");
    event.content = "reason".into();
    assert!(validate_event("https://relay.test", &event).is_err());
    event.content.clear();
    event
        .tags
        .push(vec!["a".into(), format!("30177:{owner}:{owner}")]);
    assert!(validate_event("https://relay.test", &event).is_err());
}

#[test]
fn js_signs_emoji_sets_but_never_blossom_tokens() {
    let template = |kind| EventTemplate {
        kind,
        created_at: 1,
        content: "Upload attachment".into(),
        tags: vec![vec!["t".into(), "upload".into()]],
    };
    assert!(validate_event("https://relay.test", &template(30030)).is_ok());
    assert!(validate_event("https://relay.test", &template(24242)).is_err());
}

#[test]
fn canvas_signing_shape_matches_shared_contract() {
    let cases: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../src/features/channel-templates/canvas-signing-contract.json"
    ))
    .unwrap();
    let cases = cases.as_array().unwrap();
    assert!(!cases.is_empty(), "shared Canvas signing corpus is empty");
    for case in cases {
        // Tag shape plus EventTemplate deserialization, not IPC wiring, broker
        // freshness, the native signing budget or publication.
        let event = serde_json::from_value::<EventTemplate>(serde_json::json!({
            "kind": 40100, "created_at": 100, "content": "# Plan", "tags": case["tags"]
        }));
        let accepted = if case["deserializes"] == false {
            assert!(event.is_err(), "{}", case["name"]);
            false
        } else {
            let event = event.unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
            validate_event("https://relay.test", &event).is_ok()
        };
        assert_eq!(
            accepted,
            case["accepted"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn canvas_content_is_bounded_in_utf8_bytes() {
    let mut event = EventTemplate {
        kind: 40100,
        created_at: 100,
        content: "é".repeat(12 * 1024),
        tags: vec![vec![
            "h".into(),
            "11111111-1111-4111-8111-111111111111".into(),
        ]],
    };
    assert!(validate_event("https://relay.test", &event).is_ok());
    event.content.push('x');
    assert!(validate_event("https://relay.test", &event).is_err());
}
