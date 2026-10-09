//! Managed agent runtimes (D-044): a coding agent driven through a
//! structured interface — events in, decisions out — instead of a terminal
//! someone watches. Interactive sessions (tmux, `agents::*`) stay as they
//! are; a managed run can be handed to one and back.
//!
//! The contract, generic over agents:
//!
//! - [`AgentRuntime::start`] starts a run (fresh, or resuming a conversation);
//! - [`RunHandle`]: `send_input`, `next_event` (the event stream), `decide`
//!   (answer a [`RuntimeEvent::DecisionNeeded`]), `cancel`, `inspect`.
//!
//! An adapter translates its agent's tool calls into a [`ToolCall`], so the
//! [`policy`] that decides what may run without asking is the same for every
//! agent. Agent-specific code (flags, wire format) lives under `agents/`.

pub mod policy;

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::env::EnvMap;

/// What a run is asked to do.
pub struct RunSpec {
    /// Working directory: the workspace root.
    pub cwd: PathBuf,
    /// The environment the agent runs with (login + workspace env). Passed
    /// to the process, never on its command line.
    pub env: EnvMap,
    /// The first user turn. For a resumed run, the next turn (may be empty).
    pub prompt: String,
    /// Resume this conversation (the runtime's own id), if set.
    pub resume: Option<String>,
    /// Extra instructions for the agent's system prompt.
    pub instructions: Option<String>,
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
}

/// The answer to a [`DecisionAsk`].
#[derive(Clone, Debug, PartialEq)]
pub enum DecisionReply {
    Allow,
    Deny {
        message: String,
    },
    /// The answer to a question (the chosen option or free text).
    Answer {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeEvent {
    /// The agent's conversation id, to resume it later.
    Session {
        id: String,
    },
    /// Something the agent said.
    Text {
        text: String,
    },
    /// A tool ran (or is about to, if allowed without asking).
    Tool {
        tool: String,
        call: ToolCall,
    },
    DecisionNeeded(DecisionAsk),
    /// The turn ended; the agent waits for input.
    TurnEnded {
        ok: bool,
        summary: Option<String>,
        cost_usd: Option<f64>,
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
    /// Start a run (resuming `spec.resume` if set).
    async fn start(&self, spec: RunSpec) -> Result<Box<dyn RunHandle>>;
}

#[async_trait]
pub trait RunHandle: Send {
    /// A new user turn.
    async fn send_input(&mut self, text: &str) -> Result<()>;
    /// The next event; `None` once the run is over and drained.
    async fn next_event(&mut self) -> Option<RuntimeEvent>;
    /// Answer a decision.
    async fn decide(&mut self, request_id: &str, reply: DecisionReply) -> Result<()>;
    /// Stop: interrupt the turn and end the process.
    async fn cancel(&mut self) -> Result<()>;
    fn inspect(&self) -> RunInfo;
}

/// The runtime for an agent id, if it has a managed mode.
pub fn runtime(id: &str) -> Option<&'static dyn AgentRuntime> {
    static CLAUDE: crate::agents::claude_stream::ClaudeRuntime =
        crate::agents::claude_stream::ClaudeRuntime;
    match id {
        "claude" => Some(&CLAUDE),
        _ => None,
    }
}
