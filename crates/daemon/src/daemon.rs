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
use otter_core::{Capability, HostStatus, Session, Timestamp, WorkspaceId};
use otter_protocol::{PROTOCOL_VERSION, Request, RpcError, StateSnapshot};
use serde::Serialize;
use tokio::sync::{Mutex, watch};

use crate::backend::ExecutionBackend;
use crate::env::{EnvMap, ResolvedEnv, which};
use crate::environment::EnvironmentManager;
use crate::events::EventLog;
use crate::git::GitManager;
use crate::paths::Paths;
use crate::store::Store;
use crate::tasks::TaskStore;

pub type RpcResult<T> = Result<T, RpcError>;

pub struct Daemon {
    /// Triggers (D-083): what starts unattended work.
    pub(crate) triggers: Mutex<crate::triggers::TriggerStore>,
    /// Workspaces claimed by a run that is starting, and by which task (D-079).
    pub(crate) starting:
        std::sync::Mutex<std::collections::HashMap<otter_core::WorkspaceId, otter_core::TaskId>>,
    /// Processes seen dead, and since when: their end is taken a moment
    /// later, once the backend has their last output (D-076).
    pub(crate) dying: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// The first reconciliation is done: what died while otterd was down has
    /// long settled.
    pub(crate) reconciled_once: std::sync::atomic::AtomicBool,
    /// Problems otterd met and keeps until they're gone (D-077), by kind.
    pub(crate) problems:
        std::sync::Mutex<std::collections::BTreeMap<&'static str, otter_core::Problem>>,
    pub paths: Paths,
    pub store: Mutex<Store>,
    /// Tasks (D-043): kept apart from workspace state, with their own lock.
    pub tasks: Mutex<TaskStore>,
    /// Otter's conversation on this host (D-093).
    pub(crate) chat: Mutex<crate::chat::ChatStore>,
    /// Taken by every change to Otter's memories (D-113).
    pub(crate) memory_lock: Mutex<()>,
    /// Woken whenever a task changes, for the controller.
    pub(crate) task_wake: tokio::sync::Notify,
    /// Tasks that just ended, whose own workspaces the controller puts
    /// away (D-105).
    pub(crate) finished: std::sync::Mutex<Vec<otter_core::TaskId>>,
    /// Managed agent runs in progress (runs.rs).
    pub(crate) runs: crate::runs::Runs,
    /// Runtime conversations (D-055): what the coding agents did, turn by turn.
    pub(crate) conversations: Arc<crate::runtime::conversations::Conversations>,
    /// The Control Agent's working memory (controller.rs).
    pub(crate) controller: std::sync::Mutex<crate::controller::ControllerState>,
    /// Host settings and API keys (settings.rs, D-048).
    pub(crate) settings: std::sync::RwLock<crate::settings::HostSettings>,
    pub backend: Arc<dyn ExecutionBackend>,
    pub events: EventLog,
    pub git: GitManager,
    /// Otter's own git identity, key and GitHub sign-in (D-133).
    pub git_account: Arc<crate::git_account::GitAccount>,
    pub environments: EnvironmentManager,
    pub env_source: String,
    pub shell: String,
    pub started_at: Timestamp,
    /// Resolved environment per workspace (login env + e.g. direnv). Kept in
    /// memory only: it may contain secrets.
    pub(crate) workspace_env: std::sync::Mutex<HashMap<WorkspaceId, EnvMap>>,
    /// Running preparation tasks, so deleting a workspace can cancel them
    /// and wait until they're gone (D-135).
    pub(crate) preparing: std::sync::Mutex<HashMap<WorkspaceId, tokio::task::JoinHandle<()>>>,
    /// Workspaces being deleted: one delete at a time, and no preparation
    /// starts meanwhile (D-135).
    pub(crate) deleting: std::sync::Mutex<std::collections::HashSet<WorkspaceId>>,
    /// What each agent's screen showed last (a digest) and since when, by
    /// backend ref (reconcile.rs).
    pub(crate) screens: std::sync::Mutex<HashMap<String, (u64, crate::agents::Screen)>>,
    /// Terminals a client shows on screen (`attention::OnScreen`, D-137).
    pub(crate) on_screen: std::sync::Mutex<crate::attention::Screens>,
    /// Resource usage, sampled in the background for the host page.
    pub(crate) metrics: crate::metrics::Sampler,
    /// Browser login requests waiting for an app, and how many apps listen.
    pub(crate) logins: crate::login::Pending,
    pub(crate) browser_subscribers: std::sync::atomic::AtomicUsize,
    shutdown: watch::Sender<bool>,
}

impl Daemon {
    pub fn new(
        paths: Paths,
        store: Store,
        tasks: TaskStore,
        backend: Arc<dyn ExecutionBackend>,
        events: EventLog,
        env: ResolvedEnv,
        shell: String,
    ) -> Self {
        let metrics = crate::metrics::Sampler::start(paths.state_dir.join("metrics"));
        let chat = {
            let dir = paths.state_dir.join("otter");
            crate::chat::ChatStore::load(&dir).unwrap_or_else(|e| {
                tracing::warn!("Otter's conversation: {e:#}; starting with none");
                crate::chat::ChatStore::empty(&dir)
            })
        };
        let mut problems = std::collections::BTreeMap::new();
        // A broken settings file mustn't keep the daemon down — but it's said (D-077).
        let git_account = Arc::new(crate::git_account::GitAccount::load(&paths.state_dir));
        let settings = crate::settings::HostSettings::load(&paths.state_dir).unwrap_or_else(|e| {
            tracing::warn!("settings: {e:#}; using defaults");
            problems.insert(
                "settings",
                otter_core::Problem {
                    what: format!("Otter's settings on this host couldn't be read ({e:#}); the defaults are in use."),
                    next: format!(
                        "Fix or remove {} and restart otterd; setting the model and keys again rewrites it.",
                        paths.state_dir.join("settings.json").display()
                    ),
                },
            );
            crate::settings::HostSettings::empty(&paths.state_dir)
        });
        let conversations_dir = paths.state_dir.join("conversations");
        let conversations = crate::runtime::conversations::Conversations::load(&conversations_dir)
            .unwrap_or_else(|e| {
                tracing::warn!("conversations: {e:#}; starting with none");
                problems.insert(
                    "conversations",
                    otter_core::Problem {
                        what: format!("The coding agent's conversations couldn't be loaded ({e:#})."),
                        next: format!(
                            "Check {} (permissions, a full disk), then restart otterd; nothing there was changed.",
                            conversations_dir.display()
                        ),
                    },
                );
                crate::runtime::conversations::Conversations::empty(&conversations_dir)
            });
        if let Err(e) = crate::files::install_shim(&paths) {
            tracing::warn!("installing the wl-paste stand-in: {e:#}");
        }
        let triggers = crate::triggers::TriggerStore::load(&paths.state_dir).unwrap_or_else(|e| {
            tracing::warn!("triggers: {e:#}; starting with none");
            problems.insert(
                "triggers",
                otter_core::Problem {
                    what: format!("The triggers on this host couldn't be read ({e:#}): none fire."),
                    next: format!(
                        "Fix or remove {} and restart otterd.",
                        paths.state_dir.join("triggers.json").display()
                    ),
                },
            );
            crate::triggers::TriggerStore::empty(&paths.state_dir)
        });
        Daemon {
            triggers: Mutex::new(triggers),
            starting: Default::default(),
            dying: Default::default(),
            reconciled_once: Default::default(),
            problems: std::sync::Mutex::new(problems),
            git: GitManager::new(paths.home.join("repos"), &env.vars, git_account.clone()),
            git_account,
            environments: EnvironmentManager::new(&env.vars, crate::files::shim_dir(&paths)),
            env_source: env.source,
            paths,
            store: Mutex::new(store),
            tasks: Mutex::new(tasks),
            chat: Mutex::new(chat),
            memory_lock: Mutex::new(()),
            task_wake: tokio::sync::Notify::new(),
            finished: Default::default(),
            runs: Default::default(),
            conversations: Arc::new(conversations),
            controller: Default::default(),
            settings: std::sync::RwLock::new(settings),
            backend,
            events,
            shell,
            started_at: Utc::now(),
            workspace_env: Default::default(),
            preparing: Default::default(),
            deleting: Default::default(),
            screens: Default::default(),
            on_screen: Default::default(),
            metrics,
            logins: Default::default(),
            browser_subscribers: Default::default(),
            shutdown: watch::channel(false).0,
        }
    }

    pub fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Whether otterd is on its way out.
    pub fn shutting_down(&self) -> bool {
        *self.shutdown.borrow()
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
            Request::HostMetrics => match self.metrics.latest() {
                Some(m) => json(m),
                None => Err(RpcError::new(
                    otter_protocol::ErrorCode::Unavailable,
                    "measuring; try again in a few seconds",
                )),
            },
            Request::HostPorts => json(crate::metrics::listening_ports().await),
            Request::HostProcesses => json(self.host_processes().await?),
            Request::HostProcessStop(p) => json(self.host_process_stop(p).await?),
            Request::FsList(p) => json(self.fs_list(&p).await?),
            Request::FsRemove(p) => json(self.fs_remove(&p).await?),
            Request::FsRename(p) => json(self.fs_rename(&p).await?),
            Request::FsRead(p) => json(self.fs_read(&p).await?),
            Request::FsWrite(p) => json(self.fs_write(&p).await?),
            Request::SessionPasteImage(p) => json(self.paste_image(&p).await?),
            Request::BrowserOpen(p) => json(self.browser_open(&p).await?),
            Request::BrowserTake(p) => match self.logins.take(&p.request_id) {
                Some(o) => json(otter_protocol::BrowserOpening {
                    url: o.url,
                    provider: o.provider,
                    callback_port: o.callback_port,
                }),
                None => Err(RpcError::not_found(
                    "no such sign-in request (taken or expired)",
                )),
            },
            Request::HostHistory(q) => match self.metrics.history(&q.range) {
                Some(h) => json(h),
                None => Err(RpcError::invalid(format!(
                    "unknown range `{}` (1h, 24h, 7d or 30d)",
                    q.range
                ))),
            },
            // Workspaces as shown: never a token from a repository URL (D-075).
            Request::WorkspaceCreate(p) => json(self.workspace_create(p).await?.shown()),
            Request::WorkspaceList => json(
                self.store
                    .lock()
                    .await
                    .state
                    .workspaces
                    .iter()
                    .cloned()
                    .map(otter_core::Workspace::shown)
                    .collect::<Vec<_>>(),
            ),
            Request::WorkspaceGet(r) => json(self.workspace_get(&r.workspace).await?.shown()),
            Request::WorkspacePrepare(r) => {
                json(self.workspace_prepare(&r.workspace).await?.shown())
            }
            Request::WorkspaceDelete(p) => json(self.workspace_delete(&p).await?),
            Request::WorkspaceArchive(r) => {
                json(self.workspace_archive(&r.workspace).await?.shown())
            }
            Request::WorkspaceUnarchive(r) => {
                json(self.workspace_unarchive(&r.workspace).await?.shown())
            }
            Request::WorkspaceSetBrief(p) => json(self.workspace_set_brief(p).await?.shown()),
            Request::WorkspaceRename(p) => json(self.workspace_rename(p).await?.shown()),
            Request::WorkspaceChanges(r) => json(self.workspace_changes(&r.workspace).await?),
            Request::WorkspaceDiff(p) => json(
                self.workspace_diff(&p.workspace, &p.path, p.context)
                    .await?,
            ),
            Request::WorkspacePorts(r) => json(self.workspace_ports(&r.workspace).await?),
            Request::WorkspacePublish(p) => json(self.workspace_publish(p).await?),
            Request::SessionCreate(p) => json(self.session_create(p).await?),
            Request::SessionStop(r) => json(self.session_stop(&r).await?),
            Request::SessionRestart(r) => json(self.session_restart(&r).await?),
            Request::SessionDelete(r) => json(self.session_delete(&r).await?),
            Request::SessionRename(p) => json(self.session_rename(&p).await?),
            Request::SessionMove(p) => json(self.session_move(&p).await?),
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
                    log_id: Some(self.events.log_id().to_owned()),
                    workspaces: store.state.workspaces.clone(),
                })
            }
            Request::SettingsGet => json(self.settings_get()),
            Request::SettingsModels(q) => json(self.settings_models(&q.controller).await?),
            Request::SettingsSet(u) => json(self.settings_set(u)?),
            Request::GitKeyCreate(p) => {
                let env = self.environments.base();
                self.git_account
                    .key_create(p.replace, env)
                    .await
                    .map_err(|e| RpcError::conflict(format!("{e:#}")))?;
                json(self.git_account.view())
            }
            Request::GitKeyAdd => {
                let env = self.environments.base();
                self.git_account
                    .key_add(env)
                    .await
                    .map_err(|e| RpcError::conflict(format!("{e:#}")))?;
                json(self.git_check().await)
            }
            Request::GitCheck => json(self.git_check().await),
            Request::GitHubLogin(p) => {
                let env = self.environments.base();
                self.git_account
                    .login(p.token, env)
                    .await
                    .map_err(|e| RpcError::conflict(format!("{e:#}")))?;
                json(self.git_account.view())
            }
            Request::GitHubLogout => {
                self.git_account
                    .logout()
                    .await
                    .map_err(|e| RpcError::internal(format!("{e:#}")))?;
                json(self.git_account.view())
            }
            Request::TaskList => json(
                self.task_list()
                    .await
                    .into_iter()
                    .map(|f| self.for_clients(f))
                    .collect::<Vec<_>>(),
            ),
            Request::TaskGet(r) => json(self.for_clients(self.task_get(&r.task).await?)),
            Request::TaskCreate(p) => json(self.for_clients(self.task_create(p).await?)),
            Request::TaskSend(p) => json(self.for_clients(self.task_send(p).await?)),
            Request::TaskAct(p) => json(self.for_clients(self.clone().task_act(p).await?)),
            Request::TaskEvents(p) => json(self.task_events(&p).await?),
            Request::OtterGet => json(self.otter_get().await),
            Request::OtterSend(p) => json(self.otter_send(p).await?),
            Request::OtterEvents(p) => json(self.otter_events(&p).await?),
            Request::MemoryGet(p) => json(self.memory_get(&p).await?),
            Request::MemorySet(p) => json(self.memory_set(p).await?),
            Request::TaskArtifact(p) => json(self.task_artifact(&p).await?),
            Request::TriggerCreate(p) => json(self.trigger_create(p).await?),
            Request::TriggerList => json(self.trigger_list().await),
            Request::TriggerDelete(r) => json(self.trigger_delete(&r.trigger).await?),
            Request::TriggerRun(r) => json(self.trigger_run(&r.trigger, r.occurrence).await?),
            Request::TaskDelete(r) => json(self.task_delete(&r.task).await?),
            // Contracts first (D-055); served once runs go through conversations.
            Request::RuntimeCapabilities => {
                let backend = self.settings.read().unwrap().coding_backend();
                let rt = crate::runtime::runtime(crate::runs::DEFAULT_RUNTIME, &backend)
                    .ok_or_else(|| {
                        RpcError::unsupported(format!("no managed agent runtime for `{backend}`"))
                    })?;
                json(rt.capabilities(self.environments.base()))
            }
            Request::RuntimeTest(p) => json(self.runtime_test(p).await?),
            Request::ConversationList(q) => json(
                self.conversations
                    .list(q.task.as_deref())
                    .iter()
                    .map(|c| self.conversation_view(c))
                    .collect::<Vec<_>>(),
            ),
            Request::ConversationGet(r) => match self.conversations.get(&r.conversation) {
                Some(c) => json(self.conversation_view(&c)),
                None => Err(RpcError::not_found(format!(
                    "no conversation `{}`",
                    r.conversation
                ))),
            },
            Request::ConversationHistory(q) => json(self.conversation_history(&q)?),
            Request::Shutdown | Request::SessionAttach(_) | Request::EventsSubscribe(_) => {
                Err(RpcError::invalid(format!(
                    "{} must be handled by the connection",
                    req.method()
                )))
            }
        }
    }

    fn conversation_view(
        &self,
        c: &otter_core::conversation::Conversation,
    ) -> otter_protocol::conversation::ConversationView {
        let mut v = otter_protocol::conversation::ConversationView::of(c);
        v.read_only = self.conversations.read_only(c.id.as_str());
        v
    }

    fn conversation_history(
        &self,
        q: &otter_protocol::conversation::HistoryQuery,
    ) -> RpcResult<otter_protocol::conversation::HistoryPage> {
        use otter_protocol::conversation::{HistoryCursor, HistoryPage};
        let limit = q.limit.unwrap_or(100).clamp(1, 500) as usize;
        let after = q.after.as_ref().map_or(0, |c| c.seq);
        let (log_id, records) = self
            .conversations
            .history(&q.conversation, after, limit, 1 << 20)
            .ok_or_else(|| RpcError::not_found(format!("no conversation `{}`", q.conversation)))?
            .map_err(|e| RpcError::internal(format!("reading the journal: {e:#}")))?;
        if let Some(c) = &q.after
            && c.log_id != log_id
        {
            return Err(RpcError::new(
                otter_protocol::ErrorCode::CursorExpired,
                "that cursor is from another journal; start again from the beginning",
            ));
        }
        let next = (records.len() == limit).then(|| HistoryCursor {
            log_id: log_id.clone(),
            seq: records.last().map_or(after, |r| r.seq),
        });
        Ok(HistoryPage {
            log_id,
            records: records
                .into_iter()
                .filter_map(|r| serde_json::to_value(r).ok())
                .collect(),
            next,
        })
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
        let otter = self.how_otter_answers();
        let mut problems = self.problems_now(&git, &tmux).await;
        // Asked for and not had: Otter answers without its tools here.
        if let Err(why) = &otter
            && self.brain_choice().v2
        {
            problems.push(otter_core::Problem {
                what: format!("Otter answers {why}."),
                next: "Choose Claude Code as Otter's model in Settings and install the Claude worker (`otter host install`), or turn Otter's tools off in Settings.".into(),
            });
        }
        HostStatus {
            hostname: hostname(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            otterd_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            started_at: self.started_at,
            otterd_home: self.paths.home.to_string_lossy().into_owned(),
            shell: self.shell.clone(),
            environment_source: self.env_source.clone(),
            problems,
            log: Some(self.paths.log_file.to_string_lossy().into_owned()),
            capabilities: vec![git, tmux, nix, direnv],
            agents,
            otter: Some(match otter {
                Ok(how) | Err(how) => how,
            }),
        }
    }

    /// Note a problem otterd met (D-077); a newer one of the same kind
    /// replaces it.
    pub(crate) fn note_problem(&self, kind: &'static str, what: String, next: String) {
        self.problems
            .lock()
            .unwrap()
            .insert(kind, otter_core::Problem { what, next });
    }

    /// The problem of that kind is gone.
    pub(crate) fn clear_problem(&self, kind: &'static str) {
        self.problems.lock().unwrap().remove(kind);
    }

    /// Everything wrong with this host now, each with what to do (D-077).
    async fn problems_now(&self, git: &Capability, tmux: &Capability) -> Vec<otter_core::Problem> {
        use otter_core::Problem;
        let mut out: Vec<Problem> = self.problems.lock().unwrap().values().cloned().collect();
        if !tmux.available {
            out.push(Problem {
                what: "tmux isn't installed: sessions can't start.".into(),
                next: "Install tmux 3.2 or newer on the host, then restart otterd.".into(),
            });
        }
        if !git.available {
            out.push(Problem {
                what: "Git isn't installed: Git workspaces can't be made.".into(),
                next: "Install git on the host.".into(),
            });
        }
        if self.env_source.contains("failed") {
            out.push(Problem {
                what: format!(
                    "The login shell's environment couldn't be captured ({}): tools on its PATH may be missing.",
                    self.shell
                ),
                next: format!(
                    "Check that `{} -l -c true` works on the host (no prompts, no errors), then restart otterd; its log says why.",
                    self.shell
                ),
            });
        }
        let backend = self.settings.read().unwrap().coding_backend();
        if let Some(rt) = crate::runtime::runtime(crate::runs::DEFAULT_RUNTIME, &backend) {
            let caps = rt.capabilities(self.environments.base());
            if !caps.available {
                out.push(Problem {
                    what: format!("The coding runtime ({backend}) isn't ready: tasks can't run."),
                    next: if caps.notes.is_empty() {
                        "`otter runtime status` says why.".into()
                    } else {
                        caps.notes.join(" ")
                    },
                });
            }
        }
        let read_only: Vec<String> = self
            .conversations
            .list(None)
            .iter()
            .filter(|c| self.conversations.read_only(c.id.as_str()).is_some())
            .map(|c| c.id.to_string())
            .collect();
        if !read_only.is_empty() {
            out.push(Problem {
                what: format!(
                    "{} conversation(s) can't be written any more: {}.",
                    read_only.len(),
                    read_only.join(", ")
                ),
                next: "What they recorded is still readable (`otter conversation show`); check the disk under ~/.otter/state/conversations. Their tasks carry on in new conversations.".into(),
            });
        }
        out
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

pub(crate) fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for writes of buf.len() bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown".to_owned();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Counts a client (the app, `otter attach`) subscribed for browser login
/// while it stays subscribed.
pub(crate) struct BrowserSubscriber(Arc<Daemon>);

impl BrowserSubscriber {
    pub(crate) fn new(daemon: &Arc<Daemon>) -> Self {
        daemon
            .browser_subscribers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        BrowserSubscriber(daemon.clone())
    }
}

impl Drop for BrowserSubscriber {
    fn drop(&mut self) {
        self.0
            .browser_subscribers
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Daemon {
    /// `runtime.test` (D-124): the backend and model asked, else the
    /// settings'; with no model set, a small one.
    async fn runtime_test(
        &self,
        p: otter_protocol::conversation::RuntimeTestRequest,
    ) -> RpcResult<otter_protocol::conversation::RuntimeTest> {
        let (backend, model) = {
            let s = self.settings.read().unwrap();
            (
                p.backend
                    .filter(|b| !b.is_empty())
                    .unwrap_or_else(|| s.coding_backend()),
                p.model.or_else(|| s.coding_model()),
            )
        };
        let rt = crate::runtime::runtime(crate::runs::DEFAULT_RUNTIME, &backend)
            .ok_or_else(|| RpcError::invalid(format!("unknown coding backend `{backend}`")))?;
        let model = model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty());
        // Signed in as a run would be (D-142).
        let mut env = self.environments.base().clone();
        self.agent_secrets(crate::runs::DEFAULT_RUNTIME, &mut env);
        crate::runtime::test_run::run(
            rt,
            &backend,
            env,
            &self.paths.state_dir.join("runtime-test"),
            model,
        )
        .await
        .map_err(RpcError::conflict)
    }

    /// `git.check` (D-133): the key and Otter's gh against GitHub.
    async fn git_check(&self) -> otter_protocol::host::GitCheck {
        let (ssh, github) = self.git_account.check(self.environments.base()).await;
        otter_protocol::host::GitCheck {
            ssh,
            github,
            git: self.git_account.view(),
        }
    }

    /// A browser stand-in on this host asks for a sign-in page on the Mac.
    async fn browser_open(&self, p: &otter_protocol::BrowserOpen) -> RpcResult<()> {
        let opening = crate::login::check(&p.url).map_err(RpcError::invalid)?;
        if self
            .browser_subscribers
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            return Err(RpcError::new(
                otter_protocol::ErrorCode::Unavailable,
                "no Otter app or attached terminal is connected to open it",
            ));
        }
        let workspace_id = match &p.session {
            Some(sid) => {
                let store = self.store.lock().await;
                store
                    .state
                    .workspaces
                    .iter()
                    .find(|w| w.sessions.iter().any(|s| s.id.as_str() == sid))
                    .map(|w| w.id.clone())
            }
            None => None,
        };
        let request_id = format!(
            "login_{}",
            otter_core::SessionId::generate()
                .as_str()
                .trim_start_matches("ses_")
        );
        tracing::info!(provider = %opening.provider, port = ?opening.callback_port, built_in = opening.built_in, "browser login requested");
        let built_in = opening.built_in;
        let site = opening.provider.clone();
        self.events.emit(if built_in {
            otter_protocol::Event::BrowserOpenRequested {
                request_id: request_id.clone(),
                provider: opening.provider.clone(),
                callback_port: opening.callback_port,
                workspace_id,
            }
        } else {
            otter_protocol::Event::BrowserOpenAsked {
                request_id: request_id.clone(),
                site: opening.provider.clone(),
                callback_port: opening.callback_port,
                workspace_id,
            }
        });
        self.logins.add(request_id, opening);
        if built_in {
            Ok(())
        } else {
            // The tool prints its link, as without a browser; the Mac may
            // still open the page, and the tool's login goes on.
            Err(RpcError::invalid(format!(
                "{site} is not a sign-in site Otter opens by itself: the Otter app on your Mac asks whether to open it"
            )))
        }
    }
}
