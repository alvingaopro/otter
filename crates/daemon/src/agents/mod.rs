//! Agent integrations (design §9, §22, §23; docs/architecture-lessons.md §7–§11).
//!
//! An agent is one kind of session: its process is a coding agent. Generic
//! code (sessions, reconciliation, attention) only ever asks a provider three
//! things — how to start the agent, what state it is in, and whether the host
//! can run it — and stores what the provider returns without interpreting it
//! (`AgentInfo::provider_session_id`, `AgentInfo::provider_state`).
//!
//! Everything about *how* a particular agent is launched, resumed and observed
//! (for Codex: CLI flags, rollout files, transcript parsing; for Claude Code:
//! also its hooks) lives in that provider's module: `codex`, `claude` (Claude
//! Code). Adding another means adding a module here, not changing Workspace
//! or Session.
//!
//! Knowing that an agent needs the developer (D-035): a provider reports
//! [`AgentState::Blocked`] plus a [`Blocker`] when the agent itself says so
//! (Claude Code's hooks); where no such signal exists (Codex) it falls back
//! to [`settle`], which reads a turn whose transcript *and screen* have stood
//! still as waiting on the developer.

pub mod claude;
pub mod claude_sdk;
pub mod claude_stream;
pub mod codex;

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use otter_core::{AgentCapability, AgentInfo, AgentState, AttentionKind, Timestamp};

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

/// What a provider gets when starting an agent.
pub struct LaunchContext<'a> {
    /// The environment the agent will run with (login + workspace env).
    pub env: &'a EnvMap,
    /// A private directory Otter manages for this session (created on
    /// demand, removed with the session), e.g. for hook output.
    pub dir: &'a Path,
    /// This `otterd`, for commands the agent should call back
    /// (`otterd internal-agent-hook`).
    pub otterd: &'a Path,
}

/// What a provider can see when observing one agent session.
pub struct ObserveContext<'a> {
    /// The session's working directory (on this host).
    pub cwd: &'a str,
    /// The session's private directory (see [`LaunchContext::dir`]).
    pub dir: &'a Path,
    /// When the current execution started.
    pub started_at: Timestamp,
    /// The agent's process, if known.
    pub pid: Option<u32>,
    /// What the session's terminal *shows*, if watched. (Not when it last
    /// wrote: TUIs redraw unchanged screens constantly.)
    pub screen: Option<Screen>,
    pub now: Timestamp,
    /// The environment the agent was launched with (login + workspace env).
    pub env: &'a EnvMap,
    /// `provider_session_id`s already bound to other sessions.
    pub claimed: &'a HashSet<String>,
}

/// How long a session's screen has looked as it does now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    /// Since when it has shown what it shows now.
    pub since: Timestamp,
    /// Whether Otter saw it change to that (false: the first look, e.g.
    /// after otterd restarted, so `since` is only a lower bound).
    pub changed: bool,
}

impl Screen {
    /// Whether it was seen changing after `t`.
    pub fn changed_after(&self, t: Timestamp) -> bool {
        self.changed && self.since > t
    }
}

/// What changed since the last observation. `None` fields are unchanged.
#[derive(Debug, Default)]
pub struct Observation {
    pub state: Option<AgentState>,
    pub provider_session_id: Option<String>,
    pub provider_state: Option<serde_json::Value>,
    /// Excerpt of the agent's latest message.
    pub last_message: Option<String>,
    /// What a blocked agent is waiting for, when the agent said so.
    pub blocker: Option<Blocker>,
}

/// What a blocked agent waits for, as reported by the agent itself (as
/// opposed to inferred from quiet).
#[derive(Debug, Clone, PartialEq)]
pub struct Blocker {
    /// [`AttentionKind::Approval`] or [`AttentionKind::Question`].
    pub kind: AttentionKind,
    /// What it wants approved, or what it asks (an excerpt).
    pub detail: Option<String>,
}

#[async_trait]
pub trait AgentProvider: Send + Sync {
    /// Stable id, as used in `SessionSpec::provider` (e.g. `codex`).
    fn id(&self) -> &'static str;

    /// Whether (and which version of) the agent is runnable on this host.
    async fn detect(&self, env: &EnvMap) -> AgentCapability;

    /// Command line that starts the agent — resuming
    /// `info.provider_session_id` if set, else starting fresh (with
    /// `info.prompt`, if any). May prepare files in `ctx.dir`.
    fn launch_argv(&self, info: &AgentInfo, ctx: &LaunchContext<'_>) -> Result<Vec<String>>;

    /// Observe a running agent. Called periodically; must be cheap and must
    /// not block on the agent.
    fn observe(&self, ctx: &ObserveContext<'_>, info: &AgentInfo) -> Observation;

    /// The line to record for one hook call the agent made to
    /// `otterd internal-agent-hook <provider> <file>` (its JSON input), or
    /// `None` to record nothing. Keep it small: no tool output, no file
    /// contents.
    fn hook_record(&self, _input: &serde_json::Value) -> Option<serde_json::Value> {
        None
    }
}

/// Entry point for `otterd internal-agent-hook <provider> <file>`: an agent
/// calls this from its hooks with the event as JSON on stdin; the provider
/// picks what to keep and it is appended to `file` with the time it arrived.
/// Never fails and prints nothing: what a hook prints or how it exits can
/// steer the agent, and this must only observe.
pub fn run_hook(provider: &str, file: &Path) {
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return;
    }
    let (Some(p), Ok(value)) = (
        self::provider(provider),
        serde_json::from_slice::<serde_json::Value>(&input),
    ) else {
        return;
    };
    let Some(mut record) = p.hook_record(&value) else {
        return;
    };
    record["t"] = serde_json::json!(chrono::Utc::now().timestamp_millis());
    let mut line = record.to_string();
    line.push('\n');
    // One small append: concurrent hooks don't interleave.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Default for `OTTER_AGENT_QUIET_SECS`.
const DEFAULT_QUIET_SECS: i64 = 8;

fn quiet_threshold() -> i64 {
    std::env::var("OTTER_AGENT_QUIET_SECS")
        .or_else(|_| std::env::var("WORKD_AGENT_QUIET_SECS"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_QUIET_SECS)
}

/// The quiet-turn heuristic, the fallback for agents that don't say when
/// they wait on the developer: a turn whose transcript and screen have both
/// stood still is blocked on the user. Calibrated against the real TUIs
/// (D-035): while they think or run a command they show a ticking elapsed
/// time, so the screen changes every second; an approval prompt or question
/// is a static screen. A freshly started agent that goes still is waiting
/// at a startup dialog (e.g. "trust this folder?") if it was given a prompt
/// to start on, else for its first prompt (idle).
///
/// `grew`: the transcript grew in this observation. `last_activity`: the
/// later of the screen's and the transcript's last change. If an agent kept
/// animating while it waits, this would miss rather than cry wolf.
pub fn settle(
    state: AgentState,
    grew: bool,
    last_activity: Option<Timestamp>,
    now: Timestamp,
    has_prompt: bool,
) -> AgentState {
    let Some(last) = last_activity.filter(|_| !grew) else {
        return state;
    };
    let quiet = (now - last).num_seconds();
    match state {
        AgentState::Working if quiet >= quiet_threshold() => AgentState::Blocked,
        AgentState::Blocked if quiet < 2 => AgentState::Working,
        AgentState::Starting if quiet >= quiet_threshold() => {
            if has_prompt {
                AgentState::Blocked
            } else {
                AgentState::Idle
            }
        }
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
