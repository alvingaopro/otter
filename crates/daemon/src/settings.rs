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
use otter_protocol::host::{
    CodingSettings, ControllerInfo, ModelInfo, SecretState, Settings, SettingsUpdate,
};
use serde::{Deserialize, Serialize};

use crate::daemon::{Daemon, RpcResult};

use crate::agents::claude::OAUTH_TOKEN;
use crate::providers::PROVIDERS;
use crate::slack::{SLACK_APP_TOKEN, SLACK_BOT_TOKEN};

/// The secrets a client may set (each provider's key, Claude Code's
/// sign-in, the Slack app's tokens), and what they're for.
fn secret_names() -> impl Iterator<Item = (&'static str, String)> {
    PROVIDERS
        .iter()
        .map(|p| {
            (
                p.key,
                format!("Lets Otter use {} (choice `{}`)", p.label, p.id),
            )
        })
        .chain([(
            OAUTH_TOKEN,
            "Signs Claude Code in on this host (a token from `claude setup-token`)".to_owned(),
        )])
        .chain([
            (
                SLACK_BOT_TOKEN,
                "Lets Otter read and answer your Slack messages (the app's bot token)".to_owned(),
            ),
            (
                SLACK_APP_TOKEN,
                "Lets Otter hear from Slack (the app's token with connections:write)".to_owned(),
            ),
        ])
}

/// Controller choices (`None`: automatic), in the order they're offered.
/// `OTTER_BRAIN_V2` in otterd's environment, if set (it overrides the
/// setting).
pub fn otter_tools_from_env() -> Option<String> {
    std::env::var("OTTER_BRAIN_V2")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// `OTTER_TOOL_LEVELS` in otterd's environment: `name=level` pairs,
/// comma-separated, over the settings (D-191). Read once.
pub fn tool_levels_from_env() -> &'static BTreeMap<String, otter_core::tools::ToolLevel> {
    static LEVELS: std::sync::OnceLock<BTreeMap<String, otter_core::tools::ToolLevel>> =
        std::sync::OnceLock::new();
    LEVELS.get_or_init(|| {
        std::env::var("OTTER_TOOL_LEVELS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|pair| {
                let (name, level) = pair.split_once('=')?;
                let level = otter_core::tools::ToolLevel::parse(level.trim())?;
                Some((name.trim().to_owned(), level))
            })
            .collect()
    })
}

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
    c.push(fixed("off", "Off: tasks don't run on their own", false));
    c
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct Stored {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    controller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coding_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coding_model: Option<String>,
    /// Otter answers with its tools (D-089).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    otter_tools: Option<bool>,
    /// A task's own workspace goes once its pull request is merged (D-189).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delete_merged_workspaces: Option<bool>,
    /// Levels of Otter's tools set here (D-191): only those not at their
    /// default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tool_levels: BTreeMap<String, otter_core::tools::ToolLevel>,
    /// The Slack members whose direct messages are the developer's (D-198).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    slack_users: Vec<String>,
}

#[derive(Debug, Default)]
pub struct HostSettings {
    dir: PathBuf,
    stored: Stored,
    secrets: BTreeMap<String, String>,
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

/// Write atomically, readable by this user only.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
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

    /// The coding agent's backend: `OTTER_CODING_BACKEND`, else the setting,
    /// else `sdk` (D-124).
    pub fn coding_backend(&self) -> String {
        std::env::var("OTTER_CODING_BACKEND")
            .ok()
            .filter(|b| !b.is_empty())
            .or_else(|| self.stored.coding_backend.clone())
            .unwrap_or_else(|| "sdk".into())
    }

    /// Whether Otter answers with its tools (D-089): `OTTER_BRAIN_V2` (`1`
    /// on, `0` off), else the setting; on by default (D-096).
    pub fn otter_tools(&self) -> bool {
        match otter_tools_from_env().as_deref() {
            Some("1") => true,
            Some(_) => false,
            None => self.stored.otter_tools.unwrap_or(true),
        }
    }

    /// Whether a task's own workspace is deleted once its pull request is
    /// merged (D-189); on by default.
    pub fn delete_merged_workspaces(&self) -> bool {
        self.stored.delete_merged_workspaces.unwrap_or(true)
    }

    /// A tool's level on this host (D-191): `OTTER_TOOL_LEVELS` in
    /// otterd's environment (`name=level,…`; operators, tests), else set
    /// here, else its default.
    pub fn tool_level(&self, name: &str) -> otter_core::tools::ToolLevel {
        tool_levels_from_env()
            .get(name)
            .copied()
            .or_else(|| self.stored.tool_levels.get(name).copied())
            .or_else(|| crate::levels::entry(name).map(|e| e.default))
            .unwrap_or_default()
    }

    /// The Slack members whose direct messages are the developer's (D-198).
    pub fn slack_users(&self) -> &[String] {
        &self.stored.slack_users
    }

    /// The coding agent's model (`None`: the provider's default).
    pub fn coding_model(&self) -> Option<String> {
        self.stored.coding_model.clone()
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
            coding: CodingSettings {
                backend: Some(self.coding_backend()),
                model: self.stored.coding_model.clone(),
            },
            otter_tools: Some(self.otter_tools()),
            otter_tools_from_env: otter_tools_from_env(),
            otter_answers: None,
            delete_merged_workspaces: Some(self.delete_merged_workspaces()),
            git: None,
            tools: crate::levels::view(|n| self.tool_level(n)),
            // How it is connected is the daemon's (`settings_get`).
            slack: None,
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
        if let Some(coding) = u.coding {
            if let Some(b) = coding.backend {
                let b = b.trim().to_owned();
                if !b.is_empty() && !crate::runtime::BACKENDS.contains(&b.as_str()) {
                    return Err(format!(
                        "unknown coding backend `{b}` ({})",
                        crate::runtime::BACKENDS.join(", ")
                    ));
                }
                stored.coding_backend = Some(b).filter(|b| !b.is_empty());
            }
            if let Some(m) = coding.model {
                let m = m.trim().to_owned();
                if m.len() > 200 || m.chars().any(char::is_control) {
                    return Err("that isn't a model name".into());
                }
                stored.coding_model = Some(m).filter(|m| !m.is_empty());
            }
        }
        if let Some(on) = u.otter_tools {
            stored.otter_tools = Some(on);
        }
        if let Some(on) = u.delete_merged_workspaces {
            stored.delete_merged_workspaces = Some(on);
        }
        for (name, level) in u.tool_levels {
            let Some(e) = crate::levels::entry(&name) else {
                return Err(format!("Otter has no tool `{name}`"));
            };
            match level.filter(|l| *l != e.default) {
                Some(l) => {
                    stored.tool_levels.insert(name, l);
                }
                None => {
                    stored.tool_levels.remove(&name);
                }
            }
        }
        if let Some(slack) = u.slack
            && let Some(users) = slack.users
        {
            stored.slack_users = crate::slack::members(&users)?;
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
                // What `claude setup-token` prints; an API key goes elsewhere.
                Some(v) if name == OAUTH_TOKEN && !v.starts_with("sk-ant-oat") => {
                    return Err(format!(
                        "{name} is the token `claude setup-token` prints (sk-ant-oat01-…)"
                    ));
                }
                // The Slack app's two tokens are easy to swap (D-198).
                Some(v) if name == SLACK_BOT_TOKEN && !v.starts_with("xoxb-") => {
                    return Err(format!(
                        "{name} is the app's Bot User OAuth Token (xoxb-…), under Install App"
                    ));
                }
                Some(v) if name == SLACK_APP_TOKEN && !v.starts_with("xapp-") => {
                    return Err(format!(
                        "{name} is an app-level token (xapp-…) with connections:write, under Basic Information"
                    ));
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
    /// The agent `provider`'s secrets set here, into the environment its
    /// process starts with (D-142).
    pub(crate) fn agent_secrets(&self, provider: &str, env: &mut crate::env::EnvMap) {
        let Some(p) = crate::agents::provider(provider) else {
            return;
        };
        let s = self.settings.read().unwrap();
        for name in p.secrets() {
            if let Some(v) = s.secret(name) {
                env.insert((*name).to_owned(), v);
            }
        }
    }

    pub(crate) fn settings_get(&self) -> Settings {
        let mut view = self.settings.read().unwrap().view();
        view.slack = Some(self.slack_view());
        view.git = Some(self.git_account.view());
        let brain = crate::brain::select(self.environments.base(), &self.brain_choice());
        view.active = Some(brain.name().to_owned());
        view.otter_answers = Some(match self.how_otter_answers() {
            Ok(how) | Err(how) => how,
        });
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

    pub(crate) fn settings_set(&self, mut u: SettingsUpdate) -> RpcResult<Settings> {
        // Slack connects again with what is set now (D-198).
        let slack = u.slack.is_some()
            || u.secrets
                .keys()
                .any(|k| k == SLACK_BOT_TOKEN || k == SLACK_APP_TOKEN);
        if let Some(id) = u.git.take() {
            self.git_account
                .set_identity(id)
                .map_err(RpcError::invalid)?;
        }
        let mut s = self.settings.write().unwrap();
        s.update(u).map_err(RpcError::invalid)?;
        tracing::info!("settings changed");
        drop(s);
        // A new controller or key: let it look again.
        self.task_wake.notify_one();
        if slack {
            self.slack.restart();
        }
        // As `settings.get` has it: what runs now, and how Otter answers.
        Ok(self.settings_get())
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
            coding: None,
            otter_tools: None,
            delete_merged_workspaces: None,
            git: None,
            tool_levels: Default::default(),
            slack: None,
        }
    }

    /// Otter's tools (D-089): on by default (D-096), off when turned off,
    /// and kept. (Where `OTTER_BRAIN_V2` is set, it decides: nothing to
    /// check here.)
    #[test]
    fn otters_tools_are_on_until_turned_off_and_stay_off() {
        if otter_tools_from_env().is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut s = HostSettings::load(dir.path()).unwrap();
        assert!(s.otter_tools());
        assert_eq!(s.view().otter_tools, Some(true));
        s.update(SettingsUpdate {
            otter_tools: Some(false),
            ..Default::default()
        })
        .unwrap();
        assert!(!s.otter_tools());
        // Another update leaves it as it is.
        s.update(update(Some("claude"), None)).unwrap();
        let again = HostSettings::load(dir.path()).unwrap();
        assert!(!again.otter_tools());
        assert_eq!(again.view().otter_tools, Some(false));
    }

    /// Deleting a task's workspace once its work is merged (D-189): on by
    /// default, off when turned off, and kept.
    #[test]
    fn merged_workspaces_go_until_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = HostSettings::load(dir.path()).unwrap();
        assert!(s.delete_merged_workspaces());
        assert_eq!(s.view().delete_merged_workspaces, Some(true));
        s.update(SettingsUpdate {
            delete_merged_workspaces: Some(false),
            ..Default::default()
        })
        .unwrap();
        s.update(update(Some("claude"), None)).unwrap();
        let again = HostSettings::load(dir.path()).unwrap();
        assert!(!again.delete_merged_workspaces());
        assert_eq!(again.view().delete_merged_workspaces, Some(false));
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

    /// Claude Code's sign-in (D-142): only what `claude setup-token`
    /// prints, kept like a key, listed with what it's for.
    #[test]
    fn claude_codes_token_is_a_secret_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = HostSettings::load(dir.path()).unwrap();
        let token = |v: &str| SettingsUpdate {
            secrets: BTreeMap::from([(OAUTH_TOKEN.to_owned(), Some(v.to_owned()))]),
            ..Default::default()
        };
        let err = s.update(token("sk-ant-api03-abc")).unwrap_err();
        assert!(err.contains("claude setup-token"), "{err}");
        s.update(token(" sk-ant-oat01-abc ")).unwrap();
        assert_eq!(s.secret(OAUTH_TOKEN).as_deref(), Some("sk-ant-oat01-abc"));
        let view = s.view();
        let k = view.secrets.iter().find(|k| k.name == OAUTH_TOKEN).unwrap();
        assert!(k.set);
        assert!(!serde_json::to_string(&view).unwrap().contains("oat01-abc"));
        // Not a model provider: no controller of its own.
        assert!(
            !view
                .controllers
                .iter()
                .any(|c| c.secret.as_deref() == Some(OAUTH_TOKEN))
        );
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
