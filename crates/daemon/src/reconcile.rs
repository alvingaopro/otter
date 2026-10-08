//! Reconciliation: bring recorded state in line with what is actually
//! happening — process exits (from the execution backend) and agent progress
//! (from agent providers) — and raise attention for what changed.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use otter_core::{AgentState, ExecutionState, Workspace};
use otter_protocol::Event;

use crate::agents::{self, ObserveContext};
use crate::attention;
use crate::backend::ProcessState;
use crate::daemon::Daemon;
use crate::env::EnvMap;

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
            .filter_map(|s| s.agent.as_ref()?.provider_session_id.clone())
            .collect();

        let mut events = Vec::new();
        let mut changed = false;
        for ws in &mut store.state.workspaces {
            changed |= reconcile_executions(ws, &observed, now, &mut events);
            if ws.sessions.iter().all(|s| s.agent.is_none()) {
                continue;
            }
            // Providers see the environment their agents run in.
            let env = self
                .workspace_env
                .lock()
                .unwrap()
                .get(&ws.id)
                .cloned()
                .unwrap_or_else(|| self.environments.base().clone());
            for i in 0..ws.sessions.len() {
                changed |= observe_agent(ws, i, &observed, now, &claimed, &env, &mut events);
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

/// Advance one agent session's state. How the agent is observed is entirely
/// up to its provider; this only applies the result and raises attention.
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
    let (Some(mut info), Some(exec)) = (session.agent.clone(), session.current_execution()) else {
        return false;
    };
    let mut changed = false;
    let next = if !exec.is_running() {
        AgentState::Exited
    } else if let Some(provider) = agents::provider(&info.provider) {
        let last_output = match observed.get(&exec.backend_ref) {
            Some(ProcessState::Alive { last_output, .. }) => {
                last_output.and_then(|t| DateTime::from_timestamp(t, 0))
            }
            _ => None,
        };
        let ctx = ObserveContext {
            cwd: &ws.root,
            started_at: exec.started_at,
            pid: exec.pid,
            last_output,
            now,
            env,
            claimed,
        };
        let obs = provider.observe(&ctx, &info);
        if let Some(id) = obs.provider_session_id {
            tracing::info!(session = %session.name, "agent session identified");
            info.provider_session_id = Some(id);
            // The conversation exists now; restarts resume it instead.
            info.prompt = None;
            changed = true;
        }
        if let Some(state) = obs.provider_state {
            info.provider_state = state;
            changed = true;
        }
        if let Some(message) = obs.last_message {
            changed |= info.last_message.as_ref() != Some(&message);
            info.last_message = Some(message);
        }
        obs.state.unwrap_or(info.state)
    } else {
        info.state
    };

    if next != info.state {
        tracing::info!(session = %session.name, from = info.state.as_str(), to = next.as_str(), "agent state");
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
