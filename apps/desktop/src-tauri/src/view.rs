//! What the UI renders: daemon state flattened into plain data, with the
//! derived values (activity, session status, which attention item belongs to
//! which session) computed here by the shared `workd-core` rules, so the
//! frontend never re-implements them.

use serde::Serialize;
use workd_core::{
    Activity, AgentState, Attention, AttentionKind, Session, SessionKind, SessionStatus, Timestamp,
    Workspace, WorkspaceSource, WorkspaceState,
};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Connecting,
    Connected,
    /// Not reachable right now; `workspaces` are the last known state.
    Unreachable,
    /// The host's workd speaks another protocol version.
    Incompatible,
    /// workd isn't installed where the host entry says.
    NotInstalled,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostView {
    pub name: String,
    /// How it is reached: `ssh dev-01`, `this Mac`.
    pub describe: String,
    pub status: HostStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The host's workd version, once connected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Agent providers workd supports and whether this host can run them.
    pub agents: Vec<workd_core::AgentCapability>,
    pub workspaces: Vec<WorkspaceView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    pub activity: Activity,
    pub state: WorkspaceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_message: Option<String>,
    pub root: String,
    /// `git` | `directory` | `empty`.
    pub source_kind: &'static str,
    /// Branch, directory path, or empty.
    pub source: String,
    pub sessions: Vec<SessionView>,
    pub attention: Vec<AttentionView>,
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: String,
    pub name: String,
    pub kind: SessionKind,
    pub status: SessionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_error: Option<String>,
    /// The newest open attention item for this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attention: Option<AttentionView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentView {
    pub provider: String,
    pub state: AgentState,
    pub since: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionView {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub kind: AttentionKind,
    /// Waiting on the developer (question, approval, review).
    pub needs_you: bool,
    pub summary: String,
    pub created_at: Timestamp,
}

impl From<&Attention> for AttentionView {
    fn from(a: &Attention) -> Self {
        AttentionView {
            id: a.id.to_string(),
            session_id: a.session_id.as_ref().map(|s| s.to_string()),
            kind: a.kind,
            needs_you: a.kind.needs_you(),
            summary: a.summary.clone(),
            created_at: a.created_at,
        }
    }
}

impl From<&Workspace> for WorkspaceView {
    fn from(ws: &Workspace) -> Self {
        let (source_kind, source) = match &ws.source {
            WorkspaceSource::Git(g) => ("git", g.branch.clone()),
            WorkspaceSource::Directory { path } => ("directory", path.clone()),
            WorkspaceSource::Empty => ("empty", String::new()),
        };
        WorkspaceView {
            id: ws.id.to_string(),
            name: ws.name.clone(),
            activity: ws.activity(),
            state: ws.state,
            state_message: ws.state_message.clone(),
            root: ws.root.clone(),
            source_kind,
            source,
            sessions: ws.sessions.iter().map(|s| session_view(ws, s)).collect(),
            attention: ws.attention.iter().map(AttentionView::from).collect(),
            updated_at: ws.updated_at,
        }
    }
}

fn session_view(ws: &Workspace, s: &Session) -> SessionView {
    let attention = ws
        .attention
        .iter()
        .filter(|a| a.session_id.as_ref() == Some(&s.id))
        .max_by_key(|a| a.created_at)
        .map(AttentionView::from);
    SessionView {
        id: s.id.to_string(),
        name: s.name.clone(),
        kind: s.kind,
        status: s.status(),
        agent: s.agent.as_ref().map(|a| AgentView {
            provider: a.provider.clone(),
            state: a.state,
            since: a.state_since,
            last_message: a.last_message.clone(),
        }),
        exit_code: s.current_execution().and_then(|e| e.exit_code),
        launch_error: s.launch_error.clone(),
        attention,
    }
}
