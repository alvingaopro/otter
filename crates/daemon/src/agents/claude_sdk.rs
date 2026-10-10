//! Claude through the official Claude Agent SDK (D-057): the `sdk` backend.
//!
//! otterd starts the worker (`packages/claude-runtime`, Node) as a child —
//! one per run — and speaks versioned JSONL with it on stdin/stdout:
//! commands (`initialize`, `send_turn`, `interrupt`, `resolve_interaction`,
//! `policy_reply`, `shutdown`) in, normalized events out. The worker drives
//! the SDK through its documented interface only, and the Claude Code it
//! runs is the one the SDK bundles (never whichever is on PATH). This file
//! knows the worker's protocol, not the SDK's.
//!
//! - Nothing goes to Claude before the worker says `ready` (protocol
//!   checked, SDK initialized), within [`init_timeout`].
//! - Every tool call is checked by Otter's policy first (`policy_check` →
//!   [`RuntimeEvent::ToolCheck`]); "ask" comes back as a
//!   `permission_request` ([`RuntimeEvent::DecisionNeeded`]).
//! - Frames are bounded ([`MAX_FRAME`]); stderr is drained and kept short
//!   for diagnostics; the process is killed if it doesn't leave when asked.
//!
//! Where things are: the worker's entry point is `OTTER_CLAUDE_WORKER` (its
//! `main.js`, or the package directory), else next to `otterd`
//! (`../lib/otter/claude-runtime/dist/main.js`); Node is `OTTER_NODE`, else
//! `node` on the login PATH.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use otter_core::conversation::{ErrorCategory, TurnOutcome, Usage, UsageScope};
use otter_protocol::conversation::{RuntimeCapabilities, RuntimeFeatures};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use super::claude_stream::{questions, summarize, tool_call};
use crate::env::{EnvMap, which};
use crate::runtime::{
    AgentRuntime, CheckDecision, DecisionAsk, DecisionReply, RunHandle, RunInfo, RunSpec,
    RuntimeEvent, TurnSpec,
};

/// The worker protocol this otterd speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// The SDK the worker is built and tested with (and the Claude Code it bundles).
pub const SDK_VERSION: &str = "0.3.285";
pub const BUNDLED_CLAUDE_CODE: &str = "2.1.285";
/// The largest frame either side accepts.
pub const MAX_FRAME: usize = 1024 * 1024;
/// How much of the worker's stderr is kept for diagnostics.
const STDERR_KEEP: usize = 8 * 1024;

/// How long the worker may take to be ready (`OTTER_WORKER_INIT_TIMEOUT_MS`).
pub fn init_timeout(env: &EnvMap) -> Duration {
    let ms = env
        .get("OTTER_WORKER_INIT_TIMEOUT_MS")
        .cloned()
        .or_else(|| std::env::var("OTTER_WORKER_INIT_TIMEOUT_MS").ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(90_000);
    Duration::from_millis(ms)
}

fn setting(env: &EnvMap, name: &str) -> Option<String> {
    env.get(name)
        .cloned()
        .or_else(|| std::env::var(name).ok())
        .filter(|v| !v.trim().is_empty())
}

/// The worker's entry point, if there is one.
pub fn worker_entry(env: &EnvMap) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = match setting(env, "OTTER_CLAUDE_WORKER") {
        Some(p) => {
            let p = PathBuf::from(p);
            vec![p.join("dist/main.js"), p]
        }
        None => {
            let exe = std::env::current_exe().ok()?;
            let dir = exe.parent()?;
            vec![
                dir.join("../lib/otter/claude-runtime/dist/main.js"),
                dir.join("claude-runtime/dist/main.js"),
            ]
        }
    };
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.canonicalize().unwrap_or(p))
}

/// The Node to run it with.
pub fn node(env: &EnvMap) -> Option<PathBuf> {
    match setting(env, "OTTER_NODE") {
        Some(p) => Some(PathBuf::from(p)),
        None => which("node", env),
    }
}

pub struct ClaudeSdkRuntime;

#[async_trait]
impl AgentRuntime for ClaudeSdkRuntime {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn capabilities(&self, env: &EnvMap) -> RuntimeCapabilities {
        let node = node(env);
        let worker = worker_entry(env);
        let mut notes = vec![];
        if node.is_none() {
            notes.push("Node.js isn't on this host's PATH (the Claude worker needs it).".into());
        }
        if worker.is_none() {
            notes.push(
                "The Claude worker isn't installed next to otterd (set OTTER_CLAUDE_WORKER for a development build)."
                    .into(),
            );
        }
        notes.push(format!(
            "Claude Agent SDK {SDK_VERSION} with its bundled Claude Code {BUNDLED_CLAUDE_CODE}; signs in as Claude Code does on this host."
        ));
        RuntimeCapabilities {
            provider: "claude".into(),
            backend: "sdk".into(),
            available: node.is_some() && worker.is_some(),
            notes,
            tested_version: Some(format!("claude-agent-sdk {SDK_VERSION}")),
            structured_ready: false,
            features: RuntimeFeatures {
                send_turn: true,
                resume: true,
                interrupt_turn: true,
                permission_requests: true,
                questions: true,
                tool_results: true,
                streaming: true,
                attachments: false,
                usage: true,
            },
        }
    }

    async fn start(&self, spec: RunSpec) -> Result<Box<dyn RunHandle>> {
        let node = node(&spec.env).ok_or_else(|| anyhow!("Node.js isn't on this host's PATH"))?;
        let worker =
            worker_entry(&spec.env).ok_or_else(|| anyhow!("the Claude worker isn't installed"))?;
        tracing::debug!(conversation = %spec.conversation_id, run = %spec.run_id, generation = spec.generation, worker = %worker.display(), "starting the Claude worker");
        let mut cmd = tokio::process::Command::new(&node);
        cmd.arg(&worker)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A group of its own: what it starts can be found again.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = crate::env::spawn_tokio(&mut cmd)
            .await
            .with_context(|| format!("starting {} {}", node.display(), worker.display()))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let leader = child.id();
        let shared = Arc::new(Mutex::new(Shared {
            info: RunInfo {
                pid: leader,
                ..Default::default()
            },
            ..Default::default()
        }));

        // Diagnostics: drained always (a full pipe would stall the worker),
        // the tail kept.
        let tail = Arc::new(Mutex::new(String::new()));
        let keep = tail.clone();
        let drain = tokio::spawn(async move {
            let mut stderr = stderr;
            let mut buf = vec![0u8; 8192];
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

        let (tx, rx) = mpsc::channel(256);
        let reader_shared = shared.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match read_frame(&mut lines, &mut line).await {
                    Ok(Frame::Line) => {}
                    Ok(Frame::TooLarge) => {
                        tracing::warn!(
                            "the Claude worker sent a frame over {MAX_FRAME} bytes; dropped"
                        );
                        continue;
                    }
                    Ok(Frame::End) | Err(_) => break,
                }
                let Ok(v) = serde_json::from_slice::<Value>(&line) else {
                    tracing::warn!("the Claude worker sent something that isn't JSON; dropped");
                    continue;
                };
                for msg in parse(&v, &reader_shared) {
                    if tx.send(msg).await.is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(Msg::End).await;
        });

        let mut handle = SdkRun {
            child,
            stdin: Some(stdin),
            events: rx,
            shared,
            reader,
            drain,
            tail,
            next_request: 0,
            run_id: spec.run_id.to_string(),
            generation: spec.generation,
            pending: None,
            pid: leader,
            reaped: false,
        };
        let payload = json!({
            "conversation_id": spec.conversation_id,
            "cwd": spec.cwd,
            "model": spec.model,
            "resume": spec.resume,
            "instructions": spec.instructions,
            "max_turns": spec.limits.max_turns,
            "max_budget_usd": spec.limits.max_budget_usd,
            // CLAUDE.md comes with the project's settings; Otter's policy hook
            // runs before any of their rules.
            "setting_sources": ["project"],
        });
        handle.command("initialize", strip_nulls(payload)).await?;
        let timeout = init_timeout(&spec.env);
        let ready = tokio::time::timeout(timeout, handle.wait_ready()).await;
        match ready {
            Ok(Ok(())) => Ok(Box::new(handle)),
            Ok(Err(e)) => {
                let tail = handle.stderr_tail();
                let _ = handle.stop().await;
                Err(e.context(tail))
            }
            Err(_) => {
                let _ = handle.stop().await;
                bail!(
                    "the Claude worker wasn't ready within {} s",
                    timeout.as_secs()
                )
            }
        }
    }
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        v => v,
    }
}

enum Frame {
    Line,
    TooLarge,
    End,
}

/// One line, at most [`MAX_FRAME`] bytes; a longer one is skipped whole.
async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    out: &mut Vec<u8>,
) -> std::io::Result<Frame> {
    let mut too_large = false;
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if out.is_empty() && !too_large {
                Frame::End
            } else if too_large {
                Frame::TooLarge
            } else {
                Frame::Line
            });
        }
        let (chunk, found) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (&buf[..i], Some(i + 1)),
            None => (buf, None),
        };
        if !too_large {
            if out.len() + chunk.len() > MAX_FRAME {
                too_large = true;
                out.clear();
            } else {
                out.extend_from_slice(chunk);
            }
        }
        let used = found.unwrap_or(buf.len());
        r.consume(used);
        if found.is_some() {
            return Ok(if too_large {
                Frame::TooLarge
            } else {
                Frame::Line
            });
        }
    }
}

/// What the reader passes on.
enum Msg {
    Event(RuntimeEvent),
    Ready {
        protocol: u64,
    },
    /// A command was refused.
    Nack {
        request_id: String,
        error: String,
    },
    Fatal(String),
    End,
}

#[derive(Default)]
struct Shared {
    info: RunInfo,
    /// Open permission requests: id → (tool, input).
    asks: HashMap<String, (String, Value)>,
    /// The turn the worker is on.
    turn: Option<String>,
}

struct SdkRun {
    child: Child,
    stdin: Option<ChildStdin>,
    events: mpsc::Receiver<Msg>,
    shared: Arc<Mutex<Shared>>,
    reader: tokio::task::JoinHandle<()>,
    drain: tokio::task::JoinHandle<()>,
    tail: Arc<Mutex<String>>,
    next_request: u64,
    run_id: String,
    generation: u64,
    /// An event read while waiting for something else.
    pending: Option<RuntimeEvent>,
    /// The process group's leader, and whether it has been reaped.
    pid: Option<u32>,
    reaped: bool,
}

impl SdkRun {
    /// The run's process is gone or going (its output closed, or it was
    /// given its moment): end what it left in its group — before reaping
    /// it, while the group's id is still ours — then reap it.
    async fn reap(&mut self) -> Option<std::process::ExitStatus> {
        if self.reaped {
            return None;
        }
        if let Some(pid) = self.pid {
            crate::runtime::end_group(pid);
        }
        self.reaped = true;
        tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait())
            .await
            .ok()
            .and_then(Result::ok)
    }

    async fn command(&mut self, kind: &str, payload: Value) -> Result<String> {
        self.next_request += 1;
        let request_id = format!("otter_{}", self.next_request);
        let line = json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": request_id,
            "run_id": self.run_id,
            "generation": self.generation,
            "type": kind,
            "payload": payload,
        })
        .to_string();
        if line.len() > MAX_FRAME {
            bail!("the {kind} command is too large to send");
        }
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("the worker's input is closed"))?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(request_id)
    }

    async fn wait_ready(&mut self) -> Result<()> {
        loop {
            match self.events.recv().await {
                Some(Msg::Ready { protocol }) if protocol == PROTOCOL_VERSION => return Ok(()),
                Some(Msg::Ready { protocol }) => {
                    bail!("the Claude worker speaks protocol {protocol}, otterd {PROTOCOL_VERSION}")
                }
                Some(Msg::Fatal(e)) => bail!("the Claude worker failed to start: {e}"),
                Some(Msg::Nack { error, .. }) => {
                    bail!("the Claude worker refused to start: {error}")
                }
                Some(Msg::Event(e)) => self.pending = Some(e),
                Some(Msg::End) | None => bail!("the Claude worker exited before it was ready"),
            }
        }
    }

    fn stderr_tail(&self) -> String {
        let t = self.tail.lock().unwrap();
        let t = t.trim();
        if t.is_empty() {
            "no diagnostics".into()
        } else {
            format!("worker diagnostics: {}", super::excerpt(t, 1500))
        }
    }
}

#[async_trait]
impl RunHandle for SdkRun {
    async fn send_turn(&mut self, turn: &TurnSpec) -> Result<()> {
        {
            let mut s = self.shared.lock().unwrap();
            s.info.turns += 1;
            s.turn = Some(turn.turn_id.to_string());
        }
        self.command(
            "send_turn",
            json!({"turn_id": turn.turn_id, "text": turn.text}),
        )
        .await?;
        Ok(())
    }

    async fn next_event(&mut self) -> Option<RuntimeEvent> {
        if let Some(e) = self.pending.take() {
            return Some(e);
        }
        loop {
            match self.events.recv().await? {
                Msg::Event(e) => return Some(e),
                Msg::Ready { .. } => continue,
                Msg::Nack { request_id, error } => {
                    tracing::warn!(%request_id, "the Claude worker refused a command: {error}");
                    continue;
                }
                Msg::Fatal(e) => {
                    let tail = self.stderr_tail();
                    return Some(RuntimeEvent::Exited {
                        code: None,
                        error: Some(format!("{e} ({tail})")),
                    });
                }
                Msg::End => {
                    let status = self.reap().await;
                    self.shared.lock().unwrap().info.exited = true;
                    return Some(match status {
                        Some(s) => RuntimeEvent::Exited {
                            code: s.code(),
                            error: (!s.success()).then(|| {
                                format!(
                                    "the Claude worker exited with {s} ({})",
                                    self.stderr_tail()
                                )
                            }),
                        },
                        _ => RuntimeEvent::Exited {
                            code: None,
                            error: Some(
                                "the Claude worker closed its output but didn't exit".into(),
                            ),
                        },
                    });
                }
            }
        }
    }

    async fn decide(&mut self, request_id: &str, reply: DecisionReply) -> Result<()> {
        let (tool, input) = self
            .shared
            .lock()
            .unwrap()
            .asks
            .remove(request_id)
            .ok_or_else(|| anyhow!("no open decision `{request_id}`"))?;
        let resolution = resolution(&tool, &input, &reply);
        self.command(
            "resolve_interaction",
            json!({"request_id": request_id, "resolution": resolution}),
        )
        .await?;
        Ok(())
    }

    async fn check(&mut self, check_id: &str, decision: CheckDecision) -> Result<()> {
        let payload = match decision {
            CheckDecision::Allow => json!({"check_id": check_id, "decision": "allow"}),
            CheckDecision::Ask => json!({"check_id": check_id, "decision": "ask"}),
            CheckDecision::Deny { reason } => {
                json!({"check_id": check_id, "decision": "deny", "reason": reason})
            }
        };
        self.command("policy_reply", payload).await?;
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<()> {
        let turn = self.shared.lock().unwrap().turn.clone();
        let Some(turn) = turn else {
            bail!("no turn is running")
        };
        self.command("interrupt", json!({"turn_id": turn})).await?;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if self.stdin.is_some() {
            let _ = self.command("shutdown", json!({})).await;
        }
        // The worker ends its SDK session and leaves: give it a moment to
        // close its output, then end its group (what it left running) and
        // reap it.
        self.stdin = None;
        let _ = tokio::time::timeout(Duration::from_secs(8), async {
            while let Some(msg) = self.events.recv().await {
                if matches!(msg, Msg::End) {
                    break;
                }
            }
        })
        .await;
        self.reap().await;
        self.shared.lock().unwrap().info.exited = true;
        Ok(())
    }

    fn inspect(&self) -> RunInfo {
        let s = self.shared.lock().unwrap();
        let mut info = s.info.clone();
        info.pending = s.asks.keys().cloned().collect();
        info
    }
}

impl Drop for SdkRun {
    fn drop(&mut self) {
        self.reader.abort();
        self.drain.abort();
        // Dropped without being stopped (otterd going away): its group too.
        if !self.reaped
            && let Some(pid) = self.pid
        {
            crate::runtime::end_group(pid);
        }
    }
}

/// The worker's `resolution` for a reply.
fn resolution(tool: &str, input: &Value, reply: &DecisionReply) -> Value {
    match reply {
        DecisionReply::Allow => json!({"behavior": "allow"}),
        DecisionReply::Deny { message } => json!({"behavior": "deny", "message": message}),
        DecisionReply::Answers { answers } if tool == "AskUserQuestion" => {
            json!({"behavior": "answer", "answers": answers})
        }
        // One answer for a form: the same for each question (the feature's
        // single answer field; per-question answers come with the forms UI).
        DecisionReply::Answer { text } if tool == "AskUserQuestion" => {
            let answers: serde_json::Map<String, Value> = questions(input)
                .into_iter()
                .map(|q| (q.prompt, Value::String(text.clone())))
                .collect();
            json!({"behavior": "answer", "answers": answers})
        }
        DecisionReply::Answer { .. } | DecisionReply::Answers { .. } => {
            json!({"behavior": "allow"})
        }
    }
}

fn outcome(s: &str) -> TurnOutcome {
    match s {
        "completed" => TurnOutcome::Completed,
        "interrupted" => TurnOutcome::Interrupted,
        "cancelled" => TurnOutcome::Cancelled,
        "limit_reached" => TurnOutcome::LimitReached,
        "failed" => TurnOutcome::Failed,
        _ => TurnOutcome::OutcomeUnknown,
    }
}

fn category(s: &str) -> Option<ErrorCategory> {
    Some(match s {
        "auth_required" => ErrorCategory::AuthRequired,
        "provider_unavailable" => ErrorCategory::ProviderUnavailable,
        "rate_limited" => ErrorCategory::RateLimited,
        "resume_unavailable" => ErrorCategory::ResumeUnavailable,
        "protocol_incompatible" => ErrorCategory::ProtocolIncompatible,
        "worker_crashed" => ErrorCategory::WorkerCrashed,
        "storage_failed" => ErrorCategory::StorageFailed,
        "invalid_input" => ErrorCategory::InvalidInput,
        _ => return None,
    })
}

/// One frame from the worker.
fn parse(v: &Value, shared: &Mutex<Shared>) -> Vec<Msg> {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    let ev = |e: RuntimeEvent| vec![Msg::Event(e)];
    match v["type"].as_str().unwrap_or_default() {
        "ready" => vec![Msg::Ready {
            protocol: v["protocol_version"].as_u64().unwrap_or(0),
        }],
        "ack" => vec![],
        "nack" => vec![Msg::Nack {
            request_id: s("request_id"),
            error: s("error"),
        }],
        "fatal" => vec![Msg::Fatal(s("message"))],
        "notice" => {
            tracing::debug!(notice = %s("message"), "from the Claude worker");
            vec![]
        }
        "session" => {
            tracing::info!(model = %s("model"), claude_code = %s("claude_code_version"), auth_source = %s("auth_source"), "Claude session");
            shared.lock().unwrap().info.session_id = Some(s("session_id"));
            ev(RuntimeEvent::Session {
                id: s("session_id"),
            })
        }
        "delivered" => ev(RuntimeEvent::TurnDelivered),
        "text_delta" => ev(RuntimeEvent::TextDelta {
            message: s("message"),
            block: v["block"].as_u64().unwrap_or(0) as u32,
            text: s("text"),
        }),
        "text" => ev(RuntimeEvent::Text {
            message: s("message"),
            block: v["block"].as_u64().unwrap_or(0) as u32,
            text: s("text"),
        }),
        "tool_started" => ev(RuntimeEvent::ToolStarted {
            call: s("call"),
            parent: v["parent"].as_str().map(String::from),
            tool: s("tool"),
            input: tool_call(&s("tool"), &v["input"]),
        }),
        "tool_finished" => ev(RuntimeEvent::ToolFinished {
            call: s("call"),
            ok: v["ok"].as_bool().unwrap_or(false),
            output: s("output"),
        }),
        "policy_check" => ev(RuntimeEvent::ToolCheck {
            check_id: s("check_id"),
            tool: s("tool"),
            call: tool_call(&s("tool"), &v["input"]),
            tool_use_id: v["tool_use_id"].as_str().map(String::from),
        }),
        "permission_request" => {
            let tool = s("tool");
            let input = v["input"].clone();
            let call = tool_call(&tool, &input);
            let ask = DecisionAsk {
                request_id: s("request_id"),
                summary: summarize(&call, &tool),
                tool_use_id: v["tool_use_id"].as_str().map(String::from),
                input_hash: crate::runtime::input_hash(&input),
                questions: if tool == "AskUserQuestion" {
                    questions(&input)
                } else {
                    vec![]
                },
                tool: tool.clone(),
                call,
            };
            shared
                .lock()
                .unwrap()
                .asks
                .insert(ask.request_id.clone(), (tool, input));
            ev(RuntimeEvent::DecisionNeeded(ask))
        }
        "turn_finished" => {
            shared.lock().unwrap().turn = None;
            ev(RuntimeEvent::TurnFinished {
                outcome: outcome(&s("outcome")),
                summary: v["summary"].as_str().map(String::from),
                error: v["error"].as_str().and_then(category),
                usage: v["session_cost_usd"].as_f64().map(|c| Usage {
                    scope: UsageScope::SessionCumulative,
                    input_tokens: None,
                    output_tokens: None,
                    cost_usd: Some(c),
                }),
            })
        }
        other => {
            tracing::debug!(kind = other, "an unknown frame from the Claude worker");
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const FAKE: &str = include_str!("../../tests/fake_claude_worker.sh");

    struct Fake {
        _dir: tempfile::TempDir,
        env: EnvMap,
        log: PathBuf,
    }

    fn fake(mode: &str) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("main.js");
        std::fs::write(&worker, FAKE).unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.path().join("log");
        let mut env = EnvMap::new();
        env.insert("PATH".into(), "/usr/bin:/bin".into());
        env.insert("OTTER_NODE".into(), "/bin/sh".into());
        env.insert("OTTER_CLAUDE_WORKER".into(), worker.display().to_string());
        env.insert("FAKE_WORKER_MODE".into(), mode.into());
        env.insert("FAKE_WORKER_LOG".into(), log.display().to_string());
        env.insert("OTTER_WORKER_INIT_TIMEOUT_MS".into(), "3000".into());
        Fake {
            _dir: dir,
            env,
            log,
        }
    }

    fn spec(f: &Fake) -> RunSpec {
        RunSpec {
            conversation_id: "conv_1".into(),
            run_id: "run_1".into(),
            generation: 1,
            cwd: f._dir.path().to_path_buf(),
            env: f.env.clone(),
            resume: Some("s-1".into()),
            instructions: Some("be brief".into()),
            model: Some("sonnet".into()),
            limits: Default::default(),
        }
    }

    async fn next(h: &mut Box<dyn RunHandle>) -> RuntimeEvent {
        tokio::time::timeout(Duration::from_secs(10), h.next_event())
            .await
            .expect("an event in time")
            .expect("an event")
    }

    #[tokio::test]
    async fn a_turn_goes_through_the_worker_with_a_policy_check_and_a_form() {
        let f = fake("ok");
        let mut h = ClaudeSdkRuntime.start(spec(&f)).await.unwrap();
        h.send_turn(&TurnSpec {
            turn_id: "turn_1".into(),
            text: "export it".into(),
        })
        .await
        .unwrap();
        assert_eq!(
            next(&mut h).await,
            RuntimeEvent::Session { id: "s-1".into() }
        );
        assert_eq!(next(&mut h).await, RuntimeEvent::TurnDelivered);
        let RuntimeEvent::ToolCheck { check_id, call, .. } = next(&mut h).await else {
            panic!("a policy check first")
        };
        assert_eq!(
            call,
            crate::runtime::ToolCall::Command {
                line: "make test".into()
            }
        );
        h.check(&check_id, CheckDecision::Allow).await.unwrap();
        assert!(
            matches!(next(&mut h).await, RuntimeEvent::ToolStarted { call, .. } if call == "tu_1")
        );
        assert!(
            matches!(next(&mut h).await, RuntimeEvent::ToolFinished { call, ok: true, .. } if call == "tu_1")
        );
        let RuntimeEvent::DecisionNeeded(ask) = next(&mut h).await else {
            panic!("a question form")
        };
        assert_eq!(ask.questions.len(), 2);
        let answers = [("Which format?", "CSV"), ("Which columns?", "name, time")]
            .into_iter()
            .map(|(q, a)| (q.to_owned(), a.to_owned()))
            .collect();
        h.decide(&ask.request_id, DecisionReply::Answers { answers })
            .await
            .unwrap();
        assert!(
            matches!(next(&mut h).await, RuntimeEvent::TextDelta { message, block: 0, .. } if message == "m1")
        );
        assert!(matches!(next(&mut h).await, RuntimeEvent::Text { text, .. } if text == "Done."));
        assert!(matches!(
            next(&mut h).await,
            RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::Completed,
                usage: Some(Usage {
                    scope: UsageScope::SessionCumulative,
                    ..
                }),
                ..
            }
        ));
        h.stop().await.unwrap();
        let log = std::fs::read_to_string(&f.log).unwrap();
        // What the worker was told, in order, and nothing about the env on its command line.
        let kinds: Vec<&str> = log
            .lines()
            .filter_map(|l| l.split("\"type\":\"").nth(1)?.split('"').next())
            .collect();
        assert_eq!(
            kinds,
            [
                "initialize",
                "send_turn",
                "policy_reply",
                "resolve_interaction",
                "shutdown"
            ]
        );
        assert!(
            log.contains(r#""answers":{"Which columns?":"name, time","Which format?":"CSV"}"#),
            "{log}"
        );
        assert!(log.contains(r#""setting_sources":["project"]"#));
        assert!(log.contains(r#""resume":"s-1""#) && log.contains(r#""model":"sonnet""#));
    }

    #[tokio::test]
    async fn a_worker_that_never_gets_ready_is_given_up_on_with_its_diagnostics() {
        let f = fake("silent");
        let started = std::time::Instant::now();
        let err = ClaudeSdkRuntime
            .start(spec(&f))
            .await
            .err()
            .expect("not ready");
        assert!(format!("{err:#}").contains("wasn't ready"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_failing_worker_says_why() {
        let f = fake("fatal");
        let err = ClaudeSdkRuntime.start(spec(&f)).await.err().expect("fatal");
        let msg = format!("{err:#}");
        assert!(msg.contains("no credentials"), "{msg}");
        assert!(msg.contains("diagnostic line"), "stderr is kept: {msg}");
    }

    #[tokio::test]
    async fn oversized_frames_and_a_flood_of_diagnostics_dont_stall_the_run() {
        let f = fake("noisy");
        let mut h = ClaudeSdkRuntime.start(spec(&f)).await.unwrap();
        h.send_turn(&TurnSpec {
            turn_id: "turn_1".into(),
            text: "go".into(),
        })
        .await
        .unwrap();
        // The 2 MB frame is skipped; the turn still finishes.
        loop {
            match next(&mut h).await {
                RuntimeEvent::TurnFinished { outcome, .. } => {
                    assert_eq!(outcome, TurnOutcome::Completed);
                    break;
                }
                RuntimeEvent::Text { text, .. } => assert!(text.len() < MAX_FRAME),
                _ => {}
            }
        }
        h.stop().await.unwrap();
        assert!(h.inspect().exited);
    }

    #[tokio::test]
    async fn an_old_worker_protocol_is_refused_before_any_prompt() {
        let f = fake("old");
        let err = ClaudeSdkRuntime
            .start(spec(&f))
            .await
            .err()
            .expect("refused");
        assert!(format!("{err:#}").contains("protocol 0"), "{err:#}");
        let log = std::fs::read_to_string(&f.log).unwrap_or_default();
        assert!(!log.contains("send_turn"));
    }

    /// otterd's adapter, the real worker and real Claude, end to end:
    /// `OTTER_CLAUDE_WORKER=packages/claude-runtime cargo test -p otterd --bin
    /// otterd live_sdk -- --ignored` (uses this host's Claude sign-in).
    #[tokio::test]
    #[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
    async fn live_sdk() {
        let mut env: EnvMap = std::env::vars().collect();
        env.remove("ANTHROPIC_API_KEY");
        let dir = tempfile::tempdir().unwrap();
        let mut h = ClaudeSdkRuntime
            .start(RunSpec {
                conversation_id: "conv_live".into(),
                run_id: "run_live".into(),
                generation: 1,
                cwd: dir.path().to_path_buf(),
                env,
                resume: None,
                instructions: Some("Be brief.".into()),
                model: Some("haiku".into()),
                limits: crate::runtime::RunLimits {
                    max_turns: Some(4),
                    max_budget_usd: None,
                },
            })
            .await
            .unwrap();
        h.send_turn(&TurnSpec {
            turn_id: "turn_1".into(),
            text: "Run the shell command: echo otter-ok".into(),
        })
        .await
        .unwrap();
        let mut saw = (false, false, false);
        loop {
            match tokio::time::timeout(Duration::from_secs(120), h.next_event())
                .await
                .unwrap()
                .unwrap()
            {
                RuntimeEvent::Session { .. } => saw.0 = true,
                RuntimeEvent::ToolCheck { check_id, .. } => {
                    saw.1 = true;
                    h.check(&check_id, CheckDecision::Allow).await.unwrap();
                }
                RuntimeEvent::ToolFinished { ok, output, .. } => {
                    saw.2 = ok && output.contains("otter-ok")
                }
                RuntimeEvent::TurnFinished { outcome, .. } => {
                    assert_eq!(outcome, TurnOutcome::Completed);
                    break;
                }
                RuntimeEvent::Exited { error, .. } => panic!("exited: {error:?}"),
                _ => {}
            }
        }
        h.stop().await.unwrap();
        assert_eq!(
            saw,
            (true, true, true),
            "session, policy check, tool result"
        );
    }

    #[test]
    fn capabilities_say_whats_missing() {
        let env = EnvMap::from([("PATH".to_owned(), "/nonexistent".to_owned())]);
        let caps = ClaudeSdkRuntime.capabilities(&env);
        assert_eq!(caps.backend, "sdk");
        assert!(!caps.available);
        assert!(caps.notes.iter().any(|n| n.contains("Node.js")));
        assert!(!caps.structured_ready);
    }
}
