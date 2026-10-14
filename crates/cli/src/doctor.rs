//! `otter doctor` (D-077): what's wrong with each host, and what to do —
//! from otterd's own account (`HostStatus::problems`), without reading its
//! log or a terminal.

use std::io::IsTerminal;

use anyhow::{Result, bail};
use otter_core::Problem;

use crate::config::{Config, HostEntry};
use crate::output;
use crate::target::connect;

struct Report {
    host: String,
    problems: Vec<Problem>,
    log: Option<String>,
}

/// Each host's problems; fails (exit 1) when there is any.
pub async fn run(config: &Config, host: Option<String>, json: bool) -> Result<()> {
    let hosts: Vec<HostEntry> = match host {
        Some(n) => vec![config.host(&n)?.clone()],
        None => config.hosts.clone(),
    };
    if hosts.is_empty() {
        bail!("no hosts registered; add one with `otter host add <name>`");
    }
    let mut reports = Vec::new();
    for h in hosts {
        let report = match connect(config, &h).await {
            Ok(mut conn) => match conn.host_status().await {
                Ok(s) => Report {
                    host: h.name.clone(),
                    problems: s.problems,
                    log: s.log,
                },
                Err(e) => unreachable(&h, &e.to_string()),
            },
            Err(e) => unreachable(&h, &format!("{e:#}")),
        };
        reports.push(report);
    }
    let count: usize = reports.iter().map(|r| r.problems.len()).sum();
    if json {
        let v: Vec<_> = reports
            .iter()
            .map(|r| serde_json::json!({"host": r.host, "problems": r.problems, "log": r.log}))
            .collect();
        output::print_json(&v)?;
    } else {
        let color = std::io::stdout().is_terminal();
        for r in &reports {
            if r.problems.is_empty() {
                println!("{}: {}", r.host, paint("ok", "32", color));
            } else {
                println!(
                    "{}: {}",
                    r.host,
                    paint(&format!("{} problem(s)", r.problems.len()), "31", color)
                );
                for p in &r.problems {
                    println!("  {} {}", paint("✗", "31", color), p.what);
                    println!("    → {}", p.next);
                }
            }
            if let Some(log) = &r.log {
                println!("  {}", paint(&format!("otterd's log: {log}"), "2", color));
            }
        }
    }
    if count > 0 {
        bail!("{count} problem(s) found");
    }
    Ok(())
}

/// A host that doesn't answer is a problem of its own.
fn unreachable(h: &HostEntry, e: &str) -> Report {
    Report {
        host: h.name.clone(),
        problems: vec![Problem {
            what: format!(
                "Otter can't reach otterd on {} ({}).",
                h.name,
                otter_core::redact::redact(e)
            ),
            next: format!(
                "Check that the host is up and reachable (for SSH: `ssh {}` without a prompt), and that otterd is installed: `otter host install {}`.",
                h.name, h.name
            ),
        }],
        log: None,
    }
}

fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}
