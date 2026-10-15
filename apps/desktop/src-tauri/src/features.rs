//! Features (D-043): pass-throughs to a host's daemon, which owns them. The
//! UI follows `FeatureChanged` in `host-event` and reloads what changed.

use otter_core::Activity;
use otter_core::feature::{Feature, FeatureAction, FeatureEventRecord, Need};
use otter_protocol::feature::FeatureCreate;
use serde::Serialize;
use tauri::AppHandle;

use crate::hosts::rpc;

/// A feature as the UI gets it: with what it needs from the developer and
/// where it stands computed by `otter-core` (D-071), as `view.rs` does for
/// workspaces, so the frontend never re-implements them.
#[derive(Serialize)]
pub struct FeatureView {
    #[serde(flatten)]
    feature: Feature,
    activity: Activity,
    #[serde(skip_serializing_if = "Option::is_none")]
    needs: Option<Need>,
}

impl From<Feature> for FeatureView {
    fn from(feature: Feature) -> Self {
        FeatureView {
            activity: feature.activity(),
            needs: feature.needs(),
            feature,
        }
    }
}

/// A host's features, newest first. An `otterd` too old to know features
/// answers with an error, which the UI treats as "none here".
#[tauri::command]
pub async fn features_list(app: AppHandle, host: String) -> Result<Vec<FeatureView>, String> {
    rpc(&app, &host)
        .await?
        .feature_list()
        .await
        .map(|l| l.into_iter().map(FeatureView::from).collect())
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
    answer_only: Option<bool>,
) -> Result<FeatureView, String> {
    rpc(&app, &host)
        .await?
        .feature_create(FeatureCreate {
            command_id,
            title,
            request,
            workspace,
            answer_only: answer_only.unwrap_or(false),
        })
        .await
        .map(FeatureView::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn feature_send(
    app: AppHandle,
    host: String,
    command_id: String,
    feature: String,
    text: String,
) -> Result<FeatureView, String> {
    rpc(&app, &host)
        .await?
        .feature_send(&command_id, &feature, &text)
        .await
        .map(FeatureView::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn feature_act(
    app: AppHandle,
    host: String,
    command_id: String,
    feature: String,
    action: FeatureAction,
) -> Result<FeatureView, String> {
    rpc(&app, &host)
        .await?
        .feature_act(&command_id, &feature, action)
        .await
        .map(FeatureView::from)
        .map_err(|e| e.to_string())
}

/// Delete a feature that isn't running; its workspace stays.
#[tauri::command]
pub async fn feature_delete(app: AppHandle, host: String, feature: String) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .feature_delete(&feature)
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

/// The coding agent's conversations for a feature, newest first (D-055).
#[tauri::command]
pub async fn conversations(
    app: AppHandle,
    host: String,
    feature: String,
) -> Result<Vec<otter_protocol::conversation::ConversationView>, String> {
    rpc(&app, &host)
        .await?
        .conversation_list(Some(&feature))
        .await
        .map_err(|e| e.to_string())
}

/// What the host's coding runtime can do.
#[tauri::command]
pub async fn runtime_capabilities(
    app: AppHandle,
    host: String,
) -> Result<otter_protocol::conversation::RuntimeCapabilities, String> {
    rpc(&app, &host)
        .await?
        .runtime_capabilities()
        .await
        .map_err(|e| e.to_string())
}

/// Otter's conversation on a host (D-093). An `otterd` too old for it
/// answers with an error, which the view says plainly.
#[tauri::command]
pub async fn otter_get(
    app: AppHandle,
    host: String,
) -> Result<otter_core::feature::OtterChat, String> {
    rpc(&app, &host)
        .await?
        .otter_get()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn otter_send(
    app: AppHandle,
    host: String,
    command_id: String,
    text: String,
) -> Result<otter_core::feature::OtterChat, String> {
    rpc(&app, &host)
        .await?
        .otter_send(&command_id, &text)
        .await
        .map_err(|e| e.to_string())
}
