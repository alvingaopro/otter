//! `otter watch …` (D-183): what Otter looks at again later for you, on
//! every host's tasks; stopping or removing one. Watches are set by asking
//! Otter in a task's conversation.

use anyhow::{Result, bail};
use clap::Subcommand;
use otter_core::task::{Task, TaskAction};
use otter_core::watch::{Watch, WatchStatus};

use crate::config::Config;
use crate::output;
use crate::tasks::{all_tasks, command_id, find_task};

#[derive(Subcommand)]
pub enum WatchCommand {
    /// What Otter is watching for you, on every host.
    #[command(visible_alias = "ls")]
    List {
        /// Only this task's ([host:]task).
        #[arg(long)]
        task: Option<String>,
        /// Ended ones too.
        #[arg(long)]
        all: bool,
    },
    /// Stop a watch: it won't come back to you.
    Stop { watch: String },
    /// Remove a watch (one still waiting is stopped first).
    #[command(visible_alias = "rm")]
    Remove { watch: String },
}

pub async fn command(config: &Config, cmd: WatchCommand, json: bool) -> Result<()> {
    match cmd {
        WatchCommand::List { task, all } => {
            let mut rows: Vec<(String, Task, Watch)> = Vec::new();
            if let Some(t) = task {
                let found = find_task(config, &t).await?;
                for w in &found.task.watches {
                    rows.push((found.host.name.clone(), found.task.clone(), w.clone()));
                }
            } else {
                let (found, failures) = all_tasks(config, None).await?;
                for f in failures {
                    eprintln!("could not reach: {f}");
                }
                for (host, _, list) in found {
                    for t in list {
                        for w in &t.watches {
                            rows.push((host.name.clone(), t.clone(), w.clone()));
                        }
                    }
                }
            }
            rows.retain(|(_, _, w)| all || w.status.is_open());
            rows.sort_by_key(|(_, _, w)| std::cmp::Reverse(w.created_at));
            if json {
                let out: Vec<_> = rows
                    .iter()
                    .map(|(h, t, w)| serde_json::json!({"host": h, "task": t.id, "watch": w}))
                    .collect();
                return output::print_json(&out);
            }
            if rows.is_empty() {
                println!(
                    "nothing watched{} (ask Otter in a task: “tell me when CI is done”)",
                    if all { "" } else { " now" }
                );
            }
            for (host, t, w) in &rows {
                show(host, t, w);
            }
            Ok(())
        }
        WatchCommand::Stop { watch } => {
            let (host, mut conn, t) = owner(config, &watch).await?;
            let id = otter_core::WatchId::from(watch.as_str());
            let f = conn
                .task_act(
                    &command_id(),
                    t.id.as_str(),
                    TaskAction::CancelWatch {
                        watch_id: id.clone(),
                    },
                )
                .await?;
            if json {
                return output::print_json(&f.watch(&id));
            }
            println!(
                "stopped watching {} (“{}” on {host})",
                label(&t, &id),
                t.title
            );
            Ok(())
        }
        WatchCommand::Remove { watch } => {
            let (host, mut conn, t) = owner(config, &watch).await?;
            let id = otter_core::WatchId::from(watch.as_str());
            conn.task_act(
                &command_id(),
                t.id.as_str(),
                TaskAction::DeleteWatch {
                    watch_id: id.clone(),
                },
            )
            .await?;
            println!(
                "removed the watch on {} (“{}” on {host})",
                label(&t, &id),
                t.title
            );
            Ok(())
        }
    }
}

fn label(t: &Task, id: &otter_core::WatchId) -> String {
    t.watch(id)
        .map(|w| w.label.clone())
        .unwrap_or_else(|| id.to_string())
}

/// The task a watch is on, on whichever host has it.
async fn owner(config: &Config, watch: &str) -> Result<(String, otter_client::Connection, Task)> {
    let (found, failures) = all_tasks(config, None).await?;
    for (host, conn, list) in found {
        if let Some(t) = list
            .into_iter()
            .find(|t| t.watches.iter().any(|w| w.id.as_str() == watch))
        {
            return Ok((host.name, conn, t));
        }
    }
    let mut msg = format!("no watch `{watch}` (`otter watch ls --all` lists them)");
    if !failures.is_empty() {
        msg.push_str(&format!(" (could not reach: {})", failures.join("; ")));
    }
    bail!(msg)
}

fn show(host: &str, t: &Task, w: &Watch) {
    let state = match w.status {
        WatchStatus::Waiting => format!("until {}", w.deadline.format("%m-%d %H:%M UTC")),
        WatchStatus::Firing => "came to something; Otter is saying so".into(),
        WatchStatus::Reported => match &w.finding {
            Some(f) => format!("{}: {}", f.outcome.as_str(), f.text),
            None => "said".into(),
        },
        WatchStatus::Cancelled => "stopped".into(),
    };
    println!(
        "{}  {}  {}  · “{}” on {host}",
        w.id, w.label, state, t.title
    );
    if w.status.is_open()
        && let Some(r) = w.last_reading()
    {
        println!("    last read {}: {}", r.at.format("%H:%M UTC"), r.text);
    }
}
