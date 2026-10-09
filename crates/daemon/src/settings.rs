//! Host settings (D-048): how this host's Control Agent thinks, and the API
//! keys it may use, set from a client instead of the login environment.
//!
//! ```text
//! state/settings.json   { "controller": "openrouter", "model": "…" }
//! state/secrets.json    { "OPENROUTER_API_KEY": "…", … }   (0600)
//! ```
//!
//! Secrets are write-only over the protocol: a client can set or clear one
//! and see whether it is set, never read it back. They are never logged,
//! never in `events.jsonl`, never on a command line. Only known names are
//! accepted. An explicit `OTTER_CONTROLLER` / `OTTER_CONTROLLER_MODEL` in
//! otterd's environment still wins (operators, tests); a key in the
//! environment is used when none is set here.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use otter_protocol::RpcError;
use otter_protocol::host::{ControllerInfo, ModelInfo, SecretState, Settings, SettingsUpdate};
use serde::{Deserialize, Serialize};

use crate::daemon::{Daemon, RpcResult};

use crate::providers::PROVIDERS;

/// The secrets a client may set (each provider's key), and what they're for.
fn secret_names() -> impl Iterator<Item = (&'static str, String)> {
    PROVIDERS.iter().map(|p| {
        (
            p.key,
            format!("Lets the Lead use {} (choice `{}`)", p.label, p.id),
        )
    })
}

/// Controller choices (`None`: automatic), in the order they're offered.
pub fn controllers() -> Vec<ControllerInfo> {
    let fixed = |id: &str, label: &str, models: bool| ControllerInfo {
        id: id.into(),
        label: label.into(),
        secret: None,
        models,
        default_model: None,
    };
    let mut c: Vec<ControllerInfo> = PROVIDERS
        .iter()
        .map(|p| ControllerInfo {
            id: p.id.into(),
            label: p.label.into(),
            secret: Some(p.key.into()),
            models: true,
            default_model: p.default_model.map(String::from),
        })
        .collect();
    c.push(fixed(
        "claude",
        "Claude Code (its own sign-in on the host)",
        true,
    ));
    c.push(fixed(
        "rules",
        "No model: every decision goes to you",
        false,
    ));
    c.push(fixed("off", "Off: features don't run on their own", false));
    c
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct Stored {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    controller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Default)]
pub struct HostSettings {
    dir: PathBuf,
    stored: Stored,
    secrets: BTreeMap<String, String>,
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

/// Write atomically, readable by this user only.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl HostSettings {
    /// No settings yet (or unreadable ones): everything automatic.
    pub fn empty(state_dir: &Path) -> HostSettings {
        HostSettings {
            dir: state_dir.to_path_buf(),
            ..Default::default()
        }
    }

    pub fn load(state_dir: &Path) -> Result<HostSettings> {
        Ok(HostSettings {
            dir: state_dir.to_path_buf(),
            stored: read_json(&state_dir.join("settings.json"))?,
            secrets: read_json(&state_dir.join("secrets.json"))?,
        })
    }

    /// The controller to use: the environment, else the setting.
    pub fn controller(&self) -> Option<String> {
        std::env::var("OTTER_CONTROLLER")
            .ok()
            .filter(|c| !c.is_empty())
            .or_else(|| self.stored.controller.clone())
    }

    /// The model for the controller: the environment, else the setting.
    pub fn model(&self) -> Option<String> {
        std::env::var("OTTER_CONTROLLER_MODEL")
            .ok()
            .filter(|m| !m.is_empty())
            .or_else(|| self.stored.model.clone())
    }

    /// A secret set here.
    pub fn secret(&self, name: &str) -> Option<String> {
        self.secrets.get(name).cloned().filter(|s| !s.is_empty())
    }

    /// What a client may see: choices, and whether each secret is set.
    pub fn view(&self) -> Settings {
        Settings {
            controller: self.stored.controller.clone(),
            model: self.stored.model.clone(),
            controller_from_env: std::env::var("OTTER_CONTROLLER")
                .ok()
                .filter(|c| !c.is_empty()),
            secrets: secret_names()
                .map(|(name, purpose)| SecretState {
                    name: name.into(),
                    purpose,
                    set: self.secret(name).is_some(),
                })
                .collect(),
            controllers: controllers(),
            active: None,
        }
    }

    pub fn update(&mut self, u: SettingsUpdate) -> Result<(), String> {
        let mut stored = self.stored.clone();
        if let Some(c) = u.controller {
            let c = c.trim().to_owned();
            let known = controllers();
            if !c.is_empty() && !known.iter().any(|k| k.id == c) {
                let ids: Vec<&str> = known.iter().map(|k| k.id.as_str()).collect();
                return Err(format!(
                    "unknown controller `{c}` ({} or automatic)",
                    ids.join(", ")
                ));
            }
            stored.controller = Some(c).filter(|c| !c.is_empty());
        }
        if let Some(m) = u.model {
            let m = m.trim().to_owned();
            if m.len() > 200 || m.chars().any(char::is_control) {
                return Err("that isn't a model name".into());
            }
            stored.model = Some(m).filter(|m| !m.is_empty());
        }
        let mut secrets = self.secrets.clone();
        for (name, value) in u.secrets {
            if !secret_names().any(|(n, _)| n == name) {
                return Err(format!("unknown secret `{name}`"));
            }
            match value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) {
                Some(v) if v.len() > 4096 || v.chars().any(|c| c.is_control() || c == '"') => {
                    return Err(format!("{name} doesn't look like a key"));
                }
                Some(v) => {
                    secrets.insert(name, v);
                }
                None => {
                    secrets.remove(&name);
                }
            }
        }
        let save = || -> Result<()> {
            write_private(
                &self.dir.join("settings.json"),
                &serde_json::to_vec_pretty(&stored)?,
            )?;
            write_private(
                &self.dir.join("secrets.json"),
                &serde_json::to_vec_pretty(&secrets)?,
            )?;
            Ok(())
        };
        save().map_err(|e| format!("saving settings: {e:#}"))?;
        self.stored = stored;
        self.secrets = secrets;
        Ok(())
    }
}

impl Daemon {
    pub(crate) fn settings_get(&self) -> Settings {
        let mut view = self.settings.read().unwrap().view();
        let brain = crate::brain::select(self.environments.base(), &self.brain_choice());
        view.active = Some(brain.name().to_owned());
        view
    }

    /// The models a controller offers: its provider's list, asked with the
    /// key set here (or in otterd's environment).
    pub(crate) async fn settings_models(&self, controller: &str) -> RpcResult<Vec<ModelInfo>> {
        if controller == "claude" {
            return Ok(crate::providers::CLAUDE_CODE_MODELS
                .iter()
                .map(|(id, name)| ModelInfo {
                    id: (*id).into(),
                    name: Some((*name).into()),
                })
                .collect());
        }
        let p = crate::providers::get(controller)
            .ok_or_else(|| RpcError::invalid(format!("`{controller}` has no models to list")))?;
        let env = self.environments.base().clone();
        let key = self
            .settings
            .read()
            .unwrap()
            .secret(p.key)
            .or_else(|| p.env_key(&env));
        p.models(&env, key.as_deref())
            .await
            .map_err(|e| RpcError::conflict(format!("{e:#}")))
    }

    pub(crate) fn settings_set(&self, u: SettingsUpdate) -> RpcResult<Settings> {
        let mut s = self.settings.write().unwrap();
        s.update(u).map_err(RpcError::invalid)?;
        tracing::info!("settings changed");
        let view = s.view();
        drop(s);
        // A new controller or key: let it look again.
        self.feature_wake.notify_one();
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn update(controller: Option<&str>, key: Option<Option<&str>>) -> SettingsUpdate {
        SettingsUpdate {
            controller: controller.map(String::from),
            model: None,
            secrets: key
                .map(|k| BTreeMap::from([("OPENROUTER_API_KEY".to_owned(), k.map(String::from))]))
                .unwrap_or_default(),
        }
    }

    #[test]
    fn keys_are_kept_private_and_only_their_presence_shows() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = HostSettings::load(dir.path()).unwrap();
        assert!(!s.view().secrets[0].set);
        assert_eq!(view_names(&s)[0], "OPENROUTER_API_KEY");
        s.update(update(Some("openrouter"), Some(Some("sk-or-abc"))))
            .unwrap();
        let view = s.view();
        assert!(view.secrets[0].set);
        assert_eq!(view.controller.as_deref(), Some("openrouter"));
        // The view never carries the value.
        assert!(!serde_json::to_string(&view).unwrap().contains("sk-or-abc"));
        // Stored for this user only, and survives a reload.
        let meta = std::fs::metadata(dir.path().join("secrets.json")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let again = HostSettings::load(dir.path()).unwrap();
        assert_eq!(
            again.secret("OPENROUTER_API_KEY").as_deref(),
            Some("sk-or-abc")
        );
        // Cleared with null; back to automatic with "".
        s.update(update(Some(""), Some(None))).unwrap();
        assert!(s.secret("OPENROUTER_API_KEY").is_none());
        assert_eq!(s.view().controller, None);
    }

    fn view_names(s: &HostSettings) -> Vec<String> {
        s.view().secrets.into_iter().map(|x| x.name).collect()
    }

    #[test]
    fn every_provider_is_a_choice_with_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = HostSettings::load(dir.path()).unwrap();
        let names = view_names(&s);
        for p in PROVIDERS {
            assert!(names.iter().any(|n| n == p.key), "{}", p.key);
            s.update(update(Some(p.id), None)).unwrap();
        }
        let mut u = update(Some("anthropic"), None);
        u.secrets
            .insert("ANTHROPIC_API_KEY".into(), Some("sk-ant-x".into()));
        s.update(u).unwrap();
        assert_eq!(s.secret("ANTHROPIC_API_KEY").as_deref(), Some("sk-ant-x"));
        let c = controllers();
        assert!(c.iter().any(|c| c.id == "claude" && c.models));
        assert!(c.iter().any(|c| c.id == "off" && !c.models));
    }

    #[test]
    fn only_known_names_and_choices() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = HostSettings::load(dir.path()).unwrap();
        assert!(s.update(update(Some("gpt"), None)).is_err());
        let mut u = update(None, None);
        u.secrets
            .insert("AWS_SECRET_ACCESS_KEY".into(), Some("x".into()));
        assert!(s.update(u).is_err());
        assert!(s.update(update(None, Some(Some("bad\"key")))).is_err());
        // Nothing was half-applied.
        assert_eq!(s.view().controller, None);
    }
}
