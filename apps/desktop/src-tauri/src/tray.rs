//! The menu bar item: how many workspaces need you, which ones (each opens
//! the app on it), Open and Quit. Otter keeps running in the menu bar when its
//! window is closed, so the status and notifications keep coming.

use serde::Deserialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};

const TRAY: &str = "otter";

#[derive(Debug, Deserialize)]
pub struct Waiting {
    /// `host/workspace-id`, as the UI selects it.
    key: String,
    label: String,
}

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app, &[])?;
    TrayIconBuilder::with_id(TRAY)
        .icon(tauri::include_image!("icons/32x32.png"))
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
    let header = match waiting.len() {
        0 => "Nothing needs you".to_owned(),
        1 => "1 workspace needs you".to_owned(),
        n => format!("{n} workspaces need you"),
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

/// The UI reports what needs you; the menu bar shows the count and the list.
#[tauri::command]
pub fn tray_update(app: AppHandle, waiting: Vec<Waiting>) -> Result<(), String> {
    let Some(tray) = app.tray_by_id(TRAY) else {
        return Ok(());
    };
    let menu = build_menu(&app, &waiting).map_err(|e| e.to_string())?;
    tray.set_menu(Some(menu)).map_err(|e| e.to_string())?;
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
