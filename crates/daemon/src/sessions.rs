//! SessionManager / ExecutionManager: sessions and their executions.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use workd_core::{
    AgentInfo, AgentState, AttentionKind, EnvironmentKind, Execution, ExecutionId, ExecutionState,
    Session, SessionId, SessionKind, SessionSpec, Workspace, WorkspaceId, WorkspaceState,
    validate_name,
};
use workd_protocol::{
    AttachReady, ErrorCode, Event, RpcError, SessionCreate, SessionOutput, SessionRead, SessionRef,
    SessionWrite,
};

use crate::agents;
use crate::attention;
use crate::backend::LaunchSpec;
use crate::daemon::{Daemon, RpcResult, internal, not_running, save, workspace_not_found};
use crate::env::EnvMap;
use crate::store::Store;

/// What an attach connection needs to know about its target.
pub struct AttachTarget {
    pub ready: AttachReady,
    pub workspace_id: WorkspaceId,
    pub backend_ref: String,
}

impl Daemon {
    pub(crate) async fn session_create(self: &Arc<Self>, p: SessionCreate) -> RpcResult<Session> {
        validate_spec(&p.spec)?;
        let mut store = self.store.lock().await;
        let ws = store
            .workspace(&p.workspace)
            .cloned()
            .ok_or_else(|| workspace_not_found(&p.workspace))?;
        let session_id = self.add_session_record(&mut store, &ws.id, p.spec)?;
        // Sessions added while the workspace is still preparing start when it
        // becomes ready.
        if ws.state == WorkspaceState::Ready {
            let env = self.env_for_launch(&ws).await?;
            self.launch(&mut store, &ws.id, &session_id, &env).await?;
        }
        Ok(session_snapshot(&store, &ws.id, &session_id))
    }

    pub(crate) async fn session_stop(&self, r: &SessionRef) -> RpcResult<Session> {
        let mut store = self.store.lock().await;
        let (ws_id, session) = find_session(&store, r)?;
        if let Some(exec) = session.current_execution().filter(|e| e.is_running()) {
            self.backend
                .terminate(&exec.backend_ref)
                .await
                .map_err(internal)?;
            let exec_id = exec.id.clone();
            mark_ended(
                &mut store,
                &ws_id,
                &session.id,
                &exec_id,
                ExecutionState::Stopped,
            );
            self.events.emit(Event::SessionStopped {
                workspace_id: ws_id.clone(),
                session_id: session.id.clone(),
            });
        }
        let ws = store.workspace_mut(ws_id.as_str()).expect("exists");
        let events = attention::resolve_session(ws, &session.id);
        save(&store)?;
        self.emit_all(events);
        Ok(session_snapshot(&store, &ws_id, &session.id))
    }

    pub(crate) async fn session_restart(&self, r: &SessionRef) -> RpcResult<Session> {
        let mut store = self.store.lock().await;
        let (ws_id, session) = find_session(&store, r)?;
        let ws = store.workspace(ws_id.as_str()).cloned().expect("exists");
        if ws.state != WorkspaceState::Ready {
            return Err(RpcError::conflict(format!(
                "workspace `{}` is {}",
                ws.name,
                ws.state.as_str()
            )));
        }
        let env = self.env_for_launch(&ws).await?;
        if let Some(exec) = session.current_execution() {
            // Also releases an exited process's retained output.
            self.backend
                .terminate(&exec.backend_ref)
                .await
                .map_err(internal)?;
            if exec.is_running() {
                let exec_id = exec.id.clone();
                mark_ended(
                    &mut store,
                    &ws_id,
                    &session.id,
                    &exec_id,
                    ExecutionState::Stopped,
                );
                self.events.emit(Event::SessionStopped {
                    workspace_id: ws_id.clone(),
                    session_id: session.id.clone(),
                });
            }
        }
        let ws = store.workspace_mut(ws_id.as_str()).expect("exists");
        let events = attention::resolve_session(ws, &session.id);
        self.emit_all(events);
        self.launch(&mut store, &ws_id, &session.id, &env).await?;
        Ok(session_snapshot(&store, &ws_id, &session.id))
    }

    pub(crate) async fn session_delete(&self, r: &SessionRef) -> RpcResult<()> {
        let mut store = self.store.lock().await;
        let (ws_id, session) = find_session(&store, r)?;
        self.terminate_all(&session).await?;
        let ws = store.workspace_mut(ws_id.as_str()).expect("exists");
        let events = attention::resolve_session(ws, &session.id);
        ws.sessions.retain(|s| s.id != session.id);
        ws.updated_at = Utc::now();
        save(&store)?;
        self.emit_all(events);
        self.events.emit(Event::SessionDeleted {
            workspace_id: ws_id,
            session_id: session.id,
        });
        Ok(())
    }

    pub(crate) async fn session_read(&self, p: &SessionRead) -> RpcResult<SessionOutput> {
        let store = self.store.lock().await;
        let (_, session) = find_session(&store, &p.session)?;
        let exec = held_execution(&session)?;
        let text = self
            .backend
            .capture(&exec.backend_ref, p.lines)
            .await
            .map_err(internal)?;
        Ok(SessionOutput { text })
    }

    pub(crate) async fn session_write(self: &Arc<Self>, p: &SessionWrite) -> RpcResult<()> {
        let (ws_id, session) = {
            let store = self.store.lock().await;
            let (ws_id, session) = find_session(&store, &p.session)?;
            let exec = session
                .current_execution()
                .filter(|e| e.is_running())
                .ok_or_else(|| not_running(&session))?;
            self.backend
                .send_text(&exec.backend_ref, &p.data, p.enter)
                .await
                .map_err(internal)?;
            (ws_id, session)
        };
        self.engage(&ws_id, &session.id).await;
        Ok(())
    }

    pub async fn attach_target(&self, r: &SessionRef) -> RpcResult<AttachTarget> {
        let store = self.store.lock().await;
        let (ws_id, session) = find_session(&store, r)?;
        let exec = held_execution(&session)?;
        Ok(AttachTarget {
            ready: AttachReady {
                session_id: session.id.clone(),
                execution_id: exec.id.clone(),
            },
            workspace_id: ws_id,
            backend_ref: exec.backend_ref.clone(),
        })
    }

    /// The environment for new processes in `ws`.
    pub(crate) async fn env_for_launch(&self, ws: &Workspace) -> RpcResult<EnvMap> {
        if let Some(env) = self.workspace_env.lock().unwrap().get(&ws.id) {
            return Ok(env.clone());
        }
        if ws.environment.kind == EnvironmentKind::None {
            return Ok(self.environments.base().clone());
        }
        // Not resolved since the daemon started (normally pre-warmed at
        // startup). Re-resolve without re-authorizing.
        let env = self
            .environments
            .resolve(ws.environment.kind, &PathBuf::from(&ws.root), false)
            .await
            .map_err(|e| RpcError::internal(format!("resolving environment: {e:#}")))?;
        self.workspace_env
            .lock()
            .unwrap()
            .insert(ws.id.clone(), env.clone());
        Ok(env)
    }

    /// Start a new execution for a session.
    pub(crate) async fn launch(
        &self,
        store: &mut Store,
        ws_id: &WorkspaceId,
        session_id: &SessionId,
        env: &EnvMap,
    ) -> RpcResult<()> {
        let ws = store.workspace(ws_id.as_str()).expect("exists");
        let session = ws.session(session_id.as_str()).expect("exists");
        let exec_id = ExecutionId::generate();
        let argv = self.argv(session, env);

        let mut env = env.clone();
        env.insert("WORKD_WORKSPACE_ID".into(), ws.id.to_string());
        env.insert("WORKD_WORKSPACE_NAME".into(), ws.name.clone());
        env.insert("WORKD_SESSION_ID".into(), session.id.to_string());
        env.insert("WORKD_SESSION_NAME".into(), session.name.clone());
        env.insert("WORKD_EXECUTION_ID".into(), exec_id.to_string());
        let result = match argv {
            Ok(argv) => {
                let spec = LaunchSpec {
                    backend_ref: exec_id.to_string(),
                    argv,
                    cwd: PathBuf::from(&ws.root),
                    env,
                };
                self.backend.launch(&spec).await
            }
            Err(e) => Err(e),
        };
        let session = store
            .workspace_mut(ws_id.as_str())
            .and_then(|w| w.session_mut(session_id.as_str()))
            .expect("exists");
        match result {
            Ok(launched) => {
                session.launch_error = None;
                if let Some(agent) = &mut session.agent {
                    agent.state = AgentState::Starting;
                    agent.state_since = Utc::now();
                }
                session.executions.push(Execution {
                    id: exec_id.clone(),
                    backend: self.backend.name().to_owned(),
                    backend_ref: exec_id.to_string(),
                    pid: launched.pid,
                    state: ExecutionState::Running,
                    exit_code: None,
                    started_at: Utc::now(),
                    ended_at: None,
                });
                trim_history(session);
                save(store)?;
                self.events.emit(Event::ExecutionStarted {
                    workspace_id: ws_id.clone(),
                    session_id: session_id.clone(),
                    execution_id: exec_id,
                });
                Ok(())
            }
            Err(e) => {
                let message = format!("{e:#}");
                session.launch_error = Some(message.clone());
                let summary = format!("{} failed to start", session.name);
                let ws = store.workspace_mut(ws_id.as_str()).expect("exists");
                let events =
                    attention::raise(ws, Some(session_id), AttentionKind::Failure, summary);
                save(store)?;
                self.events.emit(Event::ExecutionFailed {
                    workspace_id: ws_id.clone(),
                    session_id: session_id.clone(),
                    message: message.clone(),
                });
                self.emit_all(events);
                Err(RpcError::internal(format!(
                    "failed to start session: {message}"
                )))
            }
        }
    }

    /// Start every session of a ready workspace that has never run or failed
    /// to start.
    pub(crate) async fn launch_pending(&self, store: &mut Store, ws_id: &WorkspaceId) {
        let Some(ws) = store.workspace(ws_id.as_str()).cloned() else {
            return;
        };
        let env = match self.env_for_launch(&ws).await {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!("cannot start sessions: {e}");
                return;
            }
        };
        for s in &ws.sessions {
            if (s.executions.is_empty() || s.launch_error.is_some())
                && let Err(e) = self.launch(store, ws_id, &s.id, &env).await
            {
                tracing::warn!(session = %s.name, "session failed to start: {e}");
            }
        }
    }

    /// How to run a session's process.
    fn argv(&self, session: &Session, env: &EnvMap) -> anyhow::Result<Vec<String>> {
        if let Some(info) = &session.agent {
            let provider = agents::provider(&info.provider)
                .ok_or_else(|| anyhow::anyhow!("unknown agent provider `{}`", info.provider))?;
            // Run the agent binary directly so the session's process *is* the
            // agent (its open files and exit status are the agent's).
            return provider.argv(info, env);
        }
        Ok(match &session.command {
            // An interactive login shell, like `ssh host`.
            None => vec![self.shell.clone(), "-l".into()],
            // Commands run in the resolved environment exactly; a POSIX shell
            // doesn't re-run the user's startup files, which could reorder
            // PATH and undo e.g. a direnv/Nix environment.
            Some(cmd) => vec!["/bin/sh".into(), "-c".into(), cmd.clone()],
        })
    }

    /// Release every backend resource held for a session.
    pub(crate) async fn terminate_all(&self, session: &Session) -> RpcResult<()> {
        for exec in &session.executions {
            self.backend
                .terminate(&exec.backend_ref)
                .await
                .map_err(internal)?;
        }
        Ok(())
    }
}

impl Daemon {
    /// Add a session to a workspace without starting it.
    pub(crate) fn add_session_record(
        &self,
        store: &mut Store,
        ws_id: &WorkspaceId,
        spec: SessionSpec,
    ) -> RpcResult<SessionId> {
        let ws = store
            .workspace_mut(ws_id.as_str())
            .ok_or_else(|| workspace_not_found(ws_id.as_str()))?;
        let name = match spec.name {
            Some(name) => {
                validate_name("session", &name).map_err(RpcError::invalid)?;
                if ws.sessions.iter().any(|s| s.name == name) {
                    return Err(RpcError::new(
                        ErrorCode::AlreadyExists,
                        format!(
                            "workspace `{}` already has a session named `{name}`",
                            ws.name
                        ),
                    ));
                }
                name
            }
            None => {
                let base = match spec.kind {
                    SessionKind::Agent => spec.provider.clone().unwrap_or_else(|| "codex".into()),
                    _ => default_session_name(spec.command.as_deref()),
                };
                unique_session_name(ws, &base)
            }
        };
        let agent = (spec.kind == SessionKind::Agent).then(|| AgentInfo {
            provider: spec.provider.clone().unwrap_or_else(|| "codex".into()),
            resume_id: None,
            transcript: None,
            transcript_offset: 0,
            state: AgentState::Starting,
            state_since: Utc::now(),
            last_message: None,
            prompt: spec.prompt.clone(),
        });
        let session = Session {
            id: SessionId::generate(),
            workspace_id: ws_id.clone(),
            name,
            kind: spec.kind,
            command: spec.command,
            created_at: Utc::now(),
            launch_error: None,
            agent,
            executions: Vec::new(),
        };
        let session_id = session.id.clone();
        let (name, kind) = (session.name.clone(), session.kind);
        ws.sessions.push(session);
        ws.updated_at = Utc::now();
        save(store)?;
        self.events.emit(Event::SessionCreated {
            workspace_id: ws_id.clone(),
            session_id: session_id.clone(),
            name,
            kind,
        });
        Ok(session_id)
    }
}

pub(crate) fn validate_spec(spec: &SessionSpec) -> RpcResult<()> {
    match spec.kind {
        SessionKind::Agent => {
            let provider = spec.provider.as_deref().unwrap_or("codex");
            if agents::provider(provider).is_none() {
                return Err(RpcError::unsupported(format!(
                    "unknown agent `{provider}` (supported: {})",
                    agents::PROVIDERS.join(", ")
                )));
            }
            if spec.command.is_some() {
                return Err(RpcError::invalid(
                    "agent sessions take a prompt, not a command",
                ));
            }
        }
        _ if spec.provider.is_some() || spec.prompt.is_some() => {
            return Err(RpcError::invalid(
                "provider and prompt only apply to agent sessions",
            ));
        }
        SessionKind::Service | SessionKind::Task if spec.command.is_none() => {
            return Err(RpcError::invalid(format!(
                "a {} session needs a command",
                spec.kind.as_str()
            )));
        }
        _ => {}
    }
    if spec.command.as_deref().is_some_and(|c| c.trim().is_empty()) {
        return Err(RpcError::invalid("command must not be empty"));
    }
    Ok(())
}

pub(crate) fn find_session(store: &Store, r: &SessionRef) -> RpcResult<(WorkspaceId, Session)> {
    let ws = store
        .workspace(&r.workspace)
        .ok_or_else(|| workspace_not_found(&r.workspace))?;
    let session = ws.session(&r.session).cloned().ok_or_else(|| {
        RpcError::not_found(format!(
            "workspace `{}` has no session `{}`",
            ws.name, r.session
        ))
    })?;
    Ok((ws.id.clone(), session))
}

pub(crate) fn session_snapshot(
    store: &Store,
    ws_id: &WorkspaceId,
    session_id: &SessionId,
) -> Session {
    store
        .workspace(ws_id.as_str())
        .and_then(|w| w.session(session_id.as_str()))
        .cloned()
        .expect("session exists")
}

/// The current execution, if the backend still holds it (running, or exited
/// with its output retained).
fn held_execution(session: &Session) -> RpcResult<Execution> {
    session
        .current_execution()
        .filter(|e| matches!(e.state, ExecutionState::Running | ExecutionState::Exited))
        .cloned()
        .ok_or_else(|| not_running(session))
}

fn mark_ended(
    store: &mut Store,
    ws_id: &WorkspaceId,
    session_id: &SessionId,
    exec_id: &ExecutionId,
    state: ExecutionState,
) {
    let ws = store.workspace_mut(ws_id.as_str()).expect("exists");
    ws.updated_at = Utc::now();
    let session = ws.session_mut(session_id.as_str()).expect("exists");
    if let Some(exec) = session.executions.iter_mut().find(|e| &e.id == exec_id) {
        exec.state = state;
        exec.ended_at = Some(Utc::now());
    }
}

/// Keep execution history bounded.
fn trim_history(session: &mut Session) {
    const MAX: usize = 20;
    if session.executions.len() > MAX {
        let excess = session.executions.len() - MAX;
        session.executions.drain(..excess);
    }
}

/// `shell` for a login shell; otherwise the program name of the command
/// (`npm run dev` → `npm`, `FOO=1 ./bin/server` → `server`).
fn default_session_name(command: Option<&str>) -> String {
    command
        .and_then(|c| c.split_whitespace().find(|w| !w.contains('=')))
        .and_then(|w| w.rsplit('/').next())
        .filter(|w| !w.is_empty() && validate_name("session", w).is_ok())
        .unwrap_or("shell")
        .to_owned()
}

fn unique_session_name(ws: &Workspace, base: &str) -> String {
    let taken = |n: &str| ws.sessions.iter().any(|s| s.name == n);
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|n| !taken(n))
        .expect("unbounded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_names() {
        assert_eq!(default_session_name(None), "shell");
        assert_eq!(default_session_name(Some("npm run dev")), "npm");
        assert_eq!(
            default_session_name(Some("FOO=1 ./bin/server --x")),
            "server"
        );
        assert_eq!(default_session_name(Some("codex")), "codex");
    }
}
