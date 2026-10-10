//! One-shot Goose model lookup and inference checks. No child receives a Buzz identity.
use buzz_agent_controller::GooseModelContext;
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TEST_BYTES: u64 = 1024 * 1024;
const METHOD: &str = "_goose/unstable/providers/supported-models/list";
const TEST_FAILURE: &str = "Goose could not complete a request with this provider and model. Check its credentials, model and network, then test again.";
const TEST_TIMEOUT: &str = "Goose connection test timed out. Check the network, then test again.";
const CLEANUP_FAILURE: &str = "Goose could not delete its temporary connection-test session. It may remain in Goose history; retry the test.";

struct CheckChild(tokio::process::Child);
impl Drop for CheckChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.id() {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        let _ = self.0.start_kill();
    }
}

/// Run one Goose turn without tools. ACP uses a hidden temporary session;
/// the external CLI can avoid saving a session altogether.
/// The selected draft and write-only overrides are resolved by the controller.
/// Returns the `provider/model` that replied, which may be Goose's default.
pub async fn test(mut context: GooseModelContext) -> Result<String, String> {
    if context.model_id.is_empty() {
        // Goose owns defaults for built-in and dynamically registered providers.
        // Resolve only the requested provider, never a saved model from another.
        let response = rpc(
            &context,
            "_goose/unstable/providers/list",
            json!({"providerIds":[context.provider_id]}),
        )
        .await?;
        if response.get("error").is_some() {
            return Err(TEST_FAILURE.into());
        }
        context.model_id = response["result"]["entries"]
            .as_array()
            .and_then(|entries| entries.iter().find(|entry| entry["providerId"] == context.provider_id))
            .and_then(|entry| entry["defaultModel"].as_str())
            .filter(|model| !model.trim().is_empty())
            .ok_or("Goose has no default test model for this provider. Choose a model, then test again.")?
            .to_owned();
    }
    if context.model_id.trim().is_empty()
        || context.model_id.len() > 512
        || context.model_id.chars().any(char::is_control)
    {
        return Err("Choose a valid Goose model to test".into());
    }
    if !context.workspace.is_absolute() || !context.workspace.is_dir() {
        return Err("Choose an existing absolute workspace before testing Goose".into());
    }
    let tested_model = format!("{}/{}", context.provider_id, context.model_id);
    if context.command.file_stem().and_then(|name| name.to_str()) == Some("goose-acp") {
        return test_acp(context).await.map(|()| tested_model);
    }
    let mut command = goose_command(&context);
    command
        .args([
            "run",
            "--text",
            "Reply OK.",
            "--no-session",
            "--no-profile",
            "--max-turns",
            "1",
            "--quiet",
            "--output-format",
            "json",
        ])
        .current_dir(&context.workspace)
        .stdin(Stdio::null());
    command
        .env("GOOSE_PROVIDER", context.provider_id)
        .env("GOOSE_MODEL", context.model_id);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = CheckChild(
        command
            .spawn()
            .map_err(|error| format!("Could not start Goose to test the model: {error}"))?,
    );
    let stdout = child.0.stdout.take().ok_or(TEST_FAILURE)?;
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut output = Vec::new();
        stdout
            .take(MAX_TEST_BYTES + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|_| TEST_FAILURE)?;
        if output.len() as u64 > MAX_TEST_BYTES {
            return Err(TEST_FAILURE.into());
        }
        let status = child.0.wait().await.map_err(|_| TEST_FAILURE)?;
        let response: Value = serde_json::from_slice(&output).map_err(|_| TEST_FAILURE)?;
        if status.success() && successful_reply(&response) {
            Ok(tested_model)
        } else {
            Err(TEST_FAILURE.into())
        }
    })
    .await
    .map_err(|_| TEST_TIMEOUT.to_owned())?
}

// Holds the transport and partial line across cancellation, so Drop can finish
// an in-flight session/new and delete only the session created by this test.
struct AcpTest {
    _child: CheckChild,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::io::Take<tokio::process::ChildStdout>>,
    line: Vec<u8>,
    creating: bool,
    deleting: bool,
    session_id: Option<String>,
}

impl AcpTest {
    async fn send(&mut self, id: u64, method: &str, params: Value) -> Result<(), String> {
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.input
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|_| TEST_FAILURE.to_owned())
    }

    async fn response(&mut self, id: u64) -> Result<Value, String> {
        loop {
            if self
                .output
                .read_until(b'\n', &mut self.line)
                .await
                .map_err(|_| TEST_FAILURE)?
                == 0
            {
                return Err(TEST_FAILURE.into());
            }
            let line = std::mem::take(&mut self.line);
            let value: Value = serde_json::from_slice(&line).map_err(|_| TEST_FAILURE)?;
            if value.get("id").and_then(Value::as_u64) == Some(id) && value.get("method").is_none()
            {
                if id == 2 {
                    self.creating = false;
                    self.session_id = value["result"]["sessionId"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned);
                }
                if value.get("error").is_some() {
                    return Err(TEST_FAILURE.into());
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| TEST_FAILURE.to_owned());
            }
            if value.get("id").is_some() && value.get("method").is_some() {
                // This check grants no file, terminal, or permission requests.
                return Err(TEST_FAILURE.into());
            }
        }
    }

    async fn cleanup(&mut self) -> Result<(), String> {
        if self.creating {
            // A rejected session/new already cleans itself up in pinned Goose.
            let _ = tokio::time::timeout(Duration::from_secs(30), self.response(2)).await;
            if self.creating {
                return Err(CLEANUP_FAILURE.into());
            }
        }
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        self.output.get_mut().set_limit(MAX_TEST_BYTES + 1);
        tokio::time::timeout(Duration::from_secs(5), async {
            if !self.deleting {
                self.deleting = true;
                self.send(4, "session/delete", json!({"sessionId":session_id}))
                    .await?;
            }
            self.response(4).await?;
            self.session_id = None;
            Ok::<_, String>(())
        })
        .await
        .map_err(|_| CLEANUP_FAILURE.to_owned())?
        .map_err(|_| CLEANUP_FAILURE.to_owned())
    }
}

struct AcpTestGuard(Option<AcpTest>);
impl Drop for AcpTestGuard {
    fn drop(&mut self) {
        if let Some(mut test) = self.0.take() {
            tokio::spawn(async move {
                if test.cleanup().await.is_err() {
                    eprintln!("{CLEANUP_FAILURE}");
                }
            });
        }
    }
}

async fn test_acp(context: GooseModelContext) -> Result<(), String> {
    let mut command = goose_command(&context);
    command
        .current_dir(&context.workspace)
        .stdin(Stdio::piped());
    command
        .env("GOOSE_PROVIDER", context.provider_id)
        .env("GOOSE_MODEL", context.model_id)
        .env("GOOSE_MODE", "chat")
        // Suppress the sidecar's default developer builtin without altering user configuration.
        .env(
            "EXTENSIONS",
            r#"{"developer":{"type":"builtin","name":"developer","enabled":false}}"#,
        );
    #[cfg(unix)]
    command.process_group(0);
    let mut child = CheckChild(
        command
            .spawn()
            .map_err(|error| format!("Could not start Goose to test the model: {error}"))?,
    );
    let input = child.0.stdin.take().ok_or(TEST_FAILURE)?;
    let output = BufReader::new(
        child
            .0
            .stdout
            .take()
            .ok_or(TEST_FAILURE)?
            .take(MAX_TEST_BYTES + 1),
    );
    let mut guard = AcpTestGuard(Some(AcpTest {
        _child: child,
        input,
        output,
        line: Vec::new(),
        creating: false,
        deleting: false,
        session_id: None,
    }));
    let test = guard.0.as_mut().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        test.send(1, "initialize", json!({"protocolVersion":1,"clientInfo":{"name":"buzz-connection-test","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{}})).await?;
        let initialized = test.response(1).await?;
        if initialized["protocolVersion"] != 1 || !initialized["agentCapabilities"]["sessionCapabilities"]["delete"].is_object() {
            return Err("Goose does not support temporary connection-test session cleanup".into());
        }
        test.creating = true;
        test.send(2, "session/new", json!({"cwd":context.workspace,"mcpServers":[],"_meta":{"hidden":true,"sessionTitle":"Buzz connection test","enabledExtensions":[]}})).await?;
        test.response(2).await?;
        let session_id = test.session_id.as_ref().ok_or(TEST_FAILURE)?.clone();
        test.send(3, "session/prompt", json!({"sessionId":session_id,"prompt":[{"type":"text","text":"Reply OK."}]})).await?;
        let response = test.response(3).await?;
        if !matches!(response["stopReason"].as_str(), Some("end_turn" | "max_tokens")) { return Err(TEST_FAILURE.into()); }
        test.send(5, "_goose/unstable/session/export", json!({"sessionId":session_id,"format":"json"})).await?;
        let exported = test.response(5).await?;
        let session: Value = serde_json::from_str(exported["data"].as_str().ok_or(TEST_FAILURE)?).map_err(|_| TEST_FAILURE)?;
        if session["id"] == session_id && assistant_reply(&session["conversation"]) { Ok(()) } else { Err(TEST_FAILURE.into()) }
    }).await.unwrap_or_else(|_| Err(TEST_TIMEOUT.into()));
    let cleanup = test.cleanup().await;
    // Cleanup was awaited; retire the one-shot child without scheduling it again.
    if let Some(mut test) = guard.0.take() {
        let _ = test._child.0.kill().await;
        let _ = test._child.0.wait().await;
    }
    cleanup?;
    result
}

fn successful_reply(response: &Value) -> bool {
    if response["metadata"]["status"] != "completed" {
        return false;
    }
    // Goose also emits local provider errors as assistant text and exits zero.
    // A fresh one-turn test needs usage evidence from an actual model response.
    if !response["metadata"]["total_tokens"]
        .as_u64()
        .is_some_and(|tokens| tokens > 0)
    {
        return false;
    }
    assistant_reply(&response["messages"])
}

fn assistant_reply(messages: &Value) -> bool {
    let Some(messages) = messages.as_array() else {
        return false;
    };
    let mut replied = false;
    for content in messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .filter_map(|message| message["content"].as_array())
        .flatten()
    {
        if content["type"] == "error" {
            return false;
        }
        if content["type"] == "text"
            && content["text"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty())
        {
            replied = true;
        }
    }
    replied
}

// The same native-only environment boundary applies to catalogs and both tests.
fn goose_command(context: &GooseModelContext) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&context.command);
    command
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in [
        "HOME",
        "TMPDIR",
        "USER",
        "LOGNAME",
        "LANG",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .envs(&context.environment)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    command
}

/// The provider's model IDs from Goose's live catalog. Goose may start a
/// provider sign-in on a cache miss, so call it only on explicit user intent.
pub async fn fetch(context: GooseModelContext) -> Result<Vec<String>, String> {
    let response = rpc(&context, METHOD, json!({"providerId":context.provider_id})).await?;
    parse_response(&response, &context.provider_id)
}

/// Shared one-shot ACP lookup for the live model list and provider metadata.
async fn rpc(context: &GooseModelContext, method: &str, params: Value) -> Result<Value, String> {
    let mut command = goose_command(context);
    command.args(&context.args).stdin(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = CheckChild(
        command
            .spawn()
            .map_err(|error| format!("Could not start Goose to list models: {error}"))?,
    );
    let mut stdin = child
        .0
        .stdin
        .take()
        .ok_or("Goose catalog input unavailable")?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    });
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .map_err(|_| "Could not request Goose models".to_owned())?;
    let stdout = child
        .0
        .stdout
        .take()
        .ok_or("Goose catalog output unavailable")?;
    let mut reader = BufReader::new(stdout.take(MAX_RESPONSE_BYTES + 1));
    let response = tokio::time::timeout(Duration::from_secs(60), async {
        for _ in 0..100 {
            let mut line = String::new();
            if reader
                .read_line(&mut line)
                .await
                .map_err(|_| "Could not read Goose models")?
                == 0
            {
                break;
            }
            let value: Value = serde_json::from_str(&line)
                .map_err(|_| "Goose returned an invalid model response")?;
            if value.get("id") == Some(&json!(1)) {
                return Ok(value);
            }
        }
        Err("Goose did not return a model list".to_owned())
    })
    .await
    .map_err(|_| "Goose model lookup timed out; retry explicitly".to_owned())?;
    drop(stdin);
    drop(child);
    response
}

fn parse_response(value: &Value, provider_id: &str) -> Result<Vec<String>, String> {
    if let Some(error) = value.get("error") {
        if error.get("code").and_then(Value::as_i64) == Some(-32000) {
            return Err("Goose needs authentication for this provider. Enter its API key in Buzz if it uses one, then retry".into());
        }
        return Err("Goose could not list models for this provider. Check its credentials or try again when its API is available".into());
    }
    if value
        .get("result")
        .and_then(|result| result.get("providerId"))
        .and_then(Value::as_str)
        != Some(provider_id)
    {
        return Err("Goose returned models for a different provider".into());
    }
    let models = value
        .get("result")
        .and_then(|result| result.get("models"))
        .and_then(Value::as_array)
        .ok_or("Goose returned an invalid model list")?;
    if models.len() > 10_000 {
        return Err("Goose model list is too large to display".into());
    }
    models
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|name| {
                    !name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)
                })
                .map(str::to_owned)
                .ok_or_else(|| "Goose returned an invalid model name".to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app's test agent host refuses keychain access the same way.
    struct NoCredentials;
    impl buzz_agent_controller::Credentials for NoCredentials {
        fn read_legacy(
            &self,
            _: buzz_agent_controller::LegacySource,
            _: &str,
        ) -> Result<buzz_agent_controller::Secret, String> {
            Err("Keychain access is disabled in tests".into())
        }
        fn read(&self, _: &str, _: &str) -> Result<Option<buzz_agent_controller::Secret>, String> {
            Err("Keychain access is disabled in tests".into())
        }
        fn add(&self, _: &str, _: &buzz_agent_controller::Secret) -> Result<(), String> {
            Err("Keychain access is disabled in tests".into())
        }
        fn delete(&self, _: &str, _: &str) -> Result<(), String> {
            Err("Keychain access is disabled in tests".into())
        }
    }

    #[tokio::test]
    #[ignore = "requires installed Goose and a signed-in BUZZ_TEST_GOOSE_PROVIDER; sends tiny live requests"]
    async fn installed_goose_provider_test_uses_production_context() {
        use buzz_agent_controller::{AgentEdit, HarnessEdit};
        let command =
            std::env::var("BUZZ_TEST_GOOSE_COMMAND").expect("set BUZZ_TEST_GOOSE_COMMAND");
        let provider =
            std::env::var("BUZZ_TEST_GOOSE_PROVIDER").expect("set BUZZ_TEST_GOOSE_PROVIDER");
        let dir = tempfile::tempdir().unwrap();
        // The app resolves drafts as below, over a controller like its own.
        let controller = buzz_agent_controller::Controller::new(
            buzz_agent_controller::Store::open(dir.path().join("store")).unwrap(),
            std::sync::Arc::new(NoCredentials),
            Err("Runtime bundle is missing".into()),
            dir.path().join("ownership"),
        );
        let context = |provider: &str, environment| {
            buzz_agent_controller::prepare_tools_path();
            controller.draft_goose_model_context(controller.effective_draft(AgentEdit {
                effort: None,
                name: "Probe".into(),
                picture: None,
                system_prompt: String::new(),
                session_policy: Some(None),
                workspace: dir.path().display().to_string(),
                harness: HarnessEdit {
                    command: command.clone(),
                    args: vec!["acp".into()],
                    provider: provider.into(),
                    model: String::new(),
                    databricks: None,
                },
                environment,
            })?)
        };
        let providers = [
            "openai",
            "anthropic",
            "google",
            "openrouter",
            "databricks_v2",
        ];
        let metadata = rpc(
            &context(&provider, Default::default()).unwrap(),
            "_goose/unstable/providers/list",
            json!({"providerIds":providers}),
        )
        .await
        .unwrap();
        let entries = metadata["result"]["entries"].as_array().unwrap();
        for id in providers {
            let entry = entries
                .iter()
                .find(|entry| entry["providerId"] == id)
                .expect("requested provider metadata");
            assert!(entry["defaultModel"]
                .as_str()
                .is_some_and(|model| !model.is_empty()));
            println!("Goose {id} default: {}", entry["defaultModel"]);
        }
        let tested = test(context(&provider, Default::default()).unwrap())
            .await
            .unwrap();
        assert!(tested.starts_with(&format!("{provider}/")));
        println!("Goose provider-only test replied using {tested}");
        for (provider, key) in [
            ("openai", "OPENAI_API_KEY"),
            ("anthropic", "ANTHROPIC_API_KEY"),
            ("google", "GOOGLE_API_KEY"),
            ("openrouter", "OPENROUTER_API_KEY"),
        ] {
            let result = test(
                context(
                    provider,
                    [(key.to_owned(), Some("buzz-invalid-key".to_owned()))].into(),
                )
                .unwrap(),
            )
            .await;
            assert!(
                result.is_err(),
                "invalid {provider} credential must not succeed"
            );
            assert!(!result.unwrap_err().contains("buzz-invalid-key"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn acp_connection_test_checks_reply_and_cleans_up_on_failure_or_cancel() {
        use std::{
            io::{BufRead, Read, Write},
            os::fd::FromRawFd,
        };
        const SOCKET: &str = "BUZZ_GOOSE_TEST_SOCKET";
        const CASE: &str = "BUZZ_GOOSE_TEST_CASE";
        if let Ok(socket) = std::env::var(SOCKET) {
            let case = std::env::var(CASE).unwrap();
            let mut gate = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            gate.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut input = std::io::stdin().lock();
            let mut output = unsafe { std::fs::File::from_raw_fd(3) };
            let mut request = || {
                let mut line = String::new();
                assert_ne!(input.read_line(&mut line).unwrap(), 0);
                serde_json::from_str::<Value>(&line).unwrap()
            };
            let initialize = request();
            assert_eq!(initialize["method"], "initialize");
            let capabilities = if case == "no-delete" {
                json!({})
            } else {
                json!({"sessionCapabilities":{"delete":{}}})
            };
            if case == "no-delete" {
                // The reply lets the controller kill this child; notify first.
                gate.write_all(b"I").unwrap();
            }
            writeln!(output, "{}", json!({"jsonrpc":"2.0","id":initialize["id"],"result":{"protocolVersion":1,"agentCapabilities":capabilities}})).unwrap();
            if case == "no-delete" {
                // The parent must return without creating a session.
                let mut line = String::new();
                assert_eq!(input.read_line(&mut line).unwrap(), 0);
                return;
            }
            let new = request();
            assert_eq!(new["method"], "session/new");
            assert_eq!(new["params"]["_meta"]["hidden"], true);
            assert_eq!(new["params"]["_meta"]["enabledExtensions"], json!([]));
            let session_file = std::path::Path::new(&socket).with_file_name("session");
            std::fs::write(&session_file, "temporary").unwrap();
            let session =
                json!({"jsonrpc":"2.0","id":new["id"],"result":{"sessionId":"test-session"}})
                    .to_string();
            // Hold session creation during a partial response before cancellation.
            let split = session.len() / 2;
            output.write_all(&session.as_bytes()[..split]).unwrap();
            gate.write_all(b"C").unwrap();
            gate.read_exact(&mut [0]).unwrap();
            writeln!(output, "{}", &session[split..]).unwrap();
            let mut next = request();
            if next["method"] == "session/prompt" {
                assert_eq!(next["params"]["sessionId"], "test-session");
                assert_eq!(
                    next["params"]["prompt"],
                    json!([{"type":"text","text":"Reply OK."}])
                );
                gate.write_all(b"P").unwrap();
                gate.read_exact(&mut [0]).unwrap();
                if !matches!(case.as_str(), "cancel-prompt" | "timeout") {
                    if case == "provider-error" {
                        writeln!(output, "{}", json!({"jsonrpc":"2.0","id":next["id"],"error":{"code":-32000,"message":"DO_NOT_PROJECT_SECRET"}})).unwrap();
                    } else {
                        let session_id = if case == "wrong-session" {
                            "other-session"
                        } else {
                            "test-session"
                        };
                        let update = if case == "thought-only" {
                            "agent_thought_chunk"
                        } else {
                            "agent_message_chunk"
                        };
                        let text = if case == "empty" { " " } else { "OK" };
                        writeln!(output, "{}", json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"update":{"sessionUpdate":update,"content":{"type":"text","text":text}}}})).unwrap();
                        let stop = if case == "cancelled" {
                            "cancelled"
                        } else if matches!(case.as_str(), "max-tokens" | "synthetic-limit") {
                            "max_tokens"
                        } else {
                            "end_turn"
                        };
                        writeln!(
                            output,
                            "{}",
                            json!({"jsonrpc":"2.0","id":next["id"],"result":{"stopReason":stop}})
                        )
                        .unwrap();
                    }
                }
                next = request();
            }
            if next["method"] == "_goose/unstable/session/export" {
                assert_eq!(
                    next["params"],
                    json!({"sessionId":"test-session","format":"json"})
                );
                let content = match case.as_str() {
                    "error-text" => {
                        json!([{"type":"text","text":"Provider failed"},{"type":"error","message":"DO_NOT_PROJECT_SECRET","kind":"other"}])
                    }
                    "empty" | "synthetic-limit" => json!([]),
                    "thought-only" => json!([{"type":"thinking","thinking":"OK"}]),
                    _ => json!([{"type":"text","text":"OK"}]),
                };
                let id = if case == "wrong-session" {
                    "other-session"
                } else {
                    "test-session"
                };
                let data = json!({"id":id,"conversation":[{"role":"assistant","content":content}]})
                    .to_string();
                writeln!(
                    output,
                    "{}",
                    json!({"jsonrpc":"2.0","id":next["id"],"result":{"data":data}})
                )
                .unwrap();
                next = request();
            }
            assert_eq!(next["method"], "session/delete");
            assert_eq!(next["params"]["sessionId"], "test-session");
            if case != "cleanup-error" {
                std::fs::remove_file(session_file).unwrap();
            }
            // Notify before the terminal reply permits child teardown.
            gate.write_all(b"D").unwrap();
            if case == "cleanup-error" {
                writeln!(output, "{}", json!({"jsonrpc":"2.0","id":next["id"],"error":{"code":-32603,"message":"DO_NOT_PROJECT_SECRET"}})).unwrap();
            } else {
                writeln!(
                    output,
                    "{}",
                    json!({"jsonrpc":"2.0","id":next["id"],"result":{}})
                )
                .unwrap();
            }
            return;
        }
        let dir = tempfile::Builder::new()
            .prefix("goose-check")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("gate");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let command = dir.path().join("goose-acp");
        crate::test_executable::write_executable(
            &command,
            r#"#!/bin/sh
[ "$#" -eq 0 ] || exit 1
exec "$BUZZ_GOOSE_TEST_EXE" --exact goose_models::tests::acp_connection_test_checks_reply_and_cleans_up_on_failure_or_cancel --nocapture 3>&1 >/dev/null
"#,
        );
        for case in [
            "success",
            "max-tokens",
            "provider-error",
            "error-text",
            "synthetic-limit",
            "empty",
            "wrong-session",
            "thought-only",
            "cancelled",
            "cleanup-error",
            "no-delete",
            "cancel-new",
            "cancel-prompt",
            "timeout",
        ] {
            let context = GooseModelContext {
                command: command.clone(),
                args: vec![],
                workspace: dir.path().into(),
                provider_id: "openai".into(),
                model_id: "test-model".into(),
                model_overridden: false,
                environment: [
                    (SOCKET.into(), socket.to_str().unwrap().into()),
                    (CASE.into(), case.into()),
                    (
                        "BUZZ_GOOSE_TEST_EXE".into(),
                        std::env::current_exe().unwrap().to_str().unwrap().into(),
                    ),
                ]
                .into_iter()
                .collect(),
            };
            let task = tokio::spawn(test(context));
            let mut gate = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap()
                .0;
            let mut stage = [0];
            gate.read_exact(&mut stage).await.unwrap();
            if case == "no-delete" {
                assert_eq!(stage, *b"I");
                assert!(task
                    .await
                    .unwrap()
                    .unwrap_err()
                    .contains("does not support"));
                assert!(!dir.path().join("session").exists());
                continue;
            }
            assert_eq!(stage, *b"C");
            assert!(dir.path().join("session").exists());
            if case == "cancel-new" {
                task.abort();
            }
            gate.write_all(&[1]).await.unwrap();
            if case != "cancel-new" {
                gate.read_exact(&mut stage).await.unwrap();
                assert_eq!(stage, *b"P");
                if case == "cancel-prompt" {
                    task.abort();
                }
                if case == "timeout" {
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(31)).await;
                    tokio::time::resume();
                }
                gate.write_all(&[1]).await.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(10), gate.read_exact(&mut stage))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stage, *b"D", "{case}");
            let result = task.await;
            if case.starts_with("cancel-") {
                assert!(result.unwrap_err().is_cancelled());
            } else if matches!(case, "success" | "max-tokens") {
                result.unwrap().unwrap();
            } else {
                let error = result.unwrap().unwrap_err();
                assert!(!error.contains("DO_NOT_PROJECT_SECRET"));
                if case == "cleanup-error" {
                    assert_eq!(error, CLEANUP_FAILURE);
                } else if case == "timeout" {
                    assert_eq!(error, TEST_TIMEOUT);
                } else {
                    assert_eq!(error, TEST_FAILURE);
                }
            }
            if case == "cleanup-error" {
                std::fs::remove_file(dir.path().join("session")).unwrap();
            } else {
                assert!(!dir.path().join("session").exists(), "{case}");
            }
        }
    }

    #[test]
    fn accepts_only_bounded_model_names() {
        assert_eq!(
            parse_response(
                &json!({"result":{"providerId":"anthropic","models":["claude-opus-4-8"]}}),
                "anthropic"
            )
            .unwrap(),
            vec!["claude-opus-4-8"]
        );
        assert!(parse_response(
            &json!({"result":{"providerId":"anthropic","models":["bad\nname"]}}),
            "anthropic"
        )
        .is_err());
        assert!(
            parse_response(&json!({"error":{"message":"secret"}}), "anthropic")
                .unwrap_err()
                .contains("Check its credentials")
        );
        let auth_error = parse_response(
            &json!({"error":{"code":-32000,"data":"secret"}}),
            "anthropic",
        )
        .unwrap_err();
        assert!(auth_error.contains("needs authentication"));
        assert!(!auth_error.contains("secret"));
        assert!(parse_response(
            &json!({"result":{"providerId":"openai","models":[]}}),
            "anthropic"
        )
        .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_shot_acp_request_reads_catalog_before_closing_stdin() {
        use std::{
            future::Future,
            io::{BufRead, Read, Write},
            os::fd::FromRawFd,
        };
        const SOCKET: &str = "BUZZ_GOOSE_TEST_SOCKET";
        if let Ok(socket) = std::env::var(SOCKET) {
            // Re-enter this test as the fake Goose process. The wrapper reserves
            // fd 3 for protocol output and sends libtest's output to /dev/null.
            let mut request = String::new();
            std::io::stdin().lock().read_line(&mut request).unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(
                request["method"],
                "_goose/unstable/providers/supported-models/list"
            );
            assert_eq!(request["params"]["providerId"], "openai");
            assert_eq!(request["id"], 1);
            assert_eq!(std::env::var("OPENAI_API_KEY").unwrap(), "test-key");
            let mut gate = std::os::unix::net::UnixStream::connect(socket).unwrap();
            gate.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            gate.write_all(&[1]).unwrap();
            gate.read_exact(&mut [0]).unwrap();
            let mut input = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            };
            // No data remains after the request. Readability now means premature EOF.
            assert_eq!(
                unsafe { libc::poll(&mut input, 1, 0) },
                0,
                "stdin closed before catalog response"
            );
            let mut output = unsafe { std::fs::File::from_raw_fd(3) };
            writeln!(output, "{}", json!({"jsonrpc":"2.0","id":1,"result":{"providerId":"openai","models":["gpt-6-sol"]}})).unwrap();
            return;
        }
        // Keep the socket pathname within macOS's sockaddr_un limit.
        let dir = tempfile::Builder::new()
            .prefix("goose-test")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("gate");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        for (name, args, argument_check) in [
            (
                "goose",
                vec!["acp".into()],
                r#"[ "$#" -eq 1 ] && [ "$1" = acp ] || exit 1"#,
            ),
            ("goose-acp", vec![], r#"[ "$#" -eq 0 ] || exit 1"#),
        ] {
            let command = dir.path().join(name);
            crate::test_executable::write_executable(
                &command,
                format!(
                    r#"#!/bin/sh
{argument_check}
exec "$BUZZ_GOOSE_TEST_EXE" --exact goose_models::tests::one_shot_acp_request_reads_catalog_before_closing_stdin --nocapture 3>&1 >/dev/null
"#
                ),
            );
            let context = GooseModelContext {
                command,
                args,
                workspace: dir.path().into(),
                provider_id: "openai".into(),
                model_id: "gpt-6-sol".into(),
                environment: [
                    ("OPENAI_API_KEY".into(), "test-key".into()),
                    (SOCKET.into(), socket.to_str().unwrap().into()),
                    (
                        "BUZZ_GOOSE_TEST_EXE".into(),
                        std::env::current_exe().unwrap().to_str().unwrap().into(),
                    ),
                ]
                .into_iter()
                .collect(),
                model_overridden: false,
            };
            tokio::time::timeout(Duration::from_secs(10), async {
            let mut lookup = std::pin::pin!(fetch(context));
            let mut gate = tokio::select! {
                accepted = listener.accept() => accepted.unwrap().0,
                result = &mut lookup => panic!("lookup completed before request gate: {result:?}"),
            };
            let mut ready = [0];
            tokio::select! {
                ready = gate.read_exact(&mut ready) => { ready.unwrap(); },
                result = &mut lookup => panic!("lookup completed before request read: {result:?}"),
            }
            // Advance past write_all before the child inspects stdin. The child
            // cannot answer yet, so an early close is observable without sleeps.
            std::future::poll_fn(|cx| {
                assert!(lookup.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            gate.write_all(&[1]).await.unwrap();
            assert_eq!(lookup.await.unwrap(), vec!["gpt-6-sol"]);
        })
        .await
        .expect("catalog fixture did not finish");
        }
    }
}
