//! End-to-end tests: a real `workd` (auto-started through `workd dial`) with a
//! real, isolated tmux server per test.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use workd_client::{Connection, Transport};
use workd_core::{
    EnvironmentKind, EnvironmentStatus, ExecutionState, SessionKind, SessionStatus, Workspace,
    WorkspaceSource, WorkspaceState,
};
use workd_protocol::frame::{AttachExitReason, Frame};
use workd_protocol::{
    Event, SessionAttach, SessionCreate, SessionRef, SessionSpec, SourceSpec, WorkspaceCreate,
};

struct TestHost {
    dir: tempfile::TempDir,
    transport: Transport,
    /// A daemon started directly by the test (see `with_env`).
    daemon: Option<std::process::Child>,
}

impl TestHost {
    fn new() -> Self {
        // Short path: Unix socket paths are length-limited.
        let dir = tempfile::Builder::new().prefix("wd").tempdir().unwrap();
        let transport = Transport::Local {
            workd_path: env!("CARGO_BIN_EXE_workd").to_owned(),
            home: Some(dir.path().to_string_lossy().into_owned()),
        };
        TestHost {
            dir,
            transport,
            daemon: None,
        }
    }

    /// Start the daemon directly with extra environment variables (which the
    /// login-shell environment it captures inherits).
    async fn with_env(vars: &[(&str, String)]) -> Self {
        let mut host = Self::new();
        let daemon = Command::new(env!("CARGO_BIN_EXE_workd"))
            .arg("--home")
            .arg(host.home())
            .arg("serve")
            .envs(vars.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        host.daemon = Some(daemon);
        let socket = host.home().join("run/workd.sock");
        eventually("daemon listening", || {
            let ok = socket.exists();
            async move { ok.then_some(()) }
        })
        .await;
        host
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
    assert_eq!(status.protocol_version, workd_protocol::PROTOCOL_VERSION);
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

    // The shell runs in the workspace root with workd's variables set.
    let mut conn = host.conn().await;
    conn.session_write(
        "scratch",
        "shell",
        "echo \"id=$WORKD_SESSION_ID pwd=$(pwd)\"",
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
) -> (workd_client::AttachReader, workd_client::AttachWriter) {
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
async fn read_until(reader: &mut workd_client::AttachReader, needle: &str) -> String {
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

async fn read_exit(reader: &mut workd_client::AttachReader) -> workd_protocol::frame::AttachExit {
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

async fn send(writer: &mut workd_client::AttachWriter, text: &str) {
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
    stream: &mut workd_client::EventStream,
    seq: u64,
) -> Vec<workd_protocol::EventRecord> {
    let mut out: Vec<workd_protocol::EventRecord> = Vec::new();
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
    // A subscriber that goes away (laptop sleeps).
    let stream = host
        .conn()
        .await
        .subscribe(Some(snapshot.seq))
        .await
        .unwrap();
    assert!(stream.seq >= snapshot.seq);
    drop(stream);

    // Meanwhile: a task runs, and the daemon restarts.
    task(&host, "ev", "t").await;
    host.stop_daemon().await;

    // Reconnecting with the cursor replays exactly what was missed, in order.
    let mut stream = host
        .conn()
        .await
        .subscribe(Some(snapshot.seq))
        .await
        .unwrap();
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
        .subscribe(Some(snapshot.seq))
        .await
        .unwrap();
    task(&host, "snap", "t").await;
    let first = events_through(&mut stream, snapshot.seq + 1).await;
    assert_eq!(first[0].seq, snapshot.seq + 1);

    // A cursor this host never issued (e.g. its log was reset) is refused,
    // not silently treated as "from now".
    match host.conn().await.subscribe(Some(snapshot.seq + 1000)).await {
        Err(workd_client::ClientError::Rpc(e)) => {
            assert_eq!(e.code, workd_protocol::ErrorCode::CursorExpired, "{e}")
        }
        Err(e) => panic!("unexpected error: {e}"),
        Ok(_) => panic!("stale cursor accepted"),
    }
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

fn git_source(ws: &Workspace) -> &workd_core::GitSource {
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
    assert_eq!(ga.branch, "workd/feature-a");
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
        &["show-ref", "--verify", "refs/heads/workd/feature-a"],
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
        &[(".envrc", "export WORKD_TEST_VAR=from-direnv\n")],
    );
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws(
        "env",
        &src,
        None,
        vec![spec(SessionKind::Task, "show", "echo var=$WORKD_TEST_VAR")],
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
const FAKE_CODEX: &str = r#"#!/bin/sh
if [ "$1" = --version ]; then echo "codex-cli 0.0.0-fake"; exit 0; fi
echo "$*" >> "$FAKE_CODEX_LOG"
now() { date -u +%Y-%m-%dT%H:%M:%S.000Z; }
if [ "$2" = resume ]; then
  id="$3"
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
    echo "Allow command? [y/n]"
    read answer
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

async fn fake_codex_host(mode: &str) -> FakeCodex {
    let scratch = tempfile::Builder::new().prefix("wdc").tempdir().unwrap();
    let bin = scratch.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let codex = bin.join("codex");
    std::fs::write(&codex, FAKE_CODEX).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
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
        ("FAKE_CODEX_LOG", p(&log)),
        ("FAKE_CODEX_MODE", mode.into()),
        ("WORKD_AGENT_QUIET_SECS", "2".into()),
    ])
    .await;
    FakeCodex {
        host,
        log,
        _scratch: scratch,
    }
}

async fn wait_agent(
    host: &TestHost,
    ws: &str,
    session: &str,
    want: workd_core::AgentState,
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
    use workd_core::{Activity, AgentState, AttentionKind};
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
async fn quiet_agent_mid_turn_is_flagged_and_answering_clears_it() {
    use workd_core::{Activity, AgentState, AttentionKind};
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
async fn failures_and_completions_raise_attention() {
    use workd_core::{Activity, AttentionKind};
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
    use workd_core::AttentionKind;
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
