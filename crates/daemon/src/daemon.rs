//! The host daemon: shared state and request dispatch.
//!
//! The managers are split across modules as `impl Daemon` blocks:
//! [`crate::workspaces`] (WorkspaceManager), [`crate::sessions`]
//! (SessionManager/ExecutionManager) and [`crate::reconcile`].
//!
//! All durable state lives in one [`Store`] behind an async mutex. Operations
//! that touch the execution backend hold the lock while they do so; backend
//! calls are short (spawning a tmux command), and holding the lock keeps the
//! store and the backend consistent with each other — in particular, the
//! reconciler never observes an execution that exists in one but not yet the
//! other. Slow work (git, direnv) never runs under the lock.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use tokio::sync::{Mutex, watch};
use workd_core::{Capability, HostStatus, Session, Timestamp, WorkspaceId};
use workd_protocol::{PROTOCOL_VERSION, Request, RpcError, StateSnapshot};

use crate::backend::ExecutionBackend;
use crate::env::{EnvMap, ResolvedEnv, which};
use crate::environment::EnvironmentManager;
use crate::events::EventLog;
use crate::git::GitManager;
use crate::paths::Paths;
use crate::store::Store;

pub type RpcResult<T> = Result<T, RpcError>;

pub struct Daemon {
    pub paths: Paths,
    pub store: Mutex<Store>,
    pub backend: Arc<dyn ExecutionBackend>,
    pub events: EventLog,
    pub git: GitManager,
    pub environments: EnvironmentManager,
    pub env_source: String,
    pub shell: String,
    pub started_at: Timestamp,
    /// Resolved environment per workspace (login env + e.g. direnv). Kept in
    /// memory only: it may contain secrets.
    pub(crate) workspace_env: std::sync::Mutex<HashMap<WorkspaceId, EnvMap>>,
    /// Running preparation tasks, so deleting a workspace can cancel them.
    pub(crate) preparing: std::sync::Mutex<HashMap<WorkspaceId, tokio::task::AbortHandle>>,
    shutdown: watch::Sender<bool>,
}

impl Daemon {
    pub fn new(
        paths: Paths,
        store: Store,
        backend: Arc<dyn ExecutionBackend>,
        events: EventLog,
        env: ResolvedEnv,
        shell: String,
    ) -> Self {
        Daemon {
            git: GitManager::new(paths.home.join("repos"), &env.vars),
            environments: EnvironmentManager::new(&env.vars),
            env_source: env.source,
            paths,
            store: Mutex::new(store),
            backend,
            events,
            shell,
            started_at: Utc::now(),
            workspace_env: Default::default(),
            preparing: Default::default(),
            shutdown: watch::channel(false).0,
        }
    }

    pub fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// Handle a plain request/response method. Streaming methods
    /// (`session.attach`, `events.subscribe`) and `daemon.shutdown` are handled
    /// by the connection loop.
    pub async fn handle(self: &Arc<Self>, req: Request) -> RpcResult<serde_json::Value> {
        match req {
            Request::Ping => json(()),
            Request::HostStatus => json(self.host_status().await),
            Request::WorkspaceCreate(p) => json(self.workspace_create(p).await?),
            Request::WorkspaceList => json(self.store.lock().await.state.workspaces.clone()),
            Request::WorkspaceGet(r) => json(self.workspace_get(&r.workspace).await?),
            Request::WorkspacePrepare(r) => json(self.workspace_prepare(&r.workspace).await?),
            Request::WorkspaceDelete(p) => json(self.workspace_delete(&p).await?),
            Request::SessionCreate(p) => json(self.session_create(p).await?),
            Request::SessionStop(r) => json(self.session_stop(&r).await?),
            Request::SessionRestart(r) => json(self.session_restart(&r).await?),
            Request::SessionDelete(r) => json(self.session_delete(&r).await?),
            Request::SessionRead(p) => json(self.session_read(&p).await?),
            Request::SessionWrite(p) => json(self.session_write(&p).await?),
            Request::AttentionResolve(p) => json(self.attention_resolve(&p).await?),
            Request::EventsList(p) => {
                let limit = p.limit.unwrap_or(100) as usize;
                json(self.events.recent(limit).map_err(internal)?)
            }
            Request::StateSnapshot => {
                // Read both under the store lock. Every change to the store
                // happens under it and its event is emitted afterwards, so the
                // snapshot reflects every event up to `seq` (and maybe later
                // ones). Keep emits after the change they describe.
                let store = self.store.lock().await;
                json(StateSnapshot {
                    seq: self.events.head(),
                    workspaces: store.state.workspaces.clone(),
                })
            }
            Request::Shutdown | Request::SessionAttach(_) | Request::EventsSubscribe(_) => {
                Err(RpcError::invalid(format!(
                    "{} must be handled by the connection",
                    req.method()
                )))
            }
        }
    }

    pub async fn host_status(&self) -> HostStatus {
        let env = self.environments.base();
        let (git, tmux, nix, direnv, agents) = tokio::join!(
            detect("git", &["--version"], env),
            detect("tmux", &["-V"], env),
            detect("nix", &["--version"], env),
            detect("direnv", &["version"], env),
            crate::agents::detect_all(env),
        );
        HostStatus {
            hostname: hostname(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            workd_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            started_at: self.started_at,
            workd_home: self.paths.home.to_string_lossy().into_owned(),
            shell: self.shell.clone(),
            environment_source: self.env_source.clone(),
            capabilities: vec![git, tmux, nix, direnv],
            agents,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by the manager modules
// ---------------------------------------------------------------------------

pub(crate) fn json<T: Serialize>(v: T) -> RpcResult<serde_json::Value> {
    serde_json::to_value(v).map_err(|e| RpcError::internal(e.to_string()))
}

pub(crate) fn internal(e: anyhow::Error) -> RpcError {
    RpcError::internal(format!("{e:#}"))
}

pub(crate) fn save(store: &Store) -> RpcResult<()> {
    store
        .save()
        .map_err(|e| RpcError::internal(format!("saving state: {e:#}")))
}

pub(crate) fn workspace_not_found(r: &str) -> RpcError {
    RpcError::not_found(format!("no workspace `{r}`"))
}

pub(crate) fn not_running(session: &Session) -> RpcError {
    RpcError::conflict(format!(
        "session `{}` is {}; restart it first",
        session.name,
        session.status().as_str()
    ))
}

async fn detect(name: &str, version_args: &[&str], env: &EnvMap) -> Capability {
    let Some(path) = which(name, env) else {
        return Capability {
            name: name.to_owned(),
            available: false,
            version: None,
            path: None,
        };
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(&path)
            .args(version_args)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let version = match output {
        Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .map(|l| l.trim().to_owned())
            .filter(|l| !l.is_empty()),
        _ => None,
    };
    Capability {
        name: name.to_owned(),
        available: true,
        version,
        path: Some(path.to_string_lossy().into_owned()),
    }
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for writes of buf.len() bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown".to_owned();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
