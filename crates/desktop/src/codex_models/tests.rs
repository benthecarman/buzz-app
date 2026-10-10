use super::*;
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, path::Path};

fn tool(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// A fake `codex` CLI; the adapter is bound but never run by discovery.
fn context(cli_body: &str) -> (tempfile::TempDir, CodexContext) {
    let root = tempfile::tempdir().unwrap();
    let adapter = root.path().join("codex-acp");
    let cli = root.path().join("codex");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    tool(&adapter, "#!/bin/sh\nexit 9\n");
    tool(&cli, cli_body);
    let context = CodexContext::new(&adapter, &cli, &workspace, &BTreeMap::new()).unwrap();
    (root, context)
}

const CATALOG: &str = r#"{"models":[
{"slug":"sol","display_name":"Sol","visibility":"list","default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"ultra"}]},
{"slug":"luna","display_name":"Luna","visibility":"list","default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"low"},{"effort":"max"}]},
{"slug":"hidden","display_name":"Hidden","visibility":"hide","supported_reasoning_levels":[{"effort":"low"}]},
{"slug":"bare","visibility":"list"}]}"#;

fn catalog_cli() -> String {
    format!("#!/bin/sh\n[ \"$1 $2\" = 'debug models' ] || exit 3\ncat <<'EOF'\n{CATALOG}\nEOF\n")
}

fn entry(id: &str, name: &str) -> Entry {
    Entry {
        id: id.into(),
        name: name.into(),
    }
}

#[tokio::test]
async fn lists_only_picker_models_and_selected_model_efforts() {
    let (_root, context) = context(&catalog_cli());
    let result = discover(&context, None).await.unwrap();
    assert_eq!(
        result.models,
        vec![
            entry("sol", "Sol"),
            entry("luna", "Luna"),
            entry("bare", "bare")
        ]
    );
    assert_eq!(result.effort, None);

    let luna = discover(&context, Some("luna"))
        .await
        .unwrap()
        .effort
        .unwrap();
    assert_eq!(luna.model, "luna");
    assert_eq!(luna.current.as_deref(), Some("medium"));
    assert_eq!(luna.options, vec![entry("low", "low"), entry("max", "max")]);

    let bare = discover(&context, Some("bare"))
        .await
        .unwrap()
        .effort
        .unwrap();
    assert_eq!((bare.current, bare.options), (None, Vec::new()));
}

#[tokio::test]
async fn hidden_or_unknown_selected_models_are_rejected() {
    let (_root, context) = context(&catalog_cli());
    for selected in ["hidden", "missing"] {
        assert_eq!(
            discover(&context, Some(selected)).await.unwrap_err(),
            "The selected model is not offered by the selected Codex CLI"
        );
    }
}

#[tokio::test]
async fn malformed_failed_and_oversized_output_is_unreadable() {
    for (body, error) in [
        ("#!/bin/sh\necho '{\"models\":'\n", UNREADABLE),
        ("#!/bin/sh\necho '{}'\n", UNREADABLE),
        ("#!/bin/sh\necho '{\"models\":[]}'\nexit 1\n", UNREADABLE),
        (
            "#!/bin/sh\nhead -c 1048577 /dev/zero\n",
            "Codex model discovery exceeded its safe output limit",
        ),
    ] {
        let (_root, context) = context(body);
        assert_eq!(discover(&context, None).await.unwrap_err(), error);
    }
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("ps")
        .args(["-p", pid, "-o", "stat="])
        .output()
        .is_ok_and(|output| {
            output.status.success() && !output.stdout.is_empty() && !output.stdout.starts_with(b"Z")
        })
}

#[tokio::test]
async fn cancelled_discovery_kills_cli_and_descendant() {
    let marker_root = tempfile::tempdir().unwrap();
    let marker = marker_root.path().join("processes");
    let (_root, context) = context(&format!(
        r#"#!/bin/sh
sleep 60 & helper=$!
printf '%s %s\n' "$$" "$helper" > '{0}.tmp'
mv '{0}.tmp' '{0}'
wait
"#,
        marker.display()
    ));
    let running = tokio::spawn(async move { discover(&context, None).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "CLI did not start");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    let contents = std::fs::read_to_string(marker).unwrap();
    let pids: Vec<_> = contents.split_whitespace().collect();
    assert_eq!(pids.len(), 2);
    while pids.iter().any(|pid| alive(pid)) {
        assert!(
            std::time::Instant::now() < deadline,
            "cancelled discovery left a process"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
