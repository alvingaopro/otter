//! Reconciliation: bring recorded state in line with what is actually
//! happening — process exits (from the execution backend) and agent progress
//! (from agent providers) — and raise attention for what changed.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use otter_core::{AgentState, ExecutionState, Workspace};
use otter_protocol::Event;

use crate::agents::{self, ObserveContext, Screen};
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
        let screens = self.watch_screens(&store.state.workspaces, now).await;
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
                let dir = self.paths.agents_dir.join(ws.sessions[i].id.as_str());
                let seen = Seen {
                    now,
                    claimed: &claimed,
                    env: &env,
                    screens: &screens,
                    dir: &dir,
                };
                changed |= observe_agent(ws, i, &seen, &mut events);
            }
        }
        if changed || !events.is_empty() {
            store.save()?;
        }
        drop(store);
        self.emit_all(events);
        Ok(())
    }

    /// How long each agent's screen has looked as it does, by backend ref.
    /// Only agents that may be mid-turn are looked at (one capture each).
    async fn watch_screens(
        &self,
        workspaces: &[Workspace],
        now: DateTime<Utc>,
    ) -> HashMap<String, Screen> {
        let watched: Vec<String> = workspaces
            .iter()
            .flat_map(|w| &w.sessions)
            .filter(|s| {
                s.agent.as_ref().is_some_and(|a| {
                    matches!(
                        a.state,
                        AgentState::Starting | AgentState::Working | AgentState::Blocked
                    )
                })
            })
            .filter_map(|s| s.current_execution().filter(|e| e.is_running()))
            .map(|e| e.backend_ref.clone())
            .collect();
        let mut digests = Vec::new();
        for r in &watched {
            if let Ok(text) = self.backend.capture(r, Some(0)).await {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut h);
                digests.push((r.clone(), h.finish()));
            }
        }
        let mut screens = self.screens.lock().unwrap();
        screens.retain(|r, _| watched.contains(r));
        for (r, digest) in digests {
            let seen = |changed| Screen {
                since: now,
                changed,
            };
            match screens.get_mut(&r) {
                Some((d, _)) if *d == digest => {}
                Some(entry) => *entry = (digest, seen(true)),
                None => {
                    screens.insert(r, (digest, seen(false)));
                }
            }
        }
        screens.iter().map(|(r, (_, s))| (r.clone(), *s)).collect()
    }
}

/// What one reconciliation pass knows, for observing agents.
struct Seen<'a> {
    now: DateTime<Utc>,
    claimed: &'a HashSet<String>,
    env: &'a EnvMap,
    /// How long each agent's screen has looked as it does, by backend ref.
    screens: &'a HashMap<String, Screen>,
    /// The session's private directory.
    dir: &'a std::path::Path,
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
fn observe_agent(ws: &mut Workspace, i: usize, seen: &Seen<'_>, events: &mut Vec<Event>) -> bool {
    let now = seen.now;
    let session = &ws.sessions[i];
    let (Some(mut info), Some(exec)) = (session.agent.clone(), session.current_execution()) else {
        return false;
    };
    let mut changed = false;
    let mut blocker = None;
    let next = if !exec.is_running() {
        AgentState::Exited
    } else if let Some(provider) = agents::provider(&info.provider) {
        let ctx = ObserveContext {
            cwd: &ws.root,
            dir: seen.dir,
            started_at: exec.started_at,
            pid: exec.pid,
            screen: seen.screens.get(&exec.backend_ref).copied(),
            now,
            env: seen.env,
            claimed: seen.claimed,
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
        blocker = obs.blocker;
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
            blocker.as_ref(),
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
