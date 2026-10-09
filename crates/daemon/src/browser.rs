//! Browser verification (D-046): run a project's preview and drive a real
//! headless Chrome/Chromium against it, so the Control Agent checks the UI
//! itself instead of trusting the coding agent.
//!
//! - **Checks are declared by the project** in `.otter/browser-checks.json`
//!   (reviewable, deterministic): a preview command (with `{port}`), and
//!   steps — go to a path, click, fill, expect text, expect visible, take a
//!   screenshot.
//! - **The browser** runs on the host, next to the preview, so a remote
//!   workspace needs no tunnel to be verified. It is driven over the Chrome
//!   DevTools Protocol on `--remote-debugging-pipe` (NUL-separated JSON on
//!   fds 3 and 4): no port, no extra dependency.
//! - **Isolation:** a fresh profile directory per check (deleted after), a
//!   fresh browser context, an environment with no secrets (only PATH and a
//!   throwaway HOME), requests to anything but the preview's own origin
//!   blocked, downloads denied.
//! - **Evidence:** each step's result, console errors and uncaught
//!   exceptions, failed or 4xx/5xx requests, blocked requests, screenshots.
//!   A failed step, a console error or a failed request fails the check.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::env::{EnvMap, which};

/// Where a project declares its browser checks.
pub const CHECKS_FILE: &str = ".otter/browser-checks.json";
const STEP_TIMEOUT: Duration = Duration::from_secs(15);
const READY_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    /// Starts the app; `{port}` is replaced by the port to listen on (also
    /// in `PORT`).
    pub command: String,
    /// Polled until it answers (default `/`).
    #[serde(default = "root_path")]
    pub ready_path: String,
}

fn root_path() -> String {
    "/".into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Step {
    Goto {
        path: String,
    },
    Click {
        selector: String,
    },
    Fill {
        selector: String,
        value: String,
    },
    ExpectText {
        #[serde(default)]
        selector: Option<String>,
        text: String,
    },
    ExpectVisible {
        selector: String,
    },
    Screenshot {
        #[serde(default)]
        name: Option<String>,
    },
    Wait {
        ms: u64,
    },
}

impl Step {
    fn describe(&self) -> String {
        match self {
            Step::Goto { path } => format!("go to {path}"),
            Step::Click { selector } => format!("click {selector}"),
            Step::Fill { selector, value } => format!("fill {selector} with “{value}”"),
            Step::ExpectText {
                selector: Some(s),
                text,
            } => format!("expect “{text}” in {s}"),
            Step::ExpectText {
                selector: None,
                text,
            } => format!("expect “{text}”"),
            Step::ExpectVisible { selector } => format!("expect {selector} visible"),
            Step::Screenshot { name } => {
                format!("screenshot {}", name.as_deref().unwrap_or(""))
            }
            Step::Wait { ms } => format!("wait {ms} ms"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrowserCheck {
    pub name: String,
    pub preview: Preview,
    pub steps: Vec<Step>,
    /// Console errors don't fail the check.
    #[serde(default)]
    pub allow_console_errors: bool,
}

#[derive(Debug, Deserialize)]
struct ChecksFile {
    checks: Vec<BrowserCheck>,
}

/// The project's browser checks, if it declares any.
pub fn load_checks(root: &Path) -> Result<Vec<BrowserCheck>> {
    match std::fs::read(root.join(CHECKS_FILE)) {
        Ok(bytes) => Ok(serde_json::from_slice::<ChecksFile>(&bytes)
            .with_context(|| format!("reading {CHECKS_FILE}"))?
            .checks),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// A Chrome or Chromium on this host: `OTTER_BROWSER`, else the usual names.
pub fn find_browser(env: &EnvMap) -> Option<PathBuf> {
    if let Some(p) = env
        .get("OTTER_BROWSER")
        .cloned()
        .or_else(|| std::env::var("OTTER_BROWSER").ok())
        .filter(|p| !p.is_empty())
    {
        return Some(PathBuf::from(p));
    }
    // Google Chrome first; a Snap-packaged Chromium (Ubuntu's
    // `chromium-browser`) doesn't pass the DevTools pipe through its
    // launcher, so it is skipped.
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
    ] {
        if let Some(p) = which(name, env) {
            let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if real.starts_with("/snap") || real.ends_with("snap") {
                continue;
            }
            return Some(p);
        }
    }
    let mac = Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
    mac.exists().then(|| mac.to_path_buf())
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StepResult {
    pub step: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    pub name: String,
    pub ok: bool,
    pub url: Option<String>,
    pub steps: Vec<StepResult>,
    pub console_errors: Vec<String>,
    pub network_errors: Vec<String>,
    pub blocked: Vec<String>,
    pub screenshots: Vec<PathBuf>,
    /// Setting up failed (no browser, the preview didn't start, …).
    pub error: Option<String>,
    /// The throwaway profile it used (gone by the time the check returns).
    #[serde(skip)]
    pub profile: Option<PathBuf>,
}

impl Outcome {
    /// What happened, a line each, for evidence and the Control Agent.
    pub fn report(&self) -> String {
        let mut lines = Vec::new();
        if let Some(e) = &self.error {
            lines.push(format!("Error: {e}"));
        }
        for s in &self.steps {
            lines.push(format!(
                "{} {}{}",
                if s.ok { "✓" } else { "✗" },
                s.step,
                s.detail
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default()
            ));
        }
        for e in &self.console_errors {
            lines.push(format!("console error: {e}"));
        }
        for e in &self.network_errors {
            lines.push(format!("request failed: {e}"));
        }
        for b in &self.blocked {
            lines.push(format!("blocked (not the preview's origin): {b}"));
        }
        lines.join("\n")
    }
}

/// A running preview, stopped (with everything it started) when dropped.
pub struct PreviewServer {
    child: Child,
}

impl Drop for PreviewServer {
    fn drop(&mut self) {
        // Its own process group: take the whole tree down.
        let pgid = self.child.id() as i32;
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

/// A free local port.
pub fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// Start the preview in `root` and wait until it answers on `ready_path`.
pub fn start_preview(
    preview: &Preview,
    root: &Path,
    env: &EnvMap,
    port: u16,
) -> Result<PreviewServer> {
    let command = preview.command.replace("{port}", &port.to_string());
    let child = std::process::Command::new("sh")
        .args(["-c", &command])
        .current_dir(root)
        .env_clear()
        .envs(env)
        .env("PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting the preview `{command}`"))?;
    let mut server = PreviewServer { child };
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = http_status(port, &preview.ready_path)
            && status < 500
        {
            return Ok(server);
        }
        if let Ok(Some(status)) = server.child.try_wait() {
            bail!("the preview `{command}` exited ({status}) before it was ready");
        }
        if Instant::now() > deadline {
            bail!(
                "the preview `{command}` didn't answer on port {port} within {}s",
                READY_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The HTTP status of `GET path` on a local port, if it answers.
fn http_status(port: u16, path: &str) -> Option<u16> {
    let mut s = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n").ok()?;
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).ok()?;
    let line = String::from_utf8_lossy(&buf[..n]);
    line.split_whitespace().nth(1)?.parse().ok()
}

/// A pipe whose ends are closed in children unless passed on purpose.
fn pipe() -> Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((fds[0], fds[1]))
}

/// A headless browser on a DevTools pipe, with its own throwaway profile.
struct Browser {
    child: Child,
    profile: tempfile::TempDir,
    /// What the browser printed (to explain a browser that stops answering).
    log: tempfile::NamedTempFile,
    to: std::fs::File,
    from: mpsc::Receiver<Value>,
    next: u64,
    events: Vec<Value>,
    allowed: HashSet<String>,
    blocked: Vec<String>,
}

impl Browser {
    fn launch(program: &Path) -> Result<Browser> {
        let profile = tempfile::Builder::new().prefix("otter-browser").tempdir()?;
        let log = tempfile::Builder::new()
            .prefix("otter-browser-log")
            .tempfile()?;
        let (to_r, to_w) = pipe()?;
        let (from_r, from_w) = pipe()?;
        let mut cmd = std::process::Command::new(program);
        cmd.args([
            "--headless=new",
            "--remote-debugging-pipe",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-sync",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-default-apps",
            "--password-store=basic",
            "--use-mock-keychain",
            "--mute-audio",
            "--window-size=1280,800",
            // Machines without a GPU or with a small /dev/shm (CI runners,
            // containers): software rendering, shared memory in /tmp.
            "--disable-gpu",
            "--disable-dev-shm-usage",
        ])
        .arg(format!("--user-data-dir={}", profile.path().display()));
        // Chrome's sandbox can't run as root, nor where user namespaces are
        // restricted (some CI machines): `OTTER_BROWSER_NO_SANDBOX=1`.
        if unsafe { libc::geteuid() } == 0 || std::env::var_os("OTTER_BROWSER_NO_SANDBOX").is_some()
        {
            cmd.arg("--no-sandbox");
        }
        // No secrets: a minimal environment with a throwaway home.
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", profile.path())
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log.reopen()?))
            .process_group(0);
        unsafe {
            cmd.pre_exec(move || {
                // The DevTools pipe: Chrome reads fd 3 and writes fd 4. Copy
                // both ends out of the way first (either may already be 3
                // or 4); `dup2` then leaves 3 and 4 open across exec.
                let r = libc::fcntl(to_r, libc::F_DUPFD_CLOEXEC, 10);
                let w = libc::fcntl(from_w, libc::F_DUPFD_CLOEXEC, 10);
                if r < 0 || w < 0 || libc::dup2(r, 3) < 0 || libc::dup2(w, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        unsafe {
            libc::close(to_r);
            libc::close(from_w);
        }
        let to = unsafe { std::fs::File::from_raw_fd(to_w) };
        let mut from = unsafe { std::fs::File::from_raw_fd(from_r) };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 65536];
            loop {
                let n = match from.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
                while let Some(i) = buf.iter().position(|&b| b == 0) {
                    let msg: Vec<u8> = buf.drain(..=i).collect();
                    if let Ok(v) = serde_json::from_slice::<Value>(&msg[..msg.len() - 1])
                        && tx.send(v).is_err()
                    {
                        return;
                    }
                }
            }
        });
        Ok(Browser {
            child,
            profile,
            log,
            to,
            from: rx,
            next: 0,
            events: Vec::new(),
            allowed: HashSet::new(),
            blocked: Vec::new(),
        })
    }

    fn send(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<u64> {
        self.next += 1;
        let mut msg = json!({"id": self.next, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let mut bytes = serde_json::to_vec(&msg)?;
        bytes.push(0);
        self.to.write_all(&bytes)?;
        Ok(self.next)
    }

    /// Handle a message that isn't the reply being waited for.
    fn on_message(&mut self, v: Value) {
        if v["method"] == "Fetch.requestPaused" {
            // Only the preview's own origin gets through.
            let url = v["params"]["request"]["url"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let id = v["params"]["requestId"].clone();
            let session = v["sessionId"].as_str().map(String::from);
            let allowed = origin(&url).is_some_and(|o| self.allowed.contains(&o))
                || url.starts_with("data:")
                || url.starts_with("about:");
            let (method, params) = if allowed {
                ("Fetch.continueRequest", json!({"requestId": id}))
            } else {
                self.blocked.push(url);
                (
                    "Fetch.failRequest",
                    json!({"requestId": id, "errorReason": "BlockedByClient"}),
                )
            };
            let _ = self.send(session.as_deref(), method, params);
            return;
        }
        if v.get("method").is_some() {
            self.events.push(v);
        }
    }

    fn call(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.send(session, method, params)?;
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| anyhow!("{method} timed out"))?;
            let v = self.from.recv_timeout(left).map_err(|_| {
                anyhow!(
                    "{method}: the browser stopped answering{}",
                    self.last_words()
                )
            })?;
            if v["id"].as_u64() == Some(id) {
                if let Some(e) = v.get("error") {
                    bail!("{method}: {}", e["message"].as_str().unwrap_or("failed"));
                }
                return Ok(v["result"].clone());
            }
            self.on_message(v);
        }
    }

    /// Wait for an event (already seen, or arriving within `timeout`).
    fn wait_event(&mut self, method: &str, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(i) = self.events.iter().position(|e| e["method"] == method) {
                return Ok(self.events.remove(i));
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| anyhow!("timed out waiting for {method}"))?;
            match self.from.recv_timeout(left) {
                Ok(v) => self.on_message(v),
                Err(_) => bail!("timed out waiting for {method}"),
            }
        }
    }

    /// The end of what the browser printed, for an error.
    fn last_words(&mut self) -> String {
        let exited = match self.child.try_wait() {
            Ok(Some(status)) => format!(" (it exited: {status})"),
            _ => String::new(),
        };
        let text = std::fs::read_to_string(self.log.path()).unwrap_or_default();
        let tail: Vec<&str> = text.lines().rev().take(8).collect();
        if tail.is_empty() {
            exited
        } else {
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            format!("{exited}: {}", tail.join(" | "))
        }
    }

    /// Take in whatever arrived meanwhile (console messages, …).
    fn drain(&mut self) {
        while let Ok(v) = self.from.try_recv() {
            self.on_message(v);
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.send(None, "Browser.close", json!({}));
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let pgid = self.child.id() as i32;
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = self.child.wait();
        let _ = &self.profile; // Removed with the TempDir.
    }
}

fn origin(url: &str) -> Option<String> {
    let rest = url.split_once("://")?;
    let host = rest.1.split(['/', '?', '#']).next()?;
    Some(format!("{}://{}", rest.0, host))
}

/// JavaScript that finds `selector` (as a JSON string literal).
fn js_find(selector: &str) -> String {
    format!("document.querySelector({})", json!(selector))
}

/// Run one check against an already-running preview at `base`
/// (`http://127.0.0.1:<port>`). Screenshots go to `artifacts`.
pub fn run_against(program: &Path, check: &BrowserCheck, base: &str, artifacts: &Path) -> Outcome {
    let mut out = Outcome {
        name: check.name.clone(),
        url: Some(base.to_owned()),
        ..Default::default()
    };
    if let Err(e) = drive(program, check, base, artifacts, &mut out) {
        out.error = Some(format!("{e:#}"));
    }
    out.ok = out.error.is_none()
        && out.steps.iter().all(|s| s.ok)
        && (check.allow_console_errors || out.console_errors.is_empty())
        && out.network_errors.is_empty();
    out
}

fn drive(
    program: &Path,
    check: &BrowserCheck,
    base: &str,
    artifacts: &Path,
    out: &mut Outcome,
) -> Result<()> {
    std::fs::create_dir_all(artifacts)?;
    let mut b = Browser::launch(program)?;
    out.profile = Some(b.profile.path().to_path_buf());
    for o in [base.to_owned(), base.replace("127.0.0.1", "localhost")] {
        b.allowed.insert(o);
    }
    // A fresh context: no cookies or storage from anything else.
    let ctx = b.call(
        None,
        "Target.createBrowserContext",
        json!({"disposeOnDetach": true}),
    )?;
    let ctx = ctx["browserContextId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let target = b.call(
        None,
        "Target.createTarget",
        json!({"url": "about:blank", "browserContextId": ctx}),
    )?;
    let target = target["targetId"].as_str().unwrap_or_default().to_owned();
    let session = b.call(
        None,
        "Target.attachToTarget",
        json!({"targetId": target, "flatten": true}),
    )?;
    let s = session["sessionId"].as_str().unwrap_or_default().to_owned();
    let s = Some(s.as_str());
    b.call(
        None,
        "Browser.setDownloadBehavior",
        json!({"behavior": "deny", "browserContextId": ctx}),
    )?;
    for m in [
        "Page.enable",
        "Runtime.enable",
        "Network.enable",
        "Log.enable",
    ] {
        b.call(s, m, json!({}))?;
    }
    b.call(
        s,
        "Fetch.enable",
        json!({"patterns": [{"urlPattern": "*"}]}),
    )?;

    let mut shots = 0;
    let mut shot = |b: &mut Browser, name: Option<&str>, out: &mut Outcome| -> Result<()> {
        shots += 1;
        let png = b.call(s, "Page.captureScreenshot", json!({"format": "png"}))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(png["data"].as_str().unwrap_or_default())?;
        let file = artifacts.join(format!(
            "{}-{shots}-{}.png",
            slug(&check.name),
            slug(name.unwrap_or("step"))
        ));
        std::fs::write(&file, data)?;
        out.screenshots.push(file);
        Ok(())
    };

    for step in &check.steps {
        b.drain();
        let result = run_step(&mut b, s, base, step, &mut shot, out);
        let failed = result.is_err();
        out.steps.push(StepResult {
            step: step.describe(),
            ok: !failed,
            detail: result.err().map(|e| format!("{e:#}")),
        });
        if failed {
            let _ = shot(&mut b, Some("failed"), out);
            break;
        }
    }
    b.drain();
    if out.steps.iter().all(|s| s.ok)
        && !matches!(check.steps.last(), Some(Step::Screenshot { .. }))
    {
        let _ = shot(&mut b, Some("final"), out);
    }
    // What the page said and asked for along the way.
    for e in std::mem::take(&mut b.events) {
        let p = &e["params"];
        match e["method"].as_str() {
            Some("Runtime.consoleAPICalled") if p["type"] == "error" => {
                let text: Vec<String> = p["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| {
                        a["value"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| a["description"].as_str().unwrap_or("?").to_owned())
                    })
                    .collect();
                out.console_errors.push(text.join(" "));
            }
            Some("Runtime.exceptionThrown") => {
                let d = &p["exceptionDetails"];
                out.console_errors.push(
                    d["exception"]["description"]
                        .as_str()
                        .or(d["text"].as_str())
                        .unwrap_or("uncaught exception")
                        .to_owned(),
                );
            }
            Some("Network.responseReceived") => {
                let status = p["response"]["status"].as_u64().unwrap_or(0);
                let url = p["response"]["url"].as_str().unwrap_or_default();
                // Browsers ask for a favicon on their own.
                if status >= 400 && !url.ends_with("/favicon.ico") {
                    out.network_errors.push(format!(
                        "{} → {status}",
                        p["response"]["url"].as_str().unwrap_or("?")
                    ));
                }
            }
            Some("Network.loadingFailed")
                if !p["errorText"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("net::ERR_BLOCKED_BY_CLIENT")) =>
            {
                out.network_errors
                    .push(p["errorText"].as_str().unwrap_or("failed").to_owned());
            }
            _ => {}
        }
    }
    out.blocked = std::mem::take(&mut b.blocked);
    Ok(())
}

type Shot<'a> = dyn FnMut(&mut Browser, Option<&str>, &mut Outcome) -> Result<()> + 'a;

fn run_step(
    b: &mut Browser,
    s: Option<&str>,
    base: &str,
    step: &Step,
    shot: &mut Shot<'_>,
    out: &mut Outcome,
) -> Result<()> {
    let eval = |b: &mut Browser, expr: String| -> Result<Value> {
        let r = b.call(
            s,
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            bail!(
                "{}",
                ex["exception"]["description"]
                    .as_str()
                    .unwrap_or("script error")
            );
        }
        Ok(r["result"]["value"].clone())
    };
    // Poll until `expr` is true (the page may still be updating).
    let until = |b: &mut Browser, expr: String, what: &str| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if eval(b, expr.clone())? == json!(true) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("{what}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    match step {
        Step::Goto { path } => {
            b.events.retain(|e| e["method"] != "Page.loadEventFired");
            let url = format!("{base}{path}");
            let r = b.call(s, "Page.navigate", json!({"url": url}))?;
            if let Some(err) = r["errorText"].as_str() {
                bail!("{err}");
            }
            b.wait_event("Page.loadEventFired", STEP_TIMEOUT)?;
        }
        Step::Click { selector } => {
            until(
                b,
                format!("!!{}", js_find(selector)),
                &format!("no element matches {selector}"),
            )?;
            eval(b, format!("{}.click(), true", js_find(selector)))?;
        }
        Step::Fill { selector, value } => {
            until(
                b,
                format!("!!{}", js_find(selector)),
                &format!("no element matches {selector}"),
            )?;
            // The native setter, so frameworks see the change.
            eval(
                b,
                format!(
                    "(() => {{ const el = {}; el.focus(); \
                     const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value').set; \
                     set.call(el, {}); \
                     el.dispatchEvent(new Event('input', {{bubbles: true}})); \
                     el.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }})()",
                    js_find(selector),
                    json!(value)
                ),
            )?;
        }
        Step::ExpectText { selector, text } => {
            let scope = selector
                .as_deref()
                .map(js_find)
                .unwrap_or_else(|| "document.body".into());
            until(
                b,
                format!(
                    "(({scope}) || {{innerText: ''}}).innerText.includes({})",
                    json!(text)
                ),
                &format!("“{text}” not found"),
            )?;
        }
        Step::ExpectVisible { selector } => {
            until(
                b,
                format!(
                    "(() => {{ const el = {}; if (!el) return false; const r = el.getBoundingClientRect(); \
                     const st = getComputedStyle(el); return r.width > 0 && r.height > 0 && st.visibility !== 'hidden'; }})()",
                    js_find(selector)
                ),
                &format!("{selector} isn't visible"),
            )?;
        }
        Step::Screenshot { name } => shot(b, name.as_deref(), out)?,
        Step::Wait { ms } => std::thread::sleep(Duration::from_millis((*ms).min(10_000))),
    }
    Ok(())
}

fn slug(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    s.trim_matches('-').chars().take(40).collect()
}

/// Start the preview, run the check, stop the preview (blocking: run it on
/// a blocking thread).
pub fn run_check(
    program: &Path,
    check: &BrowserCheck,
    root: &Path,
    env: &EnvMap,
    artifacts: &Path,
) -> Outcome {
    let port = match free_port() {
        Ok(p) => p,
        Err(e) => {
            return Outcome {
                name: check.name.clone(),
                error: Some(format!("no free port: {e}")),
                ..Default::default()
            };
        }
    };
    let server = match start_preview(&check.preview, root, env, port) {
        Ok(s) => s,
        Err(e) => {
            return Outcome {
                name: check.name.clone(),
                error: Some(format!("{e:#}")),
                ..Default::default()
            };
        }
    };
    let out = run_against(
        program,
        check,
        &format!("http://127.0.0.1:{port}"),
        artifacts,
    );
    drop(server);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_parse_from_the_project_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_checks(dir.path()).unwrap().is_empty());
        std::fs::create_dir_all(dir.path().join(".otter")).unwrap();
        std::fs::write(
            dir.path().join(CHECKS_FILE),
            r##"{"checks":[{"name":"greets","preview":{"command":"python3 -m http.server {port}"},
               "steps":[{"do":"goto","path":"/"},{"do":"fill","selector":"#name","value":"Ada"},
                        {"do":"click","selector":"button"},{"do":"expect_text","selector":"#out","text":"Hello, Ada"},
                        {"do":"screenshot","name":"greeted"}]}]}"##,
        )
        .unwrap();
        let checks = load_checks(dir.path()).unwrap();
        assert_eq!(checks[0].preview.ready_path, "/");
        assert_eq!(
            checks[0].steps[3],
            Step::ExpectText {
                selector: Some("#out".into()),
                text: "Hello, Ada".into()
            }
        );
    }

    #[test]
    fn origins() {
        assert_eq!(
            origin("http://127.0.0.1:4173/a?b").as_deref(),
            Some("http://127.0.0.1:4173")
        );
        assert_eq!(
            origin("https://evil.example/x").as_deref(),
            Some("https://evil.example")
        );
        assert_eq!(origin("about:blank"), None);
    }

    /// A small app: a form that greets, a broken variant, and a request to
    /// another origin that must be blocked.
    pub(crate) fn sample_app(dir: &Path) {
        std::fs::write(
            dir.join("index.html"),
            r#"<!doctype html><html><body>
<h1>Greeter</h1>
<input id="name"><button id="go">Greet</button>
<p id="out"></p>
<img src="https://example.com/tracker.png" alt="">
<script>
document.getElementById('go').addEventListener('click', () => {
  document.getElementById('out').textContent = 'Hello, ' + document.getElementById('name').value;
});
</script>
</body></html>"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("broken.html"),
            r#"<!doctype html><html><body><p id="out">nothing</p>
<script>console.error('boom'); undefinedFunction();</script></body></html>"#,
        )
        .unwrap();
    }

    fn browser() -> Option<PathBuf> {
        let mut env = EnvMap::new();
        env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        let b = find_browser(&env);
        match &b {
            Some(p) => eprintln!("browser: {}", p.display()),
            None => eprintln!("skipping: no Chrome/Chromium on this host"),
        }
        b
    }

    fn env() -> EnvMap {
        let mut env = EnvMap::new();
        env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        env
    }

    fn check(steps: Vec<Step>) -> BrowserCheck {
        BrowserCheck {
            name: "greeter".into(),
            preview: Preview {
                command: "python3 -m http.server {port} --bind 127.0.0.1".into(),
                ready_path: "/".into(),
            },
            steps,
            allow_console_errors: false,
        }
    }

    #[test]
    fn a_real_browser_exercises_the_flow_and_cleans_up() {
        let Some(program) = browser() else { return };
        let app = tempfile::tempdir().unwrap();
        sample_app(app.path());
        let artifacts = tempfile::tempdir().unwrap();
        let out = run_check(
            &program,
            &check(vec![
                Step::Goto { path: "/".into() },
                Step::Fill {
                    selector: "#name".into(),
                    value: "Ada".into(),
                },
                Step::Click {
                    selector: "#go".into(),
                },
                Step::ExpectText {
                    selector: Some("#out".into()),
                    text: "Hello, Ada".into(),
                },
                Step::ExpectVisible {
                    selector: "h1".into(),
                },
                Step::Screenshot {
                    name: Some("greeted".into()),
                },
            ]),
            app.path(),
            &env(),
            artifacts.path(),
        );
        assert!(out.ok, "{}", out.report());
        // A screenshot was taken (a PNG).
        let png = std::fs::read(&out.screenshots[0]).unwrap();
        assert_eq!(&png[..4], b"\x89PNG");
        // The request to another origin never left the browser.
        assert!(
            out.blocked.iter().any(|u| u.contains("example.com")),
            "{:?}",
            out.blocked
        );
        // The throwaway profile is gone, and so is the preview.
        assert!(!out.profile.as_ref().unwrap().exists());
        assert!(http_status(port_of(&out), "/").is_none());
    }

    #[test]
    fn a_broken_ui_fails_the_check() {
        let Some(program) = browser() else { return };
        let app = tempfile::tempdir().unwrap();
        sample_app(app.path());
        let artifacts = tempfile::tempdir().unwrap();
        // An assertion that doesn't hold.
        let out = run_check(
            &program,
            &check(vec![
                Step::Goto { path: "/".into() },
                Step::Click {
                    selector: "#go".into(),
                },
                Step::ExpectText {
                    selector: Some("#out".into()),
                    text: "Goodbye".into(),
                },
            ]),
            app.path(),
            &env(),
            artifacts.path(),
        );
        assert!(!out.ok);
        assert!(!out.steps[2].ok);
        assert!(
            out.screenshots
                .iter()
                .any(|p| p.to_string_lossy().contains("failed"))
        );
        // Console errors and uncaught exceptions fail it too.
        let out = run_check(
            &program,
            &check(vec![Step::Goto {
                path: "/broken.html".into(),
            }]),
            app.path(),
            &env(),
            artifacts.path(),
        );
        assert!(!out.ok, "{}", out.report());
        assert!(
            out.console_errors.iter().any(|e| e.contains("boom")),
            "{:?}",
            out.console_errors
        );
        assert!(
            out.console_errors
                .iter()
                .any(|e| e.contains("undefinedFunction"))
        );
        // A missing page is a failed request.
        let out = run_check(
            &program,
            &check(vec![Step::Goto {
                path: "/missing.html".into(),
            }]),
            app.path(),
            &env(),
            artifacts.path(),
        );
        assert!(
            out.network_errors.iter().any(|e| e.contains("404")),
            "{}",
            out.report()
        );
    }

    #[test]
    fn the_browser_sees_no_secrets() {
        let Some(program) = browser() else { return };
        // Even if the workspace environment has secrets, the browser's own
        // environment is minimal: check the profile is fresh and the
        // context starts with no cookies.
        let app = tempfile::tempdir().unwrap();
        sample_app(app.path());
        let artifacts = tempfile::tempdir().unwrap();
        let out = run_check(
            &program,
            &check(vec![
                Step::Goto { path: "/".into() },
                Step::ExpectText {
                    selector: None,
                    text: "Greeter".into(),
                },
            ]),
            app.path(),
            &env(),
            artifacts.path(),
        );
        assert!(out.ok, "{}", out.report());
        let mut b = Browser::launch(&program).unwrap();
        let cookies = b.call(None, "Storage.getCookies", json!({})).unwrap();
        assert_eq!(cookies["cookies"], json!([]));
        assert!(b.profile.path().exists());
        let profile = b.profile.path().to_path_buf();
        drop(b);
        assert!(!profile.exists(), "the profile is removed with the browser");
    }

    fn port_of(out: &Outcome) -> u16 {
        out.url
            .as_deref()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
}
