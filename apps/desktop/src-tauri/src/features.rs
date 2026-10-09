//! Features (D-043): pass-throughs to a host's daemon, which owns them. The
//! UI follows `FeatureChanged` in `host-event` and reloads what changed.

use otter_core::feature::{Feature, FeatureAction, FeatureEventRecord};
use otter_protocol::feature::FeatureCreate;
use tauri::AppHandle;

use crate::hosts::rpc;

/// A host's features, newest first. An `otterd` too old to know features
/// answers with an error, which the UI treats as "none here".
#[tauri::command]
pub async fn features_list(app: AppHandle, host: String) -> Result<Vec<Feature>, String> {
    rpc(&app, &host)
        .await?
        .feature_list()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn feature_create(
    app: AppHandle,
    host: String,
    command_id: String,
    title: String,
    request: String,
    workspace: Option<String>,
) -> Result<Feature, String> {
    rpc(&app, &host)
        .await?
        .feature_create(FeatureCreate {
            command_id,
            title,
            request,
            workspace,
        })
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn feature_send(
    app: AppHandle,
    host: String,
    command_id: String,
    feature: String,
    text: String,
) -> Result<Feature, String> {
    rpc(&app, &host)
        .await?
        .feature_send(&command_id, &feature, &text)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn feature_act(
    app: AppHandle,
    host: String,
    command_id: String,
    feature: String,
    action: FeatureAction,
) -> Result<Feature, String> {
    rpc(&app, &host)
        .await?
        .feature_act(&command_id, &feature, action)
        .await
        .map_err(|e| e.to_string())
}

/// A screenshot (or other file) a feature's checks produced, base64.
#[tauri::command]
pub async fn feature_artifact(
    app: AppHandle,
    host: String,
    feature: String,
    name: String,
) -> Result<String, String> {
    rpc(&app, &host)
        .await?
        .feature_artifact(&feature, &name)
        .await
        .map(|f| f.data)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn feature_events(
    app: AppHandle,
    host: String,
    feature: String,
    after: Option<u64>,
) -> Result<Vec<FeatureEventRecord>, String> {
    rpc(&app, &host)
        .await?
        .feature_events(&feature, after)
        .await
        .map_err(|e| e.to_string())
}
