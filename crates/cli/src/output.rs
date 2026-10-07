//! Human-readable output.

use std::collections::HashMap;
use std::io::Write;

use anyhow::Result;
use workd_core::{HostStatus, Session, SessionStatus, Workspace, WorkspaceSource};
use workd_protocol::{Event, EventRecord};

use crate::config::HostEntry;

pub fn print_json<T: serde::Serialize + ?Sized>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

pub fn table(headers: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let last = cells.len() - 1;
        let mut out = String::new();
        for (i, cell) in cells.into_iter().enumerate() {
            out.push_str(cell);
            if i != last {
                let pad = widths[i] - cell.chars().count() + 2;
                out.push_str(&" ".repeat(pad));
            }
        }
        println!("{}", out.trim_end());
    };
    line(headers.to_vec());
    for row in &rows {
        line(row.iter().map(String::as_str).collect());
    }
}

pub fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

pub fn session_status(s: &Session) -> String {
    let status = s.status();
    if let (Some(agent), SessionStatus::Running) = (&s.agent, status) {
        return agent.state.as_str().replace('_', " ");
    }
    match status {
        SessionStatus::Failed if s.launch_error.is_some() => "failed to start".into(),
        SessionStatus::Failed => match s.current_execution().and_then(|e| e.exit_code) {
            Some(code) => format!("failed ({code})"),
            None => "failed".into(),
        },
        other => other.as_str().into(),
    }
}

pub fn sessions_summary(ws: &Workspace) -> String {
    if ws.sessions.is_empty() {
        return "-".into();
    }
    ws.sessions
        .iter()
        .map(|s| format!("{}:{}", s.name, session_status(s)))
        .collect::<Vec<_>>()
        .join("  ")
}

pub fn workspace_details(host: &str, ws: &Workspace) {
    println!("{}  ({})", ws.name, ws.id);
    println!("  host:     {host}");
    println!("  state:    {}", ws.state.as_str());
    if let Some(msg) = &ws.state_message {
        println!("            {msg}");
    }
    println!("  root:     {}", ws.root);
    match &ws.source {
        WorkspaceSource::Git(g) => {
            println!("  repo:     {}", g.repository);
            if !g.branch.is_empty() {
                println!(
                    "  branch:   {} (from {} {})",
                    g.branch,
                    g.base.as_deref().unwrap_or("?"),
                    g.base_commit
                        .as_deref()
                        .map_or("", |c| &c[..c.len().min(10)])
                );
            }
        }
        WorkspaceSource::Directory { .. } => println!("  source:   existing directory"),
        WorkspaceSource::Empty => {}
    }
    if ws.environment.kind != workd_core::EnvironmentKind::None {
        println!(
            "  env:      {} ({})",
            ws.environment.kind.as_str(),
            ws.environment.status.as_str()
        );
        if let Some(m) = &ws.environment.message {
            println!("            {m}");
        }
    }
    let brief = &ws.brief;
    if let Some(t) = &brief.title {
        println!("  title:    {t}");
    }
    if let Some(g) = &brief.goal {
        println!("  goal:     {g}");
    }
    if let Some(d) = &brief.description {
        println!("  about:    {d}");
    }
    println!("  created:  {}", ws.created_at.format("%Y-%m-%d %H:%M"));
    if ws.sessions.is_empty() {
        println!("  sessions: none");
        return;
    }
    println!();
    let rows = ws
        .sessions
        .iter()
        .map(|s| {
            vec![
                s.name.clone(),
                s.kind.as_str().to_owned(),
                session_status(s),
                s.executions.len().to_string(),
                match (&s.agent, &s.command) {
                    (Some(a), _) => a.provider.clone(),
                    (None, Some(c)) => c.clone(),
                    (None, None) => "(login shell)".into(),
                },
            ]
        })
        .collect();
    table(&["SESSION", "KIND", "STATUS", "RUNS", "COMMAND"], rows);
    for s in &ws.sessions {
        if let Some(e) = &s.launch_error {
            println!("{}: {e}", s.name);
        }
        if let Some(agent) = &s.agent {
            if let Some(id) = &agent.resume_id {
                println!("{}: {} conversation {id}", s.name, agent.provider);
            }
            if let Some(m) = &agent.last_message {
                println!("{}: “{m}”", s.name);
            }
        }
    }
    if !ws.attention.is_empty() {
        println!();
        for a in &ws.attention {
            println!(
                "! {:<10} {}  ({})",
                a.kind.as_str(),
                a.summary,
                crate::dashboard::ago(a.created_at)
            );
        }
    }
}

pub fn host_status(host: &HostEntry, result: Result<HostStatus>) {
    match result {
        Err(e) => println!("{}  unreachable: {e:#}", host.name),
        Ok(s) => {
            println!(
                "{}  workd {} on {} ({}/{}), up since {}",
                host.name,
                s.workd_version,
                s.hostname,
                s.os,
                s.arch,
                s.started_at.format("%Y-%m-%d %H:%M")
            );
            println!("  environment: {}", s.environment_source);
            let caps: Vec<String> = s
                .capabilities
                .iter()
                .map(|c| {
                    if c.available {
                        format!("{} ✓", c.name)
                    } else {
                        format!("{} ✗", c.name)
                    }
                })
                .collect();
            println!("  tools:       {}", caps.join("  "));
            for c in s.capabilities.iter().filter(|c| c.available) {
                if let Some(v) = &c.version {
                    println!("    {:<7} {v}", c.name);
                }
            }
        }
    }
}

pub fn event_line(host: &str, rec: &EventRecord, names: &mut HashMap<String, String>, json: bool) {
    // Learn names from events that carry them.
    match &rec.event {
        Event::WorkspaceCreated { workspace_id, name } => {
            names.insert(workspace_id.to_string(), name.clone());
        }
        Event::SessionCreated {
            session_id, name, ..
        } => {
            names.insert(session_id.to_string(), name.clone());
        }
        _ => {}
    }
    if json {
        let mut v = serde_json::to_value(rec).unwrap_or_default();
        v["host"] = serde_json::Value::String(host.to_owned());
        println!("{v}");
        return;
    }
    let name = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_owned());
    let mut subject = String::new();
    if let Some(ws) = rec.event.workspace_id() {
        subject.push_str(&name(ws.as_str()));
        if let Some(s) = rec.event.session_id() {
            subject.push('/');
            subject.push_str(&name(s.as_str()));
        }
    }
    let detail = match &rec.event {
        Event::ExecutionExited {
            exit_code: Some(c), ..
        } => format!("status {c}"),
        Event::ExecutionFailed { message, .. } => message.clone(),
        Event::DaemonStarted { version } => format!("workd {version}"),
        Event::AgentStateChanged { state, .. } => state.as_str().to_owned(),
        Event::AttentionCreated { kind, .. } => kind.as_str().to_owned(),
        Event::WorkspaceFailed { message, .. } | Event::EnvironmentFailed { message, .. } => {
            message.lines().next().unwrap_or_default().to_owned()
        }
        Event::SessionCreated { kind, .. } => kind.as_str().to_owned(),
        _ => String::new(),
    };
    println!(
        "{}  {:<10} {:<18} {} {}",
        rec.ts
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M:%S"),
        host,
        rec.event.kind(),
        subject,
        detail
    );
}
