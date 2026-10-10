//! Ephemeral Pi RPC catalog and connection test. No Buzz identity or saved
//! Pi session; only the test sends a prompt.
use buzz_agent_controller::pi::PiContext;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const FAILURE: &str = "Pi models unavailable. Check Pi sign-in and extension configuration, then retry or enter a custom ID";

// Extensions may spawn helpers. Cancellation must retire the whole lookup group.
struct LookupChild {
    child: tokio::process::Child,
    #[cfg(unix)]
    group: Option<u32>,
}
impl LookupChild {
    fn new(child: tokio::process::Child) -> Self {
        Self {
            #[cfg(unix)]
            group: child.id(),
            child,
        }
    }
}
impl Drop for LookupChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.group {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}

/// Checks the Pi CLI version before a context may run Pi. Used by Start and
/// ticket-owned model work, outside controller admission.
pub async fn verify(
    context: PiContext,
) -> Result<buzz_agent_controller::pi::VerifiedPiContext, String> {
    let mut command = tokio::process::Command::from(context.version_command());
    command.kill_on_drop(true);
    let mut child = LookupChild::new(
        command
            .spawn()
            .map_err(|_| context.version_error("Could not verify the Pi CLI version"))?,
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or("Could not verify the Pi CLI version")?;
        let mut output = String::new();
        stdout
            .take(129)
            .read_to_string(&mut output)
            .await
            .map_err(|_| "Could not verify the Pi CLI version")?;
        if output.len() > 128
            || !child
                .child
                .wait()
                .await
                .map_err(|_| "Could not verify the Pi CLI version")?
                .success()
        {
            return Err("Could not verify the Pi CLI version");
        }
        Ok(output)
    })
    .await
    .map_err(|_| context.version_error("Pi CLI version check timed out"))?
    .map_err(|error| context.version_error(error))?;
    context.accept_version(&result)
}

type Output = BufReader<tokio::io::Take<tokio::process::ChildStdout>>;
fn spawn(
    context: PiContext,
    args: Vec<String>,
) -> Result<(LookupChild, tokio::process::ChildStdin, Output), String> {
    let mut command = tokio::process::Command::new(&context.command);
    command
        .args(["--mode", "rpc", "--no-session", "--no-themes"])
        .args(args)
        .current_dir(&context.workspace)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for key in [
        "HOME",
        "TMPDIR",
        "USER",
        "LOGNAME",
        "LANG",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.envs(context.environment).env("PATH", context.path);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = LookupChild::new(
        command
            .spawn()
            .map_err(|error| format!("Could not start Pi: {error}"))?,
    );
    let stdin = child.child.stdin.take().ok_or(FAILURE)?;
    let stdout = child.child.stdout.take().ok_or(FAILURE)?;
    Ok((
        child,
        stdin,
        BufReader::new(stdout.take(8 * 1024 * 1024 + 1)),
    ))
}

/// Every `provider/model` ID Pi offers with this agent's configuration,
/// sorted and deduplicated. Starts no session and sends no prompt.
pub async fn fetch(context: PiContext) -> Result<Vec<String>, String> {
    let args = context.catalog_args()?;
    let (child, mut stdin, mut reader) = spawn(context, args)?;
    stdin
        .write_all(b"{\"id\":\"catalog\",\"type\":\"get_available_models\"}\n")
        .await
        .map_err(|_| FAILURE)?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        for _ in 0..100 {
            let mut line = String::new();
            if reader.read_line(&mut line).await.map_err(|_| FAILURE)? == 0 {
                break;
            }
            let value: Value = serde_json::from_str(&line).map_err(|_| FAILURE)?;
            if value.get("id") == Some(&json!("catalog")) {
                return parse_response(&value);
            }
        }
        Err(FAILURE.into())
    })
    .await
    .map_err(|_| "Pi model lookup timed out; retry explicitly")?;
    drop(stdin);
    // Drop kills helpers too, even after a successful response.
    drop(child);
    result
}

const TEST_FAILURE: &str =
    "Connection test failed. Check the provider, model and network, then test again.";

/// Verifies Pi's selected provider, then sends one tiny prompt through the
/// agent's setup. The first assistant reply decides the result, returned as
/// the `provider/model` that answered.
pub async fn test(context: PiContext, provider: &str, model: &str) -> Result<String, String> {
    if provider.is_empty() {
        return Err("Choose a provider to test".into());
    }
    if model.is_empty() {
        // Launch still requires a model. For this check validate the provider
        // independently, since Pi owns choosing a model inside its scope.
        if provider.len() > 128
            || provider.starts_with('-')
            || provider.contains([',', '/', '*', '?', '[', ']', '{', '}', '\\'])
            || provider.chars().any(char::is_control)
        {
            return Err("Invalid Pi provider ID".into());
        }
    } else {
        buzz_agent_controller::pi::validate_selection(provider, model)?;
    }
    let mut args = context.catalog_args()?;
    args.extend(
        [
            "--no-tools",
            "--no-context-files",
            "--no-skills",
            "--no-prompt-templates",
            "--system-prompt",
            "Reply with OK.",
        ]
        .map(String::from),
    );
    if model.is_empty() {
        args.extend(["--models".into(), format!("{provider}/*")]);
    } else {
        args.extend(["--model".into(), format!("{provider}/{model}")]);
    }
    let (child, mut stdin, mut reader) = spawn(context, args)?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        stdin
            .write_all(b"{\"id\":\"selection\",\"type\":\"get_state\"}\n")
            .await
            .map_err(|_| TEST_FAILURE)?;
        let selection = response(&mut reader, "selection").await?;
        let chosen = &selection["data"]["model"];
        let chosen_id = chosen["id"].as_str().unwrap_or_default();
        // Empty scopes can silently fall back to another signed-in provider.
        // Inspect Pi's actual selection BEFORE sending any inference request.
        if chosen["provider"] != provider || chosen_id.is_empty() {
            return Err("No test model available for this provider. Add its API key or sign in with Pi, then test again.".into());
        }
        buzz_agent_controller::pi::validate_selection(provider, chosen_id)?;
        if !model.is_empty() && chosen_id != model {
            return Err("Pi selected a different model. Check the model ID, then test again.".into());
        }
        stdin
            .write_all(b"{\"id\":\"test\",\"type\":\"prompt\",\"message\":\"Reply with OK.\"}\n")
            .await
            .map_err(|_| TEST_FAILURE)?;
        for _ in 0..10_000 {
            let mut line = String::new();
            if reader.read_line(&mut line).await.map_err(|_| TEST_FAILURE)? == 0 {
                break;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["id"] == "test" && value["success"] == false {
                return Err(classify(value["error"].as_str().unwrap_or_default()));
            }
            let message = &value["message"];
            if value["type"] == "message_end" && message["role"] == "assistant" {
                if matches!(message["stopReason"].as_str(), Some("error" | "aborted")) {
                    return Err(classify(message["errorMessage"].as_str().unwrap_or_default()));
                }
                if message["provider"] == provider && message["model"] == chosen_id
                    && message["content"].as_array().is_some_and(|content| content.iter().any(|item|
                        item["type"] == "text" && item["text"].as_str().is_some_and(|text| !text.trim().is_empty())
                    )) {
                    return Ok(format!("{provider}/{chosen_id}"));
                }
                return Err(TEST_FAILURE.into());
            }
        }
        Err(TEST_FAILURE.into())
    })
    .await
    .map_err(|_| "Connection test timed out. Check the network, then test again.")?;
    drop(stdin);
    drop(child);
    result
}

async fn response(reader: &mut Output, id: &str) -> Result<Value, String> {
    for _ in 0..100 {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .await
            .map_err(|_| TEST_FAILURE)?
            == 0
        {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value["id"] == id {
            return if value["success"] == true {
                Ok(value)
            } else {
                Err(classify(value["error"].as_str().unwrap_or_default()))
            };
        }
    }
    Err(TEST_FAILURE.into())
}

/// Provider errors can echo part of the key, so only fixed text leaves here.
fn classify(error: &str) -> String {
    let error = error.to_lowercase();
    let words: Vec<&str> = error.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let any = |codes: &[&str], phrases: &[&str]| {
        codes.iter().any(|code| words.contains(code))
            || phrases.iter().any(|phrase| error.contains(phrase))
    };
    if any(&[], &["no api key"]) {
        "No API key found for this provider. Add its API key, or sign in with Pi."
    } else if any(&["402"], &["quota", "credit", "billing"]) {
        "The provider account is out of credits or quota. Check its billing, then test again."
    } else if any(
        &["401", "403"],
        &[
            "invalid_api_key",
            "api key not valid",
            "api_key_invalid",
            "invalid x-api-key",
            "incorrect api key",
            "authentication",
            "unauthorized",
        ],
    ) {
        "The provider rejected the API key. Check the key, then test again."
    } else if any(&["429"], &["rate limit", "rate_limit", "too many requests"]) {
        "The provider is rate limiting requests. Wait a moment, then test again."
    } else if any(
        &["404"],
        &["model not found", "model_not_found", "does not exist"],
    ) {
        "The provider doesn’t recognize this model. Choose another model."
    } else {
        TEST_FAILURE
    }
    .into()
}
fn parse_response(value: &Value) -> Result<Vec<String>, String> {
    if value["type"] != "response"
        || value["command"] != "get_available_models"
        || value["success"] != true
    {
        return Err(FAILURE.into());
    }
    let models = value["data"]["models"].as_array().ok_or(FAILURE)?;
    if models.len() > 10_000 {
        return Err("Pi model catalog is too large".into());
    }
    let mut result = Vec::new();
    for model in models {
        let provider = model["provider"].as_str().ok_or(FAILURE)?;
        let id = model["id"].as_str().ok_or(FAILURE)?;
        if provider.is_empty() || id.is_empty() {
            return Err("Pi returned an invalid model ID".into());
        }
        buzz_agent_controller::pi::validate_selection(provider, id)
            .map_err(|_| "Pi returned an invalid model ID")?;
        result.push(format!("{provider}/{id}"));
    }
    result.sort();
    result.dedup();
    Ok(result)
}

#[cfg(test)]
mod tests;
