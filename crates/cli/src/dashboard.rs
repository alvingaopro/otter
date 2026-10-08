//! The primary view (design §26): workspaces grouped by what they need from
//! you, across all hosts.
//!
//! ```text
//! NEEDS YOU
//!   ● gcp-renewal   dev-01  codex finished: Updated the validation…   2m
//! WORKING
//!   ● iam-cleanup   dev-01  codex working 3m
//! ```

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::Utc;
use otter_client::Connection;
use otter_core::{Activity, AgentState, SessionKind, SessionStatus, Timestamp, Workspace};
use tokio::task::JoinSet;

use crate::config::Config;
use crate::output;

pub struct HostResult {
    pub host: String,
    pub result: Result<Vec<Workspace>, String>,
}

/// Every host's workspaces, queried concurrently, in registry order.
pub async fn fetch_all(config: &Config) -> Result<Vec<HostResult>> {
    if config.hosts.is_empty() {
        bail!("no hosts registered; add one with `otter host add <name>`");
    }
    let mut tasks = JoinSet::new();
    for (i, host) in config.hosts.iter().enumerate() {
        let transport = config.transport(host);
        let name = host.name.clone();
        tasks.spawn(async move {
            let result = async {
                let mut conn = Connection::connect(&transport).await?;
                conn.workspace_list().await
            }
            .await
            .map_err(|e| e.to_string());
            (i, HostResult { host: name, result })
        });
    }
    let mut results = tasks.join_all().await;
    results.sort_by_key(|(i, _)| *i);
    Ok(results.into_iter().map(|(_, r)| r).collect())
}

pub async fn show(config: &Config, json: bool) -> Result<()> {
    let results = fetch_all(config).await?;
    if json {
        let v: Vec<_> = results
            .iter()
            .map(|r| match &r.result {
                Ok(list) => {
                    let ws: Vec<_> = list
                        .iter()
                        .map(|w| serde_json::json!({"activity": w.activity(), "workspace": w}))
                        .collect();
                    serde_json::json!({"host": r.host, "workspaces": ws})
                }
                Err(e) => serde_json::json!({"host": r.host, "error": e}),
            })
            .collect();
        return output::print_json(&v);
    }
    print!("{}", render(&results, std::io::stdout().is_terminal()));
    Ok(())
}

/// Redraw whenever any host reports an event (and periodically, so relative
/// times stay fresh). Ctrl-C to quit.
pub async fn watch(config: &Config) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(64);
    for host in &config.hosts {
        let transport = config.transport(host);
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(conn) = Connection::connect(&transport).await
                    && let Ok(mut stream) = conn.subscribe(None).await
                {
                    while let Ok(Some(_)) = stream.next().await {
                        if tx.send(()).await.is_err() {
                            return;
                        }
                    }
                }
                // Host unreachable or connection dropped: retry later.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }
    let color = std::io::stdout().is_terminal();
    loop {
        let results = fetch_all(config).await?;
        print!("\x1b[H\x1b[2J{}", render(&results, color));
        println!(
            "\n{}",
            dim(
                &format!(
                    "updated {} · Ctrl-C to quit",
                    chrono::Local::now().format("%H:%M:%S")
                ),
                color
            )
        );
        let _ = tokio::time::timeout(Duration::from_secs(15), rx.recv()).await;
        // Coalesce bursts of events into one redraw.
        tokio::time::sleep(Duration::from_millis(150)).await;
        while rx.try_recv().is_ok() {}
    }
}

pub fn render(results: &[HostResult], color: bool) -> String {
    let mut rows: Vec<(Activity, &str, &Workspace)> = Vec::new();
    let mut out = String::new();
    for r in results {
        match &r.result {
            Ok(list) => rows.extend(list.iter().map(|w| (w.activity(), r.host.as_str(), w))),
            Err(e) => out.push_str(&format!(
                "{}\n",
                dim(&format!("{}: unreachable ({e})", r.host), color)
            )),
        }
    }
    if rows.is_empty() {
        out.push_str("no workspaces (create one with `otter new <name>`)\n");
        return out;
    }
    // Most urgent first; within a group, the longest-waiting first.
    rows.sort_by_key(|(activity, _, ws)| (*activity, oldest_attention(ws)));
    let name_w = rows
        .iter()
        .map(|(_, _, w)| w.name.chars().count())
        .max()
        .unwrap_or(0);
    let host_w = rows
        .iter()
        .map(|(_, h, _)| h.chars().count())
        .max()
        .unwrap_or(0);

    let mut current = None;
    for (activity, host, ws) in &rows {
        if current != Some(*activity) {
            if current.is_some() {
                out.push('\n');
            }
            out.push_str(&format!("{}\n", heading(*activity, color)));
            current = Some(*activity);
        }
        let (detail, since) = detail(*activity, ws);
        let line = format!(
            "  {} {:<name_w$}  {:<host_w$}  {}",
            bullet(*activity, color),
            ws.name,
            host,
            detail,
        );
        match since {
            Some(t) => out.push_str(&format!("{line}  {}\n", dim(&ago(t), color))),
            None => out.push_str(&format!("{line}\n")),
        }
    }
    out
}

fn oldest_attention(ws: &Workspace) -> Option<Timestamp> {
    ws.attention.iter().map(|a| a.created_at).min()
}

/// What to say about a workspace in its group, and since when.
fn detail(activity: Activity, ws: &Workspace) -> (String, Option<Timestamp>) {
    match activity {
        Activity::NeedsYou | Activity::Failed | Activity::Completed => {
            if let Some(latest) = ws.attention.iter().max_by_key(|a| a.created_at) {
                let more = ws.attention.len() - 1;
                let mut text = latest.summary.clone();
                if more > 0 {
                    text.push_str(&format!(" (+{more})"));
                }
                return (truncate(&text, 90), Some(latest.created_at));
            }
            let msg = ws
                .state_message
                .clone()
                .unwrap_or_else(|| ws.state.as_str().into());
            (truncate(&msg, 90), None)
        }
        Activity::Preparing => (
            ws.state_message
                .clone()
                .unwrap_or_else(|| "preparing".into()),
            None,
        ),
        Activity::Working => {
            let parts: Vec<String> = ws
                .sessions
                .iter()
                .filter_map(|s| match (&s.agent, s.kind, s.status()) {
                    (Some(a), _, SessionStatus::Running)
                        if matches!(a.state, AgentState::Working | AgentState::Starting) =>
                    {
                        Some(format!(
                            "{} {} {}",
                            s.name,
                            a.state.as_str(),
                            ago(a.state_since)
                        ))
                    }
                    (None, SessionKind::Service | SessionKind::Task, SessionStatus::Running) => {
                        Some(format!("{} running", s.name))
                    }
                    _ => None,
                })
                .collect();
            (parts.join(" · "), None)
        }
        Activity::Idle => (output::sessions_summary(ws), None),
    }
}

fn heading(activity: Activity, color: bool) -> String {
    let code = match activity {
        Activity::NeedsYou | Activity::Failed => "1;31",
        Activity::Working => "1;32",
        Activity::Completed => "1;34",
        Activity::Idle | Activity::Preparing => "1",
    };
    paint(activity.label(), code, color)
}

fn bullet(activity: Activity, color: bool) -> String {
    match activity {
        Activity::NeedsYou => paint("●", "31", color),
        Activity::Failed => paint("✗", "31", color),
        Activity::Working => paint("●", "32", color),
        Activity::Completed => paint("✓", "34", color),
        Activity::Preparing => paint("◌", "33", color),
        Activity::Idle => paint("·", "2", color),
    }
}

fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

fn dim(s: &str, color: bool) -> String {
    paint(s, "2", color)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

/// `45s`, `12m`, `3h`, `2d`.
pub fn ago(t: Timestamp) -> String {
    let secs = (Utc::now() - t).num_seconds().max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}
