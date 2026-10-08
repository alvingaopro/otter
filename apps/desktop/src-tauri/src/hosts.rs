//! One long-lived task per configured host: connect, take a snapshot, follow
//! the event stream from that snapshot's cursor, and re-snapshot whenever
//! events arrive (coalesced). The UI gets the whole host list on every change.
//!
//! The desktop holds no state of its own: everything shown comes from the
//! hosts, and a lost connection just means "reconnect and snapshot again".

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use workd_client::config::Config;
use workd_client::{ClientError, Connection, Transport};

use crate::view::{HostStatus, HostView, WorkspaceView};

/// Coalesce bursts of events into one snapshot.
const COALESCE: Duration = Duration::from_millis(150);
const RETRY_UNREACHABLE: Duration = Duration::from_secs(5);
const RETRY_INCOMPATIBLE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Hosts {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// In `hosts.toml` order.
    views: Vec<HostView>,
    transports: HashMap<String, Transport>,
    config_error: Option<String>,
    config_dir: String,
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
            .transports
            .get(host)
            .cloned()
            .ok_or_else(|| format!("unknown host `{host}`"))
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut HostView)) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(view) = inner.views.iter_mut().find(|v| v.name == name) {
            f(view);
        }
    }
}

/// Load `hosts.toml` (the one `workctl` uses) and start following every host.
pub fn start(app: &AppHandle) {
    let hosts = app.state::<Hosts>();
    let loaded = Config::dir_from_env().and_then(|dir| {
        let config = Config::load(&dir)?;
        Ok((dir, config))
    });
    {
        let mut inner = hosts.inner.lock().unwrap();
        match loaded {
            Ok((dir, config)) => {
                inner.config_dir = dir.display().to_string();
                for host in &config.hosts {
                    inner
                        .transports
                        .insert(host.name.clone(), config.transport(host));
                    inner.views.push(HostView {
                        name: host.name.clone(),
                        status: HostStatus::Connecting,
                        message: None,
                        workspaces: Vec::new(),
                    });
                }
            }
            Err(e) => inner.config_error = Some(format!("{e:#}")),
        }
    }
    let names: Vec<(String, Transport)> = {
        let inner = hosts.inner.lock().unwrap();
        inner
            .views
            .iter()
            .map(|v| (v.name.clone(), inner.transports[&v.name].clone()))
            .collect()
    };
    for (name, transport) in names {
        tauri::async_runtime::spawn(follow_host(app.clone(), name, transport));
    }
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
    let snapshot = rpc.snapshot().await?;
    show(app, name, &snapshot.workspaces);

    // Events after the snapshot; each burst triggers a fresh snapshot. Reading
    // the stream isn't cancel-safe, so it gets its own task.
    let mut stream = Connection::connect(transport)
        .await?
        .subscribe(Some(snapshot.seq))
        .await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(), ClientError>>(64);
    let reader = tauri::async_runtime::spawn(async move {
        loop {
            let item = match stream.next().await {
                Ok(Some(_)) => Ok(()),
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

fn show(app: &AppHandle, name: &str, workspaces: &[workd_core::Workspace]) {
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

async fn rpc(app: &AppHandle, host: &str) -> Result<Connection, String> {
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
) -> Result<usize, String> {
    rpc(&app, &host)
        .await?
        .attention_resolve(&workspace, session.as_deref())
        .await
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
