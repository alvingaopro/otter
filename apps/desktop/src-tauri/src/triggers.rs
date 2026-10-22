//! Triggers (D-083, D-185): pass-throughs to a host's daemon, for the
//! Automations view. The UI follows `TriggersChanged` in `host-event` and
//! reloads the host's list. An `otterd` too old for one of them answers with
//! an error, which the UI says (or, for the list, treats as "none here").

use otter_core::trigger::Trigger;
use tauri::AppHandle;

use crate::hosts::rpc;

#[tauri::command]
pub async fn triggers_list(app: AppHandle, host: String) -> Result<Vec<Trigger>, String> {
    rpc(&app, &host)
        .await?
        .trigger_list()
        .await
        .map_err(|e| e.to_string())
}

/// Pause (`enabled: false`) or resume a trigger.
#[tauri::command]
pub async fn trigger_update(
    app: AppHandle,
    host: String,
    trigger: String,
    enabled: bool,
) -> Result<Trigger, String> {
    rpc(&app, &host)
        .await?
        .trigger_update(&trigger, Some(enabled))
        .await
        .map_err(|e| e.to_string())
}

/// Fire a trigger now, by hand.
#[tauri::command]
pub async fn trigger_run(app: AppHandle, host: String, trigger: String) -> Result<Trigger, String> {
    rpc(&app, &host)
        .await?
        .trigger_run(&trigger, None)
        .await
        .map_err(|e| e.to_string())
}

/// Remove a trigger; the tasks it started stay.
#[tauri::command]
pub async fn trigger_delete(app: AppHandle, host: String, trigger: String) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .trigger_delete(&trigger)
        .await
        .map_err(|e| e.to_string())
}
