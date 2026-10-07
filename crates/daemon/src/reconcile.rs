//! Reconciliation: bring recorded state in line with what is actually
//! happening — process exits (from the execution backend) and agent progress
//! (from agent transcripts) — and raise attention for what changed.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use workd_core::{AgentState, ExecutionState, Workspace};
use workd_protocol::Event;

use crate::agents::{self, DiscoverContext};
use crate::attention;
use crate::backend::ProcessState;
use crate::daemon::Daemon;
use crate::env::EnvMap;

/// An agent mid-turn whose terminal and transcript have both been quiet this
/// long is considered blocked on the developer (approval or question). A
/// heuristic: Codex doesn't record approval requests in its transcript.
const DEFAULT_QUIET_SECS: i64 = 8;

impl Daemon {
    /// Runs at startup (recovering from a daemon restart) and then
    /// periodically.
    pub async fn reconcile(&self) -> anyhow::Result<()> {
        let mut store = self.store.lock().await;
        let observed = self.backend.inspect().await?;
        let now = Utc::now();
        let claimed: HashSet<String> = store
            .state
            .workspaces
            .iter()
            .flat_map(|w| &w.sessions)
            .filter_map(|s| s.agent.as_ref()?.resume_id.clone())
            .collect();

        let mut events = Vec::new();
        let mut changed = false;
        for ws in &mut store.state.workspaces {
            changed |= reconcile_executions(ws, &observed, now, &mut events);
            for i in 0..ws.sessions.len() {
                changed |= observe_agent(
                    ws,
                    i,
                    &observed,
                    now,
                    &claimed,
                    self.environments.base(),
                    &mut events,
                );
            }
        }
        if changed || !events.is_empty() {
            store.save()?;
        }
        drop(store);
        self.emit_all(events);
        Ok(())
    }
}

fn reconcile_executions(
    ws: &mut Workspace,
    observed: &HashMap<String, ProcessState>,
    now: DateTime<Utc>,
    events: &mut Vec<Event>,
) -> bool {
    let mut ended = Vec::new();
    let mut changed = false;
    for (i, session) in ws.sessions.iter_mut().enumerate() {
        let session_id = session.id.clone();
        let Some(exec) = session.current_execution_mut() else {
            continue;
        };
        if !exec.is_running() {
            continue;
        }
        match observed.get(&exec.backend_ref) {
            Some(ProcessState::Alive { pid, .. }) => {
                if exec.pid.is_none() && pid.is_some() {
                    exec.pid = *pid;
                    changed = true;
                }
                continue;
            }
            Some(ProcessState::Dead { exit_code }) => {
                exec.state = ExecutionState::Exited;
                exec.exit_code = *exit_code;
                exec.ended_at = Some(now);
                events.push(Event::ExecutionExited {
                    workspace_id: ws.id.clone(),
                    session_id,
                    execution_id: exec.id.clone(),
                    exit_code: *exit_code,
                });
            }
            None => {
                exec.state = ExecutionState::Lost;
                exec.ended_at = Some(now);
                events.push(Event::ExecutionLost {
                    workspace_id: ws.id.clone(),
                    session_id,
                    execution_id: exec.id.clone(),
                });
            }
        }
        ended.push(i);
        changed = true;
    }
    if changed {
        ws.updated_at = now;
    }
    for i in ended {
        let session = ws.sessions[i].clone();
        events.extend(attention::on_execution_ended(ws, &session));
    }
    changed
}

/// Advance one agent session's state from its process and transcript.
fn observe_agent(
    ws: &mut Workspace,
    i: usize,
    observed: &HashMap<String, ProcessState>,
    now: DateTime<Utc>,
    claimed: &HashSet<String>,
    env: &EnvMap,
    events: &mut Vec<Event>,
) -> bool {
    let session = &ws.sessions[i];
    let (Some(info), Some(exec)) = (session.agent.clone(), session.current_execution()) else {
        return false;
    };
    let mut info = info;
    let mut changed = false;
    let mut next = info.state;
    let mut quiet_secs = None;

    if !exec.is_running() {
        next = AgentState::Exited;
    } else if let Some(provider) = agents::provider(&info.provider) {
        // Bind the conversation once Codex has created it.
        if info.transcript.is_none() {
            let ctx = DiscoverContext {
                cwd: &ws.root,
                started_at: exec.started_at,
                pid: exec.pid,
                env,
                claimed,
            };
            if let Some((id, path)) = provider.discover(&ctx) {
                tracing::info!(session = %session.name, conversation = %id, "agent conversation found");
                info.resume_id = Some(id);
                info.transcript = Some(path.to_string_lossy().into_owned());
                info.transcript_offset = 0;
                info.prompt = None;
                changed = true;
            }
        }

        let mut grew = false;
        let mut transcript_mtime = None;
        if let Some(path) = &info.transcript {
            match provider.read_transcript(path.as_ref(), info.transcript_offset) {
                Ok(update) => {
                    if update.offset != info.transcript_offset {
                        info.transcript_offset = update.offset;
                        grew = true;
                        changed = true;
                    }
                    if let Some(m) = update.last_message {
                        info.last_message = Some(m);
                    }
                    if let Some(state) = update.state {
                        next = state;
                    }
                }
                Err(e) => tracing::debug!("reading agent transcript: {e:#}"),
            }
            transcript_mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| DateTime::<Utc>::from(t).timestamp());
        }

        // Approval prompts and questions aren't in the transcript: infer them
        // from a turn that has gone completely quiet.
        let last_output = match observed.get(&exec.backend_ref) {
            Some(ProcessState::Alive { last_output, .. }) => *last_output,
            _ => None,
        };
        let last_activity = last_output.max(transcript_mtime);
        quiet_secs = last_activity.map(|t| now.timestamp() - t);
        if !grew {
            match (next, quiet_secs) {
                (AgentState::Working, Some(q)) if q >= quiet_threshold() => {
                    next = AgentState::Blocked;
                }
                (AgentState::Blocked, Some(q)) if q < 2 => next = AgentState::Working,
                // Launched and settled without starting a conversation: it's
                // waiting for its first prompt (or a startup dialog).
                (AgentState::Starting, Some(q)) if q >= quiet_threshold() => {
                    next = AgentState::Idle;
                }
                _ => {}
            }
        }
    }

    if next != info.state {
        tracing::info!(session = %session.name, from = info.state.as_str(), to = next.as_str(), quiet = ?quiet_secs, "agent state");
        info.state = next;
        info.state_since = now;
        changed = true;
        events.push(Event::AgentStateChanged {
            workspace_id: ws.id.clone(),
            session_id: session.id.clone(),
            state: next,
        });
        let (id, name) = (session.id.clone(), session.name.clone());
        let message = info.last_message.clone();
        events.extend(attention::on_agent_state(
            ws,
            &id,
            &name,
            next,
            message.as_deref(),
        ));
    }
    if changed {
        ws.sessions[i].agent = Some(info);
    }
    changed
}

fn quiet_threshold() -> i64 {
    std::env::var("WORKD_AGENT_QUIET_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_QUIET_SECS)
}
