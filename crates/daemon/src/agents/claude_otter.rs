//! Otter's own turn through the Claude worker (D-089): the worker's `otter`
//! profile — none of Claude Code's tools, no settings, only Otter's tools —
//! in a process of its own for each turn. The model's tool calls come back
//! as `tool_call` events; otterd answers each (`tool_result`) through the
//! gateway, which decides what they may do. The reply is the turn's result.
//!
//! Only a worker that says it has the profile (`ready`/`--check`
//! `profiles`) is used: an older one would run a coding agent instead.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

use super::claude_sdk::{
    Frame, PROTOCOL_VERSION, init_timeout, node, read_frame, strip_nulls, worker_check,
    worker_entry,
};
use crate::env::EnvMap;

/// The profile a worker needs for Otter's turn.
pub const PROFILE: &str = "otter";
/// How much of the worker's stderr is kept for a failure's reason.
const STDERR_KEEP: usize = 4 * 1024;

/// Whether this host's worker can run Otter's turn; `Err` says why not.
pub fn ready(env: &EnvMap) -> Result<(), String> {
    let node = node(env).ok_or("Node.js isn't on this host's PATH")?;
    let worker = worker_entry(env).ok_or("the Claude worker isn't installed")?;
    let check = worker_check(&node, &worker, env)?;
    if let Some(p) = check.problems().first() {
        return Err(p.clone());
    }
    if !check.profiles.iter().any(|p| p == PROFILE) {
        return Err(format!(
            "the installed Claude worker ({}) has no Otter profile; reinstall with `otter host install`",
            check.worker_version
        ));
    }
    Ok(())
}

/// One turn of Otter's.
pub struct OtterRun<'a> {
    pub env: &'a EnvMap,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    /// Otter's instructions (the system prompt).
    pub instructions: &'a str,
    /// What the turn starts with: the context and the developer's messages.
    pub prompt: &'a str,
    /// Otter's tools: `{name, description, input_schema}` each.
    pub tools: Vec<Value>,
    /// Model round trips at most.
    pub max_turns: usize,
}

/// Run it: each tool call answered by `call(name, input)`; the result is
/// the reply.
pub async fn run<F, Fut>(r: OtterRun<'_>, call: F) -> Result<String>
where
    F: Fn(String, Value) -> Fut,
    Fut: std::future::Future<Output = String>,
{
    let node = node(r.env).ok_or_else(|| anyhow!("Node.js isn't on this host's PATH"))?;
    let worker = worker_entry(r.env).ok_or_else(|| anyhow!("the Claude worker isn't installed"))?;
    let mut cmd = tokio::process::Command::new(&node);
    cmd.arg(&worker)
        .current_dir(r.cwd)
        .env_clear()
        .envs(r.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = crate::env::spawn_tokio(&mut cmd)
        .await
        .with_context(|| format!("starting {} {}", node.display(), worker.display()))?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut out = BufReader::new(child.stdout.take().expect("piped"));
    // Diagnostics: drained (a full pipe would stall the worker), the tail
    // kept for a failure's reason.
    let tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let keep = tail.clone();
    let mut stderr = child.stderr.take().expect("piped");
    let drained = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let mut t = keep.lock().unwrap();
            t.push_str(&String::from_utf8_lossy(&buf[..n]));
            if t.len() > STDERR_KEEP {
                let cut = t.len() - STDERR_KEEP;
                let cut = (cut..t.len())
                    .find(|i| t.is_char_boundary(*i))
                    .unwrap_or(t.len());
                t.drain(..cut);
            }
        }
    });
    let mut n = 0u64;
    let mut send = async |kind: &str, payload: Value| -> Result<()> {
        n += 1;
        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": format!("r{n}"),
            "run_id": "otter",
            "generation": 1,
            "type": kind,
            "payload": payload,
        });
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    };
    let mut buf = Vec::new();
    let mut next = async || -> Result<Option<Value>> {
        loop {
            buf.clear();
            match read_frame(&mut out, &mut buf).await? {
                Frame::End => return Ok(None),
                Frame::TooLarge => continue,
                Frame::Line => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                        return Ok(Some(v));
                    }
                }
            }
        }
    };

    // Everything up to the end of the turn: any failure comes out here, so
    // what the worker said on the way goes with it.
    let result: Result<String> = async {
        send(
            "initialize",
            strip_nulls(json!({
                "profile": PROFILE,
                "otter_tools": r.tools,
                "conversation_id": otter_core::ConversationId::generate().to_string(),
                "cwd": r.cwd,
                "model": r.model,
                "instructions": r.instructions,
                "max_turns": r.max_turns,
                "setting_sources": [],
            })),
        )
        .await?;
        let ready = tokio::time::timeout(init_timeout(r.env), async {
            loop {
                let Some(ev) = next().await? else {
                    bail!("the Claude worker ended before it was ready");
                };
                match ev["type"].as_str() {
                    Some("ready") => return Ok(ev),
                    Some("fatal") | Some("nack") => {
                        bail!("the Claude worker: {}", text(&ev, &["message", "error"]))
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| anyhow!("the Claude worker wasn't ready in time"))??;
        let has_profile = ready["profiles"]
            .as_array()
            .is_some_and(|p| p.iter().any(|v| v == PROFILE));
        if !has_profile {
            bail!("the Claude worker has no Otter profile; reinstall with `otter host install`");
        }

        send("send_turn", json!({"turn_id": "t1", "text": r.prompt})).await?;
        let mut said = String::new();
        loop {
            let Some(ev) = next().await? else {
                break Err(anyhow!("the Claude worker ended in the middle of the turn"));
            };
            match ev["type"].as_str() {
                Some("tool_call") => {
                    let (Some(id), Some(tool)) = (ev["call_id"].as_str(), ev["tool"].as_str())
                    else {
                        continue;
                    };
                    let output = call(tool.to_owned(), ev["input"].clone()).await;
                    send("tool_result", json!({"call_id": id, "output": output})).await?;
                }
                Some("text") => said = text(&ev, &["text"]),
                Some("turn_finished") => {
                    break match ev["outcome"].as_str() {
                        Some("completed") => Ok(ev["summary"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                            .map(str::to_owned)
                            .unwrap_or(said)),
                        other => Err(anyhow!(
                            "Otter's turn ended {} ({})",
                            other.unwrap_or("?"),
                            ev["error"].as_str().unwrap_or("no reason given")
                        )),
                    };
                }
                Some("fatal") => {
                    break Err(anyhow!("the Claude worker: {}", text(&ev, &["message"])));
                }
                _ => {}
            }
        }
    }
    .await;
    let _ = send("shutdown", json!({})).await;
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), drained).await;
    let tail = tail.lock().unwrap().clone();
    result.map_err(|e| with_tail(e, &tail))
}

/// A failure, with what the worker's diagnostics last said (redacted).
fn with_tail(e: anyhow::Error, tail: &str) -> anyhow::Error {
    match tail.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(l) => anyhow!(
            "{e:#} (the worker said: {})",
            super::excerpt(&otter_core::redact::redact(l), 200)
        ),
        None => e,
    }
}

fn text(ev: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| ev[*k].as_str())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    use super::*;

    const FAKE: &str = include_str!("../../tests/fake_claude_worker.sh");

    fn fake(mode: &str) -> (tempfile::TempDir, EnvMap) {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("main.js");
        std::fs::write(&worker, FAKE).unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = EnvMap::new();
        env.insert("PATH".into(), "/usr/bin:/bin".into());
        env.insert("OTTER_NODE".into(), "/bin/sh".into());
        env.insert("OTTER_CLAUDE_WORKER".into(), worker.display().to_string());
        env.insert("FAKE_WORKER_MODE".into(), mode.into());
        env.insert("OTTER_WORKER_INIT_TIMEOUT_MS".into(), "3000".into());
        (dir, env)
    }

    async fn turn(
        env: &EnvMap,
        cwd: &Path,
        prompt: &str,
    ) -> (Result<String>, Vec<(String, Value)>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let reply = run(
            OtterRun {
                env,
                cwd,
                model: None,
                instructions: "You are Otter.",
                prompt,
                tools: vec![json!({"name": "get_task", "description": "d", "input_schema": {"type": "object"}})],
                max_turns: 6,
            },
            move |name, input| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push((name.clone(), input));
                    match name.as_str() {
                        "get_task" => r#"{"ok":true,"data":{"status":"blocked"}}"#.to_owned(),
                        _ => r#"{"ok":true,"data":{"done":"passed on"}}"#.to_owned(),
                    }
                }
            },
        )
        .await;
        let calls = calls.lock().unwrap().clone();
        (reply, calls)
    }

    #[tokio::test]
    async fn each_tool_call_is_answered_and_the_result_is_the_reply() {
        let (dir, env) = fake("ok");
        assert_eq!(ready(&env), Ok(()));
        let (reply, calls) = turn(&env, dir.path(), "Context\n[msg_abc1] Why is it stuck?").await;
        assert_eq!(reply.unwrap(), "The task is blocked.");
        assert_eq!(calls, [("get_task".to_owned(), json!({}))]);
        // The developer's message, by the id the prompt gave it.
        let (reply, calls) = turn(&env, dir.path(), "Context\n[msg_abc2] Use tabs FORWARD").await;
        assert_eq!(
            reply.unwrap(),
            "The task is blocked; your message: passed on."
        );
        assert_eq!(
            calls[1],
            (
                "forward_to_agent".to_owned(),
                json!({"message_ids": ["msg_abc2"]})
            )
        );
    }

    #[tokio::test]
    async fn a_worker_without_otters_profile_is_never_used_for_it() {
        let (dir, env) = fake("coding-only");
        let why = ready(&env).unwrap_err();
        assert!(why.contains("no Otter profile"), "{why}");
        let (reply, calls) = turn(&env, dir.path(), "[msg_x] hi").await;
        let e = reply.unwrap_err().to_string();
        assert!(e.contains("no Otter profile"), "{e}");
        assert!(calls.is_empty());
    }

    /// Real Claude, through the built worker's Otter profile: asked why a
    /// task is blocked, it looks with its tools and names the decision. Run
    /// by hand: `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime cargo test
    /// -p otterd --bin otterd live_otter_turn -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
    async fn live_otter_turn() {
        let mut env: EnvMap = std::env::vars().collect();
        env.remove("ANTHROPIC_API_KEY");
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let tools: Vec<Value> = crate::gateway::tools()
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input}))
            .collect();
        let prompt = "Feature: CSV export\nStatus: Blocked: waiting on you.\n\nThe developer's messages to answer, oldest first (forward_to_agent and redirect_agent take these ids):\n[msg_live1] Why is this task blocked?";
        let reply = run(
            OtterRun {
                env: &env,
                cwd: dir.path(),
                model: Some("haiku"),
                instructions: crate::turn::INSTRUCTIONS,
                prompt,
                tools,
                max_turns: 6,
            },
            move |name, _input| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(name.clone());
                    let data = match name.as_str() {
                        "get_pending_decisions" => json!({"decisions": [{
                            "id": "dec_1", "kind": "command", "risk": "high",
                            "summary": "Run `rm -rf /opt/shared`", "developer_only": true, "options": []
                        }]}),
                        "get_task" => json!({"status": "blocked", "status_reason": "Waiting on you: Run `rm -rf /opt/shared`", "pending_decisions": 1}),
                        "get_run_events" => json!({"run": {"state": "waiting"}, "activity": ["Bash: rm -rf /opt/shared"]}),
                        _ => return json!({"ok": false, "error": {"code": "not_allowed_in_this_turn", "message": "not in this test"}}).to_string(),
                    };
                    json!({"ok": true, "data": data}).to_string()
                }
            },
        )
        .await
        .unwrap();
        let calls = calls.lock().unwrap().clone();
        println!("calls: {calls:?}\nreply: {reply}");
        assert!(!calls.is_empty(), "it looked");
        assert!(
            calls.iter().all(|c| c.starts_with("get_")),
            "it only looked: {calls:?}"
        );
        assert!(
            reply.contains("rm") && reply.contains("/opt/shared"),
            "{reply}"
        );
        assert!(!reply.contains("INTENT:"), "{reply}");
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "nothing written"
        );
    }

    #[tokio::test]
    async fn a_worker_that_fails_says_why() {
        let (dir, env) = fake("fatal");
        let (reply, _) = turn(&env, dir.path(), "[msg_x] hi").await;
        let e = reply.unwrap_err().to_string();
        assert!(e.contains("no credentials"), "{e}");
        // What the worker last said goes with it, without its secrets.
        assert!(
            e.contains("the worker said: tried ANTHROPIC_API_KEY="),
            "{e}"
        );
        assert!(!e.contains("WORKERLEAK"), "{e}");
    }
}
