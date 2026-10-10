//! Managed agent runtimes (D-044, D-055): a coding agent driven through a
//! structured interface — events in, decisions out — instead of a terminal
//! someone watches. Interactive sessions (tmux, `agents::*`) stay as they
//! are.
//!
//! The contract, generic over agents:
//!
//! - [`AgentRuntime::start`] starts a run of a conversation (fresh, or
//!   resuming the provider's session); [`AgentRuntime::capabilities`] says
//!   what it can do, plainly;
//! - [`RunHandle`]: `send_turn`, `next_event` (the event stream), `decide`
//!   (answer a [`RuntimeEvent::DecisionNeeded`]), `interrupt` (the current
//!   turn only), `stop` (end the process), `inspect`.
//!
//! Events carry the provider's own keys for messages, blocks and tool
//! calls; the [`actor`] turns them into Otter ids and conversation state.
//! An adapter translates its agent's tool calls into a [`ToolCall`], so the
//! [`policy`] that decides what may run without asking is the same for every
//! agent. Agent-specific code (flags, wire format) lives under `agents/`.

pub mod actor;
pub mod conversations;
pub mod policy;

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use otter_core::conversation::{ErrorCategory, Question, TurnOutcome, Usage};
use otter_core::{ConversationId, RunId, TurnId};
use otter_protocol::conversation::RuntimeCapabilities;
use serde::{Deserialize, Serialize};

use crate::env::EnvMap;

/// One turn's input.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnSpec {
    pub turn_id: TurnId,
    pub text: String,
}

/// Limits the provider enforces on a run, if it can.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunLimits {
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
}

/// What a run is asked to do.
pub struct RunSpec {
    pub conversation_id: ConversationId,
    pub run_id: RunId,
    /// The conversation's generation this run serves.
    pub generation: u64,
    /// Working directory: the workspace root.
    pub cwd: PathBuf,
    /// The environment the agent runs with (login + workspace env). Passed
    /// to the process, never on its command line.
    pub env: EnvMap,
    /// Resume this conversation (the provider's own session id), if set.
    pub resume: Option<String>,
    /// Extra instructions for the agent's system prompt.
    pub instructions: Option<String>,
    /// The coding model (`None`: the provider's default).
    pub model: Option<String>,
    pub limits: RunLimits,
}

/// What a tool call does, in terms the policy understands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCall {
    /// Reads files or searches; changes nothing.
    Read,
    /// Writes or edits a file.
    Edit { path: String },
    /// Runs a shell command line.
    Command { line: String },
    /// Fetches from the network.
    Fetch { url: String },
    /// Asks the developer a question.
    Question {
        question: String,
        #[serde(default)]
        options: Vec<String>,
    },
    /// Asks to leave planning and start on a plan.
    PlanApproval { plan: String },
    /// Anything else (e.g. an MCP tool), by name.
    Other { name: String },
}

impl ToolCall {
    pub fn effect(&self) -> otter_core::conversation::ToolEffect {
        use otter_core::conversation::ToolEffect as E;
        match self {
            ToolCall::Read => E::Read,
            ToolCall::Edit { .. } => E::Edit,
            ToolCall::Command { .. } => E::Command,
            ToolCall::Fetch { .. } => E::Fetch,
            ToolCall::Question { .. } => E::Question,
            ToolCall::PlanApproval { .. } => E::Plan,
            ToolCall::Other { .. } => E::Other,
        }
    }
}

/// A decision the agent is waiting for.
#[derive(Clone, Debug, PartialEq)]
pub struct DecisionAsk {
    /// The runtime's id for this request; echo it in [`RunHandle::decide`].
    pub request_id: String,
    /// The agent's tool name, as it calls it.
    pub tool: String,
    pub call: ToolCall,
    /// One line describing what would happen.
    pub summary: String,
    /// The provider's id of the tool call it is about, if it says.
    pub tool_use_id: Option<String>,
    /// A hash of the exact input: an answer is for this input only.
    pub input_hash: String,
    /// For a question form: every question, as asked.
    pub questions: Vec<Question>,
}

/// The answer to a [`DecisionAsk`].
#[derive(Clone, Debug, PartialEq)]
pub enum DecisionReply {
    Allow,
    Deny {
        message: String,
    },
    /// One answer for the whole form (the chosen option or free text).
    Answer {
        text: String,
    },
    /// An answer per question, by question id (multi-select: comma-joined).
    Answers {
        answers: std::collections::BTreeMap<String, String>,
    },
}

/// Otter's policy for one tool call, before it runs ([`RuntimeEvent::ToolCheck`]).
#[derive(Clone, Debug, PartialEq)]
pub enum CheckDecision {
    Allow,
    Deny {
        reason: String,
    },
    /// Someone has to decide: the runtime asks with a
    /// [`RuntimeEvent::DecisionNeeded`].
    Ask,
}

/// What the runtime observed, in order. Keys are the provider's own
/// (`message`, `call`), stable within a run.
#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeEvent {
    /// The provider's conversation id, to resume it later.
    Session {
        id: String,
    },
    /// The provider began answering the turn just sent: it has the input.
    TurnDelivered,
    /// A piece of a text block being written (before its [`Self::Text`]).
    TextDelta {
        message: String,
        block: u32,
        text: String,
    },
    /// A whole text block.
    Text {
        message: String,
        block: u32,
        text: String,
    },
    /// A tool is called (it runs if allowed).
    ToolStarted {
        call: String,
        parent: Option<String>,
        tool: String,
        input: ToolCall,
    },
    /// A tool's reported result.
    ToolFinished {
        call: String,
        ok: bool,
        output: String,
    },
    DecisionNeeded(DecisionAsk),
    /// Every tool call, before it runs: answer with [`RunHandle::check`].
    /// (Runtimes with a mandatory pre-tool hook; others only ask through
    /// [`Self::DecisionNeeded`].)
    ToolCheck {
        check_id: String,
        tool: String,
        call: ToolCall,
        tool_use_id: Option<String>,
    },
    /// The turn settled; the agent waits for input.
    TurnFinished {
        outcome: TurnOutcome,
        summary: Option<String>,
        error: Option<ErrorCategory>,
        usage: Option<Usage>,
    },
    /// The process is gone.
    Exited {
        code: Option<i32>,
        error: Option<String>,
    },
}

/// What a run looks like right now.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RunInfo {
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    pub turns: u32,
    /// Decisions waiting for an answer.
    pub pending: Vec<String>,
    pub exited: bool,
}

#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Stable id, e.g. `claude`.
    fn id(&self) -> &'static str;
    /// What it is and can do on this host.
    fn capabilities(&self, env: &EnvMap) -> RuntimeCapabilities;
    /// Start a run (resuming `spec.resume` if set). No turn yet.
    async fn start(&self, spec: RunSpec) -> Result<Box<dyn RunHandle>>;
}

#[async_trait]
pub trait RunHandle: Send + Sync {
    /// Deliver a turn's input. One turn at a time: the next only after
    /// [`RuntimeEvent::TurnFinished`].
    async fn send_turn(&mut self, turn: &TurnSpec) -> Result<()>;
    /// The next event; `None` once the run is over and drained.
    async fn next_event(&mut self) -> Option<RuntimeEvent>;
    /// Answer a decision.
    async fn decide(&mut self, request_id: &str, reply: DecisionReply) -> Result<()>;
    /// Answer a [`RuntimeEvent::ToolCheck`].
    async fn check(&mut self, check_id: &str, decision: CheckDecision) -> Result<()> {
        let _ = (check_id, decision);
        anyhow::bail!("this runtime doesn't check tool calls")
    }
    /// Ask the provider to stop the current turn; it settles with
    /// [`TurnOutcome::Interrupted`]. The process stays.
    async fn interrupt(&mut self) -> Result<()>;
    /// End the process (after interrupting what it does).
    async fn stop(&mut self) -> Result<()>;
    fn inspect(&self) -> RunInfo;
}

/// The managed backends (`claude_stream` is `legacy_cli`, `claude_sdk` is
/// `sdk`), in the order Settings offers them.
pub const BACKENDS: &[&str] = &["legacy_cli", "sdk"];

/// The runtime for an agent and backend, if it has a managed mode.
pub fn runtime(id: &str, backend: &str) -> Option<&'static dyn AgentRuntime> {
    static CLAUDE: crate::agents::claude_stream::ClaudeRuntime =
        crate::agents::claude_stream::ClaudeRuntime;
    static CLAUDE_SDK: crate::agents::claude_sdk::ClaudeSdkRuntime =
        crate::agents::claude_sdk::ClaudeSdkRuntime;
    match (id, backend) {
        ("claude", "legacy_cli") => Some(&CLAUDE),
        ("claude", "sdk") => Some(&CLAUDE_SDK),
        _ => None,
    }
}

/// End what is left of a run's process group (the run's agent was started
/// as its leader, `process_group(0)`). Call it only while that leader is an
/// unreaped child of ours: then the group's id can't belong to anyone else.
pub fn end_group(leader: u32) {
    // SAFETY: a plain signal to a process group this daemon created; the
    // caller guarantees its leader hasn't been reaped.
    unsafe { libc::killpg(leader as libc::pid_t, libc::SIGKILL) };
}

/// When process `pid` started (seconds since the epoch), if it is running.
pub fn process_started(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(pid).map(|p| p.start_time())
}

/// Make sure a run's processes didn't outlive it: if `p` is still running
/// — the same process, by its start time, not just a reused id — its
/// process group is asked to stop, then made to. Returns whether it was.
pub async fn end_leftover(p: &otter_core::conversation::ProcessRef) -> bool {
    if process_started(p.pid) != Some(p.started) {
        return false;
    }
    let group = p.pid as libc::pid_t;
    tracing::warn!(
        pid = p.pid,
        "a run's process outlived its daemon; stopping it"
    );
    // SAFETY: plain signals to a process group the run itself created.
    unsafe { libc::killpg(group, libc::SIGTERM) };
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if process_started(p.pid) != Some(p.started) {
            return true;
        }
    }
    // SAFETY: as above.
    unsafe { libc::killpg(group, libc::SIGKILL) };
    true
}

/// A short, stable hash of a JSON value (key order doesn't matter), to bind
/// an answer to the exact input it was given for.
pub fn input_hash(v: &serde_json::Value) -> String {
    fn canonical(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                let parts: Vec<String> = keys
                    .into_iter()
                    .map(|k| {
                        format!(
                            "{}:{}",
                            serde_json::Value::String(k.clone()),
                            canonical(&o[k])
                        )
                    })
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
            serde_json::Value::Array(a) => {
                format!(
                    "[{}]",
                    a.iter().map(canonical).collect::<Vec<_>>().join(",")
                )
            }
            other => other.to_string(),
        }
    }
    // FNV-1a, 64 bits: stable across builds and platforms (not a security
    // hash: it tells inputs apart, it doesn't authenticate them).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical(v).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a:{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_hashes_ignore_key_order_and_tell_inputs_apart() {
        let a = serde_json::json!({"command": "rm -rf build", "timeout": 5});
        let b = serde_json::json!({"timeout": 5, "command": "rm -rf build"});
        let c = serde_json::json!({"command": "rm -rf src", "timeout": 5});
        assert_eq!(input_hash(&a), input_hash(&b));
        assert_ne!(input_hash(&a), input_hash(&c));
        assert!(input_hash(&a).starts_with("fnv1a:"));
    }

    #[tokio::test]
    async fn a_leftover_run_process_is_ended_but_a_reused_id_isnt_touched() {
        use otter_core::conversation::ProcessRef;
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let started = process_started(pid).expect("running");
        // Another process that once had this id (an older start): left alone.
        let other = ProcessRef {
            generation: 1,
            pid,
            started: started - 3600,
        };
        assert!(!end_leftover(&other).await);
        assert!(child.try_wait().unwrap().is_none(), "still running");
        let same = ProcessRef {
            generation: 1,
            pid,
            started,
        };
        assert!(end_leftover(&same).await);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }
}
