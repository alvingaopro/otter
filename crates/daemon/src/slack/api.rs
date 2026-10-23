//! Slack's Web API (D-200), through curl as the model providers' are
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
    read(status.trim(), answer)
}

/// Slack's answer, by its HTTP status and body: `ok`, refused for good, or
/// worth trying again (asked to slow down, or Slack's own trouble).
fn read(status: &str, body: &str) -> Result<Value, CallError> {
    let again = |why: String| Err(CallError::Unreachable(why));
    if status == "429" {
        return again("it asked to slow down".into());
    }
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return again(format!("it answered HTTP {status}"));
    };
    if v["ok"] == true {
        return Ok(v);
    }
    let code = v["error"].as_str().unwrap_or("unknown_error");
    let theirs = status.starts_with('5')
        || matches!(
            code,
            "ratelimited"
                | "service_unavailable"
                | "internal_error"
                | "fatal_error"
                | "request_timeout"
        );
    if theirs {
        return again(format!("it said `{code}`"));
    }
    Err(CallError::Refused(code.to_owned()))
}

/// Fetch a file Slack keeps (a picture, D-202) with `token`, at most
/// `max` bytes.
pub async fn download(
    env: &EnvMap,
    token: &str,
    url: &str,
    max: usize,
) -> Result<Vec<u8>, CallError> {
    let unreachable = |e: &dyn std::fmt::Display| CallError::Unreachable(e.to_string());
    let curl = which("curl", env).ok_or_else(|| unreachable(&"curl is not installed"))?;
    let mut cmd = tokio::process::Command::new(&curl);
    cmd.args(["-sS", "-L", "--fail", "--max-time", "30", "--config", "-"])
        .arg("--max-filesize")
        .arg(max.to_string())
        .arg(url)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = crate::env::spawn_tokio(&mut cmd)
        .await
        .map_err(|e| unreachable(&e))?;
    let config = format!("header = \"Authorization: Bearer {token}\"\n");
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
        return Err(CallError::Refused(otter_core::redact::redact(why.trim())));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slacks_own_trouble_is_tried_again_and_a_refusal_is_not() {
        assert!(read("200", r#"{"ok":true,"ts":"1.2"}"#).is_ok());
        for (status, body) in [
            ("429", r#"{"ok":false,"error":"ratelimited"}"#),
            ("200", r#"{"ok":false,"error":"ratelimited"}"#),
            ("503", r#"{"ok":false,"error":"service_unavailable"}"#),
            ("500", r#"{"ok":false,"error":"something_new"}"#),
            ("200", r#"{"ok":false,"error":"fatal_error"}"#),
            ("502", "<html>bad gateway</html>"),
        ] {
            assert!(
                matches!(read(status, body), Err(CallError::Unreachable(_))),
                "{status} {body}"
            );
        }
        match read("200", r#"{"ok":false,"error":"channel_not_found"}"#) {
            Err(CallError::Refused(code)) => assert_eq!(code, "channel_not_found"),
            other => panic!("{other:?}"),
        }
    }
}
