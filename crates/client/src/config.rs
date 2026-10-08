//! Control-plane configuration: the host registry (design §25).
//!
//! Stored in `$OTTER_CONFIG_DIR/hosts.toml` (default
//! `$XDG_CONFIG_HOME/otter` or `~/.config/otter`). Before the rename (D-024)
//! this was `~/.config/workd` (or `$WORKCTL_CONFIG_DIR`); it is still read,
//! and copied over on first use.

use std::path::{Path, PathBuf};

use crate::Transport;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Default location of `otterd` on remote hosts. Interpreted by the remote
/// shell, so `~` expands there.
pub const DEFAULT_REMOTE_OTTERD: &str = "~/.local/bin/otterd";

/// The default `otterd_path` before the rename (D-024).
const LEGACY_REMOTE_WORKD: &str = "~/.local/bin/workd";

/// First use after the rename: copy `~/.config/workd/hosts.toml` into the
/// (default) new directory. Explicit directories are left alone.
fn migrate_legacy_dir(dir: &Path, path: &Path) {
    if path.exists() || std::env::var_os("XDG_CONFIG_HOME").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let config = PathBuf::from(home).join(".config");
    if dir != config.join("otter") {
        return;
    }
    let legacy = config.join("workd").join("hosts.toml");
    if legacy.exists() && std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::copy(&legacy, path);
    }
}

/// macOS limits Unix socket paths to 104 bytes; `%C` expands to 40 chars.
const MAX_CONTROL_DIR_LEN: usize = 60;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// Host used when a command doesn't name one and several are registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_host: Option<String>,
    #[serde(default, rename = "host")]
    pub hosts: Vec<HostEntry>,
    #[serde(skip)]
    dir: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HostEntry {
    pub name: String,
    #[serde(flatten)]
    pub transport: HostTransport,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum HostTransport {
    Ssh {
        /// Anything `ssh` accepts as a destination, including `~/.ssh/config`
        /// host aliases.
        destination: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ssh_args: Vec<String>,
        #[serde(default = "default_remote_otterd", alias = "workd_path")]
        otterd_path: String,
        /// Override the daemon's state directory on the host.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        home: Option<String>,
    },
    Local {
        #[serde(default, skip_serializing_if = "Option::is_none", alias = "workd_path")]
        otterd_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        home: Option<String>,
    },
}

fn default_remote_otterd() -> String {
    DEFAULT_REMOTE_OTTERD.to_owned()
}

impl HostEntry {
    pub fn describe(&self) -> String {
        match &self.transport {
            HostTransport::Ssh { destination, .. } => format!("ssh {destination}"),
            HostTransport::Local { .. } => "local".to_owned(),
        }
    }
}

impl Config {
    pub fn dir_from_env() -> Result<PathBuf> {
        if let Some(dir) =
            std::env::var_os("OTTER_CONFIG_DIR").or_else(|| std::env::var_os("WORKCTL_CONFIG_DIR"))
        {
            return Ok(PathBuf::from(dir));
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(xdg).join("otter"));
        }
        let home = std::env::var_os("HOME").context("$HOME is not set")?;
        Ok(PathBuf::from(home).join(".config").join("otter"))
    }

    pub fn load(dir: &Path) -> Result<Config> {
        let path = dir.join("hosts.toml");
        migrate_legacy_dir(dir, &path);
        let mut config: Config = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        config.dir = dir.to_path_buf();
        // Hosts registered before the rename point at the old binary name.
        for host in &mut config.hosts {
            if let HostTransport::Ssh { otterd_path, .. } = &mut host.transport
                && otterd_path == LEGACY_REMOTE_WORKD
            {
                *otterd_path = DEFAULT_REMOTE_OTTERD.to_owned();
            }
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.dir.join("hosts.toml");
        let tmp = self.dir.join("hosts.toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Register a host (name rules and uniqueness checked); call `save` after.
    pub fn add_host(&mut self, entry: HostEntry) -> Result<()> {
        otter_core::validate_name("host", &entry.name).map_err(anyhow::Error::msg)?;
        if self.hosts.iter().any(|h| h.name == entry.name) {
            bail!("host `{}` is already registered", entry.name);
        }
        self.hosts.push(entry);
        Ok(())
    }

    /// Forget a host (nothing on it is touched); call `save` after.
    pub fn remove_host(&mut self, name: &str) -> Result<HostEntry> {
        let idx = self
            .hosts
            .iter()
            .position(|h| h.name == name)
            .with_context(|| format!("no host `{name}`"))?;
        if self.default_host.as_deref() == Some(name) {
            self.default_host = None;
        }
        Ok(self.hosts.remove(idx))
    }

    pub fn host(&self, name: &str) -> Result<&HostEntry> {
        self.hosts.iter().find(|h| h.name == name).with_context(|| {
            format!(
                "no host `{name}`; registered: {}",
                self.host_names()
                    .unwrap_or_else(|| "none (see `otter host add`)".into())
            )
        })
    }

    pub fn host_names(&self) -> Option<String> {
        (!self.hosts.is_empty()).then(|| {
            self.hosts
                .iter()
                .map(|h| h.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
    }

    /// The host to use when none was named.
    pub fn default_host(&self) -> Result<&HostEntry> {
        match self.hosts.as_slice() {
            [] => bail!("no hosts registered; add one with `otter host add <name>`"),
            [only] => Ok(only),
            _ => match &self.default_host {
                Some(name) => self.host(name),
                None => bail!(
                    "several hosts are registered ({}); pass --host or set one with `otter host default <name>`",
                    self.host_names().unwrap_or_default()
                ),
            },
        }
    }

    pub fn transport(&self, host: &HostEntry) -> Transport {
        match &host.transport {
            HostTransport::Ssh {
                destination,
                ssh_args,
                otterd_path,
                home,
            } => Transport::Ssh {
                destination: destination.clone(),
                ssh_args: ssh_args.clone(),
                otterd_path: otterd_path.clone(),
                home: home.clone(),
                control_dir: self.control_dir(),
            },
            HostTransport::Local { otterd_path, home } => Transport::Local {
                otterd_path: otterd_path.clone().unwrap_or_else(local_otterd),
                home: home.clone(),
            },
        }
    }

    /// Directory for SSH ControlMaster sockets, if it exists (or can be
    /// created) and is short enough for a socket path.
    fn control_dir(&self) -> Option<PathBuf> {
        let dir = self.dir.join("cm");
        if dir.as_os_str().len() > MAX_CONTROL_DIR_LEN {
            return None;
        }
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .ok()?;
        Some(dir)
    }
}

/// `otterd` next to the running client binary if present, else from `PATH`.
fn local_otterd() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("otterd")))
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "otterd".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_round_trip_through_toml() {
        let config = Config {
            default_host: Some("dev-01".into()),
            hosts: vec![
                HostEntry {
                    name: "dev-01".into(),
                    transport: HostTransport::Ssh {
                        destination: "alvin@dev-01".into(),
                        ssh_args: vec!["-p".into(), "2222".into()],
                        otterd_path: DEFAULT_REMOTE_OTTERD.into(),
                        home: None,
                    },
                },
                HostEntry {
                    name: "here".into(),
                    transport: HostTransport::Local {
                        otterd_path: None,
                        home: Some("/tmp/x".into()),
                    },
                },
            ],
            dir: PathBuf::new(),
        };
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("transport = \"ssh\""), "{text}");
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.hosts, config.hosts);
        assert_eq!(back.default_host, config.default_host);
    }

    #[test]
    fn hosts_from_before_the_rename_still_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hosts.toml"),
            r#"
            [[host]]
            name = "code"
            transport = "ssh"
            destination = "code"
            workd_path = "~/.local/bin/workd"

            [[host]]
            name = "custom"
            transport = "ssh"
            destination = "x"
            workd_path = "/opt/workd/bin/workd"
            "#,
        )
        .unwrap();
        let c = Config::load(dir.path()).unwrap();
        let path = |i: usize| match &c.hosts[i].transport {
            HostTransport::Ssh { otterd_path, .. } => otterd_path.clone(),
            other => panic!("{other:?}"),
        };
        // The old default follows the rename; a custom path is kept.
        assert_eq!(path(0), DEFAULT_REMOTE_OTTERD);
        assert_eq!(path(1), "/opt/workd/bin/workd");
    }

    #[test]
    fn minimal_ssh_host_gets_default_otterd_path() {
        let c: Config = toml::from_str(
            r#"
            [[host]]
            name = "dev"
            transport = "ssh"
            destination = "dev"
            "#,
        )
        .unwrap();
        match &c.hosts[0].transport {
            HostTransport::Ssh { otterd_path, .. } => {
                assert_eq!(otterd_path, DEFAULT_REMOTE_OTTERD)
            }
            other => panic!("{other:?}"),
        }
    }
}
