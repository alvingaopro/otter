//! Otter desktop: a thin client. All state comes from the hosts' daemons
//! through `otter-client`; this crate only connects, flattens state for the
//! UI (`view`), bridges terminals (`attach`), moves files (`files`), keeps
//! port mappings (`forwards`) and the menu bar item (`tray`).

mod attach;
mod features;
mod files;
mod forwards;
mod hosts;
mod pins;
mod settings;
mod tray;
mod view;

use tauri::Emitter;
use tauri::menu::{Menu, MenuItem, MenuItemKind};

const INSTALL_CLI: &str = "install-cli";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(hosts::Hosts::default())
        .manage(attach::Attaches::default())
        .manage(forwards::Forwards::default())
        .menu(|app| {
            // The standard menu, plus "Install Command Line Tools…" in the app
            // menu (the UI handles it).
            let menu = Menu::default(app)?;
            if let Some(MenuItemKind::Submenu(app_menu)) = menu.items()?.into_iter().next() {
                let item = MenuItem::with_id(
                    app,
                    INSTALL_CLI,
                    "Install Command Line Tools…",
                    true,
                    None::<&str>,
                )?;
                app_menu.insert(&item, 2)?;
            }
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            if event.id() == INSTALL_CLI {
                let _ = app.emit("menu", INSTALL_CLI);
            }
        })
        .setup(|app| {
            hosts::start(app.handle());
            forwards::start(app.handle());
            tray::setup(app.handle())?;
            Ok(())
        })
        // Closing the window keeps Otter in the menu bar (status and
        // notifications); Quit is in the menu bar item and the app menu.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            hosts::hosts_get,
            hosts::host_add,
            hosts::host_remove,
            hosts::host_install,
            hosts::install_cli,
            hosts::cli_status,
            hosts::host_metrics,
            hosts::host_ports,
            hosts::host_history,
            forwards::forwards_get,
            forwards::forward_add,
            forwards::forward_remove,
            forwards::forward_pin,
            features::features_list,
            features::feature_create,
            features::feature_send,
            features::feature_act,
            features::feature_events,
            features::feature_artifact,
            settings::settings_get,
            settings::settings_set,
            pins::pins_get,
            pins::pins_set,
            hosts::workspace_create,
            hosts::workspace_delete,
            hosts::workspace_prepare,
            hosts::workspace_archive,
            hosts::workspace_unarchive,
            hosts::workspace_set_brief,
            hosts::workspace_events,
            hosts::session_create,
            hosts::session_stop,
            hosts::session_delete,
            hosts::attention_resolve,
            hosts::session_restart,
            attach::attach_open,
            attach::attach_input,
            attach::attach_resize,
            attach::attach_close,
            files::fs_list,
            files::file_download,
            files::file_upload,
            files::paste_image,
            tray::tray_update,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Otter desktop app")
        .run(|app, event| {
            // Clicking the Dock icon brings the hidden window back.
            if let tauri::RunEvent::Reopen { .. } = event {
                tray::show(app);
            }
        });
}
