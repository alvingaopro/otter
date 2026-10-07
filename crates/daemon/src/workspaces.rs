//! WorkspaceManager: workspace lifecycle (design §28).
//!
//! ```text
//! create ──► PREPARING ──► READY (sessions start)
//!              │  files: empty dir | git worktree | existing directory
//!              │  environment: none | direnv
//!              └──► FAILED (retry with workspace.prepare)
//! ```
//!
//! Preparation runs in a background task so slow steps (clone, Nix builds) never
//! block the daemon. `workspace.create` waits briefly so fast cases (an empty
//! workspace) come back already ready.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use workd_core::{
    AttentionKind, Environment, EnvironmentKind, EnvironmentStatus, GitSource, SessionSpec,
    SourceSpec, Workspace, WorkspaceId, WorkspaceSource, WorkspaceState, validate_name,
};
use workd_protocol::{ErrorCode, Event, RpcError, WorkspaceCreate, WorkspaceDelete};

use crate::attention;
use crate::daemon::{Daemon, RpcResult, internal, save, workspace_not_found};
use crate::environment::EnvironmentManager;
use crate::git::{repo_id, slug};
use crate::sessions::validate_spec;

/// How long `workspace.create` waits for preparation before returning a
/// still-preparing workspace.
const CREATE_WAIT: Duration = Duration::from_secs(3);

impl Daemon {
    pub(crate) async fn workspace_create(
        self: &Arc<Self>,
        p: WorkspaceCreate,
    ) -> RpcResult<Workspace> {
        validate_name("workspace", &p.name).map_err(RpcError::invalid)?;
        let specs = p.sessions.unwrap_or_else(|| vec![SessionSpec::shell()]);
        for spec in &specs {
            validate_spec(spec)?;
        }

        let id = WorkspaceId::generate();
        let managed_dir = self.paths.workspace_dir(id.as_str());
        let (source, root) = match p.source {
            SourceSpec::Empty => (WorkspaceSource::Empty, managed_dir),
            SourceSpec::Git {
                repository,
                branch,
                base,
            } => {
                if repository.trim().is_empty() {
                    return Err(RpcError::invalid("repository must not be empty"));
                }
                let source = WorkspaceSource::Git(GitSource {
                    repo_id: repo_id(&repository),
                    repository,
                    branch: String::new(),
                    base,
                    base_commit: None,
                    requested_branch: branch,
                });
                (source, managed_dir.join("repo"))
            }
            SourceSpec::Directory { path } => {
                let path = PathBuf::from(&path);
                if !path.is_absolute() {
                    return Err(RpcError::invalid(
                        "directory must be an absolute path on the host",
                    ));
                }
                if !path.is_dir() {
                    return Err(RpcError::invalid(format!(
                        "{} is not a directory on this host",
                        path.display()
                    )));
                }
                let path_s = path.to_string_lossy().into_owned();
                (WorkspaceSource::Directory { path: path_s }, path)
            }
        };

        {
            let mut store = self.store.lock().await;
            if store.state.workspaces.iter().any(|w| w.name == p.name) {
                return Err(RpcError::new(
                    ErrorCode::AlreadyExists,
                    format!("a workspace named `{}` already exists", p.name),
                ));
            }
            let now = Utc::now();
            store.state.workspaces.push(Workspace {
                id: id.clone(),
                name: p.name.clone(),
                root: root.to_string_lossy().into_owned(),
                brief: p.brief.unwrap_or_default(),
                source,
                environment: Environment::default(),
                state: WorkspaceState::Preparing,
                state_message: None,
                created_at: now,
                updated_at: now,
                sessions: Vec::new(),
                attention: Vec::new(),
            });
            save(&store)?;
            self.events.emit(Event::WorkspaceCreated {
                workspace_id: id.clone(),
                name: p.name,
            });
            for spec in specs {
                self.add_session_record(&mut store, &id, spec)?;
            }
        }

        let task = self.spawn_preparation(id.clone());
        let _ = tokio::time::timeout(CREATE_WAIT, task).await;
        self.workspace_get(id.as_str()).await
    }

    pub(crate) async fn workspace_get(&self, r: &str) -> RpcResult<Workspace> {
        let store = self.store.lock().await;
        store
            .workspace(r)
            .cloned()
            .ok_or_else(|| workspace_not_found(r))
    }

    /// Retry preparation of a failed workspace, or re-resolve the environment
    /// of a ready one (new executions pick it up) and start sessions that
    /// haven't started.
    pub(crate) async fn workspace_prepare(self: &Arc<Self>, r: &str) -> RpcResult<Workspace> {
        let id = {
            let mut store = self.store.lock().await;
            let ws = store
                .workspace_mut(r)
                .ok_or_else(|| workspace_not_found(r))?;
            if ws.state == WorkspaceState::Preparing {
                return Err(RpcError::conflict(format!(
                    "workspace `{}` is already preparing",
                    ws.name
                )));
            }
            ws.state = WorkspaceState::Preparing;
            ws.state_message = None;
            let id = ws.id.clone();
            save(&store)?;
            id
        };
        self.workspace_env.lock().unwrap().remove(&id);
        let task = self.spawn_preparation(id.clone());
        let _ = tokio::time::timeout(CREATE_WAIT, task).await;
        self.workspace_get(id.as_str()).await
    }

    pub(crate) async fn workspace_delete(&self, p: &WorkspaceDelete) -> RpcResult<()> {
        let ws = self.workspace_get(&p.workspace).await?;

        if let Some(task) = self.preparing.lock().unwrap().remove(&ws.id) {
            task.abort();
        }

        if let WorkspaceSource::Git(_) = &ws.source
            && !p.force
            && self
                .git
                .is_dirty(Path::new(&ws.root))
                .await
                .unwrap_or(false)
        {
            return Err(RpcError::conflict(format!(
                "workspace `{}` has uncommitted changes in {}; delete with force to discard them",
                ws.name, ws.root
            )));
        }

        let mut store = self.store.lock().await;
        let Some(idx) = store.workspace_index(ws.id.as_str()) else {
            return Err(workspace_not_found(&p.workspace));
        };
        let ws = store.state.workspaces[idx].clone();

        // Managed processes first, then managed files, then the record — so a
        // failure leaves something the user can retry.
        for session in &ws.sessions {
            self.terminate_all(session).await?;
        }
        let managed_dir = self.paths.workspace_dir(ws.id.as_str());
        match &ws.source {
            WorkspaceSource::Git(git) => {
                if self.git.base_dir(&git.repo_id).exists() {
                    self.git
                        .remove_worktree(&git.repo_id, Path::new(&ws.root))
                        .await
                        .map_err(|e| RpcError::internal(format!("removing worktree: {e:#}")))?;
                }
                remove_managed_dir(&managed_dir)?;
            }
            WorkspaceSource::Empty => remove_managed_dir(&managed_dir)?,
            // Attached, not managed: never deleted (design §29).
            WorkspaceSource::Directory { .. } => {}
        }

        store.state.workspaces.remove(idx);
        save(&store)?;
        self.workspace_env.lock().unwrap().remove(&ws.id);
        self.events.emit(Event::WorkspaceDeleted {
            workspace_id: ws.id,
            name: ws.name,
        });
        Ok(())
    }

    /// Resume work interrupted by a daemon restart: re-run preparation of
    /// workspaces that were preparing, and re-resolve environments of ready
    /// ones so new sessions don't wait for it.
    pub async fn resume_workspaces(self: &Arc<Self>) {
        let workspaces = self.store.lock().await.state.workspaces.clone();
        for ws in workspaces {
            match ws.state {
                WorkspaceState::Preparing => {
                    self.spawn_preparation(ws.id.clone());
                }
                WorkspaceState::Ready if ws.environment.kind != EnvironmentKind::None => {
                    let daemon = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = daemon.env_for_launch(&ws).await {
                            tracing::warn!(workspace = %ws.name, "environment: {e}");
                        }
                    });
                }
                _ => {}
            }
        }
    }

    fn spawn_preparation(self: &Arc<Self>, id: WorkspaceId) -> tokio::task::JoinHandle<()> {
        let daemon = self.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move { daemon.prepare(task_id).await });
        self.preparing
            .lock()
            .unwrap()
            .insert(id, handle.abort_handle());
        handle
    }

    async fn prepare(self: Arc<Self>, id: WorkspaceId) {
        let result = self.prepare_steps(&id).await;
        self.preparing.lock().unwrap().remove(&id);

        let mut store = self.store.lock().await;
        let Some(ws) = store.workspace_mut(id.as_str()) else {
            return; // deleted meanwhile
        };
        ws.updated_at = Utc::now();
        match result {
            Ok(()) => {
                ws.state = WorkspaceState::Ready;
                ws.state_message = None;
                let events = attention::resolve(ws, |a| a.session_id.is_none());
                self.emit_all(events);
                if let Err(e) = save(&store) {
                    tracing::error!("{e}");
                }
                self.events.emit(Event::WorkspaceReady {
                    workspace_id: id.clone(),
                });
                self.launch_pending(&mut store, &id).await;
            }
            Err(e) => {
                let message = format!("{e:#}");
                tracing::warn!(workspace = %ws.name, "preparation failed: {message}");
                ws.state = WorkspaceState::Failed;
                ws.state_message = Some(message.clone());
                let events = attention::raise(
                    ws,
                    None,
                    AttentionKind::Failure,
                    "workspace preparation failed".to_owned(),
                );
                if let Err(e) = save(&store) {
                    tracing::error!("{e}");
                }
                self.events.emit(Event::WorkspaceFailed {
                    workspace_id: id,
                    message,
                });
                self.emit_all(events);
            }
        }
    }

    async fn prepare_steps(&self, id: &WorkspaceId) -> Result<()> {
        let ws = self
            .workspace_get(id.as_str())
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let root = PathBuf::from(&ws.root);
        let mut authorize_env = false;

        match &ws.source {
            WorkspaceSource::Empty => {
                std::fs::create_dir_all(&root)
                    .with_context(|| format!("creating {}", root.display()))?;
            }
            WorkspaceSource::Directory { path } => {
                if !Path::new(path).is_dir() {
                    bail!("{path} no longer exists");
                }
            }
            WorkspaceSource::Git(git) => {
                // Workd created this worktree from a repository the user
                // asked for, so it may authorize its .envrc.
                authorize_env = true;
                if !root.join(".git").exists() {
                    let progress = |msg: &str| self.set_progress(id, msg);
                    self.git
                        .sync_repo(&git.repository, &git.repo_id, &progress)
                        .await?;
                    progress("creating worktree");
                    std::fs::create_dir_all(root.parent().unwrap())?;
                    let default_branch = match slug(&ws.name) {
                        s if s.is_empty() => format!("workd/{}", ws.id),
                        s => format!("workd/{s}"),
                    };
                    let checkout = self
                        .git
                        .add_worktree(
                            &git.repo_id,
                            &root,
                            git.requested_branch.as_deref(),
                            git.base.as_deref(),
                            &default_branch,
                        )
                        .await?;
                    let mut store = self.store.lock().await;
                    if let Some(ws) = store.workspace_mut(id.as_str())
                        && let WorkspaceSource::Git(g) = &mut ws.source
                    {
                        g.branch = checkout.branch;
                        g.base = Some(checkout.base);
                        g.base_commit = Some(checkout.base_commit);
                    }
                    store.save()?;
                }
            }
        }

        self.prepare_environment(id, &root, authorize_env).await
    }

    async fn prepare_environment(
        &self,
        id: &WorkspaceId,
        root: &Path,
        authorize: bool,
    ) -> Result<()> {
        let kind = EnvironmentManager::detect(root);
        self.set_environment(id, kind, EnvironmentStatus::Preparing, None)
            .await;
        if kind != EnvironmentKind::None {
            self.set_progress(id, &format!("preparing environment ({})", kind.as_str()));
            self.events.emit(Event::EnvironmentPreparing {
                workspace_id: id.clone(),
            });
        }
        match self.environments.resolve(kind, root, authorize).await {
            Ok(env) => {
                self.workspace_env.lock().unwrap().insert(id.clone(), env);
                self.set_environment(id, kind, EnvironmentStatus::Ready, None)
                    .await;
                if kind != EnvironmentKind::None {
                    self.events.emit(Event::EnvironmentReady {
                        workspace_id: id.clone(),
                    });
                }
                Ok(())
            }
            Err(e) => {
                let message = format!("{e:#}");
                self.set_environment(id, kind, EnvironmentStatus::Failed, Some(message.clone()))
                    .await;
                self.events.emit(Event::EnvironmentFailed {
                    workspace_id: id.clone(),
                    message: message.clone(),
                });
                bail!("environment: {message}")
            }
        }
    }

    async fn set_environment(
        &self,
        id: &WorkspaceId,
        kind: EnvironmentKind,
        status: EnvironmentStatus,
        message: Option<String>,
    ) {
        let mut store = self.store.lock().await;
        if let Some(ws) = store.workspace_mut(id.as_str()) {
            ws.environment = Environment {
                kind,
                status,
                message,
            };
            if let Err(e) = store.save() {
                tracing::error!("saving state: {e:#}");
            }
        }
    }

    /// Record a human-readable progress message on a preparing workspace.
    fn set_progress(&self, id: &WorkspaceId, message: &str) {
        // Best effort; never block preparation on the store lock.
        if let Ok(mut store) = self.store.try_lock()
            && let Some(ws) = store.workspace_mut(id.as_str())
        {
            ws.state_message = Some(message.to_owned());
            let _ = store.save();
        }
    }
}

fn remove_managed_dir(dir: &Path) -> RpcResult<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(internal(anyhow::anyhow!("removing {}: {e}", dir.display()))),
    }
}
