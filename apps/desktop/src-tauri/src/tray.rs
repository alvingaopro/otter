//! The menu bar item: how many workspaces need you, which ones (each opens
//! the app on it), Open and Quit. Otter keeps running in the menu bar when its
//! window is closed, so the status and notifications keep coming.

use serde::Deserialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};

const TRAY: &str = "otter";

/// Gray when nothing needs you, orange when something does.
fn icon(waiting: bool) -> Image<'static> {
    if waiting {
        tauri::include_image!("icons/tray-waiting.png")
    } else {
        tauri::include_image!("icons/tray-idle.png")
    }
}

#[derive(Debug, Deserialize)]
pub struct Waiting {
    /// `host/workspace-id`, as the UI selects it.
    key: String,
    label: String,
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app, &[])?;
    TrayIconBuilder::with_id(TRAY)
        .icon(icon(false))
        .tooltip("Otter")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| {
            let id = event.id().as_ref();
            match id {
                "open" => show(app),
                "quit" => app.exit(0),
                _ => {
                    if let Some(key) = id.strip_prefix("ws:") {
                        show(app);
                        let _ = app.emit("jump", key.to_owned());
                    }
                }
            }
        })
        .build(app)?;
    Ok(())
}

fn build_menu(app: &AppHandle, waiting: &[Waiting]) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    // Workspaces and tasks (D-073).
    let header = match waiting.len() {
        0 => "Nothing needs you".to_owned(),
        1 => "1 thing needs you".to_owned(),
        n => format!("{n} things need you"),
    };
    menu.append(&MenuItem::with_id(
        app,
        "header",
        header,
        false,
        None::<&str>,
    )?)?;
    for w in waiting {
        menu.append(&MenuItem::with_id(
            app,
            format!("ws:{}", w.key),
            &w.label,
            true,
            None::<&str>,
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "open",
        "Open Otter",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        "Quit Otter",
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

/// Bring the main window back (it hides instead of closing).
pub fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Bring the main window forward once it exists, and again a moment later:
/// an app started by another process isn't active yet at the first try, and
/// a window raised before the app has finished starting is left behind
/// (D-171).
pub fn show_when_ready(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        for _ in 0..50 {
            if app.get_webview_window("main").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        for wait in [300, 1200] {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            show(&app);
        }
    });
}

/// The UI reports what needs you; the menu bar shows the count and the list.
#[tauri::command]
pub fn tray_update(app: AppHandle, waiting: Vec<Waiting>) -> Result<(), String> {
    let Some(tray) = app.tray_by_id(TRAY) else {
        return Ok(());
    };
    let menu = build_menu(&app, &waiting).map_err(|e| e.to_string())?;
    tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
    tray.set_icon(Some(icon(!waiting.is_empty())))
        .map_err(|e| e.to_string())?;
    let title = (!waiting.is_empty()).then(|| waiting.len().to_string());
    tray.set_title(title.as_deref())
        .map_err(|e| e.to_string())?;
    let tip = if waiting.is_empty() {
        "Otter".to_owned()
    } else {
        format!("Otter — {} need you", waiting.len())
    };
    tray.set_tooltip(Some(tip)).map_err(|e| e.to_string())?;
    Ok(())
}
