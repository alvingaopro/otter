//! Domain model shared by the daemon, the CLI and (later) the desktop app.
//!
//! Hierarchy (see docs/design.md §4):
//!
//! ```text
//! Host
//!  └── Workspace          durable logical context (NOT a repo/dir/tmux session)
//!       ├── Brief         why the workspace exists
//!       └── Session       durable logical activity (agent, shell, service, task)
//!            └── Execution  one concrete process run of the session
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AttentionId, ExecutionId, SessionId, WorkspaceId};

pub type Timestamp = DateTime<Utc>;

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// Absolute path on the host where sessions start.
    pub root: String,
    #[serde(default)]
    pub brief: Brief,
    #[serde(default)]
    pub source: WorkspaceSource,
    #[serde(default)]
    pub environment: Environment,
    pub state: WorkspaceState,
    /// Human-readable reason when `state` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_message: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub sessions: Vec<Session>,
    /// Unresolved items that need the developer (design §21).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attention: Vec<Attention>,
}

impl Workspace {
    pub fn session(&self, id_or_name: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|s| s.id.as_str() == id_or_name)
            .or_else(|| self.sessions.iter().find(|s| s.name == id_or_name))
    }

    pub fn session_mut(&mut self, id_or_name: &str) -> Option<&mut Session> {
        let idx = self
            .sessions
            .iter()
            .position(|s| s.id.as_str() == id_or_name)
            .or_else(|| self.sessions.iter().position(|s| s.name == id_or_name))?;
        Some(&mut self.sessions[idx])
    }
}

/// Durable, human-level description of why a workspace exists. Every field is
/// optional. Not to be confused with an agent's own conversation context.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Brief {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
}

impl Brief {
    pub fn is_empty(&self) -> bool {
        *self == Brief::default()
    }
}

/// Where a workspace's files come from. Git is optional (design rule 1).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceSource {
    /// A fresh empty directory under `~/.workd/workspaces/<id>`.
    #[default]
    Empty,
    /// A worktree of a shared backing repository (design §13).
    Git(GitSource),
    /// An existing directory on the host. Attached, not managed: Workd never
    /// deletes it.
    Directory { path: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitSource {
    /// Clone URL (anything `git clone` accepts on the host).
    pub repository: String,
    /// Backing repository under `~/.workd/repos/`.
    pub repo_id: String,
    /// Branch checked out in the worktree. Empty until prepared.
    #[serde(default)]
    pub branch: String,
    /// What the branch was created from (e.g. `origin/main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// The branch requested at creation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_branch: Option<String>,
}

/// How a workspace's files should be obtained (input to creation).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceSpec {
    #[default]
    Empty,
    Git {
        repository: String,
        /// Existing branch to check out, or name for the new branch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        /// Revision to branch from (default: origin's default branch).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    Directory {
        path: String,
    },
}

/// Lifecycle of the workspace itself (as opposed to the activity of its
/// sessions, which is derived).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    Preparing,
    Ready,
    Failed,
    Archived,
}

impl WorkspaceState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceState::Preparing => "preparing",
            WorkspaceState::Ready => "ready",
            WorkspaceState::Failed => "failed",
            WorkspaceState::Archived => "archived",
        }
    }
}

/// The development environment all managed sessions in a workspace inherit.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Environment {
    pub kind: EnvironmentKind,
    pub status: EnvironmentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    /// The host user's login environment, unmodified.
    #[default]
    None,
    /// The workspace's `.envrc`, evaluated by direnv (often `use flake`).
    Direnv,
}

impl EnvironmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EnvironmentKind::None => "none",
            EnvironmentKind::Direnv => "direnv",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentStatus {
    Preparing,
    #[default]
    Ready,
    Failed,
}

impl EnvironmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EnvironmentStatus::Preparing => "preparing",
            EnvironmentStatus::Ready => "ready",
            EnvironmentStatus::Failed => "failed",
        }
    }
}

// ---------------------------------------------------------------------------
// Session / Execution
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// A coding agent (e.g. Codex).
    Agent,
    /// An interactive terminal: a login shell, or any interactive command.
    Terminal,
    /// A long-running process such as a dev server.
    Service,
    /// A process expected to finish, such as a test run.
    Task,
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Agent => "agent",
            SessionKind::Terminal => "terminal",
            SessionKind::Service => "service",
            SessionKind::Task => "task",
        }
    }
}

impl std::str::FromStr for SessionKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "agent" => Ok(SessionKind::Agent),
            "terminal" | "shell" => Ok(SessionKind::Terminal),
            "service" => Ok(SessionKind::Service),
            "task" => Ok(SessionKind::Task),
            other => Err(format!(
                "unknown session kind `{other}` (expected terminal, service, task or agent)"
            )),
        }
    }
}

/// Definition of a session to create.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSpec {
    /// Defaults to a name derived from the command (or `shell`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: SessionKind,
    /// Shell command line. Required for `service` and `task`; `None` for a
    /// `terminal` means the login shell. Not used by `agent` sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Agent provider for `agent` sessions (default: `codex`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Initial prompt for `agent` sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

impl SessionSpec {
    pub fn shell() -> Self {
        SessionSpec {
            name: None,
            kind: SessionKind::Terminal,
            command: None,
            provider: None,
            prompt: None,
        }
    }

    pub fn agent(provider: &str) -> Self {
        SessionSpec {
            name: None,
            kind: SessionKind::Agent,
            command: None,
            provider: Some(provider.to_owned()),
            prompt: None,
        }
    }
}

/// A durable logical activity inside a workspace. Its identity is independent
/// of any process: restarting a session creates a new [`Execution`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub kind: SessionKind,
    /// Shell command line. `None` means the user's login shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub created_at: Timestamp,
    /// Why the most recent attempt to start the session failed, if it did.
    /// Cleared on the next successful start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_error: Option<String>,
    /// Agent integration state, for `agent` sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentInfo>,
    /// Execution history, oldest first. The last entry is the current one.
    #[serde(default)]
    pub executions: Vec<Execution>,
}

impl Session {
    pub fn current_execution(&self) -> Option<&Execution> {
        self.executions.last()
    }

    pub fn current_execution_mut(&mut self) -> Option<&mut Execution> {
        self.executions.last_mut()
    }

    pub fn status(&self) -> SessionStatus {
        if self.launch_error.is_some() {
            return SessionStatus::Failed;
        }
        match self.current_execution() {
            None => SessionStatus::Pending,
            Some(e) => match e.state {
                ExecutionState::Running => SessionStatus::Running,
                ExecutionState::Exited if e.exit_code == Some(0) => SessionStatus::Completed,
                ExecutionState::Exited => SessionStatus::Failed,
                ExecutionState::Stopped => SessionStatus::Stopped,
                ExecutionState::Lost => SessionStatus::Lost,
            },
        }
    }
}

/// Summary status of a session, derived from its current execution.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// No execution yet.
    Pending,
    Running,
    /// Process exited with status 0.
    Completed,
    /// Process exited with a non-zero status.
    Failed,
    /// Stopped by the user.
    Stopped,
    /// The backing process disappeared without Workd observing its exit.
    Lost,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Pending => "pending",
            SessionStatus::Running => "running",
            SessionStatus::Completed => "completed",
            SessionStatus::Failed => "failed",
            SessionStatus::Stopped => "stopped",
            SessionStatus::Lost => "lost",
        }
    }
}

/// One concrete run of a session on an execution backend.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Execution {
    pub id: ExecutionId,
    /// Execution backend that owns the process (V1: `tmux`).
    pub backend: String,
    /// Backend-specific handle (for tmux: the tmux session name).
    pub backend_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub state: ExecutionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
}

impl Execution {
    pub fn is_running(&self) -> bool {
        self.state == ExecutionState::Running
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Running,
    /// The process exited; see `exit_code`.
    Exited,
    /// Terminated by Workd at the user's request.
    Stopped,
    /// The backend no longer knows about the process.
    Lost,
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// What Workd knows about an agent session (design §9, §22, §23). The agent's
/// own context stays with the provider; Workd keeps only what it needs to
/// resume and observe it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AgentInfo {
    /// Agent provider id, e.g. `codex`.
    pub provider: String,
    /// The provider's identifier for the agent's own session/conversation,
    /// used to resume it. Opaque outside the provider.
    #[serde(default, alias = "resume_id", skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    /// Whatever the provider needs to keep observing the agent (e.g. where it
    /// has read up to). Opaque outside the provider.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub provider_state: serde_json::Value,
    pub state: AgentState,
    pub state_since: Timestamp,
    /// Excerpt of the agent's latest message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    /// Initial prompt, used on the first start only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Launched; no conversation observed yet.
    Starting,
    /// Has a conversation but isn't doing anything (e.g. turn aborted).
    Idle,
    /// Working on a turn.
    Working,
    /// Mid-turn and quiet: probably waiting for an approval or an answer.
    /// (Heuristic; see docs/decisions.md.)
    Blocked,
    /// Finished a turn; waiting for the developer's next message.
    WaitingForInput,
    /// The agent process is gone.
    Exited,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Starting => "starting",
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::WaitingForInput => "waiting_for_input",
            AgentState::Exited => "exited",
        }
    }
}

// ---------------------------------------------------------------------------
// Attention
// ---------------------------------------------------------------------------

/// Something that needs the developer (design §21).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Attention {
    pub id: AttentionId,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub kind: AttentionKind,
    pub summary: String,
    pub created_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// The agent asked something.
    Question,
    /// The agent is waiting for permission.
    Approval,
    /// Something broke.
    Failure,
    /// Work is ready to look at (e.g. an agent finished a turn).
    Review,
    /// Something finished successfully (e.g. a task).
    Completion,
}

impl AttentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AttentionKind::Question => "question",
            AttentionKind::Approval => "approval",
            AttentionKind::Failure => "failure",
            AttentionKind::Review => "review",
            AttentionKind::Completion => "completion",
        }
    }

    /// Whether the developer is blocking progress (as opposed to merely
    /// being informed).
    pub fn needs_you(self) -> bool {
        matches!(
            self,
            AttentionKind::Question | AttentionKind::Approval | AttentionKind::Review
        )
    }
}

/// Where a workspace stands from the developer's point of view (design §21,
/// §26).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    NeedsYou,
    Failed,
    Working,
    Completed,
    Idle,
    Preparing,
}

impl Activity {
    pub fn label(self) -> &'static str {
        match self {
            Activity::NeedsYou => "NEEDS YOU",
            Activity::Failed => "FAILED",
            Activity::Working => "WORKING",
            Activity::Completed => "COMPLETED",
            Activity::Idle => "IDLE",
            Activity::Preparing => "PREPARING",
        }
    }
}

impl Workspace {
    pub fn activity(&self) -> Activity {
        if self.attention.iter().any(|a| a.kind.needs_you()) {
            return Activity::NeedsYou;
        }
        if self.state == WorkspaceState::Failed
            || self
                .attention
                .iter()
                .any(|a| a.kind == AttentionKind::Failure)
        {
            return Activity::Failed;
        }
        if self.state == WorkspaceState::Preparing {
            return Activity::Preparing;
        }
        let working = self.sessions.iter().any(|s| match (&s.agent, s.kind) {
            (Some(a), _) => {
                matches!(a.state, AgentState::Working | AgentState::Starting)
                    && s.status() == SessionStatus::Running
            }
            // A shell sitting at a prompt isn't "work".
            (None, SessionKind::Terminal) => false,
            (None, _) => s.status() == SessionStatus::Running,
        });
        if working {
            return Activity::Working;
        }
        if self
            .attention
            .iter()
            .any(|a| a.kind == AttentionKind::Completion)
        {
            return Activity::Completed;
        }
        Activity::Idle
    }
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// What a host daemon reports about itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HostStatus {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub workd_version: String,
    pub protocol_version: u32,
    pub pid: u32,
    pub started_at: Timestamp,
    pub workd_home: String,
    pub shell: String,
    /// How the environment for launched processes was obtained.
    pub environment_source: String,
    /// Tools the host has (git, tmux, nix, direnv, …).
    pub capabilities: Vec<Capability>,
    /// Agent providers this workd supports, and whether the host can run them.
    #[serde(default)]
    pub agents: Vec<AgentCapability>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCapability {
    /// Provider id, as used in [`SessionSpec::provider`].
    pub provider: String,
    /// Installed and runnable on this host.
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Whether sessions can resume the agent's previous conversation.
    pub can_resume: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(state: ExecutionState, code: Option<i32>) -> Execution {
        Execution {
            id: ExecutionId::generate(),
            backend: "tmux".into(),
            backend_ref: "x".into(),
            pid: None,
            state,
            exit_code: code,
            started_at: Utc::now(),
            ended_at: None,
        }
    }

    #[test]
    fn session_status_follows_current_execution() {
        let mut s = Session {
            id: SessionId::generate(),
            workspace_id: WorkspaceId::generate(),
            name: "shell".into(),
            kind: SessionKind::Terminal,
            command: None,
            created_at: Utc::now(),
            launch_error: None,
            agent: None,
            executions: vec![],
        };
        assert_eq!(s.status(), SessionStatus::Pending);
        s.executions.push(exec(ExecutionState::Exited, Some(2)));
        assert_eq!(s.status(), SessionStatus::Failed);
        s.executions.push(exec(ExecutionState::Running, None));
        assert_eq!(s.status(), SessionStatus::Running);
        s.executions.push(exec(ExecutionState::Exited, Some(0)));
        assert_eq!(s.status(), SessionStatus::Completed);
        s.launch_error = Some("tmux missing".into());
        assert_eq!(s.status(), SessionStatus::Failed);
    }

    #[test]
    fn agent_info_from_older_state_still_loads() {
        let json = r#"{"provider":"codex","resume_id":"abc","transcript":"/x",
            "transcript_offset":12,"state":"waiting_for_input",
            "state_since":"2026-10-07T00:00:00Z"}"#;
        let info: AgentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.provider_session_id.as_deref(), Some("abc"));
        assert!(info.provider_state.is_null());
        assert_eq!(info.state, AgentState::WaitingForInput);
    }

    #[test]
    fn workspace_round_trips_through_json() {
        let ws = Workspace {
            id: WorkspaceId::generate(),
            name: "scratch".into(),
            root: "/home/u/.workd/workspaces/ws_x".into(),
            brief: Brief {
                goal: Some("try things".into()),
                ..Default::default()
            },
            source: WorkspaceSource::Empty,
            environment: Environment::default(),
            state: WorkspaceState::Ready,
            state_message: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            sessions: vec![],
            attention: vec![],
        };
        let json = serde_json::to_string(&ws).unwrap();
        assert!(json.contains(r#""source":{"type":"empty"}"#), "{json}");
        let back: Workspace = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ws);
    }
}
