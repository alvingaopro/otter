//! Codex integration.
//!
//! - Launch: `codex --no-daemon [prompt]`; resume: `codex --no-daemon resume
//!   <id>`. `--no-daemon` keeps the agent inside the process Workd manages (by
//!   default the Codex TUI attaches to a shared, self-updating background
//!   daemon, and the work would live outside the session).
//! - Identity: the rollout file `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`
//!   whose first record (`session_meta`) carries the conversation id and cwd.
//!   Codex creates it lazily, so it is discovered after launch: from the
//!   process's open files when possible, else by cwd and start time.
//! - State: `event_msg` records in the rollout — `task_started` (working),
//!   `task_complete` (turn finished, with `last_agent_message`),
//!   `turn_aborted`.
//!
//! The rollout format belongs to a self-updating binary: everything here
//! parses defensively and ignores what it doesn't know.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, Utc};
use serde_json::Value;
use workd_core::{AgentInfo, AgentState};

use super::{AgentProvider, DiscoverContext, TranscriptUpdate, excerpt};
use crate::env::{EnvMap, which};

/// Tolerance between our launch time and Codex's own timestamps.
const CLOCK_SLACK: i64 = 5;
const MESSAGE_EXCERPT: usize = 240;

pub struct Codex;

impl AgentProvider for Codex {
    fn argv(&self, info: &AgentInfo, env: &EnvMap) -> Result<Vec<String>> {
        let codex = which("codex", env)
            .context("codex is not installed on this host (not on the login PATH)")?;
        let mut argv = vec![codex.to_string_lossy().into_owned(), "--no-daemon".into()];
        match (&info.resume_id, &info.prompt) {
            (Some(id), _) => {
                argv.push("resume".into());
                argv.push(id.clone());
            }
            (None, Some(prompt)) => argv.push(prompt.clone()),
            (None, None) => {}
        }
        Ok(argv)
    }

    fn discover(&self, ctx: &DiscoverContext<'_>) -> Option<(String, PathBuf)> {
        if let Some(found) = ctx.pid.and_then(open_rollout) {
            return Some(found);
        }
        let dir = sessions_dir(ctx.env)?;
        let earliest = ctx.started_at - Duration::seconds(CLOCK_SLACK);
        let cwd = canonical(ctx.cwd);
        let today = Local::now().date_naive();
        let mut best: Option<(DateTime<Utc>, String, PathBuf)> = None;
        for day in [today - Duration::days(1), today, today + Duration::days(1)] {
            let day_dir = dir.join(day.format("%Y/%m/%d").to_string());
            let Ok(entries) = std::fs::read_dir(&day_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !is_rollout(&path) {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(DateTime::<Utc>::from);
                if modified.is_ok_and(|m| m < earliest) {
                    continue;
                }
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
        }
        best.map(|(_, id, path)| (id, path))
    }

    fn read_transcript(&self, path: &Path, offset: u64) -> Result<TranscriptUpdate> {
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
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            apply_record(&record, &mut update);
        }
        Ok(update)
    }
}

fn apply_record(record: &Value, update: &mut TranscriptUpdate) {
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

    fn info(resume: Option<&str>, prompt: Option<&str>) -> AgentInfo {
        AgentInfo {
            provider: "codex".into(),
            resume_id: resume.map(Into::into),
            transcript: None,
            transcript_offset: 0,
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
        std::fs::write(&codex, "#!/bin/sh\n").unwrap();
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

    #[test]
    fn launch_and_resume_command_lines() {
        let dir = tempfile::tempdir().unwrap();
        let env = fake_codex_env(dir.path());
        let argv = Codex
            .argv(&info(None, Some("fix the tests")), &env)
            .unwrap();
        assert_eq!(argv[1..], ["--no-daemon", "fix the tests"]);
        let argv = Codex
            .argv(&info(Some("0199-abc"), Some("ignored")), &env)
            .unwrap();
        assert_eq!(argv[1..], ["--no-daemon", "resume", "0199-abc"]);
        assert!(Codex.argv(&info(None, None), &EnvMap::new()).is_err());
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
        let ctx = |claimed| DiscoverContext {
            cwd: "/w/a",
            started_at: launched,
            pid: None,
            env: &env,
            claimed,
        };
        let none = HashSet::new();
        assert_eq!(Codex.discover(&ctx(&none)), Some(("id-mine".into(), mine)));
        // A second agent in the same workspace gets the next conversation.
        let taken = HashSet::from(["id-mine".to_owned()]);
        assert_eq!(
            Codex.discover(&ctx(&taken)),
            Some(("id-later".into(), later))
        );
    }

    #[test]
    fn transcript_drives_state() {
        let dir = tempfile::tempdir().unwrap();
        let env = fake_codex_env(dir.path());
        let path = write_rollout(&env, "t", "id-t", "/w", Utc::now());
        let up = Codex.read_transcript(&path, 0).unwrap();
        assert_eq!(up.state, Some(AgentState::Idle));

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
        let up2 = Codex.read_transcript(&path, up.offset).unwrap();
        assert_eq!(up2.state, Some(AgentState::Working));

        writeln!(
            f,
            r#"ete","last_agent_message":"Done.\n\nAll   tests pass."}}}}"#
        )
        .unwrap();
        let up3 = Codex.read_transcript(&path, up2.offset).unwrap();
        assert_eq!(up3.state, Some(AgentState::WaitingForInput));
        assert_eq!(up3.last_message.as_deref(), Some("Done. All tests pass."));
        let up4 = Codex.read_transcript(&path, up3.offset).unwrap();
        assert_eq!(up4.state, None);
        assert_eq!(up4.offset, up3.offset);
    }
}
