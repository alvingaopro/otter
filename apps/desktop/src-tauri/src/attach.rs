//! Interactive attach for the embedded terminal: one `session.attach`
//! connection per open terminal (D-004), bridged to the webview.
//!
//! Output goes to the webview as raw bytes over a Tauri channel (an
//! `ArrayBuffer` in JS, no JSON/base64 on the hot path); input, resize and
//! detach come back as commands. An attach is disposable: closing it never
//! affects the session.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager};
use tokio::sync::mpsc;
use workd_client::Connection;
use workd_protocol::frame::{AttachExitReason, Frame};
use workd_protocol::{SessionAttach, SessionRef};

use crate::hosts::Hosts;

/// How long a detach may take before the connection is dropped anyway.
const DETACH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Attaches {
    next: AtomicU64,
    open: Mutex<HashMap<u64, mpsc::UnboundedSender<Frame>>>,
}

impl Attaches {
    fn send(&self, id: u64, frame: Frame) {
        if let Some(tx) = self.open.lock().unwrap().get(&id) {
            let _ = tx.send(frame);
        }
    }
}

/// How an attach ended, for the terminal's status line.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AttachEvent {
    /// The daemon ended the attach (detached, process exited, session gone).
    #[serde(rename_all = "camelCase")]
    Exit {
        reason: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// The connection closed without an exit (network, daemon restart).
    Closed,
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn attach_open(
    app: AppHandle,
    host: String,
    workspace: String,
    session: String,
    cols: u16,
    rows: u16,
    output: Channel<InvokeResponseBody>,
    events: Channel<AttachEvent>,
) -> Result<u64, String> {
    let transport = app.state::<Hosts>().transport(&host)?;
    let conn = Connection::connect(&transport)
        .await
        .map_err(|e| e.to_string())?;
    let (_ready, mut reader, mut writer) = conn
        .attach(SessionAttach {
            session: SessionRef { workspace, session },
            cols,
            rows,
            term: Some("xterm-256color".into()),
        })
        .await
        .map_err(|e| e.to_string())?;

    let attaches = app.state::<Attaches>();
    let id = attaches.next.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    attaches.open.lock().unwrap().insert(id, tx);

    // Frames to the daemon. Ends (dropping the connection) once the sender is
    // removed from `open`.
    tauri::async_runtime::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if writer.send(&frame).await.is_err() {
                break;
            }
        }
    });

    // Frames from the daemon.
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        let end = loop {
            match reader.next().await {
                Ok(Some(Frame::Data(data))) => {
                    if output.send(InvokeResponseBody::Raw(data)).is_err() {
                        break None;
                    }
                }
                Ok(Some(Frame::Exit(exit))) => {
                    break Some(AttachEvent::Exit {
                        reason: match exit.reason {
                            AttachExitReason::Detached => "detached",
                            AttachExitReason::Exited => "exited",
                            AttachExitReason::Ended => "ended",
                            AttachExitReason::Error => "error",
                        },
                        exit_code: exit.exit_code,
                        message: exit.message,
                    });
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break Some(AttachEvent::Closed),
            }
        };
        if let Some(event) = end {
            let _ = events.send(event);
        }
        app2.state::<Attaches>().open.lock().unwrap().remove(&id);
    });

    Ok(id)
}

#[tauri::command]
pub fn attach_input(app: AppHandle, id: u64, data: String) {
    app.state::<Attaches>()
        .send(id, Frame::Data(data.into_bytes()));
}

#[tauri::command]
pub fn attach_resize(app: AppHandle, id: u64, cols: u16, rows: u16) {
    app.state::<Attaches>()
        .send(id, Frame::Resize { cols, rows });
}

/// Detach (the session keeps running). If the daemon doesn't confirm in time,
/// drop the connection anyway.
#[tauri::command]
pub fn attach_close(app: AppHandle, id: u64) {
    app.state::<Attaches>().send(id, Frame::Detach);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(DETACH_TIMEOUT).await;
        app.state::<Attaches>().open.lock().unwrap().remove(&id);
    });
}
