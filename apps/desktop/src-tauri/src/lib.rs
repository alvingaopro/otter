//! Otter desktop: a thin client. All state comes from the hosts' daemons
//! through `otter-client`; this crate only connects, flattens state for the
//! UI (`view`), bridges terminals (`attach`), moves files (`files`), keeps
//! port mappings (`forwards`) and the menu bar item (`tray`), updates
//! itself (`updates`) and says how its own process does (`perf`).

mod attach;
mod files;
mod forwards;
mod git;
mod hosts;
mod perf;
mod pins;
mod settings;
mod sign_in;
mod tasks;
mod tray;
mod triggers;
mod updates;
mod view;

use tauri::Emitter;
use tauri::menu::{Menu, MenuItem, MenuItemKind};

const INSTALL_CLI: &str = "install-cli";
const SETTINGS: &str = "settings";
const CHECK_UPDATES: &str = "check-updates";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Holding a key repeats it, as vim's hjkl need (D-116), instead of
    // WebKit's press-and-hold accent menu: set in this app's own defaults,
    // which outrank the system-wide setting, before any window opens.
    #[cfg(target_os = "macos")]
    objc2_foundation::NSUserDefaults::standardUserDefaults().setBool_forKey(
        false,
        objc2_foundation::ns_string!("ApplePressAndHoldEnabled"),
    );
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(hosts::Hosts::default())
        .manage(attach::Attaches::default())
        .manage(forwards::Forwards::default())
        .manage(updates::Updates::default())
        .menu(|app| {
            // The standard menu, plus "Check for Updates…", "Settings…" (⌘,)
            // and "Install Command Line Tools…" in the app menu (the UI
            // handles the last two).
            let menu = Menu::default(app)?;
            if let Some(MenuItemKind::Submenu(app_menu)) = menu.items()?.into_iter().next() {
                let check = MenuItem::with_id(
                    app,
                    CHECK_UPDATES,
                    "Check for Updates…",
                    true,
                    None::<&str>,
                )?;
                app_menu.insert(&check, 1)?;
                let settings =
                    MenuItem::with_id(app, SETTINGS, "Settings…", true, Some("CmdOrCtrl+,"))?;
                app_menu.insert(&settings, 3)?;
                let item = MenuItem::with_id(
                    app,
                    INSTALL_CLI,
                    "Install Command Line Tools…",
                    true,
                    None::<&str>,
                )?;
                app_menu.insert(&item, 4)?;
            }
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            if event.id() == INSTALL_CLI {
                let _ = app.emit("menu", INSTALL_CLI);
            } else if event.id() == SETTINGS {
                let _ = app.emit("menu", SETTINGS);
            } else if event.id() == CHECK_UPDATES {
                updates::check_now(app);
            }
        })
        .setup(|app| {
            // Keep the command-line tools at this app's version (D-085): off
            // the main thread, since it may ask a copy its version (D-125).
            std::thread::spawn(|| {
                if let Err(e) = hosts::link_cli(false) {
                    eprintln!("{e}");
                }
            });
            hosts::start(app.handle());
            forwards::start(app.handle());
            updates::start(app.handle());
            tray::setup(app.handle())?;
            updates::after_update(app.handle());
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
            hosts::host_install_worker,
            git::git_identity,
            git::git_identity_set,
            git::git_view,
            git::git_key_create,
            git::git_key_add,
            git::git_check,
            git::github_login,
            git::github_logout,
            hosts::install_cli,
            hosts::cli_status,
            updates::update_get,
            updates::update_install,
            updates::update_check,
            perf::process_stats,
            hosts::host_metrics,
            hosts::host_ports,
            hosts::host_processes,
            hosts::host_process_stop,
            hosts::host_history,
            forwards::forwards_get,
            forwards::forward_add,
            forwards::forward_remove,
            forwards::forward_pin,
            tasks::tasks_list,
            triggers::triggers_list,
            triggers::trigger_update,
            triggers::trigger_run,
            triggers::trigger_delete,
            tasks::task_create,
            tasks::memory_get,
            tasks::memory_set,
            tasks::task_send,
            tasks::task_act,
            tasks::task_events,
            tasks::task_artifact,
            tasks::task_delete,
            tasks::conversations,
            tasks::runtime_capabilities,
            tasks::runtime_test,
            settings::settings_get,
            settings::settings_set,
            settings::settings_models,
            pins::pins_get,
            pins::pins_set,
            hosts::workspace_create,
            hosts::workspace_delete,
            hosts::workspace_prepare,
            hosts::workspace_archive,
            hosts::workspace_unarchive,
            hosts::workspace_set_brief,
            hosts::workspace_rename,
            hosts::workspace_changes,
            hosts::workspace_publish,
            hosts::workspace_diff,
            hosts::workspace_ports,
            hosts::workspace_events,
            hosts::session_create,
            hosts::session_stop,
            hosts::session_delete,
            hosts::session_rename,
            hosts::session_move,
            hosts::session_history,
            hosts::attention_resolve,
            hosts::session_restart,
            attach::attach_open,
            attach::attach_input,
            attach::attach_resize,
            attach::attach_view,
            attach::attach_history,
            sign_in::sign_in_sites,
            sign_in::sign_in_allow,
            sign_in::sign_in_forget,
            sign_in::sign_in_open,
            attach::attach_close,
            files::fs_list,
            files::fs_remove,
            files::fs_rename,
            files::file_preview,
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
