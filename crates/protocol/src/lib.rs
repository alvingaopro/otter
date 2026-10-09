//! Otter wire protocol.
//!
//! A connection to `otterd` is a byte stream (in V1: the daemon's Unix socket,
//! reached over an SSH channel via `otterd dial`). The protocol has two modes:
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
//! [`ServerMessage::Event`]s, optionally replaying from a cursor.
//!
//! Wire format, cursor semantics and compatibility rules: `docs/protocol.md`.

pub mod events;
pub mod feature;
pub mod frame;
pub mod fs;
pub mod host;
pub mod wire;

use otter_core::Brief;
pub use otter_core::{SessionSpec, SourceSpec};
use serde::{Deserialize, Serialize};

pub use events::{Event, EventRecord};

/// Bumped on incompatible protocol changes only (`docs/protocol.md`).
pub const PROTOCOL_VERSION: u32 = 2;

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
    /// Result: [`otter_core::HostStatus`].
    #[serde(rename = "host.status")]
    HostStatus,
    /// Resource usage now and over the last minutes. Result:
    /// [`host::HostMetrics`].
    #[serde(rename = "host.metrics")]
    HostMetrics,
    /// TCP ports listening on the host. Result: `Vec<`[`host::ListeningPort`]`>`.
    #[serde(rename = "host.ports")]
    HostPorts,
    /// List a directory. Result: [`fs::DirListing`].
    #[serde(rename = "fs.list")]
    FsList(fs::FsPath),
    /// Read part of a file. Result: [`fs::FileChunk`].
    #[serde(rename = "fs.read")]
    FsRead(fs::FsRead),
    /// Write part of a file. Result: `null`.
    #[serde(rename = "fs.write")]
    FsWrite(fs::FsWrite),
    /// Hand an image to a session's clipboard stand-in, for Ctrl+V in Claude
    /// Code and other tools that read images with `wl-paste`. Result: `null`.
    #[serde(rename = "session.paste_image")]
    SessionPasteImage(fs::PasteImage),
    /// From the host's browser stand-ins: open a sign-in page on the Mac (see
    /// `docs/protocol.md`). Result: `null`, or an error saying why not.
    #[serde(rename = "browser.open")]
    BrowserOpen(BrowserOpen),
    /// From an app, after `BrowserOpenRequested`: the page to open, once.
    /// Result: [`BrowserOpening`].
    #[serde(rename = "browser.take")]
    BrowserTake(BrowserTake),
    /// Recorded usage over a range. Result: [`host::HostHistory`].
    #[serde(rename = "host.history")]
    HostHistory(host::HistoryQuery),
    /// Stop the daemon. Managed processes keep running. Result: `null`.
    #[serde(rename = "daemon.shutdown")]
    Shutdown,

    /// Result: [`otter_core::Workspace`].
    #[serde(rename = "workspace.create")]
    WorkspaceCreate(WorkspaceCreate),
    /// Result: `Vec<`[`otter_core::Workspace`]`>`.
    #[serde(rename = "workspace.list")]
    WorkspaceList,
    /// Result: [`otter_core::Workspace`].
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceRef),
    /// Re-run preparation (environment, then start sessions that aren't
    /// running yet), e.g. after fixing a failed `.envrc`. Result:
    /// [`otter_core::Workspace`].
    #[serde(rename = "workspace.prepare")]
    WorkspacePrepare(WorkspaceRef),
    /// Stops all sessions and removes managed resources. Result: `null`.
    #[serde(rename = "workspace.delete")]
    WorkspaceDelete(WorkspaceDelete),
    /// Put a workspace away: stop its sessions and resolve its attention,
    /// keeping its files, branch and sessions. Result:
    /// [`otter_core::Workspace`].
    #[serde(rename = "workspace.archive")]
    WorkspaceArchive(WorkspaceRef),
    /// Bring an archived workspace back: prepare it again (sessions stay
    /// stopped until restarted). Result: [`otter_core::Workspace`].
    #[serde(rename = "workspace.unarchive")]
    WorkspaceUnarchive(WorkspaceRef),
    /// Replace the workspace's brief (why it exists). Result:
    /// [`otter_core::Workspace`].
    #[serde(rename = "workspace.set_brief")]
    WorkspaceSetBrief(WorkspaceSetBrief),

    /// Create and start a session. Result: [`otter_core::Session`].
    #[serde(rename = "session.create")]
    SessionCreate(SessionCreate),
    /// Result: [`otter_core::Session`].
    #[serde(rename = "session.stop")]
    SessionStop(SessionRef),
    /// Start a new execution of the session. Result: [`otter_core::Session`].
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
    /// Result: [`Subscribed`], after which the connection streams events:
    /// first those after `after` (if given), then live ones. Fails with
    /// [`ErrorCode::CursorExpired`] if the cursor can't be served.
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(EventsSubscribe),
    /// Current runtime state plus the event cursor it corresponds to. Result:
    /// [`StateSnapshot`].
    #[serde(rename = "state.snapshot")]
    StateSnapshot,

    /// Every feature on this host, newest first. Result:
    /// `Vec<`[`otter_core::feature::Feature`]`>`.
    #[serde(rename = "feature.list")]
    FeatureList,
    /// Result: [`otter_core::feature::Feature`].
    #[serde(rename = "feature.get")]
    FeatureGet(feature::FeatureRef),
    /// Create a draft feature. Result: [`otter_core::feature::Feature`].
    #[serde(rename = "feature.create")]
    FeatureCreate(feature::FeatureCreate),
    /// Add the developer's message to the conversation. Result:
    /// [`otter_core::feature::Feature`].
    #[serde(rename = "feature.send")]
    FeatureSend(feature::FeatureSend),
    /// Start, pause, resume, cancel, retry, decide, accept or send back.
    /// Result: [`otter_core::feature::Feature`].
    #[serde(rename = "feature.act")]
    FeatureAct(feature::FeatureAct),
    /// A feature's own history after a cursor, oldest first. Result:
    /// `Vec<`[`otter_core::feature::FeatureEventRecord`]`>`.
    #[serde(rename = "feature.events")]
    FeatureEvents(feature::FeatureEvents),
}

impl Request {
    pub fn method(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::HostStatus => "host.status",
            Request::HostMetrics => "host.metrics",
            Request::HostPorts => "host.ports",
            Request::HostHistory(_) => "host.history",
            Request::BrowserOpen(_) => "browser.open",
            Request::BrowserTake(_) => "browser.take",
            Request::FsList(_) => "fs.list",
            Request::FsRead(_) => "fs.read",
            Request::FsWrite(_) => "fs.write",
            Request::SessionPasteImage(_) => "session.paste_image",
            Request::Shutdown => "daemon.shutdown",
            Request::WorkspaceCreate(_) => "workspace.create",
            Request::WorkspaceList => "workspace.list",
            Request::WorkspaceGet(_) => "workspace.get",
            Request::WorkspacePrepare(_) => "workspace.prepare",
            Request::WorkspaceDelete(_) => "workspace.delete",
            Request::WorkspaceArchive(_) => "workspace.archive",
            Request::WorkspaceUnarchive(_) => "workspace.unarchive",
            Request::WorkspaceSetBrief(_) => "workspace.set_brief",
            Request::SessionCreate(_) => "session.create",
            Request::SessionStop(_) => "session.stop",
            Request::SessionRestart(_) => "session.restart",
            Request::SessionDelete(_) => "session.delete",
            Request::SessionRead(_) => "session.read",
            Request::SessionWrite(_) => "session.write",
            Request::SessionAttach(_) => "session.attach",
            Request::AttentionResolve(_) => "attention.resolve",
            Request::EventsList(_) => "events.list",
            Request::EventsSubscribe(_) => "events.subscribe",
            Request::StateSnapshot => "state.snapshot",
            Request::FeatureList => "feature.list",
            Request::FeatureGet(_) => "feature.get",
            Request::FeatureCreate(_) => "feature.create",
            Request::FeatureSend(_) => "feature.send",
            Request::FeatureAct(_) => "feature.act",
            Request::FeatureEvents(_) => "feature.events",
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

/// The whole new brief: fields left out are cleared. Clients edit by
/// read-modify-write of the workspace's current brief.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSetBrief {
    pub workspace: String,
    pub brief: Brief,
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
    pub session_id: otter_core::SessionId,
    pub execution_id: otter_core::ExecutionId,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserOpen {
    pub url: String,
    /// The session the tool runs in (its `OTTER_SESSION_ID`), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserTake {
    pub request_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserOpening {
    pub url: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventsSubscribe {
    /// Replay events with `seq` greater than this before streaming live ones.
    /// Without it, only events after [`Subscribed::seq`] are streamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// The log `after` came from ([`StateSnapshot::log_id`] or
    /// [`Subscribed::log_id`]). A different log fails with
    /// [`ErrorCode::CursorExpired`] even if it has grown past `after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_id: Option<String>,
    /// This subscriber opens sign-in pages (`BrowserOpenRequested`): the
    /// Otter app. Browser login is refused while there is none.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub browser: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subscribed {
    /// The latest event when the subscription started. Live events follow it.
    pub seq: u64,
    /// The host's event log; send it back with the next cursor. Absent from
    /// daemons before D-037.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StateSnapshot {
    /// Subscribe with `after = seq` to follow on from this snapshot. The state
    /// reflects every event up to `seq` and may already reflect later ones,
    /// so applying an event must be idempotent (e.g. refetch).
    pub seq: u64,
    /// The event log `seq` belongs to; subscribe with it. Absent from daemons
    /// before D-037.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_id: Option<String>,
    pub workspaces: Vec<otter_core::Workspace>,
}

impl StateSnapshot {
    /// Where to follow on from this snapshot.
    pub fn cursor(&self) -> Cursor {
        Cursor {
            seq: self.seq,
            log_id: self.log_id.clone(),
        }
    }
}

/// A position in a host's event log: the last event seen and the log it came
/// from (`None` from daemons that don't name their log).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub seq: u64,
    pub log_id: Option<String>,
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
    /// `events.subscribe` cursor is older than the retained log, newer than
    /// the latest event, or from another log (the log was reset). Reload a
    /// snapshot.
    CursorExpired,
    /// Not available yet; retry shortly (e.g. metrics right after start).
    Unavailable,
    /// A code this build doesn't know.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_params_are_optional() {
        let msg: ClientMessage =
            serde_json::from_str(r#"{"id":1,"method":"events.subscribe","params":{}}"#).unwrap();
        assert_eq!(
            msg.request,
            Request::EventsSubscribe(EventsSubscribe::default())
        );
        let msg = ClientMessage {
            id: 2,
            request: Request::EventsSubscribe(EventsSubscribe {
                after: Some(41),
                log_id: None,
                browser: false,
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"id":2,"method":"events.subscribe","params":{"after":41}}"#
        );
    }

    #[test]
    fn log_ids_are_optional_both_ways() {
        // A daemon before D-037 sends neither; a client before it sends none.
        let snap: StateSnapshot = serde_json::from_str(r#"{"seq":5,"workspaces":[]}"#).unwrap();
        assert_eq!(
            snap.cursor(),
            Cursor {
                seq: 5,
                log_id: None
            }
        );
        let sub: Subscribed = serde_json::from_str(r#"{"seq":5}"#).unwrap();
        assert_eq!(sub.log_id, None);
        let msg: ClientMessage =
            serde_json::from_str(r#"{"id":1,"method":"events.subscribe","params":{"after":5}}"#)
                .unwrap();
        let Request::EventsSubscribe(p) = msg.request else {
            panic!()
        };
        assert_eq!((p.after, p.log_id), (Some(5), None));

        // A newer daemon's fields round-trip; older peers ignore them.
        let snap = StateSnapshot {
            seq: 7,
            log_id: Some("log_abc".into()),
            workspaces: vec![],
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains(r#""log_id":"log_abc""#), "{json}");
        #[derive(Deserialize)]
        struct OldSnapshot {
            seq: u64,
        }
        assert_eq!(serde_json::from_str::<OldSnapshot>(&json).unwrap().seq, 7);
    }

    #[test]
    fn unknown_error_codes_parse() {
        let e: RpcError =
            serde_json::from_str(r#"{"code":"rate_limited","message":"slow down"}"#).unwrap();
        assert_eq!(e.code, ErrorCode::Unknown);
    }

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
                    kind: otter_core::SessionKind::Service,
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
