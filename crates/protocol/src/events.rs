//! Structured events (design §20).
//!
//! Events carry identifiers, names, kinds and exit codes only — never commands,
//! environment variables or other potentially secret data (design §19).

use otter_core::task::{MessageRole, TaskStatus};
use otter_core::{
    AgentState, AttentionId, AttentionKind, ExecutionId, SessionId, SessionKind, TaskId, Timestamp,
    WorkspaceId,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EventRecord {
    /// Per-host cursor: strictly increasing, not necessarily contiguous. See
    /// `docs/protocol.md` for replay semantics.
    pub seq: u64,
    pub ts: Timestamp,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Event {
    DaemonStarted {
        version: String,
    },
    WorkspaceCreated {
        workspace_id: WorkspaceId,
        name: String,
    },
    WorkspaceDeleted {
        workspace_id: WorkspaceId,
        name: String,
    },
    /// Files and environment are in place; sessions are starting.
    WorkspaceReady {
        workspace_id: WorkspaceId,
    },
    WorkspaceFailed {
        workspace_id: WorkspaceId,
        message: String,
    },
    /// Preparation started again, or moved on a step. What it is doing isn't
    /// here: read the workspace.
    WorkspacePreparing {
        workspace_id: WorkspaceId,
    },
    /// Sessions stopped and attention resolved; files kept.
    WorkspaceArchived {
        workspace_id: WorkspaceId,
        name: String,
    },
    /// Back from the archive; preparing again.
    WorkspaceUnarchived {
        workspace_id: WorkspaceId,
    },
    /// The brief was edited. What it says isn't here: read the workspace.
    WorkspaceBriefChanged {
        workspace_id: WorkspaceId,
    },
    WorkspaceRenamed {
        workspace_id: WorkspaceId,
        name: String,
    },
    EnvironmentPreparing {
        workspace_id: WorkspaceId,
    },
    EnvironmentReady {
        workspace_id: WorkspaceId,
    },
    EnvironmentFailed {
        workspace_id: WorkspaceId,
        message: String,
    },
    SessionCreated {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        name: String,
        kind: SessionKind,
    },
    SessionStopped {
        workspace_id: WorkspaceId,
        session_id: SessionId,
    },
    SessionDeleted {
        workspace_id: WorkspaceId,
        session_id: SessionId,
    },
    SessionRenamed {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        name: String,
    },
    SessionMoved {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        to: usize,
    },
    ExecutionStarted {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        execution_id: ExecutionId,
    },
    ExecutionExited {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        execution_id: ExecutionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// The backend lost track of the process (e.g. its tmux session vanished).
    ExecutionLost {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        execution_id: ExecutionId,
    },
    /// A session could not be launched.
    ExecutionFailed {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        message: String,
    },
    AgentStateChanged {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        state: AgentState,
    },
    AttentionCreated {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        attention_id: AttentionId,
        kind: AttentionKind,
    },
    AttentionResolved {
        workspace_id: WorkspaceId,
        attention_id: AttentionId,
    },
    /// A tool on the host wants a sign-in page opened on the Mac (browser
    /// login). The URL isn't here: an app takes it with `browser.take`.
    BrowserOpenRequested {
        request_id: String,
        /// The sign-in host, e.g. `oidc.us-east-1.amazonaws.com`.
        provider: String,
        /// A loopback callback port the tool is waiting on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callback_port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<WorkspaceId>,
    },
    /// Like [`Event::BrowserOpenRequested`], for a page on a site that isn't
    /// a provider built in (D-121): the Mac opens it if the developer allowed
    /// that site there, or asks. An app from before ignores it.
    BrowserOpenAsked {
        request_id: String,
        /// The page's host, e.g. `login.example.com`.
        site: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callback_port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<WorkspaceId>,
    },
    /// A task changed: its history grew to `history_seq`. What happened
    /// is in the task's own history (`task.events`).
    #[serde(alias = "FeatureChanged")]
    TaskChanged {
        #[serde(alias = "feature_id")]
        task_id: TaskId,
        history_seq: u64,
        status: TaskStatus,
    },
    /// Otter's conversation on this host changed (D-093): a message, or a
    /// tool call in its history.
    OtterChanged {
        history_seq: u64,
    },
    /// Otter's reply on this host as it is written. **Transient**, like
    /// [`Event::TaskStream`]; `done`: the message is in the conversation.
    OtterStream {
        stream_id: String,
        text: String,
        #[serde(default)]
        done: bool,
    },
    /// The host's triggers changed (D-185): one was made, changed, removed
    /// or fired. Read them with `trigger.list`.
    TriggersChanged {},
    /// A task was deleted (`task.delete`): it is gone, history and all.
    #[serde(alias = "FeatureDeleted")]
    TaskDeleted {
        #[serde(alias = "feature_id")]
        task_id: TaskId,
    },
    /// A runtime conversation changed (D-055): read it with
    /// `conversation.get`. Ids only, never content.
    ConversationChanged {
        conversation_id: otter_core::ConversationId,
        revision: u64,
        #[serde(default, alias = "feature_id", skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
    },
    /// A message being written right now (D-051): the text so far, sent as
    /// it grows. **Transient**: not in the log, not replayed, and its `seq`
    /// is the latest logged one (it doesn't advance a cursor). `done`: the
    /// message is complete and in the task (read it there).
    #[serde(alias = "FeatureStream")]
    TaskStream {
        #[serde(alias = "feature_id")]
        task_id: TaskId,
        /// Identifies this message while it streams.
        stream_id: String,
        role: MessageRole,
        text: String,
        #[serde(default)]
        done: bool,
    },
    /// What a task's coding agent just did (a tool it used), one line
    /// (D-069). **Transient**, like [`Event::TaskStream`]: the run's
    /// latest lines are in the task while it runs, and kept when it ends.
    #[serde(alias = "FeatureActivity")]
    TaskActivity {
        #[serde(alias = "feature_id")]
        task_id: TaskId,
        run_id: otter_core::RunId,
        line: String,
        /// When it was done (absent from daemons before D-141).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<otter_core::Timestamp>,
    },
    /// A kind this build doesn't know (from a newer daemon, or an older log
    /// line). New kinds are a compatible protocol change; clients ignore them.
    #[serde(other)]
    Unknown,
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::DaemonStarted { .. } => "DaemonStarted",
            Event::WorkspaceCreated { .. } => "WorkspaceCreated",
            Event::WorkspaceDeleted { .. } => "WorkspaceDeleted",
            Event::WorkspaceReady { .. } => "WorkspaceReady",
            Event::WorkspaceFailed { .. } => "WorkspaceFailed",
            Event::WorkspacePreparing { .. } => "WorkspacePreparing",
            Event::WorkspaceArchived { .. } => "WorkspaceArchived",
            Event::WorkspaceUnarchived { .. } => "WorkspaceUnarchived",
            Event::WorkspaceBriefChanged { .. } => "WorkspaceBriefChanged",
            Event::WorkspaceRenamed { .. } => "WorkspaceRenamed",
            Event::EnvironmentPreparing { .. } => "EnvironmentPreparing",
            Event::EnvironmentReady { .. } => "EnvironmentReady",
            Event::EnvironmentFailed { .. } => "EnvironmentFailed",
            Event::SessionCreated { .. } => "SessionCreated",
            Event::SessionStopped { .. } => "SessionStopped",
            Event::SessionDeleted { .. } => "SessionDeleted",
            Event::SessionRenamed { .. } => "SessionRenamed",
            Event::SessionMoved { .. } => "SessionMoved",
            Event::ExecutionStarted { .. } => "ExecutionStarted",
            Event::ExecutionExited { .. } => "ExecutionExited",
            Event::ExecutionLost { .. } => "ExecutionLost",
            Event::ExecutionFailed { .. } => "ExecutionFailed",
            Event::AgentStateChanged { .. } => "AgentStateChanged",
            Event::AttentionCreated { .. } => "AttentionCreated",
            Event::AttentionResolved { .. } => "AttentionResolved",
            Event::BrowserOpenRequested { .. } => "BrowserOpenRequested",
            Event::BrowserOpenAsked { .. } => "BrowserOpenAsked",
            Event::TaskChanged { .. } => "TaskChanged",
            Event::OtterChanged { .. } => "OtterChanged",
            Event::TriggersChanged { .. } => "TriggersChanged",
            Event::OtterStream { .. } => "OtterStream",
            Event::TaskDeleted { .. } => "TaskDeleted",
            Event::ConversationChanged { .. } => "ConversationChanged",
            Event::TaskStream { .. } => "TaskStream",
            Event::TaskActivity { .. } => "TaskActivity",
            Event::Unknown => "Unknown",
        }
    }

    pub fn workspace_id(&self) -> Option<&WorkspaceId> {
        match self {
            Event::DaemonStarted { .. }
            | Event::TaskChanged { .. }
            | Event::OtterChanged { .. }
            | Event::TriggersChanged { .. }
            | Event::OtterStream { .. }
            | Event::TaskDeleted { .. }
            | Event::ConversationChanged { .. }
            | Event::TaskStream { .. }
            | Event::TaskActivity { .. }
            | Event::Unknown => None,
            Event::WorkspaceCreated { workspace_id, .. }
            | Event::WorkspaceDeleted { workspace_id, .. }
            | Event::WorkspaceReady { workspace_id }
            | Event::WorkspaceFailed { workspace_id, .. }
            | Event::WorkspacePreparing { workspace_id }
            | Event::WorkspaceArchived { workspace_id, .. }
            | Event::WorkspaceUnarchived { workspace_id }
            | Event::WorkspaceBriefChanged { workspace_id }
            | Event::WorkspaceRenamed { workspace_id, .. }
            | Event::EnvironmentPreparing { workspace_id }
            | Event::EnvironmentReady { workspace_id }
            | Event::EnvironmentFailed { workspace_id, .. }
            | Event::SessionCreated { workspace_id, .. }
            | Event::SessionStopped { workspace_id, .. }
            | Event::SessionRenamed { workspace_id, .. }
            | Event::SessionMoved { workspace_id, .. }
            | Event::SessionDeleted { workspace_id, .. }
            | Event::ExecutionStarted { workspace_id, .. }
            | Event::ExecutionExited { workspace_id, .. }
            | Event::ExecutionLost { workspace_id, .. }
            | Event::ExecutionFailed { workspace_id, .. }
            | Event::AgentStateChanged { workspace_id, .. }
            | Event::AttentionCreated { workspace_id, .. }
            | Event::AttentionResolved { workspace_id, .. } => Some(workspace_id),
            Event::BrowserOpenRequested { workspace_id, .. }
            | Event::BrowserOpenAsked { workspace_id, .. } => workspace_id.as_ref(),
        }
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Event::SessionCreated { session_id, .. }
            | Event::SessionStopped { session_id, .. }
            | Event::SessionDeleted { session_id, .. }
            | Event::SessionRenamed { session_id, .. }
            | Event::SessionMoved { session_id, .. }
            | Event::ExecutionStarted { session_id, .. }
            | Event::ExecutionExited { session_id, .. }
            | Event::ExecutionLost { session_id, .. }
            | Event::ExecutionFailed { session_id, .. }
            | Event::AgentStateChanged { session_id, .. } => Some(session_id),
            Event::AttentionCreated { session_id, .. } => session_id.as_ref(),
            _ => None,
        }
    }

    pub fn execution_id(&self) -> Option<&ExecutionId> {
        match self {
            Event::ExecutionStarted { execution_id, .. }
            | Event::ExecutionExited { execution_id, .. }
            | Event::ExecutionLost { execution_id, .. } => Some(execution_id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_flat_json() {
        let rec = EventRecord {
            seq: 3,
            ts: chrono::Utc::now(),
            event: Event::ExecutionExited {
                workspace_id: "ws_a".into(),
                session_id: "ses_b".into(),
                execution_id: "exec_c".into(),
                exit_code: Some(1),
            },
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains(r#""type":"ExecutionExited""#), "{json}");
        assert!(json.contains(r#""seq":3"#), "{json}");
        let back: EventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn unknown_kinds_still_parse() {
        let rec: EventRecord = serde_json::from_str(
            r#"{"seq":9,"ts":"2026-10-07T00:00:00Z","type":"PortForwarded","port":3000}"#,
        )
        .unwrap();
        assert_eq!(rec.seq, 9);
        assert_eq!(rec.event, Event::Unknown);
    }
}
