//! Reconciliation: bring recorded state in line with what is actually
//! happening — process exits (from the execution backend) and agent progress
//! (from agent providers) — and raise attention for what changed.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use otter_core::{AgentState, ExecutionState, SessionId, TAIL_LINES, Workspace};
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

        // What the ones that just ended printed last, while the backend still
        // holds their output (D-076). A process seen dead is taken as ended
        // a moment later: the backend may mark it dead before it has drawn
        // its last lines — the ones that say why.
        let dead: Vec<String> = store
            .state
            .workspaces
            .iter()
            .flat_map(|w| &w.sessions)
            .filter_map(|s| s.current_execution().filter(|e| e.is_running()))
            .filter(|e| {
                matches!(
                    observed.get(&e.backend_ref),
                    Some(ProcessState::Dead { .. })
                )
            })
            .map(|e| e.backend_ref.clone())
            .collect();
        // What died while otterd was down has long settled.
        let first = !self
            .reconciled_once
            .swap(true, std::sync::atomic::Ordering::SeqCst);
        let (ended, settling): (Vec<String>, HashSet<String>) = {
            let mut dying = self.dying.lock().unwrap();
            dying.retain(|r, _| dead.contains(r));
            let (ended, settling): (Vec<String>, Vec<String>) = dead.into_iter().partition(|r| {
                first
                    || dying
                        .entry(r.clone())
                        .or_insert_with(std::time::Instant::now)
                        .elapsed()
                        >= SETTLE
            });
            for r in &ended {
                dying.remove(r);
            }
            (ended, settling.into_iter().collect())
        };
        let mut tails = HashMap::new();
        for r in ended {
            if let Ok(screen) = self.backend.capture(&r, Some(TAIL_LINES as u32 * 2)).await
                && let Some(t) = otter_core::tail_of(&screen)
            {
                tails.insert(r, t);
            }
        }

        // Read under the store's lock: a terminal that comes on screen after
        // this settles what this pass raises (D-137).
        let on_screen = self.on_screen();
        let mut events = Vec::new();
        let mut changed = false;
        for ws in &mut store.state.workspaces {
            changed |= reconcile_executions(
                ws,
                &observed,
                &settling,
                &tails,
                &on_screen,
                now,
                &mut events,
            );
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
                    on_screen: &on_screen,
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
            let digest = self.backend.capture(r, Some(0)).await.ok().map(|text| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut h);
                h.finish()
            });
            digests.push((r.clone(), digest));
        }
        let mut screens = self.screens.lock().unwrap();
        screens.retain(|r, _| watched.contains(r));
        for (r, digest) in digests {
            match (screens.get_mut(&r), digest) {
                (Some((d, s)), Some(digest)) => {
                    *s = s.next(*d != digest, now);
                    *d = digest;
                }
                // Not seen this time, so not seen moving.
                (Some((_, s)), None) => s.moving_since = None,
                (None, Some(digest)) => {
                    screens.insert(r, (digest, Screen::first(now)));
                }
                (None, None) => {}
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
    /// Sessions the developer is looking at (D-137).
    on_screen: &'a HashSet<SessionId>,
    /// The session's private directory.
    dir: &'a std::path::Path,
}

/// How long a dead process's output is given to settle before its end is taken.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

fn reconcile_executions(
    ws: &mut Workspace,
    observed: &HashMap<String, ProcessState>,
    settling: &HashSet<String>,
    tails: &HashMap<String, String>,
    on_screen: &HashSet<SessionId>,
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
        if !exec.is_running() || settling.contains(&exec.backend_ref) {
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
                exec.tail = tails.get(&exec.backend_ref).cloned();
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
        let seen = on_screen.contains(&session.id);
        events.extend(attention::on_execution_ended(ws, &session, seen));
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
        info.blocker = (next == AgentState::Blocked)
            .then(|| attention::blocked_item(&name, blocker.as_ref()).1);
        let on_screen = seen.on_screen.contains(&id);
        events.extend(attention::on_agent_state(
            ws,
            &id,
            blocker.as_ref(),
            &name,
            next,
            message.as_deref(),
            on_screen,
        ));
    } else if let (AgentState::Blocked, Some(b)) = (next, blocker.as_ref()) {
        // Still blocked, on another prompt (D-099): the next approval came
        // before a reconcile saw the agent at work in between.
        let (kind, summary) = attention::blocked_item(&session.name, Some(b));
        if info.blocker.as_deref() != Some(summary.as_str()) {
            let id = session.id.clone();
            info.blocker = Some(summary.clone());
            changed = true;
            events.extend(attention::raise_item(ws, Some(&id), kind, summary, false));
        }
    }
    if changed {
        ws.sessions[i].agent = Some(info);
    }
    changed
}
