//! Workd wire protocol.
//!
//! A connection to `workd` is a byte stream (in V1: the daemon's Unix socket,
//! reached over an SSH channel via `workd dial`). The protocol has two modes:
//!
//! 1. **RPC mode** (initial): newline-delimited JSON. The server first sends a
//!    [`ServerMessage::Hello`]. The client then sends [`ClientMessage`]s; the
//!    server answers each with a [`ServerMessage::Response`] carrying the same
//!    `id`. Requests on one connection are processed in order.
//! 2. **Attach mode**: after a successful `session.attach` response, the
//!    connection switches to binary [`frame`]s carrying terminal I/O. The
//!    connection is dedicated to that attach until it ends.
//!
//! `events.subscribe` similarly dedicates a connection to a stream of
//! [`ServerMessage::Event`]s.

pub mod events;
pub mod frame;
pub mod wire;

use serde::{Deserialize, Serialize};
use workd_core::Brief;
pub use workd_core::{SessionSpec, SourceSpec};

pub use events::{Event, EventRecord};

/// Bumped on incompatible protocol changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request with its correlation id.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClientMessage {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    /// Liveness check. Result: `null`.
    #[serde(rename = "ping")]
    Ping,
    /// Result: [`workd_core::HostStatus`].
    #[serde(rename = "host.status")]
    HostStatus,
    /// Stop the daemon. Managed processes keep running. Result: `null`.
    #[serde(rename = "daemon.shutdown")]
    Shutdown,

    /// Result: [`workd_core::Workspace`].
    #[serde(rename = "workspace.create")]
    WorkspaceCreate(WorkspaceCreate),
    /// Result: `Vec<`[`workd_core::Workspace`]`>`.
    #[serde(rename = "workspace.list")]
    WorkspaceList,
    /// Result: [`workd_core::Workspace`].
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceRef),
    /// Re-run preparation (environment, then start sessions that aren't
    /// running yet), e.g. after fixing a failed `.envrc`. Result:
    /// [`workd_core::Workspace`].
    #[serde(rename = "workspace.prepare")]
    WorkspacePrepare(WorkspaceRef),
    /// Stops all sessions and removes managed resources. Result: `null`.
    #[serde(rename = "workspace.delete")]
    WorkspaceDelete(WorkspaceDelete),

    /// Create and start a session. Result: [`workd_core::Session`].
    #[serde(rename = "session.create")]
    SessionCreate(SessionCreate),
    /// Result: [`workd_core::Session`].
    #[serde(rename = "session.stop")]
    SessionStop(SessionRef),
    /// Start a new execution of the session. Result: [`workd_core::Session`].
    #[serde(rename = "session.restart")]
    SessionRestart(SessionRef),
    /// Stop the session and remove it from the workspace. Result: `null`.
    #[serde(rename = "session.delete")]
    SessionDelete(SessionRef),
    /// Read the session's screen + scrollback. Result: [`SessionOutput`].
    #[serde(rename = "session.read")]
    SessionRead(SessionRead),
    /// Send input to the session without attaching. Result: `null`.
    #[serde(rename = "session.write")]
    SessionWrite(SessionWrite),
    /// Attach an interactive terminal. Result: [`AttachReady`], after which the
    /// connection switches to attach mode.
    #[serde(rename = "session.attach")]
    SessionAttach(SessionAttach),

    /// Mark attention items as handled. Result: number of items resolved.
    #[serde(rename = "attention.resolve")]
    AttentionResolve(AttentionResolve),

    /// Result: `Vec<`[`EventRecord`]`>`, oldest first.
    #[serde(rename = "events.list")]
    EventsList(EventsList),
    /// Result: `null`, after which the connection streams events.
    #[serde(rename = "events.subscribe")]
    EventsSubscribe,
}

impl Request {
    pub fn method(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::HostStatus => "host.status",
            Request::Shutdown => "daemon.shutdown",
            Request::WorkspaceCreate(_) => "workspace.create",
            Request::WorkspaceList => "workspace.list",
            Request::WorkspaceGet(_) => "workspace.get",
            Request::WorkspacePrepare(_) => "workspace.prepare",
            Request::WorkspaceDelete(_) => "workspace.delete",
            Request::SessionCreate(_) => "session.create",
            Request::SessionStop(_) => "session.stop",
            Request::SessionRestart(_) => "session.restart",
            Request::SessionDelete(_) => "session.delete",
            Request::SessionRead(_) => "session.read",
            Request::SessionWrite(_) => "session.write",
            Request::SessionAttach(_) => "session.attach",
            Request::AttentionResolve(_) => "attention.resolve",
            Request::EventsList(_) => "events.list",
            Request::EventsSubscribe => "events.subscribe",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceCreate {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<Brief>,
    /// Where the files come from (default: an empty directory). Git-backed
    /// workspaces are returned in the `preparing` state and become `ready`
    /// (or `failed`) asynchronously.
    #[serde(default)]
    pub source: SourceSpec,
    /// Sessions to start once the workspace is ready. `None` means the
    /// default set (a login shell); `Some(vec![])` means none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionSpec>>,
}

/// Identifies a workspace by id or name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceRef {
    pub workspace: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceDelete {
    pub workspace: String,
    /// Delete even if a Git worktree has uncommitted changes.
    #[serde(default)]
    pub force: bool,
}

/// Identifies a session by workspace (id or name) and session (id or name).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRef {
    pub workspace: String,
    pub session: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionCreate {
    pub workspace: String,
    #[serde(flatten)]
    pub spec: SessionSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRead {
    #[serde(flatten)]
    pub session: SessionRef,
    /// Number of scrollback lines above the visible screen to include.
    /// `None` means the whole history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionOutput {
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionWrite {
    #[serde(flatten)]
    pub session: SessionRef,
    /// Literal text to type.
    pub data: String,
    /// Press Enter after `data`.
    #[serde(default)]
    pub enter: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAttach {
    #[serde(flatten)]
    pub session: SessionRef,
    pub cols: u16,
    pub rows: u16,
    /// The client terminal's `TERM`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachReady {
    pub session_id: workd_core::SessionId,
    pub execution_id: workd_core::ExecutionId,
}

/// Resolves the workspace's attention items, narrowed to one session or one
/// item when given.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttentionResolve {
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventsList {
    /// Maximum number of most recent events to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        protocol: u32,
        version: String,
    },
    Response {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    Event {
        event: EventRecord,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, message)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RpcError {}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidArgument,
    NotFound,
    AlreadyExists,
    Conflict,
    Unsupported,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_request_encodes_without_params() {
        let msg = ClientMessage {
            id: 7,
            request: Request::Ping,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"id":7,"method":"ping"}"#);
        let back: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn request_with_params_round_trips() {
        let msg = ClientMessage {
            id: 1,
            request: Request::SessionCreate(SessionCreate {
                workspace: "scratch".into(),
                spec: SessionSpec {
                    name: Some("dev".into()),
                    kind: workd_core::SessionKind::Service,
                    command: Some("npm run dev".into()),
                    provider: None,
                    prompt: None,
                },
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""method":"session.create""#), "{json}");
        assert!(json.contains(r#""kind":"service""#), "{json}");
        let back: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
        assert_eq!(back.request.method(), "session.create");
    }

    #[test]
    fn unknown_method_is_rejected() {
        let err = serde_json::from_str::<ClientMessage>(r#"{"id":1,"method":"nope"}"#);
        assert!(err.is_err());
    }
}
