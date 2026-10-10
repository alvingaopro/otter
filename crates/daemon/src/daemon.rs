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
use crate::features::FeatureStore;
use crate::git::GitManager;
use crate::paths::Paths;
use crate::store::Store;

pub type RpcResult<T> = Result<T, RpcError>;

pub struct Daemon {
    pub paths: Paths,
    pub store: Mutex<Store>,
    /// Features (D-043): kept apart from workspace state, with their own lock.
    pub features: Mutex<FeatureStore>,
    /// Woken whenever a feature changes, for the controller.
    pub(crate) feature_wake: tokio::sync::Notify,
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
    pub environments: EnvironmentManager,
    pub env_source: String,
    pub shell: String,
    pub started_at: Timestamp,
    /// Resolved environment per workspace (login env + e.g. direnv). Kept in
    /// memory only: it may contain secrets.
    pub(crate) workspace_env: std::sync::Mutex<HashMap<WorkspaceId, EnvMap>>,
    /// Running preparation tasks, so deleting a workspace can cancel them.
    pub(crate) preparing: std::sync::Mutex<HashMap<WorkspaceId, tokio::task::AbortHandle>>,
    /// What each agent's screen showed last (a digest) and since when, by
    /// backend ref (reconcile.rs).
    pub(crate) screens: std::sync::Mutex<HashMap<String, (u64, crate::agents::Screen)>>,
    /// Resource usage, sampled in the background for the host page.
    metrics: crate::metrics::Sampler,
    /// Browser login requests waiting for an app, and how many apps listen.
    pub(crate) logins: crate::login::Pending,
    pub(crate) browser_subscribers: std::sync::atomic::AtomicUsize,
    shutdown: watch::Sender<bool>,
}

impl Daemon {
    pub fn new(
        paths: Paths,
        store: Store,
        features: FeatureStore,
        backend: Arc<dyn ExecutionBackend>,
        events: EventLog,
        env: ResolvedEnv,
        shell: String,
    ) -> Self {
        let metrics = crate::metrics::Sampler::start(paths.state_dir.join("metrics"));
        // A broken settings file mustn't keep the daemon down.
        let settings = crate::settings::HostSettings::load(&paths.state_dir).unwrap_or_else(|e| {
            tracing::warn!("settings: {e:#}; using defaults");
            crate::settings::HostSettings::empty(&paths.state_dir)
        });
        let conversations_dir = paths.state_dir.join("conversations");
        let conversations = crate::runtime::conversations::Conversations::load(&conversations_dir)
            .unwrap_or_else(|e| {
                tracing::warn!("conversations: {e:#}; starting with none");
                crate::runtime::conversations::Conversations::empty(&conversations_dir)
            });
        if let Err(e) = crate::files::install_shim(&paths) {
            tracing::warn!("installing the wl-paste stand-in: {e:#}");
        }
        Daemon {
            git: GitManager::new(paths.home.join("repos"), &env.vars),
            environments: EnvironmentManager::new(&env.vars),
            env_source: env.source,
            paths,
            store: Mutex::new(store),
            features: Mutex::new(features),
            feature_wake: tokio::sync::Notify::new(),
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
            screens: Default::default(),
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
            Request::FsList(p) => json(self.fs_list(&p).await?),
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
            Request::WorkspaceCreate(p) => json(self.workspace_create(p).await?),
            Request::WorkspaceList => json(self.store.lock().await.state.workspaces.clone()),
            Request::WorkspaceGet(r) => json(self.workspace_get(&r.workspace).await?),
            Request::WorkspacePrepare(r) => json(self.workspace_prepare(&r.workspace).await?),
            Request::WorkspaceDelete(p) => json(self.workspace_delete(&p).await?),
            Request::WorkspaceArchive(r) => json(self.workspace_archive(&r.workspace).await?),
            Request::WorkspaceUnarchive(r) => json(self.workspace_unarchive(&r.workspace).await?),
            Request::WorkspaceSetBrief(p) => json(self.workspace_set_brief(p).await?),
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
                    log_id: Some(self.events.log_id().to_owned()),
                    workspaces: store.state.workspaces.clone(),
                })
            }
            Request::SettingsGet => json(self.settings_get()),
            Request::SettingsModels(q) => json(self.settings_models(&q.controller).await?),
            Request::SettingsSet(u) => json(self.settings_set(u)?),
            Request::FeatureList => json(self.feature_list().await),
            Request::FeatureGet(r) => json(self.feature_get(&r.feature).await?),
            Request::FeatureCreate(p) => json(self.feature_create(p).await?),
            Request::FeatureSend(p) => json(self.feature_send(p).await?),
            Request::FeatureAct(p) => json(self.clone().feature_act(p).await?),
            Request::FeatureEvents(p) => json(self.feature_events(&p).await?),
            Request::FeatureArtifact(p) => json(self.feature_artifact(&p).await?),
            Request::FeatureDelete(r) => json(self.feature_delete(&r.feature).await?),
            // Contracts first (D-055); served once runs go through conversations.
            Request::RuntimeCapabilities => {
                let backend = self.settings.read().unwrap().coding_backend();
                let rt = crate::runtime::runtime(crate::runs::DEFAULT_RUNTIME, &backend)
                    .ok_or_else(|| {
                        RpcError::unsupported(format!("no managed agent runtime for `{backend}`"))
                    })?;
                json(rt.capabilities(self.environments.base()))
            }
            Request::ConversationList(q) => json(
                self.conversations
                    .list(q.feature.as_deref())
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
        tracing::info!(provider = %opening.provider, port = ?opening.callback_port, "browser login requested");
        self.events
            .emit(otter_protocol::Event::BrowserOpenRequested {
                request_id: request_id.clone(),
                provider: opening.provider.clone(),
                callback_port: opening.callback_port,
                workspace_id,
            });
        self.logins.add(request_id, opening);
        Ok(())
    }
}
