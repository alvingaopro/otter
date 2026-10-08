//! Codex provider. Everything Codex-specific lives here (decisions.md D-013).
//!
//! - **Launch:** `codex --no-daemon [prompt]`; resume: `codex --no-daemon
//!   resume <id>`. `--no-daemon` keeps the agent inside the process Workd
//!   manages (by default the Codex TUI attaches to a shared, self-updating
//!   background daemon, and the work would live outside the session).
//! - **Identity:** the rollout file `$CODEX_HOME/sessions/YYYY/MM/DD/
//!   rollout-<ts>-<id>.jsonl`, whose first record (`session_meta`) carries the
//!   conversation id (= `provider_session_id`) and cwd. Codex creates it
//!   lazily, so it is discovered after launch: from the process's open files
//!   when possible, else by cwd and start time.
//! - **State:** `event_msg` records in the rollout — `task_started` (working),
//!   `task_complete` (turn finished, with `last_agent_message`),
//!   `turn_aborted`.
//! - **Heuristic:** approval prompts and questions aren't in the rollout. A
//!   turn whose terminal and rollout have both gone quiet is reported as
//!   blocked; a freshly started Codex that goes quiet is idle.
//!
//! The rollout format belongs to a self-updating binary: everything parses
//! defensively and ignores what it doesn't know.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workd_core::{AgentCapability, AgentInfo, AgentState, Timestamp};

use super::{AgentProvider, Observation, ObserveContext, excerpt, settle};
use crate::env::{EnvMap, which};

/// Tolerance between our launch time and Codex's own timestamps.
const CLOCK_SLACK: i64 = 5;
const MESSAGE_EXCERPT: usize = 240;

pub struct Codex;

/// What Workd keeps in `AgentInfo::provider_state` for a Codex session.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct CodexState {
    /// The rollout file being followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollout: Option<PathBuf>,
    /// How far it has been read.
    #[serde(default)]
    offset: u64,
}

#[async_trait]
impl AgentProvider for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    async fn detect(&self, env: &EnvMap) -> AgentCapability {
        let mut cap = AgentCapability {
            provider: self.id().to_owned(),
            available: false,
            version: None,
            can_resume: true,
        };
        let Some(codex) = which("codex", env) else {
            return cap;
        };
        cap.available = true;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(codex)
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
        let codex = which("codex", env)
            .context("codex is not installed on this host (not on the login PATH)")?;
        let mut argv = vec![codex.to_string_lossy().into_owned(), "--no-daemon".into()];
        match (&info.provider_session_id, &info.prompt) {
            (Some(id), _) => {
                argv.push("resume".into());
                argv.push(id.clone());
            }
            (None, Some(prompt)) => argv.push(prompt.clone()),
            (None, None) => {}
        }
        Ok(argv)
    }

    fn observe(&self, ctx: &ObserveContext<'_>, info: &AgentInfo) -> Observation {
        let mut state: CodexState =
            serde_json::from_value(info.provider_state.clone()).unwrap_or_default();
        let mut obs = Observation::default();
        let mut state_changed = false;

        // Bind (or re-bind) the rollout: the file the process has open wins;
        // otherwise find it by conversation id, or discover a new one.
        let open = ctx.pid.and_then(open_rollout);
        let found = match (&open, &state.rollout, &info.provider_session_id) {
            (Some(found), current, _) if current.as_ref() != Some(&found.1) => open.clone(),
            (_, Some(_), _) => None,
            (_, None, Some(id)) => find_rollout(ctx.env, id).map(|p| (id.clone(), p)),
            (_, None, None) => discover(ctx),
        };
        if let Some((id, path)) = found {
            tracing::info!(conversation = %id, rollout = %path.display(), "codex conversation bound");
            if info.provider_session_id.as_ref() != Some(&id) {
                obs.provider_session_id = Some(id);
            }
            state.rollout = Some(path);
            state.offset = 0;
            state_changed = true;
        }

        let mut next = info.state;
        let mut grew = false;
        let mut rollout_mtime = None;
        if let Some(path) = &state.rollout {
            match read_rollout(path, state.offset) {
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
                Err(e) => tracing::debug!("reading codex rollout: {e:#}"),
            }
            rollout_mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .map(DateTime::<Utc>::from);
        }

        // Approval prompts and questions aren't in the rollout.
        next = settle(next, grew, ctx.last_output.max(rollout_mtime), ctx.now);

        if next != info.state {
            obs.state = Some(next);
        }
        if state_changed {
            obs.provider_state = serde_json::to_value(&state).ok();
        }
        obs
    }
}

// ---------------------------------------------------------------------------
// Rollout files
// ---------------------------------------------------------------------------

/// New information from a rollout.
#[derive(Debug, Default)]
struct RolloutUpdate {
    offset: u64,
    state: Option<AgentState>,
    last_message: Option<String>,
}

fn read_rollout(path: &Path, offset: u64) -> Result<RolloutUpdate> {
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
    let mut update = RolloutUpdate {
        offset: offset + complete as u64,
        ..Default::default()
    };
    for line in buf[..complete].split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        apply_record(&record, &mut update);
    }
    Ok(update)
}

fn apply_record(record: &Value, update: &mut RolloutUpdate) {
    let payload = &record["payload"];
    match (record["type"].as_str(), payload["type"].as_str()) {
        (Some("session_meta"), _) => {
            update.state.get_or_insert(AgentState::Idle);
        }
        (Some("event_msg"), Some("task_started")) => update.state = Some(AgentState::Working),
        (Some("event_msg"), Some("task_complete")) => {
            update.state = Some(AgentState::WaitingForInput);
            if let Some(msg) = payload["last_agent_message"]
                .as_str()
                .filter(|m| !m.is_empty())
            {
                update.last_message = Some(excerpt(msg, MESSAGE_EXCERPT));
            }
        }
        (Some("event_msg"), Some("turn_aborted")) => update.state = Some(AgentState::Idle),
        (Some("event_msg"), Some("agent_message")) => {
            if let Some(msg) = payload["message"].as_str() {
                update.last_message = Some(excerpt(msg, MESSAGE_EXCERPT));
            }
        }
        (Some("response_item"), Some("message")) if payload["role"] == "assistant" => {
            let text: Vec<&str> = payload["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect();
            if !text.is_empty() {
                update.last_message = Some(excerpt(&text.join(" "), MESSAGE_EXCERPT));
            }
        }
        _ => {}
    }
}

struct SessionMeta {
    id: String,
    cwd: String,
    timestamp: DateTime<Utc>,
}

fn session_meta(path: &Path) -> Option<SessionMeta> {
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let record: Value = serde_json::from_str(&first).ok()?;
    if record["type"] != "session_meta" {
        return None;
    }
    let p = &record["payload"];
    Some(SessionMeta {
        id: p["id"].as_str()?.to_owned(),
        cwd: p["cwd"].as_str()?.to_owned(),
        timestamp: p["timestamp"]
            .as_str()
            .or(record["timestamp"].as_str())?
            .parse()
            .ok()?,
    })
}

/// The newest rollout started in `ctx.cwd` since the execution started and not
/// bound to another session.
fn discover(ctx: &ObserveContext<'_>) -> Option<(String, PathBuf)> {
    let earliest: Timestamp = ctx.started_at - Duration::seconds(CLOCK_SLACK);
    let cwd = canonical(ctx.cwd);
    let mut best: Option<(DateTime<Utc>, String, PathBuf)> = None;
    for path in recent_rollouts(ctx.env, earliest) {
        let Some(meta) = session_meta(&path) else {
            continue;
        };
        if canonical(&meta.cwd) != cwd
            || meta.timestamp < earliest
            || ctx.claimed.contains(&meta.id)
        {
            continue;
        }
        if best.as_ref().is_none_or(|(t, _, _)| meta.timestamp < *t) {
            best = Some((meta.timestamp, meta.id, path));
        }
    }
    best.map(|(_, id, path)| (id, path))
}

/// Rollout files in the day directories around now, modified since `since`.
fn recent_rollouts(env: &EnvMap, since: Timestamp) -> Vec<PathBuf> {
    let Some(dir) = sessions_dir(env) else {
        return Vec::new();
    };
    let today = Local::now().date_naive();
    let mut out = Vec::new();
    for day in [today - Duration::days(1), today, today + Duration::days(1)] {
        let Ok(entries) = std::fs::read_dir(dir.join(day.format("%Y/%m/%d").to_string())) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let recent = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|m| DateTime::<Utc>::from(m) >= since)
                .unwrap_or(true);
            if is_rollout(&path) && recent {
                out.push(path);
            }
        }
    }
    out
}

/// The rollout of a known conversation (file names end in `-<id>.jsonl`).
fn find_rollout(env: &EnvMap, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{id}.jsonl");
    let sessions = sessions_dir(env)?;
    // sessions/YYYY/MM/DD/rollout-…
    let mut dirs = vec![(sessions, 0)];
    while let Some((dir, depth)) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if depth < 3 && path.is_dir() {
                dirs.push((path, depth + 1));
            } else if depth == 3
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(&suffix))
            {
                return Some(path);
            }
        }
    }
    None
}

/// The rollout file a running Codex process has open (Linux).
fn open_rollout(pid: u32) -> Option<(String, PathBuf)> {
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    fds.flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .find(|p| is_rollout(p))
        .and_then(|path| session_meta(&path).map(|meta| (meta.id, path)))
}

fn is_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
}

/// `path` with symlinks resolved, so `/tmp/x` and `/private/tmp/x` match.
fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

fn sessions_dir(env: &EnvMap) -> Option<PathBuf> {
    let home = match env.get("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(env.get("HOME")?).join(".codex"),
    };
    Some(home.join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::Write;

    fn info(session: Option<&str>, prompt: Option<&str>) -> AgentInfo {
        AgentInfo {
            provider: "codex".into(),
            provider_session_id: session.map(Into::into),
            provider_state: Value::Null,
            state: AgentState::Starting,
            state_since: Utc::now(),
            last_message: None,
            prompt: prompt.map(Into::into),
        }
    }

    fn fake_codex_env(dir: &Path) -> EnvMap {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let codex = bin.join("codex");
        std::fs::write(&codex, "#!/bin/sh\necho codex-cli 9.9.9\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = EnvMap::new();
        env.insert("PATH".into(), bin.to_string_lossy().into_owned());
        env.insert(
            "CODEX_HOME".into(),
            dir.join("codex-home").to_string_lossy().into_owned(),
        );
        env
    }

    fn ctx<'a>(
        env: &'a EnvMap,
        cwd: &'a str,
        started_at: Timestamp,
        claimed: &'a HashSet<String>,
    ) -> ObserveContext<'a> {
        ObserveContext {
            cwd,
            started_at,
            pid: None,
            last_output: None,
            now: Utc::now(),
            env,
            claimed,
        }
    }

    fn write_rollout(env: &EnvMap, name: &str, id: &str, cwd: &str, ts: DateTime<Utc>) -> PathBuf {
        let day = Local::now().date_naive().format("%Y/%m/%d").to_string();
        let dir = sessions_dir(env).unwrap().join(day);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-{name}-{id}.jsonl"));
        let meta = serde_json::json!({
            "timestamp": ts.to_rfc3339(),
            "type": "session_meta",
            "payload": {"id": id, "session_id": id, "cwd": cwd, "timestamp": ts.to_rfc3339(), "originator": "codex-tui"}
        });
        std::fs::write(&path, format!("{meta}\n")).unwrap();
        path
    }

    /// Apply an observation like the generic reconciler does.
    fn apply(info: &mut AgentInfo, obs: Observation) {
        if let Some(s) = obs.state {
            info.state = s;
        }
        if let Some(id) = obs.provider_session_id {
            info.provider_session_id = Some(id);
        }
        if let Some(st) = obs.provider_state {
            info.provider_state = st;
        }
        if let Some(m) = obs.last_message {
            info.last_message = Some(m);
        }
    }

    #[tokio::test]
    async fn detects_installation() {
        let dir = tempfile::tempdir().unwrap();
        let cap = Codex.detect(&fake_codex_env(dir.path())).await;
        assert!(cap.available && cap.can_resume);
        assert_eq!(cap.version.as_deref(), Some("codex-cli 9.9.9"));
        assert!(!Codex.detect(&EnvMap::new()).await.available);
    }

    #[test]
    fn launch_and_resume_command_lines() {
        let dir = tempfile::tempdir().unwrap();
        let env = fake_codex_env(dir.path());
        let argv = Codex
            .launch_argv(&info(None, Some("fix the tests")), &env)
            .unwrap();
        assert_eq!(argv[1..], ["--no-daemon", "fix the tests"]);
        let argv = Codex
            .launch_argv(&info(Some("0199-abc"), Some("ignored")), &env)
            .unwrap();
        assert_eq!(argv[1..], ["--no-daemon", "resume", "0199-abc"]);
        assert!(
            Codex
                .launch_argv(&info(None, None), &EnvMap::new())
                .is_err()
        );
    }

    #[test]
    fn discovers_rollout_by_cwd_and_start_time() {
        let dir = tempfile::tempdir().unwrap();
        let env = fake_codex_env(dir.path());
        let launched = Utc::now();
        write_rollout(&env, "old", "id-old", "/w/a", launched - Duration::hours(2));
        write_rollout(&env, "other", "id-other", "/w/b", launched);
        let mine = write_rollout(
            &env,
            "mine",
            "id-mine",
            "/w/a",
            launched + Duration::seconds(1),
        );
        let later = write_rollout(
            &env,
            "later",
            "id-later",
            "/w/a",
            launched + Duration::seconds(30),
        );
        let none = HashSet::new();
        assert_eq!(
            discover(&ctx(&env, "/w/a", launched, &none)),
            Some(("id-mine".into(), mine))
        );
        // A second agent in the same workspace gets the next conversation.
        let taken = HashSet::from(["id-mine".to_owned()]);
        assert_eq!(
            discover(&ctx(&env, "/w/a", launched, &taken)),
            Some(("id-later".into(), later.clone()))
        );
        assert_eq!(find_rollout(&env, "id-later"), Some(later));
    }

    #[test]
    fn rollout_drives_state_through_opaque_provider_state() {
        let dir = tempfile::tempdir().unwrap();
        let env = fake_codex_env(dir.path());
        let started = Utc::now();
        let path = write_rollout(&env, "t", "id-t", "/w", started);
        let none = HashSet::new();
        let mut agent = info(None, Some("go"));

        let obs = Codex.observe(&ctx(&env, "/w", started, &none), &agent);
        assert_eq!(obs.provider_session_id.as_deref(), Some("id-t"));
        assert_eq!(obs.state, Some(AgentState::Idle));
        apply(&mut agent, obs);
        assert!(agent.provider_state.get("rollout").is_some());

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"1"}}}}"#
        )
        .unwrap();
        // Partial line: not consumed yet.
        write!(f, r#"{{"type":"event_msg","payload":{{"type":"task_compl"#).unwrap();
        let obs = Codex.observe(&ctx(&env, "/w", started, &none), &agent);
        assert_eq!(obs.state, Some(AgentState::Working));
        apply(&mut agent, obs);

        writeln!(
            f,
            r#"ete","last_agent_message":"Done.\n\nAll   tests pass."}}}}"#
        )
        .unwrap();
        let obs = Codex.observe(&ctx(&env, "/w", started, &none), &agent);
        assert_eq!(obs.state, Some(AgentState::WaitingForInput));
        assert_eq!(obs.last_message.as_deref(), Some("Done. All tests pass."));
        apply(&mut agent, obs);

        // Nothing new: nothing changes.
        let obs = Codex.observe(&ctx(&env, "/w", started, &none), &agent);
        assert!(obs.state.is_none() && obs.provider_state.is_none());
    }

    #[test]
    fn quiet_turn_is_blocked_and_quiet_start_is_idle() {
        let dir = tempfile::tempdir().unwrap();
        let env = fake_codex_env(dir.path());
        let none = HashSet::new();
        let mut c = ctx(&env, "/nowhere", Utc::now(), &none);
        c.last_output = Some(c.now - Duration::seconds(60));
        let mut agent = info(None, None);
        assert_eq!(Codex.observe(&c, &agent).state, Some(AgentState::Idle));
        agent.state = AgentState::Working;
        assert_eq!(Codex.observe(&c, &agent).state, Some(AgentState::Blocked));
        c.last_output = Some(c.now);
        agent.state = AgentState::Blocked;
        assert_eq!(Codex.observe(&c, &agent).state, Some(AgentState::Working));
    }
}
