//! Otter desktop: a thin client. All state comes from the hosts' daemons
//! through `otter-client`; this crate only connects, flattens state for the
//! UI (`view`) and bridges terminals (`attach`).

mod attach;
mod forwards;
mod hosts;
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
            Ok(())
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
            forwards::forwards_get,
            forwards::forward_add,
            forwards::forward_remove,
            forwards::forward_pin,
            hosts::workspace_create,
            hosts::workspace_delete,
            hosts::workspace_prepare,
            hosts::session_create,
            hosts::session_stop,
            hosts::session_delete,
            hosts::attention_resolve,
            hosts::session_restart,
            attach::attach_open,
            attach::attach_input,
            attach::attach_resize,
            attach::attach_close,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Otter desktop app");
}
