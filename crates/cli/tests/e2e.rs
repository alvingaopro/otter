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
}

impl Cli {
    fn new() -> Self {
        let config = tempfile::Builder::new().prefix("wc").tempdir().unwrap();
        Cli { config }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_otter"))
            .args(args)
            .env("OTTER_CONFIG_DIR", self.config.path())
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

#[test]
fn brief_is_set_shown_and_edited() {
    let home = Home::new();
    let cli = Cli::new();
    cli.add_local_host("here", &home);
    cli.ok(&[
        "new",
        "why",
        "--no-agent",
        "--goal",
        "Fix renewal validation",
    ]);
    let out = cli.ok(&["ws", "brief", "why"]);
    assert_eq!(out.trim(), "goal: Fix renewal validation");

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
    assert!(out.contains("goal: Fix renewal validation"), "{out}");
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

    cli.ok(&["ws", "brief", "why", "--goal", ""]);
    assert!(cli.workspace("why")["brief"].get("goal").is_none());
    let out = cli.ok(&["ws", "brief", "why", "--clear"]);
    assert!(out.starts_with("why has no brief"), "{out}");
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
        .arg(format!("ControlPath={}/cm/%C", cli.config.path().display()))
        .args(["-O", "exit", "localhost"])
        .stderr(Stdio::null())
        .status();
    eprintln!("ssh e2e: passed over ssh localhost");
}
