//! Slack's Web API (D-200), through curl as the model providers' are
//! (D-095): the token in curl's config on stdin, never on a command line;
//! the body from a private temporary file.

use std::io::Write;

use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::EnvMap;

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
    let mut child = cmd.spawn().map_err(|e| unreachable(&e))?;
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
    let answer = read(status.trim(), answer)?;
    if method == "chat.postMessage"
        && let (Some(channel), Some(ts)) = (body["channel"].as_str(), answer["ts"].as_str())
    {
        let mut posted = POSTED.lock().unwrap();
        posted.push_back((channel.to_owned(), ts.to_owned()));
        while posted.len() > MAX_POSTED {
            posted.pop_front();
        }
    }
    Ok(answer)
}

/// The messages this process posted lately, by channel and time, newest
/// last: where a reply in one of their threads belongs, when otterhub asks
/// (`slack.owns`, D-206). Kept in memory: after a restart a reply under an
/// older message is placed by otterhub's own record instead.
static POSTED: std::sync::Mutex<std::collections::VecDeque<(String, String)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const MAX_POSTED: usize = 2000;

/// Whether this process posted the message at `ts` in `channel` lately.
pub fn posted_lately(channel: &str, ts: &str) -> bool {
    POSTED
        .lock()
        .unwrap()
        .iter()
        .any(|(c, t)| c == channel && t == ts)
}

/// `program` on `env`'s `PATH`.
fn which(program: &str, env: &EnvMap) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = env.get("PATH")?;
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|p| {
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
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
    // What Slack says is wrong, where it says (`invalid_blocks`: which
    // block, and why).
    let why: Vec<&str> = v["response_metadata"]["messages"]
        .as_array()
        .map(|m| m.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if why.is_empty() {
        return Err(CallError::Refused(code.to_owned()));
    }
    let why = otter_core::redact::redact(&why.join("; "));
    Err(CallError::Refused(format!("{code}: {why}")))
}

/// Whether `url` is Slack's, where its files are: the bot's token goes
/// nowhere else, whatever a message says. (`OTTER_SLACK_API`'s host too, for
/// tests.)
fn slacks(url: &str) -> bool {
    let host = |u: &str| {
        url::Url::parse(u)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
    };
    let Some(h) = host(url) else {
        return false;
    };
    let ours = ["slack.com", "slack-edge.com"]
        .iter()
        .any(|d| h == *d || h.ends_with(&format!(".{d}")));
    ours || host(&base()).is_some_and(|b| b == h && std::env::var("OTTER_SLACK_API").is_ok())
}

/// Fetch a file Slack keeps (a picture, D-202) with `token`, at most `max`
/// bytes: more is refused, whether or not Slack said its length. A redirect
/// to another host goes without the token (curl drops it).
pub async fn download(
    env: &EnvMap,
    token: &str,
    url: &str,
    max: usize,
) -> Result<Vec<u8>, CallError> {
    use tokio::io::AsyncReadExt;
    if !slacks(url) {
        return Err(CallError::Refused("not a Slack address".into()));
    }
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
    let mut child = cmd.spawn().map_err(|e| unreachable(&e))?;
    let config = format!("header = \"Authorization: Bearer {token}\"\n");
    let mut stdin = child.stdin.take().expect("piped");
    stdin
        .write_all(config.as_bytes())
        .await
        .map_err(|e| unreachable(&e))?;
    drop(stdin);
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .expect("piped")
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| unreachable(&e))?;
    if bytes.len() > max {
        return Err(CallError::Refused("too large".into()));
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| unreachable(&e))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(CallError::Refused(otter_core::redact::redact(why.trim())));
    }
    Ok(bytes)
}

/// Say something more about a slash command, where Slack said to
/// (`response_url`, D-202): seen only by who typed it, no token needed.
pub async fn respond(env: &EnvMap, url: &str, text: &str) -> Result<(), CallError> {
    if !slacks(url) {
        return Err(CallError::Refused("not a Slack address".into()));
    }
    let unreachable = |e: &dyn std::fmt::Display| CallError::Unreachable(e.to_string());
    let curl = which("curl", env).ok_or_else(|| unreachable(&"curl is not installed"))?;
    let mut file = tempfile::Builder::new()
        .prefix("otter-slack")
        .tempfile()
        .map_err(|e| unreachable(&e))?;
    let body = serde_json::json!({ "response_type": "ephemeral", "text": text });
    file.write_all(body.to_string().as_bytes())
        .map_err(|e| unreachable(&e))?;
    let out = tokio::process::Command::new(&curl)
        .args(["-sS", "--fail", "--max-time", "30"])
        .args(["-H", "Content-Type: application/json", "--data-binary"])
        .arg(format!("@{}", file.path().display()))
        .arg(url)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| unreachable(&e))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(CallError::Refused(otter_core::redact::redact(why.trim())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bot's token is sent to Slack's hosts only, whatever address a
    /// message carries.
    #[test]
    fn files_are_fetched_from_slack_only() {
        if std::env::var("OTTER_SLACK_API").is_ok() {
            return;
        }
        assert!(slacks("https://files.slack.com/files-pri/T1-F1/a.png"));
        assert!(slacks("https://a.slack-edge.com/x.png"));
        for not in [
            "https://evil.example/x.png",
            "https://slack.com.evil.example/x",
            "http://127.0.0.1:9/x",
            "not a url",
        ] {
            assert!(!slacks(not), "{not}");
        }
    }

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
        // Which block, as Slack said it of a multi-select in a message.
        let blocks = r#"{"ok":false,"error":"invalid_blocks","errors":["unsupported element: multiselect [json-pointer:/blocks/4]"],"response_metadata":{"messages":["[ERROR] unsupported element: multiselect [json-pointer:/blocks/4]"]}}"#;
        match read("200", blocks) {
            Err(CallError::Refused(why)) => assert_eq!(
                why,
                "invalid_blocks: [ERROR] unsupported element: multiselect [json-pointer:/blocks/4]"
            ),
            other => panic!("{other:?}"),
        }
    }
}
