//! Workd desktop: a thin client. All state comes from the hosts' daemons
//! through `workd-client`; this crate only connects, flattens state for the
//! UI (`view`) and bridges terminals (`attach`).

mod attach;
mod hosts;
mod view;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(hosts::Hosts::default())
        .manage(attach::Attaches::default())
        .setup(|app| {
            hosts::start(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            hosts::hosts_get,
            hosts::attention_resolve,
            hosts::session_restart,
            attach::attach_open,
            attach::attach_input,
            attach::attach_resize,
            attach::attach_close,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Workd desktop app");
}
