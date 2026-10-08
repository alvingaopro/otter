//! Port mappings for the host page, on top of `otter_client::forward`.
//!
//! The app owns the list: pinned mappings come from `ports.toml` at start,
//! others live until the app quits. A loop re-applies a host's mappings
//! whenever its shared SSH connection is new (different master pid) — after
//! a reconnect, sleep or app start — and reports each one's state to the UI.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use otter_client::config::Config;
use otter_client::forward::{self, Direction, Mapping};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::hosts::Hosts;

const CHECK_EVERY: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Forwards {
    inner: Mutex<Vec<Entry>>,
}

#[derive(Clone)]
struct Entry {
    mapping: Mapping,
    state: State,
    /// Master pid the mapping was last applied on.
    applied_on: Option<u32>,
    /// A browser login's callback: removed when the host stops listening on
    /// the port (the login finished) or at this time.
    login_until: Option<std::time::Instant>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
enum State {
    Pending,
    Active,
    Failed { message: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardView {
    host: String,
    direction: Direction,
    listen_port: u16,
    target_host: String,
    target_port: u16,
    pinned: bool,
    /// A browser login's callback, removed when the login is done.
    login: bool,
    #[serde(flatten)]
    state: State,
}

impl Forwards {
    fn views(&self) -> Vec<ForwardView> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|e| ForwardView {
                host: e.mapping.host.clone(),
                direction: e.mapping.direction,
                listen_port: e.mapping.listen_port,
                target_host: e.mapping.target_host.clone(),
                target_port: e.mapping.target_port,
                pinned: e.mapping.pinned,
                login: e.login_until.is_some(),
                state: e.state.clone(),
            })
            .collect()
    }
}

fn config_dir() -> Option<std::path::PathBuf> {
    Config::dir_from_env().ok()
}

fn publish(app: &AppHandle) {
    let _ = app.emit("forwards", app.state::<Forwards>().views());
}

fn save_pins(app: &AppHandle) {
    let Some(dir) = config_dir() else { return };
    let all: Vec<Mapping> = app
        .state::<Forwards>()
        .inner
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.mapping.clone())
        .collect();
    if let Err(e) = forward::save_pins(&dir, &all) {
        tracing_log(&format!("saving ports.toml: {e:#}"));
    }
}

fn tracing_log(msg: &str) {
    eprintln!("otter: {msg}");
}

/// Load pinned mappings and keep every mapping applied.
pub fn start(app: &AppHandle) {
    if let Some(dir) = config_dir() {
        match forward::load_pins(&dir) {
            Ok(pins) => {
                *app.state::<Forwards>().inner.lock().unwrap() = pins
                    .into_iter()
                    .map(|mapping| Entry {
                        mapping,
                        state: State::Pending,
                        applied_on: None,
                        login_until: None,
                    })
                    .collect();
            }
            Err(e) => tracing_log(&format!("reading ports.toml: {e:#}")),
        }
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            keep_applied(&app).await;
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}

/// Re-apply the mappings of every host whose connection changed.
async fn keep_applied(app: &AppHandle) {
    reap_logins(app).await;
    let entries = app.state::<Forwards>().inner.lock().unwrap().clone();
    let mut by_host: HashMap<String, Vec<Entry>> = HashMap::new();
    for e in entries {
        by_host.entry(e.mapping.host.clone()).or_default().push(e);
    }
    let mut changed = false;
    for (host, entries) in by_host {
        // Hosts that aren't registered (any more) or aren't up yet: wait.
        let Ok(transport) = app.state::<Hosts>().transport(&host) else {
            continue;
        };
        let Some(pid) = forward::master_pid(&transport).await else {
            continue;
        };
        for e in entries {
            if e.applied_on == Some(pid) && e.state == State::Active {
                continue;
            }
            let result = forward::apply(&transport, &e.mapping).await;
            let forwards = app.state::<Forwards>();
            let mut all = forwards.inner.lock().unwrap();
            if let Some(cur) = all.iter_mut().find(|c| c.mapping.same_slot(&e.mapping)) {
                cur.applied_on = Some(pid);
                cur.state = match result {
                    Ok(()) => State::Active,
                    Err(err) => State::Failed {
                        message: err.to_string(),
                    },
                };
                changed = true;
            }
        }
    }
    if changed {
        publish(app);
    }
}

#[tauri::command]
pub fn forwards_get(app: AppHandle) -> Vec<ForwardView> {
    app.state::<Forwards>().views()
}

/// Add a mapping and apply it now.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn forward_add(
    app: AppHandle,
    host: String,
    direction: Direction,
    listen_port: u16,
    target_host: Option<String>,
    target_port: u16,
    pinned: bool,
) -> Result<(), String> {
    let mapping = Mapping {
        host: host.clone(),
        direction,
        listen_port,
        target_host: target_host
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "localhost".to_owned()),
        target_port,
        pinned,
    };
    if app
        .state::<Forwards>()
        .inner
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.mapping.same_slot(&mapping))
    {
        return Err(format!("port {listen_port} is already mapped for {host}"));
    }
    let transport = app.state::<Hosts>().transport(&host)?;
    forward::apply(&transport, &mapping)
        .await
        .map_err(|e| e.to_string())?;
    let pid = forward::master_pid(&transport).await;
    app.state::<Forwards>().inner.lock().unwrap().push(Entry {
        mapping,
        state: State::Active,
        applied_on: pid,
        login_until: None,
    });
    if pinned {
        save_pins(&app);
    }
    publish(&app);
    Ok(())
}

/// Remove a mapping (and unpin it).
#[tauri::command]
pub async fn forward_remove(
    app: AppHandle,
    host: String,
    direction: Direction,
    listen_port: u16,
) -> Result<(), String> {
    let removed = {
        let forwards = app.state::<Forwards>();
        let mut all = forwards.inner.lock().unwrap();
        let idx = all.iter().position(|e| {
            e.mapping.host == host
                && e.mapping.direction == direction
                && e.mapping.listen_port == listen_port
        });
        idx.map(|i| all.remove(i))
    };
    let Some(entry) = removed else {
        return Ok(());
    };
    if let Ok(transport) = app.state::<Hosts>().transport(&host) {
        // Already gone with the connection is fine.
        let _ = forward::cancel(&transport, &entry.mapping).await;
    }
    if entry.mapping.pinned {
        save_pins(&app);
    }
    publish(&app);
    Ok(())
}

/// Keep (or stop keeping) a mapping across restarts.
#[tauri::command]
pub fn forward_pin(
    app: AppHandle,
    host: String,
    direction: Direction,
    listen_port: u16,
    pinned: bool,
) {
    {
        let forwards = app.state::<Forwards>();
        let mut all = forwards.inner.lock().unwrap();
        for e in all.iter_mut() {
            if e.mapping.host == host
                && e.mapping.direction == direction
                && e.mapping.listen_port == listen_port
            {
                e.mapping.pinned = pinned;
            }
        }
    }
    save_pins(&app);
    publish(&app);
}

/// How long a browser login's callback port stays mapped at most.
const LOGIN_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// A tool on `host` asked for a sign-in page (`BrowserOpenRequested`): take
/// it, map its callback port to this Mac if it has one, and open it here.
pub async fn open_login(app: AppHandle, host: String, request_id: String) {
    use tauri_plugin_opener::OpenerExt;
    let Ok(transport) = app.state::<Hosts>().transport(&host) else {
        return;
    };
    let Ok(mut conn) = otter_client::Connection::connect(&transport).await else {
        return;
    };
    // Another app (another Mac) may have taken it first: then it's theirs.
    let Ok(opening) = conn.browser_take(&request_id).await else {
        return;
    };
    if let Some(port) = opening.callback_port {
        let mapping = Mapping {
            host: host.clone(),
            direction: Direction::ToLocal,
            listen_port: port,
            target_host: "localhost".into(),
            target_port: port,
            pinned: false,
        };
        let state = match forward::apply(&transport, &mapping).await {
            Ok(()) => State::Active,
            Err(e) => State::Failed {
                message: format!("sign-in callback: {e}"),
            },
        };
        let pid = forward::master_pid(&transport).await;
        {
            let forwards = app.state::<Forwards>();
            let mut all = forwards.inner.lock().unwrap();
            all.retain(|e| !e.mapping.same_slot(&mapping));
            all.push(Entry {
                mapping,
                state,
                applied_on: pid,
                login_until: Some(std::time::Instant::now() + LOGIN_TTL),
            });
        }
        publish(&app);
    }
    if let Err(e) = app.opener().open_url(&opening.url, None::<&str>) {
        tracing_log(&format!("opening a sign-in page: {e}"));
    }
}

/// Drop login callbacks whose time is up or whose port the host no longer
/// listens on (the tool got its redirect and exited).
async fn reap_logins(app: &AppHandle) {
    let logins: Vec<Entry> = app
        .state::<Forwards>()
        .inner
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.login_until.is_some())
        .cloned()
        .collect();
    let mut gone = Vec::new();
    for e in logins {
        let expired = e.login_until.is_some_and(|t| std::time::Instant::now() > t);
        let Ok(transport) = app.state::<Hosts>().transport(&e.mapping.host) else {
            gone.push(e);
            continue;
        };
        let listening = if expired {
            false
        } else {
            match otter_client::Connection::connect(&transport).await {
                Ok(mut c) => c
                    .host_ports()
                    .await
                    .map(|ports| ports.iter().any(|p| p.port == e.mapping.target_port))
                    .unwrap_or(true),
                Err(_) => true,
            }
        };
        if !listening {
            let _ = forward::cancel(&transport, &e.mapping).await;
            gone.push(e);
        }
    }
    if !gone.is_empty() {
        {
            let forwards = app.state::<Forwards>();
            let mut all = forwards.inner.lock().unwrap();
            all.retain(|c| !gone.iter().any(|g| g.mapping.same_slot(&c.mapping)));
        }
        publish(app);
    }
}
