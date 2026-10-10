//! Harnesses setup commands. `buzz_desktop::harness_setup` installs; this
//! resolves app storage and restarts the agents that waited for Pi.
use crate::agents::AgentHost;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::agents::{self, InstallRestart};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use buzz_agent_controller::{Action, ProcessStatus};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use buzz_desktop::harness_setup::Restart;
pub(crate) use buzz_desktop::harness_setup::{waiting_for_pi, HarnessSetup, InstallReport};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use tauri::Manager as _;

#[tauri::command]
pub(crate) async fn pi_install<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, HarnessSetup>,
    agents: tauri::State<'_, AgentHost>,
) -> Result<InstallReport, String> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (app, state, agents);
        Err("Pi installation is supported only on macOS and Linux".into())
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let claim = state.claim()?;
        let app_data = app
            .path()
            .app_data_dir()
            .map_err(|_| "Could not resolve Pi install storage")?;
        let restart = |id: String| {
            let agents = agents.inner().clone();
            async move {
                match agents::start(
                    agents,
                    id.clone(),
                    Action::Restart,
                    false,
                    None,
                    Some(InstallRestart::Pi),
                )
                .await
                {
                    Err(error) if error == agents::NOT_WAITING_FOR_PI => Restart::Skipped,
                    Ok(snapshot)
                        if snapshot.data.agents.iter().any(|agent| {
                            agent.id == id && agent.status == ProcessStatus::Running
                        }) =>
                    {
                        Restart::Running
                    }
                    _ => Restart::Failed,
                }
            }
        };
        buzz_desktop::harness_setup::install_pi(&claim, &app_data, agents.waiting_for_pi(), restart)
            .await
    }
}

#[tauri::command]
pub(crate) async fn claude_install<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, HarnessSetup>,
) -> Result<InstallReport, String> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (app, state);
        Err("Claude Code installation is supported only on macOS and Linux".into())
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let claim = state.claim()?;
        let app_data = app
            .path()
            .app_data_dir()
            .map_err(|_| "Could not resolve Claude Code install storage")?;
        buzz_desktop::harness_setup::install_claude(&claim, &app_data).await
    }
}
