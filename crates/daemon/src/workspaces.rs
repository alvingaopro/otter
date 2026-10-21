//! WorkspaceManager: workspace lifecycle (design §28).
//!
//! ```text
//! create ──► PREPARING ──► READY (sessions start)
//!              │  files: empty dir | git worktree | existing directory
//!              │  environment: none | direnv
//!              └──► FAILED (retry with workspace.prepare)
//!
//! READY | FAILED ──archive──► ARCHIVED ──unarchive──► PREPARING
//! ```
//!
//! Archiving is not deleting (D-036): sessions are stopped but kept, attention
//! is resolved, and the files, worktree and branch stay where they are.
//!
//! Preparation runs in a background task so slow steps (clone, Nix builds) never
//! block the daemon. `workspace.create` waits briefly when there is no Git work
//! to do, so fast cases (an empty workspace) come back already ready; a Git
//! workspace comes back at once and opens while it prepares (D-135).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use otter_core::{
    AttentionKind, Environment, EnvironmentKind, EnvironmentStatus, ExecutionState, GitSource,
    SessionSpec, SourceSpec, Workspace, WorkspaceId, WorkspaceSource, WorkspaceState,
    validate_name,
};
use otter_protocol::{
    ErrorCode, Event, RpcError, WorkspaceCreate, WorkspaceDelete, WorkspaceRename,
    WorkspaceSetBrief,
};
use tokio::sync::oneshot;

use crate::attention;
use crate::daemon::{Daemon, RpcResult, internal, save, workspace_not_found};
use crate::environment::EnvironmentManager;
use crate::git::{Found, repo_id, slug};
use crate::sessions::{mark_ended, validate_spec};

/// How long `workspace.create` waits for a preparation with no Git work in
/// it before returning a still-preparing workspace.
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
        let quick = !matches!(p.source, SourceSpec::Git { .. });
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
                brief: p.brief.unwrap_or_default().normalized(),
                source,
                environment: Environment::default(),
                state: WorkspaceState::Preparing,
                state_message: None,
                created_at: now,
                updated_at: now,
                sessions: Vec::new(),
                attention: Vec::new(),
                task_id: None,
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

        let prep = self.spawn_preparation(id.clone());
        wait_if_quick(quick, prep).await;
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
        let (id, quick) = {
            let mut store = self.store.lock().await;
            let ws = store
                .workspace_mut(r)
                .ok_or_else(|| workspace_not_found(r))?;
            self.not_being_deleted(ws)?;
            match ws.state {
                WorkspaceState::Preparing => {
                    return Err(RpcError::conflict(format!(
                        "workspace `{}` is already preparing",
                        ws.name
                    )));
                }
                WorkspaceState::Archived => return Err(archived(ws)),
                WorkspaceState::Ready | WorkspaceState::Failed => {}
            }
            ws.state = WorkspaceState::Preparing;
            ws.state_message = None;
            let (id, quick) = (ws.id.clone(), !needs_worktree(ws));
            save(&store)?;
            (id, quick)
        };
        self.workspace_env.lock().unwrap().remove(&id);
        self.events.emit(Event::WorkspacePreparing {
            workspace_id: id.clone(),
        });
        let prep = self.spawn_preparation(id.clone());
        wait_if_quick(quick, prep).await;
        self.workspace_get(id.as_str()).await
    }

    /// Put a workspace away (D-036): stop its sessions (they stay, and can be
    /// restarted after unarchiving), resolve its attention, keep its files.
    pub(crate) async fn workspace_archive(&self, r: &str) -> RpcResult<Workspace> {
        let mut store = self.store.lock().await;
        let ws = store
            .workspace(r)
            .cloned()
            .ok_or_else(|| workspace_not_found(r))?;
        self.not_being_deleted(&ws)?;
        match ws.state {
            WorkspaceState::Ready | WorkspaceState::Failed => {}
            // Preparation would mark it ready and start its sessions when it
            // finishes; let it finish (or fail) first.
            WorkspaceState::Preparing => {
                return Err(RpcError::conflict(format!(
                    "workspace `{}` is preparing; archive it once it is ready, or delete it",
                    ws.name
                )));
            }
            WorkspaceState::Archived => {
                return Err(RpcError::conflict(format!(
                    "workspace `{}` is already archived",
                    ws.name
                )));
            }
        }

        // Every session is tried: those stopped are saved and announced even
        // when another can't be, and then it stays as it was.
        let mut events = Vec::new();
        let mut failed = None;
        for session in &ws.sessions {
            if let Some(exec) = session.current_execution().filter(|e| e.is_running()) {
                if let Err(e) = self.backend.terminate(&exec.backend_ref).await {
                    failed.get_or_insert(internal(e));
                    continue;
                }
                mark_ended(
                    &mut store,
                    &ws.id,
                    &session.id,
                    &exec.id,
                    ExecutionState::Stopped,
                );
                events.push(Event::SessionStopped {
                    workspace_id: ws.id.clone(),
                    session_id: session.id.clone(),
                });
            }
            // Exited processes keep their retained output (D-027), as after
            // `session.stop`: their last screen stays readable.
        }
        if let Some(e) = failed {
            save(&store)?;
            self.emit_all(events);
            return Err(e);
        }

        let w = store.workspace_mut(ws.id.as_str()).expect("exists");
        events.extend(attention::resolve(w, |_| true));
        w.state = WorkspaceState::Archived;
        w.state_message = None;
        w.updated_at = Utc::now();
        save(&store)?;
        self.workspace_env.lock().unwrap().remove(&ws.id);
        self.emit_all(events);
        self.events.emit(Event::WorkspaceArchived {
            workspace_id: ws.id.clone(),
            name: ws.name.clone(),
        });
        Ok(store.workspace(ws.id.as_str()).cloned().expect("exists"))
    }

    /// Bring an archived workspace back: prepare it again (files are checked,
    /// the environment re-resolved). Its stopped sessions stay stopped until
    /// restarted; sessions that never started start when it is ready.
    pub(crate) async fn workspace_unarchive(self: &Arc<Self>, r: &str) -> RpcResult<Workspace> {
        let (id, quick) = {
            let mut store = self.store.lock().await;
            let ws = store
                .workspace_mut(r)
                .ok_or_else(|| workspace_not_found(r))?;
            self.not_being_deleted(ws)?;
            if ws.state != WorkspaceState::Archived {
                return Err(RpcError::conflict(format!(
                    "workspace `{}` is not archived",
                    ws.name
                )));
            }
            ws.state = WorkspaceState::Preparing;
            ws.state_message = None;
            ws.updated_at = Utc::now();
            let (id, quick) = (ws.id.clone(), !needs_worktree(ws));
            save(&store)?;
            self.events.emit(Event::WorkspaceUnarchived {
                workspace_id: id.clone(),
            });
            (id, quick)
        };
        self.workspace_env.lock().unwrap().remove(&id);
        let prep = self.spawn_preparation(id.clone());
        wait_if_quick(quick, prep).await;
        self.workspace_get(id.as_str()).await
    }

    /// Replace the brief (D-038). Allowed in any state: it describes the
    /// workspace, it doesn't run anything.
    pub(crate) async fn workspace_set_brief(&self, p: WorkspaceSetBrief) -> RpcResult<Workspace> {
        let mut store = self.store.lock().await;
        let ws = store
            .workspace_mut(&p.workspace)
            .ok_or_else(|| workspace_not_found(&p.workspace))?;
        let brief = p.brief.normalized();
        if ws.brief == brief {
            return Ok(ws.clone());
        }
        ws.brief = brief;
        ws.updated_at = Utc::now();
        let ws = ws.clone();
        save(&store)?;
        self.events.emit(Event::WorkspaceBriefChanged {
            workspace_id: ws.id.clone(),
        });
        Ok(ws)
    }

    /// Another name (D-101). Only what people read changes: the id, the
    /// directory, the branch and the processes stay as they are.
    pub(crate) async fn workspace_rename(&self, p: WorkspaceRename) -> RpcResult<Workspace> {
        let name = p.name.trim().to_owned();
        validate_name("workspace", &name).map_err(RpcError::invalid)?;
        let mut store = self.store.lock().await;
        let id = store
            .workspace(&p.workspace)
            .map(|w| w.id.clone())
            .ok_or_else(|| workspace_not_found(&p.workspace))?;
        if store
            .state
            .workspaces
            .iter()
            .any(|w| w.name == name && w.id != id)
        {
            return Err(RpcError::new(
                ErrorCode::AlreadyExists,
                format!("a workspace named `{name}` already exists"),
            ));
        }
        let ws = store.workspace_mut(id.as_str()).expect("exists");
        if ws.name == name {
            return Ok(ws.clone());
        }
        ws.name = name.clone();
        ws.updated_at = Utc::now();
        let ws = ws.clone();
        save(&store)?;
        self.events.emit(Event::WorkspaceRenamed {
            workspace_id: ws.id.clone(),
            name,
        });
        Ok(ws)
    }

    /// Managed processes first, then managed files, then the record — so a
    /// failure leaves something the user can retry. The store's lock is held
    /// to stop the processes and to drop the record, not while files go,
    /// which can take a while: the rest of the host goes on meanwhile
    /// (D-135).
    pub(crate) async fn workspace_delete(&self, p: &WorkspaceDelete) -> RpcResult<()> {
        let ws = self.workspace_get(&p.workspace).await?;
        self.not_being_deleted(&ws)?;

        if let WorkspaceSource::Git(g) = &ws.source
            && !p.force
        {
            let root = Path::new(&ws.root);
            // What git has there decides, not the record alone: one ready
            // without its branch, from before D-135, may have been worked in.
            let found = self.git.found(&g.repo_id, root);
            let nobodys = nobodys_work(&ws, &found);
            if !nobodys
                && root.join(".git").exists()
                && self.git.is_dirty(root).await.unwrap_or(false)
            {
                return Err(RpcError::conflict(format!(
                    "workspace `{}` has uncommitted changes in {}; delete with force to discard them",
                    ws.name, ws.root
                )));
            }
            // Its branch goes with it (D-108): not commits that are nowhere else.
            let branch = match (g.branch.as_str(), found) {
                ("", Found::Worktree { branch, .. }) if !nobodys => branch,
                ("", _) => None,
                (b, _) => Some(b.to_owned()),
            };
            if let Some(branch) = branch {
                let n = self.git.unpushed(&g.repo_id, &branch).await.unwrap_or(0);
                if n > 0 {
                    return Err(RpcError::conflict(format!(
                        "workspace `{}`'s branch `{branch}` has {n} commit{} not pushed anywhere; delete with force to discard {}",
                        ws.name,
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "it" } else { "them" },
                    )));
                }
            }
        }

        let id = ws.id.clone();
        if !self.deleting.lock().unwrap().insert(id.clone()) {
            return Err(being_deleted(&ws));
        }
        let _deleting = Deleting {
            set: &self.deleting,
            id: id.clone(),
        };
        // Its preparation stops, all of its Git with it, and is gone before
        // anything is removed.
        let prep = self.preparing.lock().unwrap().remove(&id);
        let stopped = prep.is_some();
        if let Some(prep) = prep {
            prep.abort();
            let _ = prep.await;
        }
        let deleted = self.delete_stopped(p, &id).await;
        // Its preparation was stopped: say what happened, so it can be
        // deleted again or prepared again, not left preparing.
        if stopped && let Err(e) = &deleted {
            self.fail_preparing(&id, format!("deleting it failed: {}", e.message))
                .await;
        }
        deleted
    }

    /// Delete a workspace whose preparation isn't running (D-135).
    async fn delete_stopped(&self, p: &WorkspaceDelete, id: &WorkspaceId) -> RpcResult<()> {
        // Processes, under the lock as when archiving: marked stopped before
        // anything can find them gone.
        let ws = {
            let mut store = self.store.lock().await;
            let ws = store
                .workspace(id.as_str())
                .cloned()
                .ok_or_else(|| workspace_not_found(&p.workspace))?;
            let mut events = Vec::new();
            let mut failed = None;
            for session in &ws.sessions {
                if let Err(e) = self.terminate_all(session).await {
                    failed.get_or_insert(e);
                    continue;
                }
                if let Some(exec) = session.current_execution().filter(|e| e.is_running()) {
                    mark_ended(
                        &mut store,
                        &ws.id,
                        &session.id,
                        &exec.id,
                        ExecutionState::Stopped,
                    );
                    events.push(Event::SessionStopped {
                        workspace_id: ws.id.clone(),
                        session_id: session.id.clone(),
                    });
                }
            }
            save(&store)?;
            self.emit_all(events);
            // Those stopped are saved and announced even when another can't
            // be; then the delete stops.
            if let Some(e) = failed {
                return Err(e);
            }
            ws
        };

        // Files, without the store's lock.
        let managed_dir = self.paths.workspace_dir(id.as_str());
        let mut repo = None;
        match &ws.source {
            WorkspaceSource::Git(git) => {
                let mut removed = true;
                if self.git.base_dir(&git.repo_id).exists() {
                    let had = self
                        .git
                        .remove_worktree(&git.repo_id, Path::new(&ws.root))
                        .await
                        .map_err(|e| RpcError::internal(format!("removing worktree: {e:#}")))?;
                    let branch = match git.branch.as_str() {
                        "" => had,
                        b => Some(b.to_owned()),
                    };
                    match branch {
                        // Not recorded: unless forced, only if nothing is on
                        // it (D-135).
                        Some(b) if git.branch.is_empty() && !p.force => {
                            self.git.drop_empty_branch(&git.repo_id, &b).await;
                        }
                        Some(b) => removed = self.git.delete_branch(&git.repo_id, &b).await,
                        None => {}
                    }
                }
                remove_managed_dir(&managed_dir).await?;
                if removed {
                    repo = Some(git.repo_id.clone());
                }
            }
            WorkspaceSource::Empty => remove_managed_dir(&managed_dir).await?,
            // Attached, not managed: never deleted (design §29).
            WorkspaceSource::Directory { .. } => {}
        }

        // The record.
        {
            let mut store = self.store.lock().await;
            let Some(idx) = store.workspace_index(id.as_str()) else {
                return Err(workspace_not_found(&p.workspace));
            };
            // A session started meanwhile goes with it.
            for session in store.state.workspaces[idx].sessions.clone() {
                if session.current_execution().is_some_and(|e| e.is_running()) {
                    self.terminate_all(&session).await?;
                }
            }
            let ws = store.state.workspaces.remove(idx);
            save(&store)?;
            self.workspace_env.lock().unwrap().remove(&ws.id);
            self.events.emit(Event::WorkspaceDeleted {
                workspace_id: ws.id,
                name: ws.name,
            });
        }

        // The repository's copy goes when nothing uses it any more (D-108),
        // decided under its Git lock: a workspace being made on it is either
        // counted or fetches once the copy is gone (D-135).
        if let Some(repo_id) = repo
            && let Err(e) = self
                .git
                .remove_repo(&repo_id, self.repo_in_use(&repo_id))
                .await
        {
            tracing::warn!(repo = %repo_id, "removing the repository: {e:#}");
        }
        Ok(())
    }

    /// Whether a workspace is on the repository's copy `repo_id`.
    async fn repo_in_use(&self, repo_id: &str) -> bool {
        let store = self.store.lock().await;
        store
            .state
            .workspaces
            .iter()
            .any(|w| matches!(&w.source, WorkspaceSource::Git(o) if o.repo_id == repo_id))
    }

    /// A preparation stopped by a delete that then failed: the workspace
    /// is failed, saying why, rather than preparing with nothing preparing
    /// it (D-135).
    async fn fail_preparing(&self, id: &WorkspaceId, message: String) {
        let mut store = self.store.lock().await;
        let Some(ws) = store.workspace_mut(id.as_str()) else {
            return;
        };
        if ws.state != WorkspaceState::Preparing {
            return;
        }
        ws.state = WorkspaceState::Failed;
        ws.state_message = Some(message.clone());
        ws.updated_at = Utc::now();
        if let Err(e) = save(&store) {
            tracing::error!("{e}");
        }
        self.events.emit(Event::WorkspaceFailed {
            workspace_id: id.clone(),
            message,
        });
    }

    /// A workspace being deleted takes nothing else (D-135).
    pub(crate) fn not_being_deleted(&self, ws: &Workspace) -> RpcResult<()> {
        if self.deleting.lock().unwrap().contains(&ws.id) {
            return Err(being_deleted(ws));
        }
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

    /// Prepare `id` in the background, unless it is being deleted. What is
    /// returned ends when the preparation has, finished or cancelled.
    fn spawn_preparation(self: &Arc<Self>, id: WorkspaceId) -> Option<oneshot::Receiver<()>> {
        // Held while it starts, so a delete either stops it or comes first.
        let mut preparing = self.preparing.lock().unwrap();
        if self.deleting.lock().unwrap().contains(&id) {
            return None;
        }
        let (done, ended) = oneshot::channel();
        let daemon = self.clone();
        let prep_id = id.clone();
        let handle = tokio::spawn(async move {
            let _done: oneshot::Sender<()> = done;
            daemon.prepare(prep_id).await
        });
        preparing.insert(id, handle);
        Some(ended)
    }

    async fn prepare(self: Arc<Self>, id: WorkspaceId) {
        let result = self.prepare_steps(&id).await;
        self.preparing.lock().unwrap().remove(&id);

        let mut store = self.store.lock().await;
        let Some(ws) = store.workspace_mut(id.as_str()) else {
            return; // deleted meanwhile
        };
        if ws.state != WorkspaceState::Preparing {
            return; // e.g. archived meanwhile: don't start its sessions
        }
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
                // Being deleted: nothing starts there (D-135).
                if !self.deleting.lock().unwrap().contains(&id) {
                    self.launch_pending(&mut store, &id).await;
                }
            }
            Err(e) => {
                // Kept, logged and sent: never a credential in it (D-075).
                let message = otter_core::redact::redact(&format!("{e:#}"));
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
        // A task waiting for it goes on, or says why it can't, now rather
        // than at the next tick: `workspace.create` no longer waits for Git
        // (D-135).
        self.task_wake.notify_one();
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
                // Otter created this worktree from a repository the user
                // asked for, so it may authorize its .envrc.
                authorize_env = true;
                if needs_worktree(&ws) {
                    self.make_worktree(&ws, git, &root).await?;
                } else {
                    // Preparing a worktree already there again (a retry, an
                    // unarchive) brings the repository up to date, so its
                    // "behind" is as of now (D-152). It failing, say offline,
                    // doesn't fail the workspace.
                    self.set_progress(id, "fetching").await;
                    if let Err(e) = self.git.sync_repo(&git.repository, &git.repo_id).await {
                        tracing::warn!(
                            workspace = %ws.name,
                            "fetching: {}",
                            otter_core::redact::redact(&format!("{e:#}"))
                        );
                    }
                }
            }
        }

        self.prepare_environment(id, &root, authorize_env).await
    }

    /// Make the worktree of a Git workspace: again when a checkout was cut
    /// short — otterd stopped mid-way, as each update does (D-125) — or
    /// the worktree went missing. What git has there decides: a worktree
    /// that may hold someone's work is kept and its branch recorded; what a
    /// checkout cut short left goes first; one made before goes back on its
    /// branch, with its commits (D-135).
    async fn make_worktree(&self, ws: &Workspace, git: &GitSource, root: &Path) -> Result<()> {
        let id = &ws.id;
        let found = self.git.found(&git.repo_id, root);
        let nobodys = nobodys_work(ws, &found);
        match &found {
            // Kept as it is, with whatever is in it: one finished but never
            // recorded (otterd stopped first), or one called ready without
            // its branch before D-135 and worked in since.
            Found::Worktree { branch, cut_short } if !nobodys => {
                if *cut_short {
                    self.git.unlock(&git.repo_id, root).await?;
                }
                return self.adopt(ws, git, branch.clone()).await;
            }
            // Locked by someone else than git: theirs.
            Found::Nothing {
                locked: Some(reason),
            } if reason != "initializing" => bail!(
                "{} is gone, and its worktree is locked{}: unlock it (git worktree unlock) to make it again here, or delete the workspace",
                root.display(),
                match reason.as_str() {
                    "" => String::new(),
                    r => format!(" ({r})"),
                }
            ),
            Found::Other => bail!(
                "{} holds files but no worktree of {}: move them away, or delete the workspace",
                root.display(),
                git.repo_id
            ),
            Found::Worktree { .. } | Found::Nothing { .. } => {}
        }

        let fetch = match self.git.cloned(&git.repo_id) {
            true => "fetching",
            false => "cloning repository",
        };
        self.set_progress(id, fetch).await;
        self.git.sync_repo(&git.repository, &git.repo_id).await?;
        self.set_progress(id, "creating worktree").await;
        // Cut short with nothing run there, gone, or never there: what's
        // left goes (its registration, an empty directory). The branch a
        // checkout cut short made goes too if nothing is on it, so the new
        // one takes the same name, not `-2`.
        let had = self.git.remove_worktree(&git.repo_id, root).await?;
        if nobodys
            && git.branch.is_empty()
            && let Some(branch) = had
        {
            self.git.drop_empty_branch(&git.repo_id, &branch).await;
        }
        std::fs::create_dir_all(root.parent().unwrap())?;
        let default_branch = match slug(&ws.name) {
            s if s.is_empty() => format!("otterd/{}", ws.id),
            s => format!("otterd/{s}"),
        };
        let branch = match git.branch.as_str() {
            "" => git.requested_branch.as_deref(),
            b => Some(b),
        };
        let checkout = self
            .git
            .add_worktree(
                &git.repo_id,
                root,
                branch,
                git.base.as_deref(),
                &default_branch,
            )
            .await?;
        let mut store = self.store.lock().await;
        if let Some(ws) = store.workspace_mut(id.as_str())
            && let WorkspaceSource::Git(g) = &mut ws.source
        {
            // Made before: what it was made from stays as recorded.
            if g.branch.is_empty() {
                g.base = Some(checkout.base);
                g.base_commit = Some(checkout.base_commit);
            }
            g.branch = checkout.branch;
        }
        store.save()?;
        Ok(())
    }

    /// Record the branch of a worktree found at the root, and what it came
    /// from as `add_worktree` would have said (D-135).
    async fn adopt(&self, ws: &Workspace, git: &GitSource, branch: Option<String>) -> Result<()> {
        // Detached: no branch to record.
        let Some(branch) = branch else {
            return Ok(());
        };
        let (base, base_commit) = self
            .git
            .base_of(&git.repo_id, &branch, git.base.as_deref())
            .await;
        let mut store = self.store.lock().await;
        if let Some(w) = store.workspace_mut(ws.id.as_str())
            && let WorkspaceSource::Git(g) = &mut w.source
            && g.branch.is_empty()
        {
            tracing::info!(workspace = %ws.name, branch, "recording the branch of the worktree found there");
            g.branch = branch;
            g.base = base;
            g.base_commit = base_commit;
        }
        store.save()?;
        Ok(())
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
            self.set_progress(id, &format!("preparing environment ({})", kind.as_str()))
                .await;
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
                let message = otter_core::redact::redact(&format!("{e:#}"));
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
    /// It waits for the store's lock, which is held only briefly and never
    /// across Git (D-135): none of the messages is dropped.
    async fn set_progress(&self, id: &WorkspaceId, message: &str) {
        {
            let mut store = self.store.lock().await;
            let Some(ws) = store.workspace_mut(id.as_str()) else {
                return;
            };
            ws.state_message = Some(message.to_owned());
            let _ = store.save();
        }
        // So a client shows the step (the lock is not held to emit).
        self.events.emit(Event::WorkspacePreparing {
            workspace_id: id.clone(),
        });
    }
}

/// Whether what git has at `ws`'s root is nobody's work (D-135): a checkout
/// cut short (git's add never finished it), in a workspace where nothing
/// ever ran. Sessions start only once a workspace is ready, so one cut short
/// under this otterd has none; one called ready before D-135 may have.
fn nobodys_work(ws: &Workspace, found: &Found) -> bool {
    matches!(
        found,
        Found::Worktree {
            cut_short: true,
            ..
        }
    ) && ws.sessions.iter().all(|s| s.executions.is_empty())
}

/// Whether preparing `ws` makes its worktree (D-135): a Git workspace whose
/// branch isn't recorded — it is once `git worktree add` has finished, so a
/// checkout cut short has none — or whose worktree is gone.
fn needs_worktree(ws: &Workspace) -> bool {
    matches!(&ws.source, WorkspaceSource::Git(g)
        if g.branch.is_empty() || !Path::new(&ws.root).join(".git").exists())
}

/// Wait a moment for a preparation with no Git work in it: an empty or
/// existing directory, or a worktree already there, is often ready by then.
/// Fetching and checking out hardly ever were, so a workspace that needs
/// them comes back at once, preparing (D-135).
async fn wait_if_quick(quick: bool, prep: Option<oneshot::Receiver<()>>) {
    if quick && let Some(ended) = prep {
        let _ = tokio::time::timeout(CREATE_WAIT, ended).await;
    }
}

/// A delete under way (D-135): its workspace's id stays in the daemon's
/// `deleting` until it ends, however it ends.
struct Deleting<'a> {
    set: &'a std::sync::Mutex<HashSet<WorkspaceId>>,
    id: WorkspaceId,
}

impl Drop for Deleting<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

fn being_deleted(ws: &Workspace) -> RpcError {
    RpcError::conflict(format!("workspace `{}` is being deleted", ws.name))
}

pub(crate) fn archived(ws: &Workspace) -> RpcError {
    RpcError::conflict(format!(
        "workspace `{}` is archived; unarchive it first",
        ws.name
    ))
}

async fn remove_managed_dir(dir: &Path) -> RpcResult<()> {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(internal(anyhow::anyhow!("removing {}: {e}", dir.display()))),
    }
}
