//! End-to-end tests of the `otter` binary: a real CLI with an isolated config
//! directory, talking to real `otterd`s (each with its own home and tmux
//! server) — on this machine, and over `ssh localhost` when that works.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// `otterd` from the same build as the `otter` under test.
///
/// Cargo only exposes binaries of this package to its tests, so take the
/// sibling of `otter` in the target directory. `cargo test` at the workspace
/// root builds it before any test runs (the daemon's own tests need it);
/// otherwise (`cargo test -p otter` on a clean tree) build it here.
fn otterd() -> &'static Path {
    static OTTERD: OnceLock<PathBuf> = OnceLock::new();
    OTTERD.get_or_init(|| {
        let path = Path::new(env!("CARGO_BIN_EXE_otter")).with_file_name("otterd");
        if !path.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "otterd", "--bin", "otterd"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .unwrap();
            assert!(status.success(), "building otterd failed");
        }
        assert!(path.is_file(), "no otterd at {}", path.display());
        path
    })
}

/// A daemon home. Dropping it stops the daemon and the sessions' tmux server.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        // Short path: Unix socket paths are length-limited.
        let dir = tempfile::Builder::new().prefix("wd").tempdir().unwrap();
        Home { dir }
    }

    fn path(&self) -> &str {
        self.dir.path().to_str().unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let run = self.dir.path().join("run");
        if let Ok(pid) = std::fs::read_to_string(run.join("workd.pid")) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(run.join("tmux.sock"))
            .arg("kill-server")
            .stderr(Stdio::null())
            .status();
    }
}

/// The CLI with its own config directory (never `~/.config/otter`).
struct Cli {
    config: tempfile::TempDir,
    /// More variables, for the daemons it starts too (after the defaults).
    env: Vec<(String, String)>,
}

impl Cli {
    fn new() -> Self {
        let config = tempfile::Builder::new().prefix("wc").tempdir().unwrap();
        Cli {
            config,
            env: vec![],
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_otter"))
            .args(args)
            .env("OTTER_CONFIG_DIR", self.config.path())
            // Daemons started here inherit it: no model is ever asked.
            .env("OTTER_CONTROLLER", "off")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            // Not a terminal: no prompts, no colors.
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Run and expect success; returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "otter {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Run and expect failure; returns stderr.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "otter {args:?} should fail: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.starts_with("otter: "), "{err}");
        err
    }

    fn add_local_host(&self, name: &str, home: &Home) -> String {
        self.ok(&[
            "host",
            "add",
            name,
            "--local",
            "--otterd-path",
            otterd().to_str().unwrap(),
            "--home",
            home.path(),
        ])
    }

    /// Poll `otter logs target` until it shows `needle`.
    fn wait_logs(&self, target: &str, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let out = self.run(&["logs", target]);
            if String::from_utf8_lossy(&out.stdout).contains(needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {target} to print {needle:?}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// `otter ws show target --json`.
    fn workspace(&self, target: &str) -> serde_json::Value {
        serde_json::from_str(&self.ok(&["ws", "show", target, "--json"])).unwrap()
    }
}

/// The everyday flow against host `host`: create, start, list, address,
/// stop, delete.
fn normal_flow(cli: &Cli, host: &str) {
    let out = cli.ok(&["new", "demo", "--host", host, "--no-agent"]);
    assert!(
        out.contains(&format!("workspace demo ready on {host}")),
        "{out}"
    );
    assert!(out.contains("shell  running"), "{out}");

    let out = cli.ok(&[
        "start",
        "demo",
        "--name",
        "tick",
        "--kind",
        "service",
        "--",
        "while :; do echo tick-$((40+2)); sleep 1; done",
    ]);
    assert!(out.starts_with("started demo/tick"), "{out}");
    // The arithmetic ran in the session's shell, not here.
    cli.wait_logs("demo/tick", "tick-42");
    cli.wait_logs(&format!("{host}:demo/tick"), "tick-42");

    // The grouped view, uncolored because stdout isn't a terminal.
    let ls = cli.ok(&["ls"]);
    assert!(!ls.contains('\x1b'), "{ls:?}");
    let line = ls
        .lines()
        .find(|l| l.contains("demo"))
        .unwrap_or_else(|| panic!("{ls}"));
    assert!(ls.starts_with("WORKING\n"), "{ls}");
    assert!(line.contains(host) && line.contains("tick running"), "{ls}");

    let table = cli.ok(&["ls", "--table"]);
    assert!(table.starts_with("WORKSPACE"), "{table}");
    assert!(table.contains("tick:running"), "{table}");

    // Addressing: host-qualified, session by name; first session by default.
    let out = cli.ok(&["session", "stop", &format!("{host}:demo/tick")]);
    assert_eq!(out.trim(), "demo/tick: stopped");
    let ws = cli.workspace(&format!("{host}:demo"));
    assert_eq!(ws["name"], "demo");
    let names: Vec<_> = ws["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["shell", "tick"]);
    cli.ok(&["send", "demo", "echo via-$((6*7))"]);
    cli.wait_logs("demo/shell", "via-42");

    // Failure paths say what's wrong.
    let err = cli.fails(&["logs", "demo/nope"]);
    assert!(
        err.contains("workspace `demo` has no session `nope` (sessions: shell, tick)"),
        "{err}"
    );
    let err = cli.fails(&["ws", "rm", "demo"]);
    assert!(
        err.contains("refusing to delete without confirmation; pass --yes"),
        "{err}"
    );

    cli.ok(&["session", "rm", "demo/tick"]);
    let out = cli.ok(&["ws", "rm", "demo", "--yes"]);
    assert_eq!(out.trim(), "deleted workspace demo");
    let err = cli.fails(&["ws", "show", "demo"]);
    assert!(err.contains("no workspace `demo`"), "{err}");
    let ls = cli.ok(&["ls"]);
    assert!(ls.contains("no workspaces"), "{ls}");
}

#[test]
fn local_host_normal_flow() {
    let home = Home::new();
    let cli = Cli::new();
    let out = cli.add_local_host("here", &home);
    assert!(out.contains("added host here"), "{out}");
    let hosts = cli.ok(&["host", "ls"]);
    assert!(hosts.contains("here"), "{hosts}");
    normal_flow(&cli, "here");
}

#[test]
fn archive_hides_a_workspace_until_unarchived() {
    let home = Home::new();
    let cli = Cli::new();
    cli.add_local_host("here", &home);
    cli.ok(&["new", "live", "--no-agent"]);
    cli.ok(&["new", "old", "--no-agent"]);

    let out = cli.ok(&["ws", "archive", "old"]);
    assert!(out.starts_with("archived workspace old on here"), "{out}");
    assert_eq!(cli.workspace("old")["state"], "archived");
    let ls = cli.ok(&["ls"]);
    assert!(ls.contains("live") && !ls.contains("old "), "{ls}");
    assert!(
        ls.contains("1 archived (`otter ls --archived` to show)"),
        "{ls}"
    );
    let table = cli.ok(&["ls", "--table"]);
    assert!(!table.contains("old"), "{table}");
    let all = cli.ok(&["ls", "--archived"]);
    let (_, archived) = all
        .split_once("ARCHIVED\n")
        .unwrap_or_else(|| panic!("{all}"));
    assert!(archived.contains("old"), "{all}");
    let err = cli.fails(&["start", "old"]);
    assert!(err.contains("archived; unarchive it first"), "{err}");

    let out = cli.ok(&["ws", "unarchive", "old"]);
    assert!(out.contains("workspace old ready on here"), "{out}");
    assert!(out.contains("shell  stopped"), "{out}");
    assert!(out.contains("`otter session restart old/shell`"), "{out}");
    let out = cli.ok(&["session", "restart", "old/shell"]);
    assert!(out.contains("running"), "{out}");
    assert!(cli.ok(&["ls"]).contains("old"));
}

/// `otter ws rename` and `otter session rename` (D-101): the same workspace
/// and session, under the new names.
#[test]
fn workspaces_and_sessions_are_renamed() {
    let home = Home::new();
    let cli = Cli::new();
    cli.add_local_host("here", &home);
    cli.ok(&["new", "before", "--no-agent"]);
    let id = cli.workspace("before")["id"].clone();
    let out = cli.ok(&["ws", "rename", "before", "after"]);
    assert!(out.contains("renamed workspace before to after"), "{out}");
    assert_eq!(cli.workspace("after")["id"], id);
    let out = cli.ok(&["session", "rename", "after/shell", "build"]);
    assert!(
        out.contains("renamed session after/shell to build"),
        "{out}"
    );
    let ws = cli.workspace("after");
    assert_eq!(ws["sessions"][0]["name"], "build");
}

#[test]
fn brief_is_set_shown_and_edited() {
    let home = Home::new();
    let cli = Cli::new();
    cli.add_local_host("here", &home);
    cli.ok(&["new", "why", "--no-agent"]);
    let out = cli.ok(&["ws", "brief", "why"]);
    assert!(out.starts_with("why has no brief"), "{out}");
    let out = cli.ok(&[
        "ws",
        "brief",
        "why",
        "--description",
        "Fix renewal validation",
    ]);
    assert_eq!(out.trim(), "description: Fix renewal validation");

    let out = cli.ok(&[
        "ws",
        "brief",
        "why",
        "--title",
        "GCP renewal",
        "--decision",
        "Renewal count includes the initial term.",
        "--constraint",
        "Preserve Salesforce behavior.",
    ]);
    assert!(out.contains("title: GCP renewal"), "{out}");
    assert!(out.contains("description: Fix renewal validation"), "{out}");
    assert!(
        out.contains("decisions:\n  - Renewal count includes the initial term."),
        "{out}"
    );
    let show = cli.ok(&["ws", "show", "why"]);
    assert!(
        show.contains("must:     Preserve Salesforce behavior."),
        "{show}"
    );
    assert_eq!(cli.workspace("why")["brief"]["title"], "GCP renewal");

    cli.ok(&["ws", "brief", "why", "--description", ""]);
    assert!(cli.workspace("why")["brief"].get("description").is_none());
    let out = cli.ok(&["ws", "brief", "why", "--clear"]);
    assert!(out.starts_with("why has no brief"), "{out}");
}

#[test]
fn events_follow_resyncs_when_the_hosts_log_starts_over() {
    use std::io::BufRead;

    let home = Home::new();
    let cli = Cli::new();
    cli.add_local_host("here", &home);
    cli.ok(&["new", "first", "--no-agent"]);

    let mut follow = Command::new(env!("CARGO_BIN_EXE_otter"))
        .args(["events", "--follow", "--json", "--limit", "1"])
        .env("OTTER_CONFIG_DIR", cli.config.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = |pipe: Box<dyn std::io::Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(pipe).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        rx
    };
    let stdout = lines(Box::new(follow.stdout.take().unwrap()));
    let stderr = lines(Box::new(follow.stderr.take().unwrap()));
    let wait_for = |rx: &std::sync::mpsc::Receiver<String>, needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if line.contains(needle) {
                return;
            }
            seen.push(line);
        }
        panic!("no {needle:?} in {seen:#?}");
    };
    // The backlog (one event) is printed once it follows on from its cursor.
    wait_for(&stdout, r#""seq""#);
    cli.ok(&["new", "second", "--no-agent"]);
    wait_for(&stdout, r#""name":"second""#);

    // The daemon goes away and its event log with it; the follower's cursor
    // now names a log that no longer exists.
    let run = home.dir.path().join("run");
    let pid = std::fs::read_to_string(run.join("workd.pid")).unwrap();
    Command::new("kill").arg(pid.trim()).status().unwrap();
    // Remove the log before the daemon is gone, so a daemon that the
    // follower's reconnect autostarts can only find an empty one.
    for entry in std::fs::read_dir(home.dir.path().join("state")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("events.")
        {
            std::fs::remove_file(path).unwrap();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
    {
        assert!(Instant::now() < deadline, "daemon didn't exit");
        std::thread::sleep(Duration::from_millis(100));
    }
    cli.ok(&["new", "third", "--no-agent"]);

    // It says events were missed, resyncs, and keeps following.
    wait_for(&stderr, "some events were missed");
    cli.ok(&["new", "fourth", "--no-agent"]);
    wait_for(&stdout, r#""name":"fourth""#);
    let _ = follow.kill();
    let _ = follow.wait();
}

#[test]
fn same_name_on_two_hosts_needs_the_host() {
    let (a, b) = (Home::new(), Home::new());
    let cli = Cli::new();
    cli.add_local_host("a", &a);
    cli.add_local_host("b", &b);
    for host in ["a", "b"] {
        cli.ok(&["new", "demo", "--host", host, "--no-agent"]);
    }

    let err = cli.fails(&["logs", "demo"]);
    assert!(
        err.contains("`demo` exists on several hosts; use one of: a:demo, b:demo")
            || err.contains("`demo` exists on several hosts; use one of: b:demo, a:demo"),
        "{err}"
    );

    // Qualified by host, each reaches its own daemon.
    cli.ok(&["send", "b:demo/shell", "echo on-b"]);
    cli.wait_logs("b:demo", "on-b");
    let a_screen = cli.ok(&["logs", "a:demo/shell"]);
    assert!(!a_screen.contains("on-b"), "{a_screen}");
    let ws_a = cli.workspace("a:demo");
    let ws_b = cli.workspace("b:demo");
    assert!(ws_a["root"].as_str().unwrap().starts_with(a.path()));
    assert!(ws_b["root"].as_str().unwrap().starts_with(b.path()));

    let err = cli.fails(&["logs", "c:demo"]);
    assert!(err.contains("no host `c`; registered: a, b"), "{err}");
}

#[test]
fn tasks_and_the_coding_runtime_from_the_terminal() {
    let home = Home::new();
    let cli = Cli::new();
    cli.add_local_host("here", &home);

    let out = cli.ok(&[
        "task",
        "new",
        "Export: timeline as CSV",
        "--request",
        "Add a CSV export",
    ]);
    assert!(out.contains("a draft; `otter task start ft_"), "{out}");
    // Its name before D-107 still works.
    let ls = cli.ok(&["task", "ls"]);
    assert!(
        ls.contains("Export: timeline as CSV") && ls.contains("draft"),
        "{ls}"
    );
    let all: serde_json::Value = serde_json::from_str(&cli.ok(&["task", "ls", "--json"])).unwrap();
    assert_eq!(all[0]["host"], "here");
    let id = all[0]["id"].as_str().unwrap().to_owned();

    // By id, by host and title start (a title's own colon is no host), any case.
    let out = cli.ok(&["task", "message", "here:export", "Use commas"]);
    assert!(out.contains("Otter has your message"), "{out}");
    let show = cli.ok(&["task", "show", &id]);
    assert!(
        show.contains("status: draft") && show.contains("Use commas"),
        "{show}"
    );
    // Its history, from the terminal (D-076).
    let history = cli.ok(&["task", "history", "export"]);
    assert!(history.contains("You sent a message"), "{history}");
    let err = cli.fails(&["task", "show", "Nothing like it"]);
    assert!(err.contains("no task `Nothing like it`"), "{err}");
    let err = cli.fails(&["task", "decide", "export", "dec_x"]);
    assert!(err.contains("has no decision `dec_x`"), "{err}");

    let status = cli.ok(&["runtime", "status"]);
    assert!(status.starts_with("here: claude via "), "{status}");
    let caps: serde_json::Value =
        serde_json::from_str(&cli.ok(&["runtime", "status", "--json"])).unwrap();
    assert!(
        caps["here"]["features"]["interrupt_turn"].is_boolean(),
        "{caps}"
    );
    assert!(
        cli.ok(&["conversation", "ls"])
            .contains("no conversations yet")
    );
    let err = cli.fails(&["conv", "show", "conv_nope"]);
    assert!(err.contains("no conversation `conv_nope`"), "{err}");

    assert!(cli.ok(&["task", "start", "export"]).contains(": planning"));
    // Everything at work, held and let go at once (D-081).
    let out = cli.ok(&["task", "pause", "--all"]);
    assert!(
        out.contains("here:Export: timeline as CSV: paused"),
        "{out}"
    );
    assert!(
        cli.ok(&["task", "pause", "--all"])
            .contains("nothing is at work")
    );
    let out = cli.ok(&["task", "resume", "--all", "--host", "here"]);
    assert!(out.contains(": planning"), "{out}");
    assert!(cli.ok(&["task", "pause", "export"]).contains(": paused"));
    assert!(
        cli.ok(&["task", "cancel", "export"])
            .contains(": cancelled")
    );
}

#[test]
fn clear_errors_without_a_usable_host() {
    let cli = Cli::new();
    let err = cli.fails(&["ls"]);
    assert!(
        err.contains("no hosts registered; add one with `otter host add <name>`"),
        "{err}"
    );

    let home = Home::new();
    let err = cli.fails(&[
        "host",
        "add",
        "broken",
        "--local",
        "--otterd-path",
        "/nonexistent/otterd",
        "--home",
        home.path(),
    ]);
    assert!(
        err.contains("could not reach otterd (register anyway with --no-check)"),
        "{err}"
    );
    // Nothing was registered.
    assert!(cli.fails(&["ls"]).contains("no hosts registered"));
}

/// The same flow through a real SSH transport (`ssh localhost otterd dial`).
///
/// Skips unless `ssh -o BatchMode=yes localhost true` works; with
/// `OTTER_E2E_REQUIRE_SSH` set (as in CI) a skip is a failure instead.
#[test]
fn ssh_host_normal_flow() {
    let probe = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
        .args(["localhost", "true"])
        .stdin(Stdio::null())
        .output();
    if !probe.as_ref().is_ok_and(|o| o.status.success()) {
        let why = match &probe {
            Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_owned(),
            Err(e) => e.to_string(),
        };
        if std::env::var_os("OTTER_E2E_REQUIRE_SSH").is_some() {
            panic!("ssh localhost doesn't work ({why}) but OTTER_E2E_REQUIRE_SSH is set");
        }
        eprintln!("skipping: ssh localhost doesn't work ({why})");
        return;
    }

    let home = Home::new();
    let cli = Cli::new();
    // An absolute otterd path: a non-interactive ssh's PATH won't have a
    // freshly built binary (and must not pick up an installed one).
    let out = cli.ok(&[
        "host",
        "add",
        "lo",
        "--ssh",
        "localhost",
        "--ssh-arg=-oBatchMode=yes",
        "--otterd-path",
        otterd().to_str().unwrap(),
        "--home",
        home.path(),
    ]);
    assert!(out.contains("added host lo"), "{out}");
    let hosts = cli.ok(&["host", "ls"]);
    assert!(hosts.contains("ssh localhost"), "{hosts}");
    let status = cli.ok(&["host", "status", "lo"]);
    eprintln!("ssh e2e: running over ssh localhost\n{status}");

    normal_flow(&cli, "lo");

    // The daemon really was the one under the test's home.
    assert!(Path::new(home.path()).join("run/workd.sock").exists());
    // Close the shared SSH connection (ControlPersist) if one was opened.
    let _ = Command::new("ssh")
        .arg("-o")
        .arg(format!("ControlPath={}/cz/%C", cli.config.path().display()))
        .args(["-O", "exit", "localhost"])
        .stderr(Stdio::null())
        .status();
    eprintln!("ssh e2e: passed over ssh localhost");
}

/// The M2 golden path from the terminal (D-068): a task written and
/// started with `otter`, worked on by a coding agent (a fake one), checked,
/// published as a pull request (a real Git remote, a fake `gh`), followed
/// through CI and accepted — in a disposable repository.
#[test]
fn a_task_goes_from_the_terminal_to_an_accepted_pull_request() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::Builder::new().prefix("wg").tempdir().unwrap();
    let dir = scratch.path();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = |path: &Path, body: &str| {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    exe(
        &bin.join("claude"),
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo \"2.0.0-fake (Claude Code)\"; exit 0; fi\nexec sh \"$FAKE_CLAUDE_STREAM\" \"$@\"\n",
    );
    exe(
        &bin.join("gh"),
        include_str!("../../daemon/tests/fake_gh.sh"),
    );
    let stream = dir.join("fake_claude_stream.sh");
    std::fs::write(
        &stream,
        include_str!("../../daemon/tests/fake_claude_stream.sh"),
    )
    .unwrap();
    let shell = dir.join("login-sh");
    exe(
        &shell,
        "#!/bin/sh\n[ \"$1\" = -l ] && shift\nexec /bin/sh \"$@\"\n",
    );
    let (gh, home) = (dir.join("gh"), dir.join("home"));
    std::fs::create_dir_all(&gh).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    // The disposable repository, with CI.
    let origin = dir.join("origin");
    std::fs::create_dir_all(origin.join(".github/workflows")).unwrap();
    std::fs::write(origin.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    std::fs::write(origin.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&origin)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);

    let tmux = std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).find(|d| d.join("tmux").is_file()))
        .expect("tmux on PATH");
    let p = |x: &Path| x.to_string_lossy().into_owned();
    let mut cli = Cli::new();
    cli.env = vec![
        ("HOME".into(), p(&home)),
        ("SHELL".into(), p(&shell)),
        (
            "PATH".into(),
            format!("{}:{}:/usr/local/bin:/usr/bin:/bin", p(&bin), p(&tmux)),
        ),
        ("CLAUDE_CONFIG_DIR".into(), p(&dir.join("claude-config"))),
        ("FAKE_CLAUDE_STREAM".into(), p(&stream)),
        ("FAKE_CODEX_LOG".into(), p(&dir.join("agent.log"))),
        ("FAKE_GH_DIR".into(), p(&gh)),
        ("OTTER_CONTROLLER".into(), "rules".into()),
        ("OTTER_CONTROLLER_TICK_MS".into(), "200".into()),
        ("OTTER_PLAN_TURN".into(), "0".into()),
        ("OTTER_CI_POLL_MS".into(), "0".into()),
        ("OTTER_CI_SETTLE_MS".into(), "0".into()),
        ("DEVELOPER_DIR".into(), String::new()),
        ("SDKROOT".into(), String::new()),
    ];
    let daemon = Home::new();
    cli.add_local_host("here", &daemon);
    cli.ok(&[
        "new",
        "csv",
        "--repo",
        &p(&origin),
        "--branch",
        "feature/csv",
        "--no-agent",
        "--no-shell",
    ]);
    let out = cli.ok(&[
        "task",
        "new",
        "CSV export",
        "--request",
        "Export the timeline as CSV",
        "--workspace",
        "csv",
    ]);
    assert!(out.contains("a draft"), "{out}");
    cli.ok(&["task", "start", "csv export"]);

    let show = |cli: &Cli| -> serde_json::Value {
        serde_json::from_str(&cli.ok(&["task", "show", "csv export", "--json"])).unwrap()
    };
    let wait = |what: &str, done: &dyn Fn(&serde_json::Value) -> bool| -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let f = show(&cli);
            if done(&f) {
                return f;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {f:#}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    // Implemented and checked; Otter asks to publish.
    let f = wait("the publish question", &|f| {
        f["decisions"].as_array().is_some_and(|d| {
            d.iter().any(|d| {
                d["status"] == "pending" && d["summary"].as_str().unwrap_or("").contains("git push")
            })
        })
    });
    let decision = f["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["status"] == "pending")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Something the agent left uncommitted, to see it published.
    let ws = cli.workspace("csv");
    std::fs::write(
        Path::new(ws["root"].as_str().unwrap()).join("export.sh"),
        "echo id\n",
    )
    .unwrap();
    cli.ok(&["task", "decide", "csv export", &decision]);
    wait("the pull request", &|f| f["delivery"]["pr_url"].is_string());
    std::fs::write(
        gh.join("checks.json"),
        r#"[{"name":"test","bucket":"pass","link":"","description":""}]"#,
    )
    .unwrap();
    wait("the report", &|f| f["report"].is_string());
    let out = cli.ok(&["task", "accept", "csv export"]);
    assert!(out.contains(": done"), "{out}");
    // On the remote: the branch, with the work.
    let pushed = git(&["show", "--stat", "--format=%s", "feature/csv"]);
    assert!(pushed.contains("export.sh"), "{pushed}");
    let calls = std::fs::read_to_string(gh.join("calls.log")).unwrap();
    assert_eq!(calls.matches("gh pr create").count(), 1, "{calls}");
    assert!(!calls.contains("merge"), "{calls}");

    // Unattended next time (D-083): a trigger, its policy set here.
    let out = cli.ok(&[
        "trigger",
        "add",
        "nightly",
        "--workspace",
        "csv",
        "--request",
        "Refresh the CSV fixtures",
        "--daily",
        "02:00",
        "--publish",
        "--wait-limit",
        "2h",
    ]);
    assert!(
        out.contains("trigger nightly on here: daily at 02:00 UTC, next at"),
        "{out}"
    );
    let ls = cli.ok(&["trigger", "ls"]);
    assert!(
        ls.contains("nightly") && ls.contains("publishes") && ls.contains("waits 120m for you"),
        "{ls}"
    );
    let err = cli.fails(&[
        "trigger",
        "add",
        "x",
        "--workspace",
        "csv",
        "--request",
        "r",
        "--every",
        "soon",
    ]);
    assert!(err.contains("isn't a duration"), "{err}");
    assert!(cli.ok(&["trigger", "rm", "nightly"]).contains("removed"));
}

/// `otter doctor` (D-077): each host's problems with what to do, a host it
/// can't reach as one of them, and exit 1 when there's any.
#[test]
fn doctor_says_whats_wrong_and_what_to_do() {
    let home = Home::new();
    let cli = Cli::new();
    // otterd starts with settings it can't read.
    std::fs::create_dir_all(std::path::Path::new(home.path()).join("state")).unwrap();
    std::fs::write(
        std::path::Path::new(home.path()).join("state/settings.json"),
        "{ not json",
    )
    .unwrap();
    cli.add_local_host("here", &home);
    let out = cli.run(&["doctor", "here"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains("here: ") && text.contains("problem(s)"),
        "{text}"
    );
    assert!(
        text.contains("settings on this host couldn't be read"),
        "{text}"
    );
    assert!(text.contains("→ Fix or remove"), "{text}");
    assert!(text.contains("otterd's log:"), "{text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("problem(s) found"), "{err}");
    // A host it can't reach.
    cli.ok(&[
        "host",
        "add",
        "gone",
        "--local",
        "--otterd-path",
        "/nonexistent/otterd",
        "--home",
        "/nonexistent/home",
        "--no-check",
    ]);
    let out = cli.run(&["doctor", "gone", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let what = v[0]["problems"][0]["what"].as_str().unwrap();
    assert!(what.contains("can't reach otterd on gone"), "{v}");
    assert!(
        v[0]["problems"][0]["next"]
            .as_str()
            .unwrap()
            .contains("otter host install gone")
    );
}

/// `otter chat` (D-093): a message to Otter on a host, and its answer; then
/// the conversation lately.
#[test]
fn otter_chat_talks_to_otter_on_a_host() {
    let home = Home::new();
    let mut cli = Cli::new();
    cli.env = vec![
        ("OTTER_CONTROLLER".into(), "scripted".into()),
        ("OTTER_BRAIN_V2".into(), "1".into()),
        ("OTTER_CONTROLLER_TICK_MS".into(), "200".into()),
    ];
    cli.add_local_host("here", &home);
    let out = cli.ok(&["chat", "What's here?\nCALL: list_tasks {}"]);
    assert!(out.contains("list_tasks:"), "{out}");
    assert!(out.contains("\"newest_first\":[]"), "{out}");
    let out = cli.ok(&["chat"]);
    assert!(out.contains("you: What's here?"), "{out}");
    assert!(out.contains("Otter: list_tasks:"), "{out}");
}

/// `otter attach --scrollback` in a terminal (a PTY): the session's history
/// lands in it, typing goes through, Ctrl-] detaches and undoes the
/// program's modes (D-119).
#[test]
fn attach_with_scrollback_puts_the_history_in_this_terminal() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    let home = Home::new();
    let cli = Cli::new();
    cli.add_local_host("here", &home);
    cli.ok(&["new", "sb", "--host", "here", "--no-agent"]);
    cli.ok(&[
        "send",
        "sb",
        "i=0; while [ $i -lt 200 ]; do i=$((i+1)); echo early-$i; done",
    ]);
    cli.wait_logs("sb", "early-200");

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_otter"));
    cmd.args(["attach", "--scrollback", "sb"]);
    cmd.env("OTTER_CONFIG_DIR", cli.config.path());
    cmd.env("OTTER_CONTROLLER", "off");
    cmd.env("TERM", "xterm-256color");
    let mut child = pty.slave.spawn_command(cmd).unwrap();
    drop(pty.slave);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut reader = pty.master.try_clone_reader().unwrap();
    let sink = seen.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let wait_for = |needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = String::from_utf8_lossy(&seen.lock().unwrap()).into_owned();
            if text.contains(needle) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "never saw {needle:?} in {text:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let mut writer = pty.master.take_writer().unwrap();

    // Lines long off a 30-row screen came, for this terminal to keep.
    let snapshot = wait_for("\x1b[?2026l");
    assert!(snapshot.contains("early-50\x1b[0m\r\n"), "{snapshot:?}");
    assert!(!snapshot.contains("\x1bc"), "this terminal isn't reset");
    writer.write_all(b"echo cli-$((2+3))\r").unwrap();
    wait_for("cli-5");

    writer.write_all(&[0x1d]).unwrap();
    let after = wait_for("[detached from");
    assert!(
        after.contains("\x1b[?1049l") && after.contains("\x1b[<u"),
        "modes undone"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "otter attach didn't exit");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    // The session goes on.
    cli.ok(&["send", "sb", "echo still-$((3+3))"]);
    cli.wait_logs("sb", "still-6");
}
