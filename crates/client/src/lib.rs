//! Client for the Otter daemon.
//!
//! A [`Connection`] runs `otterd dial` — locally or as `ssh <host> otterd dial` —
//! and speaks the protocol over its stdin/stdout. SSH is the only remote
//! transport (design rule 4); the user's SSH config and agent are used as-is.

pub mod config;
pub mod forward;
pub mod install;
pub mod login;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use otter_core::{Brief, HostStatus, Session, Workspace};
use otter_protocol::frame::{Frame, read_frame, write_frame};
use otter_protocol::wire::{read_json, write_json};
use otter_protocol::{
    AttachReady, AttentionResolve, ClientMessage, Cursor, EventRecord, EventsList, EventsSubscribe,
    PROTOCOL_VERSION, Request, RpcError, ServerMessage, SessionAttach, SessionCreate,
    SessionOutput, SessionRead, SessionRef, SessionWrite, StateSnapshot, Subscribed,
    WorkspaceCreate, WorkspaceDelete, WorkspaceRef, WorkspaceSetBrief,
};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// How to reach a host's daemon.
#[derive(Clone, Debug)]
pub enum Transport {
    /// Run `otterd dial` on this machine.
    Local {
        otterd_path: String,
        home: Option<String>,
    },
    /// Run `otterd dial` on a remote host over SSH.
    Ssh {
        destination: String,
        /// Extra arguments for `ssh` (e.g. `-p 2222`, `-J jump`).
        ssh_args: Vec<String>,
        /// Path of `otterd` on the remote host. Interpreted by the remote shell,
        /// so `~` works.
        otterd_path: String,
        home: Option<String>,
        /// Where to keep SSH ControlMaster sockets; `None` disables
        /// multiplexing.
        control_dir: Option<PathBuf>,
    },
}

impl Transport {
    fn command(&self) -> Command {
        match self {
            Transport::Local { otterd_path, home } => {
                let mut cmd = Command::new(otterd_path);
                if let Some(home) = home {
                    cmd.arg("--home").arg(home);
                }
                cmd.arg("dial");
                cmd
            }
            Transport::Ssh {
                otterd_path, home, ..
            } => {
                let mut remote = otterd_path.clone();
                if let Some(home) = home {
                    remote.push_str(" --home ");
                    remote.push_str(&remote_path(home));
                }
                remote.push_str(" dial");
                let mut cmd = self.ssh().expect("ssh transport");
                cmd.arg(remote);
                cmd
            }
        }
    }

    /// `ssh … destination`, ready for a remote command; `None` for local.
    fn ssh(&self) -> Option<Command> {
        self.ssh_with(&[])
    }

    /// Like [`Self::ssh`], with `extra` options before the destination (e.g.
    /// `-O forward -L …` for the shared connection).
    pub(crate) fn ssh_with(&self, extra: &[String]) -> Option<Command> {
        let Transport::Ssh {
            destination,
            ssh_args,
            control_dir,
            ..
        } = self
        else {
            return None;
        };
        let mut cmd = Command::new("ssh");
        cmd.arg("-T");
        if let Some(dir) = control_dir {
            cmd.arg("-o")
                .arg("ControlMaster=auto")
                .arg("-o")
                .arg(format!("ControlPath={}/%C", dir.display()))
                .arg("-o")
                .arg("ControlPersist=600");
        }
        cmd.args(ssh_args);
        // Notice a dead network (laptop asleep, Wi-Fi gone) within ~45s
        // instead of hanging until TCP gives up. After `ssh_args`, so explicit
        // options there take precedence.
        cmd.args([
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        cmd.args(extra);
        cmd.arg(destination);
        Some(cmd)
    }

    /// A POSIX shell on the host reading a script from stdin.
    pub(crate) fn shell(&self) -> Command {
        match self.ssh() {
            Some(mut cmd) => {
                cmd.arg("sh -s");
                cmd
            }
            None => {
                let mut cmd = Command::new("/bin/sh");
                cmd.arg("-s");
                cmd
            }
        }
    }

    /// The daemon state directory on the host, as configured (`None`: the
    /// default `~/.otter`).
    pub fn home(&self) -> Option<&str> {
        match self {
            Transport::Local { home, .. } | Transport::Ssh { home, .. } => home.as_deref(),
        }
    }

    fn otterd_path(&self) -> &str {
        match self {
            Transport::Local { otterd_path, .. } | Transport::Ssh { otterd_path, .. } => {
                otterd_path
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("otterd is not installed on the host (looked for `{path}`){detail}")]
    NotInstalled { path: String, detail: String },
    #[error("could not connect to otterd: {0}")]
    Connect(String),
    #[error(
        "otterd speaks protocol v{server} but this client speaks v{client}; install matching versions"
    )]
    ProtocolMismatch { server: u32, client: u32 },
    #[error("connection to otterd closed{0}")]
    Closed(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("installing otterd failed: {0}")]
    Install(String),
    #[error("port forwarding: {0}")]
    Forward(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ClientError {
    /// The host can't serve the event cursor (rotated away, or its log started
    /// over): reload a snapshot and follow on from its cursor.
    pub fn is_cursor_expired(&self) -> bool {
        matches!(self, ClientError::Rpc(e) if e.code == otter_protocol::ErrorCode::CursorExpired)
    }
}

pub type Result<T, E = ClientError> = std::result::Result<T, E>;

/// An RPC-mode connection to one daemon. Requests are issued sequentially.
pub struct Connection {
    child: Child,
    reader: BufReader<ChildStdout>,
    writer: ChildStdin,
    next_id: u64,
    stderr: Arc<Mutex<String>>,
    /// The daemon's version.
    pub server_version: String,
}

impl Connection {
    pub async fn connect(transport: &Transport) -> Result<Connection> {
        let mut cmd = transport.command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| match (transport, e.kind()) {
            (Transport::Local { otterd_path, .. }, std::io::ErrorKind::NotFound) => {
                ClientError::NotInstalled {
                    path: otterd_path.clone(),
                    detail: String::new(),
                }
            }
            (Transport::Ssh { .. }, std::io::ErrorKind::NotFound) => {
                ClientError::Connect("`ssh` not found".into())
            }
            _ => ClientError::Connect(e.to_string()),
        })?;

        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let stderr = stderr.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut s = stderr.lock().unwrap();
                    if s.len() < 16 * 1024 {
                        s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            });
        }

        let mut conn = Connection {
            reader: BufReader::new(child.stdout.take().expect("piped")),
            writer: child.stdin.take().expect("piped"),
            child,
            next_id: 1,
            stderr,
            server_version: String::new(),
        };
        match read_json::<_, ServerMessage>(&mut conn.reader).await {
            Ok(Some(ServerMessage::Hello { protocol, version })) => {
                if protocol != PROTOCOL_VERSION {
                    return Err(ClientError::ProtocolMismatch {
                        server: protocol,
                        client: PROTOCOL_VERSION,
                    });
                }
                conn.server_version = version;
                Ok(conn)
            }
            Ok(Some(other)) => Err(ClientError::Protocol(format!(
                "expected hello, got {other:?}"
            ))),
            Ok(None) => Err(conn.startup_failure(transport).await),
            Err(e) => Err(ClientError::Protocol(format!(
                "unexpected output from otterd dial: {e}{}",
                conn.stderr_suffix()
            ))),
        }
    }

    /// Explain why the transport ended before the daemon said hello.
    async fn startup_failure(&mut self, transport: &Transport) -> ClientError {
        let status = tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .ok()
            .and_then(|r| r.ok());
        // Let the stderr reader catch up.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stderr = self.stderr.lock().unwrap().trim().to_owned();
        let code = status.and_then(|s| s.code());
        let missing = code == Some(127)
            || stderr.contains("No such file or directory")
            || stderr.contains("command not found")
            || stderr.ends_with("not found");
        if missing && !stderr.contains("Could not resolve hostname") {
            return ClientError::NotInstalled {
                path: transport.otterd_path().to_owned(),
                detail: if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                },
            };
        }
        let what = match code {
            Some(255) if matches!(transport, Transport::Ssh { .. }) => "ssh failed",
            _ => "otterd dial exited",
        };
        ClientError::Connect(if stderr.is_empty() {
            format!(
                "{what} (status {})",
                code.map_or("unknown".into(), |c| c.to_string())
            )
        } else {
            format!("{what}: {stderr}")
        })
    }

    fn stderr_suffix(&self) -> String {
        let s = self.stderr.lock().unwrap();
        let s = s.trim();
        if s.is_empty() {
            String::new()
        } else {
            format!(" (stderr: {s})")
        }
    }

    /// Send a request and wait for its result.
    pub async fn call<T: DeserializeOwned>(&mut self, request: Request) -> Result<T> {
        let id = self.next_id;
        self.next_id += 1;
        write_json(&mut self.writer, &ClientMessage { id, request })
            .await
            .map_err(|e| ClientError::Closed(format!(": {e}{}", self.stderr_suffix())))?;
        loop {
            let msg = read_json::<_, ServerMessage>(&mut self.reader)
                .await
                .map_err(|e| ClientError::Protocol(e.to_string()))?;
            match msg {
                None => return Err(ClientError::Closed(self.stderr_suffix())),
                Some(ServerMessage::Response {
                    id: rid,
                    result,
                    error,
                }) if rid == id => {
                    if let Some(e) = error {
                        return Err(ClientError::Rpc(e));
                    }
                    return serde_json::from_value(result.unwrap_or(serde_json::Value::Null))
                        .map_err(|e| ClientError::Protocol(format!("bad result: {e}")));
                }
                Some(_) => continue,
            }
        }
    }

    pub async fn ping(&mut self) -> Result<()> {
        self.call(Request::Ping).await
    }

    pub async fn host_status(&mut self) -> Result<HostStatus> {
        self.call(Request::HostStatus).await
    }

    /// Resource usage now and over the last minutes.
    pub async fn host_metrics(&mut self) -> Result<otter_protocol::host::HostMetrics> {
        self.call(Request::HostMetrics).await
    }

    /// Recorded usage over `range` (`1h`, `24h`, `7d` or `30d`).
    pub async fn host_history(&mut self, range: &str) -> Result<otter_protocol::host::HostHistory> {
        self.call(Request::HostHistory(otter_protocol::host::HistoryQuery {
            range: range.to_owned(),
        }))
        .await
    }

    /// List a directory in a workspace (`path` relative to its root, or absolute).
    pub async fn fs_list(
        &mut self,
        workspace: &str,
        path: &str,
    ) -> Result<otter_protocol::fs::DirListing> {
        self.call(Request::FsList(otter_protocol::fs::FsPath {
            workspace: workspace.to_owned(),
            path: path.to_owned(),
        }))
        .await
    }

    /// Read up to `len` bytes of a file from `offset`.
    pub async fn fs_read(
        &mut self,
        workspace: &str,
        path: &str,
        offset: u64,
        len: u64,
    ) -> Result<otter_protocol::fs::FileChunk> {
        self.call(Request::FsRead(otter_protocol::fs::FsRead {
            at: otter_protocol::fs::FsPath {
                workspace: workspace.to_owned(),
                path: path.to_owned(),
            },
            offset,
            len,
        }))
        .await
    }

    /// Write a chunk (base64 `data`) of a file; `create` starts it.
    pub async fn fs_write(&mut self, w: otter_protocol::fs::FsWrite) -> Result<()> {
        self.call(Request::FsWrite(w)).await
    }

    /// Hand a PNG (base64) to a session's clipboard stand-in.
    pub async fn paste_image(&mut self, workspace: &str, session: &str, png: String) -> Result<()> {
        self.call(Request::SessionPasteImage(otter_protocol::fs::PasteImage {
            workspace: workspace.to_owned(),
            session: session.to_owned(),
            png,
        }))
        .await
    }

    /// Take a sign-in page announced by `BrowserOpenRequested`, once.
    pub async fn browser_take(
        &mut self,
        request_id: &str,
    ) -> Result<otter_protocol::BrowserOpening> {
        self.call(Request::BrowserTake(otter_protocol::BrowserTake {
            request_id: request_id.to_owned(),
        }))
        .await
    }

    /// TCP ports listening on the host.
    pub async fn host_ports(&mut self) -> Result<Vec<otter_protocol::host::ListeningPort>> {
        self.call(Request::HostPorts).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.call(Request::Shutdown).await
    }

    /// The host's settings (secrets only as set or not).
    pub async fn settings_get(&mut self) -> Result<otter_protocol::host::Settings> {
        self.call(Request::SettingsGet).await
    }

    /// Change settings; a secret goes to the host and is never read back.
    pub async fn settings_set(
        &mut self,
        u: otter_protocol::host::SettingsUpdate,
    ) -> Result<otter_protocol::host::Settings> {
        self.call(Request::SettingsSet(u)).await
    }

    /// The models a controller offers, as the host's provider lists them.
    pub async fn settings_models(
        &mut self,
        controller: &str,
    ) -> Result<Vec<otter_protocol::host::ModelInfo>> {
        self.call(Request::SettingsModels(otter_protocol::host::ModelsQuery {
            controller: controller.to_owned(),
        }))
        .await
    }

    pub async fn feature_list(&mut self) -> Result<Vec<otter_core::feature::Feature>> {
        self.call(Request::FeatureList).await
    }

    /// Delete a feature that isn't running (its workspace stays).
    pub async fn feature_delete(&mut self, feature: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call(Request::FeatureDelete(
                otter_protocol::feature::FeatureRef {
                    feature: feature.to_owned(),
                },
            ))
            .await?;
        Ok(())
    }

    pub async fn feature_get(&mut self, feature: &str) -> Result<otter_core::feature::Feature> {
        self.call(Request::FeatureGet(otter_protocol::feature::FeatureRef {
            feature: feature.to_owned(),
        }))
        .await
    }

    pub async fn feature_create(
        &mut self,
        p: otter_protocol::feature::FeatureCreate,
    ) -> Result<otter_core::feature::Feature> {
        self.call(Request::FeatureCreate(p)).await
    }

    /// A message from the developer; `command_id` makes a retry harmless.
    pub async fn feature_send(
        &mut self,
        command_id: &str,
        feature: &str,
        text: &str,
    ) -> Result<otter_core::feature::Feature> {
        self.call(Request::FeatureSend(otter_protocol::feature::FeatureSend {
            command_id: command_id.to_owned(),
            feature: feature.to_owned(),
            text: text.to_owned(),
        }))
        .await
    }

    pub async fn feature_act(
        &mut self,
        command_id: &str,
        feature: &str,
        action: otter_core::feature::FeatureAction,
    ) -> Result<otter_core::feature::Feature> {
        self.call(Request::FeatureAct(otter_protocol::feature::FeatureAct {
            command_id: command_id.to_owned(),
            feature: feature.to_owned(),
            action,
        }))
        .await
    }

    /// A file a feature's checks produced (a screenshot), whole, base64.
    pub async fn feature_artifact(
        &mut self,
        feature: &str,
        name: &str,
    ) -> Result<otter_protocol::fs::FileChunk> {
        self.call(Request::FeatureArtifact(
            otter_protocol::feature::FeatureArtifact {
                feature: feature.to_owned(),
                name: name.to_owned(),
            },
        ))
        .await
    }

    /// A feature's history with `seq > after`, oldest first.
    pub async fn feature_events(
        &mut self,
        feature: &str,
        after: Option<u64>,
    ) -> Result<Vec<otter_core::feature::FeatureEventRecord>> {
        self.call(Request::FeatureEvents(
            otter_protocol::feature::FeatureEvents {
                feature: feature.to_owned(),
                after,
                limit: None,
            },
        ))
        .await
    }

    pub async fn workspace_create(&mut self, p: WorkspaceCreate) -> Result<Workspace> {
        self.call(Request::WorkspaceCreate(p)).await
    }

    pub async fn workspace_list(&mut self) -> Result<Vec<Workspace>> {
        self.call(Request::WorkspaceList).await
    }

    pub async fn workspace_get(&mut self, workspace: &str) -> Result<Workspace> {
        self.call(Request::WorkspaceGet(ws_ref(workspace))).await
    }

    /// Delete a workspace. `force` discards uncommitted changes in a Git
    /// worktree.
    pub async fn workspace_delete(&mut self, workspace: &str, force: bool) -> Result<()> {
        self.call(Request::WorkspaceDelete(WorkspaceDelete {
            workspace: workspace.to_owned(),
            force,
        }))
        .await
    }

    pub async fn workspace_prepare(&mut self, workspace: &str) -> Result<Workspace> {
        self.call(Request::WorkspacePrepare(ws_ref(workspace)))
            .await
    }

    pub async fn workspace_archive(&mut self, workspace: &str) -> Result<Workspace> {
        self.call(Request::WorkspaceArchive(ws_ref(workspace)))
            .await
    }

    pub async fn workspace_unarchive(&mut self, workspace: &str) -> Result<Workspace> {
        self.call(Request::WorkspaceUnarchive(ws_ref(workspace)))
            .await
    }

    /// Replace a workspace's brief.
    pub async fn workspace_set_brief(
        &mut self,
        workspace: &str,
        brief: Brief,
    ) -> Result<Workspace> {
        self.call(Request::WorkspaceSetBrief(WorkspaceSetBrief {
            workspace: workspace.to_owned(),
            brief,
        }))
        .await
    }

    pub async fn session_create(&mut self, p: SessionCreate) -> Result<Session> {
        self.call(Request::SessionCreate(p)).await
    }

    pub async fn session_stop(&mut self, workspace: &str, session: &str) -> Result<Session> {
        self.call(Request::SessionStop(session_ref(workspace, session)))
            .await
    }

    pub async fn session_restart(&mut self, workspace: &str, session: &str) -> Result<Session> {
        self.call(Request::SessionRestart(session_ref(workspace, session)))
            .await
    }

    pub async fn session_delete(&mut self, workspace: &str, session: &str) -> Result<()> {
        self.call(Request::SessionDelete(session_ref(workspace, session)))
            .await
    }

    pub async fn session_read(
        &mut self,
        workspace: &str,
        session: &str,
        lines: Option<u32>,
    ) -> Result<String> {
        let out: SessionOutput = self
            .call(Request::SessionRead(SessionRead {
                session: session_ref(workspace, session),
                lines,
            }))
            .await?;
        Ok(out.text)
    }

    pub async fn session_write(
        &mut self,
        workspace: &str,
        session: &str,
        data: &str,
        enter: bool,
    ) -> Result<()> {
        self.call(Request::SessionWrite(SessionWrite {
            session: session_ref(workspace, session),
            data: data.to_owned(),
            enter,
        }))
        .await
    }

    /// Resolve a workspace's attention items (optionally only one session's).
    /// Returns how many were resolved.
    pub async fn attention_resolve(
        &mut self,
        workspace: &str,
        session: Option<&str>,
    ) -> Result<usize> {
        self.call(Request::AttentionResolve(AttentionResolve {
            workspace: workspace.to_owned(),
            session: session.map(str::to_owned),
            attention: None,
        }))
        .await
    }

    pub async fn events_list(&mut self, limit: Option<u32>) -> Result<Vec<EventRecord>> {
        self.call(Request::EventsList(EventsList { limit })).await
    }

    /// Turn this connection into an interactive attach.
    pub async fn attach(
        mut self,
        params: SessionAttach,
    ) -> Result<(AttachReady, AttachReader, AttachWriter)> {
        let ready: AttachReady = self.call(Request::SessionAttach(params)).await?;
        Ok((
            ready,
            AttachReader {
                reader: self.reader,
            },
            AttachWriter {
                writer: self.writer,
                _child: self.child,
            },
        ))
    }

    /// Current state and the event cursor it corresponds to; follow on with
    /// `subscribe(Some(snapshot.cursor()))`.
    pub async fn snapshot(&mut self) -> Result<StateSnapshot> {
        self.call(Request::StateSnapshot).await
    }

    /// Turn this connection into a stream of events: those after `after` (if
    /// given), then live ones. A cursor the host can no longer serve fails with
    /// [`ErrorCode::CursorExpired`](otter_protocol::ErrorCode::CursorExpired)
    /// ([`ClientError::is_cursor_expired`]); reload a snapshot then.
    pub async fn subscribe(self, after: Option<Cursor>) -> Result<EventStream> {
        self.subscribe_with(after, false).await
    }

    /// Like [`Self::subscribe`], as an app that opens sign-in pages for
    /// browser login (`BrowserOpenRequested`).
    pub async fn subscribe_for_browser(self, after: Option<Cursor>) -> Result<EventStream> {
        self.subscribe_with(after, true).await
    }

    async fn subscribe_with(mut self, after: Option<Cursor>, browser: bool) -> Result<EventStream> {
        let (after, log_id) = match after {
            Some(c) => (Some(c.seq), c.log_id),
            None => (None, None),
        };
        let ready: Subscribed = self
            .call(Request::EventsSubscribe(EventsSubscribe {
                after,
                log_id: log_id.clone(),
                browser,
            }))
            .await?;
        Ok(EventStream {
            seq: ready.seq,
            last: after.unwrap_or(ready.seq),
            // A daemon before D-037 names no log.
            log_id: ready.log_id.or(log_id),
            conn: self,
        })
    }
}

pub struct AttachReader {
    reader: BufReader<ChildStdout>,
}

impl AttachReader {
    /// Next frame from the daemon; `None` when the connection closed.
    pub async fn next(&mut self) -> Result<Option<Frame>> {
        Ok(read_frame(&mut self.reader).await?)
    }
}

pub struct AttachWriter {
    writer: ChildStdin,
    /// Keeps the transport process alive for the attach's lifetime.
    _child: Child,
}

impl AttachWriter {
    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        Ok(write_frame(&mut self.writer, frame).await?)
    }
}

pub struct EventStream {
    /// The latest event when the subscription started.
    pub seq: u64,
    /// The last event received (or the cursor subscribed from).
    last: u64,
    log_id: Option<String>,
    conn: Connection,
}

impl EventStream {
    pub async fn next(&mut self) -> Result<Option<EventRecord>> {
        loop {
            match read_json::<_, ServerMessage>(&mut self.conn.reader)
                .await
                .map_err(|e| ClientError::Protocol(e.to_string()))?
            {
                None => return Ok(None),
                Some(ServerMessage::Event { event }) => {
                    self.last = self.last.max(event.seq);
                    return Ok(Some(event));
                }
                Some(_) => continue,
            }
        }
    }

    /// Where to resume after this stream ends: the last event received, in
    /// this host's log.
    pub fn cursor(&self) -> Cursor {
        Cursor {
            seq: self.last,
            log_id: self.log_id.clone(),
        }
    }
}

fn ws_ref(workspace: &str) -> WorkspaceRef {
    WorkspaceRef {
        workspace: workspace.to_owned(),
    }
}

fn session_ref(workspace: &str, session: &str) -> SessionRef {
    SessionRef {
        workspace: workspace.to_owned(),
        session: session.to_owned(),
    }
}

/// Quote a path for the remote shell, leaving a leading `~/` unquoted so it
/// expands to the remote home (as in `otterd_path`).
fn remote_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("~/{}", sh_quote(rest)),
        None => sh_quote(path),
    }
}

/// Quote `s` for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@,+%".contains(&b));
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("/a/b"), "/a/b");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn remote_home_keeps_tilde_expandable() {
        assert_eq!(remote_path("~/.otter-dev"), "~/.otter-dev");
        assert_eq!(remote_path("~/my dir"), "~/'my dir'");
        assert_eq!(remote_path("/tmp/x y"), "'/tmp/x y'");
        assert_eq!(remote_path("~bob/x"), "'~bob/x'");
    }

    #[test]
    fn ssh_command_line() {
        let t = Transport::Ssh {
            destination: "dev-01".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
            otterd_path: "~/.local/bin/otterd".into(),
            home: Some("/tmp/x y".into()),
            control_dir: Some("/c".into()),
        };
        let cmd = t.command();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/c/%C",
                "-o",
                "ControlPersist=600",
                "-p",
                "2222",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "dev-01",
                "~/.local/bin/otterd --home '/tmp/x y' dial"
            ]
        );
    }
}
