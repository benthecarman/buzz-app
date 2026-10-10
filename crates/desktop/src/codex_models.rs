//! Codex model catalog from the selected CLI's `codex debug models`. No ACP
//! session is opened, so no prompt is sent and configured MCP servers stay off.
#![cfg(unix)]
use crate::goose_models::CheckChild;
use buzz_agent_controller::codex::CodexContext;
use serde::Deserialize;
use std::{process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
const UNREADABLE: &str = "Codex did not report a readable model catalog";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Effort {
    pub(crate) model: String,
    pub(crate) current: Option<String>,
    pub(crate) options: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Discovery {
    /// Models the CLI marks for its picker; hidden models are excluded.
    pub(crate) models: Vec<Entry>,
    /// Reported effort levels for the selected model only.
    pub(crate) effort: Option<Effort>,
}

#[derive(Deserialize)]
struct Catalog {
    models: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    slug: String,
    display_name: Option<String>,
    visibility: Option<String>,
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<Level>,
}

#[derive(Deserialize)]
struct Level {
    effort: String,
}

pub(crate) async fn discover(
    context: &CodexContext,
    selected_model: Option<&str>,
) -> Result<Discovery, String> {
    let models = tokio::time::timeout(Duration::from_secs(15), catalog(context))
        .await
        .map_err(|_| "Codex model discovery timed out; retry explicitly".to_owned())??;
    let effort = match selected_model.filter(|model| !model.is_empty()) {
        Some(selected) => {
            let model = models
                .iter()
                .find(|model| model.slug == selected)
                .ok_or("The selected model is not offered by the selected Codex CLI")?;
            Some(Effort {
                model: model.slug.clone(),
                current: model.default_reasoning_level.clone(),
                options: model
                    .supported_reasoning_levels
                    .iter()
                    .map(|level| Entry {
                        id: level.effort.clone(),
                        name: level.effort.clone(),
                    })
                    .collect(),
            })
        }
        None => None,
    };
    Ok(Discovery {
        models: models
            .into_iter()
            .map(|model| Entry {
                name: model.display_name.unwrap_or_else(|| model.slug.clone()),
                id: model.slug,
            })
            .collect(),
        effort,
    })
}

async fn catalog(context: &CodexContext) -> Result<Vec<Model>, String> {
    let mut command = tokio::process::Command::from(context.cli_command());
    command
        .args(["debug", "models"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = CheckChild(
        command
            .spawn()
            .map_err(|error| format!("Could not start Codex to list models: {error}"))?,
    );
    let mut output = Vec::new();
    child
        .0
        .stdout
        .take()
        .ok_or(UNREADABLE)?
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| UNREADABLE)?;
    if output.len() as u64 > MAX_OUTPUT_BYTES {
        return Err("Codex model discovery exceeded its safe output limit".into());
    }
    let status = child.0.wait().await.map_err(|_| UNREADABLE)?;
    if !status.success() {
        return Err(UNREADABLE.into());
    }
    let catalog: Catalog = serde_json::from_slice(&output).map_err(|_| UNREADABLE)?;
    Ok(catalog
        .models
        .into_iter()
        .filter(|model| model.visibility.as_deref() == Some("list") && valid(&model.slug))
        .collect())
}

fn valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests;
