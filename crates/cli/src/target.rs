//! Finding workspaces across hosts.
//!
//! Workspaces are addressed independently of where they run (design §25):
//! `scratch`, `scratch/shell`, or — when a name exists on several hosts —
//! `dev-01:scratch/shell`. Workspace ids (`ws_…`) work everywhere names do.

use anyhow::{Context, Result, bail};
use tokio::task::JoinSet;
use workd_client::Connection;
use workd_core::{Session, Workspace};

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
            bail!("no hosts registered; add one with `workctl host add <name>`")
        }
        None => config.hosts.clone(),
    };

    let mut tasks = JoinSet::new();
    for host in hosts {
        let transport = config.transport(&host);
        tasks.spawn(async move {
            let result = async {
                let mut conn = Connection::connect(&transport).await?;
                let list = conn.workspace_list().await?;
                Ok::<_, workd_client::ClientError>((conn, list))
            }
            .await;
            (host, result)
        });
    }

    let mut matches = Vec::new();
    let mut failures = Vec::new();
    while let Some(joined) = tasks.join_next().await {
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
