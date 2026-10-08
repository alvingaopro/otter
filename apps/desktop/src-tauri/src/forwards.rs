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
