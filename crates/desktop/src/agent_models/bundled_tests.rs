//! Actual pinned binary over ACP, native Connect/catalog adapter, synthetic HTTP.
//! The private adapter accepts the fixture origin; public IPC still rejects HTTP.
//! This proves cache/refresh/wire reuse, NOT TLS, native browser UI or live relay.
use super::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
fn python() -> Command {
    let mut command = Command::new(if cfg!(windows) {
        "python"
    } else {
        "/usr/bin/python3"
    });
    command.env_clear();
    // Windows Python needs the OS directory to load its standard library/DLLs.
    // Do not inherit user Python configuration, credentials or proxy settings.
    #[cfg(windows)]
    command.env("SystemRoot", std::env::var_os("SystemRoot").unwrap());
    command
}
struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Callback(AtomicUsize);
impl BrowserOpener for Callback {
    fn open(&self, raw: &str) -> Result<(), String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let url = url::Url::parse(raw).unwrap();
        assert_eq!(url.path(), "/authorize");
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["client_id"], "databricks-cli");
        assert_eq!(query["scope"], "all-apis offline_access");
        assert_eq!(query["code_challenge_method"], "S256");
        let mut callback = url::Url::parse(&query["redirect_uri"]).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", "synthetic-code")
            .append_pair("state", &query["state"]);
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap())).unwrap();
        write!(
            stream,
            "GET {}?{} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            callback.path(),
            callback.query().unwrap()
        )
        .unwrap();
        std::thread::spawn(move || {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
        });
        Ok(())
    }
}
fn response(
    child: &mut Owned,
    receive: &std::sync::mpsc::Receiver<Value>,
    id: u64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    writeln!(
        child.0.stdin.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    )
    .unwrap();
    let mut notices = vec![];
    loop {
        let value = receive
            .recv_timeout(Duration::from_secs(15))
            .expect("ACP response timeout");
        if value["id"] == id && value.get("method").is_none() {
            assert!(value.get("error").is_none(), "{value}");
            return (value["result"].clone(), notices);
        }
        notices.push(value);
    }
}
#[tokio::test]
#[ignore = "requires staged immutable runtime resources; run explicitly after build-agent-runtime"]
async fn connect_catalog_actual_worker_inference_401_refresh_and_restart_share_cache() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("provider.json");
    let mut server = Owned(
        python()
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/agent_models/fixtures/provider.py"
            ))
            .arg(&log)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut origin = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut origin)
        .unwrap();
    let workspace = origin.trim().to_owned();
    assert!(workspace.starts_with("http://127.0.0.1:"));
    let config = dir.path().join("app");
    let cache = oauth_root(&config).unwrap();
    let opener = Arc::new(Callback(AtomicUsize::new(0)));
    let connection = RuntimeConnection::new(workspace.clone(), &cache, opener.clone()).unwrap();
    let connected = tokio::time::timeout(Duration::from_secs(8), connection.connect()).await;
    assert!(
        matches!(connected, Ok(Ok(()))),
        "Connect failed: {connected:?}; opener calls: {}; synthetic trace: {:?}",
        opener.0.load(Ordering::SeqCst),
        std::fs::read_to_string(&log)
    );
    assert_eq!(opener.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        connection.models(None).await.unwrap()[0].id,
        "synthetic-model"
    );
    // The app stages the runtime it bundles under its own resources.
    let tools =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src-tauri/resources/agent-runtime");
    buzz_agent_controller::RuntimeBundle::new(tools.clone()).expect("verified runtime resources");
    for iteration in 0..2 {
        let mut child = Owned(
            Command::new(tools.join("buzz-agent"))
                .env_clear()
                .env("BUZZ_AGENT_CONFIG_DIR", &config)
                .env("DATABRICKS_HOST", &workspace)
                .env("BUZZ_AGENT_PROVIDER", "databricks_v2")
                .env("BUZZ_AGENT_MODEL", "synthetic-model")
                .env("BUZZ_AGENT_HINTS_ENABLED", "false")
                .env("TMPDIR", dir.path())
                .env("PATH", "/usr/bin:/bin")
                .env("BUZZ_AGENT_LLM_TIMEOUT_SECS", "5")
                .current_dir(dir.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let (send, receive) = std::sync::mpsc::channel();
        let output = child.0.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if let Ok(value) =
                    line.and_then(|l| serde_json::from_str(&l).map_err(std::io::Error::other))
                {
                    let _ = send.send(value);
                }
            }
        });
        response(
            &mut child,
            &receive,
            1,
            "initialize",
            json!({"protocolVersion":1}),
        );
        let (session, _) = response(
            &mut child,
            &receive,
            2,
            "session/new",
            json!({"cwd":dir.path(),"mcpServers":[{"name":"buzz-dev-mcp","command":tools.join("buzz-dev-mcp"),"args":[],"env":[]}]}),
        );
        assert!(session.to_string().contains("synthetic-model"));
        let (_, notices) = response(
            &mut child,
            &receive,
            3,
            "session/prompt",
            json!({"sessionId":session["sessionId"],"prompt":[{"type":"text","text":"Synthetic request"}]}),
        );
        assert!(
            notices
                .iter()
                .any(|v| v.to_string().contains("SYNTHETIC_INFERENCE_OK")),
            "{notices:?}"
        );
        let state: Value = serde_json::from_slice(&std::fs::read(&log).unwrap()).unwrap();
        assert_eq!(
            state["grants"], 2,
            "one interactive grant and one 401 refresh, no new login on restart {iteration}"
        );
        assert_eq!(state["model"], "synthetic-model");
        assert!(
            state["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t.as_str().is_some_and(|n| n.contains("shell"))),
            "bundled MCP tools missing: {state}"
        );
    }
    // A NEW native reader consumes the rotated token written by the worker.
    let connection = RuntimeConnection::new(workspace, &cache, opener.clone()).unwrap();
    assert_eq!(
        connection.models(None).await.unwrap()[0].id,
        "synthetic-model"
    );
    assert_eq!(opener.0.load(Ordering::SeqCst), 1);
    let state: Value = serde_json::from_slice(&std::fs::read(&log).unwrap()).unwrap();
    assert_eq!(state["inferences"], 3);
}

// Same production execute + immutable engine; only HTTP origin validation is
// relaxed for the owned loopback provider. No app, worker or real browser opens.
#[tokio::test]
async fn discovery_rejected_locally_fresh_token_reopens_auth_before_login() {
    struct LoopbackFactory;
    impl Factory for LoopbackFactory {
        fn open(
            &self,
            workspace: &str,
            cache: &std::path::Path,
            opener: Arc<dyn BrowserOpener>,
        ) -> Result<Box<dyn Connection>, String> {
            assert!(workspace.starts_with("http://127.0.0.1:"));
            Ok(Box::new(RuntimeConnection::new(
                workspace.into(),
                cache,
                opener,
            )?))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("provider.json");
    let mut server = Owned(
        python()
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/agent_models/fixtures/provider.py"
            ))
            .arg(&log)
            .arg("catalog-rejection")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut origin = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut origin)
        .unwrap();
    let workspace = origin.trim().to_owned();
    let cache = oauth_root(&dir.path().join("app")).unwrap();
    let opener = Arc::new(Callback(AtomicUsize::new(0)));
    let connection = RuntimeConnection::new(workspace.clone(), &cache, opener.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(8), connection.connect())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(opener.0.load(Ordering::SeqCst), 1);
    // The provider issued a one-hour token; it is fresh locally but rejects its
    // catalog requests AND its refresh grant. execute must replace cached auth.
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        execute(
            Operation::Connect,
            workspace.clone(),
            None,
            cache.clone(),
            false,
            Arc::new(LoopbackFactory),
            opener.clone(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.models[0].id, "synthetic-model");
    assert_eq!(opener.0.load(Ordering::SeqCst), 2);
    // A subsequent picker request and headless Refresh both reuse the new cache.
    for action in [Operation::Connect, Operation::Refresh] {
        let result = execute(
            action,
            workspace.clone(),
            None,
            cache.clone(),
            false,
            Arc::new(LoopbackFactory),
            opener.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result.models[0].id, "synthetic-model");
    }
    assert_eq!(opener.0.load(Ordering::SeqCst), 2);
    let trace: Value = serde_json::from_slice(&std::fs::read(log).unwrap()).unwrap();
    assert_eq!(trace["grants"], 2);
    assert!(trace["requests"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry[1] == "Bearer synthetic-1"));
}
