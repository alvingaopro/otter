//! Claude Code provider. Everything Claude-Code-specific lives here (D-023).
//!
//! - **Launch:** `claude --settings <file> [prompt]`; resume: `claude
//!   --settings <file> --resume <session-id>`. The settings file (in the
//!   session's private directory) only adds hooks, for this process: Claude
//!   Code merges hooks from every settings source, so the user's own settings
//!   and hooks stay as they are and nothing of theirs is written.
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
//! - **Needs you:** permission prompts and questions aren't in the
//!   transcript, so the hooks report them: each calls `otterd
//!   internal-agent-hook claude <file>`, which appends a small record
//!   (`hook_record`). `PermissionRequest` → blocked on an approval (a
//!   question for `AskUserQuestion`, also caught by `PreToolUse`, and for
//!   MCP `Elicitation`); `PostToolUse*` of that tool / `UserPromptSubmit`
//!   → working again; a denial shows up in the transcript as an
//!   interruption (idle). No hook says a prompt was approved: a long command
//!   shows as the screen moving at every look (Claude Code's spinner) before
//!   the tool finishes. A prompt's screen is still, or changes now and then
//!   (a background task ending, a resize) or once a second (an auto-deny
//!   countdown), which is no answer. Observed on Claude Code 2.1.295 (D-035)
//!   and 2.1.304 (D-136).
//! - **Fallback:** if no hook has reported for this execution (hooks
//!   disabled by policy, an older Claude Code), the shared quiet-turn
//!   heuristic (`agents::settle`) applies.
//!
//! The transcript format belongs to a self-updating binary: everything parses
//! defensively and ignores what it doesn't know.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use otter_core::{AgentCapability, AgentInfo, AgentState, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AgentProvider, Blocker, LaunchContext, Observation, ObserveContext, excerpt, settle};
use crate::env::{EnvMap, which};

/// Claude Code's long-lived sign-in, from `claude setup-token`
/// (`sk-ant-oat01-…`): set in Settings, given to Claude Code's processes
/// (D-142). A key in their environment (`ANTHROPIC_API_KEY`) comes first.
pub const OAUTH_TOKEN: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Claude Code's classic renderer, whatever its saved `tui` setting (D-144).
/// Its fullscreen one takes the mouse, so in an attached session every wheel
/// step went to the host and came back as a redrawn screen; the classic one
/// writes the conversation to the scrollback, which the app scrolls itself.
pub const CLASSIC_RENDERER: (&str, &str) = ("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN", "1");

/// `env` signed in with `token`, when there is one; else as it is (Claude
/// Code's own sign-in on the host).
pub fn signed_in(env: &EnvMap, token: Option<&str>) -> EnvMap {
    let mut env = env.clone();
    if let Some(t) = token {
        env.insert(OAUTH_TOKEN.into(), t.to_owned());
    }
    env
}
use otter_core::AttentionKind;

/// Tolerance between our launch time and the transcript's mtime.
const CLOCK_SLACK: i64 = 5;
const MESSAGE_EXCERPT: usize = 240;
const DETAIL_EXCERPT: usize = 160;
/// How long after a prompt appeared the screen moving still counts as the
/// prompt being drawn (or the turn that led to it) rather than answered.
const PROMPT_DRAW_MS: i64 = 2000;
/// How long the screen must keep moving, at every look, for a prompt to
/// count as answered (D-136): longer than anything once a second can fake.
const ANSWER_MOTION_MS: i64 = 2000;

const SETTINGS_FILE: &str = "claude-settings.json";
const HOOKS_FILE: &str = "claude-hooks.jsonl";
/// The hook events Otter listens to (and `hook_record` keeps).
const HOOK_EVENTS: [&str; 9] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "Notification",
    "Elicitation",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
];
/// Claude Code's tool for asking the developer a multiple-choice question.
const ASK_TOOL: &str = "AskUserQuestion";

pub struct ClaudeCode;

/// What Otter keeps in `AgentInfo::provider_state` for a Claude Code session.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct ClaudeState {
    /// The transcript being followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript: Option<PathBuf>,
    /// How far it has been read.
    #[serde(default)]
    offset: u64,
    /// The execution (start time, ms) the hook fields below belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks_execution: Option<i64>,
    /// How far the hook log has been read.
    #[serde(default)]
    hooks_offset: u64,
    /// Hooks have reported during this execution, so they are trusted and
    /// the quiet-turn heuristic is off.
    #[serde(default)]
    hooks_live: bool,
    /// A prompt Claude Code is showing the developer, not yet answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<Pending>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Pending {
    /// A question (else a permission prompt).
    #[serde(default)]
    question: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// When it was raised (unix ms).
    at: i64,
    /// The subagent that asked, if not the main agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    /// The tool asked about, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    /// Once drawn, the screen kept moving: approved, the tool runs (D-136).
    /// Kept until a hook or the transcript ends the prompt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    answered: bool,
}

#[async_trait]
impl AgentProvider for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn secrets(&self) -> &'static [&'static str] {
        &[OAUTH_TOKEN]
    }

    fn session_env(&self) -> &'static [(&'static str, &'static str)] {
        &[CLASSIC_RENDERER]
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

    fn launch_argv(&self, info: &AgentInfo, ctx: &LaunchContext<'_>) -> Result<Vec<String>> {
        let claude = which("claude", ctx.env).context(
            "Claude Code is not installed on this host (`claude` is not on the login PATH)",
        )?;
        let settings = write_hook_settings(ctx)?;
        let mut argv = vec![
            claude.to_string_lossy().into_owned(),
            "--settings".into(),
            settings.to_string_lossy().into_owned(),
        ];
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
        let mut last_user_at = None;
        if let Some(path) = &state.transcript {
            match read_transcript(path, state.offset) {
                Ok(update) => {
                    if update.offset != state.offset {
                        state.offset = update.offset;
                        grew = true;
                        state_changed = true;
                    }
                    obs.last_message = update.last_message;
                    last_user_at = update.last_user_at;
                    if let Some(s) = update.state {
                        next = s;
                    }
                }
                Err(e) => tracing::debug!("reading claude transcript: {e:#}"),
            }
            transcript_mtime = mtime(path);
        }

        // What the hooks said since the last look.
        let execution = ctx.started_at.timestamp_millis();
        if state.hooks_execution != Some(execution) {
            state.hooks_execution = Some(execution);
            state.hooks_offset = 0;
            state.hooks_live = false;
            state.pending = None;
            state_changed = true;
        }
        let before = (state.hooks_offset, state.hooks_live, state.pending.clone());
        read_hooks(&ctx.dir.join(HOOKS_FILE), &mut state, execution);
        // Answered in the terminal: a denial is written to the transcript
        // as an interruption.
        if state
            .pending
            .as_ref()
            .is_some_and(|p| last_user_at.is_some_and(|t| t > p.at))
        {
            state.pending = None;
        }
        // Approved in the terminal: once the prompt is drawn, the screen
        // keeps moving at every look while the tool runs. A change now and
        // then, or once a second, is no answer (D-136).
        if let Some(p) = &mut state.pending
            && !p.answered
            && ctx.now.timestamp_millis() >= p.at + PROMPT_DRAW_MS
            && ctx
                .screen
                .is_some_and(|s| s.moving_for(Duration::milliseconds(ANSWER_MOTION_MS), ctx.now))
        {
            p.answered = true;
        }
        state_changed |= before != (state.hooks_offset, state.hooks_live, state.pending.clone());

        if let Some(p) = &state.pending {
            if p.answered {
                next = AgentState::Working;
            } else {
                next = AgentState::Blocked;
                obs.blocker = Some(Blocker {
                    kind: if p.question {
                        AttentionKind::Question
                    } else {
                        AttentionKind::Approval
                    },
                    detail: p.detail.clone(),
                });
            }
        } else if next == AgentState::Blocked && state.hooks_live {
            next = AgentState::Working;
        }
        if !state.hooks_live {
            next = settle(
                next,
                grew,
                ctx.screen.map(|s| s.since).max(transcript_mtime),
                ctx.now,
                info.prompt.is_some(),
            );
        }

        if next != info.state {
            obs.state = Some(next);
        }
        if state_changed {
            obs.provider_state = serde_json::to_value(&state).ok();
        }
        obs
    }

    fn hook_record(&self, input: &Value) -> Option<Value> {
        let event = input["hook_event_name"].as_str()?;
        if !HOOK_EVENTS.contains(&event) {
            return None;
        }
        let tool = input["tool_name"].as_str();
        let mut record = serde_json::json!({ "event": event });
        if let Some(tool) = tool {
            record["tool"] = tool.into();
        }
        if let Some(kind) = input["notification_type"].as_str() {
            record["kind"] = kind.into();
        }
        // Set when a subagent made the call.
        if let Some(agent) = input["agent_id"].as_str() {
            record["agent"] = agent.into();
        }
        let detail = match event {
            "PermissionRequest" | "PreToolUse" => tool_detail(tool, &input["tool_input"]),
            "Elicitation" => input["message"].as_str().map(str::to_owned),
            _ => None,
        };
        if let Some(detail) = detail {
            record["detail"] = excerpt(&detail, DETAIL_EXCERPT).into();
        }
        Some(record)
    }
}

/// What a prompt is about: the question asked, or the tool and Claude
/// Code's own description of the call. Never the full input.
fn tool_detail(tool: Option<&str>, input: &Value) -> Option<String> {
    if tool == Some(ASK_TOOL) {
        return input["questions"][0]["question"]
            .as_str()
            .map(str::to_owned);
    }
    let what = input["description"]
        .as_str()
        .or(input["file_path"].as_str());
    match (tool, what) {
        (Some(t), Some(w)) => Some(format!("{t}: {w}")),
        (Some(t), None) => Some(t.to_owned()),
        (None, w) => w.map(str::to_owned),
    }
}

/// Write the settings file that adds Otter's hooks for this session, and
/// start an empty hook log. Returns the settings file.
fn write_hook_settings(ctx: &LaunchContext<'_>) -> Result<PathBuf> {
    std::fs::create_dir_all(ctx.dir).with_context(|| format!("creating {}", ctx.dir.display()))?;
    let log = ctx.dir.join(HOOKS_FILE);
    std::fs::write(&log, b"").with_context(|| format!("creating {}", log.display()))?;
    let command = format!(
        "{} internal-agent-hook claude {}",
        crate::files::shell_quote(&ctx.otterd.to_string_lossy()),
        crate::files::shell_quote(&log.to_string_lossy()),
    );
    // Asynchronous: Otter only listens and must never hold Claude Code up
    // or influence a decision.
    let handler = serde_json::json!([{
        "hooks": [{ "type": "command", "command": command, "async": true, "timeout": 10 }]
    }]);
    let mut hooks = serde_json::Map::new();
    for event in HOOK_EVENTS {
        let mut entry = handler.clone();
        if event == "PreToolUse" {
            entry[0]["matcher"] = ASK_TOOL.into();
        }
        hooks.insert(event.to_owned(), entry);
    }
    let settings = ctx.dir.join(SETTINGS_FILE);
    std::fs::write(
        &settings,
        serde_json::to_vec_pretty(&serde_json::json!({ "hooks": hooks }))?,
    )
    .with_context(|| format!("writing {}", settings.display()))?;
    Ok(settings)
}

/// Apply new hook records to `state` (which prompt is up, if any). The
/// turn itself is followed in the transcript. Records from before
/// `execution` (ms) belong to an earlier one.
fn read_hooks(path: &Path, state: &mut ClaudeState, execution: i64) -> Option<()> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < state.hooks_offset {
        state.hooks_offset = 0;
    }
    file.seek(SeekFrom::Start(state.hooks_offset)).ok()?;
    let mut buf = Vec::new();
    file.take(len - state.hooks_offset)
        .read_to_end(&mut buf)
        .ok()?;
    let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    state.hooks_offset += complete as u64;
    for line in buf[..complete].split(|&b| b == b'\n') {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let at = record["t"].as_i64().unwrap_or_default();
        if at < execution - CLOCK_SLACK * 1000 {
            continue;
        }
        state.hooks_live = true;
        let detail = record["detail"].as_str().map(str::to_owned);
        let agent = record["agent"].as_str().map(str::to_owned);
        let tool = record["tool"].as_str();
        let ask = |question: bool| Pending {
            question,
            detail: detail.clone(),
            at,
            agent: agent.clone(),
            tool: tool.map(str::to_owned),
            answered: false,
        };
        match (record["event"].as_str(), tool) {
            (Some("PermissionRequest" | "PreToolUse"), tool) => {
                // `PreToolUse` is only hooked for questions, which then
                // also get a `PermissionRequest`: keep the first.
                let question = tool == Some(ASK_TOOL);
                if !(question
                    && state
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.question && !p.answered))
                {
                    state.pending = Some(ask(question));
                }
            }
            (Some("Elicitation"), _) => state.pending = Some(ask(true)),
            (Some("Notification"), _) => match record["kind"].as_str() {
                Some("permission_prompt") if state.pending.is_none() => {
                    state.pending = Some(ask(false))
                }
                Some("elicitation_dialog") if state.pending.is_none() => {
                    state.pending = Some(ask(true))
                }
                _ => {}
            },
            (Some("PostToolUse" | "PostToolUseFailure"), tool) => {
                // A tool finished — unless another agent's prompt is still
                // up, or it is another tool, called alongside the one asked
                // about (D-136).
                if state.pending.as_ref().is_none_or(|p| {
                    p.agent == agent
                        && (p.tool.is_none() || tool.is_none() || p.tool.as_deref() == tool)
                }) {
                    state.pending = None;
                }
            }
            (Some("UserPromptSubmit" | "Stop"), _) => state.pending = None,
            _ => {}
        }
    }
    Some(())
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
    /// When the developer's side last wrote (a prompt, a tool result, an
    /// interruption), unix ms.
    last_user_at: Option<i64>,
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
            if let Some(t) = record["timestamp"]
                .as_str()
                .and_then(|t| t.parse::<DateTime<Utc>>().ok())
            {
                update.last_user_at = Some(t.timestamp_millis());
            }
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
    use crate::agents::Screen;
    use std::collections::HashSet;

    #[test]
    fn a_token_from_settings_signs_claude_code_in() {
        let env = EnvMap::from([("PATH".to_owned(), "/bin".to_owned())]);
        let signed = signed_in(&env, Some("sk-ant-oat01-x"));
        assert_eq!(
            signed.get(OAUTH_TOKEN).map(String::as_str),
            Some("sk-ant-oat01-x")
        );
        assert_eq!(signed.get("PATH").map(String::as_str), Some("/bin"));
        // None set here: the host's own sign-in, untouched.
        assert_eq!(signed_in(&env, None), env);
        assert_eq!(ClaudeCode.secrets(), [OAUTH_TOKEN]);
    }
    use std::io::Write;

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
        let session_dir = dir.path().join("agent dir");
        let ctx = LaunchContext {
            env: &env,
            dir: &session_dir,
            otterd: Path::new("/opt/o t/otterd"),
        };
        let mut info = AgentInfo {
            provider: "claude".into(),
            provider_session_id: None,
            provider_state: Value::Null,
            state: AgentState::Starting,
            state_since: Utc::now(),
            last_message: None,
            prompt: Some("fix it".into()),
            blocker: None,
        };
        let settings = session_dir
            .join(SETTINGS_FILE)
            .to_string_lossy()
            .into_owned();
        let argv = ClaudeCode.launch_argv(&info, &ctx).unwrap();
        assert_eq!(argv[1..], ["--settings", &settings, "fix it"]);
        info.provider_session_id = Some("abc-123".into());
        let argv = ClaudeCode.launch_argv(&info, &ctx).unwrap();
        assert_eq!(argv[1..], ["--settings", &settings, "--resume", "abc-123"]);

        // The settings only add asynchronous hooks calling back into otterd,
        // with every path quoted for the shell.
        let v: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(v.as_object().unwrap().len(), 1);
        assert_eq!(hooks.len(), HOOK_EVENTS.len());
        let h = &hooks["PermissionRequest"][0]["hooks"][0];
        assert_eq!(h["async"], true);
        let log = session_dir.join(HOOKS_FILE);
        assert_eq!(
            h["command"],
            format!(
                "'/opt/o t/otterd' internal-agent-hook claude '{}'",
                log.display()
            )
        );
        assert_eq!(hooks["PreToolUse"][0]["matcher"], ASK_TOOL);
        assert!(log.exists());
    }

    /// Hook input as Claude Code 2.1.295 sent it (trimmed).
    fn hook(event: &str, extra: Value) -> Value {
        let mut v = serde_json::json!({
            "session_id": "825558a1-4390-4aab-bf7e-ec5e78809858",
            "transcript_path": "/home/u/.claude/projects/-w/825558a1-4390-4aab-bf7e-ec5e78809858.jsonl",
            "cwd": "/w",
            "permission_mode": "default",
            "hook_event_name": event,
        });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    #[test]
    fn hook_records_keep_only_what_observation_needs() {
        let r = ClaudeCode
            .hook_record(&hook(
                "PermissionRequest",
                serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "touch z", "description": "Create empty file z"}, "permission_suggestions": []}),
            ))
            .unwrap();
        assert_eq!(
            r,
            serde_json::json!({"event": "PermissionRequest", "tool": "Bash", "detail": "Bash: Create empty file z"})
        );
        let r = ClaudeCode
            .hook_record(&hook(
                "PermissionRequest",
                serde_json::json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "Should the file be named a or b?", "header": "File name", "options": []}]}}),
            ))
            .unwrap();
        assert_eq!(r["detail"], "Should the file be named a or b?");
        let r = ClaudeCode
            .hook_record(&hook(
                "Notification",
                serde_json::json!({"message": "Claude needs your permission", "notification_type": "permission_prompt"}),
            ))
            .unwrap();
        assert_eq!(
            r,
            serde_json::json!({"event": "Notification", "kind": "permission_prompt"})
        );
        // Tool output never makes it into the record.
        let r = ClaudeCode
            .hook_record(&hook(
                "PostToolUse",
                serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "cat .env"}, "tool_response": {"stdout": "SECRET=1"}, "agent_id": "a1"}),
            ))
            .unwrap();
        assert_eq!(
            r,
            serde_json::json!({"event": "PostToolUse", "tool": "Bash", "agent": "a1"})
        );
        assert!(
            ClaudeCode
                .hook_record(&hook("SubagentStop", serde_json::json!({})))
                .is_none()
        );
    }

    #[test]
    fn hooks_drive_prompts_and_only_a_moving_screen_tells_an_answer() {
        use AgentState::{Blocked, Working};
        let dir = tempfile::tempdir().unwrap();
        let env: EnvMap = [(
            "CLAUDE_CONFIG_DIR".to_owned(),
            dir.path().join("config").display().to_string(),
        )]
        .into();
        let started = Utc::now() - Duration::seconds(30);
        let cwd = dir.path().join("w");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.to_string_lossy().into_owned();
        let transcripts = transcript_dirs(&env, &cwd).remove(0);
        std::fs::create_dir_all(&transcripts).unwrap();
        let transcript = transcripts.join("s1.jsonl");
        let log = dir.path().join(HOOKS_FILE);
        let none = HashSet::new();
        let ms = |t: Timestamp| t.timestamp_millis();
        let at = |secs: i64| started + Duration::seconds(secs);
        let append = |path: &Path, line: String| {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(f, "{line}").unwrap();
        };
        let mut agent = AgentInfo {
            provider: "claude".into(),
            provider_session_id: None,
            provider_state: Value::Null,
            state: AgentState::Starting,
            state_since: started,
            last_message: None,
            prompt: None,
            blocker: None,
        };
        // Changed at `since`, still since.
        let still = |since| {
            Some(Screen {
                since,
                changed: true,
                moving_since: None,
            })
        };
        let observe = |agent: &mut AgentInfo, now: Timestamp, screen: Option<Screen>| {
            let ctx = ObserveContext {
                cwd: &cwd,
                dir: dir.path(),
                started_at: started,
                pid: None,
                screen,
                now,
                env: &env,
                claimed: &none,
            };
            let obs = ClaudeCode.observe(&ctx, agent);
            if let Some(s) = obs.state {
                agent.state = s;
            }
            if let Some(p) = obs.provider_state {
                agent.provider_state = p;
            }
            if let Some(id) = obs.provider_session_id {
                agent.provider_session_id = Some(id);
            }
            obs.blocker
        };
        // Looks every 500 ms from `from`, as reconcile takes them, the
        // screen changing at each `true`: the states the agent goes through.
        let looks =
            |agent: &mut AgentInfo, screen: &mut Screen, from: Timestamp, changes: &[bool]| {
                let mut states = Vec::new();
                for (i, &changed) in changes.iter().enumerate() {
                    let now = from + Duration::milliseconds(500 * i as i64);
                    *screen = screen.next(changed, now);
                    observe(agent, now, Some(*screen));
                    states.push(agent.state);
                }
                states
            };

        // A turn calls a tool that needs permission; the transcript stops
        // at the tool call (as observed) and the hook reports the prompt.
        append(
            &transcript,
            format!(
                r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":"go"}}}}"#,
                at(1).to_rfc3339()
            ),
        );
        append(
            &transcript,
            format!(
                r#"{{"type":"assistant","timestamp":"{}","message":{{"role":"assistant","stop_reason":"tool_use","content":[{{"type":"tool_use","name":"Bash"}}]}}}}"#,
                at(2).to_rfc3339()
            ),
        );
        append(
            &log,
            format!(r#"{{"event":"SessionStart","t":{}}}"#, ms(at(0))),
        );
        // The turn's spinner moves the screen at every look.
        let mut screen = Screen::first(at(0));
        assert_eq!(
            looks(&mut agent, &mut screen, at(1), &[true, true]),
            [Working, Working]
        );
        append(
            &log,
            format!(
                r#"{{"event":"PermissionRequest","tool":"Bash","detail":"Bash: Create empty file z","t":{}}}"#,
                ms(at(2))
            ),
        );
        // Drawing the prompt is a change too, then it stands still: the
        // moving that led up to it is no answer.
        let states = looks(
            &mut agent,
            &mut screen,
            at(2),
            &[true, true, false, false, false, false],
        );
        assert!(states.iter().all(|s| *s == Blocked), "{states:?}");
        let b = observe(&mut agent, at(5), Some(screen.next(false, at(5))));
        assert_eq!(
            b,
            Some(Blocker {
                kind: AttentionKind::Approval,
                detail: Some("Bash: Create empty file z".into())
            })
        );
        // otterd restarted: its first look at the screen is no answer.
        observe(&mut agent, at(31), Some(Screen::first(at(30))));
        assert_eq!(agent.state, Blocked);
        // A background task ending, a resize: the screen changes once while
        // the prompt is up. No answer (D-136; the old rule took any change
        // after the draw for one).
        let mut screen = Screen::first(at(30));
        let states = looks(
            &mut agent,
            &mut screen,
            at(40),
            &[true, false, false, false, false, false],
        );
        assert!(states.iter().all(|s| *s == Blocked), "{states:?}");
        // An auto-deny countdown redraws it once a second, every other look
        // (and now and then at two looks in a row): no answer either.
        let countdown: Vec<bool> = (0..20).map(|i| i % 2 == 0 || i == 7).collect();
        let states = looks(&mut agent, &mut screen, at(45), &countdown);
        assert!(states.iter().all(|s| *s == Blocked), "{states:?}");
        // Hooks are live: a long still screen doesn't matter.
        observe(&mut agent, at(60), still(at(54)));
        assert_eq!(agent.state, Blocked);
        // Approved: the tool runs and its spinner moves the screen at every
        // look. Two seconds of that is an answer, before the tool finishes.
        let mut screen = Screen::first(at(60));
        assert_eq!(
            looks(&mut agent, &mut screen, at(61), &[true; 5]),
            [Blocked, Blocked, Blocked, Blocked, Working]
        );
        // Kept until the tool is done: a hiccup doesn't take it back.
        assert_eq!(
            looks(&mut agent, &mut screen, at(64), &[false, false, true]),
            [Working, Working, Working]
        );
        append(
            &log,
            format!(
                r#"{{"event":"PostToolUse","tool":"Bash","t":{}}}"#,
                ms(at(66))
            ),
        );
        observe(&mut agent, at(67), still(at(67)));
        assert_eq!(agent.state, Working);
        // Thinking for a long time with a still screen is still working:
        // nothing asked for the developer.
        observe(&mut agent, at(100), still(at(67)));
        assert_eq!(agent.state, Working);

        // Approved at once: the screen never stops moving, from the turn
        // through the prompt to the tool, so it is answered once drawn.
        let mut screen = Screen::first(at(146));
        assert_eq!(
            looks(&mut agent, &mut screen, at(147), &[true; 2]),
            [Working, Working]
        );
        append(
            &log,
            format!(
                r#"{{"event":"PermissionRequest","tool":"Bash","detail":"Bash: Run the tests","t":{}}}"#,
                ms(at(148))
            ),
        );
        assert_eq!(
            looks(&mut agent, &mut screen, at(148), &[true; 6]),
            [Blocked, Blocked, Blocked, Blocked, Working, Working]
        );
        append(
            &log,
            format!(
                r#"{{"event":"PostToolUse","tool":"Bash","t":{}}}"#,
                ms(at(160))
            ),
        );
        observe(&mut agent, at(161), still(at(161)));
        assert_eq!(agent.state, Working);

        // A question.
        append(
            &log,
            format!(
                r#"{{"event":"PreToolUse","tool":"AskUserQuestion","detail":"a or b?","t":{}}}"#,
                ms(at(201))
            ),
        );
        append(
            &log,
            format!(
                r#"{{"event":"PermissionRequest","tool":"AskUserQuestion","detail":"a or b?","t":{}}}"#,
                ms(at(201))
            ),
        );
        let b = observe(&mut agent, at(202), still(at(201)));
        assert_eq!(agent.state, Blocked);
        assert_eq!(b.unwrap().kind, AttentionKind::Question);
        // Another tool called alongside it (a subagent sent off in the
        // background) finishes: the question is still up (D-136).
        append(
            &log,
            format!(
                r#"{{"event":"PostToolUse","tool":"Agent","t":{}}}"#,
                ms(at(203))
            ),
        );
        let b = observe(&mut agent, at(204), still(at(203)));
        assert_eq!(agent.state, Blocked);
        assert_eq!(b.unwrap().kind, AttentionKind::Question);
        append(
            &log,
            format!(
                r#"{{"event":"PostToolUse","tool":"AskUserQuestion","t":{}}}"#,
                ms(at(210))
            ),
        );
        observe(&mut agent, at(211), still(at(210)));
        assert_eq!(agent.state, Working);

        // Denied: no hook, the transcript records an interruption.
        append(
            &log,
            format!(
                r#"{{"event":"PermissionRequest","tool":"Bash","t":{}}}"#,
                ms(at(220))
            ),
        );
        observe(&mut agent, at(221), still(at(220)));
        assert_eq!(agent.state, Blocked);
        append(
            &transcript,
            format!(
                r#"{{"type":"user","timestamp":"{}","message":{{"role":"user","content":[{{"type":"text","text":"[Request interrupted by user for tool use]"}}]}}}}"#,
                at(230).to_rfc3339()
            ),
        );
        let b = observe(&mut agent, at(231), still(at(230)));
        assert_eq!(agent.state, AgentState::Idle);
        assert!(b.is_none());

        // What older otterds kept still reads.
        let old: ClaudeState =
            serde_json::from_value(serde_json::json!({"pending": {"at": 1}})).unwrap();
        let p = old.pending.unwrap();
        assert!(p.tool.is_none() && !p.answered);
    }

    #[test]
    fn without_hooks_a_still_turn_falls_back_to_the_heuristic() {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvMap::new();
        let none = HashSet::new();
        let started = Utc::now() - Duration::seconds(60);
        let ctx = ObserveContext {
            cwd: "/nowhere",
            dir: dir.path(),
            started_at: started,
            pid: None,
            screen: Some(Screen {
                since: started,
                changed: true,
                moving_since: None,
            }),
            now: Utc::now(),
            env: &env,
            claimed: &none,
        };
        let agent = AgentInfo {
            provider: "claude".into(),
            provider_session_id: None,
            provider_state: Value::Null,
            state: AgentState::Working,
            state_since: started,
            last_message: None,
            prompt: None,
            blocker: None,
        };
        let obs = ClaudeCode.observe(&ctx, &agent);
        assert_eq!(obs.state, Some(AgentState::Blocked));
        assert!(obs.blocker.is_none());
    }
}
