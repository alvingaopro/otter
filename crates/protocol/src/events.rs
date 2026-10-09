//! Structured events (design §20).
//!
//! Events carry identifiers, names, kinds and exit codes only — never commands,
//! environment variables or other potentially secret data (design §19).

use otter_core::feature::{FeatureStatus, MessageRole};
use otter_core::{
    AgentState, AttentionId, AttentionKind, ExecutionId, FeatureId, SessionId, SessionKind,
    Timestamp, WorkspaceId,
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
    /// A feature changed: its history grew to `history_seq`. What happened
    /// is in the feature's own history (`feature.events`).
    FeatureChanged {
        feature_id: FeatureId,
        history_seq: u64,
        status: FeatureStatus,
    },
    /// A message being written right now (D-051): the text so far, sent as
    /// it grows. **Transient**: not in the log, not replayed, and its `seq`
    /// is the latest logged one (it doesn't advance a cursor). `done`: the
    /// message is complete and in the feature (read it there).
    FeatureStream {
        feature_id: FeatureId,
        /// Identifies this message while it streams.
        stream_id: String,
        role: MessageRole,
        text: String,
        #[serde(default)]
        done: bool,
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
            Event::WorkspaceArchived { .. } => "WorkspaceArchived",
            Event::WorkspaceUnarchived { .. } => "WorkspaceUnarchived",
            Event::WorkspaceBriefChanged { .. } => "WorkspaceBriefChanged",
            Event::EnvironmentPreparing { .. } => "EnvironmentPreparing",
            Event::EnvironmentReady { .. } => "EnvironmentReady",
            Event::EnvironmentFailed { .. } => "EnvironmentFailed",
            Event::SessionCreated { .. } => "SessionCreated",
            Event::SessionStopped { .. } => "SessionStopped",
            Event::SessionDeleted { .. } => "SessionDeleted",
            Event::ExecutionStarted { .. } => "ExecutionStarted",
            Event::ExecutionExited { .. } => "ExecutionExited",
            Event::ExecutionLost { .. } => "ExecutionLost",
            Event::ExecutionFailed { .. } => "ExecutionFailed",
            Event::AgentStateChanged { .. } => "AgentStateChanged",
            Event::AttentionCreated { .. } => "AttentionCreated",
            Event::AttentionResolved { .. } => "AttentionResolved",
            Event::BrowserOpenRequested { .. } => "BrowserOpenRequested",
            Event::FeatureChanged { .. } => "FeatureChanged",
            Event::FeatureStream { .. } => "FeatureStream",
            Event::Unknown => "Unknown",
        }
    }

    pub fn workspace_id(&self) -> Option<&WorkspaceId> {
        match self {
            Event::DaemonStarted { .. }
            | Event::FeatureChanged { .. }
            | Event::FeatureStream { .. }
            | Event::Unknown => None,
            Event::WorkspaceCreated { workspace_id, .. }
            | Event::WorkspaceDeleted { workspace_id, .. }
            | Event::WorkspaceReady { workspace_id }
            | Event::WorkspaceFailed { workspace_id, .. }
            | Event::WorkspaceArchived { workspace_id, .. }
            | Event::WorkspaceUnarchived { workspace_id }
            | Event::WorkspaceBriefChanged { workspace_id }
            | Event::EnvironmentPreparing { workspace_id }
            | Event::EnvironmentReady { workspace_id }
            | Event::EnvironmentFailed { workspace_id, .. }
            | Event::SessionCreated { workspace_id, .. }
            | Event::SessionStopped { workspace_id, .. }
            | Event::SessionDeleted { workspace_id, .. }
            | Event::ExecutionStarted { workspace_id, .. }
            | Event::ExecutionExited { workspace_id, .. }
            | Event::ExecutionLost { workspace_id, .. }
            | Event::ExecutionFailed { workspace_id, .. }
            | Event::AgentStateChanged { workspace_id, .. }
            | Event::AttentionCreated { workspace_id, .. }
            | Event::AttentionResolved { workspace_id, .. } => Some(workspace_id),
            Event::BrowserOpenRequested { workspace_id, .. } => workspace_id.as_ref(),
        }
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Event::SessionCreated { session_id, .. }
            | Event::SessionStopped { session_id, .. }
            | Event::SessionDeleted { session_id, .. }
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
