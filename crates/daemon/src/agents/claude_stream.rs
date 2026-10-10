//! Claude Code as a managed runtime (D-044, D-055): `claude -p` speaking
//! stream-json on stdin/stdout, with permission prompts sent to us. This is
//! the `legacy_cli` backend: it stays selectable while the SDK worker takes
//! over (the structured runtime plan, milestone 2).
//!
//! Observed on Claude Code 2.1.295 (probed, not from documentation alone):
//!
//! - **Launch:** `claude -p --input-format stream-json --output-format
//!   stream-json --verbose --permission-mode default --permission-prompt-tool
//!   stdio --setting-sources "" [--resume <session-id>]`. Without `--permission-prompt-tool
//!   stdio`, a prompt is denied on the spot (`system/permission_denied`).
//! - **Handshake:** we write `{"type":"control_request","request_id":…,
//!   "request":{"subtype":"initialize"}}`; it answers with a
//!   `control_response`. `system/init` then carries `session_id` and a
//!   `capabilities` list.
//! - **Turns:** we write `{"type":"user","message":{"role":"user","content":
//!   …}}`; it streams `assistant` messages (text, `tool_use` with an `id`),
//!   `user` messages (`tool_result` blocks: `tool_use_id`, `is_error`,
//!   `content`) and ends the turn with `result` (`subtype`, `is_error`,
//!   `session_id`, `total_cost_usd` — for the session so far — and `result`
//!   text). With `--include-partial-messages`, `stream_event`s come first:
//!   `message_start` (the message `id`) and `content_block_delta` (`index`,
//!   `text_delta`).
//! - **Decisions:** a tool that needs permission (commands Claude Code
//!   doesn't consider read-only, edits, `AskUserQuestion`, …) arrives as
//!   `control_request` `can_use_tool` {`tool_name`, `input`, `tool_use_id`};
//!   we answer `control_response` → `{"behavior":"allow","updatedInput":…}`
//!   or `{"behavior":"deny","message":…}`. A question is answered by allowing
//!   it with `updatedInput.answers = {<question>: <answer>}`.
//! - **Interrupt:** `control_request` `{"subtype":"interrupt"}`; the turn
//!   ends with `result` `error_during_execution`.
//!
//! The control messages are the Agent SDK's protocol, not a documented CLI
//! contract: everything here parses defensively, and the version it was
//! observed on is recorded. Commands Claude Code itself treats as read-only
//! (`echo`, `ls`) run without asking — its own rules come first, ours on top.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use otter_core::conversation::{
    ErrorCategory, Question, QuestionOption, TurnOutcome, Usage, UsageScope,
};
use otter_protocol::conversation::{RuntimeCapabilities, RuntimeFeatures};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use crate::env::which;
use crate::runtime::{
    AgentRuntime, DecisionAsk, DecisionReply, RunHandle, RunInfo, RunSpec, RuntimeEvent, ToolCall,
    TurnSpec,
};

/// The Claude Code version this protocol was observed on.
pub const OBSERVED_VERSION: &str = "2.1.295";
const SUMMARY_LEN: usize = 200;
/// A tool's output, as kept in the conversation.
const OUTPUT_LEN: usize = 2000;

pub struct ClaudeRuntime;

/// The command line for a run. Nothing secret goes here: the environment is
/// passed to the process directly, and prompts go over stdin.
pub fn argv(spec: &RunSpec) -> Vec<String> {
    let mut argv: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        "default",
        "--permission-prompt-tool",
        "stdio",
        // No settings files (user, project or local): an allow rule there
        // would let a tool run before Otter's policy sees it — and a
        // project's settings come with the repository (D-044).
        "--setting-sources",
        "",
        // Text as it is written, for streaming it to the developer (D-051).
        "--include-partial-messages",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(id) = &spec.resume {
        argv.push("--resume".into());
        argv.push(id.clone());
    }
    if let Some(model) = &spec.model {
        argv.push("--model".into());
        argv.push(model.clone());
    }
    if let Some(n) = spec.limits.max_turns {
        argv.push("--max-turns".into());
        argv.push(n.to_string());
    }
    if let Some(extra) = &spec.instructions {
        argv.push("--append-system-prompt".into());
        argv.push(extra.clone());
    }
    argv
}

#[async_trait]
impl AgentRuntime for ClaudeRuntime {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn capabilities(&self, env: &crate::env::EnvMap) -> RuntimeCapabilities {
        let available = which("claude", env).is_some();
        let mut notes = vec![format!(
            "Drives Claude Code's stream-json mode, observed on {OBSERVED_VERSION}: not a documented interface."
        )];
        if !available {
            notes.insert(
                0,
                "Claude Code (`claude`) isn't on this host's PATH.".into(),
            );
        }
        RuntimeCapabilities {
            provider: "claude".into(),
            backend: "legacy_cli".into(),
            available,
            notes,
            tested_version: Some(OBSERVED_VERSION.into()),
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
        let program = which("claude", &spec.env).ok_or_else(|| anyhow!("claude is not on PATH"))?;
        tracing::debug!(
            observed_on = OBSERVED_VERSION,
            conversation = %spec.conversation_id,
            run = %spec.run_id,
            generation = spec.generation,
            "starting claude -p (stream-json)"
        );
        let mut child = crate::env::spawn_tokio(
            tokio::process::Command::new(&program)
                .args(argv(&spec))
                .current_dir(&spec.cwd)
                .env_clear()
                .envs(&spec.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true),
        )
        .await
        .with_context(|| format!("starting {}", program.display()))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let pid = child.id();

        let shared = Arc::new(Mutex::new(Shared {
            info: RunInfo {
                pid,
                ..Default::default()
            },
            ..Default::default()
        }));
        let (tx, rx) = mpsc::channel(256);
        let reader_shared = shared.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                for ev in parse(&value, &reader_shared) {
                    if tx.send(ev).await.is_err() {
                        return;
                    }
                }
            }
            let _ = tx
                .send(RuntimeEvent::Exited {
                    code: None,
                    error: None,
                })
                .await;
        });

        let mut handle = ClaudeRun {
            child,
            stdin: Some(stdin),
            events: rx,
            shared,
            reader,
            next_request: 0,
        };
        let init = handle.request_id();
        handle
            .write(&json!({
                "type": "control_request",
                "request_id": init,
                "request": {"subtype": "initialize", "hooks": null},
            }))
            .await?;
        Ok(Box::new(handle))
    }
}

/// What the reader knows, shared with the handle.
#[derive(Default)]
struct Shared {
    info: RunInfo,
    /// Open decisions: request id → (tool name, its input).
    asks: HashMap<String, (String, Value)>,
    /// A turn was sent and the provider hasn't answered it yet.
    awaiting_delivery: bool,
    /// We asked the current turn to stop.
    interrupting: bool,
    /// The message being streamed (`message_start`), if it said its id.
    message: Option<String>,
    /// A text block streamed but not yet whole: (message, block).
    open: Option<(String, u32)>,
    /// Next block number per message, for blocks that weren't streamed.
    blocks: HashMap<String, u32>,
    /// For messages and tool calls without a provider id.
    synthetic: u64,
}

impl Shared {
    fn synthetic(&mut self, prefix: &str) -> String {
        self.synthetic += 1;
        format!("{prefix}{}", self.synthetic)
    }
}

struct ClaudeRun {
    child: Child,
    stdin: Option<ChildStdin>,
    events: mpsc::Receiver<RuntimeEvent>,
    shared: Arc<Mutex<Shared>>,
    reader: tokio::task::JoinHandle<()>,
    next_request: u64,
}

impl ClaudeRun {
    fn request_id(&mut self) -> String {
        self.next_request += 1;
        format!("otter_{}", self.next_request)
    }

    async fn write(&mut self, value: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("the run's input is closed"))?;
        let mut line = value.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

#[async_trait]
impl RunHandle for ClaudeRun {
    async fn send_turn(&mut self, turn: &TurnSpec) -> Result<()> {
        {
            let mut s = self.shared.lock().unwrap();
            s.info.turns += 1;
            s.awaiting_delivery = true;
            s.interrupting = false;
        }
        self.write(&json!({
            "type": "user",
            "session_id": "",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": turn.text},
        }))
        .await
    }

    async fn next_event(&mut self) -> Option<RuntimeEvent> {
        let ev = self.events.recv().await?;
        if let RuntimeEvent::Exited { .. } = ev {
            // The process closed its output: collect the exit status.
            let status =
                tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await;
            self.shared.lock().unwrap().info.exited = true;
            return Some(match status {
                Ok(Ok(s)) => RuntimeEvent::Exited {
                    code: s.code(),
                    error: (!s.success()).then(|| format!("claude exited with {s}")),
                },
                _ => RuntimeEvent::Exited {
                    code: None,
                    error: Some("claude closed its output but didn't exit".into()),
                },
            });
        }
        Some(ev)
    }

    async fn decide(&mut self, request_id: &str, reply: DecisionReply) -> Result<()> {
        let (tool, input) = self
            .shared
            .lock()
            .unwrap()
            .asks
            .remove(request_id)
            .ok_or_else(|| anyhow!("no open decision `{request_id}`"))?;
        let response = response_for(&tool, &input, &reply);
        self.write(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response},
        }))
        .await
    }

    async fn interrupt(&mut self) -> Result<()> {
        self.shared.lock().unwrap().interrupting = true;
        let id = self.request_id();
        self.write(&json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}}))
            .await
    }

    async fn stop(&mut self) -> Result<()> {
        if self.stdin.is_some() {
            let _ = self.interrupt().await;
        }
        // Closing input ends `claude -p`; give it a moment, then make sure.
        self.stdin = None;
        if tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
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

impl Drop for ClaudeRun {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// One structured answer from Claude, for the Control Agent (D-045):
/// `claude -p --output-format json --json-schema <schema> --tools ""`, the
/// prompt on stdin, the answer in the result's `structured_output`. No tools
/// and no settings files: it only thinks. `model`: default Claude Code's. With `read`, it may Read files there
/// (and nothing else): for looking at screenshots.
pub async fn structured_reading(
    env: &crate::env::EnvMap,
    cwd: &std::path::Path,
    prompt: &str,
    schema: &Value,
    read: Option<&std::path::Path>,
    model: Option<&str>,
) -> Result<Value> {
    let program = which("claude", env).ok_or_else(|| anyhow!("claude is not on PATH"))?;
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(["-p", "--output-format", "json", "--setting-sources", ""]);
    match read {
        Some(dir) => {
            cmd.args(["--tools", "Read", "--add-dir"]).arg(dir);
        }
        None => {
            cmd.args(["--tools", ""]);
        }
    }
    cmd.args(["--json-schema", &schema.to_string()]);
    if let Some(model) = model {
        cmd.args(["--model", model]);
    }
    cmd.current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = crate::env::spawn_tokio(&mut cmd)
        .await
        .with_context(|| format!("starting {}", program.display()))?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(prompt.as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow!("Otter took too long to answer"))??;
    let v: Value = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("reading claude's answer (exit {})", out.status))?;
    if v["is_error"] == true {
        anyhow::bail!(
            "claude failed: {}",
            v["result"].as_str().unwrap_or("no details")
        );
    }
    match v.get("structured_output") {
        Some(s) if !s.is_null() => Ok(s.clone()),
        _ => anyhow::bail!("claude gave no structured answer"),
    }
}

/// Plain text from Claude, streamed: `claude -p --output-format stream-json
/// --include-partial-messages`, no tools and no settings files. `say` gets
/// each piece as it's written; the whole text is returned (D-051).
pub async fn stream_text(
    env: &crate::env::EnvMap,
    cwd: &std::path::Path,
    prompt: &str,
    model: Option<&str>,
    say: &(dyn Fn(&str) + Send + Sync),
) -> Result<String> {
    let program = which("claude", env).ok_or_else(|| anyhow!("claude is not on PATH"))?;
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args([
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--tools",
        "",
        "--setting-sources",
        "",
    ]);
    if let Some(model) = model {
        cmd.args(["--model", model]);
    }
    cmd.current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = crate::env::spawn_tokio(&mut cmd)
        .await
        .with_context(|| format!("starting {}", program.display()))?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(prompt.as_bytes()).await?;
    drop(stdin);
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut text = String::new();
    let read = async {
        while let Some(line) = lines.next_line().await? {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match v["type"].as_str() {
                Some("stream_event") if v["event"]["delta"]["type"] == "text_delta" => {
                    if let Some(t) = v["event"]["delta"]["text"].as_str() {
                        text.push_str(t);
                        say(t);
                    }
                }
                Some("result") => {
                    if v["is_error"] == true {
                        anyhow::bail!(
                            "claude failed: {}",
                            v["result"].as_str().unwrap_or("no details")
                        );
                    }
                    // The final text, in case pieces went missing.
                    if let Some(r) = v["result"].as_str().filter(|r| r.len() > text.len()) {
                        say(&r[text.len().min(r.len())..]);
                        text = r.to_owned();
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        Ok(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(300), read)
        .await
        .map_err(|_| anyhow!("Otter took too long to answer"))??;
    let _ = child.wait().await;
    Ok(text)
}

/// The `control_response` body for a decision.
pub fn response_for(tool: &str, input: &Value, reply: &DecisionReply) -> Value {
    match reply {
        DecisionReply::Allow => json!({"behavior": "allow", "updatedInput": input}),
        DecisionReply::Deny { message } => {
            json!({"behavior": "deny", "message": message, "interrupt": false})
        }
        DecisionReply::Answer { text } if tool == "AskUserQuestion" => {
            let mut updated = input.clone();
            let answers: serde_json::Map<String, Value> = input["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|q| q["question"].as_str())
                .map(|q| (q.to_owned(), Value::String(text.clone())))
                .collect();
            updated["answers"] = Value::Object(answers);
            json!({"behavior": "allow", "updatedInput": updated})
        }
        // An answer to anything else (e.g. a plan) means go ahead, with the
        // answer passed on as the reason.
        DecisionReply::Answer { .. } => json!({"behavior": "allow", "updatedInput": input}),
    }
}

/// What a Claude Code tool call does.
pub fn tool_call(name: &str, input: &Value) -> ToolCall {
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_owned();
    match name {
        "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "TodoWrite" | "ToolSearch" | "Task"
        | "Agent" => ToolCall::Read,
        "Edit" | "Write" | "MultiEdit" => ToolCall::Edit {
            path: s("file_path"),
        },
        "NotebookEdit" => ToolCall::Edit {
            path: s("notebook_path"),
        },
        "Bash" => ToolCall::Command { line: s("command") },
        "WebFetch" => ToolCall::Fetch { url: s("url") },
        "WebSearch" => ToolCall::Fetch {
            url: format!("search: {}", s("query")),
        },
        "AskUserQuestion" => {
            let q = &input["questions"][0];
            ToolCall::Question {
                question: q["question"].as_str().unwrap_or_default().to_owned(),
                options: q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o["label"].as_str().map(String::from))
                    .collect(),
            }
        }
        "ExitPlanMode" => ToolCall::PlanApproval { plan: s("plan") },
        other => ToolCall::Other {
            name: other.to_owned(),
        },
    }
}

/// Every question of an `AskUserQuestion` form, as asked. Its questions
/// have no ids: a question's id is its text (which is also how Claude Code
/// takes the answers back).
pub fn questions(input: &Value) -> Vec<Question> {
    input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, q)| {
            let prompt = q["question"].as_str().unwrap_or_default().to_owned();
            Question {
                id: if prompt.is_empty() {
                    format!("q{}", i + 1)
                } else {
                    prompt.clone()
                },
                header: q["header"].as_str().map(String::from),
                prompt,
                options: q["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|o| {
                        Some(QuestionOption {
                            label: o["label"].as_str()?.to_owned(),
                            description: o["description"].as_str().map(String::from),
                        })
                    })
                    .collect(),
                multi_select: q["multiSelect"].as_bool().unwrap_or(false),
            }
        })
        .collect()
}

fn summarize(call: &ToolCall, tool: &str) -> String {
    let text = match call {
        ToolCall::Read => format!("{tool}: read"),
        ToolCall::Edit { path } => format!("Edit {path}"),
        ToolCall::Command { line } => format!("Run `{line}`"),
        ToolCall::Fetch { url } => format!("Fetch {url}"),
        ToolCall::Question { question, .. } => question.clone(),
        ToolCall::PlanApproval { plan } => format!("Approve the plan: {plan}"),
        ToolCall::Other { name } => format!("Use {name}"),
    };
    super::excerpt(&text, SUMMARY_LEN)
}

/// A tool result's text: a string, or text blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// How a `result` ends the turn.
fn outcome(v: &Value, interrupting: bool) -> (TurnOutcome, Option<ErrorCategory>) {
    let ok = v["is_error"] != true && v["subtype"] == "success";
    if ok {
        return (TurnOutcome::Completed, None);
    }
    match v["subtype"].as_str() {
        Some("error_during_execution") if interrupting => (TurnOutcome::Interrupted, None),
        Some(s) if s.starts_with("error_max_") => (TurnOutcome::LimitReached, None),
        _ => {
            let text = v["result"].as_str().unwrap_or_default().to_lowercase();
            let category = if text.contains("rate limit") {
                Some(ErrorCategory::RateLimited)
            } else if text.contains("login") || text.contains("api key") || text.contains("auth") {
                Some(ErrorCategory::AuthRequired)
            } else if text.contains("no conversation found") {
                Some(ErrorCategory::ResumeUnavailable)
            } else {
                None
            };
            (TurnOutcome::Failed, category)
        }
    }
}

/// Turn one stdout line into events, remembering open decisions and which
/// message and block the text belongs to.
fn parse(v: &Value, shared: &Mutex<Shared>) -> Vec<RuntimeEvent> {
    let mut out = Vec::new();
    let mut s = shared.lock().unwrap();
    // Anything the provider says after a turn was sent means it has it.
    let answering = matches!(
        v["type"].as_str(),
        Some("assistant" | "user" | "stream_event" | "result" | "control_request")
    );
    if answering && s.awaiting_delivery {
        s.awaiting_delivery = false;
        out.push(RuntimeEvent::TurnDelivered);
    }
    // A subagent's traffic isn't the main turn (lineage comes with the SDK).
    let main = v["parent_tool_use_id"].is_null();
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            if let Some(id) = v["session_id"].as_str() {
                s.info.session_id = Some(id.to_owned());
                out.push(RuntimeEvent::Session { id: id.to_owned() });
            }
        }
        Some("stream_event") if main => {
            let ev = &v["event"];
            match ev["type"].as_str() {
                Some("message_start") => {
                    s.message = ev["message"]["id"].as_str().map(String::from);
                }
                Some("content_block_delta") if ev["delta"]["type"] == "text_delta" => {
                    if let Some(t) = ev["delta"]["text"].as_str().filter(|t| !t.is_empty()) {
                        let message = match s.message.clone() {
                            Some(m) => m,
                            None => {
                                let m = s.synthetic("msg-");
                                s.message = Some(m.clone());
                                m
                            }
                        };
                        let block = ev["index"].as_u64().unwrap_or(0) as u32;
                        s.open = Some((message.clone(), block));
                        out.push(RuntimeEvent::TextDelta {
                            message,
                            block,
                            text: t.to_owned(),
                        });
                    }
                }
                _ => {}
            }
        }
        Some("assistant") if main => {
            let id = v["message"]["id"].as_str().map(String::from);
            for block in v["message"]["content"].as_array().into_iter().flatten() {
                match block["type"].as_str() {
                    Some("text") => {
                        let Some(t) = block["text"].as_str().filter(|t| !t.trim().is_empty())
                        else {
                            continue;
                        };
                        // The block that was being streamed, if it's this message's.
                        let streamed = s
                            .open
                            .take_if(|(m, _)| id.as_ref().is_none_or(|id| id == m));
                        let (message, block) = match streamed {
                            Some(key) => key,
                            None => {
                                let message = match id.clone() {
                                    Some(m) => m,
                                    None => s.synthetic("msg-"),
                                };
                                let n = s.blocks.entry(message.clone()).or_insert(0);
                                let block = 1000 + *n;
                                *n += 1;
                                (message, block)
                            }
                        };
                        // The next stream starts a new message.
                        s.message = None;
                        out.push(RuntimeEvent::Text {
                            message,
                            block,
                            text: t.to_owned(),
                        });
                    }
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or_default();
                        let call = match block["id"].as_str() {
                            Some(id) => id.to_owned(),
                            None => s.synthetic("tool-"),
                        };
                        out.push(RuntimeEvent::ToolStarted {
                            call,
                            parent: None,
                            tool: name.to_owned(),
                            input: tool_call(name, &block["input"]),
                        });
                    }
                    _ => {}
                }
            }
        }
        // Tool results come back as the user's side of the conversation.
        Some("user") if main => {
            for block in v["message"]["content"].as_array().into_iter().flatten() {
                if block["type"] != "tool_result" {
                    continue;
                }
                let Some(call) = block["tool_use_id"].as_str() else {
                    continue;
                };
                out.push(RuntimeEvent::ToolFinished {
                    call: call.to_owned(),
                    ok: block["is_error"] != true,
                    output: super::excerpt(&result_text(&block["content"]), OUTPUT_LEN),
                });
            }
        }
        Some("control_request") if v["request"]["subtype"] == "can_use_tool" => {
            let Some(request_id) = v["request_id"].as_str() else {
                return out;
            };
            let tool = v["request"]["tool_name"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let input = v["request"]["input"].clone();
            let call = tool_call(&tool, &input);
            let summary = summarize(&call, &tool);
            let questions = if tool == "AskUserQuestion" {
                questions(&input)
            } else {
                vec![]
            };
            let input_hash = crate::runtime::input_hash(&input);
            s.asks.insert(request_id.to_owned(), (tool.clone(), input));
            out.push(RuntimeEvent::DecisionNeeded(DecisionAsk {
                request_id: request_id.to_owned(),
                tool,
                call,
                summary,
                tool_use_id: v["request"]["tool_use_id"].as_str().map(String::from),
                input_hash,
                questions,
            }));
        }
        Some("result") => {
            if let Some(id) = v["session_id"].as_str() {
                s.info.session_id = Some(id.to_owned());
            }
            let (outcome, error) = outcome(v, s.interrupting);
            s.interrupting = false;
            s.open = None;
            s.message = None;
            out.push(RuntimeEvent::TurnFinished {
                outcome,
                summary: v["result"].as_str().map(|r| super::excerpt(r, 2000)),
                error,
                // Claude Code reports the session's cost so far, not the turn's.
                usage: v["total_cost_usd"].as_f64().map(|c| Usage {
                    scope: UsageScope::SessionCumulative,
                    input_tokens: v["usage"]["input_tokens"].as_u64(),
                    output_tokens: v["usage"]["output_tokens"].as_u64(),
                    cost_usd: Some(c),
                }),
            });
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Mutex<Shared> {
        Mutex::new(Shared::default())
    }

    fn spec() -> RunSpec {
        RunSpec {
            conversation_id: "conv_1".into(),
            run_id: "run_1".into(),
            generation: 1,
            cwd: "/w".into(),
            env: Default::default(),
            resume: Some("abc".into()),
            instructions: None,
            model: Some("sonnet".into()),
            limits: crate::runtime::RunLimits {
                max_turns: Some(30),
                max_budget_usd: None,
            },
        }
    }

    #[test]
    fn launch_routes_prompts_to_us_and_resumes_by_id() {
        let a = argv(&spec());
        let has = |pair: [&str; 2]| a.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1]);
        assert!(has(["--permission-prompt-tool", "stdio"]));
        assert!(has(["--input-format", "stream-json"]));
        assert!(has(["--resume", "abc"]));
        assert!(has(["--setting-sources", ""]));
        assert!(has(["--model", "sonnet"]));
        assert!(has(["--max-turns", "30"]));
        assert!(a.iter().any(|x| x == "--include-partial-messages"));
    }

    #[test]
    fn capabilities_say_what_this_backend_is() {
        let caps = ClaudeRuntime.capabilities(&Default::default());
        assert_eq!(caps.backend, "legacy_cli");
        assert_eq!(caps.tested_version.as_deref(), Some(OBSERVED_VERSION));
        assert!(!caps.structured_ready);
        assert!(caps.features.tool_results && !caps.features.attachments);
    }

    #[test]
    fn parses_the_observed_messages() {
        let s = shared();
        let init = json!({"type":"system","subtype":"init","session_id":"s-1","capabilities":["interrupt_receipt_v1"]});
        assert_eq!(
            parse(&init, &s),
            vec![RuntimeEvent::Session { id: "s-1".into() }]
        );

        let ask = json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch x"},"tool_use_id":"toolu_1"}});
        let ev = parse(&ask, &s);
        assert!(matches!(&ev[0], RuntimeEvent::DecisionNeeded(d)
            if d.request_id == "r1" && d.call == ToolCall::Command { line: "touch x".into() }
                && d.summary == "Run `touch x`" && d.tool_use_id.as_deref() == Some("toolu_1")
                && d.input_hash == crate::runtime::input_hash(&json!({"command":"touch x"}))));
        assert!(s.lock().unwrap().asks.contains_key("r1"));

        // Every question of a form, with its options, header and multi-select.
        let q = json!({"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[
            {"question":"Pick one","header":"Colour","options":[{"label":"red","description":"warm"},{"label":"blue"}],"multiSelect":false},
            {"question":"Which sizes?","options":[{"label":"S"},{"label":"M"}],"multiSelect":true}]}}});
        let ev = parse(&q, &s);
        let RuntimeEvent::DecisionNeeded(d) = &ev[0] else {
            panic!("a question")
        };
        assert_eq!(d.questions.len(), 2);
        assert_eq!(d.questions[0].header.as_deref(), Some("Colour"));
        assert_eq!(
            d.questions[0].options[0].description.as_deref(),
            Some("warm")
        );
        assert!(d.questions[1].multi_select);
        assert_eq!(d.questions[1].id, "Which sizes?");

        let done = json!({"type":"result","subtype":"success","is_error":false,"session_id":"s-1","total_cost_usd":0.003,"result":"blue"});
        assert_eq!(
            parse(&done, &s),
            vec![RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::Completed,
                summary: Some("blue".into()),
                error: None,
                usage: Some(Usage {
                    scope: UsageScope::SessionCumulative,
                    input_tokens: None,
                    output_tokens: None,
                    cost_usd: Some(0.003),
                }),
            }]
        );
        let failed = json!({"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s-1"});
        assert!(matches!(
            parse(&failed, &s)[0],
            RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::Failed,
                ..
            }
        ));
        s.lock().unwrap().interrupting = true;
        assert!(matches!(
            parse(&failed, &s)[0],
            RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::Interrupted,
                ..
            }
        ));
        let limit = json!({"type":"result","subtype":"error_max_turns","is_error":true});
        assert!(matches!(
            parse(&limit, &s)[0],
            RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::LimitReached,
                ..
            }
        ));
        // Unknown messages are ignored.
        assert!(parse(&json!({"type":"rate_limit_event"}), &s).is_empty());
        let sub = json!({"type":"stream_event","parent_tool_use_id":"toolu_9","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}});
        assert!(
            parse(&sub, &s).is_empty(),
            "a subagent's text isn't the main turn"
        );
    }

    #[test]
    fn streamed_and_whole_text_share_one_message_and_block() {
        let s = shared();
        s.lock().unwrap().awaiting_delivery = true;
        let start = json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"id":"msg_A"}}});
        assert_eq!(parse(&start, &s), vec![RuntimeEvent::TurnDelivered]);
        let delta = |t: &str| json!({"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":t}}});
        assert_eq!(
            parse(&delta("Hel"), &s),
            vec![RuntimeEvent::TextDelta {
                message: "msg_A".into(),
                block: 1,
                text: "Hel".into()
            }]
        );
        parse(&delta("lo"), &s);
        let whole = json!({"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg_A","role":"assistant","content":[{"type":"text","text":"Hello"}]}});
        assert_eq!(
            parse(&whole, &s),
            vec![RuntimeEvent::Text {
                message: "msg_A".into(),
                block: 1,
                text: "Hello".into()
            }]
        );
        // Without ids (older output): a key of its own, still one per block.
        let bare = json!({"type":"assistant","parent_tool_use_id":null,"message":{"role":"assistant","content":[{"type":"text","text":"x"}]}});
        let RuntimeEvent::Text { message, .. } = &parse(&bare, &s)[0] else {
            panic!("text")
        };
        assert!(message.starts_with("msg-"));
    }

    #[test]
    fn tool_calls_keep_their_ids_and_results() {
        let s = shared();
        let call = json!({"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg_B","content":[{"type":"tool_use","id":"toolu_7","name":"Bash","input":{"command":"make test"}}]}});
        assert_eq!(
            parse(&call, &s),
            vec![RuntimeEvent::ToolStarted {
                call: "toolu_7".into(),
                parent: None,
                tool: "Bash".into(),
                input: ToolCall::Command {
                    line: "make test".into()
                }
            }]
        );
        let result = json!({"type":"user","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_7","is_error":true,"content":[{"type":"text","text":"1 failed"}]}]}});
        assert_eq!(
            parse(&result, &s),
            vec![RuntimeEvent::ToolFinished {
                call: "toolu_7".into(),
                ok: false,
                output: "1 failed".into()
            }]
        );
    }

    #[test]
    fn answers_take_the_observed_shapes() {
        let input = json!({"questions":[{"question":"Pick one","options":[]}]});
        assert_eq!(
            response_for(
                "AskUserQuestion",
                &input,
                &DecisionReply::Answer {
                    text: "blue".into()
                }
            ),
            json!({"behavior":"allow","updatedInput":{"questions":[{"question":"Pick one","options":[]}],"answers":{"Pick one":"blue"}}})
        );
        let cmd = json!({"command":"touch x"});
        assert_eq!(
            response_for("Bash", &cmd, &DecisionReply::Allow),
            json!({"behavior":"allow","updatedInput":{"command":"touch x"}})
        );
        assert_eq!(
            response_for(
                "Bash",
                &cmd,
                &DecisionReply::Deny {
                    message: "no".into()
                }
            ),
            json!({"behavior":"deny","message":"no","interrupt":false})
        );
    }

    #[test]
    fn tool_calls_map_to_generic_kinds() {
        assert_eq!(tool_call("Grep", &json!({})), ToolCall::Read);
        assert_eq!(
            tool_call("Write", &json!({"file_path":"/w/a.rs"})),
            ToolCall::Edit {
                path: "/w/a.rs".into()
            }
        );
        assert_eq!(
            tool_call("ExitPlanMode", &json!({"plan":"1. x"})),
            ToolCall::PlanApproval {
                plan: "1. x".into()
            }
        );
        assert_eq!(
            tool_call("mcp__x__y", &json!({})),
            ToolCall::Other {
                name: "mcp__x__y".into()
            }
        );
    }
}

/// The adapter against a real process: the fake `claude` in
/// `tests/fake_claude_stream.sh`, which plays the observed protocol.
#[cfg(test)]
mod process_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const FAKE: &str = include_str!("../../tests/fake_claude_stream.sh");

    async fn start(prompt: &str, resume: Option<&str>) -> (tempfile::TempDir, Box<dyn RunHandle>) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = crate::env::EnvMap::new();
        env.insert(
            "PATH".into(),
            format!("{}:/usr/bin:/bin", dir.path().display()),
        );
        let mut handle = ClaudeRuntime
            .start(RunSpec {
                conversation_id: "conv_1".into(),
                run_id: "run_1".into(),
                generation: 1,
                cwd: dir.path().to_path_buf(),
                env,
                resume: resume.map(String::from),
                instructions: Some("be brief".into()),
                model: None,
                limits: Default::default(),
            })
            .await
            .unwrap();
        handle
            .send_turn(&TurnSpec {
                turn_id: "turn_1".into(),
                text: prompt.into(),
            })
            .await
            .unwrap();
        (dir, handle)
    }

    async fn next(h: &mut Box<dyn RunHandle>) -> RuntimeEvent {
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(10), h.next_event())
                .await
                .expect("an event in time")
                .expect("an event");
            if ev != RuntimeEvent::TurnDelivered {
                return ev;
            }
        }
    }

    #[tokio::test]
    async fn a_decision_goes_out_and_the_reply_comes_back() {
        let (_dir, mut h) = start("please ASK_RM", None).await;
        let RuntimeEvent::Session { id } = next(&mut h).await else {
            panic!("session first")
        };
        assert!(id.starts_with("fake-session-"));
        let RuntimeEvent::DecisionNeeded(ask) = next(&mut h).await else {
            panic!("a decision")
        };
        assert_eq!(
            ask.call,
            ToolCall::Command {
                line: "rm -rf build".into()
            }
        );
        assert_eq!(h.inspect().pending, vec![ask.request_id.clone()]);
        h.decide(
            &ask.request_id,
            DecisionReply::Deny {
                message: "no".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::TurnFinished { outcome: TurnOutcome::Completed, summary: Some(s), .. } if s == "denied, so I stopped"));
        assert!(h.inspect().pending.is_empty());
        // A decision can't be answered twice.
        assert!(
            h.decide(&ask.request_id, DecisionReply::Allow)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_question_is_answered_with_the_chosen_option() {
        let (_dir, mut h) = start("ASK_QUESTION", None).await;
        next(&mut h).await;
        let RuntimeEvent::DecisionNeeded(ask) = next(&mut h).await else {
            panic!("a question")
        };
        h.decide(
            &ask.request_id,
            DecisionReply::Answer {
                text: "blue".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::Text { text, .. } if text == "Going with that."));
        assert!(matches!(
            next(&mut h).await,
            RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::Completed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn interrupt_settles_the_turn_and_stop_ends_the_process() {
        let (_dir, mut h) = start("HANG", None).await;
        next(&mut h).await;
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::Text { text, .. } if text == "working on it"));
        h.interrupt().await.unwrap();
        assert!(matches!(
            next(&mut h).await,
            RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::Interrupted,
                ..
            }
        ));
        // The process is still there for another turn.
        assert!(!h.inspect().exited);
        h.stop().await.unwrap();
        assert!(h.inspect().exited);
        let mut exited = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), h.next_event()).await
        {
            if let RuntimeEvent::Exited { .. } = ev {
                exited = true;
                break;
            }
        }
        assert!(exited);
    }

    #[tokio::test]
    async fn a_resumed_run_continues_the_conversation() {
        let (_dir, mut h) = start("carry on", Some("conv-42")).await;
        assert_eq!(
            next(&mut h).await,
            RuntimeEvent::Session {
                id: "conv-42".into()
            }
        );
        // It ran something (and said how it went), said something, and finished.
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::ToolStarted { call, tool, input: ToolCall::Command { line }, .. }
                if call == "toolu_mt" && tool == "Bash" && line == "make test"));
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::ToolFinished { call, ok: true, output } if call == "toolu_mt" && output == "ok"));
        // Written in pieces, then whole — one message, one block.
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::TextDelta { message, block: 0, text } if message == "msg_fake" && text == "Did "));
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::TextDelta { text, .. } if text == "the work."));
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::Text { message, block: 0, text } if message == "msg_fake" && text == "Did the work."));
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::TurnFinished { outcome: TurnOutcome::Completed, summary: Some(s), .. } if s == "done in conv-42"));
        assert_eq!(h.inspect().session_id.as_deref(), Some("conv-42"));
    }
}
