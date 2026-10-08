//! Client for the Workd daemon.
//!
//! A [`Connection`] runs `workd dial` — locally or as `ssh <host> workd dial` —
//! and speaks the protocol over its stdin/stdout. SSH is the only remote
//! transport (design rule 4); the user's SSH config and agent are used as-is.

pub mod config;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use workd_core::{HostStatus, Session, Workspace};
use workd_protocol::frame::{Frame, read_frame, write_frame};
use workd_protocol::wire::{read_json, write_json};
use workd_protocol::{
    AttachReady, AttentionResolve, ClientMessage, EventRecord, EventsList, EventsSubscribe,
    PROTOCOL_VERSION, Request, RpcError, ServerMessage, SessionAttach, SessionCreate,
    SessionOutput, SessionRead, SessionRef, SessionWrite, StateSnapshot, Subscribed,
    WorkspaceCreate, WorkspaceDelete, WorkspaceRef,
};

/// How to reach a host's daemon.
#[derive(Clone, Debug)]
pub enum Transport {
    /// Run `workd dial` on this machine.
    Local {
        workd_path: String,
        home: Option<String>,
    },
    /// Run `workd dial` on a remote host over SSH.
    Ssh {
        destination: String,
        /// Extra arguments for `ssh` (e.g. `-p 2222`, `-J jump`).
        ssh_args: Vec<String>,
        /// Path of `workd` on the remote host. Interpreted by the remote shell,
        /// so `~` works.
        workd_path: String,
        home: Option<String>,
        /// Where to keep SSH ControlMaster sockets; `None` disables
        /// multiplexing.
        control_dir: Option<PathBuf>,
    },
}

impl Transport {
    fn command(&self) -> Command {
        match self {
            Transport::Local { workd_path, home } => {
                let mut cmd = Command::new(workd_path);
                if let Some(home) = home {
                    cmd.arg("--home").arg(home);
                }
                cmd.arg("dial");
                cmd
            }
            Transport::Ssh {
                destination,
                ssh_args,
                workd_path,
                home,
                control_dir,
            } => {
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
                // Notice a dead network (laptop asleep, Wi-Fi gone) within
                // ~45s instead of hanging until TCP gives up. After
                // `ssh_args`, so explicit options there take precedence.
                cmd.args([
                    "-o",
                    "ServerAliveInterval=15",
                    "-o",
                    "ServerAliveCountMax=3",
                ]);
                cmd.arg(destination);
                let mut remote = workd_path.clone();
                if let Some(home) = home {
                    remote.push_str(" --home ");
                    remote.push_str(&sh_quote(home));
                }
                remote.push_str(" dial");
                cmd.arg(remote);
                cmd
            }
        }
    }

    fn workd_path(&self) -> &str {
        match self {
            Transport::Local { workd_path, .. } | Transport::Ssh { workd_path, .. } => workd_path,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("workd is not installed on the host (looked for `{path}`){detail}")]
    NotInstalled { path: String, detail: String },
    #[error("could not connect to workd: {0}")]
    Connect(String),
    #[error(
        "workd speaks protocol v{server} but this client speaks v{client}; install matching versions"
    )]
    ProtocolMismatch { server: u32, client: u32 },
    #[error("connection to workd closed{0}")]
    Closed(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
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
            (Transport::Local { workd_path, .. }, std::io::ErrorKind::NotFound) => {
                ClientError::NotInstalled {
                    path: workd_path.clone(),
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
                "unexpected output from workd dial: {e}{}",
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
                path: transport.workd_path().to_owned(),
                detail: if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                },
            };
        }
        let what = match code {
            Some(255) if matches!(transport, Transport::Ssh { .. }) => "ssh failed",
            _ => "workd dial exited",
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

    pub async fn shutdown(&mut self) -> Result<()> {
        self.call(Request::Shutdown).await
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
    /// `subscribe(Some(snapshot.seq))`.
    pub async fn snapshot(&mut self) -> Result<StateSnapshot> {
        self.call(Request::StateSnapshot).await
    }

    /// Turn this connection into a stream of events: those after `after` (if
    /// given), then live ones. A cursor the host can no longer serve fails with
    /// [`ErrorCode::CursorExpired`](workd_protocol::ErrorCode::CursorExpired);
    /// reload a snapshot then.
    pub async fn subscribe(mut self, after: Option<u64>) -> Result<EventStream> {
        let ready: Subscribed = self
            .call(Request::EventsSubscribe(EventsSubscribe { after }))
            .await?;
        Ok(EventStream {
            seq: ready.seq,
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
                Some(ServerMessage::Event { event }) => return Ok(Some(event)),
                Some(_) => continue,
            }
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
    fn ssh_command_line() {
        let t = Transport::Ssh {
            destination: "dev-01".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
            workd_path: "~/.local/bin/workd".into(),
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
                "~/.local/bin/workd --home '/tmp/x y' dial"
            ]
        );
    }
}
