//! Claude Code as a managed runtime (D-044): `claude -p` speaking
//! stream-json on stdin/stdout, with permission prompts sent to us.
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
//!   …}}`; it streams `assistant` messages (text, `tool_use`), `user`
//!   messages (tool results) and ends the turn with `result` (`subtype`,
//!   `is_error`, `session_id`, `total_cost_usd`, `result` text).
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
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

use crate::env::which;
use crate::runtime::{
    AgentRuntime, DecisionAsk, DecisionReply, RunHandle, RunInfo, RunSpec, RuntimeEvent, ToolCall,
};

/// The Claude Code version this protocol was observed on.
pub const OBSERVED_VERSION: &str = "2.1.295";
const SUMMARY_LEN: usize = 200;

pub struct ClaudeRuntime;

/// The command line for a run. Nothing secret goes here: the environment is
/// passed to the process directly.
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
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(id) = &spec.resume {
        argv.push("--resume".into());
        argv.push(id.clone());
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

    async fn start(&self, spec: RunSpec) -> Result<Box<dyn RunHandle>> {
        let program = which("claude", &spec.env).ok_or_else(|| anyhow!("claude is not on PATH"))?;
        tracing::debug!(
            observed_on = OBSERVED_VERSION,
            "starting claude -p (stream-json)"
        );
        let mut child = tokio::process::Command::new(&program)
            .args(argv(&spec))
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let pid = child.id();

        let shared = Arc::new(Mutex::new(Shared {
            info: RunInfo {
                pid,
                ..Default::default()
            },
            asks: HashMap::new(),
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
        if !spec.prompt.trim().is_empty() {
            handle.send_input(&spec.prompt).await?;
        }
        Ok(Box::new(handle))
    }
}

struct Shared {
    info: RunInfo,
    /// Open decisions: request id → (tool name, its input).
    asks: HashMap<String, (String, Value)>,
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
    async fn send_input(&mut self, text: &str) -> Result<()> {
        self.shared.lock().unwrap().info.turns += 1;
        self.write(&json!({
            "type": "user",
            "session_id": "",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": text},
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

    async fn cancel(&mut self) -> Result<()> {
        if self.stdin.is_some() {
            let id = self.request_id();
            let _ = self
                .write(&json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}}))
                .await;
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
/// and no settings files: it only thinks. `OTTER_CONTROLLER_MODEL` picks the
/// model (default: Claude Code's). With `read`, it may Read files there
/// (and nothing else): for looking at screenshots.
pub async fn structured_reading(
    env: &crate::env::EnvMap,
    cwd: &std::path::Path,
    prompt: &str,
    schema: &Value,
    read: Option<&std::path::Path>,
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
    if let Ok(model) = std::env::var("OTTER_CONTROLLER_MODEL") {
        cmd.args(["--model", &model]);
    }
    let mut child = cmd
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(prompt.as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow!("the Control Agent took too long to answer"))??;
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

/// Turn one stdout line into events, remembering open decisions.
fn parse(v: &Value, shared: &Mutex<Shared>) -> Vec<RuntimeEvent> {
    let mut out = Vec::new();
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            if let Some(id) = v["session_id"].as_str() {
                shared.lock().unwrap().info.session_id = Some(id.to_owned());
                out.push(RuntimeEvent::Session { id: id.to_owned() });
            }
        }
        Some("assistant") => {
            // Subagent traffic isn't the main turn.
            if !v["parent_tool_use_id"].is_null() {
                return out;
            }
            for block in v["message"]["content"].as_array().into_iter().flatten() {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = block["text"].as_str().filter(|t| !t.trim().is_empty()) {
                            out.push(RuntimeEvent::Text { text: t.to_owned() });
                        }
                    }
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or_default();
                        out.push(RuntimeEvent::Tool {
                            tool: name.to_owned(),
                            call: tool_call(name, &block["input"]),
                        });
                    }
                    _ => {}
                }
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
            shared
                .lock()
                .unwrap()
                .asks
                .insert(request_id.to_owned(), (tool.clone(), input));
            out.push(RuntimeEvent::DecisionNeeded(DecisionAsk {
                request_id: request_id.to_owned(),
                tool,
                call,
                summary,
            }));
        }
        Some("result") => {
            if let Some(id) = v["session_id"].as_str() {
                shared.lock().unwrap().info.session_id = Some(id.to_owned());
            }
            out.push(RuntimeEvent::TurnEnded {
                ok: v["is_error"] != true && v["subtype"] == "success",
                summary: v["result"].as_str().map(|r| super::excerpt(r, 2000)),
                cost_usd: v["total_cost_usd"].as_f64(),
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
        Mutex::new(Shared {
            info: RunInfo::default(),
            asks: HashMap::new(),
        })
    }

    #[test]
    fn launch_routes_prompts_to_us_and_resumes_by_id() {
        let spec = RunSpec {
            cwd: "/w".into(),
            env: Default::default(),
            prompt: "do it".into(),
            resume: Some("abc".into()),
            instructions: None,
        };
        let a = argv(&spec);
        let has = |pair: [&str; 2]| a.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1]);
        assert!(has(["--permission-prompt-tool", "stdio"]));
        assert!(has(["--input-format", "stream-json"]));
        assert!(has(["--resume", "abc"]));
        assert!(has(["--setting-sources", ""]));
        // The prompt goes over stdin, never on the command line.
        assert!(!a.iter().any(|x| x.contains("do it")));
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
            if d.request_id == "r1" && d.call == ToolCall::Command { line: "touch x".into() } && d.summary == "Run `touch x`"));
        assert!(s.lock().unwrap().asks.contains_key("r1"));

        let q = json!({"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Pick one","header":"Colour","options":[{"label":"red"},{"label":"blue"}],"multiSelect":false}]}}});
        let ev = parse(&q, &s);
        assert!(matches!(&ev[0], RuntimeEvent::DecisionNeeded(d)
            if d.call == ToolCall::Question { question: "Pick one".into(), options: vec!["red".into(), "blue".into()] }));

        let done = json!({"type":"result","subtype":"success","is_error":false,"session_id":"s-1","total_cost_usd":0.003,"result":"blue"});
        assert_eq!(
            parse(&done, &s),
            vec![RuntimeEvent::TurnEnded {
                ok: true,
                summary: Some("blue".into()),
                cost_usd: Some(0.003)
            }]
        );
        let interrupted = json!({"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s-1"});
        assert!(matches!(
            parse(&interrupted, &s)[0],
            RuntimeEvent::TurnEnded { ok: false, .. }
        ));
        // Unknown messages are ignored.
        assert!(parse(&json!({"type":"rate_limit_event"}), &s).is_empty());
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
        let handle = ClaudeRuntime
            .start(RunSpec {
                cwd: dir.path().to_path_buf(),
                env,
                prompt: prompt.into(),
                resume: resume.map(String::from),
                instructions: Some("be brief".into()),
            })
            .await
            .unwrap();
        (dir, handle)
    }

    async fn next(h: &mut Box<dyn RunHandle>) -> RuntimeEvent {
        tokio::time::timeout(std::time::Duration::from_secs(10), h.next_event())
            .await
            .expect("an event in time")
            .expect("an event")
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
        assert_eq!(
            next(&mut h).await,
            RuntimeEvent::TurnEnded {
                ok: true,
                summary: Some("denied, so I stopped".into()),
                cost_usd: Some(0.01)
            }
        );
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
        assert_eq!(
            next(&mut h).await,
            RuntimeEvent::Text {
                text: "Going with that.".into()
            }
        );
        assert!(matches!(
            next(&mut h).await,
            RuntimeEvent::TurnEnded { ok: true, .. }
        ));
    }

    #[tokio::test]
    async fn cancel_interrupts_and_ends_the_process() {
        let (_dir, mut h) = start("HANG", None).await;
        next(&mut h).await;
        assert_eq!(
            next(&mut h).await,
            RuntimeEvent::Text {
                text: "working on it".into()
            }
        );
        h.cancel().await.unwrap();
        assert!(h.inspect().exited);
        // The interrupted turn ends, then the output closes.
        let mut saw_end = false;
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), h.next_event()).await
        {
            if let RuntimeEvent::TurnEnded { ok, .. } = ev {
                assert!(!ok);
                saw_end = true;
            }
            if let RuntimeEvent::Exited { .. } = ev {
                break;
            }
        }
        assert!(saw_end);
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
        next(&mut h).await;
        assert!(matches!(next(&mut h).await,
            RuntimeEvent::TurnEnded { ok: true, summary: Some(s), .. } if s == "done in conv-42"));
        assert_eq!(h.inspect().session_id.as_deref(), Some("conv-42"));
    }
}
