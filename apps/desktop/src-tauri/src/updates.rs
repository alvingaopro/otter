//! Updating the app where it is (D-125). The latest release's `latest.json`
//! names a signed `.app.tar.gz` per architecture; a check runs a little after
//! launch and then hourly, and "Check for Updates…" (in the app menu and the
//! settings menu) runs one on demand. What it finds is offered at the bottom of the sidebar
//! (`update`); installing is the developer's click, then the app restarts
//! into the new version.
//!
//! Only an installed copy updates ([`hosts::installed_app`]): not a
//! development build, a disk image or a translocated copy, which can't be
//! replaced. The bundle is swapped in place, so the links in `~/.local/bin`
//! (D-085) lead to the new version's `otter` and `otterd` too. This Mac's
//! running otterd is left alone: restarting it stays the developer's button.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::hosts;

/// The first check lets the app settle; then one an hour, since a release
/// ships with every merge (D-019).
const FIRST_CHECK: Duration = Duration::from_secs(10);
const EVERY: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
pub struct Updates {
    /// The newer release the last check found.
    offer: Mutex<Option<Update>>,
    installing: AtomicBool,
}

/// How far an install has got (`update-progress`).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    downloaded: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
}

pub fn start(app: &AppHandle) {
    if hosts::installed_app().is_none() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            if let Err(e) = check(&app).await {
                eprintln!("checking for an update: {e}");
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// Ask the latest release whether it's newer; keep and announce the answer.
async fn check(app: &AppHandle) -> Result<Option<String>, String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let version = update.as_ref().map(|u| u.version.clone());
    *app.state::<Updates>().offer.lock().unwrap() = update;
    let _ = app.emit("update", &version);
    Ok(version)
}

/// The version on offer, if the last check found one.
#[tauri::command]
pub fn update_get(app: AppHandle) -> Option<String> {
    let offer = app.state::<Updates>().offer.lock().unwrap().clone();
    offer.map(|u| u.version)
}

/// Download the version on offer, check its signature, put it in place of
/// this app and restart into it. Progress and failures are announced
/// (`update-progress`, `update-failed`), since the app menu can start one
/// too; asked again while one is under way, it leaves that one to finish.
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<(), String> {
    let updates = app.state::<Updates>();
    let Some(update) = updates.offer.lock().unwrap().clone() else {
        let message = "there's no update to install".to_owned();
        let _ = app.emit("update-failed", &message);
        return Err(message);
    };
    if updates.installing.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let _ = app.emit(
        "update-progress",
        Progress {
            downloaded: 0,
            total: None,
        },
    );
    let (mut downloaded, mut shown) = (0u64, None);
    let installed = update
        .download_and_install(
            |chunk, total| {
                downloaded += chunk as u64;
                // A step per percent (or per megabyte, size unknown).
                let step = match total {
                    Some(total) if total > 0 => downloaded * 100 / total,
                    _ => downloaded >> 20,
                };
                if shown != Some(step) {
                    shown = Some(step);
                    let _ = app.emit("update-progress", Progress { downloaded, total });
                }
            },
            || {},
        )
        .await;
    updates.installing.store(false, Ordering::SeqCst);
    if let Err(e) = installed {
        let message = format!("couldn't update to v{}: {e}", update.version);
        let _ = app.emit("update-failed", &message);
        return Err(message);
    }
    app.restart()
}

/// The app menu's "Check for Updates…": says what it found, and offers to
/// install a newer version.
pub fn check_now(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let current = app.package_info().version.to_string();
        if hosts::installed_app().is_none() {
            say(
                &app,
                "This copy of Otter can't update itself",
                "Only Otter installed in a folder such as Applications updates — not a development build, nor one opened from its disk image.",
            );
            return;
        }
        match check(&app).await {
            Ok(None) => say(
                &app,
                "Otter is up to date",
                &format!("v{current} is the latest release."),
            ),
            Ok(Some(version)) => {
                let install = app.clone();
                app.dialog()
                    .message(format!(
                        "You have v{current}. Otter restarts into the new version; your sessions keep running."
                    ))
                    .title(format!("Otter v{version} is available"))
                    .buttons(MessageDialogButtons::OkCancelCustom(
                        "Update and Restart".into(),
                        "Later".into(),
                    ))
                    .show(move |yes| {
                        if yes {
                            tauri::async_runtime::spawn(async move {
                                if let Err(e) = update_install(install.clone()).await {
                                    say(&install, "Couldn't update Otter", &e);
                                }
                            });
                        }
                    });
            }
            Err(e) => {
                app.dialog()
                    .message(e)
                    .title("Couldn't check for updates")
                    .kind(MessageDialogKind::Warning)
                    .show(|_| {});
            }
        }
    });
}

/// "Check for Updates…" from the settings menu: the app menu's check.
#[tauri::command]
pub fn update_check(app: AppHandle) {
    check_now(&app);
}

fn say(app: &AppHandle, title: &str, message: &str) {
    app.dialog().message(message).title(title).show(|_| {});
}
