//! Pinned workspaces (D-040): an ordered list of `host/workspace-id` keys in
//! `pins.toml`, next to `hosts.toml`. A view preference of this Mac, so no
//! daemon knows about it — pins span hosts and no one host could hold the
//! order. The UI reads the list, edits it and writes it back whole.

use std::path::Path;

use otter_client::config::Config;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Pins {
    #[serde(default)]
    workspaces: Vec<String>,
}

fn load(dir: &Path) -> anyhow::Result<Vec<String>> {
    match std::fs::read_to_string(dir.join("pins.toml")) {
        Ok(text) => Ok(toml::from_str::<Pins>(&text)?.workspaces),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn save(dir: &Path, workspaces: Vec<String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("pins.toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(&Pins { workspaces })?)?;
    std::fs::rename(&tmp, dir.join("pins.toml"))?;
    Ok(())
}

/// The pinned workspaces, top first.
#[tauri::command]
pub fn pins_get() -> Result<Vec<String>, String> {
    let dir = Config::dir_from_env().map_err(|e| e.to_string())?;
    load(&dir).map_err(|e| format!("reading pins.toml: {e:#}"))
}

/// Replace the pinned workspaces, top first.
#[tauri::command]
pub fn pins_set(workspaces: Vec<String>) -> Result<(), String> {
    let dir = Config::dir_from_env().map_err(|e| e.to_string())?;
    save(&dir, workspaces).map_err(|e| format!("saving pins.toml: {e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_in_order_and_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("otter-pins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load(&dir).unwrap().is_empty());
        save(&dir, vec!["mac/ws_b".into(), "code/ws_a".into()]).unwrap();
        assert_eq!(load(&dir).unwrap(), vec!["mac/ws_b", "code/ws_a"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
