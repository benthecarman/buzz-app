//! Model connection commands. `buzz_desktop::agent_models` owns the tickets
//! and the work; this lends it the agent host and the system browser.
use buzz_agent_controller::AgentEdit;
pub(crate) use buzz_desktop::agent_models::{defaults, Defaults, ModelHost};
use buzz_desktop::agent_models::{AgentContexts, Catalog, Request};
use std::future::Future;
use tauri_plugin_opener::OpenerExt;

impl AgentContexts for crate::agents::AgentHost {
    fn ensure_open(&self) -> impl Future<Output = Result<(), String>> + Send {
        Self::ensure_open(self)
    }
    fn inherited_workspace(&self) -> impl Future<Output = Result<Option<String>, String>> + Send {
        Self::inherited_workspace(self)
    }
    fn disconnect(&self, workspace: &str) -> impl Future<Output = Result<(), String>> + Send {
        Self::disconnect(self, workspace)
    }
    fn model_context(
        &self,
        id: Option<&str>,
        revision: Option<u64>,
        edit: AgentEdit,
    ) -> impl Future<Output = Result<buzz_agent_controller::ModelContext, String>> + Send {
        Self::model_context(self, id, revision, edit)
    }
    fn goose_model_context(
        &self,
        id: Option<&str>,
        revision: Option<u64>,
        edit: AgentEdit,
    ) -> impl Future<Output = Result<buzz_agent_controller::GooseModelContext, String>> + Send {
        Self::goose_model_context(self, id, revision, edit)
    }
    fn pi_model_context(
        &self,
        id: Option<&str>,
        revision: Option<u64>,
        edit: AgentEdit,
    ) -> impl Future<Output = Result<buzz_agent_controller::pi::PiContext, String>> + Send {
        Self::pi_model_context(self, id, revision, edit)
    }
}

#[tauri::command]
pub(crate) fn agent_models_begin(state: tauri::State<'_, ModelHost>) -> Result<u64, String> {
    state.begin()
}
#[tauri::command]
pub(crate) fn agent_models_cancel(
    state: tauri::State<'_, ModelHost>,
    ticket: u64,
) -> Result<(), String> {
    state.cancel(ticket)
}
#[tauri::command]
pub(crate) async fn agent_models_run<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, ModelHost>,
    agents: tauri::State<'_, crate::agents::AgentHost>,
    ticket: u64,
    request: Request,
) -> Result<Catalog, String> {
    let host = state.inner().clone();
    let open_url = move |url: &str| {
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|error| error.to_string())
    };
    host.run_request(ticket, request, agents.inner().clone(), open_url)
        .await
}

#[cfg(test)]
mod tests;
