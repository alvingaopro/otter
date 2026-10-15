//! `otter trigger …` (D-083): work that starts by itself, on a schedule or
//! from a script, under a policy set here in advance.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use otter_core::trigger::{Policy, Schedule, Trigger};

use crate::config::{Config, HostEntry};
use crate::output;
use crate::target::connect;

#[derive(Subcommand)]
pub enum TriggerCommand {
    /// Start a task in a workspace on a schedule (or by hand), unattended.
    Add {
        /// A name for it.
        name: String,
        /// The workspace its tasks work in.
        #[arg(long)]
        workspace: String,
        /// What each task should do.
        #[arg(long)]
        request: String,
        /// The tasks' title (default: the trigger's name).
        #[arg(long)]
        title: Option<String>,
        /// Every so long: `30m`, `6h`.
        #[arg(long, conflicts_with = "daily")]
        every: Option<String>,
        /// Every day at `HH:MM` (UTC).
        #[arg(long)]
        daily: Option<String>,
        /// Allow its tasks to publish — push the workspace's branch and
        /// open the pull request — without asking. Nothing else is allowed
        /// in advance.
        #[arg(long)]
        publish: bool,
        /// Pause a task whose decision has waited this long for you
        /// (`90s`, `30m`, `2h`), instead of letting it wait.
        #[arg(long)]
        wait_limit: Option<String>,
        /// Attempts per task.
        #[arg(long)]
        max_attempts: Option<u32>,
        /// Minutes of agent work per task.
        #[arg(long)]
        max_minutes: Option<u32>,
        #[arg(long)]
        host: Option<String>,
    },
    /// Triggers, with when they fire next and what they did last.
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        host: Option<String>,
    },
    /// Remove a trigger (the tasks it started stay).
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        #[arg(long)]
        host: Option<String>,
    },
    /// Fire a trigger now. With `--occurrence`, a retry of the same one
    /// starts no second task.
    Run {
        name: String,
        /// Which occurrence (RFC 3339), for a script that may retry.
        #[arg(long)]
        occurrence: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
}

/// `30m`, `6h`, `90s`, `2d` → seconds.
pub fn seconds(s: &str) -> Result<u64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n
        .parse()
        .with_context(|| format!("`{s}` isn't a duration (e.g. 30m, 6h)"))?;
    Ok(match unit {
        "s" => n,
        "m" | "" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => bail!("`{s}` isn't a duration (e.g. 30m, 6h)"),
    })
}

fn host(config: &Config, name: Option<&str>) -> Result<HostEntry> {
    match name {
        Some(n) => Ok(config.host(n)?.clone()),
        None => match config.hosts.as_slice() {
            [] => bail!("no hosts registered; add one with `otter host add <name>`"),
            [one] => Ok(one.clone()),
            _ => config
                .default_host()
                .cloned()
                .context("several hosts: say which with --host"),
        },
    }
}

pub async fn command(config: &Config, cmd: TriggerCommand, json: bool) -> Result<()> {
    match cmd {
        TriggerCommand::Add {
            name,
            workspace,
            request,
            title,
            every,
            daily,
            publish,
            wait_limit,
            max_attempts,
            max_minutes,
            host: h,
        } => {
            let schedule = match (every, daily) {
                (Some(e), _) => Schedule::Every {
                    minutes: (seconds(&e)? / 60).max(1) as u32,
                },
                (None, Some(at)) => Schedule::Daily { at },
                (None, None) => Schedule::Manual,
            };
            let policy = Policy {
                publish,
                max_iterations: max_attempts,
                max_minutes,
                wait_limit_secs: wait_limit.as_deref().map(seconds).transpose()?,
            };
            let entry = host(config, h.as_deref())?;
            let t = connect(config, &entry)
                .await?
                .trigger_create(otter_protocol::TriggerCreate {
                    name,
                    workspace,
                    title,
                    request,
                    schedule,
                    policy,
                })
                .await?;
            if json {
                return output::print_json(&t);
            }
            println!(
                "trigger {} on {}: {}{}",
                t.name,
                entry.name,
                t.schedule.describe(),
                t.next_at
                    .map(|n| format!(", next at {}", n.format("%Y-%m-%d %H:%M UTC")))
                    .unwrap_or_default()
            );
            Ok(())
        }
        TriggerCommand::List { host: h } => {
            let entry = host(config, h.as_deref())?;
            let list = connect(config, &entry).await?.trigger_list().await?;
            if json {
                return output::print_json(&list);
            }
            if list.is_empty() {
                println!(
                    "no triggers on {} (add one with `otter trigger add`)",
                    entry.name
                );
            }
            for t in &list {
                show(t);
            }
            Ok(())
        }
        TriggerCommand::Remove { name, host: h } => {
            let entry = host(config, h.as_deref())?;
            connect(config, &entry).await?.trigger_delete(&name).await?;
            println!("trigger {name} removed (the tasks it started stay)");
            Ok(())
        }
        TriggerCommand::Run {
            name,
            occurrence,
            host: h,
        } => {
            let occurrence = occurrence
                .map(|o| {
                    chrono::DateTime::parse_from_rfc3339(&o)
                        .map(|t| t.with_timezone(&chrono::Utc))
                        .with_context(|| format!("`{o}` isn't an RFC 3339 time"))
                })
                .transpose()?;
            let entry = host(config, h.as_deref())?;
            let t = connect(config, &entry)
                .await?
                .trigger_run(&name, occurrence)
                .await?;
            if json {
                return output::print_json(&t);
            }
            match t.history.last() {
                Some(f) => match (&f.task_id, &f.skipped) {
                    (Some(id), _) => println!("{}: started task {id}", t.name),
                    (None, Some(why)) => println!("{}: skipped — {why}", t.name),
                    _ => println!("{}: fired", t.name),
                },
                None => println!("{}: fired", t.name),
            }
            Ok(())
        }
    }
}

fn show(t: &Trigger) {
    let next = t
        .next_at
        .map(|n| format!(", next {}", n.format("%m-%d %H:%M UTC")))
        .unwrap_or_default();
    let mut policy = Vec::new();
    if t.policy.publish {
        policy.push("publishes".to_owned());
    }
    if let Some(w) = t.policy.wait_limit_secs {
        policy.push(format!("waits {}m for you", w / 60));
    }
    println!(
        "{}  {}{}{}  “{}”",
        t.name,
        t.schedule.describe(),
        next,
        if policy.is_empty() {
            String::new()
        } else {
            format!(" · {}", policy.join(" · "))
        },
        t.title
    );
    if let Some(f) = t.history.last() {
        match (&f.task_id, &f.skipped) {
            (Some(id), _) => println!("    last: {} → {id}", f.at.format("%m-%d %H:%M")),
            (None, Some(why)) => {
                println!("    last: {} skipped — {why}", f.at.format("%m-%d %H:%M"))
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_people_write_them() {
        assert_eq!(seconds("90s").unwrap(), 90);
        assert_eq!(seconds("30m").unwrap(), 1800);
        assert_eq!(seconds("6h").unwrap(), 21600);
        assert_eq!(seconds("15").unwrap(), 900);
        assert!(seconds("soon").is_err());
        assert!(seconds("3w").is_err());
    }
}
