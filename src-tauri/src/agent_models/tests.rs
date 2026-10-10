use super::*;
use crate::agents::tests::{fixture, invoke, seed};
use buzz_agent::{auth::BrowserOpener, config::DatabricksModelFilter, AgentError};
use buzz_desktop::agent_models::{Connection, Factory};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Replaces Databricks behind the real commands; clones share their counts.
#[derive(Clone, Default)]
struct Fake(Arc<Counts>);
#[derive(Default)]
struct Counts {
    opened: Mutex<Vec<(String, PathBuf)>>,
    connects: AtomicUsize,
    catalogs: AtomicUsize,
    failure: AtomicUsize,
}
impl std::ops::Deref for Fake {
    type Target = Counts;
    fn deref(&self) -> &Counts {
        &self.0
    }
}
impl Factory for Fake {
    fn open(
        &self,
        workspace: &str,
        cache: &std::path::Path,
        _: Arc<dyn BrowserOpener>,
    ) -> Result<Box<dyn Connection>, String> {
        self.opened
            .lock()
            .unwrap()
            .push((workspace.into(), cache.into()));
        Ok(Box::new(self.clone()))
    }
}
impl Connection for Fake {
    fn connect(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async {
            self.connects.fetch_add(1, Ordering::SeqCst);
            if self.failure.load(Ordering::SeqCst) == 1 {
                Err("Synthetic auth rejected".into())
            } else {
                Ok(())
            }
        })
    }
    fn models(
        &self,
        filter: Option<DatabricksModelFilter>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<buzz_agent::catalog::ModelEntry>, AgentError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.catalogs.fetch_add(1, Ordering::SeqCst);
            match self.failure.load(Ordering::SeqCst) {
                1 => return Err(AgentError::LlmAuth("Synthetic auth rejected".into())),
                2 => return Err(AgentError::Llm("Synthetic catalog rejected".into())),
                6 if self.connects.load(Ordering::SeqCst) == 0 => {
                    return Err(AgentError::LlmAuth("No cached token".into()))
                }
                3 => return Ok(vec![]),
                4 => {
                    return Ok(vec![buzz_agent::catalog::ModelEntry {
                        id: "not-discovered".into(),
                        name: "Known (default catalog)".into(),
                    }])
                }
                5 => {
                    return Ok(vec![buzz_agent::catalog::ModelEntry {
                        id: "a".repeat(513),
                        name: "Oversized".into(),
                    }])
                }
                _ => {}
            }
            Ok([
                ("catalog.schema.model-service", "Model Service"),
                ("endpoint-two", "Endpoint Two"),
            ]
            .into_iter()
            .filter(|(id, _)| filter.as_ref().is_none_or(|f| f.matches(id)))
            .map(|(id, name)| buzz_agent::catalog::ModelEntry {
                id: id.into(),
                name: name.into(),
            })
            .collect())
        })
    }
}
fn request(dir: &std::path::Path, id: &str, action: &str) -> Value {
    json!({"id":id,"expectedRevision":1,"host":"https://workspace.example.com","filter":"", "action":action,
    "edit":{"name":"Sample","systemPrompt":"Original","workspace":dir.to_str().unwrap(),"harness":{"command":"buzz-agent","args":[],"model":"custom-unchanged","provider":"databricks_v2","databricks":{"host":"https://workspace.example.com","filter":""}},"environment":{}}})
}

#[test]
#[cfg(unix)]
fn goose_databricks_models_load_through_native_ipc_for_an_unsaved_agent() {
    let (dir, _, _app, view) = fixture();
    let goose = dir.path().join("goose");
    let invoked = dir.path().join("invoked");
    crate::test_executable::write_executable(
        &goose,
        format!(
            "#!/bin/sh\n: > '{}'\nread request\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"providerId\":\"databricks_v2\",\"models\":[\"catalog.schema.goose-glm-5-3\"]}}}}'\n",
            invoked.display()
        ),
    );
    let edit = json!({"name":"Goose","systemPrompt":"","workspace":dir.path(),
        "harness":{"command":goose,"args":["acp"],"provider":"databricks_v2","model":""},
        "environment":{}});
    let refresh_ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
    assert!(invoke(
        &view,
        "agent_models_run",
        json!({"ticket":refresh_ticket,"request":{
            "host":"", "filter":"", "action":"refresh", "edit":edit
        }})
    )
    .unwrap_err()
    .to_string()
    .contains("explicit Browse or Retry"));
    assert!(!invoked.exists());
    let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
    let result = invoke(
        &view,
        "agent_models_run",
        json!({"ticket":ticket,"request":{
            "host":"", "filter":"", "action":"connect",
            "edit":edit
        }}),
    )
    .unwrap();
    assert!(invoked.exists());
    assert_eq!(result["models"][0]["id"], "catalog.schema.goose-glm-5-3");
    assert_eq!(result["host"], "");
}

#[test]
#[cfg(unix)]
fn goose_connection_test_uses_the_draft_model_and_environment() {
    let (dir, _, _app, view) = fixture();
    let goose = dir.path().join("goose");
    let script = r#"#!/bin/sh
if [ "$1" = acp ]; then
  read request
  case "$request" in *providers/list*) ;; *) exit 1;; esac
  case "$TEST_DEFAULT" in
    missing) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"entries":[]}}';;
    mismatch) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"entries":[{"providerId":"anthropic","defaultModel":"other-model"}]}}';;
    invalid) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"entries":[{"providerId":"openai","defaultModel":"bad\nmodel"}]}}';;
    *) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"entries":[{"providerId":"openai","defaultModel":"provider-default"}]}}';;
  esac
  exit
fi
[ "$1 $2 $3" = 'run --text Reply OK.' ] || exit 1
[ "$4 $5 $6 $7 $8 $9" = '--no-session --no-profile --max-turns 1 --quiet --output-format' ] || exit 1
[ "${10}" = 'json' ] || exit 1
[ "$GOOSE_PROVIDER" = 'openai' ] || exit 1
case "$GOOSE_MODEL" in effective-model|provider-default) ;; *) exit 1;; esac
[ "$(pwd)" = '__WORKSPACE__' ] || exit 1
printf '%s\n' 'prompt' >> prompts
if [ "$OPENAI_API_KEY" = 'draft-key' ]; then
  printf '%s\n' '{"metadata":{"status":"completed","total_tokens":4},"messages":[{"role":"assistant","content":[{"type":"text","text":"OK"}]}]}'
elif [ "$BAD_STYLE" = text ]; then
  printf '%s\n' '{"metadata":{"status":"completed","total_tokens":0},"messages":[{"role":"assistant","content":[{"type":"text","text":"Ran into this error: API key not valid."}]}]}'
else
  printf '%s\n' '{"metadata":{"status":"completed","total_tokens":4},"messages":[{"role":"assistant","content":[{"type":"error","error":"authentication failed"}]}]}'
fi
"#
    .replace(
        "__WORKSPACE__",
        &dir.path().canonicalize().unwrap().display().to_string(),
    );
    crate::test_executable::write_executable(&goose, script);
    let edit = json!({"name":"Goose","systemPrompt":"","workspace":dir.path(),
        "harness":{"command":goose,"args":["acp"],"provider":"openai","model":"visible-model"},
        "environment":{"GOOSE_MODEL":"effective-model","OPENAI_API_KEY":"draft-key"}});
    let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
    let result = invoke(
        &view,
        "agent_models_run",
        json!({"ticket":ticket,"request":{
            "host":"","filter":"","action":"test","edit":edit
        }}),
    );
    let result = result.unwrap();
    assert_eq!(result["models"], json!([]));
    assert!(result.get("testedModel").is_none());
    assert!(!result.to_string().contains("effective-model"));
    let mut automatic = edit.clone();
    automatic["harness"]["model"] = json!("");
    automatic["environment"]
        .as_object_mut()
        .unwrap()
        .remove("GOOSE_MODEL");
    let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
    let result = invoke(
        &view,
        "agent_models_run",
        json!({"ticket":ticket,"request":{
            "host":"","filter":"","action":"test","edit":automatic
        }}),
    )
    .unwrap();
    assert_eq!(result["testedModel"], "openai/provider-default");
    let prompts = std::fs::read_to_string(dir.path().join("prompts")).unwrap();
    for case in ["missing", "mismatch", "invalid"] {
        let mut unavailable = automatic.clone();
        unavailable["environment"]["TEST_DEFAULT"] = json!(case);
        let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
        assert!(
            invoke(
                &view,
                "agent_models_run",
                json!({"ticket":ticket,"request":{
                    "host":"","filter":"","action":"test","edit":unavailable
                }})
            )
            .is_err(),
            "{case}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("prompts")).unwrap(),
            prompts
        );
    }
    let mut bad = automatic;
    bad["environment"]["OPENAI_API_KEY"] = json!("bad-key");
    for style in ["error-content", "text"] {
        bad["environment"]["BAD_STYLE"] = json!(style);
        let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
        let error = invoke(
            &view,
            "agent_models_run",
            json!({"ticket":ticket,"request":{
                "host":"","filter":"","action":"test","edit":bad
            }}),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("could not complete a request"),
            "{style}"
        );
    }

    let mut bad = edit.clone();
    let sidecar = dir.path().join("goose-acp");
    let temporary = dir.path().join("temporary-session");
    crate::test_executable::write_executable(
        &sidecar,
        r#"#!/bin/sh
[ "$#" -eq 0 ] || exit 1
read request
case "$request" in *'"method":"_goose/unstable/providers/list"'*)
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"entries":[{"providerId":"openai","defaultModel":"provider-default"}]}}'
  exit 0;; esac
[ "$(pwd)" = '__WORKSPACE__' ] || exit 1
[ "$GOOSE_PROVIDER $GOOSE_MODE" = 'openai chat' ] || exit 1
case "$GOOSE_MODEL" in effective-model|provider-default) ;; *) exit 1;; esac
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"sessionCapabilities":{"delete":{}}}}}'
read request
case "$request" in *'"hidden":true'*) ;; *) exit 1 ;; esac
case "$request" in *'"enabledExtensions":[]'*) ;; *) exit 1 ;; esac
: > '__TEMPORARY__'
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"temporary-test"}}'
read request
if [ "$OPENAI_API_KEY" = 'draft-key' ]; then
  printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"temporary-test","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"OK"}}}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
  read request
  case "$request" in *'"method":"_goose/unstable/session/export"'*) ;; *) exit 1 ;; esac
  printf '%s\n' '{"jsonrpc":"2.0","id":5,"result":{"data":"{\"id\":\"temporary-test\",\"conversation\":[{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"OK\"}]}]}"}}'
else
  printf '%s\n' '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"DO_NOT_PROJECT_PROVIDER_SECRET"}}'
fi
read request
case "$request" in *'"method":"session/delete"'*) ;; *) exit 1 ;; esac
case "$request" in *'"sessionId":"temporary-test"'*) ;; *) exit 1 ;; esac
rm '__TEMPORARY__'
printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{}}'
"#
        .replace("__WORKSPACE__", &dir.path().canonicalize().unwrap().display().to_string())
        .replace("__TEMPORARY__", &temporary.display().to_string()),
    );
    bad["harness"]["command"] = json!(sidecar);
    bad["harness"]["args"] = json!([]);
    for (key, default_model) in [
        ("draft-key", false),
        ("bad-key", false),
        ("draft-key", true),
    ] {
        if default_model {
            bad["harness"]["model"] = json!("");
            bad["environment"]
                .as_object_mut()
                .unwrap()
                .remove("GOOSE_MODEL");
        }
        bad["environment"]["OPENAI_API_KEY"] = json!(key);
        let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
        let result = invoke(
            &view,
            "agent_models_run",
            json!({"ticket":ticket,"request":{
                "host":"","filter":"","action":"test","edit":bad
            }}),
        );
        if key == "draft-key" {
            let result = result.unwrap();
            assert_eq!(result["models"], json!([]));
            if default_model {
                assert_eq!(result["testedModel"], "openai/provider-default");
            } else {
                assert!(result.get("testedModel").is_none());
            }
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("could not complete a request"));
            assert!(!error.contains("DO_NOT_PROJECT_PROVIDER_SECRET"));
        }
        assert!(
            !temporary.exists(),
            "the test must delete its own temporary session before returning"
        );
    }
}
#[test]
fn real_ipc_explicit_only_projection_overrides_retry_disconnect_and_gates() {
    let fake = Fake::default();
    let (dir, _, _app, view) = crate::agents::tests::fixture_with_models(|dir| {
        ModelHost::with_factory(Ok(dir.join("store")), Arc::new(fake.clone()))
    });
    let id = seed(dir.path());
    let call = |req: Value| {
        let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
        invoke(
            &view,
            "agent_models_run",
            json!({"ticket":ticket,"request":req}),
        )
    };
    for _ in 0..3 {
        invoke(&view, "agent_control_snapshot", json!({})).unwrap();
    }
    assert!(fake.opened.lock().unwrap().is_empty());
    let req = request(dir.path(), &id, "refresh");
    let result = call(req.clone()).unwrap();
    assert_eq!(fake.connects.load(Ordering::SeqCst), 0);
    assert_eq!(result["models"][0]["id"], "catalog.schema.model-service");
    assert_eq!(result["models"][0]["name"], "Model Service");
    assert!(!result.to_string().contains("DO_NOT_PROJECT"));
    let mut filtered = req.clone();
    filtered["filter"] = json!("endpoint-*");
    filtered["edit"]["harness"]["databricks"]["filter"] = filtered["filter"].clone();
    let filtered_result = call(filtered).unwrap();
    assert_eq!(
        filtered_result["models"],
        json!([{"id":"endpoint-two","name":"Endpoint Two"}])
    );
    let first_cache = fake.opened.lock().unwrap()[0].1.clone();
    assert_eq!(first_cache, dir.path().join("store/buzz-agent/oauth"));
    let mut connect = req.clone();
    connect["action"] = json!("connect");
    call(connect.clone()).unwrap();
    assert_eq!(fake.connects.load(Ordering::SeqCst), 0); // cached discovery does not sign in
    fake.failure.store(6, Ordering::SeqCst);
    assert!(call(req.clone()).is_err()); // Refresh must stay headless
    assert_eq!(fake.connects.load(Ordering::SeqCst), 0);
    call(connect.clone()).unwrap();
    assert_eq!(fake.connects.load(Ordering::SeqCst), 1);
    fake.failure.store(1, Ordering::SeqCst);
    assert!(call(connect.clone()).is_err());
    fake.failure.store(0, Ordering::SeqCst);
    call(connect).unwrap();
    for mode in [2, 3, 4, 5] {
        fake.failure.store(mode, Ordering::SeqCst);
        let response = call(req.clone());
        if mode == 2 || mode == 5 {
            assert!(response.is_err());
        } else {
            assert_eq!(response.unwrap()["models"], json!([]));
        }
    }
    fake.failure.store(0, Ordering::SeqCst);
    let mut other = req.clone();
    other["host"] = json!("https://other.example.com");
    other["edit"]["harness"]["databricks"]["host"] = other["host"].clone();
    call(other).unwrap();
    assert_eq!(fake.opened.lock().unwrap().last().unwrap().1, first_cache);
    let count = fake.opened.lock().unwrap().len();
    for patch in [
        json!({"DATABRICKS_TOKEN":"NEVER_PRINT"}),
        json!({"BUZZ_AGENT_PROVIDER":"other"}),
        json!({"DATABRICKS_HOST":"https://other.example.com"}),
        json!({"DATABRICKS_MODEL_FILTER":"other*"}),
    ] {
        let mut conflict = req.clone();
        conflict["edit"]["environment"] = patch;
        let error = call(conflict).unwrap_err().to_string();
        assert!(!error.contains("NEVER_PRINT"));
    }
    let mut stale = req.clone();
    stale["expectedRevision"] = json!(2);
    assert!(call(stale).is_err());
    assert_eq!(fake.opened.lock().unwrap().len(), count);
    let mut overridden = req.clone();
    overridden["edit"]["environment"] = json!({"BUZZ_AGENT_MODEL":"PRIVATE_MODEL"});
    let projected = call(overridden).unwrap();
    assert_eq!(projected["modelOverridden"], true);
    assert!(!projected.to_string().contains("PRIVATE_MODEL"));
    std::fs::create_dir_all(&first_cache).unwrap();
    let cache_key = "https://workspace.example.com/oidc/.well-known/oauth-authorization-server|databricks-cli|all-apis,offline_access";
    use sha2::{Digest, Sha256};
    let cached = first_cache
        .join("databricks")
        .join(format!("{:x}.json", Sha256::digest(cache_key.as_bytes())));
    std::fs::write(&cached, "SYNTHETIC").unwrap();
    let sentinel = dir.path().join("legacy-sentinel");
    std::fs::write(&sentinel, "UNCHANGED").unwrap();
    let mut disconnect = req;
    disconnect["action"] = json!("disconnect");
    disconnect.as_object_mut().unwrap().remove("edit");
    assert_eq!(call(disconnect).unwrap()["disconnected"], true);
    assert!(!cached.exists());
    assert!(first_cache.exists());
    assert!(sentinel.exists());
    let snapshot = invoke(&view, "agent_control_snapshot", json!({})).unwrap();
    assert_eq!(snapshot["agents"][0]["harness"]["model"], "sample");
    assert_eq!(snapshot["runtimeAvailable"], false);
    assert_eq!(snapshot["importAvailable"], cfg!(target_os = "macos"));
    invoke(
        &view,
        "agent_control_action",
        json!({"id":id,"action":"stop"}),
    )
    .unwrap();
    assert!(invoke(
        &view,
        "agent_control_action",
        json!({"id":id,"action":"start"})
    )
    .is_err());
}
#[test]
fn browse_uses_write_only_agent_defaults_workspace_and_filter_through_ipc() {
    let fake = Fake::default();
    let (dir, _, _app, view) = crate::agents::tests::fixture_with_models(|dir| {
        ModelHost::with_factory(Ok(dir.join("store")), Arc::new(fake.clone()))
    });
    let id = seed(dir.path());
    invoke(
        &view,
        "agent_control_save_defaults",
        json!({"edit":{"harness":"buzz-agent","provider":"databricks_v2","model":"","effort":"",
            "environment":{"DATABRICKS_HOST":"https://inherited.example.com",
                "DATABRICKS_MODEL_FILTER":"endpoint-*"}}}),
    )
    .unwrap();
    let call = |req: Value| {
        let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
        invoke(
            &view,
            "agent_models_run",
            json!({"ticket":ticket,"request":req}),
        )
    };
    // The UI cannot see write-only defaults, so it sends blanks and native
    // supplies the inherited workspace and filter.
    let mut req = request(dir.path(), &id, "refresh");
    req["host"] = json!("");
    req["inheritWorkspace"] = json!(true);
    req["edit"]["harness"]["provider"] = json!("");
    req["edit"]["harness"]
        .as_object_mut()
        .unwrap()
        .remove("databricks");
    let result = call(req.clone()).unwrap();
    assert_eq!(
        result["models"],
        json!([{"id":"endpoint-two","name":"Endpoint Two"}])
    );
    assert_eq!(result["host"], ""); // The write-only inherited URL stays native.
    assert!(!result.to_string().contains("https://inherited.example.com"));
    assert!(!result.to_string().contains("endpoint-*"));
    assert_eq!(
        fake.opened.lock().unwrap().last().unwrap().0,
        "https://inherited.example.com"
    );
    // An explicit, different workspace still conflicts instead of silently
    // browsing a workspace the launch would not use.
    req["host"] = json!("https://other.example.com");
    assert!(call(req.clone()).is_err());
    req["host"] = json!("");
    req["filter"] = json!("other-*");
    assert!(call(req.clone()).is_err());
    // Without the flag a blank is explicit and still conflicts with the
    // inherited workspace, as before.
    req["filter"] = json!("");
    req["inheritWorkspace"] = json!(false);
    assert!(call(req.clone()).is_err());

    // An explicit per-agent workspace cannot be bypassed by a caller that
    // manually sets inheritWorkspace, even while global env defaults exist.
    req["edit"]["harness"]["databricks"] = json!({
        "host":"https://agent.example.com", "filter":"agent-*"
    });
    req["inheritWorkspace"] = json!(true);
    assert!(call(req.clone()).is_err());
    req["inheritWorkspace"] = json!(false);
    req["host"] = json!("https://agent.example.com");
    req["filter"] = json!("agent-*");
    let result = call(req).unwrap();
    assert_eq!(result["host"], "https://agent.example.com");
    assert_eq!(
        fake.opened.lock().unwrap().last().unwrap().0,
        "https://agent.example.com"
    );
}

#[test]
fn disconnect_recovers_an_inherited_workspace_without_revealing_it() {
    let fake = Fake::default();
    let (dir, _, _app, view) = crate::agents::tests::fixture_with_models(|dir| {
        ModelHost::with_factory(Ok(dir.join("store")), Arc::new(fake.clone()))
    });
    let id = seed(dir.path());
    invoke(
        &view,
        "agent_control_save_defaults",
        json!({"edit":{"harness":"buzz-agent","provider":"databricks_v2","model":"","effort":"",
            "environment":{"DATABRICKS_HOST":"https://inherited.example.com"}}}),
    )
    .unwrap();
    let call = |req: Value| {
        let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
        invoke(
            &view,
            "agent_models_run",
            json!({"ticket":ticket,"request":req}),
        )
    };
    // Browse signs in against the inherited workspace.
    let mut browse = request(dir.path(), &id, "refresh");
    browse["host"] = json!("");
    browse["inheritWorkspace"] = json!(true);
    browse["edit"]["harness"]
        .as_object_mut()
        .unwrap()
        .remove("databricks");
    call(browse).unwrap();
    let cache = dir.path().join("store/buzz-agent/oauth/databricks");
    std::fs::create_dir_all(&cache).unwrap();
    let key = "https://inherited.example.com/oidc/.well-known/oauth-authorization-server|databricks-cli|all-apis,offline_access";
    use sha2::{Digest, Sha256};
    let cached = cache.join(format!("{:x}.json", Sha256::digest(key.as_bytes())));
    std::fs::write(&cached, "SYNTHETIC").unwrap();
    // Disconnect carries no draft and a blank host, like the picker sends.
    let disconnect = json!({"host":"","filter":"","action":"disconnect","inheritWorkspace":true});
    let result = call(disconnect.clone()).unwrap();
    assert_eq!(result["disconnected"], true);
    assert_eq!(result["host"], "");
    assert!(!result.to_string().contains("inherited.example.com"));
    assert!(!cached.exists());
    // Without the flag, a blank host is still refused.
    let mut unflagged = disconnect;
    unflagged["inheritWorkspace"] = json!(false);
    assert!(call(unflagged).is_err());
}

#[test]
fn native_discovery_preserves_absolute_harness_and_saved_or_draft_provider_overrides() {
    let fake = Fake::default();
    let (dir, _, _app, view) = crate::agents::tests::fixture_with_models(|dir| {
        ModelHost::with_factory(Ok(dir.join("store")), Arc::new(fake.clone()))
    });
    let id = seed(dir.path());
    let call = |req: Value| {
        let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
        invoke(
            &view,
            "agent_models_run",
            json!({"ticket":ticket,"request":req}),
        )
    };
    let mut req = request(dir.path(), &id, "connect");
    req["edit"]["harness"]["command"] = json!("/fixture/bin/buzz-agent");
    call(req.clone()).unwrap();
    req["edit"]["harness"]["provider"] = json!("selector-other");
    req["edit"]["environment"] = json!({"BUZZ_AGENT_PROVIDER":"databricks_v2"});
    call(req.clone()).unwrap();
    invoke(
        &view,
        "agent_control_save",
        json!({"id":id,"expectedRevision":1,"edit":req["edit"]}),
    )
    .unwrap();
    req["expectedRevision"] = json!(2);
    req["edit"]["environment"] = json!({}); // saved native override remains authoritative
    call(req.clone()).unwrap();
    let opens = fake.opened.lock().unwrap().len();
    req["edit"]["environment"] = json!({"BUZZ_AGENT_PROVIDER":null});
    assert!(call(req).is_err()); // effective unsupported provider never reaches auth
    assert_eq!(fake.opened.lock().unwrap().len(), opens);
    assert_eq!(fake.connects.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn real_ipc_refuses_linked_helper_namespace_before_opening_connection() {
    let fake = Fake::default();
    let (dir, _, _app, view) = crate::agents::tests::fixture_with_models(|dir| {
        ModelHost::with_factory(Ok(dir.join("store")), Arc::new(fake.clone()))
    });
    let id = seed(dir.path());
    let other = tempfile::tempdir().unwrap();
    let sentinel = other.path().join("untouched");
    std::fs::write(&sentinel, "SYNTHETIC").unwrap();
    let cache = buzz_agent_controller::connection::oauth_root(&dir.path().join("store")).unwrap();
    let namespace = cache.join("databricks");
    std::fs::create_dir_all(&namespace).unwrap();
    std::fs::remove_dir(&namespace).unwrap();
    std::os::unix::fs::symlink(other.path(), namespace).unwrap();
    for action in ["connect", "refresh", "disconnect"] {
        let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
        assert!(invoke(
            &view,
            "agent_models_run",
            json!({"ticket":ticket,"request":request(dir.path(), &id, action)})
        )
        .is_err());
    }
    assert!(fake.opened.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "SYNTHETIC");
}

#[cfg(unix)]
#[test]
fn pi_catalog_uses_native_ticket_and_draft_configuration_without_saving() {
    let (dir, _host, _app, view) = fixture();
    std::fs::create_dir(dir.path().join("local-config")).unwrap();
    let tools = dir.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    for tool in ["pi", "node", "buzz-pi-acp"] {
        let file = tools.join(tool);
        crate::test_executable::write_executable(
            &file,
            r#"#!/bin/sh
[ "$BUZZ_PRIVATE_KEY" = "" ] || exit 1
if [ "$1" = --version ]; then printf '0.99.1\n'; exit 0; fi
read request
[ "$PI_CODING_AGENT_DIR" -ef "./local-config" ] || exit 1
[ "$BUZZ_ACP_AGENTS" = "10" ] || exit 1
printf '%s\n' '{"id":"catalog","type":"response","command":"get_available_models","success":true,"data":{"models":[{"provider":"extension","id":"namespace/model.v1"}]}}'
"#,
        );
    }
    let ticket = invoke(&view, "agent_models_begin", json!({})).unwrap();
    let result=invoke(&view,"agent_models_run",json!({"ticket":ticket,"request":{
        "host":"","filter":"","action":"connect","edit":{
            "name":"Pi draft","systemPrompt":"","workspace":dir.path(),
            "harness":{"command":tools.join("buzz-pi-acp"),"args":[],"provider":"extension","model":"invalid-old-id"},
            "environment":{"PI_CODING_AGENT_DIR":dir.path().join("local-config"),"BUZZ_ACP_AGENTS":"10"}
        }
    }})).unwrap();
    assert_eq!(
        result["models"],
        json!([{"id":"extension/namespace/model.v1","name":"extension/namespace/model.v1"}])
    );
    assert_eq!(
        invoke(&view, "agent_control_snapshot", json!({})).unwrap()["agents"],
        json!([])
    );
}
