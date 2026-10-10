//! Tauri commands for the native terminal in [`buzz_desktop::terminal`].
//! Each runs on a blocking thread because session teardown can wait.
pub(crate) use buzz_desktop::terminal::Terminals;
use buzz_desktop::terminal::{dimensions, ReadResult, TerminalContext, MAX_PENDING_INPUT};

#[tauri::command]
pub(crate) async fn terminal_create_owner(
    state: tauri::State<'_, Terminals>,
) -> Result<String, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.create_owner())
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
pub(crate) async fn terminal_spawn(
    state: tauri::State<'_, Terminals>,
    owner: String,
    context: TerminalContext,
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.spawn(&owner, context, cols, rows))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
pub(crate) async fn terminal_read(
    state: tauri::State<'_, Terminals>,
    owner: String,
    id: String,
) -> Result<ReadResult, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.read(&owner, &id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
pub(crate) async fn terminal_write(
    state: tauri::State<'_, Terminals>,
    owner: String,
    id: String,
    data: String,
) -> Result<(), String> {
    if data.len() > MAX_PENDING_INPUT {
        return Err("Terminal input exceeds 1 MiB".into());
    }
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.write(&owner, &id, data.as_bytes()))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
pub(crate) async fn terminal_resize(
    state: tauri::State<'_, Terminals>,
    owner: String,
    id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    dimensions(cols, rows)?;
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.resize(&owner, &id, cols, rows))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
pub(crate) async fn terminal_close(
    state: tauri::State<'_, Terminals>,
    owner: String,
    id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.close(&owner, &id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
pub(crate) async fn terminal_close_owner(
    state: tauri::State<'_, Terminals>,
    owner: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.close_owner(&owner))
        .await
        .map_err(|e| e.to_string())?
}
