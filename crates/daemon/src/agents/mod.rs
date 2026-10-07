//! Agent integrations (design §9, §22, §23).
//!
//! An agent is a session whose process is a coding agent. Workd launches and
//! resumes it, and observes it through *structured* output the agent writes
//! anyway (for Codex: its rollout transcript) — never by editing the agent's
//! configuration. Codex is the only provider in V1; the trait keeps it an
//! integration rather than a core concept.

pub mod codex;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use workd_core::{AgentInfo, AgentState, Timestamp};

use crate::env::EnvMap;

/// What a provider needs to find the transcript of an agent it launched.
pub struct DiscoverContext<'a> {
    /// The agent's working directory.
    pub cwd: &'a str,
    /// When the current execution started.
    pub started_at: Timestamp,
    /// The agent's process, if known.
    pub pid: Option<u32>,
    pub env: &'a EnvMap,
    /// Transcripts already bound to other sessions.
    pub claimed: &'a HashSet<String>,
}

/// New information from a transcript.
#[derive(Debug, Default)]
pub struct TranscriptUpdate {
    /// Offset to continue reading from.
    pub offset: u64,
    /// Latest state implied by the new records, if any.
    pub state: Option<AgentState>,
    /// Latest agent message, if any.
    pub last_message: Option<String>,
}

pub trait AgentProvider: Send + Sync {
    /// Command line that starts the agent, resuming `info.resume_id` if set.
    fn argv(&self, info: &AgentInfo, env: &EnvMap) -> Result<Vec<String>>;

    /// Find the conversation id and transcript of a running agent.
    fn discover(&self, ctx: &DiscoverContext<'_>) -> Option<(String, PathBuf)>;

    /// Read transcript records written since `offset`.
    fn read_transcript(&self, path: &Path, offset: u64) -> Result<TranscriptUpdate>;
}

pub fn provider(name: &str) -> Option<&'static dyn AgentProvider> {
    match name {
        "codex" => Some(&codex::Codex),
        _ => None,
    }
}

pub const PROVIDERS: &[&str] = &["codex"];

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
