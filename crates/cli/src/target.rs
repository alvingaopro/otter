//! Finding workspaces across hosts.
//!
//! Workspaces are addressed independently of where they run (design §25):
//! `scratch`, `scratch/shell`, or — when a name exists on several hosts —
//! `dev-01:scratch/shell`. Workspace ids (`ws_…`) work everywhere names do.

use anyhow::{Context, Result, bail};
use otter_client::Connection;
use otter_core::{Session, Workspace};
use tokio::task::JoinSet;

use crate::config::{Config, HostEntry};

/// A parsed `[host:]workspace[/session]`.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub host: Option<String>,
    pub workspace: String,
    pub session: Option<String>,
}

impl Target {
    pub fn parse(s: &str) -> Result<Target> {
        let (host, rest) = match s.split_once(':') {
            Some((h, rest)) => (Some(h.to_owned()), rest),
            None => (None, s),
        };
        let (workspace, session) = match rest.split_once('/') {
            Some((w, sess)) => (w, Some(sess.to_owned()).filter(|s| !s.is_empty())),
            None => (rest, None),
        };
        if workspace.is_empty() {
            bail!("expected [host:]workspace[/session], got `{s}`");
        }
        Ok(Target {
            host,
            workspace: workspace.to_owned(),
            session,
        })
    }
}

/// A workspace together with a live connection to its host.
pub struct Found {
    pub host: HostEntry,
    pub conn: Connection,
    pub workspace: Workspace,
}

impl Found {
    /// The named session, or the workspace's first session.
    pub fn session(&self, name: Option<&str>) -> Result<Session> {
        match name {
            Some(name) => self.workspace.session(name).cloned().with_context(|| {
                format!(
                    "workspace `{}` has no session `{name}` (sessions: {})",
                    self.workspace.name,
                    session_names(&self.workspace)
                )
            }),
            None => self
                .workspace
                .sessions
                .first()
                .cloned()
                .with_context(|| format!("workspace `{}` has no sessions", self.workspace.name)),
        }
    }

    pub fn label(&self, session: &Session) -> String {
        format!("{}/{}", self.workspace.name, session.name)
    }
}

fn session_names(ws: &Workspace) -> String {
    if ws.sessions.is_empty() {
        return "none".into();
    }
    ws.sessions
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub async fn connect(config: &Config, host: &HostEntry) -> Result<Connection> {
    Connection::connect(&config.transport(host))
        .await
        .with_context(|| format!("host `{}`", host.name))
}

/// Locate a workspace by `target`, querying hosts concurrently.
pub async fn find(config: &Config, target: &Target) -> Result<Found> {
    let hosts: Vec<HostEntry> = match &target.host {
        Some(h) => vec![config.host(h)?.clone()],
        None if config.hosts.is_empty() => {
            bail!("no hosts registered; add one with `otter host add <name>`")
        }
        None => config.hosts.clone(),
    };

    let mut jobs = JoinSet::new();
    for host in hosts {
        let transport = config.transport(&host);
        jobs.spawn(async move {
            let result = async {
                let mut conn = Connection::connect(&transport).await?;
                let list = conn.workspace_list().await?;
                Ok::<_, otter_client::ClientError>((conn, list))
            }
            .await;
            (host, result)
        });
    }

    let mut matches = Vec::new();
    let mut failures = Vec::new();
    while let Some(joined) = jobs.join_next().await {
        let (host, result) = joined?;
        match result {
            Ok((conn, list)) => {
                let ws = list
                    .iter()
                    .find(|w| w.id.as_str() == target.workspace)
                    .or_else(|| list.iter().find(|w| w.name == target.workspace));
                if let Some(ws) = ws {
                    matches.push(Found {
                        host,
                        conn,
                        workspace: ws.clone(),
                    });
                }
            }
            Err(e) => failures.push(format!("{}: {e}", host.name)),
        }
    }

    match matches.len() {
        1 => Ok(matches.pop().unwrap()),
        0 => {
            let mut msg = format!("no workspace `{}`", target.workspace);
            if !failures.is_empty() {
                msg.push_str(&format!(" (could not reach: {})", failures.join("; ")));
            }
            bail!(msg)
        }
        _ => {
            let options: Vec<String> = matches
                .iter()
                .map(|m| format!("{}:{}", m.host.name, m.workspace.name))
                .collect();
            bail!(
                "`{}` exists on several hosts; use one of: {}",
                target.workspace,
                options.join(", ")
            )
        }
    }
}

/// How long a host gets to say which workspaces it has when a message is
/// being routed: a host that is down must not hold the others up.
const ROUTE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The host a message goes to (D-179): the one asked for with `--host`; the
/// only one there is; else the one that has the workspace or repository the
/// message names (every host asked at once); else the default host. Said on
/// stderr when it was the message that chose.
pub async fn pick_host(config: &Config, explicit: Option<String>, text: &str) -> Result<HostEntry> {
    if let Some(h) = explicit {
        return Ok(config.host(&h)?.clone());
    }
    match config.hosts.as_slice() {
        [] => bail!("no hosts registered; add one with `otter host add <name>`"),
        [only] => return Ok(only.clone()),
        _ => {}
    }
    let default = config
        .default_host
        .as_deref()
        .map(|d| config.host(d).cloned())
        .transpose()?;
    if text.trim().is_empty() {
        return default.ok_or_else(|| anyhow::anyhow!("several hosts; choose one with --host"));
    }
    let mut jobs = JoinSet::new();
    for host in config.hosts.clone() {
        let mut transport = config.transport(&host);
        // Asking is not worth a prompt: a host whose key or passphrase
        // needs one is left out, not waited for.
        if let otter_client::Transport::Ssh { ssh_args, .. } = &mut transport {
            ssh_args.extend(["-o".to_owned(), "BatchMode=yes".to_owned()]);
        }
        jobs.spawn(async move {
            let list = tokio::time::timeout(ROUTE_WAIT, async {
                let mut conn = Connection::connect(&transport).await.ok()?;
                conn.workspace_list().await.ok()
            })
            .await
            .ok()
            .flatten();
            (host.name, list)
        });
    }
    let mut asked: Vec<(String, Vec<Workspace>)> = Vec::new();
    let mut unreached: Vec<String> = Vec::new();
    for (host, list) in jobs.join_all().await {
        match list {
            // Put away (D-105) is not where a message is about.
            Some(mut list) => {
                list.retain(|w| w.state != otter_core::WorkspaceState::Archived);
                asked.push((host, list));
            }
            None => unreached.push(host),
        }
    }
    asked.sort_by(|a, b| a.0.cmp(&b.0));
    unreached.sort();
    if !unreached.is_empty() {
        eprintln!("(not asked, unreachable: {})", unreached.join(", "));
    }
    match otter_client::route::route(text, &asked) {
        otter_client::route::Route::One(m) => {
            eprintln!("→ {} ({})", m.host, m.why);
            Ok(config.host(&m.host)?.clone())
        }
        otter_client::route::Route::Several(m) => default.ok_or_else(|| {
            let names: Vec<String> = m.iter().map(|m| m.host.clone()).collect();
            anyhow::anyhow!(
                "this is about workspaces on {}; choose one with --host",
                names.join(" and ")
            )
        }),
        otter_client::route::Route::Nowhere => {
            default.ok_or_else(|| anyhow::anyhow!("several hosts; choose one with --host"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        assert_eq!(
            Target::parse("scratch").unwrap(),
            Target {
                host: None,
                workspace: "scratch".into(),
                session: None
            }
        );
        assert_eq!(
            Target::parse("dev:Fix GCP renewal/codex").unwrap(),
            Target {
                host: Some("dev".into()),
                workspace: "Fix GCP renewal".into(),
                session: Some("codex".into())
            }
        );
        assert!(Target::parse("dev:").is_err());
    }
}
