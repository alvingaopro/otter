//! End-to-end tests: a real `otterd` (auto-started through `otterd dial`) with a
//! real, isolated tmux server per test.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use otter_client::{Connection, Transport};
use otter_core::{
    EnvironmentKind, EnvironmentStatus, ExecutionState, SessionKind, SessionStatus, Workspace,
    WorkspaceSource, WorkspaceState,
};
use otter_protocol::frame::{AttachExitReason, Control, Frame};
use otter_protocol::{
    Cursor, Event, SessionAttach, SessionCreate, SessionRef, SessionSpec, SourceSpec,
    WorkspaceCreate,
};

#[path = "e2e/chat_bootstrap.rs"]
mod chat_bootstrap;
#[path = "e2e/commands.rs"]
mod commands;
#[path = "e2e/converse.rs"]
mod converse;
#[path = "e2e/coordination.rs"]
mod coordination;
#[path = "e2e/git_account.rs"]
mod git_account;
#[path = "e2e/host_chat.rs"]
mod host_chat;
#[path = "e2e/memory.rs"]
mod memory;
#[path = "e2e/merged.rs"]
mod merged;
#[path = "e2e/otter_turn.rs"]
mod otter_turn;
#[path = "e2e/provider_turn.rs"]
mod provider_turn;
#[path = "e2e/watches.rs"]
mod watches;
#[path = "e2e/workspace_prep.rs"]
mod workspace_prep;

struct TestHost {
    dir: tempfile::TempDir,
    transport: Transport,
    /// A daemon started directly by the test (see `with_env`).
    daemon: Option<std::process::Child>,
    /// The variables it was started with, to start it again.
    vars: Vec<(String, String)>,
}

impl TestHost {
    fn new() -> Self {
        // Short path: Unix socket paths are length-limited.
        let dir = tempfile::Builder::new().prefix("wd").tempdir().unwrap();
        let transport = Transport::Local {
            otterd_path: env!("CARGO_BIN_EXE_otterd").to_owned(),
            home: Some(dir.path().to_string_lossy().into_owned()),
        };
        TestHost {
            dir,
            transport,
            daemon: None,
            vars: Vec::new(),
        }
    }

    /// Start the daemon directly with extra environment variables (which the
    /// login-shell environment it captures inherits).
    async fn with_env(vars: &[(&str, String)]) -> Self {
        let mut host = Self::new();
        host.vars = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        host.spawn_daemon().await;
        host
    }

    async fn spawn_daemon(&mut self) {
        let _ = std::fs::remove_file(self.home().join("run/workd.sock"));
        let daemon = Command::new(env!("CARGO_BIN_EXE_otterd"))
            .arg("--home")
            .arg(self.home())
            .arg("serve")
            .envs(self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(daemon);
        let socket = self.home().join("run/workd.sock");
        eventually("daemon listening", || {
            let ok = socket.exists();
            async move { ok.then_some(()) }
        })
        .await;
    }

    /// Stop a daemon started by `with_env` and start it again with the same
    /// variables (a dialed restart wouldn't have them).
    async fn restart(&mut self) {
        self.conn().await.shutdown().await.unwrap();
        if let Some(mut d) = self.daemon.take() {
            let _ = d.wait();
        }
        self.spawn_daemon().await;
    }

    /// After a daemon started by `with_env` died on its own (a crash test),
    /// start it again without `var`.
    async fn revive_without(&mut self, var: &str) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.wait();
        }
        self.vars.retain(|(k, _)| k != var);
        self.spawn_daemon().await;
    }

    fn home(&self) -> &Path {
        self.dir.path()
    }

    async fn conn(&self) -> Connection {
        Connection::connect(&self.transport).await.unwrap()
    }

    fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-S")
            .arg(self.home().join("run/tmux.sock"))
            .args(args)
            .output()
            .unwrap()
    }

    fn tmux_has_session(&self, name: &str) -> bool {
        self.tmux(&["has-session", "-t", &format!("={name}")])
            .status
            .success()
    }

    fn daemon_pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.home().join("run/workd.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Shut the daemon down and wait until it has exited.
    async fn stop_daemon(&self) {
        let pid = self.daemon_pid();
        self.conn().await.shutdown().await.unwrap();
        if let Some(pid) = pid {
            eventually("daemon exits", || async move {
                (unsafe { libc::kill(pid, 0) } != 0).then_some(())
            })
            .await;
        }
    }
}

impl Drop for TestHost {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let _ = self.tmux(&["kill-server"]);
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

async fn eventually<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn spec(kind: SessionKind, name: &str, command: &str) -> SessionSpec {
    SessionSpec {
        name: Some(name.into()),
        kind,
        command: Some(command.into()),
        provider: None,
        prompt: None,
        resume: None,
    }
}

async fn create(host: &TestHost, name: &str, sessions: Option<Vec<SessionSpec>>) -> Workspace {
    host.conn()
        .await
        .workspace_create(WorkspaceCreate {
            name: name.into(),
            sessions,
            ..Default::default()
        })
        .await
        .unwrap()
}

async fn wait_status(host: &TestHost, ws: &str, session: &str, want: SessionStatus) -> Workspace {
    eventually(&format!("{ws}/{session} becomes {want:?}"), || async {
        let w = host.conn().await.workspace_get(ws).await.unwrap();
        (w.session(session).unwrap().status() == want).then_some(w)
    })
    .await
}

async fn wait_output(host: &TestHost, ws: &str, session: &str, needle: &str) -> String {
    eventually(&format!("{ws}/{session} prints {needle:?}"), || async {
        let text = host
            .conn()
            .await
            .session_read(ws, session, None)
            .await
            .unwrap();
        text.contains(needle).then_some(text)
    })
    .await
}

#[tokio::test]
async fn dial_autostarts_daemon_and_reports_status() {
    let host = TestHost::new();
    let mut conn = host.conn().await;
    conn.ping().await.unwrap();
    let status = conn.host_status().await.unwrap();
    assert_eq!(status.protocol_version, otter_protocol::PROTOCOL_VERSION);
    let tmux = status
        .capabilities
        .iter()
        .find(|c| c.name == "tmux")
        .unwrap();
    assert!(tmux.available, "{status:?}");
    // The socket is private to the user.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(host.home().join("run/workd.sock"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn scratch_workspace_with_default_login_shell() {
    let host = TestHost::new();
    let ws = create(&host, "scratch", None).await;
    assert_eq!(ws.sessions.len(), 1);
    let shell = &ws.sessions[0];
    assert_eq!(shell.name, "shell");
    assert_eq!(shell.kind, SessionKind::Terminal);
    assert_eq!(shell.status(), SessionStatus::Running);
    assert!(Path::new(&ws.root).is_dir());

    // The environment never appears on a command line (visible to all users).
    let server_pid = host.tmux(&["display-message", "-p", "#{pid}"]);
    let server_pid = String::from_utf8_lossy(&server_pid.stdout)
        .trim()
        .to_owned();
    let cmdline = Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &server_pid])
        .output()
        .unwrap();
    let cmdline = String::from_utf8_lossy(&cmdline.stdout);
    assert!(
        !cmdline.contains("PATH="),
        "env leaked into tmux argv: {cmdline}"
    );
    assert!(
        std::fs::read_dir(host.home().join("run/env"))
            .unwrap()
            .next()
            .is_none(),
        "env file left behind"
    );

    // The shell runs in the workspace root with otterd's variables set.
    let mut conn = host.conn().await;
    conn.session_write(
        "scratch",
        "shell",
        "echo \"id=$OTTER_SESSION_ID pwd=$(pwd)\"",
        true,
    )
    .await
    .unwrap();
    let text = wait_output(&host, "scratch", "shell", &format!("id={}", shell.id)).await;
    // `pwd` reports the resolved path (on macOS the temp dir is under a symlink).
    let root = std::fs::canonicalize(&ws.root).unwrap();
    assert!(text.contains(&format!("pwd={}", root.display())), "{text}");

    // Names are unique per host.
    let err = host
        .conn()
        .await
        .workspace_create(WorkspaceCreate {
            name: "scratch".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    let exec = shell.current_execution().unwrap().backend_ref.clone();
    assert!(host.tmux_has_session(&exec));
    host.conn()
        .await
        .workspace_delete("scratch", false)
        .await
        .unwrap();
    assert!(!host.tmux_has_session(&exec));
    assert!(!Path::new(&ws.root).exists());
    assert!(host.conn().await.workspace_list().await.unwrap().is_empty());
}

#[tokio::test]
async fn command_exit_code_is_recorded_and_restart_creates_new_execution() {
    let host = TestHost::new();
    create(
        &host,
        "commands",
        Some(vec![
            spec(SessionKind::Command, "fail", "echo command-output; exit 3"),
            spec(SessionKind::Command, "ok", "true"),
        ]),
    )
    .await;
    let ws = wait_status(&host, "commands", "fail", SessionStatus::Failed).await;
    let exec = ws
        .session("fail")
        .unwrap()
        .current_execution()
        .unwrap()
        .clone();
    assert_eq!(exec.state, ExecutionState::Exited);
    assert_eq!(exec.exit_code, Some(3));
    wait_status(&host, "commands", "ok", SessionStatus::Completed).await;

    // Output of an exited process stays readable.
    wait_output(&host, "commands", "fail", "command-output").await;

    let restarted = host
        .conn()
        .await
        .session_restart("commands", "fail")
        .await
        .unwrap();
    assert_eq!(restarted.executions.len(), 2);
    assert_ne!(restarted.executions[1].id, exec.id);
    assert_eq!(
        restarted.id,
        ws.session("fail").unwrap().id,
        "session identity is stable"
    );
    // The previous execution's backend resources are released.
    assert!(!host.tmux_has_session(&exec.backend_ref));
    wait_status(&host, "commands", "fail", SessionStatus::Failed).await;

    let events = host.conn().await.events_list(None).await.unwrap();
    assert!(events.iter().any(|e| matches!(
        &e.event,
        Event::ExecutionExited {
            exit_code: Some(3),
            ..
        }
    )));
    // Commands never end up in the event log.
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(!log.contains("command-output"));
}

#[tokio::test]
async fn service_without_command_is_rejected() {
    let host = TestHost::new();
    create(&host, "w", Some(vec![])).await;
    let err = host
        .conn()
        .await
        .session_create(SessionCreate {
            workspace: "w".into(),
            spec: SessionSpec {
                name: None,
                kind: SessionKind::Service,
                command: None,
                provider: None,
                prompt: None,
                resume: None,
            },
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("needs a command"), "{err}");
}

#[tokio::test]
async fn stop_then_restart() {
    let host = TestHost::new();
    create(
        &host,
        "svc",
        Some(vec![spec(SessionKind::Service, "srv", "sleep 600")]),
    )
    .await;
    let stopped = host.conn().await.session_stop("svc", "srv").await.unwrap();
    assert_eq!(stopped.status(), SessionStatus::Stopped);
    let backend_ref = &stopped.current_execution().unwrap().backend_ref;
    assert!(!host.tmux_has_session(backend_ref));
    let err = host
        .conn()
        .await
        .session_write("svc", "srv", "x", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("restart it first"), "{err}");
    let restarted = host
        .conn()
        .await
        .session_restart("svc", "srv")
        .await
        .unwrap();
    assert_eq!(restarted.status(), SessionStatus::Running);
}

async fn attach(
    host: &TestHost,
    ws: &str,
    session: &str,
) -> (otter_client::AttachReader, otter_client::AttachWriter) {
    let (ready, reader, writer) = attach_with(host, ws, session, None).await;
    assert!(!ready.history);
    (reader, writer)
}

async fn attach_with(
    host: &TestHost,
    ws: &str,
    session: &str,
    history: Option<u32>,
) -> (
    otter_protocol::AttachReady,
    otter_client::AttachReader,
    otter_client::AttachWriter,
) {
    host.conn()
        .await
        .attach(SessionAttach {
            session: SessionRef {
                workspace: ws.into(),
                session: session.into(),
            },
            cols: 100,
            rows: 30,
            term: Some("xterm-256color".into()),
            history,
            controls: false,
            own_terminal: false,
        })
        .await
        .unwrap()
}

/// A stream attach that takes control frames (D-119), at `cols`×`rows`.
async fn controlled_attach(
    host: &TestHost,
    ws: &str,
    session: &str,
    (cols, rows): (u16, u16),
    history: u32,
) -> (otter_client::AttachReader, otter_client::AttachWriter) {
    let (ready, reader, writer) = host
        .conn()
        .await
        .attach(SessionAttach {
            session: SessionRef {
                workspace: ws.into(),
                session: session.into(),
            },
            cols,
            rows,
            term: Some("xterm-256color".into()),
            history: Some(history),
            controls: true,
            own_terminal: false,
        })
        .await
        .unwrap();
    assert!(ready.history && ready.controls);
    (reader, writer)
}

/// Read frames until a control frame `want` accepts; the output before it.
async fn read_until_control(
    reader: &mut otter_client::AttachReader,
    want: impl Fn(&Control) -> bool,
) -> (String, Control) {
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match reader.next().await.unwrap() {
                Some(Frame::Data(d)) => seen.extend_from_slice(&d),
                Some(Frame::Control(c)) if want(&c) => {
                    return (String::from_utf8_lossy(&seen).into_owned(), c);
                }
                Some(Frame::Control(_)) => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "no such control frame after {:?}",
            String::from_utf8_lossy(&seen)
        )
    })
}

fn snapshot_done(c: &Control) -> bool {
    matches!(c, Control::Snapshot { .. })
}

/// Output that comes within `wait`, if any.
async fn quiet_for(reader: &mut otter_client::AttachReader, wait: Duration) -> Vec<u8> {
    let mut seen = Vec::new();
    let _ = tokio::time::timeout(wait, async {
        loop {
            match reader.next().await.unwrap() {
                Some(Frame::Data(d)) => seen.extend_from_slice(&d),
                Some(_) => {}
                None => return,
            }
        }
    })
    .await;
    seen
}

/// Read frames until the accumulated output contains `needle`.
async fn read_until(reader: &mut otter_client::AttachReader, needle: &str) -> String {
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match reader.next().await.unwrap() {
                Some(Frame::Data(d)) => {
                    seen.extend_from_slice(&d);
                    if String::from_utf8_lossy(&seen).contains(needle) {
                        return;
                    }
                }
                other => panic!("unexpected frame {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "never saw {needle:?} in {:?}",
            String::from_utf8_lossy(&seen)
        )
    });
    String::from_utf8_lossy(&seen).into_owned()
}

async fn read_exit(reader: &mut otter_client::AttachReader) -> otter_protocol::frame::AttachExit {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match reader.next().await.unwrap() {
                Some(Frame::Exit(e)) => return e,
                Some(_) => continue,
                None => panic!("connection closed without exit frame"),
            }
        }
    })
    .await
    .expect("exit frame")
}

#[tokio::test]
async fn attach_round_trip_resize_and_detach() {
    let host = TestHost::new();
    let ws = create(
        &host,
        "a",
        Some(vec![spec(SessionKind::Terminal, "sh", "sh")]),
    )
    .await;
    let backend_ref = ws.sessions[0]
        .current_execution()
        .unwrap()
        .backend_ref
        .clone();
    let (mut reader, mut writer) = attach(&host, "a", "sh").await;

    writer
        .send(&Frame::Data(b"echo attach-$((6*7))\r".to_vec()))
        .await
        .unwrap();
    read_until(&mut reader, "attach-42").await;

    writer
        .send(&Frame::Resize {
            cols: 111,
            rows: 33,
        })
        .await
        .unwrap();
    eventually("window resized", || async {
        let out = host.tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("={backend_ref}:"),
            "#{window_width}x#{window_height}",
        ]);
        (String::from_utf8_lossy(&out.stdout).trim() == "111x33").then_some(())
    })
    .await;

    writer.send(&Frame::Detach).await.unwrap();
    let exit = read_exit(&mut reader).await;
    assert_eq!(exit.reason, AttachExitReason::Detached);

    // Detaching leaves the session running.
    let ws = host.conn().await.workspace_get("a").await.unwrap();
    assert_eq!(ws.sessions[0].status(), SessionStatus::Running);
    assert!(host.tmux_has_session(&backend_ref));
}

#[tokio::test]
async fn attach_ends_when_the_process_exits() {
    let host = TestHost::new();
    create(
        &host,
        "b",
        Some(vec![spec(SessionKind::Terminal, "sh", "sh")]),
    )
    .await;
    let (mut reader, mut writer) = attach(&host, "b", "sh").await;
    writer
        .send(&Frame::Data(b"exit 5\r".to_vec()))
        .await
        .unwrap();
    let exit = read_exit(&mut reader).await;
    assert_eq!(exit.reason, AttachExitReason::Exited);
    assert_eq!(exit.exit_code, Some(5));
}

#[tokio::test]
async fn client_disconnect_does_not_affect_session() {
    let host = TestHost::new();
    let ws = create(
        &host,
        "c",
        Some(vec![spec(SessionKind::Terminal, "sh", "sh")]),
    )
    .await;
    {
        let (mut reader, mut writer) = attach(&host, "c", "sh").await;
        writer
            .send(&Frame::Data(b"echo before-$((1+1))-drop\r".to_vec()))
            .await
            .unwrap();
        read_until(&mut reader, "before-2-drop").await;
        // Dropping the transport without detaching simulates the Mac
        // sleeping or the SSH connection dropping.
    }
    wait_output(&host, "c", "sh", "before-2-drop").await;
    let backend_ref = ws.sessions[0]
        .current_execution()
        .unwrap()
        .backend_ref
        .clone();
    eventually("attach client gone", || async {
        let out = host.tmux(&["list-clients", "-F", "#{client_tty}"]);
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .is_empty()
            .then_some(())
    })
    .await;
    assert!(host.tmux_has_session(&backend_ref));
    let ws = host.conn().await.workspace_get("c").await.unwrap();
    assert_eq!(ws.sessions[0].status(), SessionStatus::Running);
}

async fn sh_session(host: &TestHost, ws: &str) {
    create(
        host,
        ws,
        Some(vec![spec(SessionKind::Terminal, "sh", "sh")]),
    )
    .await;
}

async fn send(writer: &mut otter_client::AttachWriter, text: &str) {
    writer
        .send(&Frame::Data(text.as_bytes().to_vec()))
        .await
        .unwrap();
}

/// tmux clients attached to the host's server (one per live attach).
fn attach_clients(host: &TestHost) -> usize {
    let out = host.tmux(&["list-clients", "-F", "#{client_tty}"]);
    String::from_utf8_lossy(&out.stdout).lines().count()
}

#[tokio::test]
async fn attach_shows_existing_screen_and_survives_repeated_cycles() {
    let host = TestHost::new();
    sh_session(&host, "cyc").await;
    let mut conn = host.conn().await;
    conn.session_write("cyc", "sh", "echo before-$((2*3))-attach", true)
        .await
        .unwrap();
    wait_output(&host, "cyc", "sh", "before-6-attach").await;

    for i in 0..5 {
        let (mut reader, mut writer) = attach(&host, "cyc", "sh").await;
        if i == 0 {
            // Attaching shows what is already on the screen, unprompted.
            read_until(&mut reader, "before-6-attach").await;
        }
        send(&mut writer, &format!("echo cycle-$((100+{i}))\r")).await;
        read_until(&mut reader, &format!("cycle-{}", 100 + i)).await;
        writer.send(&Frame::Detach).await.unwrap();
        assert_eq!(
            read_exit(&mut reader).await.reason,
            AttachExitReason::Detached
        );
    }
    eventually("no attach clients left", || async {
        (attach_clients(&host) == 0).then_some(())
    })
    .await;
    let ws = host.conn().await.workspace_get("cyc").await.unwrap();
    assert_eq!(ws.sessions[0].status(), SessionStatus::Running);
}

/// What a terminal sends for the mouse wheel once tmux has turned on mouse
/// reporting (SGR encoding; 64 = wheel up, 65 = wheel down).
const WHEEL_UP: &[u8] = b"\x1b[<64;20;10M";

#[tokio::test]
async fn mouse_wheel_scrolls_session_history() {
    let host = TestHost::new();
    sh_session(&host, "wheel").await;
    let (mut reader, mut writer) = attach(&host, "wheel", "sh").await;
    send(
        &mut writer,
        "i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo line-$i; done; echo printed-all\r",
    )
    .await;
    read_until(&mut reader, "printed-all").await;
    let exec = host
        .conn()
        .await
        .workspace_get("wheel")
        .await
        .unwrap()
        .sessions[0]
        .current_execution()
        .unwrap()
        .backend_ref
        .clone();
    let in_history = || {
        let out = host.tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("={exec}:"),
            "#{pane_in_mode} #{scroll_position}",
        ]);
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    assert!(in_history().starts_with('0'), "{}", in_history());

    for _ in 0..3 {
        writer.send(&Frame::Data(WHEEL_UP.to_vec())).await.unwrap();
    }
    eventually("wheel scrolls into history", || {
        let state = in_history();
        async move {
            let mut it = state.split_whitespace();
            let in_mode = it.next() == Some("1");
            let pos: u32 = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            (in_mode && pos > 0).then_some(())
        }
    })
    .await;
}

#[tokio::test]
async fn mouse_drag_copies_to_the_client_clipboard() {
    let host = TestHost::new();
    sh_session(&host, "copy").await;
    let (mut reader, mut writer) = attach(&host, "copy", "sh").await;
    send(&mut writer, "clear; echo copy-$((40+2))-me\r").await;
    read_until(&mut reader, "copy-42-me").await;
    // Press on row 2, drag along it, release (SGR mouse, 1-based cells).
    for seq in [
        &b"\x1b[<0;1;2M"[..],
        b"\x1b[<32;5;2M",
        b"\x1b[<32;12;2M",
        b"\x1b[<0;12;2m",
    ] {
        writer.send(&Frame::Data(seq.to_vec())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // tmux hands the selection to the client terminal as OSC 52.
    let out = read_until(&mut reader, "\x1b]52;").await;
    let b64 = out
        .split("\x1b]52;")
        .nth(1)
        .and_then(|rest| rest.split(';').nth(1))
        .map(|p| p.trim_end_matches(['\x07', '\x1b', '\\']).to_owned())
        .unwrap_or_default();
    assert!(!b64.is_empty(), "no OSC 52 payload in {out:?}");
}

#[tokio::test]
async fn attach_carries_utf8_and_ctrl_c() {
    let host = TestHost::new();
    sh_session(&host, "keys").await;
    let (mut reader, mut writer) = attach(&host, "keys", "sh").await;

    // Output: bytes the typed command doesn't contain literally (é ✓).
    send(&mut writer, "printf 'out-\\303\\251\\342\\234\\223\\n'\r").await;
    read_until(&mut reader, "out-é✓").await;
    // Input: multi-byte characters typed into the session.
    send(&mut writer, "echo 日本-$((1+1))\r").await;
    read_until(&mut reader, "日本-2").await;

    // Ctrl-C interrupts the foreground process, not the session.
    send(&mut writer, "sleep 600\r").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(&mut writer, "\x03").await;
    send(&mut writer, "echo after-$((3*3))\r").await;
    read_until(&mut reader, "after-9").await;
    writer.send(&Frame::Detach).await.unwrap();
    assert_eq!(
        read_exit(&mut reader).await.reason,
        AttachExitReason::Detached
    );
}

/// A stream attach (D-118): the client's terminal keeps the history.
async fn stream_attach(
    host: &TestHost,
    ws: &str,
    session: &str,
) -> (otter_client::AttachReader, otter_client::AttachWriter) {
    let (ready, reader, writer) = attach_with(host, ws, session, Some(1000)).await;
    assert!(ready.history, "the daemon can stream");
    (reader, writer)
}

fn backend_ref(ws: &Workspace) -> String {
    ws.sessions[0]
        .current_execution()
        .unwrap()
        .backend_ref
        .clone()
}

#[tokio::test]
async fn stream_attach_brings_the_history_and_round_trips() {
    let host = TestHost::new();
    sh_session(&host, "st").await;
    let mut conn = host.conn().await;
    conn.session_write(
        "st",
        "sh",
        "i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo line-$i; done",
        true,
    )
    .await
    .unwrap();
    wait_output(&host, "st", "sh", "line-300").await;
    let exec = backend_ref(&conn.workspace_get("st").await.unwrap());

    let (mut reader, mut writer) = stream_attach(&host, "st", "sh").await;
    // Lines long scrolled off a 30-row screen come with the snapshot, which
    // ends with the screen and the cursor, shown all at once.
    let snapshot = read_until(&mut reader, "\x1b[?2026l").await;
    assert!(snapshot.contains("line-150\x1b[0m\r\n"), "{snapshot:?}");
    assert!(snapshot.contains("line-300"));
    assert!(snapshot.contains("\x1b[?2004h"), "pastes are bracketed");

    // Then the process's own output, as it writes it.
    send(&mut writer, "echo stream-$((6*7))\r").await;
    read_until(&mut reader, "stream-42").await;

    writer
        .send(&Frame::Resize {
            cols: 111,
            rows: 33,
        })
        .await
        .unwrap();
    eventually("window resized", || async {
        let out = host.tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("={exec}:"),
            "#{window_width}x#{window_height}",
        ]);
        (String::from_utf8_lossy(&out.stdout).trim() == "111x33").then_some(())
    })
    .await;

    writer.send(&Frame::Detach).await.unwrap();
    assert_eq!(
        read_exit(&mut reader).await.reason,
        AttachExitReason::Detached
    );
    eventually("no clients left", || async {
        let out = host.tmux(&["list-clients", "-F", "#{client_name}"]);
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .is_empty()
            .then_some(())
    })
    .await;
    assert!(host.tmux_has_session(&exec));
    let ws = host.conn().await.workspace_get("st").await.unwrap();
    assert_eq!(ws.sessions[0].status(), SessionStatus::Running);
}

#[tokio::test]
async fn stream_attach_restores_the_alternate_screen_and_its_modes() {
    let host = TestHost::new();
    sh_session(&host, "alt").await;
    let mut conn = host.conn().await;
    conn.session_write("alt", "sh", "echo normal-screen-line", true)
        .await
        .unwrap();
    wait_output(&host, "alt", "sh", "normal-screen-line").await;
    // A full-screen program: the alternate screen, the mouse, cursor keys.
    conn.session_write(
        "alt",
        "sh",
        "printf '\\033[?1049h\\033[?1002h\\033[?1006h\\033[?1h\\033[Hin-alt-screen'; sleep 600",
        true,
    )
    .await
    .unwrap();
    wait_output(&host, "alt", "sh", "in-alt-screen").await;

    let (mut reader, _writer) = stream_attach(&host, "alt", "sh").await;
    let snapshot = read_until(&mut reader, "\x1b[?2026l").await;
    let phase2 = snapshot.split("\x1bc").nth(1).unwrap_or_default();
    let (normal, alternate) = phase2.split_once("\x1b[?1049h").expect("alternate screen");
    assert!(normal.contains("normal-screen-line"), "{phase2:?}");
    assert!(alternate.contains("in-alt-screen"), "{phase2:?}");
    for mode in ["\x1b[?1002h", "\x1b[?1006h", "\x1b[?1h"] {
        assert!(alternate.contains(mode), "{mode:?} in {alternate:?}");
    }
}

#[tokio::test]
async fn stream_attach_pastes_with_brackets_only_when_asked() {
    for asked in [true, false] {
        let host = TestHost::new();
        // `cat -v` shows what the program receives, escapes as `^[`.
        let program = if asked {
            "printf '\\033[?2004h'; stty raw -echo; exec cat -v"
        } else {
            "stty raw -echo; exec cat -v"
        };
        create(
            &host,
            "p",
            Some(vec![spec(SessionKind::Terminal, "cat", program)]),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (mut reader, mut writer) = stream_attach(&host, "p", "cat").await;
        read_until(&mut reader, "\x1b[?2026l").await;
        // A paste as the client's terminal sends it, brackets and all.
        send(
            &mut writer,
            "\x1b[200~say \"$HOME\" \\ #{pane_id} ; ok\rnext\x1b[201~typed",
        )
        .await;
        let out = read_until(&mut reader, "typed").await;
        let pasted = "say \"$HOME\" \\ #{pane_id} ; ok^Mnext";
        if asked {
            assert!(
                out.contains(&format!("^[[200~{pasted}^[[201~typed")),
                "{out:?}"
            );
        } else {
            assert!(out.contains(&format!("{pasted}typed")), "{out:?}");
            assert!(!out.contains("200~"), "{out:?}");
        }
    }
}

#[tokio::test]
async fn stream_attach_keeps_styles_that_go_on_across_rows() {
    let host = TestHost::new();
    sh_session(&host, "sty").await;
    let exec = backend_ref(&host.conn().await.workspace_get("sty").await.unwrap());
    // A first attach makes the window the client's size, 100 columns.
    let (mut reader, mut writer) = stream_attach(&host, "sty", "sh").await;
    read_until(&mut reader, "\x1b[?2026l").await;
    writer.send(&Frame::Detach).await.unwrap();
    read_exit(&mut reader).await;
    let width = host.tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={exec}:"),
        "#{pane_width}",
    ]);
    assert_eq!(String::from_utf8_lossy(&width.stdout).trim(), "100");

    // Two full rows of blue: tmux captures the style once, on the first.
    host.conn()
        .await
        .session_write(
            "sty",
            "sh",
            "clear; printf '\\033[44m%200s\\033[0m\\r\\n\\033[7mSTATUS   \\033[0m\\r\\n\\033(0lqqk\\033(B\\r\\nstyled-done\\r\\n' ''",
            true,
        )
        .await
        .unwrap();
    wait_output(&host, "sty", "sh", "styled-done").await;
    let (mut reader, _writer) = stream_attach(&host, "sty", "sh").await;
    let snapshot = read_until(&mut reader, "\x1b[?2026l").await;
    let phase2 = snapshot.split("\x1bc").nth(1).unwrap_or_default();
    let blank = " ".repeat(100);
    assert!(
        phase2.contains(&format!("\x1b[0m\r\n\x1b[44m{blank}")),
        "the second row is blue too: {phase2:?}"
    );
    // Styled blanks at the end of a row stay.
    assert!(phase2.contains("STATUS   \x1b["), "{phase2:?}");
    // Line drawing, which tmux marks with SO and SI.
    assert!(phase2.starts_with("\x1b[?2026h\x1b)0"), "{phase2:?}");
    assert!(phase2.contains("\x0elqqk"), "{phase2:?}");
}

#[tokio::test]
async fn stream_attach_that_falls_behind_gets_a_fresh_snapshot() {
    let host = TestHost::new();
    sh_session(&host, "fb").await;
    let (mut reader, mut writer) = stream_attach(&host, "fb", "sh").await;
    read_until(&mut reader, "\x1b[?2026l").await;
    // About 7 MB, while the client reads nothing.
    send(&mut writer, "seq 1 1000000; echo flood-$((2+3))-done\r").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let out = read_until(&mut reader, "flood-5-done").await;
    assert!(out.contains("\x18\x1bc"), "a fresh snapshot came");
    assert!(out.len() < 3 << 20, "not the backlog: {} bytes", out.len());
    // And after it, the program's output as usual.
    send(&mut writer, "echo after-$((4+4))\r").await;
    read_until(&mut reader, "after-8").await;
}

#[tokio::test]
async fn stream_attach_sends_home_and_end_as_a_terminal_client_would() {
    let host = TestHost::new();
    create(
        &host,
        "ke",
        Some(vec![spec(
            SessionKind::Terminal,
            "cat",
            "stty raw -echo; exec cat -v",
        )]),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut reader, mut writer) = stream_attach(&host, "ke", "cat").await;
    read_until(&mut reader, "\x1b[?2026l").await;
    // What xterm sends; the session's terminal (tmux-256color) says `\e[1~`.
    for key in ["\x1b[H", "\x1b[F", "\x1bOH", "done"] {
        send(&mut writer, key).await;
    }
    let out = read_until(&mut reader, "done").await;
    assert!(out.contains("^[[1~^[[4~^[[1~done"), "{out:?}");
}

#[tokio::test]
async fn stream_attach_off_screen_gets_nothing_and_catches_up_on_return() {
    let host = TestHost::new();
    sh_session(&host, "vw").await;
    let (mut reader, mut writer) = controlled_attach(&host, "vw", "sh", (100, 30), 1000).await;
    read_until_control(&mut reader, snapshot_done).await;
    let view = |visible| {
        Frame::Control(Control::View {
            visible,
            cols: 100,
            rows: 30,
        })
    };

    // What's already on its way still comes after going off screen: wait
    // for the shell to be quiet first (its prompt).
    quiet_for(&mut reader, Duration::from_millis(300)).await;

    // Off screen and back, nothing written meanwhile: no snapshot, just on.
    writer.send(&view(false)).await.unwrap();
    writer.send(&view(true)).await.unwrap();
    send(&mut writer, "echo plain-$((1+1))\r").await;
    let out = read_until(&mut reader, "plain-2").await;
    assert!(!out.contains("\x1bc"), "{out:?}");
    quiet_for(&mut reader, Duration::from_millis(300)).await;

    // Off screen while the program writes: it runs on, the client gets none.
    writer.send(&view(false)).await.unwrap();
    host.conn()
        .await
        .session_write("vw", "sh", "echo while-away-$((2+2))", true)
        .await
        .unwrap();
    wait_output(&host, "vw", "sh", "while-away-4").await;
    assert!(
        quiet_for(&mut reader, Duration::from_millis(500))
            .await
            .is_empty()
    );
    // Back on screen: what was missed comes, without a snapshot, so the
    // client's history stays as it is (D-160).
    writer.send(&view(true)).await.unwrap();
    let back = read_until(&mut reader, "while-away-4").await;
    assert!(!back.contains("\x1bc"), "{back:?}");
    quiet_for(&mut reader, Duration::from_millis(300)).await;

    // More than is kept for it: back on screen, a snapshot instead.
    writer.send(&view(false)).await.unwrap();
    host.conn()
        .await
        .session_write(
            "vw",
            "sh",
            "head -c 3000000 /dev/zero | tr '\\0' x; echo; echo much-$((3+3))",
            true,
        )
        .await
        .unwrap();
    wait_output(&host, "vw", "sh", "much-6").await;
    // The pane has it; give the daemon a moment to have read past the limit.
    tokio::time::sleep(Duration::from_millis(500)).await;
    writer.send(&view(true)).await.unwrap();
    let (back, _) = read_until_control(&mut reader, snapshot_done).await;
    assert!(
        back.contains("\x18\x1bc") && back.contains("much-6"),
        "{}",
        back.len()
    );
}

#[tokio::test]
async fn stream_attach_hears_when_another_client_sizes_the_window() {
    let host = TestHost::new();
    sh_session(&host, "sz").await;
    let (mut reader, _writer) = controlled_attach(&host, "sz", "sh", (100, 30), 100).await;
    read_until_control(&mut reader, snapshot_done).await;

    // A terminal client (as `otter attach`) at 80×24 that types last.
    let (_, mut other, mut other_writer) = host
        .conn()
        .await
        .attach(SessionAttach {
            session: SessionRef {
                workspace: "sz".into(),
                session: "sh".into(),
            },
            cols: 80,
            rows: 24,
            term: Some("xterm-256color".into()),
            history: None,
            controls: false,
            own_terminal: false,
        })
        .await
        .unwrap();
    send(&mut other_writer, " ").await;
    let (_, told) = read_until_control(&mut reader, |c| matches!(c, Control::Window { .. })).await;
    assert_eq!(
        told,
        Control::Window {
            size: Some([80, 24])
        }
    );

    // It leaves: the window is the stream client's again.
    other_writer.send(&Frame::Detach).await.unwrap();
    read_exit(&mut other).await;
    let (_, told) = read_until_control(&mut reader, |c| matches!(c, Control::Window { .. })).await;
    assert_eq!(told, Control::Window { size: None });
}

#[tokio::test]
async fn stream_attach_restores_the_cursor_the_program_set() {
    let host = TestHost::new();
    sh_session(&host, "cur").await;
    let exec = backend_ref(&host.conn().await.workspace_get("cur").await.unwrap());
    let shape = host.tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={exec}:"),
        "#{cursor_shape}",
    ]);
    if String::from_utf8_lossy(&shape.stdout).trim().is_empty() {
        eprintln!("skipped: this tmux has no cursor formats (3.4+)");
        return;
    }
    host.conn()
        .await
        .session_write(
            "cur",
            "sh",
            "printf '\\033[4 q\\033]12;#ff8800\\007'; echo cursor-set",
            true,
        )
        .await
        .unwrap();
    wait_output(&host, "cur", "sh", "cursor-set").await;
    let (mut reader, _writer) = controlled_attach(&host, "cur", "sh", (100, 30), 100).await;
    let (snapshot, _) = read_until_control(&mut reader, snapshot_done).await;
    assert!(
        snapshot.contains("\x1b[4 q"),
        "steady underline: {snapshot:?}"
    );
    assert!(snapshot.contains("\x1b]12;#ff8800\x07"), "{snapshot:?}");
}

#[tokio::test]
async fn stream_attach_brings_earlier_history_when_asked() {
    let host = TestHost::new();
    sh_session(&host, "hi").await;
    host.conn()
        .await
        .session_write(
            "hi",
            "sh",
            "i=0; while [ $i -lt 3000 ]; do i=$((i+1)); echo row-$i; done",
            true,
        )
        .await
        .unwrap();
    wait_output(&host, "hi", "sh", "row-3000").await;
    let (mut reader, mut writer) = controlled_attach(&host, "hi", "sh", (100, 30), 500).await;
    let (first, done) = read_until_control(&mut reader, snapshot_done).await;
    assert!(!first.contains("row-100\x1b"), "not asked for yet");
    assert!(
        matches!(done, Control::Snapshot { more: true, .. }),
        "{done:?}"
    );

    writer
        .send(&Frame::Control(Control::History { lines: 5000 }))
        .await
        .unwrap();
    let (more, done) = read_until_control(&mut reader, snapshot_done).await;
    assert!(more.contains("row-100\x1b[0m\r\n"), "earlier rows came");
    assert!(
        matches!(done, Control::Snapshot { more: false, .. }),
        "{done:?}"
    );
}

#[tokio::test]
async fn a_stream_to_the_users_own_terminal_keeps_queries_out_of_it() {
    let host = TestHost::new();
    sh_session(&host, "own").await;
    let attach = |own_terminal| {
        let host = &host;
        async move {
            let (ready, reader, writer) = host
                .conn()
                .await
                .attach(SessionAttach {
                    session: SessionRef {
                        workspace: "own".into(),
                        session: "sh".into(),
                    },
                    cols: 100,
                    rows: 30,
                    term: Some("xterm-256color".into()),
                    history: Some(100),
                    controls: false,
                    own_terminal,
                })
                .await
                .unwrap();
            assert!(ready.history);
            (reader, writer)
        }
    };
    let (mut own, _own_writer) = attach(true).await;
    let (mut app, _app_writer) = attach(false).await;
    let first = read_until(&mut own, "\x1b[?2026l").await;
    assert!(
        !first.contains("\x1bc"),
        "the user's terminal isn't reset: {first:?}"
    );
    read_until(&mut app, "\x1b[?2026l").await;

    // The program asks its terminal things, then says it's done.
    host.conn()
        .await
        .session_write(
            "own",
            "sh",
            "printf 'a\\033[cb\\033[6nc\\033]11;?\\007d\\033[31me\\n'; echo asked-$((3+4))",
            true,
        )
        .await
        .unwrap();
    let own_out = read_until(&mut own, "asked-7").await;
    let app_out = read_until(&mut app, "asked-7").await;
    for query in ["\x1b[c", "\x1b[6n", "\x1b]11;?"] {
        assert!(
            !own_out.contains(query),
            "{query:?} reached it: {own_out:?}"
        );
        assert!(
            app_out.contains(query),
            "{query:?} goes to the app: {app_out:?}"
        );
    }
    assert!(
        own_out.contains("abcd\x1b[31me"),
        "the rest is as it was: {own_out:?}"
    );
}

#[tokio::test]
async fn stream_attach_ends_with_the_process_and_leaves_it_on_disconnect() {
    let host = TestHost::new();
    sh_session(&host, "se").await;
    let ws = host.conn().await.workspace_get("se").await.unwrap();
    {
        let (mut reader, mut writer) = stream_attach(&host, "se", "sh").await;
        send(&mut writer, "echo before-$((1+1))-drop\r").await;
        read_until(&mut reader, "before-2-drop").await;
        // Dropped without detaching, like a Mac going to sleep.
    }
    eventually("stream client gone", || async {
        let out = host.tmux(&["list-clients", "-F", "#{client_name}"]);
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .is_empty()
            .then_some(())
    })
    .await;
    assert!(host.tmux_has_session(&backend_ref(&ws)));
    // Nothing was typed into the session on the way out.
    let (mut reader, mut writer) = stream_attach(&host, "se", "sh").await;
    send(&mut writer, "echo still-$((2+3))\r").await;
    read_until(&mut reader, "still-5").await;

    send(&mut writer, "exit 5\r").await;
    let exit = read_exit(&mut reader).await;
    assert_eq!(exit.reason, AttachExitReason::Exited);
    assert_eq!(exit.exit_code, Some(5));
}

#[tokio::test]
async fn attach_ends_cleanly_when_the_session_restarts_or_is_deleted() {
    let host = TestHost::new();
    sh_session(&host, "end").await;

    let (mut reader, _writer) = attach(&host, "end", "sh").await;
    host.conn()
        .await
        .session_restart("end", "sh")
        .await
        .unwrap();
    assert_eq!(read_exit(&mut reader).await.reason, AttachExitReason::Ended);

    // The restarted session (a new execution) can be attached again.
    let (mut reader, mut writer) = attach(&host, "end", "sh").await;
    send(&mut writer, "echo again-$((5+5))\r").await;
    read_until(&mut reader, "again-10").await;

    host.conn()
        .await
        .workspace_delete("end", true)
        .await
        .unwrap();
    assert_eq!(read_exit(&mut reader).await.reason, AttachExitReason::Ended);
    eventually("no attach clients left", || async {
        (attach_clients(&host) == 0).then_some(())
    })
    .await;
}

#[tokio::test]
async fn attach_is_disposable_across_daemon_restart() {
    let host = TestHost::new();
    sh_session(&host, "dr").await;
    let exec = host
        .conn()
        .await
        .workspace_get("dr")
        .await
        .unwrap()
        .sessions[0]
        .current_execution()
        .unwrap()
        .id
        .clone();
    let (mut reader, mut writer) = attach(&host, "dr", "sh").await;
    send(&mut writer, "echo first-$((4*4))\r").await;
    read_until(&mut reader, "first-16").await;

    // The daemon goes away mid-attach: the attach ends, the session doesn't.
    host.stop_daemon().await;
    let end = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match reader.next().await {
                Ok(Some(Frame::Data(_))) => continue,
                other => return other.map(|f| f.is_none()).unwrap_or(true),
            }
        }
    })
    .await
    .expect("attach noticed the daemon going away");
    assert!(end, "connection should close");

    // A new daemon adopts the same, still running, execution (nothing was
    // typed into it on the way out) and it can be attached again with the
    // earlier output still on screen.
    let ws = host.conn().await.workspace_get("dr").await.unwrap();
    let now = ws.sessions[0].current_execution().unwrap();
    assert_eq!((&now.id, now.state), (&exec, ExecutionState::Running));
    let (mut reader, mut writer) = attach(&host, "dr", "sh").await;
    read_until(&mut reader, "first-16").await;
    send(&mut writer, "echo second-$((5*5))\r").await;
    read_until(&mut reader, "second-25").await;
    writer.send(&Frame::Detach).await.unwrap();
    assert_eq!(
        read_exit(&mut reader).await.reason,
        AttachExitReason::Detached
    );
    eventually("no attach clients left", || async {
        (attach_clients(&host) == 0).then_some(())
    })
    .await;
}

#[tokio::test]
async fn daemon_restart_adopts_running_sessions_and_records_changes() {
    let host = TestHost::new();
    let ws = create(
        &host,
        "persist",
        Some(vec![
            spec(SessionKind::Service, "keep", "sleep 600"),
            spec(SessionKind::Service, "vanish", "sleep 600"),
            spec(SessionKind::Command, "finish", "sleep 1; exit 7"),
        ]),
    )
    .await;
    let keep = ws
        .session("keep")
        .unwrap()
        .current_execution()
        .unwrap()
        .clone();
    let vanish = ws
        .session("vanish")
        .unwrap()
        .current_execution()
        .unwrap()
        .clone();
    let old_pid = host.daemon_pid().unwrap();

    host.stop_daemon().await;
    // While the daemon is down: one process disappears, one exits.
    assert!(
        host.tmux(&["kill-session", "-t", &format!("={}", vanish.backend_ref)])
            .status
            .success()
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(host.tmux_has_session(&keep.backend_ref));

    // Next client connection starts a fresh daemon, which reconciles.
    let ws = host.conn().await.workspace_get("persist").await.unwrap();
    assert_ne!(host.daemon_pid().unwrap(), old_pid);
    let keep_now = ws.session("keep").unwrap().current_execution().unwrap();
    assert_eq!(keep_now.id, keep.id);
    assert_eq!(keep_now.state, ExecutionState::Running);
    assert_eq!(ws.session("vanish").unwrap().status(), SessionStatus::Lost);
    let finish = ws.session("finish").unwrap().current_execution().unwrap();
    assert_eq!(finish.state, ExecutionState::Exited);
    assert_eq!(finish.exit_code, Some(7));
}

#[tokio::test]
async fn event_stream_reports_lifecycle() {
    let host = TestHost::new();
    create(&host, "ev", Some(vec![])).await;
    let mut stream = host.conn().await.subscribe(None).await.unwrap();
    host.conn()
        .await
        .session_create(SessionCreate {
            workspace: "ev".into(),
            spec: spec(SessionKind::Command, "t", "exit 0"),
        })
        .await
        .unwrap();
    let mut kinds = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(rec) = stream.next().await.unwrap() {
            kinds.push(rec.event.kind());
            if rec.event.kind() == "ExecutionExited" {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("events so far: {kinds:?}"));
    assert_eq!(
        kinds,
        ["SessionCreated", "ExecutionStarted", "ExecutionExited"]
    );
}

/// Events from `stream` up to and including `seq`.
async fn events_through(
    stream: &mut otter_client::EventStream,
    seq: u64,
) -> Vec<otter_protocol::EventRecord> {
    let mut out: Vec<otter_protocol::EventRecord> = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while out.last().is_none_or(|r| r.seq < seq) {
            out.push(stream.next().await.unwrap().expect("stream open"));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("waiting for event {seq}; got {out:?}"));
    out
}

async fn command(host: &TestHost, ws: &str, name: &str) {
    host.conn()
        .await
        .session_create(SessionCreate {
            workspace: ws.into(),
            spec: spec(SessionKind::Command, name, "exit 0"),
        })
        .await
        .unwrap();
    wait_status(host, ws, name, SessionStatus::Completed).await;
}

#[tokio::test]
async fn event_replay_resumes_after_disconnect_and_daemon_restart() {
    let host = TestHost::new();
    create(&host, "ev", Some(vec![])).await;
    let snapshot = host.conn().await.snapshot().await.unwrap();
    assert!(snapshot.workspaces.iter().any(|w| w.name == "ev"));
    assert!(snapshot.log_id.is_some());
    // A subscriber that goes away (laptop sleeps).
    let stream = host
        .conn()
        .await
        .subscribe(Some(snapshot.cursor()))
        .await
        .unwrap();
    assert!(stream.seq >= snapshot.seq);
    assert_eq!(stream.cursor(), snapshot.cursor());
    drop(stream);

    // Meanwhile: a command runs, and the daemon restarts.
    command(&host, "ev", "t").await;
    host.stop_daemon().await;

    // Reconnecting with the cursor replays exactly what was missed, in order:
    // the restarted daemon continues the same log.
    let mut stream = host
        .conn()
        .await
        .subscribe(Some(snapshot.cursor()))
        .await
        .unwrap();
    assert_eq!(stream.cursor().log_id, snapshot.log_id);
    let head = stream.seq;
    let missed = events_through(&mut stream, head).await;
    let seqs: Vec<u64> = missed.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, (snapshot.seq + 1..=head).collect::<Vec<_>>());
    let kinds: Vec<&str> = missed.iter().map(|r| r.event.kind()).collect();
    for want in [
        "SessionCreated",
        "ExecutionStarted",
        "ExecutionExited",
        "DaemonStarted",
    ] {
        assert!(kinds.contains(&want), "{want} missing from {kinds:?}");
    }

    // Then it continues live, without a gap.
    command(&host, "ev", "u").await;
    let next = events_through(&mut stream, head + 1).await;
    assert_eq!(next[0].seq, head + 1);
    assert_eq!(next[0].event.kind(), "SessionCreated");
}

#[tokio::test]
async fn snapshot_then_subscribe_has_no_gap_and_stale_cursors_are_rejected() {
    let host = TestHost::new();
    create(&host, "snap", Some(vec![])).await;
    let snapshot = host.conn().await.snapshot().await.unwrap();

    // Following on from a snapshot: the next event is the next seq.
    let mut stream = host
        .conn()
        .await
        .subscribe(Some(snapshot.cursor()))
        .await
        .unwrap();
    command(&host, "snap", "t").await;
    let first = events_through(&mut stream, snapshot.seq + 1).await;
    assert_eq!(first[0].seq, snapshot.seq + 1);

    // A cursor this host never issued (e.g. its log was reset) is refused,
    // not silently treated as "from now".
    let ahead = Cursor {
        seq: snapshot.seq + 1000,
        ..snapshot.cursor()
    };
    assert_cursor_expired(&host, ahead).await;
    // So is one without a log id (as old clients send) that this log can't
    // serve.
    let ahead = Cursor {
        seq: snapshot.seq + 1000,
        log_id: None,
    };
    assert_cursor_expired(&host, ahead).await;
}

async fn assert_cursor_expired(host: &TestHost, cursor: Cursor) {
    match host.conn().await.subscribe(Some(cursor.clone())).await {
        Err(e) if e.is_cursor_expired() => {}
        Err(e) => panic!("unexpected error for {cursor:?}: {e}"),
        Ok(_) => panic!("stale cursor {cursor:?} accepted"),
    }
}

/// Workspaces are a cheap way to produce events.
async fn churn(host: &TestHost, prefix: &str, n: usize) {
    for i in 0..n {
        create(host, &format!("{prefix}{i}"), Some(vec![])).await;
    }
}

#[tokio::test]
async fn rotated_cursors_resync_and_recent_ones_replay() {
    // Tiny segments, so a few dozen events rotate the log several times.
    let host = TestHost::with_env(&[("OTTER_EVENTS_SEGMENT_BYTES", "1024".into())]).await;
    let old = host.conn().await.snapshot().await.unwrap().cursor();
    churn(&host, "a", 30).await;
    let recent = host.conn().await.snapshot().await.unwrap();
    let segments = std::fs::read_dir(host.home().join("state"))
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            let name = name.to_string_lossy();
            name.starts_with("events.") && name.ends_with(".jsonl") && name != "events.jsonl"
        })
        .count();
    assert!((1..=3).contains(&segments), "{segments} rotated segments");

    // A cursor from before the retained log is refused; the client resyncs
    // from a snapshot, which has everything.
    assert_cursor_expired(&host, old.clone()).await;
    assert_cursor_expired(
        &host,
        Cursor {
            log_id: None,
            ..old
        },
    )
    .await;
    assert_eq!(
        recent
            .workspaces
            .iter()
            .filter(|w| w.name.starts_with('a'))
            .count(),
        30
    );

    // A recent cursor still replays exactly what followed it.
    let mut stream = host
        .conn()
        .await
        .subscribe(Some(recent.cursor()))
        .await
        .unwrap();
    create(&host, "b", Some(vec![])).await;
    let next = events_through(&mut stream, recent.seq + 1).await;
    assert_eq!(next[0].seq, recent.seq + 1);
    assert_eq!(next[0].event.kind(), "WorkspaceCreated");
}

#[tokio::test]
async fn a_log_that_started_over_is_detected_even_past_the_old_cursor() {
    let host = TestHost::new();
    create(&host, "before", Some(vec![])).await;
    let old = host.conn().await.snapshot().await.unwrap();
    assert!(old.seq > 0);

    // The host's event log is lost (as if `state/` were restored without it).
    host.stop_daemon().await;
    for entry in std::fs::read_dir(host.home().join("state")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("events.") {
            std::fs::remove_file(path).unwrap();
        }
    }

    // The new log grows past the old cursor; by `seq` alone it would look
    // valid and the client would silently miss everything up to it.
    churn(&host, "after", old.seq as usize + 1).await;
    let now = host.conn().await.snapshot().await.unwrap();
    assert!(now.seq > old.seq, "{} <= {}", now.seq, old.seq);
    assert_ne!(now.log_id, old.log_id);
    assert_cursor_expired(&host, old.cursor()).await;

    // Resyncing from a fresh snapshot follows on normally.
    let mut stream = host
        .conn()
        .await
        .subscribe(Some(now.cursor()))
        .await
        .unwrap();
    create(&host, "later", Some(vec![])).await;
    let next = events_through(&mut stream, now.seq + 1).await;
    assert_eq!(next[0].seq, now.seq + 1);
}

// ---------------------------------------------------------------------------
// Git, directories and environments (phases 5 and 6)
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repository with one commit on `main` containing `files`.
fn make_repo(host: &TestHost, name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = host.home().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    for (path, content) in files {
        std::fs::write(dir.join(path), content).unwrap();
    }
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    dir
}

fn git_ws(
    name: &str,
    repo: &Path,
    branch: Option<&str>,
    sessions: Vec<SessionSpec>,
) -> WorkspaceCreate {
    WorkspaceCreate {
        name: name.into(),
        source: SourceSpec::Git {
            repository: repo.to_string_lossy().into_owned(),
            branch: branch.map(Into::into),
            base: None,
        },
        sessions: Some(sessions),
        ..Default::default()
    }
}

async fn wait_prepared(host: &TestHost, name: &str) -> Workspace {
    eventually(&format!("{name} finishes preparing"), || async {
        let w = host.conn().await.workspace_get(name).await.unwrap();
        (w.state != WorkspaceState::Preparing).then_some(w)
    })
    .await
}

fn git_source(ws: &Workspace) -> &otter_core::GitSource {
    match &ws.source {
        WorkspaceSource::Git(g) => g,
        other => panic!("not a git workspace: {other:?}"),
    }
}

#[tokio::test]
async fn git_workspaces_share_one_backing_repository() {
    let host = TestHost::new();
    let src = make_repo(&host, "src", &[("README.md", "hello\n")]);
    let head = git(&src, &["rev-parse", "HEAD"]).trim().to_owned();
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("task-a", &src, None, vec![]))
        .await
        .unwrap();
    conn.workspace_create(git_ws("task-b", &src, None, vec![]))
        .await
        .unwrap();
    let a = wait_prepared(&host, "task-a").await;
    let b = wait_prepared(&host, "task-b").await;
    assert_eq!(a.state, WorkspaceState::Ready, "{:?}", a.state_message);
    assert_eq!(b.state, WorkspaceState::Ready, "{:?}", b.state_message);

    let ga = git_source(&a);
    assert_eq!(ga.branch, "otterd/task-a");
    assert_eq!(ga.base.as_deref(), Some("origin/main"));
    assert_eq!(ga.base_commit.as_deref(), Some(head.as_str()));
    assert!(Path::new(&a.root).join("README.md").is_file());
    assert!(a.root.ends_with(&format!("workspaces/{}/repo", a.id)));

    // One backing repository, two worktrees of it.
    let repos: Vec<_> = std::fs::read_dir(host.home().join("repos"))
        .unwrap()
        .collect();
    assert_eq!(repos.len(), 1);
    let base = host.home().join("repos").join(&ga.repo_id).join("base");
    let worktrees = git(&base, &["worktree", "list", "--porcelain"]);
    assert!(
        worktrees.contains(&a.root) && worktrees.contains(&b.root),
        "{worktrees}"
    );

    // Uncommitted work is protected unless forced; then the worktree and its
    // branch go, the shared copy stays for task-b, and goes with it (D-108).
    std::fs::write(Path::new(&a.root).join("wip.txt"), "wip").unwrap();
    let err = conn.workspace_delete("task-a", false).await.unwrap_err();
    assert!(err.to_string().contains("uncommitted"), "{err}");
    conn.workspace_delete("task-a", true).await.unwrap();
    assert!(!Path::new(&a.root).exists());
    let branches = git(&base, &["branch", "--list", "otterd/*"]);
    assert!(
        !branches.contains("otterd/task-a") && branches.contains("otterd/task-b"),
        "{branches}"
    );
    conn.workspace_delete("task-b", false).await.unwrap();
    assert!(!Path::new(&b.root).exists() && !base.exists());
}

#[tokio::test]
async fn git_workspace_on_existing_branch_and_failures() {
    let host = TestHost::new();
    let src = make_repo(&host, "src", &[("README.md", "hello\n")]);
    git(&src, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(src.join("topic.txt"), "topic work").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "topic"]);
    git(&src, &["checkout", "-q", "main"]);

    let mut conn = host.conn().await;
    conn.workspace_create(git_ws(
        "topic",
        &src,
        Some("topic"),
        vec![SessionSpec::shell()],
    ))
    .await
    .unwrap();
    let ws = wait_prepared(&host, "topic").await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    assert_eq!(git_source(&ws).branch, "topic");
    assert!(Path::new(&ws.root).join("topic.txt").is_file());
    // Sessions start once the workspace is ready.
    let ws = wait_status(&host, "topic", "shell", SessionStatus::Running).await;
    assert_eq!(ws.sessions.len(), 1);

    conn.workspace_create(git_ws(
        "broken",
        Path::new("/nonexistent/repo"),
        None,
        vec![SessionSpec::shell()],
    ))
    .await
    .unwrap();
    let ws = wait_prepared(&host, "broken").await;
    assert_eq!(ws.state, WorkspaceState::Failed);
    assert!(
        ws.state_message.as_deref().unwrap_or("").contains("clone"),
        "{:?}",
        ws.state_message
    );
    assert_eq!(ws.sessions[0].status(), SessionStatus::Pending);
    // A retry is announced, so a client drops the old failure at once.
    let mut stream = host.conn().await.subscribe(None).await.unwrap();
    let ws = conn.workspace_prepare("broken").await.unwrap();
    let ws = match ws.state {
        WorkspaceState::Preparing => wait_prepared(&host, "broken").await,
        _ => ws,
    };
    assert_eq!(ws.state, WorkspaceState::Failed);
    let mut kinds = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(rec) = stream.next().await.unwrap() {
            kinds.push(rec.event.kind());
            if rec.event.kind() == "WorkspaceFailed" {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("events so far: {kinds:?}"));
    assert_eq!(
        kinds.first().copied(),
        Some("WorkspacePreparing"),
        "{kinds:?}"
    );
    conn.workspace_delete("broken", false).await.unwrap();
}

#[tokio::test]
async fn existing_directory_workspace_is_attached_not_managed() {
    let host = TestHost::new();
    let dir = host.home().join("mine");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep.txt"), "precious").unwrap();
    let mut conn = host.conn().await;
    let ws = conn
        .workspace_create(WorkspaceCreate {
            name: "mine".into(),
            source: SourceSpec::Directory {
                path: dir.to_string_lossy().into_owned(),
            },
            sessions: Some(vec![spec(SessionKind::Command, "where", "pwd")]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ws.root, dir.to_string_lossy());
    wait_output(&host, "mine", "where", &dir.to_string_lossy()).await;
    conn.workspace_delete("mine", false).await.unwrap();
    assert!(
        dir.join("keep.txt").is_file(),
        "attached directory must survive deletion"
    );
}

#[tokio::test]
async fn direnv_environment_is_resolved_and_inherited() {
    if Command::new("direnv").arg("version").output().is_err() {
        eprintln!("skipping: direnv not installed");
        return;
    }
    // Keep direnv's allow-list out of the real user's data directory.
    let scratch = tempfile::Builder::new().prefix("wdx").tempdir().unwrap();
    let host = TestHost::with_env(&[
        (
            "XDG_DATA_HOME",
            scratch.path().join("data").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CONFIG_HOME",
            scratch.path().join("config").to_string_lossy().into_owned(),
        ),
    ])
    .await;
    let src = make_repo(
        &host,
        "src",
        &[(".envrc", "export OTTER_TEST_VAR=from-direnv\n")],
    );
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws(
        "env",
        &src,
        None,
        vec![spec(
            SessionKind::Command,
            "show",
            "echo var=$OTTER_TEST_VAR",
        )],
    ))
    .await
    .unwrap();
    let ws = wait_prepared(&host, "env").await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    assert_eq!(ws.environment.kind, EnvironmentKind::Direnv);
    assert_eq!(ws.environment.status, EnvironmentStatus::Ready);
    wait_output(&host, "env", "show", "var=from-direnv").await;
    // Allowed in the scratch XDG dir, not the user's.
    assert!(scratch.path().join("data/direnv/allow").is_dir());

    // A broken .envrc fails the workspace visibly instead of starting
    // sessions in the wrong environment.
    let bad = make_repo(&host, "bad", &[(".envrc", "echo boom >&2; exit 3\n")]);
    conn.workspace_create(git_ws("bad-env", &bad, None, vec![SessionSpec::shell()]))
        .await
        .unwrap();
    let ws = wait_prepared(&host, "bad-env").await;
    assert_eq!(ws.state, WorkspaceState::Failed);
    assert_eq!(ws.environment.status, EnvironmentStatus::Failed);
    assert!(
        ws.environment
            .message
            .as_deref()
            .unwrap_or("")
            .contains("boom"),
        "{:?}",
        ws.environment
    );
    assert_eq!(ws.sessions[0].status(), SessionStatus::Pending);

    let kinds: Vec<_> = conn
        .events_list(None)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event.kind())
        .collect();
    assert!(
        kinds.contains(&"EnvironmentReady") && kinds.contains(&"EnvironmentFailed"),
        "{kinds:?}"
    );
}

/// Every session starts with Claude Code on its classic renderer, a shell too
/// (where the developer runs `claude` themselves); the host's own setting of
/// the variable wins (D-144).
#[tokio::test]
async fn sessions_start_claude_code_on_its_classic_renderer() {
    let host = TestHost::new();
    sh_session(&host, "tui").await;
    let mut conn = host.conn().await;
    conn.session_write(
        "tui",
        "sh",
        "echo alt=[$CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN]",
        true,
    )
    .await
    .unwrap();
    wait_output(&host, "tui", "sh", "alt=[1]").await;
    // Through the environment, not a command line or the event log.
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(!log.contains("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"));

    let host = TestHost::with_env(&[("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN", "0".into())]).await;
    sh_session(&host, "tui").await;
    let mut conn = host.conn().await;
    conn.session_write(
        "tui",
        "sh",
        "echo alt=[$CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN]",
        true,
    )
    .await
    .unwrap();
    wait_output(&host, "tui", "sh", "alt=[0]").await;
}

/// A session's shell hook reloads direnv when a file it watches changes
/// (a refreshed Nix dev shell), and goes back to the PATH the export started
/// from: otterd's stand-ins (`run/bin`, e.g. the `wl-paste` image paste
/// needs) must still be on it afterwards.
#[tokio::test]
async fn stand_ins_survive_a_direnv_reload_in_a_session() {
    if Command::new("direnv").arg("version").output().is_err() {
        eprintln!("skipping: direnv not installed");
        return;
    }
    let scratch = tempfile::Builder::new().prefix("wdx").tempdir().unwrap();
    let host = TestHost::with_env(&[
        (
            "XDG_DATA_HOME",
            scratch.path().join("data").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CONFIG_HOME",
            scratch.path().join("config").to_string_lossy().into_owned(),
        ),
    ])
    .await;
    let shim = host.home().join("run/bin");
    // What `eval "$(direnv hook bash)"` does at a prompt, after the .envrc's
    // time changed (its content didn't: still allowed).
    let reload = format!(
        "has() {{ case \":$PATH:\" in *\":{shim}:\"*) echo \"$1=kept\" ;; *) echo \"$1=lost\" ;; esac; }}\n\
         has before\n\
         touch -t 203001010000 .envrc\n\
         out=$(direnv export bash 2>/dev/null)\n\
         [ -n \"$out\" ] && echo reloaded\n\
         eval \"$out\"\n\
         has after\n",
        shim = shim.display()
    );
    let src = make_repo(
        &host,
        "src",
        &[(".envrc", "PATH_add tools\n"), ("reload.sh", &reload)],
    );
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws(
        "env",
        &src,
        None,
        vec![spec(SessionKind::Command, "reload", "bash reload.sh")],
    ))
    .await
    .unwrap();
    let ws = wait_prepared(&host, "env").await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    let out = wait_output(&host, "env", "reload", "after=").await;
    assert!(out.contains("before=kept"), "{out}");
    assert!(out.contains("reloaded"), "direnv didn't reload: {out}");
    assert!(out.contains("after=kept"), "{out}");
}

// ---------------------------------------------------------------------------
// Agents and attention (phases 4 and 7)
// ---------------------------------------------------------------------------

/// A stand-in for the Codex TUI: records its arguments, writes a rollout
/// transcript like Codex does (holding it open), and plays a scripted turn.
/// Mode `old` is a Codex before 0.156, which rejects `--no-daemon`.
const FAKE_CODEX: &str = r#"#!/bin/sh
case " $* " in *" --version "*) ;; *) echo "$*" >> "$FAKE_CODEX_LOG" ;; esac
if [ "$1" = --no-daemon ]; then
  if [ "$FAKE_CODEX_MODE" = old ]; then
    echo "error: unexpected argument '--no-daemon' found" >&2
    exit 2
  fi
  shift
fi
if [ "$1" = --version ]; then echo "codex-cli 0.0.0-fake"; exit 0; fi
now() { date -u +%Y-%m-%dT%H:%M:%S.000Z; }
if [ "$1" = resume ]; then
  id="$2"
  f=$(ls "$CODEX_HOME"/sessions/*/*/*/rollout-*-"$id".jsonl | head -n 1)
else
  id=$(cat /proc/sys/kernel/random/uuid 2>/dev/null || uuidgen | tr A-Z a-z)
  d="$CODEX_HOME/sessions/$(date +%Y/%m/%d)"
  mkdir -p "$d"
  f="$d/rollout-$(date +%Y-%m-%dT%H-%M-%S)-$id.jsonl"
  printf '{"timestamp":"%s","type":"session_meta","payload":{"id":"%s","cwd":"%s","timestamp":"%s","originator":"codex-tui"}}\n' "$(now)" "$id" "$(pwd)" "$(now)" > "$f"
fi
exec 3>>"$f"
echo "codex ready"
sleep 1
printf '%s\n' '{"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}' >&3
case "$FAKE_CODEX_MODE" in
  approval)
    # Like the Codex TUI at an approval prompt: the terminal is redrawn
    # every second, but what it shows doesn't change.
    (while :; do printf '\rAllow command? [y/n] '; sleep 0.3; done) &
    redraw=$!
    read answer
    kill $redraw
    ;;
  thinking)
    # Like Codex thinking at length: nothing in the rollout, but the
    # elapsed time ticks on screen.
    for i in 1 2 3 4 5 6 7 8 9 10 11 12; do printf '\rWorking (%ss)' "$i"; sleep 0.5; done
    echo
    ;;
  *)
    for i in 1 2; do echo "working $i"; sleep 0.5; done
    ;;
esac
printf '%s\n' '{"type":"event_msg","payload":{"type":"task_complete","turn_id":"t","last_agent_message":"All   tests\npass."}}' >&3
echo "done"
exec sleep 600
"#;

struct FakeCodex {
    host: TestHost,
    log: std::path::PathBuf,
    /// The fake `gh`'s pull requests and CI (fake_gh.sh).
    gh: std::path::PathBuf,
    _scratch: tempfile::TempDir,
}

/// A login shell for an isolated test user that skips the system profile: on
/// macOS `/etc/profile` runs `path_helper`, which moves system directories
/// (e.g. `/opt/homebrew/bin`, where a real `codex` may live) ahead of `PATH`.
fn isolated_shell(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let shell = dir.join("login-sh");
    std::fs::write(
        &shell,
        "#!/bin/sh\n[ \"$1\" = -l ] && shift\nexec /bin/sh \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    shell.to_string_lossy().into_owned()
}

/// The directory holding the `tmux` the tests use, for isolated `PATH`s (it is
/// not in a system directory on macOS).
fn tmux_dir() -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .find(|d| d.join("tmux").is_file())
        .expect("tmux on PATH")
        .to_string_lossy()
        .into_owned()
}

/// A stand-in for Claude Code: records its arguments, writes a transcript
/// where Claude Code does (one JSON record per line, appended as the turn
/// goes), calls the hooks Otter passes with `--settings` the way Claude Code
/// 2.1.295 does (event JSON on stdin), and plays a scripted turn.
/// `--resume <id>` continues that file. Modes: `approval` / `question` stop
/// at a permission prompt / AskUserQuestion until a line is typed;
/// `twoprompts` asks again, for another command, right after the first is
/// answered; `nohooks` ignores the settings (hooks disabled) and stops at a
/// prompt; `countdown` stops at a prompt that redraws once a second (Claude
/// Code's auto-deny countdown) and, once answered, runs the tool for ~5 s
/// behind a spinner that redraws ten times a second.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = --version ]; then echo "2.0.0-fake (Claude Code)"; exit 0; fi
# Managed runs (`claude -p`, stream-json): fake_claude_stream.sh.
if [ "$1" = -p ]; then exec sh "$FAKE_CLAUDE_STREAM" "$@"; fi
hook_cmd=
if [ "$1" = --settings ]; then
  hook_cmd=$(sed -n 's/^ *"command": "\(.*\)",$/\1/p' "$2" | head -n 1)
  shift 2
fi
[ "$FAKE_CODEX_MODE" = nohooks ] && hook_cmd=
echo "claude $*" >> "$FAKE_CODEX_LOG"
# Whether Settings signed it in (D-142), never the token: a log of its own.
if [ -n "$CLAUDE_CODE_OAUTH_TOKEN" ]; then t=set; else t=unset; fi
echo "claude $* (oauth token: $t)" >> "$FAKE_CODEX_LOG.auth"
dir="$CLAUDE_CONFIG_DIR/projects/$(pwd -P | sed 's/[^A-Za-z0-9]/-/g')"
mkdir -p "$dir"
if [ "$1" = --resume ]; then
  id="$2"
else
  id=$(cat /proc/sys/kernel/random/uuid 2>/dev/null || uuidgen | tr A-Z a-z)
fi
f="$dir/$id.jsonl"
now() { date -u +%Y-%m-%dT%H:%M:%S.000Z; }
rec() { printf '%s\n' "$1" >> "$f"; }
hook() {
  [ -n "$hook_cmd" ] || return 0
  printf '{"session_id":"%s","transcript_path":"%s","cwd":"%s","permission_mode":"default","hook_event_name":"%s"%s}' \
    "$id" "$f" "$(pwd)" "$1" "$2" | sh -c "$hook_cmd"
}
hook SessionStart ',"source":"startup"'
echo "claude ready"
hook UserPromptSubmit ',"prompt":"go"'
rec '{"type":"user","timestamp":"'$(now)'","sessionId":"'$id'","message":{"role":"user","content":"go"}}'
case "$FAKE_CODEX_MODE" in
  question)
    rec '{"type":"assistant","timestamp":"'$(now)'","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"AskUserQuestion"}]}}'
    hook PreToolUse ',"tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Name the file a or b?","options":[]}]}'
    hook PermissionRequest ',"tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Name the file a or b?","options":[]}]}'
    echo "Name the file a or b?"
    read answer
    hook PostToolUse ',"tool_name":"AskUserQuestion","tool_response":{}'
    ;;
  approval | nohooks)
    rec '{"type":"assistant","timestamp":"'$(now)'","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}'
    hook PreToolUse ',"tool_name":"Bash","tool_input":{"command":"touch z","description":"Create empty file z"}'
    hook PermissionRequest ',"tool_name":"Bash","tool_input":{"command":"touch z","description":"Create empty file z"}'
    echo "Do you want to proceed?"
    read answer
    hook PostToolUse ',"tool_name":"Bash","tool_response":{"stdout":""}'
    ;;
  countdown)
    rec '{"type":"assistant","timestamp":"'$(now)'","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}'
    hook PermissionRequest ',"tool_name":"Bash","tool_input":{"command":"make test","description":"Run the tests"}'
    echo "Do you want to proceed?"
    ( n=59; while :; do printf '\rautomatically deny this request in 1:%02d ' "$n"; n=$(( (n + 59) % 60 )); sleep 1; done ) &
    ticking=$!
    read answer
    kill $ticking
    i=0
    while [ $i -lt 50 ]; do printf '\rrunning %s ' "$i"; i=$((i + 1)); sleep 0.1; done
    echo
    hook PostToolUse ',"tool_name":"Bash","tool_response":{"stdout":""}'
    ;;
  twoprompts)
    rec '{"type":"assistant","timestamp":"'$(now)'","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}'
    hook PreToolUse ',"tool_name":"Bash","tool_input":{"command":"touch z","description":"Create empty file z"}'
    hook PermissionRequest ',"tool_name":"Bash","tool_input":{"command":"touch z","description":"Create empty file z"}'
    echo "Do you want to proceed?"
    read answer
    hook PostToolUse ',"tool_name":"Bash","tool_response":{"stdout":""}'
    hook PreToolUse ',"tool_name":"Bash","tool_input":{"command":"rm y","description":"Remove file y"}'
    hook PermissionRequest ',"tool_name":"Bash","tool_input":{"command":"rm y","description":"Remove file y"}'
    echo "Do you want to proceed again?"
    read answer
    hook PostToolUse ',"tool_name":"Bash","tool_response":{"stdout":""}'
    ;;
  *)
    rec '{"type":"assistant","timestamp":"'$(now)'","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash"}]}}'
    for i in 1 2; do echo "working $i"; sleep 0.5; done
    hook PostToolUse ',"tool_name":"Bash","tool_response":{"stdout":"ok"}'
    ;;
esac
rec '{"type":"user","timestamp":"'$(now)'","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}'
rec '{"type":"assistant","timestamp":"'$(now)'","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"All   tests\npass."}]}}'
hook Stop ',"stop_hook_active":false'
echo "done"
exec sleep 600
"#;

async fn fake_codex_host(mode: &str) -> FakeCodex {
    fake_agent_host(mode, "rules").await
}

/// A host with fake agents, and the given Control Agent brain for tasks
/// (`rules` or `yes`; never a real model in tests).
async fn fake_agent_host(mode: &str, controller: &str) -> FakeCodex {
    fake_agent_host_with(mode, controller, |_| vec![]).await
}

/// Like [`fake_agent_host`], with more variables (given the scratch dir).
/// The tool levels most tests run with (D-191): moving, publishing,
/// approving plans and accepting stay the developer's, so a test's flow is
/// the one it was written for.
pub(crate) const HARNESS_LEVELS: &str =
    "set_workspace=developer,publish=developer,approve_plan=developer,accept_task=developer";

/// Set Otter's tools' levels on `host` (D-191), through Settings.
pub(crate) async fn set_levels(host: &TestHost, levels: &[(&str, &str)]) {
    let update = otter_protocol::host::SettingsUpdate {
        tool_levels: levels
            .iter()
            .map(|(n, l)| {
                (
                    n.to_string(),
                    Some(otter_core::tools::ToolLevel::parse(l).expect("a level")),
                )
            })
            .collect(),
        ..Default::default()
    };
    host.conn().await.settings_set(update).await.unwrap();
}

async fn fake_agent_host_with(
    mode: &str,
    controller: &str,
    extra: impl FnOnce(&Path) -> Vec<(&'static str, String)>,
) -> FakeCodex {
    let scratch = tempfile::Builder::new().prefix("wdc").tempdir().unwrap();
    let bin = scratch.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let codex = bin.join("codex");
    std::fs::write(&codex, FAKE_CODEX).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let stream = scratch.path().join("fake_claude_stream.sh");
    std::fs::write(&stream, include_str!("fake_claude_stream.sh")).unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, include_str!("fake_gh.sh")).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let gh_dir = scratch.path().join("gh");
    std::fs::create_dir_all(&gh_dir).unwrap();
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let log = scratch.path().join("codex-args.log");
    let p = |x: &Path| x.to_string_lossy().into_owned();
    let vars = vec![
        // The fake `claude -p` (fake_claude_stream.sh), unless a test picks
        // the SDK worker (`fake_sdk_host`).
        ("OTTER_CODING_BACKEND", "legacy_cli".into()),
        // An isolated user: no dotfiles that could put the real codex first.
        ("HOME", p(&home)),
        ("SHELL", isolated_shell(scratch.path())),
        (
            "PATH",
            format!("{}:{}:/usr/local/bin:/usr/bin:/bin", p(&bin), tmux_dir()),
        ),
        ("CODEX_HOME", p(&scratch.path().join("codex-home"))),
        (
            "CLAUDE_CONFIG_DIR",
            p(&scratch.path().join("claude-config")),
        ),
        ("FAKE_CODEX_LOG", p(&log)),
        ("FAKE_CODEX_MODE", mode.into()),
        ("FAKE_CLAUDE_STREAM", p(&stream)),
        ("FAKE_GH_DIR", p(&gh_dir)),
        ("OTTER_CI_POLL_MS", "0".into()),
        ("OTTER_CI_SETTLE_MS", "0".into()),
        // Tasks: the deterministic controller (no model calls in tests).
        ("OTTER_CONTROLLER", controller.into()),
        ("OTTER_CONTROLLER_TICK_MS", "200".into()),
        // A Nix dev shell's SDK paths make macOS's /usr/bin/make look for a
        // make that isn't there (tasks check their work with `make test`).
        ("DEVELOPER_DIR", String::new()),
        ("SDKROOT", String::new()),
        ("OTTER_AGENT_QUIET_SECS", "2".into()),
        // Planning starts without the agent's look at the code, unless a
        // test asks for it (D-064).
        ("OTTER_PLAN_TURN", "0".into()),
        // What the controller does on its own when these are automatic
        // (D-191, D-193) is the developer's in these tests, as before;
        // the tests of Otter acting on its own set it themselves.
        ("OTTER_TOOL_LEVELS", HARNESS_LEVELS.into()),
    ];
    let more = extra(scratch.path());
    let vars: Vec<(&str, String)> = vars.into_iter().chain(more).collect();
    let host = TestHost::with_env(&vars).await;
    FakeCodex {
        host,
        log,
        gh: gh_dir,
        _scratch: scratch,
    }
}

async fn wait_agent(
    host: &TestHost,
    ws: &str,
    session: &str,
    want: otter_core::AgentState,
) -> Workspace {
    eventually(
        &format!("{ws}/{session} agent becomes {want:?}"),
        || async {
            let w = host.conn().await.workspace_get(ws).await.unwrap();
            let state = w.session(session).unwrap().agent.as_ref().unwrap().state;
            (state == want).then_some(w)
        },
    )
    .await
}

#[tokio::test]
async fn codex_session_is_observed_resumed_and_needs_you_when_done() {
    use otter_core::{Activity, AgentState, AttentionKind};
    let fake = fake_codex_host("normal").await;
    let host = &fake.host;
    // The host advertises which agent providers it can run.
    let status = host.conn().await.host_status().await.unwrap();
    let codex = status
        .agents
        .iter()
        .find(|a| a.provider == "codex")
        .unwrap();
    assert!(codex.available && codex.can_resume, "{:?}", status.agents);
    assert_eq!(codex.version.as_deref(), Some("codex-cli 0.0.0-fake"));
    let ws = create(
        host,
        "agent",
        Some(vec![SessionSpec::agent("codex"), SessionSpec::shell()]),
    )
    .await;
    assert_eq!(ws.sessions[0].name, "codex");

    let ws = wait_agent(host, "agent", "codex", AgentState::WaitingForInput).await;
    let codex = ws.session("codex").unwrap();
    let info = codex.agent.as_ref().unwrap();
    let conversation = info
        .provider_session_id
        .clone()
        .expect("conversation discovered");
    assert_eq!(info.last_message.as_deref(), Some("All tests pass."));
    assert_eq!(ws.activity(), Activity::NeedsYou);
    assert_eq!(ws.attention.len(), 1);
    assert_eq!(ws.attention[0].kind, AttentionKind::Review);
    assert!(
        ws.attention[0].summary.contains("All tests pass."),
        "{:?}",
        ws.attention
    );
    // The message excerpt stays out of the event log.
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(!log.contains("tests pass"));
    assert!(log.contains("AttentionCreated"));

    // Acknowledging clears it.
    let n = host
        .conn()
        .await
        .attention_resolve("agent", Some("codex"))
        .await
        .unwrap();
    assert_eq!(n, 1);
    let ws = host.conn().await.workspace_get("agent").await.unwrap();
    assert!(ws.attention.is_empty());
    assert_eq!(ws.activity(), Activity::Idle);

    // Restarting resumes the same conversation.
    let restarted = host
        .conn()
        .await
        .session_restart("agent", "codex")
        .await
        .unwrap();
    assert_eq!(
        restarted
            .agent
            .as_ref()
            .unwrap()
            .provider_session_id
            .as_deref(),
        Some(conversation.as_str())
    );
    // Restart resets the agent to `starting`, so this is the resumed turn
    // finishing. (The brief `working` in between can fall between two polls.)
    assert_eq!(
        restarted.agent.as_ref().unwrap().state,
        AgentState::Starting
    );
    let ws = wait_agent(host, "agent", "codex", AgentState::WaitingForInput).await;
    assert_eq!(ws.attention.len(), 1);
    let args = std::fs::read_to_string(&fake.log).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    assert_eq!(
        lines,
        ["--no-daemon", &format!("--no-daemon resume {conversation}")]
    );
}

#[tokio::test]
async fn codex_without_no_daemon_is_launched_without_it() {
    use otter_core::AgentState;
    let fake = fake_codex_host("old").await;
    let host = &fake.host;
    create(host, "old", Some(vec![SessionSpec::agent("codex")])).await;
    let ws = wait_agent(host, "old", "codex", AgentState::WaitingForInput).await;
    let conversation = ws.session("codex").unwrap().agent.as_ref().unwrap();
    let conversation = conversation.provider_session_id.clone().unwrap();
    host.conn()
        .await
        .session_restart("old", "codex")
        .await
        .unwrap();
    wait_agent(host, "old", "codex", AgentState::WaitingForInput).await;
    let args = std::fs::read_to_string(&fake.log).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    assert_eq!(lines, ["", &format!("resume {conversation}")]);
}

#[tokio::test]
async fn claude_code_session_is_observed_resumed_and_needs_you_when_done() {
    use otter_core::{Activity, AgentState, AttentionKind};
    let fake = fake_codex_host("normal").await;
    let host = &fake.host;
    let status = host.conn().await.host_status().await.unwrap();
    let claude = status
        .agents
        .iter()
        .find(|a| a.provider == "claude")
        .unwrap();
    assert!(claude.available && claude.can_resume, "{:?}", status.agents);
    assert_eq!(claude.version.as_deref(), Some("2.0.0-fake (Claude Code)"));

    let mut spec = SessionSpec::agent("claude");
    spec.prompt = Some("fix the tests".into());
    let ws = create(host, "cc", Some(vec![spec])).await;
    assert_eq!(ws.sessions[0].name, "claude");

    let ws = wait_agent(host, "cc", "claude", AgentState::WaitingForInput).await;
    let info = ws.session("claude").unwrap().agent.clone().unwrap();
    let id = info
        .provider_session_id
        .clone()
        .expect("session discovered");
    assert_eq!(info.last_message.as_deref(), Some("All tests pass."));
    assert_eq!(ws.activity(), Activity::NeedsYou);
    assert_eq!(ws.attention[0].kind, AttentionKind::Review);

    // Restarting resumes the same Claude Code session.
    host.conn()
        .await
        .attention_resolve("cc", Some("claude"))
        .await
        .unwrap();
    let restarted = host
        .conn()
        .await
        .session_restart("cc", "claude")
        .await
        .unwrap();
    assert_eq!(
        restarted.agent.unwrap().provider_session_id.as_deref(),
        Some(id.as_str())
    );
    wait_agent(host, "cc", "claude", AgentState::WaitingForInput).await;
    let args = std::fs::read_to_string(&fake.log).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    assert_eq!(
        lines,
        ["claude fix the tests", &format!("claude --resume {id}")]
    );
}

#[tokio::test]
async fn quiet_agent_mid_turn_is_flagged_and_answering_clears_it() {
    use otter_core::{Activity, AgentState, AttentionKind};
    let fake = fake_codex_host("approval").await;
    let host = &fake.host;
    create(host, "ask", Some(vec![SessionSpec::agent("codex")])).await;
    let ws = wait_agent(host, "ask", "codex", AgentState::Blocked).await;
    assert_eq!(ws.activity(), Activity::NeedsYou);
    assert_eq!(ws.attention[0].kind, AttentionKind::Approval);

    // Answering (typing into the session) resolves it; the turn then ends.
    host.conn()
        .await
        .session_write("ask", "codex", "y", true)
        .await
        .unwrap();
    let ws = host.conn().await.workspace_get("ask").await.unwrap();
    assert!(
        ws.attention
            .iter()
            .all(|a| a.kind != AttentionKind::Approval),
        "{:?}",
        ws.attention
    );
    let ws = wait_agent(host, "ask", "codex", AgentState::WaitingForInput).await;
    assert_eq!(ws.attention.len(), 1);
    assert_eq!(ws.attention[0].kind, AttentionKind::Review);
}

#[tokio::test]
async fn long_thinking_with_a_ticking_screen_is_not_waiting() {
    use otter_core::AgentState;
    // No signal from the agent: only the heuristic (quiet after 2 s here)
    // could flag this turn, and the ticking screen keeps it working.
    let fake = fake_codex_host("thinking").await;
    let host = &fake.host;
    create(host, "think", Some(vec![SessionSpec::agent("codex")])).await;
    wait_agent(host, "think", "codex", AgentState::Working).await;
    let start = std::time::Instant::now();
    loop {
        let ws = host.conn().await.workspace_get("think").await.unwrap();
        let state = ws.session("codex").unwrap().agent.as_ref().unwrap().state;
        assert_ne!(state, AgentState::Blocked, "{:?}", ws.attention);
        if state == AgentState::WaitingForInput {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "turn never ended"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(start.elapsed() > Duration::from_secs(4));
}

#[tokio::test]
async fn claude_code_prompts_come_from_its_hooks() {
    use otter_core::{Activity, AgentState, AttentionKind};
    let fake = fake_codex_host("approval").await;
    let host = &fake.host;
    create(host, "perm", Some(vec![SessionSpec::agent("claude")])).await;
    let ws = wait_agent(host, "perm", "claude", AgentState::Blocked).await;
    assert_eq!(ws.activity(), Activity::NeedsYou);
    assert_eq!(ws.attention.len(), 1);
    assert_eq!(ws.attention[0].kind, AttentionKind::Approval);
    // What the hook said, not the quiet-turn guess.
    assert_eq!(
        ws.attention[0].summary,
        "claude needs your approval: Bash: Create empty file z"
    );
    assert!(!ws.attention[0].inferred, "reported (D-109)");
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(!log.contains("empty file"));

    // Approving clears it and the turn goes on to its end.
    host.conn()
        .await
        .session_write("perm", "claude", "1", true)
        .await
        .unwrap();
    let ws = wait_agent(host, "perm", "claude", AgentState::WaitingForInput).await;
    assert_eq!(ws.attention.len(), 1);
    assert_eq!(ws.attention[0].kind, AttentionKind::Review);

    // A question is a question.
    let fake = fake_codex_host("question").await;
    let host = &fake.host;
    create(host, "ask", Some(vec![SessionSpec::agent("claude")])).await;
    let ws = wait_agent(host, "ask", "claude", AgentState::Blocked).await;
    assert_eq!(ws.attention[0].kind, AttentionKind::Question);
    assert_eq!(
        ws.attention[0].summary,
        "claude asks: Name the file a or b?"
    );
    host.conn()
        .await
        .session_write("ask", "claude", "a", true)
        .await
        .unwrap();
    wait_agent(host, "ask", "claude", AgentState::WaitingForInput).await;
}

/// Looking at a session doesn't answer it (D-099): an approval stays open
/// while attached and after; answered in the terminal, the next prompt is
/// what needs the developer, even when no reconcile saw the agent at work in
/// between. A finished turn is settled by looking.
#[tokio::test]
async fn looking_at_a_prompt_does_not_answer_it() {
    use otter_core::{AgentState, AttentionKind};
    let fake = fake_codex_host("twoprompts").await;
    let host = &fake.host;
    create(host, "look", Some(vec![SessionSpec::agent("claude")])).await;
    wait_agent(host, "look", "claude", AgentState::Blocked).await;
    let summaries = || async {
        let w = host.conn().await.workspace_get("look").await.unwrap();
        w.attention
            .iter()
            .map(|a| a.summary.clone())
            .collect::<Vec<_>>()
    };
    let first = "claude needs your approval: Bash: Create empty file z";
    eventually("the first prompt needs the developer", || async {
        (summaries().await == [first]).then_some(())
    })
    .await;

    let (mut reader, mut writer) = attach(host, "look", "claude").await;
    read_until(&mut reader, "proceed?").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        summaries().await,
        [first],
        "attaching is looking, not answering"
    );

    // Answered in the terminal (not `session.write`): the second prompt.
    send(&mut writer, "1\r").await;
    read_until(&mut reader, "proceed again?").await;
    eventually("the second prompt needs the developer", || async {
        (summaries().await == ["claude needs your approval: Bash: Remove file y"]).then_some(())
    })
    .await;
    writer.send(&Frame::Detach).await.unwrap();
    read_exit(&mut reader).await;
    let ws = host.conn().await.workspace_get("look").await.unwrap();
    assert_eq!(ws.attention.len(), 1, "detaching leaves it too");
    assert_eq!(ws.attention[0].kind, AttentionKind::Approval);

    // The turn ends: looking at it settles the review.
    host.conn()
        .await
        .session_write("look", "claude", "1", true)
        .await
        .unwrap();
    let ws = wait_agent(host, "look", "claude", AgentState::WaitingForInput).await;
    assert_eq!(ws.attention[0].kind, AttentionKind::Review);
    let (mut reader, mut writer) = attach(host, "look", "claude").await;
    read_until(&mut reader, "done").await;
    eventually("looking settles the review", || async {
        summaries().await.is_empty().then_some(())
    })
    .await;
    writer.send(&Frame::Detach).await.unwrap();
    read_exit(&mut reader).await;
}

/// A terminal on screen is looking (D-137): a turn that ends in front of the
/// developer asks for nothing, and one that ended off screen is settled when
/// it comes back on. Neither answers a prompt (D-099).
#[tokio::test]
async fn a_turn_that_ends_on_screen_is_already_seen() {
    use otter_core::{AgentState, AttentionKind};
    let fake = fake_codex_host("approval").await;
    let host = &fake.host;
    let kinds = |ws: &'static str| async move {
        let w = host.conn().await.workspace_get(ws).await.unwrap();
        w.attention.iter().map(|a| a.kind).collect::<Vec<_>>()
    };
    let view = |visible| {
        Frame::Control(Control::View {
            visible,
            cols: 100,
            rows: 30,
        })
    };

    // On screen while the agent asks, and when its turn ends; another
    // terminal on it is off screen meanwhile (a workspace kept, D-100).
    create(host, "seen", Some(vec![SessionSpec::agent("claude")])).await;
    wait_agent(host, "seen", "claude", AgentState::Blocked).await;
    let (mut reader, mut writer) = controlled_attach(host, "seen", "claude", (100, 30), 1000).await;
    read_until_control(&mut reader, snapshot_done).await;
    let (mut kept, mut kept_writer) =
        controlled_attach(host, "seen", "claude", (100, 30), 1000).await;
    read_until_control(&mut kept, snapshot_done).await;
    // Off screen and back again is looking again, still not answering.
    writer.send(&view(false)).await.unwrap();
    writer.send(&view(true)).await.unwrap();
    kept_writer.send(&view(false)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        kinds("seen").await,
        [AttentionKind::Approval],
        "on screen is looking, not answering"
    );
    // Answered in the terminal: the turn ends in front of the developer.
    send(&mut writer, "1\r").await;
    wait_agent(host, "seen", "claude", AgentState::WaitingForInput).await;
    assert_eq!(
        kinds("seen").await,
        [],
        "a turn seen ending asks for nothing"
    );
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(kinds("seen").await, []);
    for (mut reader, mut writer) in [(reader, writer), (kept, kept_writer)] {
        writer.send(&Frame::Detach).await.unwrap();
        read_exit(&mut reader).await;
    }

    // Off screen when the turn ends: there's a turn to review, until the
    // terminal is back on screen.
    create(host, "away", Some(vec![SessionSpec::agent("claude")])).await;
    wait_agent(host, "away", "claude", AgentState::Blocked).await;
    let (mut reader, mut writer) = controlled_attach(host, "away", "claude", (100, 30), 1000).await;
    read_until_control(&mut reader, snapshot_done).await;
    writer.send(&view(false)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    host.conn()
        .await
        .session_write("away", "claude", "1", true)
        .await
        .unwrap();
    wait_agent(host, "away", "claude", AgentState::WaitingForInput).await;
    assert_eq!(kinds("away").await, [AttentionKind::Review]);
    writer.send(&view(true)).await.unwrap();
    eventually("back on screen settles the review", || async {
        kinds("away").await.is_empty().then_some(())
    })
    .await;
    writer.send(&Frame::Detach).await.unwrap();
    read_exit(&mut reader).await;
}

/// A command that completes on screen is seen completing (D-137); off
/// screen, its completion waits to be seen.
#[tokio::test]
async fn a_command_that_completes_on_screen_is_already_seen() {
    use otter_core::AttentionKind;
    let host = TestHost::new();
    let kinds = |ws: &'static str| {
        let host = &host;
        async move {
            let w = host.conn().await.workspace_get(ws).await.unwrap();
            w.attention.iter().map(|a| a.kind).collect::<Vec<_>>()
        }
    };
    for (ws, on_screen) in [("cmd-seen", true), ("cmd-away", false)] {
        create(
            &host,
            ws,
            Some(vec![spec(
                SessionKind::Command,
                "ask",
                "read x; echo got-$x",
            )]),
        )
        .await;
        wait_status(&host, ws, "ask", SessionStatus::Running).await;
        let (mut reader, mut writer) = controlled_attach(&host, ws, "ask", (100, 30), 1000).await;
        read_until_control(&mut reader, snapshot_done).await;
        if !on_screen {
            writer
                .send(&Frame::Control(Control::View {
                    visible: false,
                    cols: 100,
                    rows: 30,
                }))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        send(&mut writer, "yes\r").await;
        assert_eq!(
            read_exit(&mut reader).await.reason,
            AttachExitReason::Exited
        );
        wait_status(&host, ws, "ask", SessionStatus::Completed).await;
        let want: &[AttentionKind] = if on_screen {
            &[]
        } else {
            &[AttentionKind::Completion]
        };
        assert_eq!(kinds(ws).await, want, "{ws}");
    }
}

/// A terminal whose client went without a word (no `view`, no detach) is
/// no longer on screen (D-137): the turn that ends after it is to review.
#[tokio::test]
async fn a_terminal_gone_without_a_word_is_no_longer_on_screen() {
    use otter_core::{AgentState, AttentionKind};
    let fake = fake_codex_host("approval").await;
    let host = &fake.host;
    create(host, "gone", Some(vec![SessionSpec::agent("claude")])).await;
    wait_agent(host, "gone", "claude", AgentState::Blocked).await;
    let (mut reader, writer) = controlled_attach(host, "gone", "claude", (100, 30), 1000).await;
    read_until_control(&mut reader, snapshot_done).await;
    drop((reader, writer));
    eventually("the attach is over", || async {
        (attach_clients(host) == 0).then_some(())
    })
    .await;
    host.conn()
        .await
        .session_write("gone", "claude", "1", true)
        .await
        .unwrap();
    let ws = wait_agent(host, "gone", "claude", AgentState::WaitingForInput).await;
    let kinds: Vec<_> = ws.attention.iter().map(|a| a.kind).collect();
    assert_eq!(kinds, [AttentionKind::Review]);
}

/// A client that stops taking output is let go once a frame can't reach it
/// (D-137): one whose end vanished (a Mac asleep) would otherwise hold the
/// attach, and its terminal on screen, until the connection failed.
#[tokio::test]
async fn an_attach_whose_client_stops_reading_is_let_go() {
    let host = TestHost::with_env(&[("OTTER_ATTACH_WRITE_TIMEOUT_MS", "1000".into())]).await;
    sh_session(&host, "stuck").await;
    let (mut reader, _writer) = controlled_attach(&host, "stuck", "sh", (100, 30), 1000).await;
    read_until_control(&mut reader, snapshot_done).await;
    assert_eq!(attach_clients(&host), 1);
    // More output than the way to the client holds, never read.
    host.conn()
        .await
        .session_write("stuck", "sh", "seq 1 400000", true)
        .await
        .unwrap();
    eventually("the attach is let go", || async {
        (attach_clients(&host) == 0).then_some(())
    })
    .await;
    // The session goes on.
    wait_output(&host, "stuck", "sh", "400000").await;
    let ws = host.conn().await.workspace_get("stuck").await.unwrap();
    assert_eq!(ws.sessions[0].status(), SessionStatus::Running);
}

/// A prompt that redraws once a second (Claude Code's auto-deny countdown)
/// still needs the developer; approved, the tool's spinner says so before
/// the tool finishes (D-136).
#[tokio::test]
async fn a_ticking_prompt_is_not_an_answer_but_a_running_tool_is() {
    use otter_core::{AgentState, AttentionKind};
    let fake = fake_codex_host("countdown").await;
    let host = &fake.host;
    create(host, "tick", Some(vec![SessionSpec::agent("claude")])).await;
    let ws = wait_agent(host, "tick", "claude", AgentState::Blocked).await;
    let summary = "claude needs your approval: Bash: Run the tests";
    assert_eq!(ws.attention[0].summary, summary);

    // Well past the 2 s that used to take any change for an answer, and
    // the screen did change.
    let screen = || async {
        host.conn()
            .await
            .session_read("tick", "claude", Some(0))
            .await
            .unwrap()
    };
    let before = screen().await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_ne!(screen().await, before, "the prompt ticks");
    let ws = host.conn().await.workspace_get("tick").await.unwrap();
    let agent = ws.session("claude").unwrap().agent.clone().unwrap();
    assert_eq!(agent.state, AgentState::Blocked);
    assert_eq!(ws.attention.len(), 1, "{:?}", ws.attention);
    assert_eq!(ws.attention[0].kind, AttentionKind::Approval);
    assert_eq!(ws.attention[0].summary, summary);

    // Approved: working while the tool runs, before its hook says it ended.
    host.conn()
        .await
        .session_write("tick", "claude", "1", true)
        .await
        .unwrap();
    wait_agent(host, "tick", "claude", AgentState::Working).await;
    let hooks = host
        .home()
        .join("run/agents")
        .join(ws.session("claude").unwrap().id.as_str())
        .join("claude-hooks.jsonl");
    let hooks = std::fs::read_to_string(hooks).unwrap();
    assert!(hooks.contains("PermissionRequest"), "{hooks}");
    assert!(!hooks.contains("PostToolUse"), "the tool still runs");
    let ws = wait_agent(host, "tick", "claude", AgentState::WaitingForInput).await;
    assert_eq!(ws.attention.len(), 1);
    assert_eq!(ws.attention[0].kind, AttentionKind::Review);
}

/// A Claude Code session started on the host's own sign-in, restarted after
/// a token is set in Settings (D-142): the same conversation resumes,
/// signed in with the token. A shell beside it never gets it.
#[tokio::test]
async fn a_restarted_claude_code_session_signs_in_with_the_token() {
    use otter_core::AgentState;
    use otter_protocol::host::SettingsUpdate;
    let fake = fake_codex_host("ok").await;
    let host = &fake.host;
    create(
        host,
        "tok",
        Some(vec![
            SessionSpec::agent("claude"),
            SessionSpec {
                name: Some("sh".into()),
                command: Some(
                    "echo \"tok=${CLAUDE_CODE_OAUTH_TOKEN:-none}\" >> tok.txt; sleep 600".into(),
                ),
                ..SessionSpec::shell()
            },
        ]),
    )
    .await;
    wait_agent(host, "tok", "claude", AgentState::WaitingForInput).await;
    let auth = fake.log.with_extension("log.auth");
    let read = || std::fs::read_to_string(&auth).unwrap_or_default();
    assert!(read().ends_with("(oauth token: unset)\n"), "{}", read());
    let id = host
        .conn()
        .await
        .workspace_get("tok")
        .await
        .unwrap()
        .session("claude")
        .and_then(|s| s.agent.as_ref()?.provider_session_id.clone())
        .expect("bound to its transcript");

    let mut conn = host.conn().await;
    conn.settings_set(SettingsUpdate {
        secrets: std::collections::BTreeMap::from([(
            "CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
            Some("sk-ant-oat01-RESTARTTOKEN0123456789".to_owned()),
        )]),
        ..Default::default()
    })
    .await
    .unwrap();
    conn.session_restart("tok", "claude").await.unwrap();
    eventually("it starts again", || async {
        read().contains("(oauth token: set)").then_some(())
    })
    .await;
    assert!(
        read().contains(&format!("claude --resume {id} (oauth token: set)")),
        "{}",
        read()
    );
    // A command beside it, started again with the token set: not for it.
    conn.session_restart("tok", "sh").await.unwrap();
    let root = std::path::PathBuf::from(conn.workspace_get("tok").await.unwrap().root);
    eventually("the command ran twice", || async {
        let out = std::fs::read_to_string(root.join("tok.txt")).ok()?;
        (out.lines().count() == 2).then_some(out)
    })
    .await
    .lines()
    .for_each(|l| assert_eq!(l, "tok=none"));
}

#[tokio::test]
async fn claude_code_without_hooks_falls_back_to_the_quiet_turn() {
    use otter_core::{AgentState, AttentionKind};
    let fake = fake_codex_host("nohooks").await;
    let host = &fake.host;
    create(host, "nohooks", Some(vec![SessionSpec::agent("claude")])).await;
    let ws = wait_agent(host, "nohooks", "claude", AgentState::Blocked).await;
    assert_eq!(ws.attention[0].kind, AttentionKind::Approval);
    assert!(ws.attention[0].inferred, "guessed (D-109)");
    assert!(
        ws.attention[0].summary.contains("seems to be waiting"),
        "{:?}",
        ws.attention
    );
    host.conn()
        .await
        .session_write("nohooks", "claude", "1", true)
        .await
        .unwrap();
    wait_agent(host, "nohooks", "claude", AgentState::WaitingForInput).await;
}

/// One driver per workspace (D-079): a second task on a workspace another
/// task's agent works in waits, says why, never runs alongside it, and
/// goes on by itself when the workspace is free.
#[tokio::test]
async fn a_second_task_waits_for_the_workspace() {
    use otter_core::task::{RunState, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let first = started_with(host, "shared", "A long job HANG", "test:\n\t@true\n", false).await;
    let a = wait_task(host, &first, "working", |f| {
        f.live_run().is_some_and(|r| r.state == RunState::Running)
    })
    .await;
    // As clients see it (D-141): when its agent was last heard from, and
    // what comes after the step under way.
    assert!(a.live_run().unwrap().heard_at.is_some());
    assert_eq!(
        a.next.as_deref(),
        Some("Then: the work is checked against its 2 acceptance criteria."),
    );
    let mut conn = host.conn().await;
    let second = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "create-second".into(),
            title: "Second".into(),
            request: "Add a README".into(),
            workspace: Some("shared".into()),
            answer_only: false,
        })
        .await
        .unwrap();
    let second = second.id.to_string();
    conn.task_act("start-second", &second, TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &second, "waiting for the workspace", |f| {
        f.status_reason.as_deref() == Some("Waiting for shared: “CSV export” is working there")
    })
    .await;
    assert_eq!(f.status, TaskStatus::Implementing);
    assert!(f.runs.is_empty());
    // Still waiting a while later: not alongside.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(conn.task_get(&second).await.unwrap().runs.is_empty());
    // The first one holds: the second goes on by itself.
    conn.task_act("pause-first", &first, TaskAction::Pause)
        .await
        .unwrap();
    let f = wait_task(host, &second, "the second done", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    let a = conn.task_get(&first).await.unwrap();
    assert!(
        f.runs[0].started_at >= a.runs[0].ended_at.unwrap(),
        "one at a time: {:?} vs {:?}",
        f.runs[0].started_at,
        a.runs[0].ended_at
    );
    assert!(!f.rationale.is_empty());
}

/// A task in review keeps its Git workspace (D-156): its verified work
/// isn't committed until it's published, so another task started there
/// waits — else publishing the first would commit the second's edits
/// under its title (the live bench's B03). Accepted, the first lets go.
#[tokio::test]
async fn a_task_waiting_to_publish_keeps_its_workspace() {
    use otter_core::task::{TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let (first, _) = git_task(&fake).await;
    wait_task(host, &first, "asked to publish", |f| {
        f.status == TaskStatus::Review
            && f.pending_decisions()
                .any(|d| d.summary.contains("git push"))
    })
    .await;
    let mut conn = host.conn().await;
    let second = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "create-second".into(),
            title: "Second".into(),
            request: "Add a README".into(),
            workspace: Some("csv".into()),
            answer_only: false,
        })
        .await
        .unwrap()
        .id
        .to_string();
    conn.task_act("start-second", &second, TaskAction::Start)
        .await
        .unwrap();
    wait_task(host, &second, "waiting for the first", |f| {
        f.status_reason.as_deref()
            == Some(&*format!(
                "Waiting for csv: “CSV export”{}",
                otter_core::task::HELD
            ))
    })
    .await;
    // Published: still the first one's until it's accepted.
    allow_publish(host, &first).await;
    wait_task(host, &first, "published", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(conn.task_get(&second).await.unwrap().runs.is_empty());
    conn.task_act(
        "accept-first",
        &first,
        TaskAction::Accept {
            override_gates: true,
        },
    )
    .await
    .unwrap();
    let f = wait_task(host, &second, "the second at work", |f| !f.runs.is_empty()).await;
    let a = conn.task_get(&first).await.unwrap();
    assert_eq!(a.status, TaskStatus::Done);
    assert!(f.runs[0].started_at >= a.delivery.unwrap().pushed_at.unwrap());
}

/// A task waiting for a Git workspace gets one of its own (D-080): a
/// worktree of the same repository on a branch for it, and both tasks'
/// agents work at once, each in its own directory.
#[tokio::test]
async fn a_waiting_task_gets_a_worktree_of_its_own() {
    use otter_core::task::{RunState, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let src = host.home().join("origin");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "init"]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("app", &src, Some("feature/a"), vec![]))
        .await
        .unwrap();
    let ready = |name: &'static str| async move {
        eventually("workspace ready", || async {
            let ws = host.conn().await.workspace_get(name).await.unwrap();
            (ws.state == WorkspaceState::Ready).then_some(ws)
        })
        .await
    };
    let app = ready("app").await;
    let new = |title: &str, request: &str| otter_protocol::task::TaskCreate {
        command_id: format!("create-{title}"),
        title: title.into(),
        request: request.into(),
        workspace: Some("app".into()),
        answer_only: false,
    };
    let first = conn
        .task_create(new("First", "A long job HANG"))
        .await
        .unwrap()
        .id
        .to_string();
    conn.task_act("start-first", &first, TaskAction::Start)
        .await
        .unwrap();
    wait_task(host, &first, "working", |f| {
        f.live_run().is_some_and(|r| r.state == RunState::Running)
    })
    .await;
    let second = conn
        .task_create(new("Second", "Add a README"))
        .await
        .unwrap()
        .id
        .to_string();
    conn.task_act("start-second", &second, TaskAction::Start)
        .await
        .unwrap();
    wait_task(host, &second, "waiting", |f| {
        f.status_reason.as_deref() == Some("Waiting for app: “First” is working there")
    })
    .await;
    let f = conn
        .task_act("own", &second, TaskAction::OwnWorkspace)
        .await
        .unwrap();
    let own = f.workspace_id.clone().unwrap();
    assert_ne!(own, app.id);
    let f = wait_task(host, &second, "the second done", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    assert_eq!(f.workspace_id.as_ref(), Some(&own));
    // In parallel: the first is still at work, in its own directory.
    let a = conn.task_get(&first).await.unwrap();
    assert_eq!(a.live_run().map(|r| r.state), Some(RunState::Running));
    let ws = conn.workspace_get(own.as_str()).await.unwrap();
    assert_eq!(ws.name, "app-second");
    assert_ne!(ws.root, app.root);
    let otter_core::WorkspaceSource::Git(g) = &ws.source else {
        panic!("{:?}", ws.source)
    };
    assert!(g.branch.starts_with("otter/second-"), "{}", g.branch);
    // Not for a task that has worked already: its work is where it was.
    let err = conn
        .task_act("own-again", &first, TaskAction::OwnWorkspace)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("has worked in its workspace already"),
        "{err:#}"
    );

    // Accepted, it's over: the workspace made for it is put away (D-105),
    // and its conversation says so. The one it was given stays.
    assert_eq!(
        ws.task_id.as_ref().map(|f| f.as_str()),
        Some(second.as_str())
    );
    conn.task_act(
        "accept-second",
        &second,
        TaskAction::Accept {
            override_gates: true,
        },
    )
    .await
    .unwrap();
    // A push it still asks to be allowed (raised as review ended) holds the
    // workspace until it's answered.
    let pending = eventually("archived, or held by a question", || async {
        let ws = host.conn().await.workspace_get(own.as_str()).await.unwrap();
        let f = host.conn().await.task_get(&second).await.unwrap();
        let asks: Vec<_> = f.pending_decisions().cloned().collect();
        match ws.state {
            WorkspaceState::Archived => Some(Vec::new()),
            _ if !asks.is_empty() => Some(asks),
            _ => None,
        }
    })
    .await;
    if !pending.is_empty() {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let ws = conn.workspace_get(own.as_str()).await.unwrap();
        assert_eq!(ws.state, WorkspaceState::Ready, "held while it asks");
        for d in pending {
            conn.task_act(
                &format!("deny-{}", d.id),
                &second,
                TaskAction::Decide {
                    decision_id: d.id.clone(),
                    approve: false,
                    answer: None,
                    answers: None,
                },
            )
            .await
            .unwrap();
        }
    }
    eventually("its own workspace is archived", || async {
        let ws = host.conn().await.workspace_get(own.as_str()).await.unwrap();
        (ws.state == WorkspaceState::Archived).then_some(())
    })
    .await;
    let f = wait_task(host, &second, "it says so", |f| {
        f.messages
            .last()
            .is_some_and(|m| m.text.contains("app-second is archived"))
    })
    .await;
    assert_eq!(f.status, TaskStatus::Done);
    assert_eq!(
        conn.workspace_get("app").await.unwrap().state,
        WorkspaceState::Ready
    );
    // More asked of it: its workspace comes back, and the work goes on there.
    let runs = f.runs.len();
    conn.task_act(
        "more-second",
        &second,
        TaskAction::RequestChanges {
            note: Some("And a LICENSE".into()),
        },
    )
    .await
    .unwrap();
    wait_task(host, &second, "it works again", |f| {
        f.runs.len() > runs
            && f.runs
                .last()
                .is_some_and(|r| r.state == RunState::Completed)
    })
    .await;
    let ws = conn.workspace_get(own.as_str()).await.unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready);
}

/// Workspaces and sessions take another name (D-101): the id, the directory
/// and the running process stay; a name already taken is refused.
#[tokio::test]
async fn workspaces_and_sessions_are_renamed() {
    let host = TestHost::new();
    sh_session(&host, "old").await;
    create(&host, "other", Some(vec![])).await;
    let mut conn = host.conn().await;
    let before = conn.workspace_get("old").await.unwrap();
    let ws = conn.workspace_rename("old", " fresh name ").await.unwrap();
    assert_eq!(ws.name, "fresh name");
    assert_eq!((&ws.id, &ws.root), (&before.id, &before.root));
    assert!(conn.workspace_get("old").await.is_err());
    let err = conn
        .workspace_rename("other", "fresh name")
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    let err = conn.workspace_rename("other", "a/b").await.unwrap_err();
    assert!(format!("{err:#}").contains("must not contain"), "{err:#}");

    let s = conn
        .session_rename("fresh name", "sh", "build shell")
        .await
        .unwrap();
    assert_eq!(s.name, "build shell");
    assert_eq!(s.id, before.sessions[0].id);
    assert_eq!(s.status(), SessionStatus::Running);
    conn.session_write("fresh name", "build shell", "echo still-$((6*7))", true)
        .await
        .unwrap();
    eventually("the same process answers", || async {
        let out = host
            .conn()
            .await
            .session_read("fresh name", "build shell", None)
            .await
            .unwrap();
        out.contains("still-42").then_some(())
    })
    .await;
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(log.contains("WorkspaceRenamed") && log.contains("SessionRenamed"));
}

/// A session takes another place among its workspace's (D-150): the
/// sessions and their processes stay, the order is kept across a restart,
/// and the workspace doesn't count as active for it.
#[tokio::test]
async fn sessions_are_put_in_another_order() {
    let host = TestHost::new();
    let sh = |name| spec(SessionKind::Terminal, name, "sh");
    let before = create(&host, "order", Some(vec![sh("a"), sh("b"), sh("c")])).await;
    let names = |ws: &Workspace| {
        ws.sessions
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>()
    };
    let mut conn = host.conn().await;

    let ws = conn.session_move("order", "c", 0).await.unwrap();
    assert_eq!(names(&ws), ["c", "a", "b"]);
    assert_eq!(ws.updated_at, before.updated_at);
    // Past the end is the last place.
    let ws = conn.session_move("order", "a", 99).await.unwrap();
    assert_eq!(names(&ws), ["c", "b", "a"]);
    // Its own place: nothing changes, nothing is said.
    let ws = conn.session_move("order", "b", 1).await.unwrap();
    assert_eq!(names(&ws), ["c", "b", "a"]);
    let err = conn.session_move("order", "d", 0).await.unwrap_err();
    assert!(format!("{err:#}").contains("no session `d`"), "{err:#}");

    let ws = conn.workspace_get("order").await.unwrap();
    assert_eq!(names(&ws), ["c", "b", "a"]);
    let mut ids: Vec<_> = ws.sessions.iter().map(|s| s.id.clone()).collect();
    let mut was: Vec<_> = before.sessions.iter().map(|s| s.id.clone()).collect();
    ids.sort();
    was.sort();
    assert_eq!(ids, was);
    assert!(
        ws.sessions
            .iter()
            .all(|s| s.status() == SessionStatus::Running)
    );
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert_eq!(log.matches("SessionMoved").count(), 2, "{log}");

    host.stop_daemon().await;
    let ws = host.conn().await.workspace_get("order").await.unwrap();
    assert_eq!(names(&ws), ["c", "b", "a"]);
}

/// A workspace's Git work for the developer (D-102): its branch, commits
/// ahead of its base, every changed file — committed or not, new ones too —
/// with its lines, and one file's diff.
#[tokio::test]
async fn a_workspace_shows_its_changes() {
    use otter_protocol::work::ChangeKind;
    let host = TestHost::new();
    let long: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    let repo = make_repo(
        &host,
        "origin",
        &[
            ("a.txt", "one\ntwo\n"),
            ("b.txt", "keep\n"),
            ("long.txt", long.as_str()),
        ],
    );
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("work", &repo, Some("feature/x"), vec![]))
        .await
        .unwrap();
    let ws = eventually("workspace ready", || async {
        let ws = host.conn().await.workspace_get("work").await.unwrap();
        (ws.state == WorkspaceState::Ready).then_some(ws)
    })
    .await;
    let c = conn.workspace_changes("work").await.unwrap();
    assert!(c.git);
    assert_eq!(c.branch.as_deref(), Some("feature/x"));
    assert_eq!((c.ahead, c.behind, c.files.len()), (0, 0, 0));
    assert_eq!(c.unpushed, Some(0));

    let root = Path::new(&ws.root);
    std::fs::write(root.join("a.txt"), "one\n2\nthree\n").unwrap();
    git(root, &["commit", "-qam", "edit a"]);
    // Work no remote has (D-154): until it is pushed, whatever the base.
    assert_eq!(
        conn.workspace_changes("work").await.unwrap().unpushed,
        Some(1)
    );
    git(
        root,
        &["push", "-q", "-u", "origin", "HEAD:refs/heads/feature/x"],
    );
    let c = conn.workspace_changes("work").await.unwrap();
    assert_eq!((c.ahead, c.unpushed), (1, Some(0)));
    std::fs::write(root.join("b.txt"), "keep\nmore\n").unwrap();
    std::fs::write(root.join("new.md"), "# new\nline\n").unwrap();
    let c = conn.workspace_changes("work").await.unwrap();
    assert_eq!(c.base.as_deref(), Some("origin/main"));
    assert_eq!(c.ahead, 1);
    let files: Vec<_> = c
        .files
        .iter()
        .map(|f| {
            (
                f.path.as_str(),
                f.change,
                f.insertions,
                f.deletions,
                f.uncommitted,
            )
        })
        .collect();
    assert_eq!(
        files,
        [
            ("a.txt", ChangeKind::Modified, Some(2), Some(1), false),
            ("b.txt", ChangeKind::Modified, Some(1), Some(0), true),
            ("new.md", ChangeKind::Untracked, Some(2), Some(0), true),
        ]
    );
    assert_eq!((c.insertions, c.deletions), (5, 1));
    let d = conn.workspace_diff("work", "a.txt", None).await.unwrap();
    assert!(
        d.text.contains("-two") && d.text.contains("+three"),
        "{}",
        d.text
    );
    // The whole file, for the app to fold (D-111): far from the change too.
    std::fs::write(
        root.join("long.txt"),
        long.replace("line 20\n", "line twenty\n"),
    )
    .unwrap();
    let near = conn.workspace_diff("work", "long.txt", None).await.unwrap();
    assert!(!near.text.contains("line 40"), "{}", near.text);
    let all = conn
        .workspace_diff("work", "long.txt", Some(1_000_000))
        .await
        .unwrap();
    assert!(
        all.text.contains("line 40") && all.text.contains("+line twenty"),
        "{}",
        all.text
    );
    let d = conn.workspace_diff("work", "new.md", None).await.unwrap();
    assert!(d.text.contains("+# new"), "{}", d.text);

    create(&host, "plain", Some(vec![])).await;
    assert!(!conn.workspace_changes("plain").await.unwrap().git);
}

/// How far behind its base a workspace is, is as of the last fetch: preparing
/// it again fetches (D-154).
#[tokio::test]
async fn preparing_a_workspace_again_brings_its_repository_up_to_date() {
    let host = TestHost::new();
    let repo = make_repo(&host, "origin", &[("a.txt", "one\n")]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("work", &repo, Some("feature/y"), vec![]))
        .await
        .unwrap();
    wait_prepared(&host, "work").await;
    assert_eq!(conn.workspace_changes("work").await.unwrap().behind, 0);

    std::fs::write(repo.join("b.txt"), "two\n").unwrap();
    git(&repo, &["add", "b.txt"]);
    git(&repo, &["commit", "-qm", "more upstream"]);
    // Not fetched yet: the workspace can't know.
    assert_eq!(conn.workspace_changes("work").await.unwrap().behind, 0);

    conn.workspace_prepare("work").await.unwrap();
    eventually("fetched", || async {
        let ws = host.conn().await.workspace_get("work").await.unwrap();
        let c = host.conn().await.workspace_changes("work").await.unwrap();
        (ws.state == WorkspaceState::Ready && c.behind == 1).then_some(())
    })
    .await;
}

/// A workspace driven by hand can be pushed, and its pull request opened
/// (D-154): the developer's own action. Nothing is committed for them, and a
/// pull request that exists keeps its description.
#[tokio::test]
async fn a_workspace_is_pushed_and_its_pull_request_opened() {
    use otter_protocol::work::WorkspacePublish;
    let fake = fake_codex_host("normal").await;
    let host = &fake.host;
    let repo = make_repo(host, "origin", &[("a.txt", "one\n")]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("work", &repo, Some("feature/p"), vec![]))
        .await
        .unwrap();
    let ws = wait_prepared(host, "work").await;
    let root = Path::new(&ws.root);
    let publish = |pull_request: bool, title: Option<&str>| WorkspacePublish {
        workspace: "work".into(),
        pull_request,
        title: title.map(str::to_owned),
        body: None,
    };
    let calls = || std::fs::read_to_string(fake.gh.join("calls.log")).unwrap_or_default();

    std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    git(root, &["commit", "-qam", "edit a"]);
    std::fs::write(root.join("left.txt"), "not committed\n").unwrap();
    // Pushed, and what is not committed said so; no pull request unless asked.
    let out = conn.workspace_publish(publish(false, None)).await.unwrap();
    assert_eq!(out.branch, "feature/p");
    assert!(out.uncommitted && out.pull_request.is_none(), "{out:?}");
    assert!(git(&repo, &["branch", "--list", "feature/p"]).contains("feature/p"));
    assert_eq!(
        conn.workspace_changes("work").await.unwrap().unpushed,
        Some(0)
    );
    assert!(calls().is_empty(), "{}", calls());

    // Opened, titled by the last commit; again, found, not edited.
    let out = conn.workspace_publish(publish(true, None)).await.unwrap();
    let pr = out.pull_request.unwrap();
    assert_eq!((pr.number, pr.opened), (7, true));
    assert!(calls().contains("--title edit a"), "{}", calls());
    // To the branch it was cut from, which the remote has.
    assert!(calls().contains("--base main"), "{}", calls());
    let out = conn
        .workspace_publish(publish(true, Some("another")))
        .await
        .unwrap();
    assert_eq!(out.pull_request.map(|p| p.opened), Some(false));
    assert!(!calls().contains("pr edit"), "{}", calls());

    create(host, "plain", Some(vec![])).await;
    let err = conn
        .workspace_publish(WorkspacePublish {
            workspace: "plain".into(),
            ..publish(false, None)
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a Git workspace"), "{err}");
}

/// The host lists what Otter runs and who owns it (D-161): a session's
/// process, and what it started, belong to the session.
#[tokio::test]
async fn the_host_lists_its_processes_with_their_owners() {
    use otter_protocol::host::ProcessOwner;
    let host = TestHost::new();
    let ws = create(
        &host,
        "owned",
        Some(vec![spec(SessionKind::Command, "sleeper", "sleep 300")]),
    )
    .await;
    wait_status(&host, "owned", "sleeper", SessionStatus::Running).await;
    let own = host.conn().await.host_status().await.unwrap().pid;
    let found = eventually("the session's process is listed", || async {
        let all = host.conn().await.host_processes().await.ok()?;
        let session = all.processes.iter().find(|p| {
            matches!(&p.owner, ProcessOwner::Execution { workspace_id, .. } if *workspace_id == ws.id)
        })?;
        let daemon = all.processes.iter().find(|p| p.pid == own)?;
        Some((session.clone(), daemon.clone()))
    })
    .await;
    let (session, daemon) = found;
    assert!(!session.stoppable, "a session is stopped as a session");
    assert!(matches!(daemon.owner, ProcessOwner::Daemon { this: true }));
    assert!(!daemon.stoppable);
}

/// Stopping a process by hand reaches only what nothing owns, named by pid
/// and start time (D-161): never this otterd, a session's process, a pid
/// with another start time, or anything unlisted.
#[tokio::test]
async fn only_an_orphan_can_be_stopped_by_pid_and_start_time() {
    use otter_protocol::host::{ProcessOwner, ProcessStop};
    let host = TestHost::new();
    let ws = create(
        &host,
        "owned",
        Some(vec![spec(SessionKind::Command, "sleeper", "sleep 300")]),
    )
    .await;
    wait_status(&host, "owned", "sleeper", SessionStatus::Running).await;
    // Something started in a workspace directory that no workspace has.
    let gone = host.home().join("workspaces/ws_gone");
    std::fs::create_dir_all(&gone).unwrap();
    let mut orphan = std::process::Command::new("sleep")
        .arg("300")
        .current_dir(&gone)
        .spawn()
        .unwrap();
    let pid = orphan.id();
    let all = eventually("the orphan is listed", || async {
        let all = host.conn().await.host_processes().await.ok()?;
        all.processes
            .iter()
            .any(|p| p.pid == pid && matches!(p.owner, ProcessOwner::Orphan { .. }))
            .then_some(all)
    })
    .await;
    let me = host.conn().await.host_status().await.unwrap().pid;
    let row = |pid: u32| all.processes.iter().find(|p| p.pid == pid).unwrap().clone();
    let stop = |pid: u32, started: u64| ProcessStop {
        pid,
        started,
        force: false,
    };
    let mut conn = host.conn().await;

    // This otterd, and a session's process: refused.
    let err = conn
        .host_process_stop(stop(me, row(me).started))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("this otterd"), "{err}");
    let session = all
        .processes
        .iter()
        .find(|p| matches!(&p.owner, ProcessOwner::Execution { workspace_id, .. } if *workspace_id == ws.id))
        .unwrap();
    let err = conn
        .host_process_stop(stop(session.pid, session.started))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("session or a run"), "{err}");
    // The orphan's pid with another start time (a reused id), or a pid
    // nobody listed: refused, and the orphan still runs.
    let err = conn
        .host_process_stop(stop(pid, row(pid).started + 1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no such process"), "{err}");
    let err = conn.host_process_stop(stop(1, 0)).await.unwrap_err();
    assert!(err.to_string().contains("no such process"), "{err}");
    assert!(orphan.try_wait().unwrap().is_none());

    // The orphan itself, by both: ended.
    conn.host_process_stop(stop(pid, row(pid).started))
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while orphan.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the orphan did not end"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Threads are not processes (D-161): on Linux an otterd's threads are listed
/// by the system like processes, with its own executable and arguments, and
/// a signal to a thread's id reaches the whole process: this daemon. None is
/// listed, and none can be stopped.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_otterds_threads_are_not_processes_to_stop() {
    use otter_protocol::host::ProcessStop;
    let host = TestHost::new();
    let me = host.conn().await.host_status().await.unwrap().pid;
    let threads: Vec<u32> = std::fs::read_dir(format!("/proc/{me}/task"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .filter(|t| *t != me)
        .collect();
    assert!(!threads.is_empty(), "otterd has threads");
    let all = eventually("processes are listed", || async {
        host.conn().await.host_processes().await.ok()
    })
    .await;
    // (Other otterds, of tests running at the same time, are listed: they are
    // processes. Their threads and this one's are not.)
    assert!(
        all.processes.iter().all(|p| !threads.contains(&p.pid)),
        "{:?}",
        all.processes
    );
    let started = all.processes.iter().find(|p| p.pid == me).unwrap().started;
    let mut conn = host.conn().await;
    for t in threads.iter().take(5) {
        let err = conn
            .host_process_stop(ProcessStop {
                pid: *t,
                started,
                force: false,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such process"), "{err}");
    }
    // Still there.
    conn.host_status().await.unwrap();
}

/// An orphan that leads a group of its own is ended with its group (D-161),
/// and nothing outside it: not this test's own group.
#[tokio::test]
async fn stopping_an_orphan_ends_its_group_only() {
    use otter_protocol::host::{ProcessOwner, ProcessStop};
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let host = TestHost::new();
    let gone = host.home().join("workspaces/ws_gone");
    std::fs::create_dir_all(&gone).unwrap();
    // A leader in its own group, with a child that prints its pid.
    let mut leader = std::process::Command::new("sh")
        .args(["-c", "sleep 300 & echo $!; wait"])
        .current_dir(&gone)
        .stdout(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(leader.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let child: u32 = line.trim().parse().unwrap();
    let pid = leader.id();
    let all = eventually("the leader is listed", || async {
        let all = host.conn().await.host_processes().await.ok()?;
        all.processes
            .iter()
            .any(|p| p.pid == pid && matches!(p.owner, ProcessOwner::Orphan { .. }))
            .then_some(all)
    })
    .await;
    let started = all.processes.iter().find(|p| p.pid == pid).unwrap().started;
    host.conn()
        .await
        .host_process_stop(ProcessStop {
            pid,
            started,
            force: false,
        })
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while leader.try_wait().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the leader did not end"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Its child went with the group (a zombie until reaped is gone for us:
    // signal 0 fails once it has been reaped by init).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while unsafe { libc::kill(child as libc::pid_t, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the child did not end"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Files are removed and renamed inside the workspace only (D-103): never
/// its root or anything outside it, a link rather than what it points to,
/// and never over another file.
#[tokio::test]
async fn files_are_removed_and_renamed_inside_the_workspace() {
    let host = TestHost::new();
    let ws = create(&host, "files", Some(vec![])).await;
    let root = Path::new(&ws.root);
    std::fs::create_dir_all(root.join("dir/sub")).unwrap();
    std::fs::write(root.join("dir/sub/f"), "x").unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    std::fs::write(root.join("b.txt"), "b").unwrap();
    let outside = host.home().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "k").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let mut conn = host.conn().await;

    conn.fs_rename("files", "a.txt", "c.txt").await.unwrap();
    assert!(root.join("c.txt").exists() && !root.join("a.txt").exists());
    let err = conn.fs_rename("files", "c.txt", "b.txt").await.unwrap_err();
    assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    let err = conn
        .fs_rename("files", "c.txt", "../c.txt")
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("isn't a file name"), "{err:#}");

    conn.fs_remove("files", "dir").await.unwrap();
    assert!(!root.join("dir").exists());
    conn.fs_remove("files", "link").await.unwrap();
    assert!(!root.join("link").exists() && outside.join("keep").exists());
    let abs = outside.join("keep").to_string_lossy().into_owned();
    for bad in ["", ".", "..", "../outside", abs.as_str()] {
        let err = conn.fs_remove("files", bad).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("inside the workspace"),
            "{bad}: {err:#}"
        );
    }
    assert!(root.exists() && outside.join("keep").exists());
}

/// The ports a workspace's sessions listen on, by session (D-104): a
/// service bound to a port the system chose is found under that session.
#[tokio::test]
async fn a_workspace_knows_its_sessions_ports() {
    let host = TestHost::new();
    let listen = "python3 -c \"import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); print('port', s.getsockname()[1], flush=True); time.sleep(600)\"";
    create(
        &host,
        "ports",
        Some(vec![
            spec(SessionKind::Service, "srv", listen),
            spec(SessionKind::Terminal, "sh", "sh"),
        ]),
    )
    .await;
    let port: u16 = eventually("the service listens", || async {
        let out = host
            .conn()
            .await
            .session_read("ports", "srv", None)
            .await
            .ok()?;
        out.split_whitespace()
            .skip_while(|w| *w != "port")
            .nth(1)
            .and_then(|p| p.parse().ok())
    })
    .await;
    let ws = host.conn().await.workspace_get("ports").await.unwrap();
    let srv = ws.session("srv").unwrap().id.clone();
    let ports = eventually("its port is found", || async {
        let p = host.conn().await.workspace_ports("ports").await.unwrap();
        p.iter().any(|p| p.port == port).then_some(p)
    })
    .await;
    assert!(ports.iter().all(|p| p.session_id == srv), "{ports:?}");
}

/// A host says what's wrong with it, each with what to do (D-077): here,
/// settings it couldn't read, met as it started.
#[tokio::test]
async fn a_host_says_whats_wrong_with_it() {
    let host = TestHost::new();
    std::fs::create_dir_all(host.home().join("state")).unwrap();
    std::fs::write(host.home().join("state/settings.json"), "{ not json").unwrap();
    let status = host.conn().await.host_status().await.unwrap();
    let p = status
        .problems
        .iter()
        .find(|p| p.what.contains("settings"))
        .unwrap_or_else(|| panic!("{:?}", status.problems));
    assert!(p.what.contains("couldn't be read"), "{}", p.what);
    assert!(p.next.contains("settings.json"), "{}", p.next);
    assert!(status.log.as_deref().unwrap().ends_with("logs/workd.log"));
    // Everything else on this host is fine but what its environment lacks.
    for p in &status.problems {
        assert!(!p.next.is_empty(), "{p:?}");
    }
}

/// Why a process failed is kept with it and said, without its terminal —
/// never a credential it printed — and survives otterd restarting (D-076).
#[tokio::test]
async fn a_failed_process_says_why_without_its_terminal() {
    let mut host = TestHost::with_env(&[]).await;
    create(
        &host,
        "why",
        Some(vec![spec(
            SessionKind::Command,
            "deploy",
            "echo starting; echo 'using API_TOKEN=tok_abcdef0123456789'; echo 'error: config/prod.toml not found' >&2; exit 3",
        )]),
    )
    .await;
    let ws = eventually("the failure", || async {
        let w = host.conn().await.workspace_get("why").await.unwrap();
        (!w.attention.is_empty()).then_some(w)
    })
    .await;
    assert_eq!(
        ws.attention[0].summary,
        "deploy failed (status 3): error: config/prod.toml not found"
    );
    let tail = |w: &otter_core::Workspace| {
        w.sessions[0]
            .executions
            .last()
            .unwrap()
            .tail
            .clone()
            .unwrap_or_default()
    };
    let t = tail(&ws);
    assert!(t.contains("starting") && t.contains("not found"), "{t}");
    assert!(!t.contains("tok_abcdef"), "{t}");
    assert!(t.contains("API_TOKEN=***"), "{t}");
    // Kept: after a restart, when the pane is gone.
    host.restart().await;
    let ws = host.conn().await.workspace_get("why").await.unwrap();
    assert_eq!(tail(&ws), t);
}

#[tokio::test]
async fn failures_and_completions_raise_attention() {
    use otter_core::{Activity, AttentionKind};
    let host = TestHost::new();
    create(
        &host,
        "jobs",
        Some(vec![
            spec(SessionKind::Command, "tests", "exit 1"),
            spec(SessionKind::Command, "lint", "true"),
            spec(SessionKind::Terminal, "sh", "exit 4"),
        ]),
    )
    .await;
    let ws = eventually("attention raised", || async {
        let w = host.conn().await.workspace_get("jobs").await.unwrap();
        (w.attention.len() == 2).then_some(w)
    })
    .await;
    let kinds: Vec<_> = ws
        .attention
        .iter()
        .map(|a| (a.summary.as_str(), a.kind))
        .collect();
    assert!(
        kinds.contains(&("tests failed (status 1)", AttentionKind::Failure)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("lint completed", AttentionKind::Completion)),
        "{kinds:?}"
    );
    // A terminal the user left is not attention-worthy.
    assert_eq!(ws.activity(), Activity::Failed);

    // Restarting the failed command resolves its item.
    host.conn()
        .await
        .session_restart("jobs", "tests")
        .await
        .unwrap();
    let ws = host.conn().await.workspace_get("jobs").await.unwrap();
    assert!(
        ws.attention.iter().all(|a| !a.summary.starts_with("tests")),
        "{:?}",
        ws.attention
    );
}

#[tokio::test]
async fn missing_agent_binary_is_a_visible_failure() {
    use otter_core::AttentionKind;
    let scratch = tempfile::Builder::new().prefix("wdn").tempdir().unwrap();
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let host = TestHost::with_env(&[
        ("HOME", home.to_string_lossy().into_owned()),
        ("SHELL", isolated_shell(scratch.path())),
        ("PATH", "/usr/bin:/bin".into()),
    ])
    .await;
    let status = host.conn().await.host_status().await.unwrap();
    assert!(
        status.agents.iter().all(|a| !a.available),
        "{:?}",
        status.agents
    );
    let ws = create(&host, "no-codex", Some(vec![SessionSpec::agent("codex")])).await;
    let s = &ws.sessions[0];
    assert_eq!(s.status(), SessionStatus::Failed);
    assert!(
        s.launch_error
            .as_deref()
            .unwrap_or("")
            .contains("codex is not installed"),
        "{s:?}"
    );
    assert_eq!(ws.attention[0].kind, AttentionKind::Failure);
}

// ---------------------------------------------------------------------------
// Host page: usage and listening ports
// ---------------------------------------------------------------------------

#[tokio::test]
async fn host_reports_usage_and_listening_ports() {
    let host = TestHost::new();
    let metrics = eventually("first metrics sample", || async {
        host.conn().await.host_metrics().await.ok()
    })
    .await;
    assert!(metrics.cpus > 0, "{metrics:?}");
    assert!(metrics.memory.total >= metrics.memory.used, "{metrics:?}");
    assert!(!metrics.disks.is_empty() && !metrics.history.is_empty());

    // Recorded history: a fresh host has no complete minute yet, but answers.
    let mut conn = host.conn().await;
    let day = conn.host_history("24h").await.unwrap();
    assert_eq!(day.resolution_secs, 300);
    let err = conn.host_history("forever").await.unwrap_err();
    assert!(err.to_string().contains("unknown range"), "{err}");
    assert!(host.home().join("state/metrics").is_dir());

    // A port opened here shows up among the host's listening ports.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ports = host.conn().await.host_ports().await.unwrap();
    assert!(
        ports.iter().any(|p| p.port == port),
        "{port} not in {ports:?}"
    );
}

// ---------------------------------------------------------------------------
// Files and image paste
// ---------------------------------------------------------------------------

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[tokio::test]
async fn files_upload_list_download_in_chunks() {
    use otter_protocol::fs::{EntryKind, FsPath, FsWrite};
    let host = TestHost::new();
    create(&host, "files", Some(vec![])).await;
    let mut conn = host.conn().await;
    let at = |path: &str| FsPath {
        workspace: "files".into(),
        path: path.into(),
    };
    // Upload in two chunks.
    let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (a, b) = content.split_at(200_000);
    conn.fs_write(FsWrite {
        at: at("data.bin"),
        offset: 0,
        data: b64(a),
        create: true,
        overwrite: false,
    })
    .await
    .unwrap();
    conn.fs_write(FsWrite {
        at: at("data.bin"),
        offset: a.len() as u64,
        data: b64(b),
        create: false,
        overwrite: false,
    })
    .await
    .unwrap();
    // Creating it again without overwrite is refused.
    let err = conn
        .fs_write(FsWrite {
            at: at("data.bin"),
            offset: 0,
            data: b64(b"x"),
            create: true,
            overwrite: false,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    let listing = conn.fs_list("files", "").await.unwrap();
    let entry = listing
        .entries
        .iter()
        .find(|e| e.name == "data.bin")
        .unwrap();
    assert_eq!((entry.kind, entry.size), (EntryKind::File, 300_000));

    // Download in 128 KB chunks.
    let mut got = Vec::new();
    loop {
        let chunk = conn
            .fs_read("files", "data.bin", got.len() as u64, 128 * 1024)
            .await
            .unwrap();
        use base64::Engine;
        got.extend(
            base64::engine::general_purpose::STANDARD
                .decode(&chunk.data)
                .unwrap(),
        );
        if chunk.eof {
            break;
        }
    }
    assert_eq!(got, content);
}

#[tokio::test]
async fn pasted_image_reaches_wl_paste_in_the_session() {
    use std::os::unix::fs::PermissionsExt;
    // A Mac host's pasteboard gets the image too (D-145): a stand-in for
    // `osascript` keeps what it was given, off the real clipboard.
    let fake = tempfile::Builder::new().prefix("osa").tempdir().unwrap();
    let pasteboard = fake.path().join("pasteboard.png");
    let osascript = fake.path().join("osascript");
    std::fs::write(
        &osascript,
        format!(
            "#!/bin/sh\nfor a; do last=$a; done\ncp \"$last\" '{}'\n",
            pasteboard.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&osascript, std::fs::Permissions::from_mode(0o755)).unwrap();
    let host =
        TestHost::with_env(&[("OTTER_OSASCRIPT", osascript.to_string_lossy().into_owned())]).await;
    sh_session(&host, "paste").await;
    let png = b"\x89PNG\r\n\x1a\n-not-really-an-image-";
    host.conn()
        .await
        .paste_image("paste", "sh", b64(png))
        .await
        .unwrap();
    // What Claude Code runs on Ctrl+V, inside the session.
    let mut conn = host.conn().await;
    conn.session_write(
        "paste",
        "sh",
        "echo types=$(wl-paste -l) bytes=$(wl-paste --type image/png | wc -c | tr -d ' ')",
        true,
    )
    .await
    .unwrap();
    wait_output(
        &host,
        "paste",
        "sh",
        &format!("types=image/png bytes={}", png.len()),
    )
    .await;
    if cfg!(target_os = "macos") {
        assert_eq!(std::fs::read(&pasteboard).unwrap(), png);
    } else {
        assert!(!pasteboard.exists());
    }
    // Not a PNG: refused.
    let err = host
        .conn()
        .await
        .paste_image("paste", "sh", b64(b"GIF89a"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a PNG"), "{err}");
}

// ---------------------------------------------------------------------------
// Browser login
// ---------------------------------------------------------------------------

/// Keep `login/sh` away from the test machine's own openers: its PATH
/// becomes otterd's stand-ins, then a host `xdg-open` of the test's, then
/// the system's — no desktop, no `~/.local/bin`. That `xdg-open` stands for
/// another tool's stand-in (as some install in `~/.local/bin`): it records
/// what it was asked to open, then hands over to the first other `xdg-open`
/// on PATH, which is otterd's. Returns what it recorded so far.
async fn host_opener(host: &TestHost) -> impl Fn() -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = host.home().join("host-opener");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("opened");
    std::fs::write(&log, "").unwrap();
    let opener = dir.join("xdg-open");
    std::fs::write(
        &opener,
        format!(
            "#!/bin/sh\necho \"$1\" >> '{}'\nself=$(cd \"$(dirname \"$0\")\" && pwd)/xdg-open\nIFS=:\nfor d in $PATH; do\n  [ -x \"$d/xdg-open\" ] && [ \"$d/xdg-open\" != \"$self\" ] && exec \"$d/xdg-open\" \"$@\"\ndone\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
    host.conn()
        .await
        .session_write(
            "login",
            "sh",
            &format!(
                "export PATH=\"${{PATH%%:*}}:{}:/usr/bin:/bin\"; unset DISPLAY WAYLAND_DISPLAY; echo isolated-$((40+2))",
                dir.display()
            ),
            true,
        )
        .await
        .unwrap();
    wait_output(host, "login", "sh", "isolated-42").await;
    move || {
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }
}

#[tokio::test]
async fn browser_login_opens_trusted_pages_through_a_connected_app() {
    let host = TestHost::new();
    sh_session(&host, "login").await;
    let opened_on_host = host_opener(&host).await;
    let mut conn = host.conn().await;

    // No app connected: the tool hears why, and the host's own opener gets
    // its turn — once, though it hands straight back.
    conn.session_write(
        "login",
        "sh",
        "xdg-open https://github.com/login/device; echo first-done-$((40+2))",
        true,
    )
    .await
    .unwrap();
    let out = wait_output(&host, "login", "sh", "first-done-42").await;
    assert!(
        out.contains("no Otter app or attached terminal is connected"),
        "{out}"
    );
    assert_eq!(opened_on_host(), ["https://github.com/login/device"]);

    // An app subscribes as the browser.
    let mut stream = host.conn().await.subscribe_for_browser(None).await.unwrap();
    let aws = "https://oidc.us-east-1.amazonaws.com/authorize?client_id=x&redirect_uri=http%3A%2F%2F127.0.0.1%3A37265%2Foauth%2Fcallback";
    conn.session_write(
        "login",
        "sh",
        &format!("xdg-open '{aws}'; echo ok-$?; echo browser=$BROWSER"),
        true,
    )
    .await
    .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rec = stream.next().await.unwrap().unwrap();
            if let Event::BrowserOpenRequested {
                request_id,
                provider,
                callback_port,
                workspace_id,
            } = rec.event
            {
                return (request_id, provider, callback_port, workspace_id);
            }
        }
    })
    .await
    .expect("BrowserOpenRequested");
    assert_eq!(event.1, "oidc.us-east-1.amazonaws.com");
    assert_eq!(event.2, Some(37265));
    assert!(event.3.is_some(), "attributed to the workspace");
    wait_output(&host, "login", "sh", "browser=otter-open").await;
    // The URL stays out of the event log, and is taken once.
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(!log.contains("client_id=x"), "URL leaked into the log");
    let opening = conn.browser_take(&event.0).await.unwrap();
    assert_eq!(opening.url, aws);
    assert!(conn.browser_take(&event.0).await.is_err());

    // Not a provider built in: the Mac decides (D-121). The tool hears so
    // and prints its link as it would; the app is asked, by site only.
    conn.session_write(
        "login",
        "sh",
        "xdg-open 'https://sso.example.com/start?token=secret'; echo asked-done-$((40+2))",
        true,
    )
    .await
    .unwrap();
    let (request_id, site) = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let rec = stream.next().await.unwrap().unwrap();
            match rec.event {
                Event::BrowserOpenAsked {
                    request_id, site, ..
                } => return (request_id, site),
                Event::BrowserOpenRequested { .. } => panic!("not to be opened at once"),
                _ => {}
            }
        }
    })
    .await
    .expect("BrowserOpenAsked");
    assert_eq!(site, "sso.example.com");
    let out = wait_output(&host, "login", "sh", "asked-done-42").await;
    assert!(out.contains("asks whether to open it"), "{out}");
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(!log.contains("token=secret"), "URL leaked into the log");
    // Allowed on the Mac, it's taken like any other page, once.
    let opening = conn.browser_take(&request_id).await.unwrap();
    assert_eq!(opening.url, "https://sso.example.com/start?token=secret");
    assert!(conn.browser_take(&request_id).await.is_err());
}

#[tokio::test]
async fn archive_stops_sessions_and_unarchive_brings_the_workspace_back() {
    let host = TestHost::new();
    let ws = create(
        &host,
        "shelf",
        Some(vec![
            spec(SessionKind::Service, "server", "sleep 600"),
            spec(SessionKind::Command, "tests", "exit 3"),
        ]),
    )
    .await;
    std::fs::write(Path::new(&ws.root).join("notes.txt"), "keep me").unwrap();
    let server = ws.session("server").unwrap().clone();
    let server_exec = server.current_execution().unwrap().clone();
    let tests_exec = wait_status(&host, "shelf", "tests", SessionStatus::Failed)
        .await
        .session("tests")
        .unwrap()
        .current_execution()
        .unwrap()
        .clone();
    let ws = eventually("the failed command asks for attention", || async {
        let w = host.conn().await.workspace_get("shelf").await.unwrap();
        (!w.attention.is_empty()).then_some(w)
    })
    .await;
    assert!(host.tmux_has_session(&server_exec.backend_ref));

    let mut stream = host.conn().await.subscribe(None).await.unwrap();
    let mut conn = host.conn().await;
    let archived = conn.workspace_archive("shelf").await.unwrap();
    assert_eq!(archived.state, WorkspaceState::Archived);
    assert_eq!(archived.activity(), otter_core::Activity::Archived);
    assert!(archived.attention.is_empty());
    let s = archived.session("server").unwrap();
    assert_eq!(
        (s.id.clone(), s.status()),
        (server.id.clone(), SessionStatus::Stopped)
    );
    assert_eq!(
        archived.session("tests").unwrap().status(),
        SessionStatus::Failed
    );
    // Running processes are stopped; an exited one keeps its last screen;
    // the files are kept.
    assert!(!host.tmux_has_session(&server_exec.backend_ref));
    assert!(host.tmux_has_session(&tests_exec.backend_ref));
    conn.session_read("shelf", "tests", None).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&ws.root).join("notes.txt")).unwrap(),
        "keep me"
    );

    let mut kinds = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(rec) = stream.next().await.unwrap() {
            kinds.push(rec.event.kind());
            if rec.event.kind() == "WorkspaceArchived" {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("events so far: {kinds:?}"));
    assert_eq!(
        kinds,
        ["SessionStopped", "AttentionResolved", "WorkspaceArchived"]
    );

    // Nothing runs or starts in an archived workspace.
    let err = conn.workspace_archive("shelf").await.unwrap_err();
    assert!(err.to_string().contains("already archived"), "{err}");
    let err = conn
        .session_create(SessionCreate {
            workspace: "shelf".into(),
            spec: spec(SessionKind::Command, "more", "exit 0"),
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unarchive it first"), "{err}");
    let err = conn.session_restart("shelf", "server").await.unwrap_err();
    assert!(err.to_string().contains("archived"), "{err}");
    let err = conn.workspace_prepare("shelf").await.unwrap_err();
    assert!(err.to_string().contains("unarchive it first"), "{err}");
    let err = conn.workspace_unarchive("nope").await.unwrap_err();
    assert!(err.to_string().contains("no workspace"), "{err}");

    // Archived survives a daemon restart, still quiet.
    host.stop_daemon().await;
    tokio::time::sleep(Duration::from_millis(1200)).await; // a few reconcile ticks
    let ws = host.conn().await.workspace_get("shelf").await.unwrap();
    assert_eq!(ws.state, WorkspaceState::Archived);
    assert!(ws.attention.is_empty());
    assert_eq!(
        ws.session("server").unwrap().status(),
        SessionStatus::Stopped
    );
    assert!(ws.sessions.iter().all(|s| s.executions.len() == 1));
    let listed = host.conn().await.workspace_list().await.unwrap();
    assert_eq!(
        listed.len(),
        1,
        "workspace.list still returns archived ones"
    );

    // Unarchiving prepares it again; sessions stay stopped until restarted,
    // which starts a new execution of the same session.
    let mut conn = host.conn().await;
    let back = conn.workspace_unarchive("shelf").await.unwrap();
    let back = if back.state == WorkspaceState::Preparing {
        wait_prepared(&host, "shelf").await
    } else {
        back
    };
    assert_eq!(
        back.state,
        WorkspaceState::Ready,
        "{:?}",
        back.state_message
    );
    assert_eq!(
        back.session("server").unwrap().status(),
        SessionStatus::Stopped
    );
    assert_eq!(back.session("server").unwrap().executions.len(), 1);
    let err = conn.workspace_unarchive("shelf").await.unwrap_err();
    assert!(err.to_string().contains("not archived"), "{err}");
    let restarted = conn.session_restart("shelf", "server").await.unwrap();
    assert_eq!(restarted.id, server.id);
    assert_eq!(restarted.executions.len(), 2);
    assert_eq!(restarted.status(), SessionStatus::Running);
    assert_ne!(restarted.current_execution().unwrap().id, server_exec.id);
}

#[tokio::test]
async fn archived_git_workspace_keeps_its_worktree_until_deleted() {
    let host = TestHost::new();
    let src = make_repo(&host, "src", &[("README.md", "hello\n")]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws(
        "parked",
        &src,
        None,
        vec![spec(SessionKind::Service, "srv", "sleep 600")],
    ))
    .await
    .unwrap();
    let ws = wait_prepared(&host, "parked").await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    let root = Path::new(&ws.root).to_owned();
    std::fs::write(root.join("wip.txt"), "unpushed").unwrap();

    conn.workspace_archive("parked").await.unwrap();
    let base = host
        .home()
        .join("repos")
        .join(&git_source(&ws).repo_id)
        .join("base");
    let worktrees = git(&base, &["worktree", "list", "--porcelain"]);
    assert!(worktrees.contains(&ws.root), "{worktrees}");
    assert!(root.join("wip.txt").is_file());

    // Back again: same worktree, same uncommitted work.
    conn.workspace_unarchive("parked").await.unwrap();
    let ws = wait_prepared(&host, "parked").await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    assert_eq!(git_source(&ws).branch, "otterd/parked");
    assert!(root.join("wip.txt").is_file());

    // Deleting an archived workspace still protects uncommitted work, then
    // removes the worktree, its branch, and (the last one on it) the
    // repository's copy (D-108).
    conn.workspace_archive("parked").await.unwrap();
    let err = conn.workspace_delete("parked", false).await.unwrap_err();
    assert!(err.to_string().contains("uncommitted"), "{err}");
    conn.workspace_delete("parked", true).await.unwrap();
    assert!(!root.exists());
    assert!(!base.exists());
}

#[tokio::test]
async fn browser_login_opens_pages_from_attached_terminals_once() {
    use otter_client::login::{Logins, Opener};
    use std::sync::{Arc, Mutex};

    let host = TestHost::new();
    sh_session(&host, "login").await;
    let opened_on_host = host_opener(&host).await;
    let mut conn = host.conn().await;

    // Two clients serve browser login at once (`otter attach` in two
    // terminals, or one beside the app); each opener records what it opens.
    let opened = Arc::new(Mutex::new(Vec::<(usize, String)>::new()));
    let opener = |n: usize| -> Opener {
        let opened = opened.clone();
        Arc::new(move |url: &str| {
            opened.lock().unwrap().push((n, url.to_owned()));
            Ok(())
        })
    };
    let first = Logins::start(host.transport.clone(), "test", opener(1), vec![])
        .await
        .unwrap();
    let second = Logins::start(host.transport.clone(), "test", opener(2), vec![])
        .await
        .unwrap();

    let github = "https://github.com/login/device";
    conn.session_write(
        "login",
        "sh",
        &format!("xdg-open '{github}'; echo ok-$((40+$?))"),
        true,
    )
    .await
    .unwrap();
    wait_output(&host, "login", "sh", "ok-40").await;
    let opened_url = eventually("the page opens", || {
        let first = opened.lock().unwrap().first().map(|(_, u)| u.clone());
        async move { first }
    })
    .await;
    assert_eq!(opened_url, github);
    // Taken once: the other client doesn't open it too.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        opened.lock().unwrap().len(),
        1,
        "{:?}",
        opened.lock().unwrap()
    );

    // Once both stop, nothing serves it and the tool hears why.
    first.stop().await;
    second.stop().await;
    conn.session_write(
        "login",
        "sh",
        &format!("xdg-open '{github}'; echo after-done-$((40+2))"),
        true,
    )
    .await
    .unwrap();
    let out = wait_output(&host, "login", "sh", "after-done-42").await;
    assert!(
        out.contains("no Otter app or attached terminal is connected"),
        "{out}"
    );
    assert_eq!(opened.lock().unwrap().len(), 1);
    // Only then the host's own opener: served, the page didn't go there.
    assert_eq!(opened_on_host(), [github]);
}

#[tokio::test]
async fn a_terminal_opens_other_sign_in_sites_only_if_this_mac_allowed_them() {
    use otter_client::login::{Logins, Opener};
    use std::sync::{Arc, Mutex};

    let host = TestHost::new();
    sh_session(&host, "login").await;
    let _opened_on_host = host_opener(&host).await;
    let mut conn = host.conn().await;
    let opened = Arc::new(Mutex::new(Vec::<String>::new()));
    let record = opened.clone();
    let opener: Opener = Arc::new(move |url: &str| {
        record.lock().unwrap().push(url.to_owned());
        Ok(())
    });
    // This Mac allowed one site of its own (D-121).
    let logins = Logins::start(
        host.transport.clone(),
        "test",
        opener,
        vec!["*.example.com".into()],
    )
    .await
    .unwrap();
    let mut stream = host.conn().await.subscribe(None).await.unwrap();

    let allowed = "https://sso.example.com/start";
    conn.session_write("login", "sh", &format!("xdg-open '{allowed}'"), true)
        .await
        .unwrap();
    let got = eventually("the allowed site opens", || {
        let first = opened.lock().unwrap().first().cloned();
        async move { first }
    })
    .await;
    assert_eq!(got, allowed);

    // Another site: not opened here, and left for the app to ask about.
    conn.session_write("login", "sh", "xdg-open 'https://other.test/login'", true)
        .await
        .unwrap();
    let request_id = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Event::BrowserOpenAsked {
                request_id, site, ..
            } = stream.next().await.unwrap().unwrap().event
                && site == "other.test"
            {
                return request_id;
            }
        }
    })
    .await
    .expect("asked about other.test");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        opened.lock().unwrap().len(),
        1,
        "{:?}",
        opened.lock().unwrap()
    );
    assert!(
        conn.browser_take(&request_id).await.is_ok(),
        "still the app's to take"
    );
    logins.stop().await;
}

#[tokio::test]
async fn brief_is_set_at_creation_and_edited_later() {
    let host = TestHost::new();
    let mut conn = host.conn().await;
    let ws = conn
        .workspace_create(WorkspaceCreate {
            name: "why".into(),
            brief: Some(otter_core::Brief {
                description: Some("  Fix renewal validation \n".into()),
                title: Some(" ".into()),
                ..Default::default()
            }),
            sessions: Some(vec![]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        ws.brief.description.as_deref(),
        Some("Fix renewal validation")
    );
    assert_eq!(ws.brief.title, None, "blank text is absent, not empty");

    let mut stream = host.conn().await.subscribe(None).await.unwrap();
    let mut brief = ws.brief.clone();
    brief.title = Some("GCP renewal".into());
    brief.decisions = vec!["Renewal count includes the initial term.".into(), "".into()];
    let edited = conn.workspace_set_brief("why", brief).await.unwrap();
    assert_eq!(edited.brief.title.as_deref(), Some("GCP renewal"));
    assert_eq!(
        edited.brief.description.as_deref(),
        Some("Fix renewal validation")
    );
    assert_eq!(edited.brief.decisions.len(), 1);
    assert_eq!(conn.workspace_get("why").await.unwrap().brief, edited.brief);
    let rec = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        rec.event,
        Event::WorkspaceBriefChanged {
            workspace_id: ws.id.clone()
        }
    );
    // The event names the workspace; what the brief says stays out of the log.
    let log = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(log.contains("WorkspaceBriefChanged"), "{log}");
    assert!(!log.contains("Renewal count"), "{log}");

    // Setting the same brief again changes nothing; replacing clears fields.
    conn.workspace_set_brief("why", edited.brief.clone())
        .await
        .unwrap();
    let cleared = conn
        .workspace_set_brief("why", otter_core::Brief::default())
        .await
        .unwrap();
    assert!(cleared.brief.is_empty());
    let rec = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(rec.seq > 0 && rec.event.kind() == "WorkspaceBriefChanged");
    assert_eq!(
        host.conn()
            .await
            .events_list(None)
            .await
            .unwrap()
            .iter()
            .filter(|r| r.event.kind() == "WorkspaceBriefChanged")
            .count(),
        2,
        "an unchanged brief emits nothing"
    );

    // Archived workspaces can still be described.
    conn.workspace_archive("why").await.unwrap();
    let ws = conn
        .workspace_set_brief(
            "why",
            otter_core::Brief {
                description: Some("parked".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ws.state, WorkspaceState::Archived);
    assert_eq!(ws.brief.description.as_deref(), Some("parked"));
    let err = conn
        .workspace_set_brief("nope", otter_core::Brief::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no workspace"), "{err}");
}

// ---------------------------------------------------------------------------
// Tasks (D-043)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_message_keeps_its_pictures_with_the_task() {
    use base64::Engine;
    use otter_core::task::MessageRole;
    use otter_protocol::task::{TaskCreate, TaskImage};

    let host = TestHost::with_env(&[("OTTER_CONTROLLER", "off".into())]).await;
    create(&host, "pic", Some(vec![])).await;
    let mut conn = host.conn().await;
    let f = conn
        .task_create(TaskCreate {
            command_id: "pic-create".into(),
            title: "Match the mock".into(),
            request: "Make the page look like the mock".into(),
            workspace: Some("pic".into()),
            answer_only: false,
        })
        .await
        .unwrap();
    let id = f.id.as_str();
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(b"pixels");
    let image = |bytes: &[u8]| TaskImage {
        media_type: "image/png".into(),
        data: b64(bytes),
    };

    // Pictures alone are a message; they are kept, and named on it.
    let sent = conn
        .task_send_with_images("pic-1", id, "", vec![image(&png)])
        .await
        .unwrap();
    let message = sent
        .messages
        .iter()
        .rfind(|m| m.role == MessageRole::User)
        .unwrap();
    assert_eq!(message.text, otter_core::task::PICTURES_ONLY);
    assert_eq!(message.attachments.len(), 1);
    let kept = conn
        .task_artifact(id, &message.attachments[0])
        .await
        .unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(kept.data)
            .unwrap(),
        png
    );
    // The file is named for the message, and the history says which message.
    assert!(
        message.attachments[0].contains(message.id.as_str()),
        "{:?}",
        message.attachments
    );
    let history = serde_json::to_string(&conn.task_events(id, None).await.unwrap()).unwrap();
    assert!(history.contains(message.id.as_str()), "{history}");

    // Sent again (a lost reply): still one message, one picture.
    let again = conn
        .task_send_with_images("pic-1", id, "", vec![image(&png)])
        .await
        .unwrap();
    assert_eq!(again.messages.len(), sent.messages.len());
    let dir = host.home().join("state/tasks").join(id).join("artifacts");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    // Sent again with other pictures under the same command: still the first
    // message, and its picture is as it was.
    let mut other = png.clone();
    other.extend_from_slice(b" and more");
    conn.task_send_with_images("pic-1", id, "", vec![image(&other)])
        .await
        .unwrap();
    let kept = conn
        .task_artifact(id, &message.attachments[0])
        .await
        .unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(kept.data)
            .unwrap(),
        png
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

    // What can't be a picture, or is too much, is refused whole.
    let refused = |what: &str, r: Result<otter_core::task::Task, _>| {
        assert!(r.is_err(), "{what} should be refused");
    };
    refused(
        "a gif",
        conn.task_send_with_images(
            "pic-gif",
            id,
            "x",
            vec![TaskImage {
                media_type: "image/gif".into(),
                data: b64(&png),
            }],
        )
        .await,
    );
    refused(
        "base64 that isn't",
        conn.task_send_with_images(
            "pic-b64",
            id,
            "x",
            vec![TaskImage {
                media_type: "image/png".into(),
                data: "not base64!".into(),
            }],
        )
        .await,
    );
    refused(
        "a jpeg that is a png",
        conn.task_send_with_images(
            "pic-type",
            id,
            "x",
            vec![TaskImage {
                media_type: "image/jpeg".into(),
                data: b64(&png),
            }],
        )
        .await,
    );
    refused(
        "five pictures",
        conn.task_send_with_images("pic-five", id, "x", vec![image(&png); 5])
            .await,
    );
    let mut big = png.clone();
    big.resize(700 * 1024 + 1, 0);
    refused(
        "a picture over 700 KB",
        conn.task_send_with_images("pic-big", id, "x", vec![image(&big)])
            .await,
    );
    let mut large = png.clone();
    large.resize(600 * 1024, 0);
    refused(
        "pictures over 2 MB together",
        conn.task_send_with_images("pic-all", id, "x", vec![image(&large); 4])
            .await,
    );
    refused(
        "nothing at all",
        conn.task_send_with_images("pic-none", id, "", vec![]).await,
    );
    assert_eq!(
        conn.task_get(id).await.unwrap().messages.len(),
        sent.messages.len()
    );
    // And a refused message leaves nothing behind.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}

#[tokio::test]
async fn tasks_persist_dedupe_commands_and_replay_after_a_restart() {
    use otter_core::task::{MessageRole, TaskAction, TaskStatus};
    use otter_protocol::task::TaskCreate;

    // The state machine on its own: no Control Agent stepping it.
    let mut host = TestHost::with_env(&[("OTTER_CONTROLLER", "off".into())]).await;
    create(&host, "fw", Some(vec![])).await;
    let mut conn = host.conn().await;
    let created = TaskCreate {
        command_id: "cmd-create-1".into(),
        title: "Export CSV".into(),
        request: "Export the timeline as CSV".into(),
        workspace: Some("fw".into()),
        answer_only: false,
    };
    let f = conn.task_create(created.clone()).await.unwrap();
    assert_eq!(f.status, TaskStatus::Draft);
    assert!(f.workspace_id.is_some());
    // The same create again (a retry after a lost reply) is the same task.
    let again = conn.task_create(created).await.unwrap();
    assert_eq!(again.id, f.id);
    assert_eq!(conn.task_list().await.unwrap().len(), 1);

    // A message delivered twice is added once.
    let id = f.id.as_str();
    conn.task_send("cmd-msg-1", id, "Include session names")
        .await
        .unwrap();
    let f = conn
        .task_send("cmd-msg-1", id, "Include session names")
        .await
        .unwrap();
    let user: Vec<_> = f
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .collect();
    assert_eq!(user.len(), 2, "request + one message");

    // A client following the task remembers where it was.
    let seen = f.history_seq;
    let snapshot = conn.snapshot().await.unwrap();
    drop(conn);

    // While it's away: the task starts, and the daemon restarts.
    host.conn()
        .await
        .task_act("cmd-start", id, TaskAction::Start)
        .await
        .unwrap();
    host.restart().await;

    // Everything is still there after the restart...
    let mut conn = host.conn().await;
    let f = conn.task_get(id).await.unwrap();
    assert_eq!(f.status, TaskStatus::Planning);
    assert_eq!(f.messages.len(), 2);
    // ...the host's event stream says the task changed...
    let mut stream = host
        .conn()
        .await
        .subscribe(Some(snapshot.cursor()))
        .await
        .unwrap();
    let head = stream.seq;
    let missed = events_through(&mut stream, head).await;
    assert!(missed.iter().any(|r| matches!(
        &r.event,
        Event::TaskChanged { task_id, status: TaskStatus::Planning, .. } if task_id == &f.id
    )));
    // ...and the task's own history replays exactly what was missed, in order.
    let replay = conn.task_events(id, Some(seen)).await.unwrap();
    let seqs: Vec<u64> = replay.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, (seen + 1..=f.history_seq).collect::<Vec<_>>());
    assert!(
        replay
            .iter()
            .all(|r| r.correlation_id.as_deref() == Some("cmd-start"))
    );
    assert!(replay.iter().any(|r| r.text.starts_with("Planning")));

    // The repeated start is harmless; an impossible action is refused.
    let f2 = conn
        .task_act("cmd-start", id, TaskAction::Start)
        .await
        .unwrap();
    assert_eq!(f2.history_seq, f.history_seq);
    let err = conn
        .task_act(
            "cmd-accept",
            id,
            TaskAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("review"), "{err:#}");

    // Tasks are kept apart from workspace state.
    let state = std::fs::read_to_string(host.home().join("state/state.json")).unwrap();
    assert!(!state.contains(id));
}

// ---------------------------------------------------------------------------
// The Control Agent driving managed runs (D-044, D-045), with the fake
// `claude -p` (tests/fake_claude_stream.sh) and the deterministic brains.
// ---------------------------------------------------------------------------

/// A project with a `make test` check, a task on it asking for
/// `request`, started. Returns the task id.
async fn started_task(host: &TestHost, request: &str) -> String {
    started_in(host, "proj", request).await
}

/// Like [`started_task`], in a workspace of its own (several per host).
async fn started_task_named(host: &TestHost, request: &str) -> String {
    let name: String = request
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    started_in(host, &name, request).await
}

async fn started_in(host: &TestHost, ws: &str, request: &str) -> String {
    started_with(host, ws, request, "test:\n\t@echo all good\n", false).await
}

/// A project `ws` with this `Makefile` (a Git repository with it committed,
/// if `git`), a task on it asking for `request`, started.
async fn started_with(
    host: &TestHost,
    ws: &str,
    request: &str,
    makefile: &str,
    git: bool,
) -> String {
    use otter_core::task::TaskAction;
    let dir = host.home().join(ws);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), makefile).unwrap();
    if git {
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "start",
            ],
        ] {
            let ok = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
    }
    let mut conn = host.conn().await;
    conn.workspace_create(WorkspaceCreate {
        name: ws.into(),
        source: SourceSpec::Directory {
            path: dir.to_string_lossy().into_owned(),
        },
        sessions: Some(vec![]),
        ..Default::default()
    })
    .await
    .unwrap();
    let f = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: format!("create-{ws}"),
            title: "CSV export".into(),
            request: request.into(),
            workspace: Some(ws.into()),
            answer_only: false,
        })
        .await
        .unwrap();
    conn.task_act("start", f.id.as_str(), TaskAction::Start)
        .await
        .unwrap();
    f.id.to_string()
}

/// A task that changes nothing has nothing to publish (B11, D-166): it
/// isn't asked to, nothing is pushed and no pull request is opened; the
/// pull request and CI gates don't apply, and it waits for acceptance with
/// its report, as finished work does.
#[tokio::test]
async fn a_task_that_changes_nothing_has_nothing_to_publish() {
    use otter_core::task::{GateStatus, MessageRole, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, origin) = git_task_asking(&fake, "Check the export is already there").await;
    let f = wait_task(&fake.host, &id, "the report", |f| f.report.is_some()).await;
    assert_eq!(f.status, TaskStatus::Review, "{:?}", f.status_reason);
    assert!(
        !f.decisions.iter().any(|d| d.summary.contains("git push")),
        "not asked to publish: {:?}",
        f.decisions
    );
    for name in ["Pull request", "CI"] {
        let g = f.gates.iter().find(|g| g.name == name).unwrap();
        assert_eq!(g.status, GateStatus::NotApplicable, "{g:?}");
        assert_eq!(g.detail.as_deref(), Some("nothing to publish"), "{g:?}");
    }
    assert!(f.delivery.is_none());
    // Where its own work started: the workspace's commit as it began (D-167).
    let main = git(&origin, &["rev-parse", "main"]).trim().to_owned();
    assert_eq!(
        f.started_from.as_ref().map(|s| s.revision.as_str()),
        Some(main.as_str())
    );
    // Said once, however often review is stepped.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let f = fake.host.conn().await.task_get(&id).await.unwrap();
    let said: Vec<&str> = f
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Controller && m.text.starts_with("Nothing to publish"))
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(
        said,
        [
            "Nothing to publish: this task left the code as it found it, so there's no pull request to open."
        ]
    );
    assert!(
        git(&origin, &["branch", "--list", "feature/csv"])
            .trim()
            .is_empty(),
        "nothing pushed"
    );
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap_or_default();
    assert!(!calls.contains("pr create"), "{calls}");
    let f = fake
        .host
        .conn()
        .await
        .task_act(
            "accept",
            &id,
            TaskAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Done);
}

/// The second task on a branch the first published to (B09, B11): the
/// first's commit is below it and not on the base it was made from (as
/// when the base is stale, or the pull request not merged yet). The second
/// changes nothing: measured from where it started (D-167), its working
/// tree has no changes, and it has nothing of its own to publish (D-166).
#[tokio::test]
async fn a_second_task_on_the_branch_is_measured_from_where_it_started() {
    use otter_core::task::{EvidenceKind, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let (first, origin) = git_task(&fake).await;
    allow_publish(host, &first).await;
    let a = wait_task(host, &first, "published", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    let pushed = a.delivery.unwrap().head.unwrap();
    let mut conn = host.conn().await;
    conn.task_act(
        "accept-first",
        &first,
        TaskAction::Accept {
            override_gates: true,
        },
    )
    .await
    .unwrap();
    let second = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "create-second".into(),
            title: "Check the export".into(),
            request: "Check the export is there".into(),
            workspace: Some("csv".into()),
            answer_only: false,
        })
        .await
        .unwrap()
        .id
        .to_string();
    conn.task_act("start-second", &second, TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &second, "the report", |f| f.report.is_some()).await;
    assert_eq!(f.status, TaskStatus::Review, "{:?}", f.status_reason);
    assert_eq!(
        f.started_from.as_ref().map(|s| s.revision.as_str()),
        Some(pushed.as_str())
    );
    let tree = f
        .evidence
        .iter()
        .rev()
        .find(|e| e.kind == EvidenceKind::Diff && e.title.starts_with("Working tree"))
        .unwrap();
    assert_eq!(tree.title, "Working tree: no changes");
    let detail = tree.detail.as_deref().unwrap();
    assert!(
        detail.contains("changes since this task started"),
        "{detail}"
    );
    assert!(!detail.contains("timeline.csv"), "{detail}");
    assert!(
        !f.decisions.iter().any(|d| d.summary.contains("git push")),
        "{:?}",
        f.decisions
    );
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap();
    assert_eq!(calls.matches("gh pr create").count(), 1, "{calls}");
    assert_eq!(git(&origin, &["rev-parse", "feature/csv"]).trim(), pushed);
}

/// GitHub refusing the pull request because the branch has no commit its
/// base doesn't is nothing to publish, not a failure (B11, D-166), and it
/// isn't tried again.
#[tokio::test]
async fn no_commits_between_is_nothing_to_publish() {
    use otter_core::task::{GateStatus, MessageRole, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    std::fs::write(fake.gh.join("no-commits"), "").unwrap();
    let (id, origin) = git_task(&fake).await;
    allow_publish(&fake.host, &id).await;
    let f = wait_task(&fake.host, &id, "the report", |f| f.report.is_some()).await;
    assert_eq!(f.status, TaskStatus::Review, "{:?}", f.status_reason);
    for name in ["Pull request", "CI"] {
        let g = f.gates.iter().find(|g| g.name == name).unwrap();
        assert_eq!(g.status, GateStatus::NotApplicable, "{g:?}");
    }
    assert!(
        f.messages.iter().any(|m| m.role == MessageRole::Controller
            && m.text
                == "Nothing to publish: main already has every commit of feature/csv, so there's no pull request to open."),
        "{:?}",
        f.messages
    );
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let f = fake.host.conn().await.task_get(&id).await.unwrap();
    assert_eq!(f.status, TaskStatus::Review);
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap();
    assert_eq!(calls.matches("gh pr create").count(), 1, "{calls}");
    // The push happened: the branch is on the remote, and kept with it.
    let d = f.delivery.clone().expect("the push is recorded");
    assert_eq!(d.branch, "feature/csv");
    assert!(d.pushed_at.is_some() && d.pr_url.is_none(), "{d:?}");
    assert_eq!(
        d.head.as_deref(),
        Some(git(&origin, &["rev-parse", "feature/csv"]).trim())
    );
    // So cancelling names it, as it names anything it leaves (D-068).
    let f = fake
        .host
        .conn()
        .await
        .task_act("cancel", &id, TaskAction::Cancel)
        .await
        .unwrap();
    let said = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Controller)
        .unwrap();
    assert!(
        said.text.contains("its branch `feature/csv`"),
        "{}",
        said.text
    );
}

/// Only files that may hold secrets changed: a commit leaves them out, so
/// there is nothing to publish, and the message says what changed rather
/// than that nothing did (D-166).
#[tokio::test]
async fn only_secrets_changed_is_nothing_to_publish_and_says_so() {
    use otter_core::task::{MessageRole, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, _) = git_task_asking(&fake, "Set the local settings WRITE:.env").await;
    let f = wait_task(&fake.host, &id, "the report", |f| f.report.is_some()).await;
    assert_eq!(f.status, TaskStatus::Review, "{:?}", f.status_reason);
    assert!(!f.decisions.iter().any(|d| d.summary.contains("git push")));
    let said: Vec<&str> = f
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Controller && m.text.starts_with("Nothing to publish"))
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(
        said,
        [
            "Nothing to publish: only files that may hold secrets changed (.env), which aren't committed, so there's no pull request to open."
        ]
    );
}

/// What a commit would take can't be read (here, a broken index): that is
/// not "nothing to commit", so the developer is asked to publish as before
/// (D-166).
#[tokio::test]
async fn a_status_git_cant_read_is_not_nothing_to_publish() {
    use otter_core::task::{MessageRole, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, _) = git_task_asking(&fake, "Check the export is already there").await;
    let ws = fake.host.conn().await.workspace_get("csv").await.unwrap();
    let root = std::path::PathBuf::from(&ws.root);
    let index = root.join(git(&root, &["rev-parse", "--git-path", "index"]).trim());
    std::fs::write(&index, "not an index").unwrap();
    let f = wait_task(&fake.host, &id, "asked to publish", |f| {
        f.status == TaskStatus::Review
            && f.pending_decisions()
                .any(|d| d.summary.contains("git push"))
    })
    .await;
    assert!(
        !f.messages
            .iter()
            .any(|m| m.role == MessageRole::Controller && m.text.starts_with("Nothing to publish")),
        "{:?}",
        f.messages
    );
    assert!(
        !f.rationale
            .iter()
            .any(|r| r.starts_with("Nothing to publish"))
    );
}

async fn wait_task(
    host: &TestHost,
    id: &str,
    what: &str,
    pred: impl Fn(&otter_core::task::Task) -> bool,
) -> otter_core::task::Task {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let f = host.conn().await.task_get(id).await.unwrap();
        if pred(&f) {
            return f;
        }
        if tokio::time::Instant::now() > deadline {
            let history = host.conn().await.task_events(id, None).await.unwrap();
            let lines: Vec<&str> = history.iter().map(|r| r.text.as_str()).collect();
            panic!(
                "timed out waiting for: {what}\nstatus: {:?} ({:?})\nhistory: {lines:#?}\nrationale: {:?}\nruns: {:?}\nevidence: {:?}",
                f.status,
                f.status_reason,
                f.rationale,
                f.runs,
                f.evidence.last()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_task_goes_from_request_to_review_with_evidence() {
    use otter_core::task::{EvidenceKind, MessageRole, RunState, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "Export the timeline as CSV").await;
    let f = wait_task(&fake.host, &id, "review, with the report", |f| {
        f.status == TaskStatus::Review && f.report.is_some()
    })
    .await;
    // Not a Git workspace: no pull request or CI to wait for.
    use otter_core::task::GateStatus;
    let gate = |name: &str| f.gates.iter().find(|g| g.name == name).unwrap().status;
    assert_eq!(gate("Pull request"), GateStatus::NotApplicable);
    assert_eq!(gate("Local tests"), GateStatus::Passed);
    assert!(f.report.as_deref().unwrap().contains("## Unresolved risks"));
    // Planned, ran, reported, checked.
    assert_eq!(f.steps.len(), 1);
    assert_eq!(f.verify_command.as_deref(), Some("make test"));
    assert!(
        f.verify_approved,
        "make test is routine: no one had to approve it"
    );
    assert_eq!(f.runs.len(), 1);
    assert_eq!(f.runs[0].state, RunState::Completed);
    assert!(
        f.runs[0]
            .provider_session_id
            .as_deref()
            .unwrap()
            .starts_with("fake-session-")
    );
    assert!(
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::Agent && m.text.starts_with("done in"))
    );
    // What it said along the way streamed (and is in its conversation);
    // the task keeps its result, once, and what it ran (D-069).
    assert!(
        !f.messages
            .iter()
            .any(|m| m.role == MessageRole::Agent && m.text == "Did the work.")
    );
    assert_eq!(
        f.messages
            .iter()
            .filter(|m| m.role == MessageRole::Agent)
            .count(),
        1,
        "{:?}",
        f.messages
    );
    assert_eq!(f.runs[0].activity, ["$ make test"]);
    // With its time; when the agent was last heard from isn't kept (D-141).
    assert_eq!(f.runs[0].activity_at.len(), 1);
    assert_eq!((f.runs[0].heard_at, f.runs[0].tools_running), (None, 0));
    // Why Otter did what it did is in the history, at its time.
    let history = fake.host.conn().await.task_events(&id, None).await.unwrap();
    assert!(
        history.iter().any(
            |e| matches!(&e.event, otter_core::task::TaskEvent::Reasoned { .. })
                && e.text.starts_with("Why: Plan (")
        ),
        "{:?}",
        history.iter().map(|e| &e.text).collect::<Vec<_>>()
    );
    assert!(
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::Controller && m.text.starts_with("Plan:"))
    );
    let check = f
        .evidence
        .iter()
        .find(|e| e.kind == EvidenceKind::Test)
        .unwrap();
    assert_eq!(check.ok, Some(true));
    assert!(check.detail.as_deref().unwrap().contains("all good"));
    assert!(f.acceptance.iter().all(|c| c.met == Some(true)));
    assert_eq!(f.budget.iterations_used, 1);
    // The history tells the story in order.
    let kinds: Vec<String> = fake
        .host
        .conn()
        .await
        .task_events(&id, None)
        .await
        .unwrap()
        .iter()
        .map(|r| {
            serde_json::to_value(&r.event).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let pos = |k: &str| {
        kinds
            .iter()
            .position(|x| x == k)
            .unwrap_or_else(|| panic!("{k} in {kinds:?}"))
    };
    assert!(pos("PlanSet") < pos("RunStarted"));
    assert!(pos("RunStarted") < pos("EvidenceAdded"));
    // Done only when the developer accepts.
    let f = fake
        .host
        .conn()
        .await
        .task_act(
            "accept",
            &id,
            TaskAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Done);

    // Done isn't final (D-050): a new ask plans again, keeps what is done,
    // and comes back for review.
    fake.host
        .conn()
        .await
        .task_act(
            "more",
            &id,
            TaskAction::RequestChanges {
                note: Some("Also export JSON".into()),
            },
        )
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "back in review", |f| {
        f.status == TaskStatus::Review && f.steps.len() == 2 && f.report.is_some()
    })
    .await;
    assert_eq!(f.steps[0].status, otter_core::task::StepStatus::Done);
    assert!(
        f.steps[1]
            .detail
            .as_deref()
            .unwrap()
            .contains("Also export JSON")
    );
    assert_eq!(f.runs.len(), 2, "the done step wasn't redone");
    // One conversation for the task (D-064): the new step is a turn in it.
    assert!(f.runs[0].provider_session_id.is_some());
    assert_eq!(f.runs[1].provider_session_id, f.runs[0].provider_session_id);
    assert_eq!(f.runs[1].conversation_id, f.runs[0].conversation_id);
}

#[tokio::test]
async fn an_approval_is_answered_and_the_run_continues() {
    use otter_core::task::{Decider, DecisionStatus, Risk, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "Pick a colour ASK_QUESTION").await;
    // The rules brain doesn't decide: it's the developer's.
    let f = wait_task(&fake.host, &id, "blocked on a decision", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    assert_eq!(d.summary, "Pick one");
    assert_eq!(d.options, ["red", "blue"]);
    assert_eq!(d.risk, Risk::Medium);
    assert!(!d.user_only);
    fake.host
        .conn()
        .await
        .task_act(
            "approve",
            &id,
            TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: Some("blue".into()),
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "review after the approval", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    let d = f.decisions.iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(d.status, DecisionStatus::Answered);
    assert_eq!(d.answer.as_deref(), Some("blue"));
    assert_eq!(d.decided_by, Some(Decider::User));
    assert_eq!(f.runs[0].summary.as_deref(), Some("answered"));
}

#[tokio::test]
async fn ordinary_work_runs_without_asking() {
    use otter_core::task::TaskStatus;
    let fake = fake_agent_host("ok", "rules").await;
    // Installing a dependency, then editing a file: no one is asked (D-049).
    for request in ["Add a dependency ASK_INSTALL", "Write a file ASK_EDIT"] {
        let id = started_task_named(&fake.host, request).await;
        let f = wait_task(&fake.host, &id, "review", |f| {
            f.status == TaskStatus::Review
        })
        .await;
        assert!(
            !f.rationale
                .iter()
                .any(|r| r.contains("Needs your decision")),
            "{:?}",
            f.rationale
        );
        // A tool policy allows is a tool, not a decision (D-069): nothing
        // in the approvals, nothing "low risk" in the history.
        assert!(f.decisions.is_empty(), "{request}: {:?}", f.decisions);
        let history = fake.host.conn().await.task_events(&id, None).await.unwrap();
        assert!(
            !history.iter().any(|r| r.text.contains("decision")),
            "{:?}",
            history.iter().map(|r| &r.text).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn a_model_cannot_approve_what_only_the_developer_may() {
    use otter_core::task::{Decider, DecisionStatus, Risk, TaskAction, TaskStatus};
    // A brain that approves everything it is asked.
    let fake = fake_agent_host("ok", "yes").await;
    let id = started_task(&fake.host, "Clean up ASK_RM").await;
    let f = wait_task(&fake.host, &id, "blocked on the developer", |f| {
        f.status == TaskStatus::Blocked
            && f.rationale
                .iter()
                .any(|r| r.contains("only the developer may approve"))
    })
    .await;
    let d = f.pending_decisions().next().expect("still pending").clone();
    assert_eq!(d.summary, "Run `rm -rf /opt/shared`");
    assert!(d.user_only);
    assert_eq!(d.risk, Risk::High);
    // Some time later, still not approved by anyone but the developer.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let f = fake.host.conn().await.task_get(&id).await.unwrap();
    assert_eq!(
        f.decisions.iter().find(|x| x.id == d.id).unwrap().status,
        DecisionStatus::Pending
    );
    // The developer says no; nobody can turn that around.
    fake.host
        .conn()
        .await
        .task_act(
            "deny",
            &id,
            TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: false,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    // The agent is told who said no (D-066).
    let f = wait_task(&fake.host, &id, "the denial reaches the agent", |f| {
        f.runs.first().is_some_and(|r| {
            r.summary.as_deref() == Some("denied (The developer declined this.), so I stopped")
        })
    })
    .await;
    let d = f.decisions.iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(
        (d.status, d.decided_by),
        (DecisionStatus::Denied, Some(Decider::User))
    );
}

/// D-174: what the policy flags only by a word, and the reflog brings back,
/// is Otter's model's to judge, not the developer's.
#[tokio::test]
async fn otters_model_judges_what_comes_back_from_the_reflog() {
    use otter_core::task::{Decider, DecisionStatus, Risk, TaskStatus};
    let fake = fake_agent_host("ok", "yes").await;
    let id = started_task(&fake.host, "Rebase onto main ASK_REBASE").await;
    let f = wait_task(&fake.host, &id, "review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    let d = &f.decisions[0];
    assert_eq!(d.summary, "Run `git rebase main`");
    assert!(!d.user_only);
    assert_eq!(d.risk, Risk::High);
    assert_eq!(
        (d.status, d.decided_by),
        (DecisionStatus::Approved, Some(Decider::Controller))
    );
    assert_eq!(d.rationale.as_deref(), Some("yes"));
    assert_eq!(f.runs[0].summary.as_deref(), Some("allowed and done"));
}

/// A host with no model, and a model that does not answer in time, leave a
/// judged call to the developer, as before (D-174).
#[tokio::test]
async fn without_a_model_or_in_time_a_judged_call_is_the_developers() {
    use otter_core::task::{DecisionStatus, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "Rebase onto main ASK_REBASE").await;
    let f = wait_task(&fake.host, &id, "blocked on the developer", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap();
    assert!(!d.user_only);

    let late = fake_agent_host_with("ok", "sleepy", |_| {
        vec![("OTTER_JUDGED_DEADLINE_MS", "300".into())]
    })
    .await;
    let id = started_task(&late.host, "Rebase onto main ASK_REBASE").await;
    let f = wait_task(&late.host, &id, "blocked after the deadline", |f| {
        f.status == TaskStatus::Blocked
            && f.rationale.iter().any(|r| r.contains("no answer in time"))
    })
    .await;
    assert_eq!(
        f.decisions[0].status,
        DecisionStatus::Pending,
        "{:?}",
        f.decisions
    );
}

/// A model's denial refuses the call once; asked again, it is the
/// developer's, who may allow it (D-174).
#[tokio::test]
async fn a_model_denial_is_not_final_a_repeat_goes_to_the_developer() {
    use otter_core::task::{Decider, DecisionStatus, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "no").await;
    let id = started_task(&fake.host, "Rebase onto main ASK_REBASE_TWICE").await;
    let f = wait_task(&fake.host, &id, "the repeat waits for the developer", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let first = f
        .decisions
        .iter()
        .find(|d| d.status == DecisionStatus::Denied)
        .expect("the model's denial");
    assert_eq!(first.decided_by, Some(Decider::Controller));
    assert_eq!(first.rationale.as_deref(), Some("not part of this task"));
    let again = f.pending_decisions().next().unwrap().clone();
    assert!(again.user_only, "{again:?}");
    assert!(
        again
            .detail
            .as_deref()
            .unwrap()
            .contains("declined this once")
    );
    fake.host
        .conn()
        .await
        .task_act(
            "approve",
            &id,
            TaskAction::Decide {
                decision_id: again.id.clone(),
                approve: true,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_task(
        &fake.host,
        &id,
        "review after the developer allowed it",
        |f| f.status == TaskStatus::Review,
    )
    .await;
    assert_eq!(f.runs[0].summary.as_deref(), Some("allowed and done"));
    let again = f.decisions.iter().find(|d| d.id == again.id).unwrap();
    assert_eq!(
        (again.status, again.decided_by),
        (DecisionStatus::Approved, Some(Decider::User))
    );
}

/// B06 (D-157): a command too long for a decision's one line is there whole
/// for the developer who decides it, and its credential in nothing Otter
/// keeps.
#[tokio::test]
async fn a_long_command_is_there_whole_for_the_developer() {
    use otter_core::task::TaskStatus;
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let id = started_task(host, "Rename the storage path ASK_LONG").await;
    let f = wait_task(host, &id, "blocked on the developer", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap();
    assert!(d.user_only, "{d:?}");
    // Its one line stops before the `rm` policy matched…
    assert!(
        d.summary.starts_with("Run `python3 - <<'E' import re") && d.summary.ends_with('…'),
        "{}",
        d.summary
    );
    assert!(!d.summary.contains("rm -rf"), "{}", d.summary);
    // …so the whole command is there too, as written, without its credential.
    let command = d.command.as_deref().expect("the whole command");
    assert!(
        command.starts_with("python3 - <<'E'\nimport re\np = \"taskbench/cli.py\"\n"),
        "{command}"
    );
    assert!(command.ends_with(" rm -rf /opt/shared"), "{command}");
    // Kept with its decision, and only there: what comes past the cut goes
    // neither to the host's event log nor to the task's history.
    let state = host.home().join("state");
    let read = |p: std::path::PathBuf| {
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    };
    let task_dir = state.join("tasks").join(&id);
    let doc = read(task_dir.join("task.json"));
    assert!(doc.contains(" rm -rf /opt/shared"), "{doc}");
    let history = read(task_dir.join("history.jsonl"));
    assert!(
        history.contains("Run `python3 - <<'E' import re"),
        "{history}"
    );
    let events = read(state.join("events.jsonl"));
    assert!(events.contains(&id), "{events}");
    for log in [&history, &events] {
        assert!(!log.contains("rm -rf"), "{log}");
    }
    let mut kept = String::new();
    let mut stack = vec![host.home().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                kept.push_str(&text);
            }
        }
    }
    let answered = serde_json::to_string(&f).unwrap();
    assert!(!answered.contains("LONGLONG"), "{answered}");
    assert!(!kept.contains("LONGLONG"), "{kept}");
}

/// Invariant 13 (D-075): a credential printed by what fails — the coding
/// agent's tool, its own words, its error, a repository URL that can't be
/// cloned — is in nothing Otter keeps or answers with. (The workspace's
/// record keeps the URL it was given: Git needs it.)
#[tokio::test]
async fn credentials_in_failures_stay_out_of_what_otter_keeps() {
    use otter_core::task::TaskStatus;
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let id = started_task(host, "Deploy it LEAK").await;
    wait_task(host, &id, "the run failed", |f| {
        f.runs.first().is_some_and(|r| r.state.is_over()) && f.status != TaskStatus::Planning
    })
    .await;
    // A repository URL with a token in it, that can't be cloned.
    let url = "https://bot:ghp_URLURLURLURLURLURLURLURL1234@127.0.0.1:9/o/r.git";
    let mut conn = host.conn().await;
    let ws = conn
        .workspace_create(WorkspaceCreate {
            name: "leaky".into(),
            source: SourceSpec::Git {
                repository: url.into(),
                branch: None,
                base: None,
            },
            sessions: Some(vec![]),
            ..Default::default()
        })
        .await
        .unwrap();
    let shown = serde_json::to_string(&ws).unwrap();
    assert!(!shown.contains("URLURL"), "{shown}");
    let failed = eventually("the clone fails", || async {
        let ws = host.conn().await.workspace_get("leaky").await.unwrap();
        (ws.state == WorkspaceState::Failed).then_some(ws)
    })
    .await;
    assert!(
        failed
            .state_message
            .as_deref()
            .unwrap()
            .contains("git clone failed")
    );

    let secrets = ["LEAKLEAK", "leakyleakybearer", "URLURL"];
    // In the answers…
    let f = conn.task_get(&id).await.unwrap();
    let c = conn
        .conversation_get(f.runs[0].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap();
    let answers = format!(
        "{}{}{}",
        serde_json::to_string(&f).unwrap(),
        serde_json::to_string(&c).unwrap(),
        serde_json::to_string(&conn.workspace_list().await.unwrap()).unwrap()
    );
    // …and in every file Otter keeps, but the workspace record's URL.
    let mut kept = String::new();
    let mut stack = vec![host.home().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = e.path();
            if path.is_dir() {
                // Git's own clone attempt is Git's.
                if !path.ends_with("repos") {
                    stack.push(path);
                }
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                let text = if path.ends_with("state/state.json") {
                    text.replace(url, "<the URL as given>")
                } else {
                    text
                };
                kept.push_str(&format!("\n== {}\n{text}", path.display()));
            }
        }
    }
    for s in secrets {
        assert!(!answers.contains(s), "{s} in an answer");
        assert!(!kept.contains(s), "{s} kept: {kept}");
    }
    // What failed is still readable.
    assert!(
        f.runs[0].summary.as_deref().unwrap().contains("Bearer ***"),
        "{:?}",
        f.runs[0].summary
    );
}

/// Allowed once, allowed for the rest of the task (D-066): the same
/// write outside the workspace isn't asked about again.
#[tokio::test]
async fn an_approval_is_remembered_for_the_task() {
    use otter_core::task::{Decider, DecisionStatus, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let id = started_task(host, "Keep a build log ASK_TWICE").await;
    let f = wait_task(host, &id, "blocked on the developer", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    assert!(d.user_only);
    assert_eq!(d.grant.as_deref(), Some("cmd:cargo build > /opt/build.log"));
    host.conn()
        .await
        .task_act(
            "allow-log",
            &id,
            TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    assert_eq!(f.grants, ["cmd:cargo build > /opt/build.log"]);
    assert_eq!(f.decisions.len(), 2, "{:?}", f.decisions);
    let again = &f.decisions[1];
    assert_eq!(
        (again.status, again.decided_by),
        (DecisionStatus::Approved, Some(Decider::Policy))
    );
    assert_eq!(
        again.rationale.as_deref(),
        Some("you allowed this earlier in this task")
    );
    assert_eq!(f.runs[0].summary.as_deref(), Some("allowed and done"));
}

#[tokio::test]
async fn the_control_agent_decides_what_policy_leaves_open() {
    use otter_core::task::{Decider, DecisionStatus, TaskStatus};
    let fake = fake_agent_host("ok", "yes").await;
    // The coding agent's question: the Control Agent answers it.
    let id = started_task(&fake.host, "Pick a colour ASK_QUESTION").await;
    let f = wait_task(&fake.host, &id, "review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    let d = &f.decisions[0];
    assert_eq!(
        (d.status, d.decided_by),
        (DecisionStatus::Answered, Some(Decider::Controller))
    );
    assert_eq!(d.answer.as_deref(), Some("red"));
    assert_eq!(d.rationale.as_deref(), Some("yes"));
}

#[tokio::test]
async fn runs_survive_a_restart_and_follow_a_changing_goal() {
    use otter_core::task::{RunState, TaskAction, TaskStatus};
    let mut fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "A long job HANG").await;
    let f = wait_task(&fake.host, &id, "a run with a conversation", |f| {
        f.runs
            .first()
            .is_some_and(|r| r.provider_session_id.is_some() && r.state == RunState::Running)
    })
    .await;
    let conv = f.runs[0].provider_session_id.clone().unwrap();

    // The daemon restarts: the run ends with it, and its conversation resumes.
    fake.host.restart().await;
    let f = wait_task(&fake.host, &id, "the run resumed", |f| {
        f.runs.len() == 2 && f.runs[1].state == RunState::Running
    })
    .await;
    assert_eq!(
        f.runs[0].state,
        RunState::Interrupted,
        "{:?}",
        f.runs[0].summary
    );
    assert_eq!(
        f.runs[1].provider_session_id.as_deref(),
        Some(conv.as_str())
    );
    assert_eq!(
        f.budget.iterations_used, 1,
        "resuming isn't another attempt"
    );
    let args = std::fs::read_to_string(&fake.log).unwrap();
    assert!(args.contains(&format!("--resume {conv}")), "{args}");
    // One conversation (D-055): a resumed run is a new run of it, a new
    // generation; the first run's turn was lost with the daemon.
    let cid = f.runs[0].conversation_id.clone().expect("linked");
    assert_eq!(f.runs[1].conversation_id.as_ref(), Some(&cid));
    let c = fake
        .host
        .conn()
        .await
        .conversation_get(cid.as_str())
        .await
        .unwrap();
    assert!(c.resumable);
    assert_eq!(c.conversation.generation, 2);
    assert_eq!(c.conversation.active_run.as_ref(), Some(&f.runs[1].id));
    assert_eq!(
        c.conversation.turns[0].outcome,
        Some(otter_core::conversation::TurnOutcome::OutcomeUnknown)
    );

    // Paused and resumed: the same conversation carries on.
    let mut conn = fake.host.conn().await;
    conn.task_act("pause", &id, TaskAction::Pause)
        .await
        .unwrap();
    conn.task_act("resume", &id, TaskAction::Resume)
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "running again", |f| {
        f.runs.len() == 3 && f.runs[2].state == RunState::Running
    })
    .await;
    assert_eq!(f.runs[2].conversation_id.as_ref(), Some(&cid));
    let c = conn.conversation_get(cid.as_str()).await.unwrap();
    assert_eq!(c.conversation.generation, 3);
    assert_eq!(
        c.conversation.turns[1].outcome,
        Some(otter_core::conversation::TurnOutcome::Interrupted),
        "paused: the turn was interrupted, not failed"
    );
    assert_eq!(
        f.runs[2].provider_session_id.as_deref(),
        Some(conv.as_str())
    );

    // The developer changes the goal mid-way (D-050): the run stops, and
    // the work is planned again with what they said. (Long again: a turn in
    // the task's conversation isn't told the request again, D-064.)
    conn.task_act(
        "revise",
        &id,
        TaskAction::RequestChanges {
            note: Some("Also write a README HANG".into()),
        },
    )
    .await
    .unwrap();
    let f = wait_task(&fake.host, &id, "a revised plan at work", |f| {
        f.runs.len() == 4 && f.runs[3].state == RunState::Running
    })
    .await;
    assert_eq!(f.runs[2].state, RunState::Cancelled);
    assert_eq!(f.steps.len(), 1, "the unfinished step was replaced");
    assert!(
        f.steps[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("Also write a README")
    );
    assert!(!f.replan);
    assert!(
        f.messages
            .iter()
            .any(|m| m.text.starts_with("Revised plan"))
    );

    // Cancel stops it for good.
    let f = conn
        .task_act("cancel", &id, TaskAction::Cancel)
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Cancelled);
    assert_eq!(f.runs[3].state, RunState::Cancelled);
}

#[tokio::test]
async fn repeated_failures_stop_at_the_limit_and_the_plan_survives() {
    use otter_core::task::{RunState, TaskAction, TaskEvent, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "This will FAIL").await;
    let f = wait_task(&fake.host, &id, "failed", |f| {
        f.status == TaskStatus::Failed
    })
    .await;
    assert_eq!(f.runs.len(), 3);
    assert!(f.runs.iter().all(|r| r.state == RunState::Failed));
    assert!(
        f.status_reason
            .as_deref()
            .unwrap()
            .contains("same failure 3 times"),
        "{:?}",
        f.status_reason
    );
    let history = fake.host.conn().await.task_events(&id, None).await.unwrap();
    assert!(
        history
            .iter()
            .any(|r| matches!(&r.event, TaskEvent::LimitReached { limit } if limit == "loop"))
    );
    // The plan is kept: retrying starts from it with a fresh budget.
    let steps = f.steps.clone();
    let f = fake
        .host
        .conn()
        .await
        .task_act("retry", &id, TaskAction::Retry)
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Implementing);
    assert_eq!(
        f.steps.iter().map(|t| &t.id).collect::<Vec<_>>(),
        steps.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    assert_eq!(f.budget.iterations_used, 0);
    // The earlier failures don't count against the retry: three new ones do.
    let f = wait_task(&fake.host, &id, "failed again", |f| {
        f.status == TaskStatus::Failed && f.runs.len() == 6
    })
    .await;
    assert_eq!(f.budget.iterations_used, 3);
}

/// A check that can't run here (a missing command) is the environment's
/// problem, not the work's (D-062): one verification, one message, blocked on
/// the developer — no fix step, no more runs — until they fix it.
#[tokio::test]
async fn a_check_that_cant_run_blocks_on_the_developer_once() {
    use otter_core::task::{DecisionKind, EvidenceKind, MessageRole, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let id = started_with(
        host,
        "proj",
        "Add CSV export",
        "test:\n\t@not-on-this-path test\n",
        false,
    )
    .await;
    let f = wait_task(host, &id, "blocked on the check", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let cant_run = |f: &otter_core::task::Task| {
        f.messages
            .iter()
            .filter(|m| m.role == MessageRole::Controller && m.text.contains("can't run here"))
            .count()
    };
    let check_runs = |f: &otter_core::task::Task| {
        f.evidence
            .iter()
            .filter(|e| e.kind == EvidenceKind::Test)
            .count()
    };
    assert_eq!(f.runs.len(), 1);
    assert_eq!(f.budget.iterations_used, 1);
    assert!(
        !f.steps
            .iter()
            .any(|t| t.title == "Fix what verification found")
    );
    assert_eq!(cant_run(&f), 1);
    assert_eq!(check_runs(&f), 1);
    let check = f
        .evidence
        .iter()
        .find(|e| e.kind == EvidenceKind::Test)
        .unwrap();
    assert_eq!(check.title, "`make test` can't run here");
    assert_eq!(check.ok, None);
    assert!(
        f.status_reason
            .as_deref()
            .unwrap()
            .contains("not-on-this-path: No such file or directory"),
        "{:?}",
        f.status_reason
    );
    let d = f.pending_decisions().next().unwrap().clone();
    assert_eq!(d.kind, DecisionKind::Question);
    assert!(d.user_only, "the environment is the developer's");
    assert_eq!(d.options, ["Run it again", "Go on without it"]);

    // It stays that way: nothing retries it behind the developer's back.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert_eq!(f.status, TaskStatus::Blocked);
    assert_eq!((f.runs.len(), cant_run(&f), check_runs(&f)), (1, 1, 1));

    // The developer fixes the environment and runs it again: verified,
    // without another run of the coding agent.
    std::fs::write(
        host.home().join("proj/Makefile"),
        "test:\n\t@echo all good\n",
    )
    .unwrap();
    host.conn()
        .await
        .task_act(
            "again",
            &id,
            TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: Some("Run it again".into()),
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_task(host, &id, "verified", |f| f.status == TaskStatus::Review).await;
    assert_eq!(f.runs.len(), 1);
    assert!(f.acceptance.iter().all(|c| c.met == Some(true)));
    let last = f
        .evidence
        .iter()
        .rev()
        .find(|e| e.kind == EvidenceKind::Test)
        .unwrap();
    assert_eq!(last.ok, Some(true));
}

/// Time spent waiting on the developer isn't the run's (D-063, audit E1):
/// a run that waits longer than its limit for an approval still does what
/// was approved, in the same run.
#[tokio::test]
async fn waiting_on_an_approval_doesnt_use_up_the_runs_time() {
    use otter_core::task::{RunState, TaskAction, TaskStatus};
    let fake = fake_agent_host_with("ok", "rules", |_| {
        vec![("OTTER_RUN_TIMEOUT_MS", "3000".into())]
    })
    .await;
    let host = &fake.host;
    let id = started_task(host, "Clean up ASK_RM").await;
    let f = wait_task(host, &id, "waiting on the developer", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    assert_eq!(f.runs[0].state, RunState::Waiting);
    assert!(f.runs[0].working_since.is_none(), "waiting isn't working");
    let d = f.pending_decisions().next().unwrap().clone();
    // Longer than the run may work.
    tokio::time::sleep(Duration::from_secs(5)).await;
    host.conn()
        .await
        .task_act(
            "approve",
            &id,
            TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    assert_eq!(f.runs.len(), 1, "{:?}", f.runs);
    assert_eq!(f.runs[0].state, RunState::Completed);
    assert_eq!(f.runs[0].summary.as_deref(), Some("allowed and done"));
    assert!(
        f.runs[0].worked_ms < 3000,
        "worked {} ms",
        f.runs[0].worked_ms
    );
    assert_eq!(f.budget.iterations_used, 1);
}

/// A run out of time is cut off and resumed in the same conversation with a
/// word on why — not failed and started over (D-063).
#[tokio::test]
async fn a_run_out_of_time_resumes_its_conversation() {
    use otter_core::task::{RunState, TaskStatus};
    let fake = fake_agent_host_with("ok", "rules", |_| {
        vec![("OTTER_RUN_TIMEOUT_MS", "1500".into())]
    })
    .await;
    let host = &fake.host;
    let id = started_task(host, "A slow job SLOW").await;
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    assert_eq!(f.runs.len(), 2, "{:?}", f.runs);
    assert_eq!(f.runs[0].state, RunState::Cancelled);
    assert_eq!(
        f.runs[0].summary.as_deref(),
        Some("Out of time after 1 s of work")
    );
    assert_eq!(f.runs[1].state, RunState::Completed);
    assert_eq!(
        f.runs[1].summary.as_deref(),
        Some("finished after resuming")
    );
    assert!(f.runs[0].provider_session_id.is_some());
    assert_eq!(f.runs[1].provider_session_id, f.runs[0].provider_session_id);
    assert_eq!(
        f.budget.iterations_used, 1,
        "resuming isn't another attempt"
    );
    // The agent was told why.
    let c = host
        .conn()
        .await
        .conversation_get(f.runs[1].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap();
    let told = serde_json::to_string(&c).unwrap();
    assert!(told.contains("You ran out of time on"), "{told}");
}

/// An agent that goes quiet (no word, no tool running) is cut off and
/// resumed the same way (D-063).
#[tokio::test]
async fn a_silent_run_is_cut_off_and_resumed() {
    use otter_core::task::{RunState, TaskStatus};
    let fake = fake_agent_host_with("ok", "rules", |_| {
        vec![("OTTER_RUN_IDLE_MS", "1000".into())]
    })
    .await;
    let host = &fake.host;
    let id = started_task(host, "A slow job SLOW").await;
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    assert_eq!(f.runs.len(), 2, "{:?}", f.runs);
    assert!(
        f.runs[0]
            .summary
            .as_deref()
            .is_some_and(|s| s.starts_with("No progress for ")),
        "{:?}",
        f.runs[0].summary
    );
    assert_eq!(f.runs[1].provider_session_id, f.runs[0].provider_session_id);
    assert_eq!(f.runs[1].state, RunState::Completed);
    assert_eq!(f.budget.iterations_used, 1);
}

/// A judge that is never satisfied, a check that passes and a fix that
/// changes nothing: the second identical verdict on identical work goes to
/// review, unverified — no third run (D-062).
#[tokio::test]
async fn verification_stops_when_a_fix_changes_nothing() {
    use otter_core::task::{EvidenceKind, GateStatus, MessageRole, Proof, TaskStatus};
    let fake = fake_agent_host("ok", "skeptic").await;
    let host = &fake.host;
    let id = started_with(
        host,
        "proj",
        "Add CSV export",
        "test:\n\t@echo all good\n",
        true,
    )
    .await;
    let f = wait_task(host, &id, "in review, gates checked", |f| {
        f.status == TaskStatus::Review && !f.gates.is_empty()
    })
    .await;
    assert_eq!(f.runs.len(), 2, "the step, and one fix");
    // The next verdict's judge would see what ran after this one (D-165).
    assert_eq!(f.judged_runs, 2);
    // The fix is a turn in the same session (D-064).
    let sid = f.runs[0].provider_session_id.clone().unwrap();
    assert_eq!(f.runs[1].provider_session_id.as_deref(), Some(sid.as_str()));
    let log = std::fs::read_to_string(&fake.log).unwrap();
    let launches: Vec<&str> = log
        .lines()
        .filter(|l| l.starts_with("claude-stream"))
        .collect();
    assert_eq!(launches.len(), 2, "{log}");
    assert!(!launches[0].contains("--resume"), "{log}");
    assert!(launches[1].contains(&format!("--resume {sid}")), "{log}");
    assert_eq!(
        f.steps
            .iter()
            .filter(|t| t.title == "Fix what verification found")
            .count(),
        1
    );
    assert_eq!(
        f.status_reason.as_deref(),
        Some("Not verified: nothing changed after a fix")
    );
    assert!(f.messages.iter().any(|m| {
        m.role == MessageRole::Controller
            && m.text
                .starts_with("Not verified, and nothing changed after the last fix")
    }));
    // The check decided its own criterion; the judge only the other.
    let by = |how| f.acceptance.iter().find(|c| c.how == how).unwrap();
    assert_eq!(by(Proof::Check).met, Some(true));
    assert_eq!(by(Proof::Diff).met, Some(false));
    // The judge was shown the work itself.
    let tree: Vec<_> = f
        .evidence
        .iter()
        .filter(|e| e.kind == EvidenceKind::Diff)
        .collect();
    assert_eq!(tree.len(), 2, "one snapshot per verification");
    assert_eq!(tree[1].title, "Working tree: no changes");
    assert!(tree[1].detail.as_deref().unwrap().contains("(clean)"));
    // The developer sees what isn't proven.
    let gate = f.gates.iter().find(|g| g.name == "Requirements met");
    assert_eq!(gate.map(|g| g.status), Some(GateStatus::Failed));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert_eq!((f.status, f.runs.len()), (TaskStatus::Review, 2));
}

/// A task started without a request (D-067): the developer's first
/// message is the request, not a note to skip; with none, Otter asks once
/// and waits, and a plain message answers it.
#[tokio::test]
async fn a_task_without_a_request_takes_the_developers_word() {
    use otter_core::task::{DecisionKind, MessageRole, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    // Said before it starts.
    let dir = host.home().join("said");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    let mut conn = host.conn().await;
    conn.workspace_create(WorkspaceCreate {
        name: "said".into(),
        source: SourceSpec::Directory {
            path: dir.to_string_lossy().into_owned(),
        },
        sessions: Some(vec![]),
        ..Default::default()
    })
    .await
    .unwrap();
    let f = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "create-said".into(),
            title: "Otter".into(),
            request: String::new(),
            workspace: Some("said".into()),
            answer_only: false,
        })
        .await
        .unwrap();
    let id = f.id.to_string();
    conn.task_send("msg-goal", &id, "Add CSV export of the orders")
        .await
        .unwrap();
    conn.task_act("start", &id, TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    assert_eq!(f.request, "Add CSV export of the orders");
    assert!(f.steps[0].detail.as_deref().unwrap().contains("orders"));
    let c = conn
        .conversation_get(f.runs[0].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap()
        .conversation;
    let first = c.turns[0].text();
    assert!(
        first.contains("Request:\nAdd CSV export of the orders"),
        "{first}"
    );
    assert!(
        !first.contains("From the developer"),
        "not a note too: {first}"
    );

    // Nothing said at all: Otter asks, once, and waits.
    let id = started_in(host, "silent", "").await;
    let f = wait_task(host, &id, "asking what to do", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    assert_eq!(
        (d.kind, d.summary.as_str(), d.user_only),
        (DecisionKind::Question, "What should this task do?", true)
    );
    assert_eq!(
        f.status_reason.as_deref(),
        Some("Waiting for what this task should do")
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let f = conn.task_get(&id).await.unwrap();
    assert_eq!(f.status, TaskStatus::Blocked);
    let asked = |f: &otter_core::task::Task| {
        f.messages
            .iter()
            .filter(|m| m.role == MessageRole::Controller && m.text.starts_with("What should"))
            .count()
    };
    assert_eq!(asked(&f), 1);
    assert!(f.runs.is_empty() && f.steps.is_empty());
    // A plain message answers it.
    conn.task_send("msg-late", &id, "Export the orders as CSV")
        .await
        .unwrap();
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    assert_eq!(f.request, "Export the orders as CSV");
    assert_eq!(asked(&f), 1);
}

/// A turn the agent ends while its background work still runs isn't the end
/// of the run (D-065): what it does when the work is done is part of it.
#[tokio::test]
async fn a_run_waits_for_the_agents_background_work() {
    use otter_core::task::{RunState, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let id = started_task(host, "Build it in the background BGWAIT").await;
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    assert_eq!(f.runs.len(), 1, "{:?}", f.runs);
    assert_eq!(f.runs[0].state, RunState::Completed);
    assert_eq!(f.runs[0].summary.as_deref(), Some("done after waiting"));
    let c = host
        .conn()
        .await
        .conversation_get(f.runs[0].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(c.turns.len(), 1, "one turn, the follow-up included");
    assert!(
        c.messages
            .iter()
            .any(|m| m.text().contains("It printed done."))
    );
}

/// Planning starts with the coding agent reading the code (D-064): its
/// proposal is the plan (no model to review it here), and the steps are
/// turns in the same conversation after it.
#[tokio::test]
async fn the_coding_agent_plans_first_in_the_tasks_conversation() {
    use otter_core::task::{MessageRole, Proof, StepStatus, TaskStatus};
    let fake = fake_agent_host_with("ok", "rules", |_| vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    let titles: Vec<&str> = f.steps.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Look at the code and plan the work",
            "Write the exporter",
            "Cover it with a test"
        ]
    );
    assert!(f.steps[0].planning && !f.steps[1].planning);
    assert!(f.steps.iter().all(|t| t.status == StepStatus::Done));
    let criteria: Vec<(&str, Proof)> = f
        .acceptance
        .iter()
        .map(|c| (c.text.as_str(), c.how))
        .collect();
    assert_eq!(
        criteria,
        [
            ("Rows export as CSV with a header", Proof::Diff),
            ("make test passes", Proof::Check)
        ]
    );
    assert_eq!(f.verify_command.as_deref(), Some("make test"));
    assert!(
        f.rationale
            .iter()
            .any(|r| r.contains("The coding agent's plan") && r.contains("Add an exporter")),
        "{:?}",
        f.rationale
    );
    // Three turns, one session; planning costs no attempt.
    assert_eq!(f.runs.len(), 3);
    let sid = f.runs[0].provider_session_id.clone();
    assert!(sid.is_some());
    assert!(f.runs.iter().all(|r| r.provider_session_id == sid));
    assert_eq!(f.budget.iterations_used, 2);
    // The proposal is Otter's to read, not a message in the conversation.
    assert!(
        !f.messages
            .iter()
            .any(|m| m.role == MessageRole::Agent && m.text.contains("\"steps\"")),
        "{:?}",
        f.messages
    );
    // The first step's turn doesn't restate the request: the planning turn had it.
    let c = host
        .conn()
        .await
        .conversation_get(f.runs[1].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap()
        .conversation;
    let texts: Vec<String> = c.turns.iter().map(|t| t.text()).collect();
    assert!(texts[0].contains("plan the work"), "{texts:#?}");
    assert!(
        texts[1].contains("Your step (1 of 2): Write the exporter"),
        "{texts:#?}"
    );
    assert!(!texts[1].contains("Request:"), "{texts:#?}");
}

/// The coding agent's answer reaches the conversation as it wrote it
/// (D-086): its lines and Markdown kept, and not cut at a status line's
/// length.
#[tokio::test]
async fn a_long_answer_keeps_its_lines() {
    use otter_core::task::{MessageRole, TaskStatus};
    let fake = fake_agent_host_with("ok", "rules", |_| vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = started_task(host, "How does the backport work? LONGANSWER").await;
    let f = wait_task(host, &id, "answered", |f| f.status == TaskStatus::Done).await;
    let answer = f
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Agent)
        .map(|m| m.text.as_str())
        .unwrap_or_else(|| panic!("{:?}", f.messages));
    assert!(
        answer
            .starts_with("## How it works\n\n1. Open a PR.\n2. Label it **backport v3.159**.\n\n"),
        "{answer:?}"
    );
    assert!(
        answer.ends_with(&"x".repeat(2500)),
        "cut short: {} chars",
        answer.len()
    );
}

/// A request that asks no change is answered (D-084): the planning turn's
/// words are the answer, shown in the conversation, and the task is done
/// — no plan, no attempt, no check. Asking for a change after it plans, in
/// the same conversation, through the agent.
#[tokio::test]
async fn a_question_is_answered_and_a_change_after_it_is_planned() {
    use otter_core::task::{MessageRole, TaskStatus};
    let fake = fake_agent_host_with("ok", "rules", |_| vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = started_task(host, "How does the export work? NOPLAN").await;
    let f = wait_task(host, &id, "answered", |f| f.status == TaskStatus::Done).await;
    assert!(
        f.status_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("Answered")),
        "{:?}",
        f.status_reason
    );
    let titles: Vec<&str> = f.steps.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(titles, ["Look at the code and plan the work"]);
    assert_eq!(f.runs.len(), 1);
    assert_eq!(f.budget.iterations_used, 0);
    assert!(f.acceptance.is_empty() && f.verify_command.is_none());
    assert!(
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::Agent
                && m.text == "I would write the export and test it."),
        "{:?}",
        f.messages
    );
    // Asking for a change plans it: the agent's look again, in its conversation.
    host.conn()
        .await
        .task_send("cmd-change", &id, "Then write it, please")
        .await
        .unwrap();
    let f = wait_task(host, &id, "review", |f| f.status == TaskStatus::Review).await;
    let titles: Vec<&str> = f.steps.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Look at the code and plan the work",
            "Write the exporter",
            "Cover it with a test"
        ]
    );
    assert_eq!(f.runs.len(), 4);
    let sid = f.runs[0].provider_session_id.clone();
    assert!(f.runs.iter().all(|r| r.provider_session_id == sid));
    assert_eq!(f.budget.iterations_used, 2);
    // Nothing was planned before: a plan, not a revised one.
    assert!(
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::Controller && m.text.starts_with("Plan: 2 step(s)")),
        "{:?}",
        f.messages
    );
    let c = host
        .conn()
        .await
        .conversation_get(f.runs[1].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap()
        .conversation;
    let texts: Vec<String> = c.turns.iter().map(|t| t.text()).collect();
    assert!(texts[1].contains("Then write it, please"), "{texts:#?}");
    assert!(texts[1].contains("plan the work"), "{texts:#?}");
    assert!(!texts[1].contains("Request:"), "{texts:#?}");
}

/// The brain reviews the agent's plan and doesn't rewrite it (D-084): what
/// it finds goes back to the agent once, in the same conversation; found
/// again, the developer decides, and their OK sets the agent's plan as is.
#[tokio::test]
async fn a_plan_the_brain_faults_goes_back_once_then_to_the_developer() {
    use otter_core::task::{DecisionKind, TaskAction, TaskStatus};
    let fake =
        fake_agent_host_with("ok", "skeptic", |_| vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    let f = wait_task(host, &id, "asking about the plan", |f| {
        f.status == TaskStatus::Blocked
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    assert_eq!(d.kind, DecisionKind::PlanApproval);
    assert!(d.user_only && d.run_id.is_none());
    assert!(d.detail.as_deref().unwrap().contains("too vague"), "{d:?}");
    assert!(
        f.steps.iter().all(|t| t.planning),
        "nothing is set before the developer says: {:?}",
        f.steps
    );
    assert_eq!(f.runs.len(), 2);
    assert_eq!(f.runs[1].provider_session_id, f.runs[0].provider_session_id);
    let c = host
        .conn()
        .await
        .conversation_get(f.runs[1].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap()
        .conversation;
    let texts: Vec<String> = c.turns.iter().map(|t| t.text()).collect();
    assert!(texts[1].contains("Otter reviewed your plan"), "{texts:#?}");
    assert!(texts[1].contains("too vague"), "{texts:#?}");
    host.conn()
        .await
        .task_act(
            "approve-plan",
            &id,
            TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_task(host, &id, "planned", |f| {
        f.steps.iter().any(|t| t.title == "Write the exporter")
    })
    .await;
    assert!(
        f.rationale
            .iter()
            .any(|r| r.starts_with("Plan (approved by you)")),
        "{:?}",
        f.rationale
    );
    assert_eq!(f.verify_command.as_deref(), Some("make test"));
}

/// What the agent says in any script reaches the task whole (D-134): a
/// request in Chinese with a link, said back, once took the run down in
/// otterd, and the task with it.
#[tokio::test]
async fn a_request_in_chinese_is_planned() {
    let fake = fake_agent_host_with("ok", "rules", |_| vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = started_task(host, "修复这个PR，https://example.com/o/r/pull/7 CJK").await;
    let f = wait_task(host, &id, "planned", |f| {
        f.steps.iter().any(|t| t.title == "Write the exporter")
            || f.status == otter_core::task::TaskStatus::Failed
    })
    .await;
    assert!(
        f.steps.iter().any(|t| t.title == "Write the exporter"),
        "{:?}: {:?}",
        f.status,
        f.status_reason
    );
}

/// A planning turn that fails stops planning and says so (D-084): no plan
/// is guessed without the agent's look at the code. Retrying looks again,
/// in the same conversation.
#[tokio::test]
async fn a_failed_look_at_the_code_stops_planning() {
    use otter_core::task::{TaskAction, TaskStatus};
    let fake = fake_agent_host_with("ok", "rules", |_| vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export PLANFAIL").await;
    let f = wait_task(host, &id, "failed", |f| f.status == TaskStatus::Failed).await;
    assert!(
        f.status_reason
            .as_deref()
            .is_some_and(|r| r.contains("look at the code failed")),
        "{:?}",
        f.status_reason
    );
    assert!(f.steps.iter().all(|t| t.planning), "{:?}", f.steps);
    assert!(f.acceptance.is_empty() && f.verify_command.is_none());
    assert_eq!(f.runs.len(), 1);
    host.conn()
        .await
        .task_act("retry", &id, TaskAction::Retry)
        .await
        .unwrap();
    // The conversation has the request: the look again (no PLANFAIL in it) plans.
    let f = wait_task(host, &id, "planned", |f| {
        f.steps.iter().any(|t| t.title == "Write the exporter")
    })
    .await;
    assert_eq!(f.runs[1].step_id, f.steps[0].id);
    assert_eq!(f.runs[1].provider_session_id, f.runs[0].provider_session_id);
}

/// In the task's one conversation, a developer message reaches the agent
/// once: the next turn's prompt (here, a fix) doesn't say it again (D-064).
#[tokio::test]
async fn a_message_reaches_the_agent_once_across_turns() {
    use otter_core::task::{TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "skeptic").await;
    let host = &fake.host;
    let id = started_task(host, "Pick a colour ASK_QUESTION").await;
    let f = wait_task(host, &id, "asking", |f| {
        f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    let cid = f.runs[0].conversation_id.clone().unwrap();
    let mut conn = host.conn().await;
    conn.task_send("msg-tabs", &id, "Use tabs, not spaces")
        .await
        .unwrap();
    let turns = |c: &otter_core::conversation::Conversation, what: &str| {
        c.turns.iter().filter(|t| t.text().contains(what)).count()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while turns(
        &conn
            .conversation_get(cid.as_str())
            .await
            .unwrap()
            .conversation,
        "Use tabs",
    ) == 0
    {
        assert!(std::time::Instant::now() < deadline, "never queued");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    conn.task_act(
        "answer",
        &id,
        TaskAction::Decide {
            decision_id: d.id,
            approve: true,
            answer: Some("blue".into()),
            answers: None,
        },
    )
    .await
    .unwrap();
    // Never satisfied: a fix, in the same conversation, then review.
    let f = wait_task(host, &id, "review after a fix", |f| {
        f.status == TaskStatus::Review && f.runs.len() == 2
    })
    .await;
    assert_eq!(f.runs[1].conversation_id.as_ref(), Some(&cid));
    let c = conn
        .conversation_get(cid.as_str())
        .await
        .unwrap()
        .conversation;
    let texts: Vec<String> = c.turns.iter().map(|t| t.text()).collect();
    assert_eq!(turns(&c, "Use tabs"), 1, "{texts:#?}");
    let fix = texts.last().unwrap();
    assert!(
        fix.contains("Your step (2 of 2): Fix what verification found"),
        "{fix}"
    );
    assert!(
        !fix.contains("ASK_QUESTION"),
        "the request isn't said again: {fix}"
    );
}

// ---------------------------------------------------------------------------
// Runs whose agent process is lost (D-061): what the cut-off step did stays
// unknown, the conversation resumes, and what Otter can't safely continue
// waits for the developer.
// ---------------------------------------------------------------------------

/// How often the fake agent was started fresh, and how often resumed.
fn agent_starts(log: &Path) -> (usize, usize) {
    let args = std::fs::read_to_string(log).unwrap_or_default();
    let starts: Vec<&str> = args
        .lines()
        .filter(|l| l.starts_with("claude-stream "))
        .collect();
    let resumed = starts.iter().filter(|l| l.contains("--resume")).count();
    (starts.len() - resumed, resumed)
}

#[tokio::test]
async fn a_run_lost_mid_tool_resumes_without_claiming_the_step() {
    use otter_core::conversation::{ErrorCategory, ToolStatus, TurnOutcome};
    use otter_core::task::{RunState, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "Ship the preview DIE_MIDTOOL").await;
    let f = wait_task(&fake.host, &id, "resumed and finished", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    assert_eq!(f.runs.len(), 2, "{:?}", f.runs);
    let (lost, resumed) = (&f.runs[0], &f.runs[1]);
    assert_eq!(lost.state, RunState::Interrupted, "{:?}", lost.summary);
    assert!(lost.summary.as_deref().unwrap().contains("resumable"));
    assert_eq!(resumed.state, RunState::Completed);
    assert_eq!(resumed.provider_session_id, lost.provider_session_id);
    assert_eq!(resumed.conversation_id, lost.conversation_id);
    assert_eq!(
        f.budget.iterations_used, 1,
        "resuming isn't another attempt"
    );

    // The cut-off turn isn't a success, and neither is its tool.
    let cid = lost.conversation_id.clone().unwrap();
    let c = fake
        .host
        .conn()
        .await
        .conversation_get(cid.as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(c.turns[0].outcome, Some(TurnOutcome::OutcomeUnknown));
    assert_eq!(c.turns[0].error, Some(ErrorCategory::WorkerCrashed));
    let cut = c
        .tools
        .iter()
        .find(|t| t.run_id == lost.id)
        .expect("its tool");
    assert_eq!(cut.status, ToolStatus::ResultUnavailable);

    // Never started over: the agent resumed once and was told the step's
    // result is unknown, to check before repeating it.
    assert_eq!(agent_starts(&fake.log), (1, 1));
    let args = std::fs::read_to_string(&fake.log).unwrap();
    let told = args
        .lines()
        .find(|l| l.starts_with("resumed-turn "))
        .expect("the resumed turn");
    assert!(told.contains("cut off before it finished"), "{told}");
    assert!(told.contains("check whether it took effect"), "{told}");
}

#[tokio::test]
async fn a_run_that_keeps_getting_lost_waits_for_the_developer() {
    use otter_core::task::{
        Decider, DecisionKind, DecisionStatus, RunState, StepStatus, TaskAction, TaskStatus,
    };
    let mut fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "Ship the preview DIE_ALWAYS").await;
    let f = wait_task(&fake.host, &id, "a recovery decision", |f| {
        f.pending_decisions().next().is_some()
    })
    .await;
    let states: Vec<RunState> = f.runs.iter().map(|r| r.state).collect();
    assert_eq!(
        states,
        [
            RunState::Interrupted,
            RunState::Interrupted,
            RunState::RecoveryRequired
        ]
    );
    let d = f.pending_decisions().next().unwrap().clone();
    assert_eq!(d.kind, DecisionKind::Recovery);
    assert!(d.user_only);
    assert!(d.detail.as_deref().unwrap().contains("3 runs in a row"));
    assert_eq!(f.steps[0].status, StepStatus::Blocked);
    let f = wait_task(&fake.host, &id, "blocked on the developer", |f| {
        f.status == TaskStatus::Blocked
    })
    .await;
    assert!(
        f.status_reason.as_deref().unwrap().contains("Needs you"),
        "{:?}",
        f.status_reason
    );

    // A restart keeps the question open, and starts nothing.
    fake.host.restart().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let f = fake.host.conn().await.task_get(&id).await.unwrap();
    assert_eq!(f.status, TaskStatus::Blocked);
    assert_eq!(f.runs.len(), 3);
    let still = f.decisions.iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(
        still.status,
        DecisionStatus::Pending,
        "nobody decided for them"
    );

    // Declined: nothing starts over until the developer resumes.
    let act = TaskAction::Decide {
        decision_id: d.id.clone(),
        approve: false,
        answer: None,
        answers: None,
    };
    let mut conn = fake.host.conn().await;
    let f = conn.task_act("no-restart", &id, act.clone()).await.unwrap();
    assert_eq!(f.status, TaskStatus::Paused);
    assert_eq!(f.steps[0].status, StepStatus::Pending);
    let d = f.decisions.iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(
        (d.status, d.decided_by),
        (DecisionStatus::Denied, Some(Decider::User))
    );
    // The same command again is the same answer, applied once.
    let again = conn.task_act("no-restart", &id, act).await.unwrap();
    assert_eq!(again.status, TaskStatus::Paused);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(conn.task_get(&id).await.unwrap().runs.len(), 3);
    conn.task_act("stop", &id, TaskAction::Cancel)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_run_lost_before_its_conversation_was_kept_starts_over_only_when_allowed() {
    use otter_core::task::{DecisionKind, RunState, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    // The agent dies mid-tool before it says its session id: nothing to resume.
    let skip = fake.log.parent().unwrap().join("skip-init");
    std::fs::write(&skip, "").unwrap();
    let id = started_task(&fake.host, "Ship the preview DIE_MIDTOOL").await;
    let f = wait_task(&fake.host, &id, "a recovery decision", |f| {
        f.pending_decisions().next().is_some()
    })
    .await;
    assert_eq!(f.runs.len(), 1);
    assert_eq!(f.runs[0].state, RunState::RecoveryRequired);
    assert!(f.runs[0].provider_session_id.is_none());
    let d = f.pending_decisions().next().unwrap().clone();
    assert_eq!(d.kind, DecisionKind::Recovery);
    assert!(
        d.detail
            .as_deref()
            .unwrap()
            .contains("could repeat what it did"),
        "{:?}",
        d.detail
    );
    assert_eq!(
        agent_starts(&fake.log),
        (1, 0),
        "not started over by itself"
    );

    // Allowed: a fresh run (an attempt of its own). That one is lost too,
    // with a conversation this time, so it resumes and finishes.
    std::fs::remove_file(&skip).unwrap();
    fake.host
        .conn()
        .await
        .task_act(
            "start-over",
            &id,
            TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    let states: Vec<RunState> = f.runs.iter().map(|r| r.state).collect();
    assert_eq!(
        states,
        [
            RunState::RecoveryRequired,
            RunState::Interrupted,
            RunState::Completed
        ]
    );
    assert_eq!(f.budget.iterations_used, 2);
    assert_eq!(agent_starts(&fake.log), (2, 1));
}

// ---------------------------------------------------------------------------
// Browser verification and previews (D-046). They drive a real Chrome or
// Chromium (and python3), so they run only with `OTTER_TEST_BROWSER=1`
// (D-137), in CI's job of their own; asked for, a missing one fails.
// ---------------------------------------------------------------------------

fn browser_available() -> bool {
    if std::env::var("OTTER_TEST_BROWSER").as_deref() != Ok("1") {
        eprintln!("skipping: drives a real browser; set OTTER_TEST_BROWSER=1");
        return false;
    }
    let ok = std::env::var("OTTER_BROWSER").is_ok()
        || [
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
        ]
        .iter()
        .any(|b| {
            Command::new("sh")
                .args(["-c", &format!("command -v {b}")])
                .output()
                .is_ok_and(|o| o.status.success())
        })
        || Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome").exists();
    let python = Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        ok && python,
        "OTTER_TEST_BROWSER=1, but this needs Chrome/Chromium and python3"
    );
    true
}

const PREVIEW: &str = "python3 -m http.server {port} --bind 127.0.0.1";

/// A web project: a greeter page, `make test`, and a browser check that
/// expects `expect` after greeting Ada.
async fn web_task(host: &TestHost, expect: &str) -> String {
    use otter_core::task::TaskAction;
    let dir = host.home().join("web");
    std::fs::create_dir_all(dir.join(".otter")).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    std::fs::write(
        dir.join("index.html"),
        r#"<!doctype html><html><body><h1>Greeter</h1><input id="name"><button id="go">Greet</button><p id="out"></p>
<script>document.getElementById('go').onclick = () => { document.getElementById('out').textContent = 'Hello, ' + document.getElementById('name').value; };</script>
</body></html>"#,
    )
    .unwrap();
    let checks = serde_json::json!({"checks": [{
        "name": "greets",
        "preview": {"command": PREVIEW},
        "steps": [
            {"do": "goto", "path": "/"},
            {"do": "fill", "selector": "#name", "value": "Ada"},
            {"do": "click", "selector": "#go"},
            {"do": "expect_text", "selector": "#out", "text": expect},
            {"do": "screenshot", "name": "greeted"}
        ]
    }]});
    std::fs::write(dir.join(".otter/browser-checks.json"), checks.to_string()).unwrap();
    let mut conn = host.conn().await;
    conn.workspace_create(WorkspaceCreate {
        name: "web".into(),
        source: SourceSpec::Directory {
            path: dir.to_string_lossy().into_owned(),
        },
        sessions: Some(vec![]),
        ..Default::default()
    })
    .await
    .unwrap();
    let f = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "create".into(),
            title: "Greeter".into(),
            request: "Greet people by name".into(),
            workspace: Some("web".into()),
            answer_only: false,
        })
        .await
        .unwrap();
    conn.task_act("start", f.id.as_str(), TaskAction::Start)
        .await
        .unwrap();
    f.id.to_string()
}

#[tokio::test]
async fn a_browser_check_verifies_the_ui_with_evidence() {
    use otter_core::task::{EvidenceKind, TaskStatus};
    if !browser_available() {
        return;
    }
    let fake = fake_agent_host("ok", "rules").await;
    let id = web_task(&fake.host, "Hello, Ada").await;
    let f = wait_task(&fake.host, &id, "review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    let browser = f
        .evidence
        .iter()
        .find(|e| e.kind == EvidenceKind::Browser)
        .expect("browser evidence");
    assert_eq!(browser.ok, Some(true), "{:?}", browser.detail);
    assert!(
        browser
            .detail
            .as_deref()
            .unwrap()
            .contains("✓ expect “Hello, Ada” in #out")
    );
    assert!(browser.detail.as_deref().unwrap().contains("✓ click #go"));
    // The screenshot is kept with the task and served by name.
    let shot = f
        .evidence
        .iter()
        .find(|e| e.kind == EvidenceKind::Screenshot)
        .and_then(|e| e.uri.clone())
        .expect("a screenshot");
    let file = fake
        .host
        .conn()
        .await
        .task_artifact(&id, &shot)
        .await
        .unwrap();
    assert!(file.size > 100);
    assert!(file.data.starts_with("iVBORw0KGgo"), "a PNG");
    // Names only: no wandering outside the artifacts.
    assert!(
        fake.host
            .conn()
            .await
            .task_artifact(&id, "../task.json")
            .await
            .is_err()
    );
    // Starting the preview is ordinary work: nobody was asked.
    assert!(!f.decisions.iter().any(|d| d.summary.contains(PREVIEW)));
}

#[tokio::test]
async fn a_broken_ui_fails_verification() {
    use otter_core::task::{EvidenceKind, TaskStatus};
    if !browser_available() {
        return;
    }
    let fake = fake_agent_host("ok", "rules").await;
    // The page says "Hello, Ada"; the check wants something else.
    let id = web_task(&fake.host, "Goodbye, Ada").await;
    let f = wait_task(
        &fake.host,
        &id,
        "verification failed and work resumed",
        |f| {
            f.evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::Browser && e.ok == Some(false))
                && f.steps
                    .iter()
                    .any(|t| t.title == "Fix what verification found")
        },
    )
    .await;
    assert_ne!(f.status, TaskStatus::Review);
    assert_ne!(f.status, TaskStatus::Done);
    let failed = f
        .evidence
        .iter()
        .find(|e| e.kind == EvidenceKind::Browser && e.ok == Some(false))
        .unwrap();
    assert!(
        failed
            .detail
            .as_deref()
            .unwrap()
            .contains("✗ expect “Goodbye, Ada” in #out"),
        "{:?}",
        failed.detail
    );
    // The make check passed, but the criteria aren't met.
    assert!(f.acceptance.iter().any(|c| c.met == Some(false)));
}

#[tokio::test]
async fn a_preview_runs_as_a_session_on_one_port() {
    use otter_core::task::{TaskAction, TaskStatus};
    if !browser_available() {
        return;
    }
    let fake = fake_agent_host("ok", "rules").await;
    let id = web_task(&fake.host, "Hello, Ada").await;
    let mut conn = fake.host.conn().await;
    wait_task(&fake.host, &id, "review", |f| {
        f.status == TaskStatus::Review
    })
    .await;

    let f = conn
        .task_act("pv-2", &id, TaskAction::Preview)
        .await
        .unwrap();
    let link = f.preview.clone().expect("a preview");
    let ws = conn.workspace_get("web").await.unwrap();
    let s = ws.session(link.session_id.as_str()).unwrap();
    assert_eq!(s.kind, SessionKind::Service);
    assert_eq!(s.name, format!("preview-{}", link.port));
    // It serves the app on that one port (what a client forwards).
    let page = eventually("the preview answers", || async {
        let out = Command::new("curl")
            .args([
                "-s",
                &format!("http://127.0.0.1:{}{}", link.port, link.path),
            ])
            .output()
            .ok()?;
        let body = String::from_utf8_lossy(&out.stdout).into_owned();
        body.contains("Greeter").then_some(body)
    })
    .await;
    assert!(page.contains("<h1>Greeter</h1>"));
    // Previewing again replaces it.
    let f = conn
        .task_act("pv-3", &id, TaskAction::Preview)
        .await
        .unwrap();
    let again = f.preview.unwrap();
    assert_ne!(again.session_id, link.session_id);
    let ws = conn.workspace_get("web").await.unwrap();
    assert!(ws.session(link.session_id.as_str()).is_none());
}

// ---------------------------------------------------------------------------
// Delivery (D-047): a pull request, CI and the gates, with a fake `gh` and a
// real Git remote.
// ---------------------------------------------------------------------------

/// A Git workspace (a worktree on `feature/csv`) whose origin is a real
/// repository with CI configured, and a task on it, started: its agent
/// writes `timeline.csv`, something to publish.
async fn git_task(fake: &FakeCodex) -> (String, std::path::PathBuf) {
    git_task_asking(fake, "Export the timeline as CSV WRITE:timeline.csv").await
}

/// [`git_task`], asking for `request`.
async fn git_task_asking(fake: &FakeCodex, request: &str) -> (String, std::path::PathBuf) {
    use otter_core::task::TaskAction;
    let src = fake.host.home().join("origin");
    std::fs::create_dir_all(src.join(".github/workflows")).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    std::fs::write(src.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "init"]);
    let mut conn = fake.host.conn().await;
    conn.workspace_create(git_ws("csv", &src, Some("feature/csv"), vec![]))
        .await
        .unwrap();
    eventually("workspace ready", || async {
        let ws = fake.host.conn().await.workspace_get("csv").await.unwrap();
        (ws.state == WorkspaceState::Ready).then_some(())
    })
    .await;
    let f = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "create".into(),
            title: "CSV export".into(),
            request: request.into(),
            workspace: Some("csv".into()),
            answer_only: false,
        })
        .await
        .unwrap();
    conn.task_act("start", f.id.as_str(), TaskAction::Start)
        .await
        .unwrap();
    (f.id.to_string(), src)
}

/// The developer allows publishing (pushing the branch and opening the PR).
async fn allow_publish(host: &TestHost, id: &str) {
    use otter_core::task::{Risk, TaskAction, TaskStatus};
    let f = wait_task(host, id, "asked to publish", |f| {
        f.status == TaskStatus::Review
            && f.pending_decisions()
                .any(|d| d.summary.contains("git push"))
    })
    .await;
    let d = f
        .pending_decisions()
        .find(|d| d.summary.contains("git push"))
        .unwrap();
    assert!(d.user_only, "publishing is the developer's call");
    assert_eq!(d.risk, Risk::High);
    assert_eq!(
        d.summary,
        "Run `git push origin feature/csv` and open a pull request"
    );
    host.conn()
        .await
        .task_act(
            "publish",
            id,
            TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
}

fn write_checks(dir: &Path, file: &str, checks: serde_json::Value) {
    std::fs::write(dir.join(file), checks.to_string()).unwrap();
}

#[tokio::test]
async fn a_task_ships_as_a_pull_request_once_ci_is_green() {
    use otter_core::task::{EvidenceKind, GateStatus, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, origin) = git_task(&fake).await;
    allow_publish(&fake.host, &id).await;

    // Pushed for real, PR opened through gh; CI hasn't reported yet.
    let f = wait_task(&fake.host, &id, "published", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    assert!(git(&origin, &["branch", "--list", "feature/csv"]).contains("feature/csv"));
    let body = std::fs::read_to_string(fake.gh.join("body")).unwrap();
    assert!(body.contains("Export the timeline as CSV") && body.contains("Otter doesn't merge"));
    assert!(
        f.evidence
            .iter()
            .any(|e| e.kind == EvidenceKind::PullRequest
                && e.uri.as_deref() == Some("https://github.com/o/r/pull/7"))
    );
    let f = wait_task(&fake.host, &id, "CI pending", |f| {
        f.gates
            .iter()
            .any(|g| g.name == "CI" && g.status == GateStatus::Pending)
    })
    .await;
    assert!(f.report.is_none());
    // Not done while a gate waits.
    let err = fake
        .host
        .conn()
        .await
        .task_act(
            "early",
            &id,
            TaskAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("gates not met"), "{err:#}");

    // CI fails on the code: back to work, with the log.
    // (No "FAIL" in it: that word makes the fake agent fail.)
    std::fs::write(
        fake.gh.join("log"),
        "test csv::escapes ... broken\nassertion failed: left == right\n",
    )
    .unwrap();
    write_checks(
        &fake.gh,
        "checks.json",
        serde_json::json!([
            {"name": "test", "bucket": "fail", "link": "https://github.com/o/r/actions/runs/99/job/1", "description": "Process completed with exit code 101"},
            {"name": "lint", "bucket": "pass", "link": "", "description": ""}
        ]),
    );
    let f = wait_task(&fake.host, &id, "remediation after failed CI", |f| {
        f.steps.iter().any(|t| t.title == "Fix what CI found")
    })
    .await;
    let fix = f
        .steps
        .iter()
        .find(|t| t.title == "Fix what CI found")
        .unwrap();
    assert!(
        fix.detail.as_deref().unwrap().contains("assertion failed"),
        "{:?}",
        fix.detail
    );
    assert!(
        f.evidence
            .iter()
            .any(|e| e.kind == EvidenceKind::Ci && e.ok == Some(false))
    );

    // The fix lands and CI goes green: every gate passes, the report is written and posted.
    write_checks(
        &fake.gh,
        "checks.json",
        serde_json::json!([
            {"name": "test", "bucket": "pass", "link": "", "description": ""},
            {"name": "lint", "bucket": "pass", "link": "", "description": ""}
        ]),
    );
    let f = wait_task(&fake.host, &id, "ready to accept", |f| {
        f.status == TaskStatus::Review && f.report.is_some()
    })
    .await;
    assert!(
        f.gates
            .iter()
            .filter(|g| g.name != "Your acceptance")
            .all(|g| g.status.clear()),
        "{:?}",
        f.gates
    );
    let report = f.report.clone().unwrap();
    assert!(report.contains("Pull request: https://github.com/o/r/pull/7"));
    eventually("the report on the PR", || async {
        std::fs::read_to_string(fake.gh.join("comment"))
            .ok()
            .filter(|c| c.contains("## Gates"))
    })
    .await;
    let f = fake
        .host
        .conn()
        .await
        .task_act(
            "accept",
            &id,
            TaskAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Done);
    // Never merged, never anything gh wasn't meant to do; no token on a command line.
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap();
    assert!(!calls.contains("pr merge"), "{calls}");
    assert!(!calls.to_lowercase().contains("token"), "{calls}");
}

/// Publishing commits what the agent left uncommitted — never files that
/// may hold secrets — and pushes that (D-068).
#[tokio::test]
async fn publishing_commits_the_work_but_not_its_secrets() {
    use otter_core::task::{EvidenceKind, MessageRole, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, origin) = git_task(&fake).await;
    let ws = fake.host.conn().await.workspace_get("csv").await.unwrap();
    let root = std::path::PathBuf::from(&ws.root);
    std::fs::write(root.join("export.sh"), "echo id,name\n").unwrap();
    std::fs::write(root.join(".env"), "API_TOKEN=do-not-push\n").unwrap();
    allow_publish(&fake.host, &id).await;
    let f = wait_task(&fake.host, &id, "published", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    assert_eq!(f.status, TaskStatus::Review);
    let pushed = git(&origin, &["show", "--stat", "--format=%s", "feature/csv"]);
    assert!(pushed.starts_with("CSV export"), "{pushed}");
    assert!(pushed.contains("export.sh"), "{pushed}");
    assert!(!pushed.contains(".env"), "{pushed}");
    let tree = git(&origin, &["ls-tree", "-r", "--name-only", "feature/csv"]);
    assert!(!tree.contains(".env"), "{tree}");
    let head = f.delivery.as_ref().unwrap().head.clone().unwrap();
    assert_eq!(git(&origin, &["rev-parse", "feature/csv"]).trim(), head);
    assert!(f.evidence.iter().any(|e| e.kind == EvidenceKind::Diff
        && e.title.starts_with("Committed ")
        && e.detail.as_deref() == Some(head.as_str())));
    assert!(f.messages.iter().any(|m| m.role == MessageRole::Controller
        && m.text.contains("leaving out what may hold secrets: .env")));
}

/// CI failing the same way after a fix goes to the developer, once — not
/// round after round of fixes (D-068).
#[tokio::test]
async fn the_same_ci_failure_after_a_fix_goes_to_the_developer() {
    use otter_core::task::{EvidenceKind, GateStatus, MessageRole, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, _) = git_task(&fake).await;
    allow_publish(&fake.host, &id).await;
    // gh's shape: a time on every line, the test run's duration (D-139).
    std::fs::write(
        fake.gh.join("log"),
        "test\tRun make test\t\u{feff}@TIME@ assertion failed: left == right\n\
         test\tRun make test\t@TIME@ Ran 21 tests in @SECS@\n",
    )
    .unwrap();
    write_checks(
        &fake.gh,
        "checks.json",
        serde_json::json!([
            {"name": "test", "bucket": "fail", "link": "https://github.com/o/r/actions/runs/99/job/1", "description": "exit code 101"}
        ]),
    );
    let over = |f: &otter_core::task::Task| {
        f.messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Controller && m.text.starts_with("CI failed the same way")
            })
            .count()
    };
    let f = wait_task(&fake.host, &id, "handed to the developer", |f| {
        f.status == TaskStatus::Review && over(f) == 1
    })
    .await;
    // Some polls later, still once, and no second fix.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let f2 = fake.host.conn().await.task_get(&id).await.unwrap();
    assert_eq!(over(&f2), 1);
    assert_eq!(f2.status, TaskStatus::Review);
    assert_eq!(
        f2.steps
            .iter()
            .filter(|t| t.title == "Fix what CI found")
            .count(),
        1
    );
    assert_eq!(
        f2.evidence
            .iter()
            .filter(|e| e.kind == EvidenceKind::Ci && e.ok == Some(false))
            .count(),
        1
    );
    assert!(
        f.gates
            .iter()
            .any(|g| g.name == "CI" && g.status == GateStatus::Failed),
        "{:?}",
        f.gates
    );
}

/// Otter stopping and starting again at each delivery checkpoint does
/// nothing twice: one run, one pull request, one branch (D-068).
#[tokio::test]
async fn delivery_resumes_at_each_checkpoint_without_repeating_itself() {
    use otter_core::task::{EvidenceKind, TaskAction, TaskStatus};
    let mut fake = fake_agent_host("ok", "rules").await;
    let (id, origin) = git_task(&fake).await;
    // Asked to publish.
    wait_task(&fake.host, &id, "asked to publish", |f| {
        f.pending_decisions()
            .any(|d| d.summary.contains("git push"))
    })
    .await;
    fake.host.restart().await;
    allow_publish(&fake.host, &id).await;
    // Published; CI not reported yet.
    wait_task(&fake.host, &id, "published", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    fake.host.restart().await;
    write_checks(
        &fake.gh,
        "checks.json",
        serde_json::json!([{"name": "test", "bucket": "pass", "link": "", "description": ""}]),
    );
    // Ready for acceptance.
    wait_task(&fake.host, &id, "ready", |f| f.report.is_some()).await;
    fake.host.restart().await;
    let f = fake
        .host
        .conn()
        .await
        .task_act(
            "accept",
            &id,
            TaskAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Done);
    assert_eq!(f.runs.len(), 1, "{:?}", f.runs);
    assert_eq!(
        f.evidence
            .iter()
            .filter(|e| e.kind == EvidenceKind::PullRequest)
            .count(),
        1
    );
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap();
    assert_eq!(calls.matches("gh pr create").count(), 1, "{calls}");
    assert_eq!(
        git(&origin, &["branch", "--list"])
            .lines()
            .filter(|l| l.contains("feature/csv"))
            .count(),
        1
    );
}

/// An open pull request for the branch is adopted, not opened again; and a
/// cancelled task says what stays (D-068).
#[tokio::test]
async fn an_open_pull_request_is_adopted_and_cancelling_keeps_the_work() {
    use otter_core::task::{MessageRole, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, origin) = git_task(&fake).await;
    std::fs::write(fake.gh.join("pr"), "").unwrap();
    allow_publish(&fake.host, &id).await;
    let f = wait_task(&fake.host, &id, "published", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    assert_eq!(f.delivery.as_ref().unwrap().pr_number, Some(7));
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap();
    assert!(
        calls.contains("gh pr edit") && !calls.contains("gh pr create"),
        "{calls}"
    );
    let f = fake
        .host
        .conn()
        .await
        .task_act("cancel", &id, TaskAction::Cancel)
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Cancelled);
    let said = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Controller)
        .unwrap();
    assert!(
        said.text.starts_with("Cancelled. Nothing is removed"),
        "{}",
        said.text
    );
    assert!(
        said.text.contains("`feature/csv`") && said.text.contains("pull/7"),
        "{}",
        said.text
    );
    let ws = fake.host.conn().await.workspace_get("csv").await.unwrap();
    assert!(std::path::Path::new(&ws.root).is_dir(), "the work stays");
    assert!(git(&origin, &["branch", "--list", "feature/csv"]).contains("feature/csv"));
}

#[tokio::test]
async fn flaky_ci_is_rerun_not_blamed_on_the_code() {
    use otter_core::task::TaskStatus;
    let fake = fake_agent_host("ok", "rules").await;
    let (id, _) = git_task(&fake).await;
    // The runner dies; a rerun passes.
    write_checks(
        &fake.gh,
        "checks.json",
        serde_json::json!([
            {"name": "test", "bucket": "fail", "link": "https://github.com/o/r/actions/runs/5/job/9", "description": "The runner has received a shutdown signal."}
        ]),
    );
    write_checks(
        &fake.gh,
        "after-rerun.json",
        serde_json::json!([
            {"name": "test", "bucket": "pass", "link": "", "description": ""}
        ]),
    );
    allow_publish(&fake.host, &id).await;
    let f = wait_task(&fake.host, &id, "green after a rerun", |f| {
        f.report.is_some()
    })
    .await;
    assert_eq!(f.status, TaskStatus::Review);
    assert!(
        !f.steps.iter().any(|t| t.title == "Fix what CI found"),
        "no remediation for a flake"
    );
    assert_eq!(f.delivery.as_ref().unwrap().reruns, 1);
    assert_eq!(
        std::fs::read_to_string(fake.gh.join("reruns"))
            .unwrap()
            .trim(),
        "5"
    );
}

#[tokio::test]
async fn declining_to_publish_keeps_delivery_local() {
    use otter_core::task::{GateStatus, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, origin) = git_task(&fake).await;
    let f = wait_task(&fake.host, &id, "asked to publish", |f| {
        f.pending_decisions()
            .any(|d| d.summary.contains("git push"))
    })
    .await;
    let d = f
        .pending_decisions()
        .find(|d| d.summary.contains("git push"))
        .unwrap()
        .clone();
    fake.host
        .conn()
        .await
        .task_act(
            "no",
            &id,
            TaskAction::Decide {
                decision_id: d.id,
                approve: false,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "report without a PR", |f| {
        f.report.is_some()
    })
    .await;
    assert!(f.delivery.as_ref().unwrap().declined);
    let gate = |n: &str| f.gates.iter().find(|g| g.name == n).unwrap().status;
    assert_eq!(gate("Pull request"), GateStatus::NotApplicable);
    assert_eq!(gate("CI"), GateStatus::NotApplicable);
    assert!(
        !git(&origin, &["branch", "--list", "feature/csv"]).contains("feature/csv"),
        "nothing pushed"
    );
    assert!(
        !fake.gh.join("calls.log").exists()
            || !std::fs::read_to_string(fake.gh.join("calls.log"))
                .unwrap()
                .contains("pr create")
    );
    let f = fake
        .host
        .conn()
        .await
        .task_act(
            "accept",
            &id,
            TaskAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Done);
}

#[tokio::test]
async fn settings_take_keys_write_only_and_keep_them() {
    use otter_protocol::host::SettingsUpdate;
    use std::os::unix::fs::PermissionsExt;
    let mut host = TestHost::with_env(&[("OTTER_CONTROLLER", "off".into())]).await;
    let mut conn = host.conn().await;
    let s = conn.settings_get().await.unwrap();
    assert_eq!(s.controller_from_env.as_deref(), Some("off"));
    assert!(
        s.secrets
            .iter()
            .any(|x| x.name == "OPENROUTER_API_KEY" && !x.set)
    );

    let s = conn
        .settings_set(SettingsUpdate {
            controller: Some("openrouter".into()),
            model: Some("anthropic/some-model".into()),
            secrets: [(
                "OPENROUTER_API_KEY".to_owned(),
                Some("sk-or-test-123".to_owned()),
            )]
            .into(),
            coding: None,
            otter_tools: None,
            delete_merged_workspaces: None,
            git: None,
            tool_levels: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(s.controller.as_deref(), Some("openrouter"));
    assert!(
        s.secrets
            .iter()
            .any(|x| x.name == "OPENROUTER_API_KEY" && x.set)
    );
    // The value never comes back, nor lands in the event log.
    let json = serde_json::to_string(&conn.settings_get().await.unwrap()).unwrap();
    assert!(!json.contains("sk-or-test-123"));
    let events = std::fs::read_to_string(host.home().join("state/events.jsonl")).unwrap();
    assert!(!events.contains("sk-or-test-123"));
    let secrets = host.home().join("state/secrets.json");
    assert_eq!(
        std::fs::metadata(&secrets).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Unknown names are refused.
    let err = conn
        .settings_set(SettingsUpdate {
            secrets: [("AWS_SECRET_ACCESS_KEY".to_owned(), Some("x".to_owned()))].into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("unknown secret"), "{err:#}");
    drop(conn);

    // Kept across a restart; cleared with null.
    host.restart().await;
    let mut conn = host.conn().await;
    let s = conn.settings_get().await.unwrap();
    assert_eq!(s.model.as_deref(), Some("anthropic/some-model"));
    assert!(s.secrets[0].set);
    let s = conn
        .settings_set(SettingsUpdate {
            secrets: [("OPENROUTER_API_KEY".to_owned(), None)].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!s.secrets[0].set);
    assert!(!std::fs::read_to_string(&secrets).unwrap().contains("sk-or"));

    // Every provider is a choice; models are listed per controller (D-052).
    for id in [
        "openrouter",
        "anthropic",
        "openai",
        "gemini",
        "claude",
        "off",
    ] {
        assert!(s.controllers.iter().any(|c| c.id == id), "{id}");
    }
    let models = conn.settings_models("claude").await.unwrap();
    assert!(models.iter().any(|m| m.id == "sonnet"));
    let err = conn.settings_models("openai").await.unwrap_err();
    assert!(format!("{err:#}").contains("set the OpenAI key"), "{err:#}");
    assert!(conn.settings_models("off").await.is_err());
}

#[tokio::test]
async fn the_control_agent_answers_the_developer() {
    use otter_core::task::{MessageRole, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "A long job HANG").await;
    wait_task(&fake.host, &id, "working", |f| f.live_run().is_some()).await;
    let mut conn = fake.host.conn().await;
    conn.task_act("pause", &id, TaskAction::Pause)
        .await
        .unwrap();

    let f = conn
        .task_send("ask-1", &id, "what are you doing?")
        .await
        .unwrap();
    let asked = f.messages.last().unwrap().id.clone();
    let f = wait_task(&fake.host, &id, "an answer", |f| {
        f.messages.iter().any(|m| {
            m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(asked.as_str())
        })
    })
    .await;
    let answer = f.messages.last().unwrap();
    assert!(answer.text.contains("paused"), "{}", answer.text);
    // Without a model nothing reads intent into words: still paused, and it says so.
    assert!(answer.text.contains("No model"), "{}", answer.text);
    conn.task_send("ask-2", &id, "continue").await.unwrap();
    let f = wait_task(&fake.host, &id, "a second answer", |f| {
        f.messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Controller
                    && m.correlation_id
                        .as_deref()
                        .is_some_and(|c| c.starts_with("msg_"))
            })
            .count()
            == 2
    })
    .await;
    assert_eq!(f.status, TaskStatus::Paused);
    // Answered once each: no further replies pile up.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let f = conn.task_get(&id).await.unwrap();
    assert_eq!(
        f.messages
            .iter()
            .filter(|m| m.role == MessageRole::Controller
                && m.correlation_id
                    .as_deref()
                    .is_some_and(|c| c.starts_with("msg_")))
            .count(),
        2
    );
}

/// What the agent does is shown as it happens without rewriting the
/// task for every tool: an event, and the task as read (D-069); the
/// lines are kept in the task when the run ends.
#[tokio::test]
async fn activity_is_live_without_rewriting_the_task() {
    use otter_core::task::TaskAction;
    let fake = fake_agent_host("ok", "rules").await;
    let mut events = fake.host.conn().await.subscribe(None).await.unwrap();
    let id = started_task(&fake.host, "Keep building BUSY").await;
    let line = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Event::TaskActivity { task_id, line, .. } =
                events.next().await.unwrap().unwrap().event
            {
                assert_eq!(task_id.as_str(), id);
                return line;
            }
        }
    })
    .await
    .expect("the activity event");
    assert_eq!(line, "$ cargo build");
    let f = wait_task(&fake.host, &id, "the activity, read live", |f| {
        f.live_run()
            .is_some_and(|r| r.activity == ["$ cargo build"])
    })
    .await;
    // Not in the document while it runs.
    let doc = std::fs::read_to_string(
        fake.host
            .home()
            .join(format!("state/tasks/{}/task.json", f.id)),
    )
    .unwrap();
    assert!(!doc.contains("cargo build"), "{doc}");
    let log = std::fs::read_to_string(fake.host.home().join("state/events.jsonl")).unwrap();
    assert!(!log.contains("TaskActivity"));
    // What an action returns shows it too (the app keeps that copy).
    let f = fake
        .host
        .conn()
        .await
        .task_send("msg-live", &id, "Keep going")
        .await
        .unwrap();
    assert_eq!(f.live_run().unwrap().activity, ["$ cargo build"]);
    // Kept once the run ends.
    fake.host
        .conn()
        .await
        .task_act("stop", &id, TaskAction::Pause)
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "the run over", |f| f.live_run().is_none()).await;
    assert_eq!(f.runs[0].activity, ["$ cargo build"]);
}

#[tokio::test]
async fn the_agents_text_streams_as_it_is_written() {
    use otter_core::task::MessageRole;
    let fake = fake_agent_host("ok", "rules").await;
    // Listen first, then start: everything said is seen.
    let mut events = fake.host.conn().await.subscribe(None).await.unwrap();
    let head = events.seq;
    let id = started_task(&fake.host, "Stream it").await;
    let mut pieces = Vec::new();
    let done = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let rec = events.next().await.unwrap().unwrap();
            if let Event::TaskStream {
                task_id,
                role: MessageRole::Agent,
                text,
                done,
                ..
            } = rec.event
            {
                assert_eq!(task_id.as_str(), id);
                // Transient: never past the logged head (no cursor moves).
                assert!(rec.seq >= head);
                if done {
                    return text;
                }
                pieces.push(text);
            }
        }
    })
    .await
    .expect("the stream finishes");
    assert_eq!(
        pieces.first().map(String::as_str),
        Some("Did "),
        "{pieces:?}"
    );
    assert_eq!(done, "Did the work.");
    // Transient: the task keeps the turn's result, not what streamed
    // along the way (D-069), and the event log keeps neither the stream
    // nor the activity.
    let f = wait_task(&fake.host, &id, "its result kept", |f| {
        f.messages.iter().any(|m| m.text.starts_with("done in"))
    })
    .await;
    assert!(!f.messages.iter().any(|m| m.text == "Did the work."));
    let log = std::fs::read_to_string(fake.host.home().join("state/events.jsonl")).unwrap();
    assert!(!log.contains("TaskStream"));
    assert!(!log.contains("TaskActivity"));
}

#[tokio::test]
async fn a_task_that_isnt_running_can_be_deleted() {
    use otter_core::task::TaskAction;
    use otter_protocol::task::TaskCreate;

    let host = TestHost::with_env(&[("OTTER_CONTROLLER", "off".into())]).await;
    create(&host, "dw", Some(vec![])).await;
    let mut conn = host.conn().await;
    let f = conn
        .task_create(TaskCreate {
            command_id: "cmd-del-1".into(),
            title: "Throwaway".into(),
            request: "Nothing much".into(),
            workspace: Some("dw".into()),
            answer_only: false,
        })
        .await
        .unwrap();
    let id = f.id.as_str();
    let dir = host.home().join("state/tasks").join(id);
    assert!(dir.join("task.json").exists());

    // Being worked on: not until it's paused or cancelled.
    conn.task_act("cmd-del-start", id, TaskAction::Start)
        .await
        .unwrap();
    let err = conn.task_delete(id).await.unwrap_err();
    assert!(format!("{err:#}").contains("pause or cancel"), "{err:#}");
    conn.task_act("cmd-del-cancel", id, TaskAction::Cancel)
        .await
        .unwrap();

    let mut stream = host.conn().await.subscribe(None).await.unwrap();
    conn.task_delete(id).await.unwrap();
    assert!(conn.task_list().await.unwrap().is_empty());
    assert!(conn.task_get(id).await.is_err());
    assert!(!dir.exists(), "history and artifacts go with it");
    // The workspace stays; clients hear the task is gone.
    assert!(conn.workspace_get("dw").await.is_ok());
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let r = stream.next().await.unwrap().expect("stream open");
            if let Event::TaskDeleted { task_id } = &r.event {
                assert_eq!(task_id.as_str(), id);
                break;
            }
        }
    })
    .await
    .expect("TaskDeleted");
    // Twice: there's nothing left to delete.
    assert!(conn.task_delete(id).await.is_err());
}

#[tokio::test]
async fn a_run_takes_the_developers_message_as_a_turn_of_its_own() {
    use otter_core::conversation::{
        Delivery, InteractionKind, InteractionStatus, MessageLifecycle, ToolStatus, TurnOutcome,
    };
    use otter_core::task::{RunState, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let mut conn = fake.host.conn().await;
    let caps = conn.runtime_capabilities().await.unwrap();
    assert_eq!(caps.backend, "legacy_cli");
    assert!(caps.available && !caps.structured_ready);

    let id = started_task(&fake.host, "Pick a colour ASK_QUESTION").await;
    let f = wait_task(&fake.host, &id, "a question for the developer", |f| {
        f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    // Written while the agent waits: queued as its own turn.
    conn.task_send("msg-1", &id, "Use the brand colours")
        .await
        .unwrap();
    conn.task_act(
        "answer",
        &id,
        TaskAction::Decide {
            decision_id: d.id.clone(),
            approve: true,
            answer: Some("blue".into()),
            answers: None,
        },
    )
    .await
    .unwrap();
    let f = wait_task(&fake.host, &id, "review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    assert_eq!(f.runs.len(), 1, "one run, two turns");
    assert_eq!(f.runs[0].state, RunState::Completed);

    let cid = f.runs[0].conversation_id.clone().unwrap();
    let all = conn.conversation_list(Some(&id)).await.unwrap();
    assert_eq!(all.len(), 1);
    let c = conn
        .conversation_get(cid.as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(c.task_id.as_ref().map(|f| f.as_str()), Some(id.as_str()));
    assert_eq!(c.turns.len(), 2);
    assert!(c.turns[1].text().contains("Use the brand colours"));
    assert!(!c.turns[0].text().contains("brand"), "never merged");
    for t in &c.turns {
        assert_eq!(t.outcome, Some(TurnOutcome::Completed));
        assert_eq!(t.delivery, Delivery::Delivered);
        assert_eq!(t.run_id.as_ref(), Some(&f.runs[0].id));
    }
    // The question kept its form, and the answer went back once.
    let i = &c.interactions[0];
    assert_eq!(i.id, d.id, "the task's decision is the interaction");
    assert!(
        matches!(&i.kind, InteractionKind::Questions { questions } if questions[0].prompt == "Pick one")
    );
    assert_eq!(i.status, InteractionStatus::Delivered);
    // The second turn ran a tool and reported its result; its text is whole, once.
    assert_eq!(c.tools.len(), 1);
    assert_eq!(c.tools[0].status, ToolStatus::Succeeded);
    assert_eq!(c.tools[0].result.as_ref().unwrap().summary, "ok");
    let said: Vec<_> = c.messages.iter().map(|m| (m.text(), m.lifecycle)).collect();
    assert!(said.contains(&("Did the work.".to_owned(), MessageLifecycle::Completed)));
    assert_eq!(said.iter().filter(|(t, _)| t == "Did the work.").count(), 1);

    // Its journal (D-058): every change in order, a page at a time.
    use otter_protocol::conversation::{HistoryCursor, HistoryQuery};
    let page = conn
        .conversation_history(HistoryQuery {
            conversation: cid.to_string(),
            after: None,
            limit: Some(3),
        })
        .await
        .unwrap();
    assert_eq!(page.records.len(), 3);
    assert_eq!(page.records[0]["kind"], "create");
    assert_eq!(page.records[1]["op"]["op"], "accept");
    let next = page.next.clone().expect("more");
    let rest = conn
        .conversation_history(HistoryQuery {
            conversation: cid.to_string(),
            after: Some(next),
            limit: Some(500),
        })
        .await
        .unwrap();
    assert!(rest.next.is_none());
    let seqs: Vec<u64> = page
        .records
        .iter()
        .chain(&rest.records)
        .map(|r| r["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>(), "no gaps");
    let err = conn
        .conversation_history(HistoryQuery {
            conversation: cid.to_string(),
            after: Some(HistoryCursor {
                log_id: "log_other".into(),
                seq: 1,
            }),
            limit: None,
        })
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("another journal"), "{err:#}");
    // Its records are on disk, this user's only.
    let journal = fake
        .host
        .home()
        .join("state/conversations")
        .join(cid.as_str())
        .join("journal.jsonl");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

/// A host whose coding agent runs through the SDK worker (D-057) — a fake
/// one, speaking the worker protocol (`tests/fake_claude_worker.sh`).
async fn fake_sdk_host() -> (FakeCodex, std::path::PathBuf) {
    fake_sdk_host_in("ok").await
}

/// [`fake_sdk_host`], its worker in a mode (`FAKE_WORKER_MODE`).
async fn fake_sdk_host_in(mode: &'static str) -> (FakeCodex, std::path::PathBuf) {
    let log = std::sync::Arc::new(std::sync::Mutex::new(std::path::PathBuf::new()));
    let keep = log.clone();
    let fake = fake_agent_host_with("ok", "rules", move |scratch| {
        let worker = scratch.join("worker.sh");
        std::fs::write(&worker, include_str!("fake_claude_worker.sh")).unwrap();
        let worker_log = scratch.join("worker.log");
        *keep.lock().unwrap() = worker_log.clone();
        vec![
            ("OTTER_CODING_BACKEND", "sdk".into()),
            ("OTTER_NODE", "/bin/sh".into()),
            ("OTTER_CLAUDE_WORKER", worker.to_string_lossy().into_owned()),
            ("FAKE_WORKER_LOG", worker_log.to_string_lossy().into_owned()),
            ("FAKE_WORKER_MODE", mode.into()),
        ]
    })
    .await;
    let log = log.lock().unwrap().clone();
    (fake, log)
}

/// A coding agent that can't see pictures isn't given them, and the
/// developer is told, once, on the task.
#[tokio::test]
async fn a_picture_for_an_agent_that_cannot_see_is_said_not_dropped() {
    use base64::Engine;
    use otter_core::task::MessageRole;
    use otter_protocol::task::TaskImage;
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_task(&fake.host, "Keep building BUSY").await;
    wait_task(&fake.host, &id, "a run", |f| f.live_run().is_some()).await;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(b"pixels");
    let send = || async {
        fake.host
            .conn()
            .await
            .task_send_with_images(
                "pic-blind",
                &id,
                "make it look like this",
                vec![TaskImage {
                    media_type: "image/png".into(),
                    data: base64::engine::general_purpose::STANDARD.encode(&png),
                }],
            )
            .await
            .unwrap()
    };
    send().await;
    let f = wait_task(&fake.host, &id, "the notice", |f| {
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::System && m.text.contains("can't see pictures"))
    })
    .await;
    // The words still went on their way, and a repeat says nothing more.
    send().await;
    let f2 = fake.host.conn().await.task_get(&id).await.unwrap();
    let notices = |f: &otter_core::task::Task| {
        f.messages
            .iter()
            .filter(|m| m.text.contains("can't see pictures"))
            .count()
    };
    assert_eq!(notices(&f), 1);
    assert_eq!(notices(&f2), 1);
}

/// A picture sent while the coding agent works reaches it, as a turn of its
/// own, with the words; the task keeps the file.
#[tokio::test]
async fn a_picture_sent_while_the_agent_works_reaches_its_next_turn() {
    use base64::Engine;
    use otter_protocol::task::TaskImage;
    let (fake, worker_log) = fake_sdk_host_in("slow").await;
    let id = started_task(&fake.host, "Do the work").await;
    wait_task(&fake.host, &id, "a run", |f| f.live_run().is_some()).await;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(b"pixels");
    let sent = fake
        .host
        .conn()
        .await
        .task_send_with_images(
            "pic-live",
            &id,
            "make it look like this",
            vec![TaskImage {
                media_type: "image/png".into(),
                data: base64::engine::general_purpose::STANDARD.encode(&png),
            }],
        )
        .await
        .unwrap();
    assert_eq!(sent.messages.last().unwrap().attachments.len(), 1);
    eventually("the picture reached the worker", || async {
        let log = std::fs::read_to_string(&worker_log).unwrap();
        log.lines()
            .any(|l| {
                l.contains(r#""type":"send_turn""#)
                    && l.contains("make it look like this")
                    && l.contains(r#""images":[{"data":""#)
                    && l.contains(r#""media_type":"image/png""#)
            })
            .then_some(())
    })
    .await;
    // The path on the host is nowhere in what the agent was told.
    let log = std::fs::read_to_string(&worker_log).unwrap();
    assert!(!log.contains("/artifacts/"), "{log}");
}

/// A run's first turn carries the newest pictures that fit — four, of the
/// six two messages sent before it started — and the agent and the
/// developer are told the rest were left out (D-181): the worker would
/// refuse the whole turn over its limits.
#[tokio::test]
async fn a_runs_first_turn_carries_the_pictures_that_fit() {
    use base64::Engine;
    use otter_core::task::TaskAction;
    use otter_protocol::task::{TaskCreate, TaskImage};
    let (fake, worker_log) = fake_sdk_host().await;
    let host = &fake.host;
    let dir = host.home().join("pics");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    let mut conn = host.conn().await;
    conn.workspace_create(WorkspaceCreate {
        name: "pics".into(),
        source: SourceSpec::Directory {
            path: dir.to_string_lossy().into_owned(),
        },
        sessions: Some(vec![]),
        ..Default::default()
    })
    .await
    .unwrap();
    let f = conn
        .task_create(TaskCreate {
            command_id: "pics-create".into(),
            title: "Match the mock".into(),
            request: "Make the page look like the mock".into(),
            workspace: Some("pics".into()),
            answer_only: false,
        })
        .await
        .unwrap();
    let id = f.id.as_str();
    let picture = |n: u8| {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.push(n);
        TaskImage {
            media_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(&png),
        }
    };
    conn.task_send_with_images("pics-1", id, "first mock", (1..=3).map(picture).collect())
        .await
        .unwrap();
    conn.task_send_with_images("pics-2", id, "second mock", (4..=6).map(picture).collect())
        .await
        .unwrap();
    conn.task_act("pics-start", id, TaskAction::Start)
        .await
        .unwrap();
    let turn = eventually("the run's first turn", || async {
        let log = std::fs::read_to_string(&worker_log).unwrap_or_default();
        log.lines()
            .find(|l| l.contains(r#""type":"send_turn""#) && l.contains("second mock"))
            .map(str::to_owned)
    })
    .await;
    let v: serde_json::Value = serde_json::from_str(&turn).unwrap();
    let images = v["payload"]["images"].as_array().unwrap();
    // The newest four, in the order they were sent: the first message's
    // last, then the second's.
    let sent: Vec<String> = (3..=6).map(|n| picture(n).data).collect();
    let got: Vec<&str> = images.iter().map(|i| i["data"].as_str().unwrap()).collect();
    assert_eq!(got, sent);
    assert!(
        v["payload"]["text"].as_str().unwrap().contains(
            "(2 older pictures the developer sent are left out: a turn carries at most 4, 2 MB in all.)"
        ),
        "{turn}"
    );
    // And so is the developer, on the task.
    wait_task(host, id, "the developer told", |f| {
        f.messages.iter().any(|m| {
            m.role == otter_core::task::MessageRole::System
                && m.text.starts_with("The coding agent was given the newest 4 pictures you sent, not the 2 before them")
        })
    })
    .await;
}

/// Claude Code's sign-in from Settings (D-142): a token from `claude
/// setup-token` reaches the coding agent's worker, for a test and for a
/// task's run, and goes when cleared; it is never shown or written out.
#[tokio::test]
async fn claude_codes_token_from_settings_signs_its_runs_in() {
    use otter_protocol::host::SettingsUpdate;
    let (fake, worker_log) = fake_sdk_host().await;
    let mut conn = fake.host.conn().await;
    let token = "sk-ant-oat01-SETTINGSTOKEN0123456789abcdef";
    let set = |v: Option<&str>| SettingsUpdate {
        secrets: std::collections::BTreeMap::from([(
            "CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
            v.map(String::from),
        )]),
        ..Default::default()
    };
    // Not a token: refused.
    let err = conn.settings_set(set(Some("sk-ant-api03-nope"))).await;
    assert!(format!("{:#}", err.unwrap_err()).contains("claude setup-token"));
    conn.runtime_test(None, None).await.unwrap();
    let log = || std::fs::read_to_string(&worker_log).unwrap();
    assert!(log().contains("oauth token: unset"), "{}", log());
    assert!(!log().contains("oauth token: set"));

    let s = conn.settings_set(set(Some(token))).await.unwrap();
    let k = s
        .secrets
        .iter()
        .find(|k| k.name == "CLAUDE_CODE_OAUTH_TOKEN")
        .unwrap();
    assert!(k.set && k.purpose.contains("claude setup-token"), "{k:?}");
    assert!(!serde_json::to_string(&s).unwrap().contains(token));
    std::fs::write(&worker_log, "").unwrap();
    conn.runtime_test(None, None).await.unwrap();
    assert!(log().contains("oauth token: set"), "{}", log());

    // A task's run is signed in the same way.
    std::fs::write(&worker_log, "").unwrap();
    let id = started_task(&fake.host, "Export the timeline as CSV").await;
    wait_task(&fake.host, &id, "a run", |f| !f.runs.is_empty()).await;
    eventually("the worker started", || async {
        log().contains("oauth token:").then_some(())
    })
    .await;
    assert!(!log().contains("oauth token: unset"), "{}", log());

    // Nowhere in what otterd writes but its secrets file.
    fn scan(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                scan(&p, found);
            } else if p.file_name().is_some_and(|n| n != "secrets.json")
                && std::fs::read(&p).is_ok_and(|b| b.windows(13).any(|w| w == b"SETTINGSTOKEN"))
            {
                found.push(p);
            }
        }
    }
    let mut found = Vec::new();
    scan(fake.host.home(), &mut found);
    assert!(found.is_empty(), "{found:?}");

    // Cleared: the host's own sign-in again.
    conn.settings_set(set(None)).await.unwrap();
    std::fs::write(&worker_log, "").unwrap();
    conn.runtime_test(None, None).await.unwrap();
    assert!(log().contains("oauth token: unset"), "{}", log());
}

#[tokio::test]
async fn the_coding_agent_is_tested_with_one_short_reply() {
    use otter_protocol::conversation::StepState;
    let (fake, worker_log) = fake_sdk_host().await;
    let mut conn = fake.host.conn().await;
    // The host's backend, with a small model: the tool it tries is refused,
    // and it answers.
    let t = conn.runtime_test(None, None).await.unwrap();
    assert!(t.ready, "{t:?}");
    assert_eq!(t.backend, "sdk");
    let states: Vec<StepState> = t.steps.iter().map(|s| s.state).collect();
    assert_eq!(states, [StepState::Passed, StepState::Passed], "{t:?}");
    assert_eq!(
        t.steps[0].detail,
        "worker 0.1.0 · claude-agent-sdk fake · node 0"
    );
    assert!(t.steps[1].detail.contains("with haiku"), "{t:?}");
    let log = std::fs::read_to_string(&worker_log).unwrap();
    assert!(log.contains(r#""model":"haiku""#), "{log}");
    assert!(
        log.contains("runtime-test"),
        "in a scratch directory: {log}"
    );
    assert!(log.contains(r#""decision":"deny""#), "{log}");
    // Nothing is left behind: no directory, no conversation.
    assert!(!fake.host.home().join("state/runtime-test").exists());
    assert!(conn.conversation_list(None).await.unwrap().is_empty());

    // Another backend and model than the saved ones (not saved by testing).
    let t = conn
        .runtime_test(Some("legacy_cli"), Some("sonnet"))
        .await
        .unwrap();
    assert!(t.ready && t.backend == "legacy_cli", "{t:?}");
    assert!(t.steps[1].detail.contains("with sonnet"), "{t:?}");
    let args = std::fs::read_to_string(&fake.log).unwrap();
    assert!(args.contains("--model sonnet"), "{args}");
    let s = conn.settings_get().await.unwrap();
    assert_eq!(s.coding.backend.as_deref(), Some("sdk"));
    assert!(conn.runtime_test(Some("nope"), None).await.is_err());

    // A worker that can't sign in says so, without its secrets.
    let failing = fake_agent_host_with("ok", "rules", |scratch| {
        let worker = scratch.join("worker.sh");
        std::fs::write(&worker, include_str!("fake_claude_worker.sh")).unwrap();
        vec![
            ("OTTER_CODING_BACKEND", "sdk".into()),
            ("OTTER_NODE", "/bin/sh".into()),
            ("OTTER_CLAUDE_WORKER", worker.to_string_lossy().into_owned()),
            ("FAKE_WORKER_MODE", "fatal".into()),
        ]
    })
    .await;
    let t = failing
        .host
        .conn()
        .await
        .runtime_test(None, None)
        .await
        .unwrap();
    assert!(!t.ready);
    assert_eq!(t.steps[1].state, StepState::Failed);
    assert!(
        t.steps[1].detail.starts_with("It didn't start:")
            && t.steps[1].detail.contains("no credentials"),
        "{t:?}"
    );
    assert!(!format!("{t:?}").contains("WORKERLEAK"));

    // Not installed anywhere otterd looks: the run isn't tried, and
    // installing the worker would help (D-129).
    let nobody = tempfile::tempdir().unwrap();
    let missing = TestHost::with_env(&[
        ("OTTER_CONTROLLER", "off".into()),
        ("OTTER_NODE", "/bin/sh".into()),
        ("HOME", nobody.path().to_string_lossy().into_owned()),
    ])
    .await;
    let t = missing.conn().await.runtime_test(None, None).await.unwrap();
    let states: Vec<StepState> = t.steps.iter().map(|s| s.state).collect();
    assert_eq!(states, [StepState::Failed, StepState::Skipped], "{t:?}");
    assert!(t.steps[0].detail.contains("isn't installed"), "{t:?}");
    assert!(t.worker_missing, "{t:?}");
    let caps = missing.conn().await.runtime_capabilities().await.unwrap();
    assert!(caps.worker_missing && !caps.available, "{caps:?}");
    // The fake worker is there: nothing to install.
    assert!(!conn.runtime_capabilities().await.unwrap().worker_missing);
}

#[tokio::test]
async fn a_task_runs_through_the_sdk_worker_with_policy_checks_and_a_form() {
    use otter_core::conversation::{InteractionKind, InteractionResponse, ToolStatus, TurnOutcome};
    use otter_core::task::{DecisionStatus, TaskAction, TaskStatus};
    let (fake, worker_log) = fake_sdk_host().await;
    let mut conn = fake.host.conn().await;
    let caps = conn.runtime_capabilities().await.unwrap();
    assert_eq!(caps.backend, "sdk");
    assert!(caps.available, "{:?}", caps.notes);

    let id = started_task(&fake.host, "Export it").await;
    // `make test` passed Otter's policy silently; the form needs the developer.
    let f = wait_task(&fake.host, &id, "a question form", |f| {
        f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    assert_eq!(
        f.decisions.len(),
        1,
        "the routine command wasn't a decision"
    );
    conn.task_act(
        "answer-form",
        &id,
        TaskAction::Decide {
            decision_id: d.id.clone(),
            approve: true,
            answer: None,
            answers: Some(
                [("Which format?", "JSON"), ("Which columns?", "name, time")]
                    .into_iter()
                    .map(|(q, a)| (q.to_owned(), a.to_owned()))
                    .collect(),
            ),
        },
    )
    .await
    .unwrap();
    let f = wait_task(&fake.host, &id, "review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    let d = f.decisions.iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(d.status, DecisionStatus::Answered);
    assert_eq!(
        d.answer.as_deref(),
        Some("Which columns? name, time; Which format? JSON")
    );

    // Each question got its own answer, in the SDK's shape.
    let told = std::fs::read_to_string(&worker_log).unwrap();
    assert!(
        told.contains(r#""answers":{"Which columns?":"name, time","Which format?":"JSON"}"#),
        "{told}"
    );
    assert!(
        told.contains(r#""decision":"allow""#),
        "policy allowed `make test`"
    );

    let cid = f.runs[0].conversation_id.clone().unwrap();
    let c = conn
        .conversation_get(cid.as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(c.backend, "sdk");
    // What the run ran on, from the worker's own `--check` (D-060).
    assert_eq!(
        c.runtime_version.as_deref(),
        Some("worker 0.1.0 · claude-agent-sdk fake · node 0")
    );
    assert_eq!(c.turns[0].outcome, Some(TurnOutcome::Completed));
    assert_eq!(c.tools[0].status, ToolStatus::Succeeded);
    let i = &c.interactions[0];
    assert!(
        matches!(&i.kind, InteractionKind::Questions { questions } if questions.len() == 2 && questions[1].multi_select)
    );
    assert!(
        matches!(&i.response, Some(InteractionResponse::Answers { answers }) if answers["Which format?"].text.as_deref() == Some("JSON"))
    );
}

#[tokio::test]
async fn the_coding_backend_and_model_are_settings_of_their_own() {
    use otter_protocol::host::{CodingSettings, SettingsUpdate};
    let host = TestHost::with_env(&[("OTTER_CONTROLLER", "off".into())]).await;
    let mut conn = host.conn().await;
    let s = conn.settings_get().await.unwrap();
    // The SDK worker by default (D-124).
    assert_eq!(s.coding.backend.as_deref(), Some("sdk"));
    let s = conn
        .settings_set(SettingsUpdate {
            coding: Some(CodingSettings {
                backend: Some("sdk".into()),
                model: Some("claude-sonnet-5-5".into()),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.coding.backend.as_deref(), Some("sdk"));
    assert_eq!(s.coding.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(s.model, None, "Otter's own model is another setting");
    assert_eq!(conn.runtime_capabilities().await.unwrap().backend, "sdk");
    let err = conn
        .settings_set(SettingsUpdate {
            coding: Some(CodingSettings {
                backend: Some("tmux".into()),
                model: None,
            }),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("unknown coding backend"),
        "{err:#}"
    );
}

/// Real Claude, through the SDK worker, survives an otterd restart in the
/// middle of a turn (D-058). Uses this host's Claude sign-in; run by hand:
/// `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime cargo test -p otterd
/// --test e2e live_sdk_restart -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
async fn live_sdk_restart() {
    use otter_core::conversation::{Delivery, ToolStatus, TurnOutcome};
    use otter_core::task::{RunState, TaskStatus};
    use otter_protocol::host::{CodingSettings, SettingsUpdate};
    let worker = std::env::var("OTTER_CLAUDE_WORKER").expect("OTTER_CLAUDE_WORKER");
    let node = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v node"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let mut host = TestHost::with_env(&[
        ("OTTER_CODING_BACKEND", "sdk".into()),
        ("OTTER_CLAUDE_WORKER", worker),
        ("OTTER_NODE", node),
        ("OTTER_CONTROLLER", "rules".into()),
        ("OTTER_CONTROLLER_TICK_MS", "200".into()),
        ("DEVELOPER_DIR", String::new()),
        ("SDKROOT", String::new()),
    ])
    .await;
    host.conn()
        .await
        .settings_set(SettingsUpdate {
            coding: Some(CodingSettings {
                backend: None,
                model: Some("haiku".into()),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let ws = "livesdk";
    let id = started_in(
        &host,
        ws,
        "Work only in the current working directory. Use the Bash tool to run exactly `sleep 20 && echo slept > slept.txt`, then create ./done.txt (in the current working directory) containing the word ok. Nothing else.",
    )
    .await;
    let wait = |what: &'static str, secs: u64| {
        (what, std::time::Instant::now() + Duration::from_secs(secs))
    };

    // Mid-turn: the sleep is running.
    let (what, deadline) = wait("the sleep running", 180);
    let conv = loop {
        let list = host
            .conn()
            .await
            .conversation_list(Some(&id))
            .await
            .unwrap();
        if let Some(c) = list.first()
            && c.conversation
                .tools
                .iter()
                .any(|t| t.status == ToolStatus::Running && t.summary.contains("sleep"))
        {
            break c.conversation.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let session = host.conn().await.task_get(&id).await.unwrap().runs[0]
        .provider_session_id
        .clone()
        .expect("bound");
    eprintln!(
        "live: conversation {} running the sleep; restarting otterd",
        conv.id
    );
    assert!(conv.process.is_some(), "the run's process is recorded");

    host.restart().await;

    // The turn that was cut off is unknown, never completed; the run resumes
    // the same native session as a new generation.
    let (what, deadline) = wait("the resumed run", 120);
    let f = loop {
        let f = host.conn().await.task_get(&id).await.unwrap();
        if f.runs.len() >= 2 {
            break f;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(
        f.runs[0].state,
        RunState::Cancelled,
        "{:?}",
        f.runs[0].summary
    );
    assert_eq!(f.runs[1].conversation_id.as_ref(), Some(&conv.id));
    assert_eq!(
        f.runs[1].provider_session_id.as_deref(),
        Some(session.as_str())
    );
    let c = host
        .conn()
        .await
        .conversation_get(conv.id.as_str())
        .await
        .unwrap();
    let first = &c.conversation.turns[0];
    assert_eq!(first.outcome, Some(TurnOutcome::OutcomeUnknown));
    assert_eq!(first.delivery, Delivery::Delivered);
    assert!(c.conversation.generation >= 2);
    eprintln!(
        "live: turn 1 {:?}/{:?}; resumed as generation {}",
        first.outcome, first.delivery, c.conversation.generation
    );

    // And the work gets done.
    let (what, deadline) = wait("review", 300);
    let f = loop {
        let f = host.conn().await.task_get(&id).await.unwrap();
        if matches!(
            f.status,
            TaskStatus::Review | TaskStatus::Failed | TaskStatus::Blocked
        ) {
            break f;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            f.status
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(
        f.status,
        TaskStatus::Review,
        "{:?} {:?}",
        f.status_reason,
        f.runs.last().and_then(|r| r.summary.clone())
    );
    assert!(host.home().join(ws).join("done.txt").exists());
    let c = host
        .conn()
        .await
        .conversation_get(conv.id.as_str())
        .await
        .unwrap();
    eprintln!(
        "live: done; {} turns, outcomes {:?}",
        c.conversation.turns.len(),
        c.conversation
            .turns
            .iter()
            .map(|t| t.outcome)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn interrupting_stops_the_turn_and_waits_and_redirecting_goes_next() {
    use otter_core::conversation::TurnOutcome;
    use otter_core::task::{RunState, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let mut conn = fake.host.conn().await;

    // Interrupt: the turn stops, the task waits for the developer.
    let id = started_task_named(&fake.host, "A long job HANG one").await;
    wait_task(&fake.host, &id, "working", |f| {
        f.live_run()
            .is_some_and(|r| r.provider_session_id.is_some())
    })
    .await;
    conn.task_act("interrupt-1", &id, TaskAction::Interrupt)
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "the run over", |f| f.live_run().is_none()).await;
    assert_eq!(f.status, TaskStatus::Paused);
    assert_eq!(
        f.runs[0].state,
        RunState::Cancelled,
        "interrupted isn't failed"
    );
    let cid = f.runs[0].conversation_id.clone().unwrap();
    let c = conn
        .conversation_get(cid.as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(c.turns[0].outcome, Some(TurnOutcome::Interrupted));
    // Nothing is running now: interrupting again is refused.
    assert!(
        conn.task_act("interrupt-2", &id, TaskAction::Interrupt)
            .await
            .is_err()
    );
    // Resumed: the same conversation carries on.
    conn.task_act("resume-1", &id, TaskAction::Resume)
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "resumed", |f| f.runs.len() == 2).await;
    assert_eq!(f.runs[1].conversation_id.as_ref(), Some(&cid));
    conn.task_act("cancel-1", &id, TaskAction::Cancel)
        .await
        .unwrap();

    // Redirect: the turn stops and the new direction goes next, same run.
    let id = started_task_named(&fake.host, "A long job HANG two").await;
    wait_task(&fake.host, &id, "working", |f| {
        f.live_run()
            .is_some_and(|r| r.provider_session_id.is_some())
    })
    .await;
    conn.task_act(
        "redirect-1",
        &id,
        TaskAction::Redirect {
            text: "Do the other thing".into(),
        },
    )
    .await
    .unwrap();
    let f = wait_task(&fake.host, &id, "review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    assert_eq!(f.runs.len(), 1, "the same run went on");
    assert_eq!(f.runs[0].state, RunState::Completed);
    assert!(f.messages.iter().any(|m| m.text == "Do the other thing"));
    let c = conn
        .conversation_get(f.runs[0].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(c.turns[0].outcome, Some(TurnOutcome::Interrupted));
    assert!(c.turns[1].redirect);
    assert!(c.turns[1].text().contains("Do the other thing"));
    assert_eq!(c.turns[1].outcome, Some(TurnOutcome::Completed));
    assert_eq!(
        c.turns
            .iter()
            .filter(|t| t.text().contains("Do the other thing"))
            .count(),
        1,
        "passed on once, not again as guidance"
    );
    // Nothing running: a redirect is refused (send a message instead).
    let err = conn
        .task_act(
            "redirect-2",
            &id,
            TaskAction::Redirect {
                text: "again".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("send a message"), "{err:#}");
}

#[tokio::test]
async fn guidance_reaches_the_agent_once_across_a_pause_and_a_restart() {
    use otter_core::conversation::{Delivery, Lifecycle, TurnState};
    use otter_core::task::{RunState, TaskAction};
    let mut fake = fake_agent_host("ok", "rules").await;
    let id = started_task_named(&fake.host, "A long job HANG three").await;
    wait_task(&fake.host, &id, "working", |f| {
        f.live_run()
            .is_some_and(|r| r.provider_session_id.is_some())
    })
    .await;
    let mut conn = fake.host.conn().await;
    conn.task_send("msg-tabs", &id, "Use tabs, not spaces")
        .await
        .unwrap();
    let f = conn.task_get(&id).await.unwrap();
    let cid = f.runs[0].conversation_id.clone().unwrap();
    let carrying = |c: &otter_core::conversation::Conversation| {
        c.turns
            .iter()
            .filter(|t| t.text().contains("Use tabs"))
            .cloned()
            .collect::<Vec<_>>()
    };
    // Otter passes it on as guidance: a turn of its own, queued.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let c = conn
            .conversation_get(cid.as_str())
            .await
            .unwrap()
            .conversation;
        if !carrying(&c).is_empty() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never queued");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Paused: the run ends; the guidance waits.
    conn.task_act("pause-g", &id, TaskAction::Pause)
        .await
        .unwrap();
    let f = wait_task(&fake.host, &id, "paused", |f| f.live_run().is_none()).await;
    assert_eq!(f.runs[0].state, RunState::Cancelled);
    let c = conn
        .conversation_get(cid.as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(c.lifecycle, Lifecycle::Paused);
    assert_eq!(carrying(&c)[0].state, TurnState::Queued);
    drop(conn);

    // otterd restarts while it waits; then the developer resumes.
    fake.host.restart().await;
    let mut conn = fake.host.conn().await;
    conn.task_act("resume-g", &id, TaskAction::Resume)
        .await
        .unwrap();
    wait_task(&fake.host, &id, "resumed", |f| {
        f.runs.len() == 2 && f.runs[1].state == RunState::Running
    })
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let c = loop {
        let c = conn
            .conversation_get(cid.as_str())
            .await
            .unwrap()
            .conversation;
        if carrying(&c)[0].delivery != Delivery::Queued {
            break c;
        }
        assert!(std::time::Instant::now() < deadline, "never delivered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // Delivered by the turn that carried it — once; the continuation the
    // controller sent doesn't say it again.
    assert_eq!(
        carrying(&c).len(),
        1,
        "{:#?}",
        c.turns.iter().map(|t| t.text()).collect::<Vec<_>>()
    );
    // It says the last turn was cut off (D-063).
    let continuation = c.turns.last().unwrap();
    assert!(
        continuation.text().starts_with(
            "Your last turn was interrupted before it finished. \
             Continue the step where you left off."
        ),
        "{}",
        continuation.text()
    );
    assert!(!continuation.text().contains("Use tabs"));
}

/// Real Claude, through the SDK worker: pause mid-tool, resume, redirect
/// mid-tool (D-059). Uses this host's Claude sign-in; run by hand:
/// `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime cargo test -p otterd
/// --test e2e live_sdk_controls -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
async fn live_sdk_controls() {
    use otter_core::conversation::{Lifecycle, TurnOutcome};
    use otter_core::task::{RunState, TaskAction, TaskStatus};
    use otter_protocol::host::{CodingSettings, SettingsUpdate};
    let worker = std::env::var("OTTER_CLAUDE_WORKER").expect("OTTER_CLAUDE_WORKER");
    let node = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v node"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let host = TestHost::with_env(&[
        ("OTTER_CODING_BACKEND", "sdk".into()),
        ("OTTER_CLAUDE_WORKER", worker),
        ("OTTER_NODE", node),
        ("OTTER_CONTROLLER", "rules".into()),
        ("OTTER_CONTROLLER_TICK_MS", "200".into()),
        ("DEVELOPER_DIR", String::new()),
        ("SDKROOT", String::new()),
    ])
    .await;
    let mut conn = host.conn().await;
    conn.settings_set(SettingsUpdate {
        coding: Some(CodingSettings {
            backend: None,
            model: Some("haiku".into()),
        }),
        ..Default::default()
    })
    .await
    .unwrap();
    let ws = "livectl";
    let id = started_in(
        &host,
        ws,
        "Work only in the current working directory. Use the Bash tool to run exactly `sleep 30 && echo slept > slept.txt`, then create ./done.txt containing the word ok. Nothing else.",
    )
    .await;

    // 1. Pause mid-tool: the turn is interrupted, the run ends, the
    //    conversation is held.
    let c = live_sleeping(&host, &id, 0).await;
    let tools_before = c.tools.len();
    conn.task_act("live-pause", &id, TaskAction::Pause)
        .await
        .unwrap();
    let f = wait_task(&host, &id, "paused", |f| f.live_run().is_none()).await;
    assert_eq!(f.runs[0].state, RunState::Cancelled);
    let session = f.runs[0].provider_session_id.clone().expect("bound");
    let c = conn
        .conversation_get(c.id.as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(c.lifecycle, Lifecycle::Paused);
    assert_eq!(c.turns[0].outcome, Some(TurnOutcome::Interrupted));
    eprintln!("live: paused mid-tool; turn 1 interrupted");

    // 2. Resume: a new run of the same native session.
    conn.task_act("live-resume", &id, TaskAction::Resume)
        .await
        .unwrap();
    let c2 = live_sleeping(&host, &id, tools_before).await;
    let f = conn.task_get(&id).await.unwrap();
    assert_eq!(f.runs.len(), 2);
    assert_eq!(
        f.runs[1].provider_session_id.as_deref(),
        Some(session.as_str())
    );
    assert_eq!(c2.generation, 2);
    eprintln!("live: resumed as generation 2 of session {session}; sleeping again");

    // 3. Redirect mid-tool: the turn stops, the redirect goes next, same run.
    conn.task_act(
        "live-redirect",
        &id,
        TaskAction::Redirect {
            text: "Stop waiting. Skip the sleep; instead create ./redirected.txt containing yes, then create ./done.txt containing ok, and finish.".into(),
        },
    )
    .await
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let f = loop {
        let f = conn.task_get(&id).await.unwrap();
        if matches!(
            f.status,
            TaskStatus::Review | TaskStatus::Failed | TaskStatus::Blocked
        ) {
            break f;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: {:?}",
            f.status
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert_eq!(f.status, TaskStatus::Review, "{:?}", f.status_reason);
    assert_eq!(f.runs.len(), 2, "the redirect ran in the same run");
    assert_eq!(f.runs[1].state, RunState::Completed);
    assert!(host.home().join(ws).join("redirected.txt").exists());
    let c = conn
        .conversation_get(c2.id.as_str())
        .await
        .unwrap()
        .conversation;
    let redirect = c
        .turns
        .iter()
        .find(|t| t.redirect)
        .expect("a redirect turn");
    assert_eq!(redirect.outcome, Some(TurnOutcome::Completed));
    assert_eq!(redirect.generation, Some(2));
    eprintln!(
        "live: done; turns {:?}",
        c.turns
            .iter()
            .map(|t| (t.generation, t.outcome, t.redirect))
            .collect::<Vec<_>>()
    );
}

/// The live checks' conversation, once a `sleep` tool (after the first
/// `after` tools) is running.
async fn live_sleeping(
    host: &TestHost,
    id: &str,
    after: usize,
) -> otter_core::conversation::Conversation {
    use otter_core::conversation::ToolStatus;
    let mut conn = host.conn().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        let list = conn.conversation_list(Some(id)).await.unwrap();
        if let Some(c) = list.first()
            && c.conversation
                .tools
                .iter()
                .skip(after)
                .any(|t| t.status == ToolStatus::Running && t.summary.contains("sleep"))
        {
            return c.conversation.clone();
        }
        if std::time::Instant::now() > deadline {
            let f = conn.task_get(id).await.unwrap();
            let c = list.first().map(|c| &c.conversation);
            panic!(
                "no sleep running; task {:?} {:?}; runs {:?}; turns {:?}; tools {:?}",
                f.status,
                f.status_reason,
                f.runs
                    .iter()
                    .map(|r| (r.state, r.summary.clone()))
                    .collect::<Vec<_>>(),
                c.map(|c| c
                    .turns
                    .iter()
                    .map(|t| (
                        t.generation,
                        t.state,
                        t.outcome,
                        t.text().chars().take(60).collect::<String>()
                    ))
                    .collect::<Vec<_>>()),
                c.map(|c| c
                    .tools
                    .iter()
                    .map(|t| (t.name.clone(), t.status, t.summary.clone()))
                    .collect::<Vec<_>>()),
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---------------------------------------------------------------------------
// Triggers (D-083): unattended work under a policy set in advance.
// ---------------------------------------------------------------------------

/// A directory workspace with a passing check, for triggers' tasks.
async fn plain_workspace(host: &TestHost, name: &str) {
    let dir = host.home().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    host.conn()
        .await
        .workspace_create(WorkspaceCreate {
            name: name.into(),
            source: SourceSpec::Directory {
                path: dir.to_string_lossy().into_owned(),
            },
            sessions: Some(vec![]),
            ..Default::default()
        })
        .await
        .unwrap();
}

fn trigger(
    name: &str,
    workspace: &str,
    request: &str,
    policy: otter_core::trigger::Policy,
) -> otter_protocol::TriggerCreate {
    otter_protocol::TriggerCreate {
        name: name.into(),
        workspace: workspace.into(),
        title: None,
        request: request.into(),
        schedule: otter_core::trigger::Schedule::Manual,
        policy,
    }
}

async fn triggered(
    host: &TestHost,
    trigger: &otter_core::TriggerId,
) -> Vec<otter_core::task::Task> {
    host.conn()
        .await
        .task_list()
        .await
        .unwrap()
        .into_iter()
        .filter(|f| {
            f.started_by
                .as_ref()
                .is_some_and(|s| &s.trigger_id == trigger)
        })
        .collect()
}

/// One task per occurrence however often it's fired, its caps applied;
/// an occurrence whose last task is still at work is skipped (D-083).
#[tokio::test]
async fn a_trigger_starts_one_task_per_occurrence_and_doesnt_overlap() {
    use otter_core::task::TaskStatus;
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    plain_workspace(host, "tidy").await;
    let mut conn = host.conn().await;
    let policy = otter_core::trigger::Policy {
        max_iterations: Some(2),
        max_minutes: Some(30),
        ..Default::default()
    };
    let t = conn
        .trigger_create(trigger("tidy", "tidy", "Tidy up", policy))
        .await
        .unwrap();
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-14T02:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    conn.trigger_run("tidy", Some(at)).await.unwrap();
    let again = conn.trigger_run(t.id.as_str(), Some(at)).await.unwrap();
    assert_eq!(again.history.len(), 1, "{:?}", again.history);
    let fs = triggered(host, &t.id).await;
    assert_eq!(fs.len(), 1, "one task for one occurrence");
    let f = &fs[0];
    let started = f.started_by.as_ref().unwrap();
    assert_eq!((started.trigger.as_str(), started.occurrence), ("tidy", at));
    assert_eq!((f.budget.max_iterations, f.budget.max_minutes), (2, 30));
    assert_eq!(f.request, "Tidy up");
    let f = wait_task(host, f.id.as_str(), "worked unattended", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    assert!(f.decisions.is_empty());

    // A long one: the next occurrence, while it works, is skipped.
    let long = conn
        .trigger_create(trigger(
            "long",
            "tidy",
            "A long job HANG",
            Default::default(),
        ))
        .await
        .unwrap();
    conn.trigger_run("long", None).await.unwrap();
    let first = triggered(host, &long.id).await.remove(0);
    wait_task(host, first.id.as_str(), "working", |f| {
        f.live_run().is_some()
    })
    .await;
    let t = conn.trigger_run("long", None).await.unwrap();
    let last = t.history.last().unwrap();
    assert!(last.task_id.is_none(), "{:#?}", t.history);
    assert!(
        last.skipped
            .as_deref()
            .unwrap()
            .contains("is still at work"),
        "{last:?}"
    );
    assert_eq!(triggered(host, &long.id).await.len(), 1);
    let listed = conn.trigger_list().await.unwrap();
    assert_eq!(listed.len(), 2);
    conn.trigger_delete("long").await.unwrap();
    assert_eq!(conn.trigger_list().await.unwrap().len(), 1);
}

/// A Git workspace whose origin runs CI, for a trigger's task to publish.
async fn git_workspace(fake: &FakeCodex, name: &str) {
    let src = fake.host.home().join(format!("origin-{name}"));
    std::fs::create_dir_all(src.join(".github/workflows")).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    std::fs::write(src.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "init"]);
    fake.host
        .conn()
        .await
        .workspace_create(git_ws(name, &src, Some(&format!("feature/{name}")), vec![]))
        .await
        .unwrap();
    eventually("workspace ready", || async {
        let ws = fake.host.conn().await.workspace_get(name).await.unwrap();
        (ws.state == WorkspaceState::Ready).then_some(())
    })
    .await;
}

/// Publishing allowed in advance: delivered without asking (D-083).
#[tokio::test]
async fn an_unattended_task_publishes_when_its_trigger_allows_it() {
    let fake = fake_agent_host("ok", "rules").await;
    git_workspace(&fake, "nightly").await;
    let mut conn = fake.host.conn().await;
    let policy = otter_core::trigger::Policy {
        publish: true,
        ..Default::default()
    };
    let t = conn
        .trigger_create(trigger(
            "nightly",
            "nightly",
            "Export the timeline as CSV WRITE:timeline.csv",
            policy,
        ))
        .await
        .unwrap();
    conn.trigger_run("nightly", None).await.unwrap();
    let id = triggered(&fake.host, &t.id).await.remove(0).id;
    let f = wait_task(&fake.host, id.as_str(), "published", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    assert!(
        !f.decisions.iter().any(|d| d.summary.contains("git push")),
        "nobody was asked: {:?}",
        f.decisions
    );
}

/// Not allowed in advance: the publish waits for the developer; past the
/// trigger's wait limit the task pauses — nothing decided for them — and
/// resuming asks again (D-083).
#[tokio::test]
async fn an_unattended_task_pauses_when_it_waits_too_long() {
    use otter_core::task::{DecisionStatus, NeedKind, TaskAction, TaskStatus};
    let fake = fake_agent_host("ok", "rules").await;
    git_workspace(&fake, "weekly").await;
    let mut conn = fake.host.conn().await;
    let policy = otter_core::trigger::Policy {
        wait_limit_secs: Some(1),
        ..Default::default()
    };
    let t = conn
        .trigger_create(trigger(
            "weekly",
            "weekly",
            "Export the timeline as CSV WRITE:timeline.csv",
            policy,
        ))
        .await
        .unwrap();
    conn.trigger_run("weekly", None).await.unwrap();
    let id = triggered(&fake.host, &t.id).await.remove(0).id;
    let f = wait_task(&fake.host, id.as_str(), "paused, having waited", |f| {
        f.status == TaskStatus::Paused
    })
    .await;
    let reason = f.status_reason.clone().unwrap();
    assert!(
        reason.starts_with("Waited ") && reason.contains("git push"),
        "{reason}"
    );
    assert_eq!(
        f.needs().map(|n| n.kind),
        Some(NeedKind::Blocked),
        "it waits on you"
    );
    assert!(
        f.decisions
            .iter()
            .all(|d| d.status != DecisionStatus::Approved && d.status != DecisionStatus::Denied),
        "nothing decided for the developer: {:?}",
        f.decisions
    );
    conn.task_act("resume-weekly", id.as_str(), TaskAction::Resume)
        .await
        .unwrap();
    wait_task(&fake.host, id.as_str(), "asked again", |f| {
        f.pending_decisions()
            .any(|d| d.summary.contains("git push"))
    })
    .await;
}

/// Occurrences missed while otterd was down fire once — the latest — when
/// it's back (D-083).
#[tokio::test]
async fn missed_occurrences_fire_once_after_a_restart() {
    let mut fake = fake_agent_host_with("ok", "rules", |_| {
        vec![("OTTER_TRIGGER_TICK_MS", "200".into())]
    })
    .await;
    plain_workspace(&fake.host, "hourly").await;
    let ws = fake
        .host
        .conn()
        .await
        .workspace_get("hourly")
        .await
        .unwrap();
    let now = chrono::Utc::now();
    let created = now - chrono::Duration::minutes(3) - chrono::Duration::seconds(30);
    let t = serde_json::json!([{
        "id": "trg_missed",
        "name": "every-minute",
        "workspace_id": ws.id,
        "title": "Tidy",
        "request": "Tidy up",
        "schedule": {"kind": "every", "minutes": 1},
        "created_at": created,
        "next_at": created + chrono::Duration::minutes(1),
        "history": []
    }]);
    // Written as a daemon that was down would have left it, read on start.
    std::fs::write(fake.host.home().join("state/triggers.json"), t.to_string()).unwrap();
    fake.host.restart().await;
    let fired = eventually("the missed one fires", || async {
        let ts = fake.host.conn().await.trigger_list().await.unwrap();
        (!ts[0].history.is_empty()).then_some(ts)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let ts = fake.host.conn().await.trigger_list().await.unwrap();
    assert_eq!(
        ts[0].history.len(),
        1,
        "once, not three times: {:?}",
        ts[0].history
    );
    let occurrence = fired[0].history[0].occurrence;
    assert_eq!(
        occurrence,
        created + chrono::Duration::minutes(3),
        "the latest"
    );
    assert!(ts[0].next_at.unwrap() > now);
}

/// Writes a v2 host's state — before the rename of tasks to tasks
/// (D-107) — to `OTTER_CAPTURE_DIR`, for `tests/fixtures/v2-home`. Kept,
/// ignored, so the next on-disk change can capture its own.
#[tokio::test]
#[ignore]
async fn capture_v2_state() {
    use otter_core::task::{TaskAction, TaskStatus};
    let out =
        std::path::PathBuf::from(std::env::var("OTTER_CAPTURE_DIR").expect("OTTER_CAPTURE_DIR"));
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let src = host.home().join("origin");
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "init"]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws(
        "app",
        &src,
        Some("feature/a"),
        vec![spec(SessionKind::Command, "check", "echo checked")],
    ))
    .await
    .unwrap();
    eventually("workspace ready", || async {
        let ws = host.conn().await.workspace_get("app").await.unwrap();
        (ws.state == WorkspaceState::Ready).then_some(())
    })
    .await;
    let new = |title: &str, request: &str| otter_protocol::task::TaskCreate {
        command_id: format!("create-{title}"),
        title: title.into(),
        request: request.into(),
        workspace: Some("app".into()),
        answer_only: false,
    };
    // One through review, with its plan, run and evidence.
    let first = conn
        .task_create(new("First", "Add a README"))
        .await
        .unwrap()
        .id
        .to_string();
    conn.task_act("start-first", &first, TaskAction::Start)
        .await
        .unwrap();
    wait_task(host, &first, "in review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    // One given a workspace of its own (`task_id` on the workspace).
    let own = conn
        .task_create(new("Own", "Add a LICENSE"))
        .await
        .unwrap()
        .id
        .to_string();
    conn.task_act("own", &own, TaskAction::OwnWorkspace)
        .await
        .unwrap();
    let _ = conn.otter_send("hello-otter", "What needs me?").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let state = host.home().join("state");
    let copy = |from: &Path, to: &Path| {
        let ok = Command::new("cp")
            .arg("-R")
            .arg(from)
            .arg(to)
            .status()
            .unwrap();
        assert!(ok.success());
    };
    std::fs::create_dir_all(&out).unwrap();
    copy(&state, &out.join("state"));
    let _ = std::fs::remove_dir_all(out.join("state/metrics"));
}

/// A host's state from before D-107, when tasks were features: moved once,
/// at start, and read as tasks — their plans' steps, history, the workspace
/// made for one, a command session, old events and a conversation.
#[tokio::test]
async fn a_hosts_features_become_its_tasks() {
    use otter_core::task::{TaskEvent, TaskStatus};
    use otter_protocol::Event;
    let host = TestHost::new();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-home/state");
    let ok = Command::new("cp")
        .arg("-R")
        .arg(&fixture)
        .arg(host.home().join("state"))
        .status()
        .unwrap();
    assert!(ok.success());
    let mut conn = host.conn().await;

    let state = host.home().join("state");
    assert!(!state.join("features").exists(), "moved");
    let dirs: Vec<_> = std::fs::read_dir(state.join("tasks"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 2);
    for d in &dirs {
        assert!(
            d.join("task.json").exists() && !d.join("feature.json").exists(),
            "{d:?}"
        );
    }

    let tasks = conn.task_list().await.unwrap();
    assert_eq!(tasks.len(), 2);
    let first = tasks.iter().find(|t| t.title == "First").unwrap();
    assert_eq!(first.status, TaskStatus::Review);
    assert!(!first.steps.is_empty());
    assert!(!first.runs.is_empty());
    assert!(first.runs.iter().all(|r| first.step(&r.step_id).is_some()));
    let history = conn.task_events(first.id.as_str(), None).await.unwrap();
    assert!(
        history
            .iter()
            .any(|r| matches!(r.event, TaskEvent::TaskCreated { .. }))
    );
    assert!(
        history
            .iter()
            .any(|r| matches!(r.event, TaskEvent::StepChanged { .. }))
    );
    assert!(
        history
            .iter()
            .any(|r| matches!(r.event, TaskEvent::RunStarted { .. }))
    );

    let own = tasks.iter().find(|t| t.title == "Own").unwrap();
    let made = conn.workspace_list().await.unwrap();
    assert!(
        made.iter().any(|w| w.task_id.as_ref() == Some(&own.id)),
        "{made:#?}"
    );
    let app = conn.workspace_get("app").await.unwrap();
    assert_eq!(app.session("check").unwrap().kind, SessionKind::Command);

    let events = conn.events_list(Some(1000)).await.unwrap();
    assert!(
        events
            .iter()
            .any(|r| matches!(r.event, Event::TaskChanged { .. }))
    );
    assert!(!events.iter().any(|r| matches!(r.event, Event::Unknown)));

    let convs = conn
        .conversation_list(Some(first.id.as_str()))
        .await
        .unwrap();
    assert_eq!(convs.len(), 1, "{convs:#?}");
    let c = &convs[0].conversation;
    assert_eq!(c.task_id.as_ref(), Some(&first.id));
    assert!(
        c.step_id.as_ref().is_some_and(|s| first.step(s).is_some()),
        "{c:?}"
    );
    // Otter's conversation on the host, which fails soft: it read.
    let chat = conn.otter_get().await.unwrap();
    assert!(
        chat.messages.iter().any(|m| m.text == "What needs me?"),
        "{chat:#?}"
    );

    // Started again: nothing more to move, the same tasks.
    conn.shutdown().await.unwrap();
    let mut conn = host.conn().await;
    assert_eq!(conn.task_list().await.unwrap().len(), 2);
    assert!(!state.join("features").exists());
}

/// Deleting a workspace removes what it had (D-108): its worktree and its
/// branch — never commits that are nowhere else, unless forced — and the
/// repository's copy once no workspace uses it, unless a branch in it still
/// holds unpushed work.
#[tokio::test]
async fn deleting_a_workspace_cleans_up_its_branch_and_repository() {
    let th = TestHost::new();
    let host = &th;
    let repo = make_repo(host, "origin", &[("a.txt", "a\n")]);
    let mut conn = host.conn().await;
    let ready = |name: &'static str| async move {
        eventually("workspace ready", || async {
            let ws = host.conn().await.workspace_get(name).await.unwrap();
            (ws.state == WorkspaceState::Ready).then_some(ws)
        })
        .await
    };
    for name in ["one", "two"] {
        conn.workspace_create(git_ws(name, &repo, None, vec![]))
            .await
            .unwrap();
    }
    let (one, two) = (ready("one").await, ready("two").await);
    let repos = host.home().join("repos");
    let copy = std::fs::read_dir(&repos)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let base = copy.join("base");
    let branches = || git(&base, &["branch", "--list", "--format=%(refname:short)"]);
    let commit = |dir: &Path, file: &str| {
        std::fs::write(dir.join(file), "x\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-qm", file]);
    };

    // A commit on one's branch, pushed nowhere: not without force.
    commit(Path::new(&one.root), "b.txt");
    let err = conn.workspace_delete("one", false).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("1 commit not pushed anywhere"),
        "{err:#}"
    );
    conn.workspace_delete("one", true).await.unwrap();
    assert!(!branches().contains("otterd/one"), "{}", branches());
    assert!(!Path::new(&one.root).exists());
    assert!(base.exists(), "two still uses it");

    // Unpushed work on a branch of its own keeps the copy, with the branch.
    let root = Path::new(&two.root);
    git(root, &["checkout", "-qb", "extra"]);
    commit(root, "c.txt");
    git(root, &["checkout", "-q", "otterd/two"]);
    conn.workspace_delete("two", false).await.unwrap();
    let left = branches();
    assert!(
        left.contains("extra") && !left.contains("otterd/two"),
        "{left}"
    );
    assert!(base.exists(), "extra holds unpushed work");

    // Without it, the copy goes with the last workspace.
    git(&base, &["branch", "-D", "extra"]);
    conn.workspace_create(git_ws("three", &repo, None, vec![]))
        .await
        .unwrap();
    ready("three").await;
    conn.workspace_delete("three", false).await.unwrap();
    assert!(
        !copy.exists(),
        "{:?}",
        std::fs::read_dir(&repos).unwrap().count()
    );
}

fn commit_file(dir: &Path, file: &str) {
    std::fs::write(dir.join(file), "x\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", file]);
}

async fn ready_git_ws(host: &TestHost, name: &str) -> (Workspace, std::path::PathBuf) {
    let ws = wait_prepared(host, name).await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    let base = host
        .home()
        .join("repos")
        .join(&git_source(&ws).repo_id)
        .join("base");
    (ws, base)
}

fn local_branches(base: &Path) -> String {
    git(base, &["branch", "--list", "--format=%(refname:short)"])
}

/// The repository's copy holds its workspaces' branches, not copies of the
/// remote's (D-132): a branch checked out by name is the remote's as it is
/// now, and one deleted on the remote doesn't keep the copy.
#[tokio::test]
async fn a_repositorys_copy_holds_only_its_workspaces_branches() {
    let host = TestHost::new();
    let origin = make_repo(&host, "origin", &[("a.txt", "a\n")]);
    git(&origin, &["checkout", "-qb", "gone"]);
    commit_file(&origin, "gone.txt");
    git(&origin, &["checkout", "-q", "main"]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("one", &origin, None, vec![]))
        .await
        .unwrap();
    let (_, base) = ready_git_ws(&host, "one").await;
    assert_eq!(local_branches(&base).trim(), "otterd/one");

    // main moves on; a `main` left behind here catches up with it.
    let old = git(&origin, &["rev-parse", "HEAD"]);
    commit_file(&origin, "b.txt");
    git(&base, &["branch", "main", old.trim()]);
    conn.workspace_create(git_ws("two", &origin, Some("main"), vec![]))
        .await
        .unwrap();
    let (two, _) = ready_git_ws(&host, "two").await;
    let root = Path::new(&two.root);
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]),
        git(&origin, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(root, &["rev-parse", "--abbrev-ref", "@{upstream}"]).trim(),
        "origin/main"
    );

    // Deleted on the remote, with commits nowhere else: not the copy's.
    git(&origin, &["branch", "-qD", "gone"]);
    git(&base, &["fetch", "-q", "--prune", "origin"]);
    conn.workspace_delete("one", false).await.unwrap();
    conn.workspace_delete("two", false).await.unwrap();
    assert!(!base.exists());
}

/// A copy cloned before D-132 holds each branch the remote had that day.
/// Those go — when a workspace is prepared on it, or the last one deleted —
/// but never a branch someone committed on.
#[tokio::test]
async fn an_older_repository_copy_drops_the_branches_it_cloned() {
    let host = TestHost::new();
    let origin = make_repo(&host, "origin", &[("a.txt", "a\n")]);
    let gone = |name: &str| {
        git(&origin, &["checkout", "-qb", name]);
        commit_file(&origin, name);
        git(&origin, &["checkout", "-q", "main"]);
    };
    gone("gone");
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("one", &origin, None, vec![]))
        .await
        .unwrap();
    let (one, base) = ready_git_ws(&host, "one").await;
    // Work on a branch of its own: committed on here, so it has a reflog.
    let root = Path::new(&one.root);
    git(root, &["checkout", "-qb", "kept"]);
    commit_file(root, "kept.txt");
    git(root, &["checkout", "-q", "otterd/one"]);
    // What `git clone --bare` left: the remote's branches, packed.
    let as_cloned = |branches: &[&str]| {
        for b in branches {
            git(&base, &["branch", b, &format!("origin/{b}")]);
        }
        git(&base, &["pack-refs", "--all"]);
        git(&base, &["config", "--unset", "otter.ownBranchesOnly"]);
    };
    as_cloned(&["gone", "main"]);
    git(&origin, &["branch", "-qD", "gone"]);
    commit_file(&origin, "b.txt");

    // Without reflogs a branch with work looks cloned: nothing goes. The
    // cloned `main`, behind the remote's, is caught up when asked for.
    git(&base, &["config", "core.logAllRefUpdates", "false"]);
    conn.workspace_create(git_ws("two", &origin, Some("main"), vec![]))
        .await
        .unwrap();
    let (two, _) = ready_git_ws(&host, "two").await;
    assert_eq!(
        git(Path::new(&two.root), &["rev-parse", "HEAD"]),
        git(&origin, &["rev-parse", "HEAD"])
    );
    let left = local_branches(&base);
    assert!(left.contains("gone") && left.contains("kept"), "{left}");

    // With them, preparing drops what was cloned.
    git(&base, &["config", "--unset", "core.logAllRefUpdates"]);
    conn.workspace_create(git_ws("three", &origin, None, vec![]))
        .await
        .unwrap();
    ready_git_ws(&host, "three").await;
    let left = local_branches(&base);
    assert!(!left.contains("gone") && left.contains("kept"), "{left}");

    // The last one deleted: cloned branches go first, and what holds work
    // stays, with the copy.
    gone("gone2");
    git(&base, &["fetch", "-q", "origin"]);
    as_cloned(&["gone2"]);
    git(&origin, &["branch", "-qD", "gone2"]);
    git(&base, &["fetch", "-q", "--prune", "origin"]);
    for name in ["one", "two", "three"] {
        conn.workspace_delete(name, false).await.unwrap();
    }
    assert_eq!(local_branches(&base).trim(), "kept");
}

/// A branch merged by squash or rebase, then deleted on the remote, has its
/// work there under other commits: deleting its workspace needs no force
/// (D-132). Work added since still does, and so do commits that undo each
/// other, which an empty commit upstream must not pass for.
#[tokio::test]
async fn a_branch_squashed_or_rebased_in_is_not_unpushed_work() {
    let host = TestHost::new();
    let origin = make_repo(&host, "origin", &[("a.txt", "a\n")]);
    let mut conn = host.conn().await;
    for name in ["squashed", "rebased", "reverted"] {
        conn.workspace_create(git_ws(name, &origin, None, vec![]))
            .await
            .unwrap();
    }
    let (squashed, base) = ready_git_ws(&host, "squashed").await;
    let (rebased, _) = ready_git_ws(&host, "rebased").await;
    let (reverted, _) = ready_git_ws(&host, "reverted").await;
    let (s, r) = (Path::new(&squashed.root), Path::new(&rebased.root));
    for (root, name) in [(s, "squashed"), (r, "rebased")] {
        commit_file(root, &format!("{name}-1.txt"));
        commit_file(root, &format!("{name}-2.txt"));
        git(root, &["push", "-q", "origin", &format!("otterd/{name}")]);
    }
    let v = Path::new(&reverted.root);
    commit_file(v, "tried.txt");
    git(v, &["revert", "--no-edit", "HEAD"]);

    // main moves on; the pull requests merge; their branches go.
    commit_file(&origin, "m.txt");
    git(&origin, &["merge", "-q", "--squash", "otterd/squashed"]);
    git(&origin, &["commit", "-qm", "squashed"]);
    git(&origin, &["cherry-pick", "main..otterd/rebased"]);
    git(&origin, &["commit", "-q", "--allow-empty", "-m", "ci"]);
    git(
        &origin,
        &["branch", "-qD", "otterd/squashed", "otterd/rebased"],
    );
    git(&base, &["fetch", "-q", "--prune", "origin"]);

    commit_file(s, "since.txt");
    for name in ["squashed", "reverted"] {
        let err = conn.workspace_delete(name, false).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("not pushed anywhere"),
            "{name}: {err:#}"
        );
    }
    git(s, &["reset", "-q", "--hard", "HEAD~1"]);
    conn.workspace_delete("squashed", false).await.unwrap();
    conn.workspace_delete("rebased", false).await.unwrap();
    conn.workspace_delete("reverted", true).await.unwrap();
    assert!(!base.exists());
}
