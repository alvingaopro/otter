//! Tasks (D-043): pass-throughs to a host's daemon, which owns them. The
//! UI follows `TaskChanged` in `host-event` and reloads what changed.

use otter_core::Activity;
use otter_core::task::{Need, Task, TaskAction, TaskEventRecord};
use otter_protocol::task::TaskCreate;
use serde::Serialize;
use tauri::AppHandle;

use crate::hosts::rpc;

/// A task as the UI gets it: with what it needs from the developer and
/// where it stands computed by `otter-core` (D-071), as `view.rs` does for
/// workspaces, so the frontend never re-implements them.
#[derive(Serialize)]
pub struct TaskView {
    #[serde(flatten)]
    task: Task,
    activity: Activity,
    #[serde(skip_serializing_if = "Option::is_none")]
    needs: Option<Need>,
}

impl From<Task> for TaskView {
    fn from(task: Task) -> Self {
        TaskView {
            activity: task.activity(),
            needs: task.needs(),
            task,
        }
    }
}

/// A host's tasks, newest first. An `otterd` too old to know tasks
/// answers with an error, which the UI treats as "none here".
#[tauri::command]
pub async fn tasks_list(app: AppHandle, host: String) -> Result<Vec<TaskView>, String> {
    rpc(&app, &host)
        .await?
        .task_list()
        .await
        .map(|l| l.into_iter().map(TaskView::from).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn task_create(
    app: AppHandle,
    host: String,
    command_id: String,
    title: String,
    request: String,
    workspace: Option<String>,
    answer_only: Option<bool>,
) -> Result<TaskView, String> {
    rpc(&app, &host)
        .await?
        .task_create(TaskCreate {
            command_id,
            title,
            request,
            workspace,
            answer_only: answer_only.unwrap_or(false),
        })
        .await
        .map(TaskView::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn task_send(
    app: AppHandle,
    host: String,
    command_id: String,
    task: String,
    text: String,
) -> Result<TaskView, String> {
    rpc(&app, &host)
        .await?
        .task_send(&command_id, &task, &text)
        .await
        .map(TaskView::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn task_act(
    app: AppHandle,
    host: String,
    command_id: String,
    task: String,
    action: TaskAction,
) -> Result<TaskView, String> {
    rpc(&app, &host)
        .await?
        .task_act(&command_id, &task, action)
        .await
        .map(TaskView::from)
        .map_err(|e| e.to_string())
}

/// Delete a task that isn't running; its workspace stays.
#[tauri::command]
pub async fn task_delete(app: AppHandle, host: String, task: String) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .task_delete(&task)
        .await
        .map_err(|e| e.to_string())
}

/// A screenshot (or other file) a task's checks produced, base64.
#[tauri::command]
pub async fn task_artifact(
    app: AppHandle,
    host: String,
    task: String,
    name: String,
) -> Result<String, String> {
    rpc(&app, &host)
        .await?
        .task_artifact(&task, &name)
        .await
        .map(|f| f.data)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn task_events(
    app: AppHandle,
    host: String,
    task: String,
    after: Option<u64>,
) -> Result<Vec<TaskEventRecord>, String> {
    rpc(&app, &host)
        .await?
        .task_events(&task, after)
        .await
        .map_err(|e| e.to_string())
}

/// The coding agent's conversations for a task, newest first (D-055).
#[tauri::command]
pub async fn conversations(
    app: AppHandle,
    host: String,
    task: String,
) -> Result<Vec<otter_protocol::conversation::ConversationView>, String> {
    rpc(&app, &host)
        .await?
        .conversation_list(Some(&task))
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

/// Start the host's coding agent for one short reply (D-124), as set on
/// screen: `backend` and `model`, else the host's settings.
#[tauri::command]
pub async fn runtime_test(
    app: AppHandle,
    host: String,
    backend: Option<String>,
    model: Option<String>,
) -> Result<otter_protocol::conversation::RuntimeTest, String> {
    rpc(&app, &host)
        .await?
        .runtime_test(backend.as_deref(), model.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// Otter's memories (D-113): the host's, and the task's conversation's (or,
/// without a task, Otter's conversation's on the host).
#[tauri::command]
pub async fn memory_get(
    app: AppHandle,
    host: String,
    task: Option<String>,
) -> Result<otter_protocol::otter::OtterMemory, String> {
    rpc(&app, &host)
        .await?
        .memory_get(task.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// The developer's edit of one memory: all of what it says, and what it
/// said when the edit began.
#[tauri::command]
pub async fn memory_set(
    app: AppHandle,
    host: String,
    task: Option<String>,
    memory: otter_protocol::otter::MemoryScope,
    text: String,
    previous: String,
) -> Result<otter_protocol::otter::OtterMemory, String> {
    rpc(&app, &host)
        .await?
        .memory_set(otter_protocol::otter::MemorySet {
            task,
            memory,
            text,
            previous: Some(previous),
        })
        .await
        .map_err(|e| e.to_string())
}
