//! Reconciliation: bring recorded state in line with what is actually
//! happening — process exits (from the execution backend) and agent progress
//! (from agent providers) — and raise attention for what changed.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use otter_core::{AgentState, ExecutionId, ExecutionState, SessionId, TAIL_LINES, Workspace};
use otter_protocol::Event;

use crate::agents::{self, ObserveContext, Screen};
use crate::attention;
use crate::backend::ProcessState;
use crate::daemon::Daemon;
use crate::env::EnvMap;

impl Daemon {
    /// Runs at startup (recovering from a daemon restart) and then
    /// periodically. One pass at a time: what one leaves for the next
    /// (`dying`, `screens`, `reconciled_once`) assumes it, so a pass waits
    /// for one still going.
    ///
    /// The backend is asked without the store's lock (D-159): what runs is
    /// read under it, the backend asked about that, and what it says applied
    /// under the lock again, only to executions still running as they were.
    /// One started meanwhile is the next pass's to look at.
    pub async fn reconcile(&self) -> anyhow::Result<()> {
        let _pass = self.reconciling.lock().await;
        let mut looked = Looked::at(&self.store.lock().await.state.workspaces);
        let observed = self.backend.inspect().await?;
        let now = Utc::now();

        // What the ones that just ended printed last, while the backend still
        // holds their output (D-076). A process seen dead is taken as ended
        // a moment later: the backend may mark it dead before it has drawn
        // its last lines — the ones that say why.
        let dead: Vec<String> = looked
            .running
            .values()
            .filter(|r| matches!(observed.get(*r), Some(ProcessState::Dead { .. })))
            .cloned()
            .collect();
        // What died while otterd was down has long settled.
        let first = !self
            .reconciled_once
            .swap(true, std::sync::atomic::Ordering::SeqCst);
        let ended: Vec<String> = {
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
            // Still settling: the next pass's.
            looked.running.retain(|_, r| !settling.contains(r));
            ended
        };
        // One look at each watched screen the backend still holds, and at
        // what the ended ones printed last.
        let shown: Vec<&str> = looked
            .watched
            .iter()
            .map(String::as_str)
            .filter(|r| observed.contains_key(*r))
            .collect();
        let targets: Vec<(&str, Option<u32>)> = shown
            .iter()
            .map(|r| (*r, Some(0)))
            .chain(
                ended
                    .iter()
                    .map(|r| (r.as_str(), Some(TAIL_LINES as u32 * 2))),
            )
            .collect();
        let mut captures = self.backend.capture_each(&targets).await;
        // One result a target; any missing is a look that saw nothing.
        debug_assert_eq!(captures.len(), targets.len());
        captures.resize_with(targets.len(), || Err(anyhow::anyhow!("not captured")));
        let last_lines = captures.split_off(shown.len());
        let digests: HashMap<&str, u64> = shown
            .into_iter()
            .zip(captures)
            .filter_map(|(r, text)| {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.ok()?.hash(&mut h);
                Some((r, h.finish()))
            })
            .collect();
        let tails: HashMap<String, String> = ended
            .into_iter()
            .zip(last_lines)
            .filter_map(|(r, text)| Some((r, otter_core::tail_of(&text.ok()?)?)))
            .collect();

        let mut store = self.store.lock().await;
        // Read under the store's lock: a terminal that comes on screen after
        // this settles what this pass raises (D-137).
        let on_screen = self.on_screen();
        let screens = self.watch_screens(&looked.watched, &digests, &store.state.workspaces, now);
        // The native sessions of managed runs' conversations (D-162): a run
        // works in the same directory as a session and writes a transcript
        // there; a session that took it for its own would follow the run's
        // turns, and resume its conversation.
        let managed: HashSet<String> = self.conversations.native_sessions().into_iter().collect();
        let claimed: HashSet<String> = store
            .state
            .workspaces
            .iter()
            .flat_map(|w| &w.sessions)
            .filter_map(|s| s.agent.as_ref()?.provider_session_id.clone())
            .chain(managed.iter().cloned())
            .collect();
        let mut events = Vec::new();
        let mut changed = false;
        for ws in &mut store.state.workspaces {
            changed |= reconcile_executions(
                ws,
                &looked.running,
                &observed,
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
                    managed: &managed,
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

    /// How long each agent's screen has looked as it does, by backend ref,
    /// with this pass's looks (`digests`, of `watched`) taken in. An agent
    /// stopped, restarted or deleted since it was looked at is let go.
    fn watch_screens(
        &self,
        watched: &[String],
        digests: &HashMap<&str, u64>,
        workspaces: &[Workspace],
        now: DateTime<Utc>,
    ) -> HashMap<String, Screen> {
        let running: HashSet<&str> = workspaces
            .iter()
            .flat_map(|w| &w.sessions)
            .filter_map(|s| s.current_execution().filter(|e| e.is_running()))
            .map(|e| e.backend_ref.as_str())
            .collect();
        let mut screens = self.screens.lock().unwrap();
        screens.retain(|r, _| watched.contains(r) && running.contains(r.as_str()));
        for r in watched.iter().filter(|r| running.contains(r.as_str())) {
            match (screens.get_mut(r), digests.get(r.as_str())) {
                (Some((d, s)), Some(&digest)) => {
                    *s = s.next(*d != digest, now);
                    *d = digest;
                }
                // Not seen this time, so not seen moving.
                (Some((_, s)), None) => s.moving_since = None,
                (None, Some(&digest)) => {
                    screens.insert(r.clone(), (digest, Screen::first(now)));
                }
                (None, None) => {}
            }
        }
        screens.iter().map(|(r, (_, s))| (r.clone(), *s)).collect()
    }
}

/// What a pass asks the backend about, read under the store's lock: the
/// running executions, by id, and the backend refs of the agents among them
/// that may be mid-turn (one screen look each).
struct Looked {
    running: HashMap<ExecutionId, String>,
    watched: Vec<String>,
}

impl Looked {
    fn at(workspaces: &[Workspace]) -> Self {
        let running = || {
            workspaces
                .iter()
                .flat_map(|w| &w.sessions)
                .filter_map(|s| Some((s, s.current_execution().filter(|e| e.is_running())?)))
        };
        Looked {
            running: running()
                .map(|(_, e)| (e.id.clone(), e.backend_ref.clone()))
                .collect(),
            watched: running()
                .filter(|(s, _)| {
                    s.agent.as_ref().is_some_and(|a| {
                        matches!(
                            a.state,
                            AgentState::Starting | AgentState::Working | AgentState::Blocked
                        )
                    })
                })
                .map(|(_, e)| e.backend_ref.clone())
                .collect(),
        }
    }
}

/// What one reconciliation pass knows, for observing agents.
struct Seen<'a> {
    now: DateTime<Utc>,
    claimed: &'a HashSet<String>,
    managed: &'a HashSet<String>,
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

/// `running`: the executions the backend was asked about, by id, with their
/// backend refs; only these are decided on.
fn reconcile_executions(
    ws: &mut Workspace,
    running: &HashMap<ExecutionId, String>,
    observed: &HashMap<String, ProcessState>,
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
        // One started after the backend was asked (a restart, a new
        // session) isn't in what it said (D-159).
        if !exec.is_running() || running.get(&exec.id) != Some(&exec.backend_ref) {
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
            managed: seen.managed,
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use otter_core::{AgentInfo, Execution, Session, SessionKind, WorkspaceId, WorkspaceState};
    use otter_protocol::SessionRef;
    use tokio::sync::Notify;

    use super::*;
    use crate::backend::{AttachCommand, ExecutionBackend, LaunchSpec, Launched};
    use crate::env::ResolvedEnv;
    use crate::paths::Paths;

    /// Processes in a map. `inspect` takes its look, then waits for the test
    /// to let it answer; each capture shows its process as it was then.
    #[derive(Default)]
    struct Paused {
        procs: Mutex<HashMap<String, ProcessState>>,
        looked: Notify,
        answer: Notify,
    }

    #[async_trait::async_trait]
    impl ExecutionBackend for Paused {
        fn name(&self) -> &'static str {
            "paused"
        }
        async fn launch(&self, spec: &LaunchSpec) -> anyhow::Result<Launched> {
            let alive = ProcessState::Alive { pid: Some(7) };
            self.procs
                .lock()
                .unwrap()
                .insert(spec.backend_ref.clone(), alive);
            Ok(Launched { pid: Some(7) })
        }
        async fn inspect(&self) -> anyhow::Result<HashMap<String, ProcessState>> {
            let seen = self.procs.lock().unwrap().clone();
            self.looked.notify_one();
            self.answer.notified().await;
            Ok(seen)
        }
        async fn terminate(&self, backend_ref: &str) -> anyhow::Result<()> {
            self.procs.lock().unwrap().remove(backend_ref);
            Ok(())
        }
        async fn capture(&self, backend_ref: &str, _: Option<u32>) -> anyhow::Result<String> {
            Ok(format!("{backend_ref} failed: no such file\n"))
        }
        async fn send_text(&self, _: &str, _: &str, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn attach_command(&self, _: &str, _: &str) -> AttachCommand {
            unreachable!()
        }
        async fn detach_client(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn session(ws: &WorkspaceId, name: &str, agent: bool, exec: &str) -> Session {
        let now = Utc::now();
        Session {
            id: SessionId::generate(),
            workspace_id: ws.clone(),
            name: name.into(),
            kind: if agent {
                SessionKind::Agent
            } else {
                SessionKind::Terminal
            },
            command: None,
            created_at: now,
            launch_error: None,
            // A provider this build doesn't have: only the screen is watched.
            agent: agent.then(|| AgentInfo {
                provider: "test".into(),
                provider_session_id: None,
                provider_state: serde_json::Value::Null,
                state: AgentState::Working,
                state_since: now,
                last_message: None,
                prompt: None,
                blocker: None,
            }),
            executions: vec![Execution {
                id: exec.into(),
                backend: "paused".into(),
                backend_ref: exec.into(),
                pid: Some(7),
                state: ExecutionState::Running,
                exit_code: None,
                started_at: now,
                ended_at: None,
                tail: None,
            }],
        }
    }

    #[tokio::test]
    async fn what_changes_while_the_backend_is_asked_is_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let mut store = crate::store::Store::load(&paths.state_file).unwrap();
        let ws_id = WorkspaceId::generate();
        let now = Utc::now();
        store.state.workspaces.push(Workspace {
            id: ws_id.clone(),
            name: "w".into(),
            root: dir.path().to_string_lossy().into_owned(),
            brief: Default::default(),
            source: Default::default(),
            environment: Default::default(),
            state: WorkspaceState::Ready,
            state_message: None,
            created_at: now,
            updated_at: now,
            sessions: vec![
                session(&ws_id, "shell", false, "exec_shell"),
                session(&ws_id, "build", false, "exec_build"),
                session(&ws_id, "agent", true, "exec_agent"),
                session(&ws_id, "gone", true, "exec_gone"),
            ],
            attention: Vec::new(),
            task_id: None,
        });
        let backend = Arc::new(Paused::default());
        backend.procs.lock().unwrap().extend([
            ("exec_shell".into(), ProcessState::Alive { pid: Some(7) }),
            (
                "exec_build".into(),
                ProcessState::Dead { exit_code: Some(2) },
            ),
            ("exec_agent".into(), ProcessState::Alive { pid: Some(7) }),
            ("exec_gone".into(), ProcessState::Alive { pid: Some(7) }),
        ]);
        let tasks = crate::tasks::TaskStore::load(&paths.tasks_dir).unwrap();
        let events = crate::events::EventLog::open(&paths.events_file).unwrap();
        let env = ResolvedEnv {
            vars: EnvMap::from([("HOME".into(), dir.path().to_string_lossy().into_owned())]),
            source: "test".into(),
        };
        let d = Arc::new(Daemon::new(
            paths,
            store,
            tasks,
            backend.clone(),
            events,
            env,
            "/bin/sh".into(),
        ));

        let pass = tokio::spawn({
            let d = d.clone();
            async move { d.reconcile().await }
        });
        backend.looked.notified().await;
        // Between the backend's look and its answer: the shell is restarted
        // and an agent deleted. Neither waits for the pass.
        let shell = SessionRef {
            workspace: "w".into(),
            session: "shell".into(),
        };
        let gone = SessionRef {
            workspace: "w".into(),
            session: "gone".into(),
        };
        let free = Duration::from_secs(5);
        tokio::time::timeout(free, d.session_restart(&shell))
            .await
            .expect("the store isn't held while the backend is asked")
            .unwrap();
        tokio::time::timeout(free, d.session_delete(&gone))
            .await
            .unwrap()
            .unwrap();
        backend.answer.notify_one();
        pass.await.unwrap().unwrap();

        let store = d.store.lock().await;
        let ws = &store.state.workspaces[0];
        // The new shell started after the look: not lost for being missing
        // from it.
        let shell = ws.session("shell").unwrap();
        assert_eq!(shell.executions.len(), 2);
        assert_eq!(shell.executions[0].state, ExecutionState::Stopped);
        assert!(shell.executions[1].is_running(), "{:?}", shell.executions);
        // What didn't change is taken as it was seen: the build ended (with
        // its last lines), the agent's screen is watched.
        let build = ws.session("build").unwrap().current_execution().unwrap();
        assert_eq!(build.state, ExecutionState::Exited);
        assert_eq!(build.exit_code, Some(2));
        assert_eq!(
            build.tail.as_deref(),
            Some("exec_build failed: no such file")
        );
        // The deleted agent's screen, seen before it went, doesn't come back.
        let screens = d.screens.lock().unwrap();
        assert!(screens.contains_key("exec_agent"));
        assert!(!screens.contains_key("exec_gone"), "{:?}", screens.keys());
        assert!(ws.session("gone").is_none());
    }
}
