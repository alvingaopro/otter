//! otterhub run by otterd (D-208). On the host chosen for it, otterd starts
//! `otter hub serve` for the hosts it serves, starts it again when it stops,
//! and stops it when it is turned off. The hub is a process of its own, the
//! `otter` installed beside otterd (D-206): otterd gives it its hosts, as
//! this host reaches them, and the Slack app's tokens, and reads back what it
//! says of itself. otterd itself still reaches no other host; it keeps the
//! hub's list for the hub.
//!
//! ```text
//! state/otterhub/hosts.toml         the hub's hosts: this one first (local)
//! state/otterhub/hub/state.json     whose each message is (the hub's)
//! state/otterhub/hub/status.json    what the hub says of itself, lately
//! ```
//!
//! The tokens reach the hub in its environment, never on its command line
//! nor in a file of the hub's; it takes them out of its environment before
//! it starts anything (`otter hub serve`). Turned on here, otterd also sets
//! itself to start with this machine (`boot.rs`): a hub that is meant to be
//! always there must come back after a restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use otter_protocol::host::{HubServed, HubSettings, HubVia, HubView};
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;

use crate::daemon::Daemon;
use crate::slack::{SLACK_APP_TOKEN, SLACK_BOT_TOKEN};

/// What the hub said of itself longer ago than this is not "running".
const FRESH: chrono::TimeDelta = chrono::TimeDelta::seconds(90);

/// The hub as this otterd runs it.
#[derive(Default)]
pub struct Hub {
    run: std::sync::Mutex<Run>,
    restart: tokio::sync::Notify,
}

#[derive(Clone, Debug, Default)]
struct Run {
    /// `starting` (its process runs) or `failed`; `None`: not started.
    state: Option<&'static str>,
    detail: Option<String>,
    /// How otterd starts with this machine (`boot.rs`).
    boot: Option<String>,
}

impl Hub {
    /// Its settings or tokens changed: start it again with them.
    pub fn restart(&self) {
        self.restart.notify_one();
    }
}

/// What the hub writes of itself (`otter hub serve`).
#[derive(Debug, Default, Deserialize)]
struct Said {
    at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    slack: Option<String>,
    #[serde(default)]
    hosts: std::collections::BTreeMap<String, String>,
}

/// `settings.set {hub}` as kept: names as hosts take them, each once, its
/// members as Slack writes them.
pub fn checked(mut h: HubSettings) -> Result<HubSettings, String> {
    h.name = h.name.trim().to_owned();
    otter_core::validate_name("host", &h.name)?;
    h.users = crate::slack::members(&h.users)?;
    if h.enabled && h.users.is_empty() {
        return Err("otterhub needs your Slack member ID (U…)".into());
    }
    let mut names = vec![h.name.clone()];
    for host in &h.hosts {
        otter_core::validate_name("host", &host.name)?;
        if names.contains(&host.name) {
            return Err(format!("host `{}` is there twice", host.name));
        }
        names.push(host.name.clone());
    }
    if let Some(d) = &h.default_host
        && !names.contains(d)
    {
        return Err(format!("the default host `{d}` isn't one the hub serves"));
    }
    Ok(h)
}

/// What the hub runs with.
struct Plan {
    hub: HubSettings,
    bot: String,
    app: String,
}

impl Daemon {
    fn hub_dir(&self) -> PathBuf {
        self.paths.state_dir.join("otterhub")
    }

    fn hub_status(&self, state: Option<&'static str>, detail: Option<String>) {
        let mut run = self.hub.run.lock().unwrap();
        run.state = state;
        run.detail = detail;
    }

    /// Settings' view of the hub: as set, and how it is — `running` once
    /// it has said so lately; `off` where it was never set (an otterd before
    /// it has no view).
    pub(crate) fn hub_view(&self) -> Option<HubView> {
        let Some(hub) = self.settings.read().unwrap().hub().cloned() else {
            return Some(HubView {
                state: "off".into(),
                ..Default::default()
            });
        };
        let run = self.hub.run.lock().unwrap().clone();
        let said = std::fs::read(self.hub_dir().join("hub/status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Said>(&b).ok())
            .unwrap_or_default();
        let lately = said.at.is_some_and(|at| chrono::Utc::now() - at < FRESH);
        let state = match (hub.enabled, run.state) {
            (false, _) => "off",
            (true, Some("starting")) if lately => "running",
            (true, Some(s)) => s,
            (true, None) => "starting",
        };
        let (slack, served) = match lately && state == "running" {
            true => (
                said.slack,
                said.hosts
                    .into_iter()
                    .map(|(host, line)| HubServed { host, line })
                    .collect(),
            ),
            false => (None, vec![]),
        };
        let mut hosts = vec![hub.name.clone()];
        hosts.extend(hub.hosts.iter().map(|h| h.name.clone()));
        Some(HubView {
            enabled: hub.enabled,
            name: hub.name,
            users: hub.users,
            default_host: hub.default_host,
            hosts,
            state: state.into(),
            detail: run.detail.filter(|_| hub.enabled),
            slack,
            served,
            boot: run.boot,
        })
    }

    /// The hub as it is to run now; `None`: it isn't to run.
    fn hub_plan(&self) -> Result<Option<Plan>, String> {
        let s = self.settings.read().unwrap();
        let Some(hub) = s.hub().filter(|h| h.enabled).cloned() else {
            return Ok(None);
        };
        match (s.secret(SLACK_BOT_TOKEN), s.secret(SLACK_APP_TOKEN)) {
            (Some(bot), Some(app)) => Ok(Some(Plan { hub, bot, app })),
            _ => Err("Add the Slack app's bot token (xoxb-…) and app token (xapp-…) on this host: the hub connects with them.".into()),
        }
    }

    /// Keep the hub running while it is turned on here: again after it
    /// stops (waiting longer each time it stops soon, up to a minute), at
    /// once when its settings change, and stopped with otterd.
    pub(crate) async fn run_hub(self: Arc<Self>) {
        let mut shutdown = self.shutdown_signal();
        let mut wait = Duration::ZERO;
        loop {
            let plan = match self.hub_plan() {
                Ok(Some(plan)) => plan,
                idle => {
                    let detail = idle.err();
                    let state = detail.is_some().then_some("failed");
                    self.hub_status(state, detail);
                    tokio::select! {
                        _ = self.hub.restart.notified() => {}
                        _ = shutdown.changed() => return,
                    }
                    wait = Duration::ZERO;
                    continue;
                }
            };
            let began = std::time::Instant::now();
            // Dropped (a restart, or otterd stopping), the hub's process is
            // killed with it.
            let ended = tokio::select! {
                r = self.hub_session(&plan) => r,
                _ = self.hub.restart.notified() => {
                    wait = Duration::ZERO;
                    continue;
                }
                _ = shutdown.changed() => return,
            };
            let why = match ended {
                Ok(how) => how,
                Err(e) => otter_core::redact::redact(&format!("{e:#}")),
            };
            tracing::warn!("otterhub: {why}");
            self.hub_status(
                Some("failed"),
                Some(format!("{why}; otterd starts it again.")),
            );
            wait = match began.elapsed() > Duration::from_secs(60) {
                true => Duration::from_secs(1),
                false => (wait * 2).clamp(Duration::from_secs(1), Duration::from_secs(60)),
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.hub.restart.notified() => wait = Duration::ZERO,
                _ = shutdown.changed() => return,
            }
        }
    }

    /// The hub's files written, otterd set to start with this machine, and
    /// the hub run until it stops: how it stopped.
    async fn hub_session(&self, plan: &Plan) -> anyhow::Result<String> {
        let dir = self.hub_dir();
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
        }
        let otterd = crate::env::self_exe()?;
        hosts_file(&dir, &plan.hub, &otterd, &self.paths.home)?;
        let env = self.environments.base().clone();
        let boot = crate::boot::ensure(&otterd, &self.paths.home, &env).await;
        self.hub.run.lock().unwrap().boot = Some(boot);
        let otter = otter_beside(&otterd, &env).ok_or_else(|| {
            anyhow::anyhow!(
                "`otter` isn't installed beside otterd (`otter host install` puts it there)"
            )
        })?;
        let mut cmd = tokio::process::Command::new(&otter);
        cmd.args(["hub", "serve", "--here"]).arg(&plan.hub.name);
        for u in &plan.hub.users {
            cmd.arg("--user").arg(u);
        }
        // otterd's own environment, as a daemon it starts has it, with the
        // hub's directory and, apart from any command line, the tokens.
        cmd.env("OTTER_CONFIG_DIR", &dir)
            .env(SLACK_BOT_TOKEN, &plan.bot)
            .env(SLACK_APP_TOKEN, &plan.app)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = crate::env::spawn_tokio(&mut cmd).await?;
        tracing::info!("otterhub: started ({})", otter.display());
        self.hub_status(Some("starting"), None);
        // What it says goes to otterd's log, as otterd's own does.
        let mut said = Vec::new();
        for pipe in [
            child
                .stdout
                .take()
                .map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|e| Box::new(e) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        {
            said.push(tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!("otterhub: {}", otter_core::redact::redact(&line));
                }
            }));
        }
        let status = child.wait().await?;
        for s in said {
            let _ = s.await;
        }
        Ok(format!("the hub stopped ({status})"))
    }
}

/// `otter` beside otterd, as `otter host install` puts them; else on `PATH`.
fn otter_beside(otterd: &Path, env: &crate::env::EnvMap) -> Option<PathBuf> {
    otterd
        .parent()
        .map(|d| d.join("otter"))
        .filter(|p| p.is_file())
        .or_else(|| crate::env::which("otter", env))
}

/// The hub's `hosts.toml`, as the CLI writes it: this host first, local to
/// otterd at `otterd` with this `home`, then the others as set.
fn hosts_file(dir: &Path, hub: &HubSettings, otterd: &Path, home: &Path) -> anyhow::Result<()> {
    use otter_client::config::{Config, HostEntry, HostTransport};
    let path = dir.join("hosts.toml");
    let _ = std::fs::remove_file(&path);
    let mut c = Config::load(dir)?;
    c.add_host(HostEntry {
        name: hub.name.clone(),
        transport: HostTransport::Local {
            otterd_path: Some(otterd.to_string_lossy().into_owned()),
            home: Some(home.to_string_lossy().into_owned()),
        },
    })?;
    for h in &hub.hosts {
        let transport = match &h.via {
            HubVia::Local { otterd_path, home } => HostTransport::Local {
                otterd_path: otterd_path.clone(),
                home: home.clone(),
            },
        };
        c.add_host(HostEntry {
            name: h.name.clone(),
            transport,
        })?;
    }
    c.default_host = hub.default_host.clone();
    c.save()
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_protocol::host::HubHost;

    fn hub(name: &str, users: &[&str], hosts: &[&str]) -> HubSettings {
        HubSettings {
            enabled: true,
            name: name.into(),
            users: users.iter().map(|u| u.to_string()).collect(),
            default_host: None,
            hosts: hosts
                .iter()
                .map(|h| HubHost {
                    name: h.to_string(),
                    via: HubVia::Local {
                        otterd_path: None,
                        home: Some(format!("/w/{h}")),
                    },
                })
                .collect(),
        }
    }

    /// What is kept is a name hosts take, each once, a default it serves,
    /// and members as Slack writes them; turned on, it needs one.
    #[test]
    fn hub_settings_are_checked_as_kept() {
        let h = checked(hub(" code-server ", &["@U06MPBPRX7C"], &["gpu"])).unwrap();
        assert_eq!(h.name, "code-server");
        assert_eq!(h.users, vec!["U06MPBPRX7C"]);
        assert!(
            checked(hub("a", &["U1A"], &["a"]))
                .unwrap_err()
                .contains("twice")
        );
        assert!(
            checked(hub("a", &[], &[]))
                .unwrap_err()
                .contains("member ID")
        );
        let mut off = hub("a", &[], &[]);
        off.enabled = false;
        assert!(checked(off).is_ok());
        let mut d = hub("a", &["U1A"], &["b"]);
        d.default_host = Some("c".into());
        assert!(checked(d.clone()).unwrap_err().contains("default host"));
        d.default_host = Some("b".into());
        assert!(checked(d).is_ok());
        assert!(checked(hub("a:b", &["U1A"], &[])).is_err());
    }

    /// The hub's hosts as the CLI reads them: this one first, local to this
    /// otterd and home, then the others, and the default.
    #[test]
    fn the_hubs_hosts_are_this_one_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = hub("code-server", &["U1A"], &["gpu"]);
        h.default_host = Some("code-server".into());
        hosts_file(
            dir.path(),
            &h,
            Path::new("/bin/otterd"),
            Path::new("/home/u/.workd"),
        )
        .unwrap();
        // Written again, it is the same.
        hosts_file(
            dir.path(),
            &h,
            Path::new("/bin/otterd"),
            Path::new("/home/u/.workd"),
        )
        .unwrap();
        let c = otter_client::config::Config::load(dir.path()).unwrap();
        let names: Vec<&str> = c.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["code-server", "gpu"]);
        assert_eq!(c.default_host.as_deref(), Some("code-server"));
        match &c.hosts[0].transport {
            otter_client::config::HostTransport::Local { otterd_path, home } => {
                assert_eq!(otterd_path.as_deref(), Some("/bin/otterd"));
                assert_eq!(home.as_deref(), Some("/home/u/.workd"));
            }
            other => panic!("{other:?}"),
        }
    }
}
