//! Agent integrations (design §9, §22, §23; docs/architecture-lessons.md §7–§11).
//!
//! An agent is one kind of session: its process is a coding agent. Generic
//! code (sessions, reconciliation, attention) only ever asks a provider three
//! things — how to start the agent, what state it is in, and whether the host
//! can run it — and stores what the provider returns without interpreting it
//! (`AgentInfo::provider_session_id`, `AgentInfo::provider_state`).
//!
//! Everything about *how* a particular agent is launched, resumed and observed
//! (for Codex: CLI flags, rollout files, transcript parsing, the quiet-turn
//! heuristic) lives in that provider's module: `codex`, `claude` (Claude
//! Code). Adding another means adding a module here, not changing Workspace
//! or Session.

pub mod claude;
pub mod codex;

use std::collections::HashSet;

use anyhow::Result;
use async_trait::async_trait;
use workd_core::{AgentCapability, AgentInfo, AgentState, Timestamp};

use crate::env::EnvMap;

/// Provider used when a session doesn't name one.
pub const DEFAULT_PROVIDER: &str = "codex";

static PROVIDERS: [&dyn AgentProvider; 2] = [&codex::Codex, &claude::ClaudeCode];

/// Every provider this build supports.
pub fn all() -> &'static [&'static dyn AgentProvider] {
    &PROVIDERS
}

pub fn provider(id: &str) -> Option<&'static dyn AgentProvider> {
    all().iter().copied().find(|p| p.id() == id)
}

pub fn ids() -> Vec<&'static str> {
    all().iter().map(|p| p.id()).collect()
}

/// What each provider can do on this host.
pub async fn detect_all(env: &EnvMap) -> Vec<AgentCapability> {
    let mut out = Vec::new();
    for p in all() {
        out.push(p.detect(env).await);
    }
    out
}

/// What a provider can see when observing one agent session.
pub struct ObserveContext<'a> {
    /// The session's working directory (on this host).
    pub cwd: &'a str,
    /// When the current execution started.
    pub started_at: Timestamp,
    /// The agent's process, if known.
    pub pid: Option<u32>,
    /// When the session last produced terminal output, if known.
    pub last_output: Option<Timestamp>,
    pub now: Timestamp,
    /// The environment the agent was launched with (login + workspace env).
    pub env: &'a EnvMap,
    /// `provider_session_id`s already bound to other sessions.
    pub claimed: &'a HashSet<String>,
}

/// What changed since the last observation. `None` fields are unchanged.
#[derive(Debug, Default)]
pub struct Observation {
    pub state: Option<AgentState>,
    pub provider_session_id: Option<String>,
    pub provider_state: Option<serde_json::Value>,
    /// Excerpt of the agent's latest message.
    pub last_message: Option<String>,
}

#[async_trait]
pub trait AgentProvider: Send + Sync {
    /// Stable id, as used in `SessionSpec::provider` (e.g. `codex`).
    fn id(&self) -> &'static str;

    /// Whether (and which version of) the agent is runnable on this host.
    async fn detect(&self, env: &EnvMap) -> AgentCapability;

    /// Command line that starts the agent — resuming
    /// `info.provider_session_id` if set, else starting fresh (with
    /// `info.prompt`, if any).
    fn launch_argv(&self, info: &AgentInfo, env: &EnvMap) -> Result<Vec<String>>;

    /// Observe a running agent. Called periodically; must be cheap and must
    /// not block on the agent.
    fn observe(&self, ctx: &ObserveContext<'_>, info: &AgentInfo) -> Observation;
}

/// Default for `WORKD_AGENT_QUIET_SECS`.
const DEFAULT_QUIET_SECS: i64 = 8;

fn quiet_threshold() -> i64 {
    std::env::var("WORKD_AGENT_QUIET_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_QUIET_SECS)
}

/// The quiet-turn heuristic, for agents whose approval prompts and questions
/// don't appear in their transcripts: a turn whose terminal and transcript
/// have both gone quiet is blocked on the user; a freshly started agent that
/// goes quiet is idle (waiting for its first prompt, or a startup dialog).
/// `grew`: the transcript grew in this observation. If the agent keeps
/// animating while it waits, this misses rather than crying wolf.
pub fn settle(
    state: AgentState,
    grew: bool,
    last_activity: Option<Timestamp>,
    now: Timestamp,
) -> AgentState {
    let Some(last) = last_activity.filter(|_| !grew) else {
        return state;
    };
    let quiet = (now - last).num_seconds();
    match state {
        AgentState::Working if quiet >= quiet_threshold() => AgentState::Blocked,
        AgentState::Blocked if quiet < 2 => AgentState::Working,
        AgentState::Starting if quiet >= quiet_threshold() => AgentState::Idle,
        other => other,
    }
}

/// A single-line excerpt suitable for a status line.
pub fn excerpt(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
