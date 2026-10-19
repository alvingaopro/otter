//! One long-lived task per configured host: connect, take a snapshot, follow
//! the event stream from that snapshot's cursor, and re-snapshot whenever
//! events arrive (coalesced). The UI gets the whole host list on every change.
//!
//! The host list is `hosts.toml`, shared with `otter`. The app reconciles
//! its tasks against the file at startup, whenever the file changes (so
//! `otter host add` shows up live) and after it edits the file itself.
//!
//! The desktop holds no state of its own: everything shown comes from the
//! hosts, and a lost connection just means "reconnect and snapshot again".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use otter_client::config::{Config, HostEntry, HostTransport};
use otter_client::{ClientError, Connection, Transport};
use serde::Serialize;
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager};

use crate::view::{HostStatus, HostView, WorkspaceView};

/// Coalesce bursts of events into one snapshot.
const COALESCE: Duration = Duration::from_millis(150);
const RETRY_UNREACHABLE: Duration = Duration::from_secs(5);
const RETRY_INCOMPATIBLE: Duration = Duration::from_secs(30);
/// How often to look for edits to `hosts.toml`.
const CONFIG_POLL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Hosts {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// In `hosts.toml` order.
    views: Vec<HostView>,
    running: HashMap<String, Running>,
    config_error: Option<String>,
    config_dir: String,
    config_mtime: Option<SystemTime>,
}

struct Running {
    entry: HostEntry,
    transport: Transport,
    task: JoinHandle<()>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostsPayload {
    hosts: Vec<HostView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config_error: Option<String>,
    config_dir: String,
}

impl Hosts {
    fn payload(&self) -> HostsPayload {
        let inner = self.inner.lock().unwrap();
        HostsPayload {
            hosts: inner.views.clone(),
            config_error: inner.config_error.clone(),
            config_dir: inner.config_dir.clone(),
        }
    }

    pub fn transport(&self, host: &str) -> Result<Transport, String> {
        self.inner
            .lock()
            .unwrap()
            .running
            .get(host)
            .map(|r| r.transport.clone())
            .ok_or_else(|| format!("unknown host `{host}`"))
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut HostView)) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(view) = inner.views.iter_mut().find(|v| v.name == name) {
            f(view);
        }
    }
}

fn config_dir() -> anyhow::Result<PathBuf> {
    Config::dir_from_env()
}

pub(crate) fn load_config() -> anyhow::Result<Config> {
    Config::load(&config_dir()?)
}

fn config_mtime() -> Option<SystemTime> {
    let path = config_dir().ok()?.join("hosts.toml");
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Start following the configured hosts, and keep following `hosts.toml`.
pub fn start(app: &AppHandle) {
    reconcile(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(CONFIG_POLL).await;
            let mtime = config_mtime();
            if mtime != app.state::<Hosts>().inner.lock().unwrap().config_mtime {
                reconcile(&app);
            }
        }
    });
}

/// Make the running host tasks match `hosts.toml`: start new hosts, stop
/// removed ones, restart changed ones.
fn reconcile(app: &AppHandle) {
    let hosts = app.state::<Hosts>();
    let loaded = load_config();
    {
        let mut inner = hosts.inner.lock().unwrap();
        inner.config_mtime = config_mtime();
        inner.config_dir = config_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        match loaded {
            Err(e) => inner.config_error = Some(format!("{e:#}")),
            Ok(config) => {
                inner.config_error = None;
                let mut views = Vec::new();
                for entry in &config.hosts {
                    let same = inner
                        .running
                        .get(&entry.name)
                        .is_some_and(|r| r.entry == *entry);
                    if !same {
                        if let Some(old) = inner.running.remove(&entry.name) {
                            old.task.abort();
                        }
                        let transport = config.transport(entry);
                        let task = tauri::async_runtime::spawn(follow_host(
                            app.clone(),
                            entry.name.clone(),
                            transport.clone(),
                        ));
                        inner.running.insert(
                            entry.name.clone(),
                            Running {
                                entry: entry.clone(),
                                transport,
                                task,
                            },
                        );
                    }
                    let view = inner
                        .views
                        .iter()
                        .find(|v| v.name == entry.name && same)
                        .cloned()
                        .unwrap_or_else(|| HostView {
                            name: entry.name.clone(),
                            describe: describe(entry),
                            status: HostStatus::Connecting,
                            message: None,
                            version: None,
                            agents: Vec::new(),
                            problems: Vec::new(),
                            workspaces: Vec::new(),
                        });
                    views.push(view);
                }
                let keep: Vec<String> = config.hosts.iter().map(|h| h.name.clone()).collect();
                inner.running.retain(|name, r| {
                    let k = keep.contains(name);
                    if !k {
                        r.task.abort();
                    }
                    k
                });
                inner.views = views;
            }
        }
    }
    publish(app);
}

fn describe(entry: &HostEntry) -> String {
    match &entry.transport {
        HostTransport::Ssh { destination, .. } => format!("ssh {destination}"),
        HostTransport::Local { .. } => "this Mac".to_owned(),
    }
}

/// Restart one host's task now (e.g. after installing otterd on it).
fn restart(app: &AppHandle, name: &str) {
    {
        let hosts = app.state::<Hosts>();
        let mut inner = hosts.inner.lock().unwrap();
        if let Some(r) = inner.running.remove(name) {
            r.task.abort();
        }
        if let Some(v) = inner.views.iter_mut().find(|v| v.name == name) {
            v.status = HostStatus::Connecting;
            v.message = None;
        }
    }
    reconcile(app);
}

fn publish(app: &AppHandle) {
    let payload = app.state::<Hosts>().payload();
    let _ = app.emit("hosts", payload);
}

async fn follow_host(app: AppHandle, name: String, transport: Transport) {
    loop {
        let err = follow_once(&app, &name, &transport).await;
        let (status, retry) = match &err {
            ClientError::ProtocolMismatch { .. } => (HostStatus::Incompatible, RETRY_INCOMPATIBLE),
            ClientError::NotInstalled { .. } => (HostStatus::NotInstalled, RETRY_INCOMPATIBLE),
            _ => (HostStatus::Unreachable, RETRY_UNREACHABLE),
        };
        app.state::<Hosts>().update(&name, |v| {
            v.status = status;
            v.message = Some(err.to_string());
        });
        publish(&app);
        tokio::time::sleep(retry).await;
        app.state::<Hosts>()
            .update(&name, |v| v.status = HostStatus::Connecting);
        publish(&app);
    }
}

/// Follow the host until something fails; returns why.
async fn follow_once(app: &AppHandle, name: &str, transport: &Transport) -> ClientError {
    match follow(app, name, transport).await {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

enum Never {}

async fn follow(app: &AppHandle, name: &str, transport: &Transport) -> Result<Never, ClientError> {
    let mut rpc = Connection::connect(transport).await?;
    let version = rpc.server_version.clone();
    // Which agents the host can run (for the new-workspace form); probing
    // is cheap enough to do once per connection.
    // …and what's wrong with it (D-077), as of connecting.
    let status = rpc.host_status().await?;
    app.state::<Hosts>().update(name, |v| {
        v.version = Some(version);
        v.agents = status.agents;
        v.problems = status.problems;
    });
    // Events after the snapshot; each burst triggers a fresh snapshot. If the
    // host can't serve the snapshot's cursor (its event log rotated or started
    // over in between), take another snapshot rather than report an error.
    let mut resyncs = 0;
    let mut stream = loop {
        let snapshot = rpc.snapshot().await?;
        show(app, name, &snapshot.workspaces);
        match Connection::connect(transport)
            .await?
            .subscribe_for_browser(Some(snapshot.cursor()))
            .await
        {
            Ok(stream) => break stream,
            Err(e) if e.is_cursor_expired() && resyncs < 3 => resyncs += 1,
            Err(e) => return Err(e),
        }
    };
    // Timelines may have missed events while the stream was down (or the
    // host's log started over): they reload, then follow the events
    // forwarded below.
    let _ = app.emit("host-resync", HostResync { host: name });
    // Reading the stream isn't cancel-safe, so it gets its own task.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(), ClientError>>(64);
    let (login_app, login_host) = (app.clone(), name.to_owned());
    let reader = tauri::async_runtime::spawn(async move {
        loop {
            let item = match stream.next().await {
                Ok(Some(rec)) => {
                    let _ = login_app.emit(
                        "host-event",
                        HostEvent {
                            host: &login_host,
                            record: &rec,
                        },
                    );
                    // Tasks load on their own (taskSource.ts): their
                    // events, and the text streaming in many times a second
                    // (D-051), don't need a fresh snapshot of the workspaces.
                    if matches!(
                        rec.event,
                        otter_protocol::Event::TaskStream { .. }
                            | otter_protocol::Event::TaskActivity { .. }
                            | otter_protocol::Event::TaskChanged { .. }
                            | otter_protocol::Event::TaskDeleted { .. }
                            | otter_protocol::Event::ConversationChanged { .. }
                            | otter_protocol::Event::OtterChanged { .. }
                            | otter_protocol::Event::OtterStream { .. }
                    ) {
                        continue;
                    }
                    // Browser login: a tool on the host wants a sign-in page,
                    // from a provider built in or (asked) another site.
                    match rec.event {
                        otter_protocol::Event::BrowserOpenRequested { request_id, .. } => {
                            let (app, host) = (login_app.clone(), login_host.clone());
                            tauri::async_runtime::spawn(async move {
                                let _ =
                                    crate::forwards::open_login(app, host, request_id, None).await;
                            });
                        }
                        otter_protocol::Event::BrowserOpenAsked {
                            request_id,
                            site,
                            workspace_id,
                            ..
                        } => crate::sign_in::asked(
                            &login_app,
                            &login_host,
                            &request_id,
                            &site,
                            workspace_id.as_ref(),
                        ),
                        _ => {}
                    }
                    Ok(())
                }
                Ok(None) => Err(ClientError::Closed(String::new())),
                Err(e) => Err(e),
            };
            let end = item.is_err();
            if tx.send(item).await.is_err() || end {
                return;
            }
        }
    });

    let result = loop {
        match rx.recv().await {
            Some(Ok(())) => {}
            Some(Err(e)) => break e,
            None => break ClientError::Closed(String::new()),
        }
        tokio::time::sleep(COALESCE).await;
        let mut ended = None;
        while let Ok(item) = rx.try_recv() {
            if let Err(e) = item {
                ended = Some(e);
            }
        }
        match rpc.snapshot().await {
            Ok(snapshot) => show(app, name, &snapshot.workspaces),
            Err(e) => break e,
        }
        if let Some(e) = ended {
            break e;
        }
    };
    reader.abort();
    Err(result)
}

/// One event as it arrives from a host, for open timelines.
#[derive(Clone, Serialize)]
struct HostEvent<'a> {
    host: &'a str,
    record: &'a otter_protocol::EventRecord,
}

/// A host's event stream (re)started: what came before may have been missed.
#[derive(Clone, Serialize)]
struct HostResync<'a> {
    host: &'a str,
}

fn show(app: &AppHandle, name: &str, workspaces: &[otter_core::Workspace]) {
    let views: Vec<WorkspaceView> = workspaces.iter().map(WorkspaceView::from).collect();
    app.state::<Hosts>().update(name, |v| {
        v.status = HostStatus::Connected;
        v.message = None;
        v.workspaces = views;
    });
    publish(app);
}

#[tauri::command]
pub fn hosts_get(app: AppHandle) -> HostsPayload {
    app.state::<Hosts>().payload()
}

pub(crate) async fn rpc(app: &AppHandle, host: &str) -> Result<Connection, String> {
    let transport = app.state::<Hosts>().transport(host)?;
    Connection::connect(&transport)
        .await
        .map_err(|e| e.to_string())
}

/// Mark what a workspace (or one of its sessions) was asking for as handled.
#[tauri::command]
pub async fn attention_resolve(
    app: AppHandle,
    host: String,
    workspace: String,
    session: Option<String>,
    attention: Option<String>,
) -> Result<usize, String> {
    let mut conn = rpc(&app, &host).await?;
    match attention {
        // One item: what showing a workspace again settles (D-099).
        Some(id) => conn.attention_resolve_item(&workspace, &id).await,
        None => conn.attention_resolve(&workspace, session.as_deref()).await,
    }
    .map_err(|e| e.to_string())
}

/// Start a session again (a new execution of the same session).
#[tauri::command]
pub async fn session_restart(
    app: AppHandle,
    host: String,
    workspace: String,
    session: String,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .session_restart(&workspace, &session)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Managing hosts (the same `hosts.toml` edits as `otter host add/rm`)
// ---------------------------------------------------------------------------

fn app_version(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddOutcome {
    added: bool,
    /// otterd is missing or too old there; adding with `install` fixes that.
    needs_install: bool,
    message: String,
}

/// Register a host. `destination` (anything `ssh` accepts) makes it remote;
/// without one it is this Mac. Unless `force`, the host must answer first;
/// with `install`, otterd is installed (or updated) on it before that.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn host_add(
    app: AppHandle,
    name: String,
    destination: Option<String>,
    otterd_path: Option<String>,
    home: Option<String>,
    install: bool,
    force: bool,
) -> Result<AddOutcome, String> {
    let name = name.trim().to_owned();
    let nonempty = |s: Option<String>| s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let (destination, otterd_path, home) =
        (nonempty(destination), nonempty(otterd_path), nonempty(home));
    let config = load_config().map_err(|e| format!("{e:#}"))?;
    let transport = match destination {
        Some(destination) => HostTransport::Ssh {
            destination,
            ssh_args: Vec::new(),
            otterd_path: otterd_path
                .unwrap_or_else(|| otter_client::config::DEFAULT_REMOTE_OTTERD.to_owned()),
            home,
        },
        // A Finder-launched app has no useful PATH: point at where installs go.
        None => HostTransport::Local {
            otterd_path: Some(otterd_path.unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/.local/bin/otterd")
            })),
            home,
        },
    };
    let entry = HostEntry {
        name: name.clone(),
        transport,
    };
    // Validate before touching the host.
    {
        let mut probe = Config::default();
        probe.add_host(entry.clone()).map_err(|e| e.to_string())?;
        if config.hosts.iter().any(|h| h.name == name) {
            return Err(format!("host `{name}` is already registered"));
        }
    }
    let transport = config.transport(&entry);
    let mut message = String::new();
    if install {
        message = install_release(&app, &transport).await?;
    }
    if !force {
        let checked = async {
            let mut conn = Connection::connect(&transport).await?;
            conn.host_status().await
        }
        .await;
        match checked {
            Ok(status) => {
                if !message.is_empty() {
                    message.push('\n');
                }
                message.push_str(&format!(
                    "otterd {} on {} ({}/{})",
                    status.otterd_version, status.hostname, status.os, status.arch
                ));
            }
            Err(e @ (ClientError::NotInstalled { .. } | ClientError::ProtocolMismatch { .. })) => {
                return Ok(AddOutcome {
                    added: false,
                    needs_install: true,
                    message: e.to_string(),
                });
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    // Read again: the file may have changed while the host was set up (a
    // sign-in site allowed, the CLI adding a host).
    let mut config = load_config().map_err(|e| format!("{e:#}"))?;
    config.add_host(entry).map_err(|e| e.to_string())?;
    config.save().map_err(|e| format!("{e:#}"))?;
    reconcile(&app);
    Ok(AddOutcome {
        added: true,
        needs_install: false,
        message,
    })
}

/// Forget a host. Nothing on it is touched; its sessions keep running.
#[tauri::command]
pub fn host_remove(app: AppHandle, name: String) -> Result<(), String> {
    let mut config = load_config().map_err(|e| format!("{e:#}"))?;
    config.remove_host(&name).map_err(|e| e.to_string())?;
    config.save().map_err(|e| format!("{e:#}"))?;
    reconcile(&app);
    Ok(())
}

/// Install or update otterd on a registered host to this app's version, and
/// restart its daemon (sessions keep running).
#[tauri::command]
pub async fn host_install(app: AppHandle, name: String) -> Result<String, String> {
    let transport = app.state::<Hosts>().transport(&name)?;
    let out = install_release(&app, &transport).await?;
    restart(&app, &name);
    Ok(out)
}

/// Install the Claude worker (D-060) that the host's otterd looks for, its
/// own release's, and nothing else (D-128): otterd stays as it is and keeps
/// running, and finds the worker on the next check.
#[tauri::command]
pub async fn host_install_worker(app: AppHandle, name: String) -> Result<String, String> {
    let transport = app.state::<Hosts>().transport(&name)?;
    let version = Connection::connect(&transport)
        .await
        .map_err(|e| e.to_string())?
        .host_status()
        .await
        .map_err(|e| e.to_string())?
        .otterd_version;
    otter_client::install::install(&transport, &version, false, false)
        .await
        .map_err(|e| e.to_string())
}

/// Install this app's release on a host and stop its running daemon. On
/// this Mac, when its otterd is the one in `~/.local/bin` and the app
/// carries its own (D-085), that's a link to the app's and only the Claude
/// worker is downloaded; otherwise the release's binaries are too.
async fn install_release(app: &AppHandle, transport: &Transport) -> Result<String, String> {
    let linked = match transport {
        Transport::Local { otterd_path, .. }
            if local_bin().is_some_and(|bin| Path::new(otterd_path) == bin.join("otterd")) =>
        {
            link_cli(true)?
        }
        _ => None,
    };
    let out = otter_client::install::install(transport, &app_version(app), linked.is_none(), true)
        .await
        .map_err(|e| e.to_string())?;
    Ok(match linked {
        Some(linked) => format!("{linked}\n{out}"),
        None => out,
    })
}

/// Install `otter` and `otterd` for this user on this Mac (`~/.local/bin`):
/// links to the app's own when it carries them (D-085), else downloaded.
#[tauri::command]
pub async fn install_cli(app: AppHandle) -> Result<String, String> {
    let here = Transport::Local {
        otterd_path: String::new(),
        home: None,
    };
    let linked = link_cli(true)?;
    let out = otter_client::install::install(&here, &app_version(&app), linked.is_none(), false)
        .await
        .map_err(|e| e.to_string())?;
    Ok(match linked {
        Some(linked) => format!("{linked}\n{out}"),
        None => out,
    })
}

/// `~/.local/bin`, where the command-line tools go.
fn local_bin() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin"))
}

const CLI: [&str; 2] = ["otter", "otterd"];

/// This app's `Otter.app/Contents/MacOS`, when it runs from an installed
/// copy. None for a development build, and for an app run from a disk image
/// or a translocated copy, which a link would outlive and an update can't
/// replace (D-125).
pub fn installed_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let path = dir.to_string_lossy();
    let installed = path.ends_with(".app/Contents/MacOS")
        && !path.starts_with("/Volumes/")
        && !path.contains("/AppTranslocation/");
    installed.then(|| dir.to_owned())
}

/// Where this app carries its own `otter` and `otterd` (D-085): beside its
/// executable in an installed `Otter.app`.
fn bundled_cli() -> Option<PathBuf> {
    installed_app().filter(|dir| CLI.iter().all(|b| dir.join(b).is_file()))
}

/// Point `~/.local/bin/otter` and `otterd` at this app's own, so they are
/// always the app's version (D-085). With `replace`, whatever is there is
/// replaced (the developer asked); without it — at every launch — links into
/// an Otter app and copies a release installed are (D-125), so anything else
/// put there is left alone, and nothing is added where there was nothing.
/// None when the app carries no binaries; else what it did.
pub fn link_cli(replace: bool) -> Result<Option<String>, String> {
    let (Some(app), Some(bin)) = (bundled_cli(), local_bin()) else {
        return Ok(None);
    };
    // One at a time: the launch's runs beside a developer's install.
    static LINKING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = LINKING.lock().unwrap_or_else(|e| e.into_inner());
    link(&app, &bin, replace).map_err(|e| format!("couldn't link the command-line tools: {e}"))?;
    Ok(Some(format!(
        "otter and otterd in {} are this app's own; they update with it",
        bin.display()
    )))
}

// ---------------------------------------------------------------------------
// Workspaces and sessions (thin RPCs; the daemon does the work)
// ---------------------------------------------------------------------------

fn err(e: ClientError) -> String {
    e.to_string()
}

/// Create a workspace. Returns its id; it appears through the event stream.
#[tauri::command]
pub async fn workspace_create(
    app: AppHandle,
    host: String,
    name: String,
    brief: Option<otter_core::Brief>,
    source: otter_protocol::SourceSpec,
    sessions: Vec<otter_protocol::SessionSpec>,
) -> Result<String, String> {
    let ws = rpc(&app, &host)
        .await?
        .workspace_create(otter_protocol::WorkspaceCreate {
            name: name.trim().to_owned(),
            brief,
            source,
            sessions: Some(sessions),
        })
        .await
        .map_err(err)?;
    Ok(ws.id.to_string())
}

/// Delete a workspace and its sessions. `force` discards uncommitted changes
/// in a Git worktree; an existing directory is never deleted.
#[tauri::command]
pub async fn workspace_delete(
    app: AppHandle,
    host: String,
    workspace: String,
    force: bool,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .workspace_delete(&workspace, force)
        .await
        .map_err(err)
}

/// Retry a failed workspace's preparation.
#[tauri::command]
pub async fn workspace_prepare(
    app: AppHandle,
    host: String,
    workspace: String,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .workspace_prepare(&workspace)
        .await
        .map(|_| ())
        .map_err(err)
}

/// Put a workspace away: its sessions stop, its files stay (D-036).
#[tauri::command]
pub async fn workspace_archive(
    app: AppHandle,
    host: String,
    workspace: String,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .workspace_archive(&workspace)
        .await
        .map(|_| ())
        .map_err(err)
}

/// Bring an archived workspace back; it prepares again.
#[tauri::command]
pub async fn workspace_unarchive(
    app: AppHandle,
    host: String,
    workspace: String,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .workspace_unarchive(&workspace)
        .await
        .map(|_| ())
        .map_err(err)
}

/// Replace a workspace's brief (D-038). The pane updates from the event.
#[tauri::command]
pub async fn workspace_set_brief(
    app: AppHandle,
    host: String,
    workspace: String,
    brief: otter_core::Brief,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .workspace_set_brief(&workspace, brief)
        .await
        .map(|_| ())
        .map_err(err)
}

/// Another name for a workspace (D-101). The sidebar updates from the event.
#[tauri::command]
pub async fn workspace_rename(
    app: AppHandle,
    host: String,
    workspace: String,
    name: String,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .workspace_rename(&workspace, &name)
        .await
        .map(|_| ())
        .map_err(err)
}

/// Another name for a session (D-101).
#[tauri::command]
pub async fn session_rename(
    app: AppHandle,
    host: String,
    workspace: String,
    session: String,
    name: String,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .session_rename(&workspace, &session, &name)
        .await
        .map(|_| ())
        .map_err(err)
}

/// A workspace's Git work (D-102).
#[tauri::command]
pub async fn workspace_changes(
    app: AppHandle,
    host: String,
    workspace: String,
) -> Result<otter_protocol::work::WorkspaceChanges, String> {
    rpc(&app, &host)
        .await?
        .workspace_changes(&workspace)
        .await
        .map_err(err)
}

/// One changed file's diff (D-102).
#[tauri::command]
pub async fn workspace_diff(
    app: AppHandle,
    host: String,
    workspace: String,
    path: String,
    context: Option<u32>,
) -> Result<otter_protocol::work::FileDiff, String> {
    rpc(&app, &host)
        .await?
        .workspace_diff(&workspace, &path, context)
        .await
        .map_err(err)
}

/// Ports a workspace's sessions listen on (D-104).
#[tauri::command]
pub async fn workspace_ports(
    app: AppHandle,
    host: String,
    workspace: String,
) -> Result<Vec<otter_protocol::work::SessionPort>, String> {
    rpc(&app, &host)
        .await?
        .workspace_ports(&workspace)
        .await
        .map_err(err)
}

/// A session's text, its history included (up to `lines` above the
/// screen), for finding something in it (D-106).
#[tauri::command]
pub async fn session_history(
    app: AppHandle,
    host: String,
    workspace: String,
    session: String,
    lines: u32,
) -> Result<String, String> {
    rpc(&app, &host)
        .await?
        .session_read(&workspace, &session, Some(lines))
        .await
        .map_err(err)
}

/// How many of a host's most recent events a timeline looks through.
const TIMELINE_WINDOW: u32 = 2000;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Timeline {
    /// This workspace's events, oldest first.
    records: Vec<otter_protocol::EventRecord>,
    /// Reaches back to the workspace's creation.
    complete: bool,
}

/// A workspace's recent history, from the host's event log (D-039). Live
/// events follow as `host-event`; after `host-resync`, load again.
#[tauri::command]
pub async fn workspace_events(
    app: AppHandle,
    host: String,
    workspace: String,
) -> Result<Timeline, String> {
    let all = rpc(&app, &host)
        .await?
        .events_list(Some(TIMELINE_WINDOW))
        .await
        .map_err(err)?;
    let records: Vec<_> = all
        .into_iter()
        .filter(|r| r.event.workspace_id().map(|w| w.as_str()) == Some(workspace.as_str()))
        .collect();
    let complete = records
        .first()
        .is_some_and(|r| matches!(r.event, otter_protocol::Event::WorkspaceCreated { .. }));
    Ok(Timeline { records, complete })
}

/// Start a new session in a workspace. Returns its id.
#[tauri::command]
pub async fn session_create(
    app: AppHandle,
    host: String,
    workspace: String,
    spec: otter_protocol::SessionSpec,
) -> Result<String, String> {
    let s = rpc(&app, &host)
        .await?
        .session_create(otter_protocol::SessionCreate { workspace, spec })
        .await
        .map_err(err)?;
    Ok(s.id.to_string())
}

#[tauri::command]
pub async fn session_stop(
    app: AppHandle,
    host: String,
    workspace: String,
    session: String,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .session_stop(&workspace, &session)
        .await
        .map(|_| ())
        .map_err(err)
}

#[tauri::command]
pub async fn session_delete(
    app: AppHandle,
    host: String,
    workspace: String,
    session: String,
) -> Result<(), String> {
    rpc(&app, &host)
        .await?
        .session_delete(&workspace, &session)
        .await
        .map_err(err)
}

/// [`link_cli`]'s work: `bin/otter` and `bin/otterd` → `app/…`.
fn link(app: &Path, bin: &Path, replace: bool) -> std::io::Result<()> {
    if replace {
        std::fs::create_dir_all(bin)?;
    }
    for b in CLI {
        let (to, at) = (app.join(b), bin.join(b));
        let current = std::fs::read_link(&at).ok();
        let ours = || match &current {
            Some(p) => p
                .to_string_lossy()
                .ends_with(&format!(".app/Contents/MacOS/{b}")),
            // A copy a release installed — an older app's download (D-020),
            // `otter host install` — is Otter's too (D-125).
            None => otter_build(&at, b),
        };
        if current.as_ref() == Some(&to) || !(replace || ours()) {
            continue;
        }
        // A link made beside it, then renamed over: never a moment without one.
        let tmp = bin.join(format!(".{b}.link"));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&to, &tmp)?;
        std::fs::rename(&tmp, &at)?;
    }
    Ok(())
}

/// Whether the file at `path` is a build of Otter's `name`: it answers
/// `--version` with `<name> <version>`, as `otter` and `otterd` do.
fn otter_build(path: &Path, name: &str) -> bool {
    path.is_file()
        && std::process::Command::new(path)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| {
                o.status.success()
                    && String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .strip_prefix(name)
                        .and_then(|v| v.strip_prefix(' '))
                        .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
            })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliStatus {
    /// `otter` in `~/.local/bin`, if there.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    path: String,
}

/// Whether this Mac has the command-line tools, and which version.
#[tauri::command]
pub async fn cli_status() -> CliStatus {
    let path = format!(
        "{}/.local/bin/otter",
        std::env::var("HOME").unwrap_or_default()
    );
    let version = tokio::process::Command::new(&path)
        .arg("--version")
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .nth(1)
                .map(str::to_owned)
        });
    CliStatus { version, path }
}

#[cfg(test)]
mod tests {
    use otter_core::SessionKind;
    use otter_protocol::{SessionSpec, SourceSpec};

    /// At launch, links into an Otter app follow this one and anything else
    /// is left; asked, everything there becomes a link (D-085).
    #[test]
    fn command_line_tools_link_to_the_app() {
        let dir = std::env::temp_dir().join(format!("otter-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = dir.join("New/Otter.app/Contents/MacOS");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let old = dir.join("Old/Otter.app/Contents/MacOS/otter");
        std::os::unix::fs::symlink(&old, bin.join("otter")).unwrap();
        std::fs::write(bin.join("otterd"), "by hand").unwrap();

        super::link(&app, &bin, false).unwrap();
        assert_eq!(
            std::fs::read_link(bin.join("otter")).unwrap(),
            app.join("otter")
        );
        assert_eq!(
            std::fs::read_to_string(bin.join("otterd")).unwrap(),
            "by hand"
        );

        super::link(&app, &bin, true).unwrap();
        assert_eq!(
            std::fs::read_link(bin.join("otterd")).unwrap(),
            app.join("otterd")
        );
        // Nothing left beside them.
        assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// At launch, a copy a release installed becomes a link too, so nothing
    /// needs "Update command line tools" (D-125); a program that only shares
    /// the name is left.
    #[test]
    fn released_copies_become_links() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("otter-copies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = dir.join("Otter.app/Contents/MacOS");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for (b, says) in [("otter", "otter 0.6.0"), ("otterd", "otterd-like 1.0")] {
            let path = bin.join(b);
            std::fs::write(&path, format!("#!/bin/sh\necho '{says}'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        super::link(&app, &bin, false).unwrap();
        assert_eq!(
            std::fs::read_link(bin.join("otter")).unwrap(),
            app.join("otter")
        );
        assert!(std::fs::read_link(bin.join("otterd")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The shapes the New workspace / New session dialogs send.
    #[test]
    fn dialog_payloads_deserialize() {
        let git: SourceSpec = serde_json::from_str(
            r#"{"type":"git","repository":"git@github.com:org/repo.git","branch":"fix/x"}"#,
        )
        .unwrap();
        assert!(
            matches!(git, SourceSpec::Git { ref branch, base: None, .. } if branch.as_deref() == Some("fix/x"))
        );
        let empty: SourceSpec = serde_json::from_str(r#"{"type":"empty"}"#).unwrap();
        assert_eq!(empty, SourceSpec::Empty);
        let dir: SourceSpec =
            serde_json::from_str(r#"{"type":"directory","path":"~/src/p"}"#).unwrap();
        assert!(matches!(dir, SourceSpec::Directory { .. }));

        let sessions: Vec<SessionSpec> = serde_json::from_str(
            r#"[{"kind":"agent","provider":"codex","prompt":"fix it"},{"kind":"terminal"},{"kind":"command","name":"tests","command":"cargo test"}]"#,
        )
        .unwrap();
        assert_eq!(sessions[0].kind, SessionKind::Agent);
        assert_eq!(sessions[0].prompt.as_deref(), Some("fix it"));
        assert_eq!(sessions[1], SessionSpec::shell());
        assert_eq!(sessions[2].command.as_deref(), Some("cargo test"));

        // The brief the New workspace / Brief dialogs send.
        let brief: otter_core::Brief =
            serde_json::from_str(r#"{"title":"fix it","decisions":["keep v1"]}"#).unwrap();
        assert_eq!(brief.title.as_deref(), Some("fix it"));
        assert_eq!(brief.decisions, ["keep v1"]);
    }
}

/// Resource usage of a host, for its page.
#[tauri::command]
pub async fn host_metrics(
    app: AppHandle,
    host: String,
) -> Result<otter_protocol::host::HostMetrics, String> {
    rpc(&app, &host).await?.host_metrics().await.map_err(err)
}

/// TCP ports listening on a host.
#[tauri::command]
pub async fn host_ports(
    app: AppHandle,
    host: String,
) -> Result<Vec<otter_protocol::host::ListeningPort>, String> {
    rpc(&app, &host).await?.host_ports().await.map_err(err)
}

/// Recorded usage of a host over a range (`1h`, `24h`, `7d`, `30d`).
#[tauri::command]
pub async fn host_history(
    app: AppHandle,
    host: String,
    range: String,
) -> Result<otter_protocol::host::HostHistory, String> {
    rpc(&app, &host)
        .await?
        .host_history(&range)
        .await
        .map_err(err)
}
