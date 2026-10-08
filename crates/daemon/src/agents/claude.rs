//! Claude Code provider. Everything Claude-Code-specific lives here (D-023).
//!
//! - **Launch:** `claude [prompt]`; resume: `claude --resume <session-id>`.
//! - **Identity:** the transcript
//!   `$CLAUDE_CONFIG_DIR/projects/<cwd>/<session-id>.jsonl` (default config
//!   dir `~/.claude`; `<cwd>` with every non-alphanumeric character replaced
//!   by `-`). The file name is the session id (= `provider_session_id`).
//!   Found after launch as the newest transcript for the workspace written
//!   since the execution started and not bound to another session.
//! - **State:** from the turn records — a prompt or tool result means the
//!   agent is working; an assistant message ending its turn (`end_turn`)
//!   means it is waiting for input, and its text is the last message; an
//!   interruption means idle.
//! - **Heuristic:** permission prompts aren't in the transcript; a quiet
//!   turn is reported as blocked (shared with Codex, `agents::settle`).
//!
//! The transcript format belongs to a self-updating binary: everything parses
//! defensively and ignores what it doesn't know.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workd_core::{AgentCapability, AgentInfo, AgentState, Timestamp};

use super::{AgentProvider, Observation, ObserveContext, excerpt, settle};
use crate::env::{EnvMap, which};

/// Tolerance between our launch time and the transcript's mtime.
const CLOCK_SLACK: i64 = 5;
const MESSAGE_EXCERPT: usize = 240;

pub struct ClaudeCode;

/// What Workd keeps in `AgentInfo::provider_state` for a Claude Code session.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct ClaudeState {
    /// The transcript being followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript: Option<PathBuf>,
    /// How far it has been read.
    #[serde(default)]
    offset: u64,
}

#[async_trait]
impl AgentProvider for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude"
    }

    async fn detect(&self, env: &EnvMap) -> AgentCapability {
        let mut cap = AgentCapability {
            provider: self.id().to_owned(),
            available: false,
            version: None,
            can_resume: true,
        };
        let Some(claude) = which("claude", env) else {
            return cap;
        };
        cap.available = true;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(claude)
                .arg("--version")
                .env_clear()
                .envs(env)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        if let Ok(Ok(out)) = out
            && out.status.success()
        {
            cap.version = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .map(|l| l.trim().to_owned());
        }
        cap
    }

    fn launch_argv(&self, info: &AgentInfo, env: &EnvMap) -> Result<Vec<String>> {
        let claude = which("claude", env).context(
            "Claude Code is not installed on this host (`claude` is not on the login PATH)",
        )?;
        let mut argv = vec![claude.to_string_lossy().into_owned()];
        match (&info.provider_session_id, &info.prompt) {
            (Some(id), _) => {
                argv.push("--resume".into());
                argv.push(id.clone());
            }
            (None, Some(prompt)) => argv.push(prompt.clone()),
            (None, None) => {}
        }
        Ok(argv)
    }

    fn observe(&self, ctx: &ObserveContext<'_>, info: &AgentInfo) -> Observation {
        let mut state: ClaudeState =
            serde_json::from_value(info.provider_state.clone()).unwrap_or_default();
        let mut obs = Observation::default();
        let mut state_changed = false;
        let earliest: Timestamp = ctx.started_at - Duration::seconds(CLOCK_SLACK);

        // Follow the transcript this execution writes. A resumed session may
        // continue its old file or start a new one, so until the current file
        // is written during this execution, look for a newer one.
        let current_fresh = state
            .transcript
            .as_deref()
            .and_then(mtime)
            .is_some_and(|m| m >= earliest);
        if !current_fresh
            && let Some((id, path)) = newest_transcript(ctx, earliest)
            && state.transcript.as_ref() != Some(&path)
        {
            tracing::info!(session = %id, transcript = %path.display(), "claude session bound");
            if info.provider_session_id.as_ref() != Some(&id) {
                obs.provider_session_id = Some(id);
            }
            state.transcript = Some(path);
            state.offset = 0;
            state_changed = true;
        }

        let mut next = info.state;
        let mut grew = false;
        let mut transcript_mtime = None;
        if let Some(path) = &state.transcript {
            match read_transcript(path, state.offset) {
                Ok(update) => {
                    if update.offset != state.offset {
                        state.offset = update.offset;
                        grew = true;
                        state_changed = true;
                    }
                    obs.last_message = update.last_message;
                    if let Some(s) = update.state {
                        next = s;
                    }
                }
                Err(e) => tracing::debug!("reading claude transcript: {e:#}"),
            }
            transcript_mtime = mtime(path);
        }

        next = settle(next, grew, ctx.last_output.max(transcript_mtime), ctx.now);

        if next != info.state {
            obs.state = Some(next);
        }
        if state_changed {
            obs.provider_state = serde_json::to_value(&state).ok();
        }
        obs
    }
}

fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(DateTime::<Utc>::from)
}

/// Claude Code's per-project transcript directory name for `cwd`.
fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn projects_dir(env: &EnvMap) -> Option<PathBuf> {
    let config = match env.get("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env.get("HOME")?).join(".claude"),
    };
    Some(config.join("projects"))
}

/// The transcript directories for the session's cwd (as given, and with
/// symlinks resolved, which is what Claude Code records on macOS).
fn transcript_dirs(env: &EnvMap, cwd: &str) -> Vec<PathBuf> {
    let Some(projects) = projects_dir(env) else {
        return Vec::new();
    };
    let mut names = vec![project_dir_name(cwd)];
    if let Ok(real) = std::fs::canonicalize(cwd) {
        let real = project_dir_name(&real.to_string_lossy());
        if !names.contains(&real) {
            names.push(real);
        }
    }
    names.into_iter().map(|n| projects.join(n)).collect()
}

/// The most recently written transcript for the session's cwd, written since
/// `since` and not bound to another session.
fn newest_transcript(ctx: &ObserveContext<'_>, since: Timestamp) -> Option<(String, PathBuf)> {
    let mut best: Option<(DateTime<Utc>, String, PathBuf)> = None;
    for dir in transcript_dirs(ctx.env, ctx.cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let Some(modified) = mtime(&path) else {
                continue;
            };
            if modified < since || ctx.claimed.contains(&id) {
                continue;
            }
            if best.as_ref().is_none_or(|(t, _, _)| modified > *t) {
                best = Some((modified, id, path));
            }
        }
    }
    best.map(|(_, id, path)| (id, path))
}

// ---------------------------------------------------------------------------
// Transcripts
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct TranscriptUpdate {
    offset: u64,
    state: Option<AgentState>,
    last_message: Option<String>,
}

fn read_transcript(path: &Path, offset: u64) -> Result<TranscriptUpdate> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    // Truncated or replaced: start over.
    let offset = if len < offset { 0 } else { offset };
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.take(len - offset).read_to_end(&mut buf)?;
    // Only consume complete lines; a partial last line is read next time.
    let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut update = TranscriptUpdate {
        offset: offset + complete as u64,
        ..Default::default()
    };
    for line in buf[..complete].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_slice::<Value>(line) {
            apply_record(&record, &mut update);
        }
    }
    Ok(update)
}

const INTERRUPTED: &str = "[Request interrupted";

fn apply_record(record: &Value, update: &mut TranscriptUpdate) {
    // Subagent traffic and injected context don't change the main turn.
    if record["isSidechain"] == true || record["isMeta"] == true {
        return;
    }
    let message = &record["message"];
    match record["type"].as_str() {
        Some("user") => {
            let interrupted = match &message["content"] {
                Value::String(text) => text.starts_with(INTERRUPTED),
                Value::Array(items) => items.iter().any(|i| {
                    i["type"] == "text"
                        && i["text"]
                            .as_str()
                            .is_some_and(|t| t.starts_with(INTERRUPTED))
                }),
                _ => return,
            };
            update.state = Some(if interrupted {
                AgentState::Idle
            } else {
                AgentState::Working
            });
        }
        Some("assistant") => {
            let text: Vec<&str> = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["type"] == "text")
                .filter_map(|c| c["text"].as_str())
                .collect();
            if !text.is_empty() {
                update.last_message = Some(excerpt(&text.join(" "), MESSAGE_EXCERPT));
            }
            update.state = Some(match message["stop_reason"].as_str() {
                Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal") => {
                    AgentState::WaitingForInput
                }
                // Calling a tool, or still streaming.
                _ => AgentState::Working,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(lines: &[&str]) -> TranscriptUpdate {
        let mut update = TranscriptUpdate::default();
        for l in lines {
            apply_record(&serde_json::from_str(l).unwrap(), &mut update);
        }
        update
    }

    #[test]
    fn project_dirs_replace_every_non_alphanumeric() {
        assert_eq!(
            project_dir_name("/Users/a/Project/port-keeper"),
            "-Users-a-Project-port-keeper"
        );
        assert_eq!(
            project_dir_name("/private/var/folders/0r/x_y.z/T"),
            "-private-var-folders-0r-x-y-z-T"
        );
    }

    #[test]
    fn turn_records_drive_the_state() {
        let prompt = r#"{"type":"user","message":{"role":"user","content":"fix the tests"}}"#;
        let tool = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"text","text":"Running them."},{"type":"tool_use","name":"Bash"}]}}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        let done = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"thinking","thinking":"…"},{"type":"text","text":"All   tests\npass."}]}}"#;
        let side = r#"{"type":"assistant","isSidechain":true,"message":{"stop_reason":"end_turn","content":[{"type":"text","text":"subagent"}]}}"#;
        let interrupted = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;

        assert_eq!(apply(&[prompt]).state, Some(AgentState::Working));
        assert_eq!(apply(&[prompt, tool]).state, Some(AgentState::Working));
        assert_eq!(
            apply(&[prompt, tool, result]).state,
            Some(AgentState::Working)
        );
        let u = apply(&[prompt, tool, result, done, side]);
        assert_eq!(u.state, Some(AgentState::WaitingForInput));
        assert_eq!(u.last_message.as_deref(), Some("All tests pass."));
        assert_eq!(apply(&[prompt, interrupted]).state, Some(AgentState::Idle));
        // Unknown record types are ignored.
        let other = r#"{"type":"file-history-snapshot","snapshot":{}}"#;
        assert_eq!(apply(&[other]).state, None);
    }

    #[test]
    fn launch_resumes_by_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: EnvMap = [("PATH".to_owned(), dir.path().display().to_string())].into();
        let mut info = AgentInfo {
            provider: "claude".into(),
            provider_session_id: None,
            provider_state: Value::Null,
            state: AgentState::Starting,
            state_since: Utc::now(),
            last_message: None,
            prompt: Some("fix it".into()),
        };
        let argv = ClaudeCode.launch_argv(&info, &env).unwrap();
        assert_eq!(argv[1..], ["fix it"]);
        info.provider_session_id = Some("abc-123".into());
        let argv = ClaudeCode.launch_argv(&info, &env).unwrap();
        assert_eq!(argv[1..], ["--resume", "abc-123"]);
    }
}
