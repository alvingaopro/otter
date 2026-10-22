//! Slack's Web API (D-198), through curl as the model providers' are
//! (D-095): the token in curl's config on stdin, never on a command line;
//! the body from a private temporary file.

use std::io::Write;

use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::env::{EnvMap, which};

/// Where Slack's Web API is. `OTTER_SLACK_API` (tests only) points it at a
/// stand-in server.
fn base() -> String {
    std::env::var("OTTER_SLACK_API")
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "https://slack.com/api".into())
}

/// Why a call didn't do what it asked.
#[derive(Debug)]
pub enum CallError {
    /// Slack wasn't reached, or asked to slow down: worth trying later.
    Unreachable(String),
    /// Slack answered no, with its `error` code: trying again won't help.
    Refused(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Unreachable(why) => write!(f, "Slack wasn't reached: {why}"),
            CallError::Refused(code) => write!(f, "Slack said `{code}`"),
        }
    }
}

impl std::error::Error for CallError {}

/// Call `method` with `token` and a JSON `body`; Slack's answer when it is
/// `ok`.
pub async fn call(
    env: &EnvMap,
    token: &str,
    method: &str,
    body: &Value,
) -> Result<Value, CallError> {
    let unreachable = |e: &dyn std::fmt::Display| CallError::Unreachable(e.to_string());
    let curl = which("curl", env).ok_or_else(|| unreachable(&"curl is not installed"))?;
    let mut file = tempfile::Builder::new()
        .prefix("otter-slack")
        .tempfile()
        .map_err(|e| unreachable(&e))?;
    file.write_all(body.to_string().as_bytes())
        .map_err(|e| unreachable(&e))?;
    let mut cmd = tokio::process::Command::new(&curl);
    cmd.args(["-sS", "--max-time", "30", "--config", "-"])
        .args(["--write-out", "\n%{http_code}", "--data-binary"])
        .arg(format!("@{}", file.path().display()))
        .arg(format!("{}/{method}", base()))
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = crate::env::spawn_tokio(&mut cmd)
        .await
        .map_err(|e| unreachable(&e))?;
    let config = format!(
        "header = \"Authorization: Bearer {token}\"\nheader = \"Content-Type: application/json; charset=utf-8\"\n"
    );
    let mut stdin = child.stdin.take().expect("piped");
    stdin
        .write_all(config.as_bytes())
        .await
        .map_err(|e| unreachable(&e))?;
    drop(stdin);
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| unreachable(&e))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(unreachable(&otter_core::redact::redact(why.trim())));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (answer, status) = stdout.rsplit_once('\n').unwrap_or(("", &stdout));
    if status.trim() == "429" {
        return Err(unreachable(&"it asked to slow down"));
    }
    let v: Value = serde_json::from_str(answer)
        .map_err(|_| unreachable(&format!("it answered HTTP {}", status.trim())))?;
    if v["ok"] == true {
        return Ok(v);
    }
    match v["error"].as_str().unwrap_or("unknown_error") {
        "ratelimited" => Err(unreachable(&"it asked to slow down")),
        code => Err(CallError::Refused(code.to_owned())),
    }
}
