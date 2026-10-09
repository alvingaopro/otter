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
use otter_protocol::frame::{AttachExitReason, Frame};
use otter_protocol::{
    Cursor, Event, SessionAttach, SessionCreate, SessionRef, SessionSpec, SourceSpec,
    WorkspaceCreate,
};

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
async fn task_exit_code_is_recorded_and_restart_creates_new_execution() {
    let host = TestHost::new();
    create(
        &host,
        "tasks",
        Some(vec![
            spec(SessionKind::Task, "fail", "echo task-output; exit 3"),
            spec(SessionKind::Task, "ok", "true"),
        ]),
    )
    .await;
    let ws = wait_status(&host, "tasks", "fail", SessionStatus::Failed).await;
    let exec = ws
        .session("fail")
        .unwrap()
        .current_execution()
        .unwrap()
        .clone();
    assert_eq!(exec.state, ExecutionState::Exited);
    assert_eq!(exec.exit_code, Some(3));
    wait_status(&host, "tasks", "ok", SessionStatus::Completed).await;

    // Output of an exited process stays readable.
    wait_output(&host, "tasks", "fail", "task-output").await;

    let restarted = host
        .conn()
        .await
        .session_restart("tasks", "fail")
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
    wait_status(&host, "tasks", "fail", SessionStatus::Failed).await;

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
    assert!(!log.contains("task-output"));
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
    let (_, reader, writer) = host
        .conn()
        .await
        .attach(SessionAttach {
            session: SessionRef {
                workspace: ws.into(),
                session: session.into(),
            },
            cols: 100,
            rows: 30,
            term: Some("xterm-256color".into()),
        })
        .await
        .unwrap();
    (reader, writer)
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
            spec(SessionKind::Task, "finish", "sleep 1; exit 7"),
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
            spec: spec(SessionKind::Task, "t", "exit 0"),
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

async fn task(host: &TestHost, ws: &str, name: &str) {
    host.conn()
        .await
        .session_create(SessionCreate {
            workspace: ws.into(),
            spec: spec(SessionKind::Task, name, "exit 0"),
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

    // Meanwhile: a task runs, and the daemon restarts.
    task(&host, "ev", "t").await;
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
    task(&host, "ev", "u").await;
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
    task(&host, "snap", "t").await;
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
    conn.workspace_create(git_ws("feature-a", &src, None, vec![]))
        .await
        .unwrap();
    conn.workspace_create(git_ws("feature-b", &src, None, vec![]))
        .await
        .unwrap();
    let a = wait_prepared(&host, "feature-a").await;
    let b = wait_prepared(&host, "feature-b").await;
    assert_eq!(a.state, WorkspaceState::Ready, "{:?}", a.state_message);
    assert_eq!(b.state, WorkspaceState::Ready, "{:?}", b.state_message);

    let ga = git_source(&a);
    assert_eq!(ga.branch, "otterd/feature-a");
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

    // Uncommitted work is protected unless forced; the branch survives.
    std::fs::write(Path::new(&a.root).join("wip.txt"), "wip").unwrap();
    let err = conn.workspace_delete("feature-a", false).await.unwrap_err();
    assert!(err.to_string().contains("uncommitted"), "{err}");
    conn.workspace_delete("feature-a", true).await.unwrap();
    assert!(!Path::new(&a.root).exists());
    git(
        &base,
        &["show-ref", "--verify", "refs/heads/otterd/feature-a"],
    );
    conn.workspace_delete("feature-b", false).await.unwrap();
    let worktrees = git(&base, &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains(&b.root), "{worktrees}");
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
    let ws = conn.workspace_prepare("broken").await.unwrap();
    let ws = match ws.state {
        WorkspaceState::Preparing => wait_prepared(&host, "broken").await,
        _ => ws,
    };
    assert_eq!(ws.state, WorkspaceState::Failed);
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
            sessions: Some(vec![spec(SessionKind::Task, "where", "pwd")]),
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
        vec![spec(SessionKind::Task, "show", "echo var=$OTTER_TEST_VAR")],
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
/// `nohooks` ignores the settings (hooks disabled) and stops at a prompt.
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

/// A host with fake agents, and the given Control Agent brain for features
/// (`rules` or `yes`; never a real model in tests).
async fn fake_agent_host(mode: &str, controller: &str) -> FakeCodex {
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
    let host = TestHost::with_env(&[
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
        // Features: the deterministic controller (no model calls in tests).
        ("OTTER_CONTROLLER", controller.into()),
        ("OTTER_CONTROLLER_TICK_MS", "200".into()),
        // A Nix dev shell's SDK paths make macOS's /usr/bin/make look for a
        // make that isn't there (features check their work with `make test`).
        ("DEVELOPER_DIR", String::new()),
        ("SDKROOT", String::new()),
        ("OTTER_AGENT_QUIET_SECS", "2".into()),
    ])
    .await;
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

#[tokio::test]
async fn claude_code_without_hooks_falls_back_to_the_quiet_turn() {
    use otter_core::{AgentState, AttentionKind};
    let fake = fake_codex_host("nohooks").await;
    let host = &fake.host;
    create(host, "nohooks", Some(vec![SessionSpec::agent("claude")])).await;
    let ws = wait_agent(host, "nohooks", "claude", AgentState::Blocked).await;
    assert_eq!(ws.attention[0].kind, AttentionKind::Approval);
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

#[tokio::test]
async fn failures_and_completions_raise_attention() {
    use otter_core::{Activity, AttentionKind};
    let host = TestHost::new();
    create(
        &host,
        "jobs",
        Some(vec![
            spec(SessionKind::Task, "tests", "exit 1"),
            spec(SessionKind::Task, "lint", "true"),
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

    // Restarting the failed task resolves its item.
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
    let host = TestHost::new();
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

#[tokio::test]
async fn browser_login_opens_trusted_pages_through_a_connected_app() {
    let host = TestHost::new();
    sh_session(&host, "login").await;
    let mut conn = host.conn().await;

    // No app connected: the tool hears why (and falls back to printing).
    // A real xdg-open on the host then gets its turn, so its exit status varies.
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

    // Not a trusted provider: refused, with the reason on the host.
    conn.session_write(
        "login",
        "sh",
        "xdg-open https://example.com/; echo bad-done-$((40+2))",
        true,
    )
    .await
    .unwrap();
    let out = wait_output(&host, "login", "sh", "bad-done-42").await;
    assert!(out.contains("not a trusted sign-in provider"), "{out}");
}

#[tokio::test]
async fn archive_stops_sessions_and_unarchive_brings_the_workspace_back() {
    let host = TestHost::new();
    let ws = create(
        &host,
        "shelf",
        Some(vec![
            spec(SessionKind::Service, "server", "sleep 600"),
            spec(SessionKind::Task, "tests", "exit 3"),
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
    let ws = eventually("the failed task asks for attention", || async {
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
            spec: spec(SessionKind::Task, "more", "exit 0"),
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
    // removes the worktree and keeps the branch.
    conn.workspace_archive("parked").await.unwrap();
    let err = conn.workspace_delete("parked", false).await.unwrap_err();
    assert!(err.to_string().contains("uncommitted"), "{err}");
    conn.workspace_delete("parked", true).await.unwrap();
    assert!(!root.exists());
    git(&base, &["show-ref", "--verify", "refs/heads/otterd/parked"]);
}

#[tokio::test]
async fn browser_login_opens_pages_from_attached_terminals_once() {
    use otter_client::login::{Logins, Opener};
    use std::sync::{Arc, Mutex};

    let host = TestHost::new();
    sh_session(&host, "login").await;
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
    let first = Logins::start(host.transport.clone(), "test", opener(1))
        .await
        .unwrap();
    let second = Logins::start(host.transport.clone(), "test", opener(2))
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
}

#[tokio::test]
async fn brief_is_set_at_creation_and_edited_later() {
    let host = TestHost::new();
    let mut conn = host.conn().await;
    let ws = conn
        .workspace_create(WorkspaceCreate {
            name: "why".into(),
            brief: Some(otter_core::Brief {
                goal: Some("  Fix renewal validation \n".into()),
                title: Some(" ".into()),
                ..Default::default()
            }),
            sessions: Some(vec![]),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ws.brief.goal.as_deref(), Some("Fix renewal validation"));
    assert_eq!(ws.brief.title, None, "blank text is absent, not empty");

    let mut stream = host.conn().await.subscribe(None).await.unwrap();
    let mut brief = ws.brief.clone();
    brief.title = Some("GCP renewal".into());
    brief.decisions = vec!["Renewal count includes the initial term.".into(), "".into()];
    let edited = conn.workspace_set_brief("why", brief).await.unwrap();
    assert_eq!(edited.brief.title.as_deref(), Some("GCP renewal"));
    assert_eq!(edited.brief.goal.as_deref(), Some("Fix renewal validation"));
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
                goal: Some("parked".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(ws.state, WorkspaceState::Archived);
    assert_eq!(ws.brief.goal.as_deref(), Some("parked"));
    let err = conn
        .workspace_set_brief("nope", otter_core::Brief::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no workspace"), "{err}");
}

// ---------------------------------------------------------------------------
// Features (D-043)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn features_persist_dedupe_commands_and_replay_after_a_restart() {
    use otter_core::feature::{FeatureAction, FeatureStatus, MessageRole};
    use otter_protocol::feature::FeatureCreate;

    let host = TestHost::new();
    create(&host, "fw", Some(vec![])).await;
    let mut conn = host.conn().await;
    let created = FeatureCreate {
        command_id: "cmd-create-1".into(),
        title: "Export CSV".into(),
        request: "Export the timeline as CSV".into(),
        workspace: Some("fw".into()),
    };
    let f = conn.feature_create(created.clone()).await.unwrap();
    assert_eq!(f.status, FeatureStatus::Draft);
    assert!(f.workspace_id.is_some());
    // The same create again (a retry after a lost reply) is the same feature.
    let again = conn.feature_create(created).await.unwrap();
    assert_eq!(again.id, f.id);
    assert_eq!(conn.feature_list().await.unwrap().len(), 1);

    // A message delivered twice is added once.
    let id = f.id.as_str();
    conn.feature_send("cmd-msg-1", id, "Include session names")
        .await
        .unwrap();
    let f = conn
        .feature_send("cmd-msg-1", id, "Include session names")
        .await
        .unwrap();
    let user: Vec<_> = f
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::User)
        .collect();
    assert_eq!(user.len(), 2, "request + one message");

    // A client following the feature remembers where it was.
    let seen = f.history_seq;
    let snapshot = conn.snapshot().await.unwrap();
    drop(conn);

    // While it's away: the feature starts, and the daemon restarts.
    host.conn()
        .await
        .feature_act("cmd-start", id, FeatureAction::Start)
        .await
        .unwrap();
    host.stop_daemon().await;

    // Everything is still there after the restart...
    let mut conn = host.conn().await;
    let f = conn.feature_get(id).await.unwrap();
    assert_eq!(f.status, FeatureStatus::Planning);
    assert_eq!(f.messages.len(), 2);
    // ...the host's event stream says the feature changed...
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
        Event::FeatureChanged { feature_id, status: FeatureStatus::Planning, .. } if feature_id == &f.id
    )));
    // ...and the feature's own history replays exactly what was missed, in order.
    let replay = conn.feature_events(id, Some(seen)).await.unwrap();
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
        .feature_act("cmd-start", id, FeatureAction::Start)
        .await
        .unwrap();
    assert_eq!(f2.history_seq, f.history_seq);
    let err = conn
        .feature_act(
            "cmd-accept",
            id,
            FeatureAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("review"), "{err:#}");

    // Features are kept apart from workspace state.
    let state = std::fs::read_to_string(host.home().join("state/state.json")).unwrap();
    assert!(!state.contains(id));
}

// ---------------------------------------------------------------------------
// The Control Agent driving managed runs (D-044, D-045), with the fake
// `claude -p` (tests/fake_claude_stream.sh) and the deterministic brains.
// ---------------------------------------------------------------------------

/// A project with a `make test` check, a feature on it asking for
/// `request`, started. Returns the feature id.
async fn started_feature(host: &TestHost, request: &str) -> String {
    use otter_core::feature::FeatureAction;
    let dir = host.home().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    let mut conn = host.conn().await;
    conn.workspace_create(WorkspaceCreate {
        name: "proj".into(),
        source: SourceSpec::Directory {
            path: dir.to_string_lossy().into_owned(),
        },
        sessions: Some(vec![]),
        ..Default::default()
    })
    .await
    .unwrap();
    let f = conn
        .feature_create(otter_protocol::feature::FeatureCreate {
            command_id: "create".into(),
            title: "CSV export".into(),
            request: request.into(),
            workspace: Some("proj".into()),
        })
        .await
        .unwrap();
    conn.feature_act("start", f.id.as_str(), FeatureAction::Start)
        .await
        .unwrap();
    f.id.to_string()
}

async fn wait_feature(
    host: &TestHost,
    id: &str,
    what: &str,
    pred: impl Fn(&otter_core::feature::Feature) -> bool,
) -> otter_core::feature::Feature {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let f = host.conn().await.feature_get(id).await.unwrap();
        if pred(&f) {
            return f;
        }
        if tokio::time::Instant::now() > deadline {
            let history = host.conn().await.feature_events(id, None).await.unwrap();
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
async fn a_feature_goes_from_request_to_review_with_evidence() {
    use otter_core::feature::{EvidenceKind, FeatureAction, FeatureStatus, MessageRole, RunState};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_feature(&fake.host, "Export the timeline as CSV").await;
    let f = wait_feature(&fake.host, &id, "review, with the report", |f| {
        f.status == FeatureStatus::Review && f.report.is_some()
    })
    .await;
    // Not a Git workspace: no pull request or CI to wait for.
    use otter_core::feature::GateStatus;
    let gate = |name: &str| f.gates.iter().find(|g| g.name == name).unwrap().status;
    assert_eq!(gate("Pull request"), GateStatus::NotApplicable);
    assert_eq!(gate("Local tests"), GateStatus::Passed);
    assert!(f.report.as_deref().unwrap().contains("## Unresolved risks"));
    // Planned, ran, reported, checked.
    assert_eq!(f.tasks.len(), 1);
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
        .feature_events(&id, None)
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
        .feature_act(
            "accept",
            &id,
            FeatureAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, FeatureStatus::Done);
}

#[tokio::test]
async fn an_approval_is_answered_and_the_run_continues() {
    use otter_core::feature::{Decider, DecisionStatus, FeatureAction, FeatureStatus, Risk};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_feature(&fake.host, "Add a dependency ASK_INSTALL").await;
    // The rules brain doesn't decide: it's the developer's.
    let f = wait_feature(&fake.host, &id, "blocked on a decision", |f| {
        f.status == FeatureStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    assert_eq!(d.summary, "Run `npm install left-pad`");
    assert_eq!(d.risk, Risk::Medium);
    assert!(!d.user_only);
    fake.host
        .conn()
        .await
        .feature_act(
            "approve",
            &id,
            FeatureAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
            },
        )
        .await
        .unwrap();
    let f = wait_feature(&fake.host, &id, "review after the approval", |f| {
        f.status == FeatureStatus::Review
    })
    .await;
    let d = f.decisions.iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(d.status, DecisionStatus::Approved);
    assert_eq!(d.decided_by, Some(Decider::User));
    assert_eq!(f.runs[0].summary.as_deref(), Some("allowed and done"));
}

#[tokio::test]
async fn a_model_cannot_approve_what_only_the_developer_may() {
    use otter_core::feature::{Decider, DecisionStatus, FeatureAction, FeatureStatus, Risk};
    // A brain that approves everything it is asked.
    let fake = fake_agent_host("ok", "yes").await;
    let id = started_feature(&fake.host, "Clean up ASK_RM").await;
    let f = wait_feature(&fake.host, &id, "blocked on the developer", |f| {
        f.status == FeatureStatus::Blocked
            && f.rationale
                .iter()
                .any(|r| r.contains("only the developer may approve"))
    })
    .await;
    let d = f.pending_decisions().next().expect("still pending").clone();
    assert_eq!(d.summary, "Run `rm -rf build`");
    assert!(d.user_only);
    assert_eq!(d.risk, Risk::High);
    // Some time later, still not approved by anyone but the developer.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let f = fake.host.conn().await.feature_get(&id).await.unwrap();
    assert_eq!(
        f.decisions.iter().find(|x| x.id == d.id).unwrap().status,
        DecisionStatus::Pending
    );
    // The developer says no; nobody can turn that around.
    fake.host
        .conn()
        .await
        .feature_act(
            "deny",
            &id,
            FeatureAction::Decide {
                decision_id: d.id.clone(),
                approve: false,
                answer: None,
            },
        )
        .await
        .unwrap();
    let f = wait_feature(&fake.host, &id, "the denial reaches the agent", |f| {
        f.runs
            .first()
            .is_some_and(|r| r.summary.as_deref() == Some("denied, so I stopped"))
    })
    .await;
    let d = f.decisions.iter().find(|x| x.id == d.id).unwrap();
    assert_eq!(
        (d.status, d.decided_by),
        (DecisionStatus::Denied, Some(Decider::User))
    );
}

#[tokio::test]
async fn the_control_agent_decides_what_policy_leaves_open() {
    use otter_core::feature::{Decider, DecisionStatus, FeatureStatus};
    let fake = fake_agent_host("ok", "yes").await;
    let id = started_feature(&fake.host, "Add a dependency ASK_INSTALL").await;
    let f = wait_feature(&fake.host, &id, "review", |f| {
        f.status == FeatureStatus::Review
    })
    .await;
    let d = &f.decisions[0];
    assert_eq!(
        (d.status, d.decided_by),
        (DecisionStatus::Approved, Some(Decider::Controller))
    );
    assert_eq!(d.rationale.as_deref(), Some("yes"));
}

#[tokio::test]
async fn runs_survive_a_restart_stop_on_cancel_and_hand_over_to_the_developer() {
    use otter_core::feature::{FeatureAction, FeatureStatus, RunState};
    let mut fake = fake_agent_host("ok", "rules").await;
    let id = started_feature(&fake.host, "A long job HANG").await;
    let f = wait_feature(&fake.host, &id, "a run with a conversation", |f| {
        f.runs
            .first()
            .is_some_and(|r| r.provider_session_id.is_some() && r.state == RunState::Running)
    })
    .await;
    let conv = f.runs[0].provider_session_id.clone().unwrap();

    // The daemon restarts: the run ends with it, and its conversation resumes.
    fake.host.restart().await;
    let f = wait_feature(&fake.host, &id, "the run resumed", |f| {
        f.runs.len() == 2 && f.runs[1].state == RunState::Running
    })
    .await;
    assert_eq!(f.runs[0].state, RunState::Cancelled);
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

    // The developer takes over: the managed run stops, the conversation
    // opens in an interactive session.
    let mut conn = fake.host.conn().await;
    let f = conn
        .feature_act("take", &id, FeatureAction::TakeOver)
        .await
        .unwrap();
    assert_eq!(f.status, FeatureStatus::Paused);
    assert_eq!(f.runs[1].state, RunState::HandedOff);
    let sid = f.tasks[0].session_id.clone().expect("a takeover session");
    let ws = conn.workspace_get("proj").await.unwrap();
    let s = ws.session(sid.as_str()).unwrap();
    assert_eq!(
        s.agent.as_ref().unwrap().provider_session_id.as_deref(),
        Some(conv.as_str())
    );
    eventually("the interactive agent resumes the conversation", || async {
        std::fs::read_to_string(&fake.log)
            .unwrap()
            .contains(&format!("claude --resume {conv}"))
            .then_some(())
    })
    .await;
    // Retrying the takeover is harmless.
    let again = conn
        .feature_act("take", &id, FeatureAction::TakeOver)
        .await
        .unwrap();
    let extra = conn.feature_events(&id, Some(f.history_seq)).await.unwrap();
    assert!(
        extra.is_empty(),
        "the retry changed nothing: {:?}",
        extra.iter().map(|r| &r.text).collect::<Vec<_>>()
    );
    assert_eq!(again.history_seq, f.history_seq);

    // Handing back while the developer's agent still runs is refused...
    let err = conn
        .feature_act("back-1", &id, FeatureAction::HandBack)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("quit the agent"), "{err:#}");
    // ...and works once it has stopped: the managed run picks it up again.
    conn.session_stop("proj", sid.as_str()).await.unwrap();
    conn.feature_act("back-2", &id, FeatureAction::HandBack)
        .await
        .unwrap();
    let f = wait_feature(&fake.host, &id, "managed again", |f| {
        f.runs.len() == 3 && f.runs[2].state == RunState::Running
    })
    .await;
    assert_eq!(
        f.runs[2].provider_session_id.as_deref(),
        Some(conv.as_str())
    );

    // Cancel stops it for good.
    let f = conn
        .feature_act("cancel", &id, FeatureAction::Cancel)
        .await
        .unwrap();
    assert_eq!(f.status, FeatureStatus::Cancelled);
    assert_eq!(f.runs[2].state, RunState::Cancelled);
}

#[tokio::test]
async fn repeated_failures_stop_at_the_limit_and_the_plan_survives() {
    use otter_core::feature::{FeatureAction, FeatureEvent, FeatureStatus, RunState};
    let fake = fake_agent_host("ok", "rules").await;
    let id = started_feature(&fake.host, "This will FAIL").await;
    let f = wait_feature(&fake.host, &id, "failed", |f| {
        f.status == FeatureStatus::Failed
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
    let history = fake
        .host
        .conn()
        .await
        .feature_events(&id, None)
        .await
        .unwrap();
    assert!(
        history
            .iter()
            .any(|r| matches!(&r.event, FeatureEvent::LimitReached { limit } if limit == "loop"))
    );
    // The plan is kept: retrying starts from it with a fresh budget.
    let tasks = f.tasks.clone();
    let f = fake
        .host
        .conn()
        .await
        .feature_act("retry", &id, FeatureAction::Retry)
        .await
        .unwrap();
    assert_eq!(f.status, FeatureStatus::Implementing);
    assert_eq!(
        f.tasks.iter().map(|t| &t.id).collect::<Vec<_>>(),
        tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    assert_eq!(f.budget.iterations_used, 0);
    // The earlier failures don't count against the retry: three new ones do.
    let f = wait_feature(&fake.host, &id, "failed again", |f| {
        f.status == FeatureStatus::Failed && f.runs.len() == 6
    })
    .await;
    assert_eq!(f.budget.iterations_used, 3);
}

// ---------------------------------------------------------------------------
// Browser verification and previews (D-046). Skipped without a Chrome or
// Chromium and python3 on this machine.
// ---------------------------------------------------------------------------

fn browser_available() -> bool {
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
    if !(ok && python) {
        eprintln!("skipping: needs Chrome/Chromium and python3");
    }
    ok && python
}

const PREVIEW: &str = "python3 -m http.server {port} --bind 127.0.0.1";

/// A web project: a greeter page, `make test`, and a browser check that
/// expects `expect` after greeting Ada.
async fn web_feature(host: &TestHost, expect: &str) -> String {
    use otter_core::feature::FeatureAction;
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
        .feature_create(otter_protocol::feature::FeatureCreate {
            command_id: "create".into(),
            title: "Greeter".into(),
            request: "Greet people by name".into(),
            workspace: Some("web".into()),
        })
        .await
        .unwrap();
    conn.feature_act("start", f.id.as_str(), FeatureAction::Start)
        .await
        .unwrap();
    f.id.to_string()
}

/// Approve the pending decision about running the preview command.
async fn allow_preview(host: &TestHost, id: &str) {
    use otter_core::feature::{FeatureAction, Risk};
    let f = wait_feature(host, id, "asked to allow the preview", |f| {
        f.pending_decisions().any(|d| d.summary.contains(PREVIEW))
    })
    .await;
    let d = f
        .pending_decisions()
        .find(|d| d.summary.contains(PREVIEW))
        .unwrap();
    assert_eq!(
        d.risk,
        Risk::Medium,
        "an unknown command: the developer or the Control Agent decides"
    );
    host.conn()
        .await
        .feature_act(
            &format!("allow-{}", d.id),
            id,
            FeatureAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_browser_check_verifies_the_ui_with_evidence() {
    use otter_core::feature::{EvidenceKind, FeatureStatus};
    if !browser_available() {
        return;
    }
    let fake = fake_agent_host("ok", "rules").await;
    let id = web_feature(&fake.host, "Hello, Ada").await;
    allow_preview(&fake.host, &id).await;
    let f = wait_feature(&fake.host, &id, "review", |f| {
        f.status == FeatureStatus::Review
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
    // The screenshot is kept with the feature and served by name.
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
        .feature_artifact(&id, &shot)
        .await
        .unwrap();
    assert!(file.size > 100);
    assert!(file.data.starts_with("iVBORw0KGgo"), "a PNG");
    // Names only: no wandering outside the artifacts.
    assert!(
        fake.host
            .conn()
            .await
            .feature_artifact(&id, "../feature.json")
            .await
            .is_err()
    );
    // Approved once, remembered.
    assert!(f.allowed_commands.iter().any(|c| c == PREVIEW));
}

#[tokio::test]
async fn a_broken_ui_fails_verification() {
    use otter_core::feature::{EvidenceKind, FeatureStatus};
    if !browser_available() {
        return;
    }
    let fake = fake_agent_host("ok", "rules").await;
    // The page says "Hello, Ada"; the check wants something else.
    let id = web_feature(&fake.host, "Goodbye, Ada").await;
    allow_preview(&fake.host, &id).await;
    let f = wait_feature(
        &fake.host,
        &id,
        "verification failed and work resumed",
        |f| {
            f.evidence
                .iter()
                .any(|e| e.kind == EvidenceKind::Browser && e.ok == Some(false))
                && f.tasks
                    .iter()
                    .any(|t| t.title == "Fix what verification found")
        },
    )
    .await;
    assert_ne!(f.status, FeatureStatus::Review);
    assert_ne!(f.status, FeatureStatus::Done);
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
    use otter_core::feature::{FeatureAction, FeatureStatus};
    if !browser_available() {
        return;
    }
    let fake = fake_agent_host("ok", "rules").await;
    let id = web_feature(&fake.host, "Hello, Ada").await;
    let mut conn = fake.host.conn().await;
    // Not allowed yet: asking for it creates the decision.
    wait_feature(&fake.host, &id, "verification asks", |f| {
        f.pending_decisions().next().is_some()
    })
    .await;
    let err = conn
        .feature_act("pv-1", &id, FeatureAction::Preview)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("allow"), "{err:#}");
    allow_preview(&fake.host, &id).await;
    wait_feature(&fake.host, &id, "review", |f| {
        f.status == FeatureStatus::Review
    })
    .await;

    let f = conn
        .feature_act("pv-2", &id, FeatureAction::Preview)
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
        .feature_act("pv-3", &id, FeatureAction::Preview)
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
/// repository with CI configured, and a feature on it, started.
async fn git_feature(fake: &FakeCodex) -> (String, std::path::PathBuf) {
    use otter_core::feature::FeatureAction;
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
        .feature_create(otter_protocol::feature::FeatureCreate {
            command_id: "create".into(),
            title: "CSV export".into(),
            request: "Export the timeline as CSV".into(),
            workspace: Some("csv".into()),
        })
        .await
        .unwrap();
    conn.feature_act("start", f.id.as_str(), FeatureAction::Start)
        .await
        .unwrap();
    (f.id.to_string(), src)
}

/// The developer allows publishing (pushing the branch and opening the PR).
async fn allow_publish(host: &TestHost, id: &str) {
    use otter_core::feature::{FeatureAction, FeatureStatus, Risk};
    let f = wait_feature(host, id, "asked to publish", |f| {
        f.status == FeatureStatus::Review
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
        .feature_act(
            "publish",
            id,
            FeatureAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
            },
        )
        .await
        .unwrap();
}

fn write_checks(dir: &Path, file: &str, checks: serde_json::Value) {
    std::fs::write(dir.join(file), checks.to_string()).unwrap();
}

#[tokio::test]
async fn a_feature_ships_as_a_pull_request_once_ci_is_green() {
    use otter_core::feature::{EvidenceKind, FeatureAction, FeatureStatus, GateStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, origin) = git_feature(&fake).await;
    allow_publish(&fake.host, &id).await;

    // Pushed for real, PR opened through gh; CI hasn't reported yet.
    let f = wait_feature(&fake.host, &id, "published", |f| {
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
    let f = wait_feature(&fake.host, &id, "CI pending", |f| {
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
        .feature_act(
            "early",
            &id,
            FeatureAction::Accept {
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
    let f = wait_feature(&fake.host, &id, "remediation after failed CI", |f| {
        f.tasks.iter().any(|t| t.title == "Fix what CI found")
    })
    .await;
    let fix = f
        .tasks
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
    let f = wait_feature(&fake.host, &id, "ready to accept", |f| {
        f.status == FeatureStatus::Review && f.report.is_some()
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
        .feature_act(
            "accept",
            &id,
            FeatureAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, FeatureStatus::Done);
    // Never merged, never anything gh wasn't meant to do; no token on a command line.
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap();
    assert!(!calls.contains("merge"), "{calls}");
    assert!(!calls.to_lowercase().contains("token"), "{calls}");
}

#[tokio::test]
async fn flaky_ci_is_rerun_not_blamed_on_the_code() {
    use otter_core::feature::FeatureStatus;
    let fake = fake_agent_host("ok", "rules").await;
    let (id, _) = git_feature(&fake).await;
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
    let f = wait_feature(&fake.host, &id, "green after a rerun", |f| {
        f.report.is_some()
    })
    .await;
    assert_eq!(f.status, FeatureStatus::Review);
    assert!(
        !f.tasks.iter().any(|t| t.title == "Fix what CI found"),
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
    use otter_core::feature::{FeatureAction, FeatureStatus, GateStatus};
    let fake = fake_agent_host("ok", "rules").await;
    let (id, origin) = git_feature(&fake).await;
    let f = wait_feature(&fake.host, &id, "asked to publish", |f| {
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
        .feature_act(
            "no",
            &id,
            FeatureAction::Decide {
                decision_id: d.id,
                approve: false,
                answer: None,
            },
        )
        .await
        .unwrap();
    let f = wait_feature(&fake.host, &id, "report without a PR", |f| {
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
        .feature_act(
            "accept",
            &id,
            FeatureAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, FeatureStatus::Done);
}
