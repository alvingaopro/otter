//! A host's settings (D-048): the Control Agent's model and API keys. Keys
//! go to the host's otterd over the connection and are never read back.

use otter_protocol::host::{Settings, SettingsUpdate};
use tauri::AppHandle;

use crate::hosts::rpc;

#[tauri::command]
pub async fn settings_get(app: AppHandle, host: String) -> Result<Settings, String> {
    rpc(&app, &host)
        .await?
        .settings_get()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn settings_set(
    app: AppHandle,
    host: String,
    update: SettingsUpdate,
) -> Result<Settings, String> {
    rpc(&app, &host)
        .await?
        .settings_set(update)
        .await
        .map_err(|e| e.to_string())
}
