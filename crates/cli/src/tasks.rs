//! Tasks and the coding agent's conversations, from the terminal (D-043,
//! D-055–D-059): the same commands and state the desktop app uses.
//!
//! Tasks are addressed like workspaces: `[host:]task`, where `task`
//! is an id (`ft_…`), a title, or the start of one title.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use otter_client::Connection;
use otter_core::conversation::{Delivery, Turn, TurnState};
use otter_core::task::{DecisionRequest, DecisionStatus, Task, TaskAction};
use otter_protocol::conversation::{ConversationView, HistoryCursor, HistoryQuery};
use otter_protocol::task::TaskCreate;
use tokio::task::JoinSet;

use crate::config::{Config, HostEntry};
use crate::output;
use crate::target::connect;

#[derive(Subcommand)]
pub enum TaskCommand {
    /// Tasks on every host, newest first.
    #[command(visible_alias = "ls")]
    List,
    /// A task: where it stands, what waits on you, the latest messages.
    Show { task: String },
    /// Write down a task (a draft until `otter task start`).
    New {
        title: String,
        /// What should be built, in your words (default: the title).
        #[arg(long)]
        request: Option<String>,
        /// The workspace to do the work in.
        #[arg(long)]
        workspace: Option<String>,
        /// Only an answer is wanted: the coding agent reads and changes
        /// nothing, and a plan to change things comes to you first (needs
        /// the coding agent on the SDK runtime).
        #[arg(long)]
        answer_only: bool,
        /// The host (default: the default host, else the only one).
        #[arg(long)]
        host: Option<String>,
    },
    /// Hand a draft to Otter.
    Start { task: String },
    /// A message to Otter, which passes on to the coding agent what it should
    /// know — when it's useful, not necessarily now.
    #[command(visible_alias = "send")]
    Message { task: String, text: String },
    /// Stop the coding agent's current work now and have it do this next.
    Redirect { task: String, text: String },
    /// Stop the coding agent's current work, and wait for you.
    Interrupt { task: String },
    /// Let the current work finish, then hold.
    Pause {
        /// The task; or `--all`.
        #[arg(required_unless_present = "all")]
        task: Option<String>,
        /// Every task at work, on every host (or `--host`'s): what's
        /// running is held at once (D-081).
        #[arg(long, conflicts_with = "task")]
        all: bool,
        #[arg(long, requires = "all")]
        host: Option<String>,
    },
    /// Go on with a paused task, from where it was held.
    Resume {
        /// The task; or `--all`.
        #[arg(required_unless_present = "all")]
        task: Option<String>,
        /// Every paused task, on every host (or `--host`'s).
        #[arg(long, conflicts_with = "task")]
        all: bool,
        #[arg(long, requires = "all")]
        host: Option<String>,
    },
    /// Try a failed task again, with a fresh budget, from the step that
    /// failed.
    Retry { task: String },
    /// Stop a task for good. Nothing of the work is removed: its files,
    /// branch and pull request stay.
    Cancel { task: String },
    /// Give a task a workspace of its own — a worktree of its Git
    /// workspace's repository, on a branch for it — when another agent
    /// (another task's, or one you started) is working in the one it has.
    OwnWorkspace { task: String },
    /// What happened to a task, oldest first: its history, a line each.
    History {
        task: String,
        /// Only the last N lines.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Accept a task in review: it's done. Refused while a gate (the
    /// pull request, CI, the checks) hasn't passed, unless `--anyway`.
    Accept {
        task: String,
        /// Accept with gates not met (on record in the report).
        #[arg(long)]
        anyway: bool,
    },
    /// Answer what a task waits on you for: approve (default) or deny, or
    /// answer a question.
    Decide {
        task: String,
        /// The decision (`dec_…`, or the start of one).
        decision: String,
        #[arg(long, conflicts_with = "answer")]
        deny: bool,
        /// The answer to a question.
        #[arg(long)]
        answer: Option<String>,
        /// For a form of questions: `--answer-for "Which format?=CSV"`, one per
        /// question.
        #[arg(
            long = "answer-for",
            value_name = "QUESTION=ANSWER",
            conflicts_with = "answer"
        )]
        answers: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum ConversationCommand {
    /// The coding agent's conversations (a task's, if given).
    #[command(visible_alias = "ls")]
    List {
        /// `[host:]task`
        #[arg(long, alias = "feature")]
        task: Option<String>,
        #[arg(long)]
        host: Option<String>,
    },
    /// A conversation, turn by turn: what was asked, the tools, what was said.
    Show {
        /// `[host:]conv_…`
        conversation: String,
    },
    /// The conversation's journal: every recorded change, oldest first.
    History {
        /// `[host:]conv_…`
        conversation: String,
        /// Records after this cursor (`log_id:seq`, printed after a full page).
        #[arg(long)]
        after: Option<String>,
        #[arg(short = 'n', long)]
        limit: Option<u32>,
    },
}

// ---------------------------------------------------------------------------
// Finding things across hosts
// ---------------------------------------------------------------------------

/// `host:rest`, when `host` is a registered host (a title may hold a colon).
fn split_host(s: &str, is_host: impl Fn(&str) -> bool) -> (Option<&str>, &str) {
    match s.split_once(':') {
        Some((h, rest)) if is_host(h) => (Some(h), rest),
        _ => (None, s),
    }
}

fn split<'a>(config: &Config, s: &'a str) -> (Option<&'a str>, &'a str) {
    split_host(s, |h| config.host(h).is_ok())
}

fn hosts(config: &Config, host: Option<&str>) -> Result<Vec<HostEntry>> {
    match host {
        Some(h) => Ok(vec![config.host(h)?.clone()]),
        None if config.hosts.is_empty() => {
            bail!("no hosts registered; add one with `otter host add <name>`")
        }
        None => Ok(config.hosts.clone()),
    }
}

/// Every host's tasks (skipping hosts that can't be reached, said so).
async fn all_tasks(
    config: &Config,
    host: Option<&str>,
) -> Result<(Vec<(HostEntry, Connection, Vec<Task>)>, Vec<String>)> {
    let mut jobs = JoinSet::new();
    for host in hosts(config, host)? {
        let transport = config.transport(&host);
        jobs.spawn(async move {
            let result = async {
                let mut conn = Connection::connect(&transport).await?;
                let list = conn.task_list().await?;
                Ok::<_, otter_client::ClientError>((conn, list))
            }
            .await;
            (host, result)
        });
    }
    let mut found = Vec::new();
    let mut failures = Vec::new();
    while let Some(joined) = jobs.join_next().await {
        let (host, result) = joined?;
        match result {
            Ok((conn, list)) => found.push((host, conn, list)),
            Err(e) => failures.push(format!("{}: {e}", host.name)),
        }
    }
    found.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok((found, failures))
}

/// A task with a connection to its host.
pub struct FoundTask {
    pub host: HostEntry,
    pub conn: Connection,
    pub task: Task,
}

/// Which of `list` `name` means: its id, its title, or the start of one
/// title (ignoring case).
fn pick<'a>(list: &'a [Task], name: &str) -> Vec<&'a Task> {
    if let Some(f) = list.iter().find(|f| f.id.as_str() == name) {
        return vec![f];
    }
    let exact: Vec<_> = list.iter().filter(|f| f.title == name).collect();
    if !exact.is_empty() {
        return exact;
    }
    let lower = name.to_lowercase();
    list.iter()
        .filter(|f| f.title.to_lowercase().starts_with(&lower))
        .collect()
}

pub async fn find_task(config: &Config, target: &str) -> Result<FoundTask> {
    let (host, name) = split(config, target);
    if name.is_empty() {
        bail!("expected [host:]task, got `{target}`");
    }
    let (found, failures) = all_tasks(config, host).await?;
    let mut matches = Vec::new();
    for (host, conn, list) in found {
        let picked: Vec<Task> = pick(&list, name).into_iter().cloned().collect();
        if picked.len() == 1 {
            let task = picked.into_iter().next().unwrap();
            matches.push(FoundTask { host, conn, task });
        } else if picked.len() > 1 {
            let names: Vec<String> = picked
                .iter()
                .map(|f| format!("{} ({})", f.title, f.id))
                .collect();
            bail!(
                "`{name}` matches several tasks on {}: {}",
                host.name,
                names.join(", ")
            );
        }
    }
    match matches.len() {
        1 => Ok(matches.pop().unwrap()),
        0 => {
            let mut msg = format!("no task `{name}`");
            if !failures.is_empty() {
                msg.push_str(&format!(" (could not reach: {})", failures.join("; ")));
            }
            bail!(msg)
        }
        _ => {
            let options: Vec<String> = matches
                .iter()
                .map(|m| format!("{}:{}", m.host.name, m.task.id))
                .collect();
            bail!(
                "`{name}` is a task on several hosts; use one of: {}",
                options.join(", ")
            )
        }
    }
}

/// A conversation by `[host:]conv_…`, asking each host in turn.
async fn find_conversation(
    config: &Config,
    target: &str,
) -> Result<(HostEntry, Connection, ConversationView)> {
    let (host, id) = split(config, target);
    let mut errors = Vec::new();
    for entry in hosts(config, host)? {
        let mut conn = match connect(config, &entry).await {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{e:#}"));
                continue;
            }
        };
        match conn.conversation_get(id).await {
            Ok(v) => return Ok((entry, conn, v)),
            Err(e) => errors.push(format!("{}: {e}", entry.name)),
        }
    }
    bail!("no conversation `{id}` ({})", errors.join("; "))
}

/// A command id: the host records it, so retrying the same command is
/// harmless; a new command gets a new one.
pub(crate) fn command_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("cli-{nanos:x}-{:x}", std::process::id())
}

/// A value's wire name (`implementing`, `waiting`), for people.
fn word<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s.replace('_', " "),
        _ => "?".into(),
    }
}

/// A decision waiting on the developer, with what they need to make it:
/// why it is asked, and the whole command when its summary can't show it
/// (D-157).
fn waiting(d: &DecisionRequest) -> String {
    let mut out = format!("  {}  {} ({})\n", d.id, d.summary, word(&d.kind));
    for line in d.detail.iter().flat_map(|w| w.trim().lines()) {
        out.push_str(&format!("      {line}\n"));
    }
    if let Some(command) = &d.command {
        out.push_str("      the whole command:\n");
        for line in command.lines() {
            out.push_str(&format!("        {line}\n"));
        }
    }
    if !d.options.is_empty() {
        out.push_str(&format!("      options: {}\n", d.options.join(" | ")));
    }
    out
}

fn ago(at: &chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - *at).num_seconds().max(0);
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max - 1).collect::<String>())
    } else if s.trim().lines().count() > 1 {
        format!("{line} …")
    } else {
        line.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

pub async fn task_command(config: &Config, cmd: TaskCommand, json: bool) -> Result<()> {
    match cmd {
        TaskCommand::List => list(config, json).await,
        TaskCommand::Show { task } => {
            let found = find_task(config, &task).await?;
            if json {
                return output::print_json(&found.task);
            }
            show(&found.host.name, &found.task);
            Ok(())
        }
        TaskCommand::New {
            title,
            request,
            workspace,
            answer_only,
            host,
        } => {
            let entry = match host {
                Some(h) => config.host(&h)?.clone(),
                None => match (&config.default_host, config.hosts.as_slice()) {
                    (Some(d), _) => config.host(d)?.clone(),
                    (None, [only]) => only.clone(),
                    (None, []) => {
                        bail!("no hosts registered; add one with `otter host add <name>`")
                    }
                    (None, _) => bail!("several hosts; choose one with --host"),
                },
            };
            let mut conn = connect(config, &entry).await?;
            let f = conn
                .task_create(TaskCreate {
                    command_id: command_id(),
                    request: request.unwrap_or_else(|| title.clone()),
                    title,
                    workspace,
                    answer_only,
                })
                .await?;
            if json {
                return output::print_json(&f);
            }
            println!(
                "task {} ({}) on {} — a draft; `otter task start {}` hands it to Otter",
                f.title, f.id, entry.name, f.id
            );
            Ok(())
        }
        TaskCommand::Message { task, text } => {
            let mut found = find_task(config, &task).await?;
            let id = found.task.id.to_string();
            let f = found.conn.task_send(&command_id(), &id, &text).await?;
            if json {
                return output::print_json(&f);
            }
            println!(
                "Otter has your message for {}. It passes on to the coding agent what it should \
                 know, when it's useful; `otter conversation list --task {}` shows when it's \
                 delivered.",
                f.title, f.id
            );
            Ok(())
        }
        TaskCommand::Redirect { task, text } => {
            act(config, &task, TaskAction::Redirect { text }, json, |f| {
                format!(
                    "{}: the coding agent's current work is stopping; yours goes next ({})",
                    f.title,
                    word(&f.status)
                )
            })
            .await
        }
        TaskCommand::Interrupt { task } => {
            act(config, &task, TaskAction::Interrupt, json, |f| {
                format!(
                    "{}: the coding agent's current work is stopping; then it waits for you \
                     (`otter task resume`)",
                    f.title
                )
            })
            .await
        }
        TaskCommand::Start { task } => {
            act(config, &task, TaskAction::Start, json, |f| {
                format!("{}: {}", f.title, word(&f.status))
            })
            .await
        }
        TaskCommand::Pause {
            all: true, host, ..
        } => every(config, host.as_deref(), TaskAction::Pause, json).await,
        TaskCommand::Resume {
            all: true, host, ..
        } => every(config, host.as_deref(), TaskAction::Resume, json).await,
        TaskCommand::Pause { task, .. } => {
            let task = task.expect("clap requires it");
            act(config, &task, TaskAction::Pause, json, |f| {
                format!(
                    "{}: {} — what's running finishes, nothing new starts",
                    f.title,
                    word(&f.status)
                )
            })
            .await
        }
        TaskCommand::Resume { task, .. } => {
            let task = task.expect("clap requires it");
            act(config, &task, TaskAction::Resume, json, |f| {
                format!("{}: {}", f.title, word(&f.status))
            })
            .await
        }
        TaskCommand::Retry { task } => {
            act(config, &task, TaskAction::Retry, json, |f| {
                format!("{}: {}", f.title, word(&f.status))
            })
            .await
        }
        TaskCommand::Cancel { task } => {
            act(config, &task, TaskAction::Cancel, json, |f| {
                format!("{}: {}", f.title, word(&f.status))
            })
            .await
        }
        TaskCommand::OwnWorkspace { task } => {
            act(config, &task, TaskAction::OwnWorkspace, json, |f| {
                format!(
                    "{}: a workspace of its own ({}), being prepared",
                    f.title,
                    f.workspace_id
                        .as_ref()
                        .map(|w| w.to_string())
                        .unwrap_or_default()
                )
            })
            .await
        }
        TaskCommand::History { task, limit } => {
            let mut found = find_task(config, &task).await?;
            let records = found.conn.task_events(found.task.id.as_str(), None).await?;
            let skip = limit.map_or(0, |n| records.len().saturating_sub(n));
            if json {
                return output::print_json(&records[skip..]);
            }
            for r in &records[skip..] {
                println!(
                    "{}  {}",
                    r.ts.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S"),
                    r.text
                );
            }
            Ok(())
        }
        TaskCommand::Accept { task, anyway } => {
            act(
                config,
                &task,
                TaskAction::Accept {
                    override_gates: anyway,
                },
                json,
                |f| format!("{}: {}", f.title, word(&f.status)),
            )
            .await
        }
        TaskCommand::Decide {
            task,
            decision,
            deny,
            answer,
            answers,
        } => {
            let found = find_task(config, &task).await?;
            let d = {
                let hits: Vec<_> = found
                    .task
                    .decisions
                    .iter()
                    .filter(|d| d.id.as_str() == decision)
                    .chain(found.task.decisions.iter().filter(|d| {
                        d.id.as_str() != decision && d.id.as_str().starts_with(&decision)
                    }))
                    .collect();
                match hits.as_slice() {
                    [] => bail!("{} has no decision `{decision}`", found.task.title),
                    [d, ..] if d.id.as_str() == decision => (*d).clone(),
                    [d] => (*d).clone(),
                    _ => bail!("`{decision}` matches several decisions; give more of its id"),
                }
            };
            if d.status != DecisionStatus::Pending {
                bail!("{} is already {}", d.id, word(&d.status));
            }
            let answers = if answers.is_empty() {
                None
            } else {
                let mut map = BTreeMap::new();
                for a in answers {
                    let (q, v) = a
                        .split_once('=')
                        .with_context(|| format!("expected QUESTION=ANSWER, got `{a}`"))?;
                    map.insert(q.trim().to_owned(), v.trim().to_owned());
                }
                Some(map)
            };
            let action = TaskAction::Decide {
                decision_id: d.id.clone(),
                approve: !deny,
                answer,
                answers,
            };
            let summary = d.summary.clone();
            act_found(found, action, json, |_| {
                format!("{}: {summary}", if deny { "denied" } else { "answered" })
            })
            .await
        }
    }
}

async fn act(
    config: &Config,
    task: &str,
    action: TaskAction,
    json: bool,
    say: impl FnOnce(&Task) -> String,
) -> Result<()> {
    let found = find_task(config, task).await?;
    act_found(found, action, json, say).await
}

/// Pause every task at work, or resume every paused one, on every host
/// (or one): one control for all the agents at once (D-081).
async fn every(config: &Config, host: Option<&str>, action: TaskAction, json: bool) -> Result<()> {
    use otter_core::task::TaskStatus;
    let pausing = action == TaskAction::Pause;
    let (found, failures) = all_tasks(config, host).await?;
    let mut done = Vec::new();
    let mut errors = Vec::new();
    for (host, mut conn, list) in found {
        for f in list {
            let wanted = if pausing {
                f.status.is_active() || f.status == TaskStatus::Blocked
            } else {
                f.status == TaskStatus::Paused
            };
            if !wanted {
                continue;
            }
            match conn
                .task_act(&command_id(), f.id.as_str(), action.clone())
                .await
            {
                Ok(after) => done.push((host.name.clone(), after)),
                Err(e) => errors.push(format!("{}:{}: {e}", host.name, f.title)),
            }
        }
    }
    if json {
        let v: Vec<_> = done
            .iter()
            .map(|(h, f)| serde_json::json!({"host": h, "task": f.id, "status": f.status}))
            .collect();
        output::print_json(&v)?;
    } else if done.is_empty() {
        println!(
            "{}",
            if pausing {
                "nothing is at work"
            } else {
                "nothing is paused"
            }
        );
    } else {
        for (h, f) in &done {
            println!("{h}:{}: {}", f.title, word(&f.status));
        }
    }
    for e in failures.iter().chain(&errors) {
        eprintln!("otter: {e}");
    }
    if !errors.is_empty() {
        bail!(
            "{} task(s) couldn't be {}",
            errors.len(),
            if pausing { "paused" } else { "resumed" }
        );
    }
    Ok(())
}

async fn act_found(
    mut found: FoundTask,
    action: TaskAction,
    json: bool,
    say: impl FnOnce(&Task) -> String,
) -> Result<()> {
    let id = found.task.id.to_string();
    let f = found.conn.task_act(&command_id(), &id, action).await?;
    if json {
        return output::print_json(&f);
    }
    println!("{}", say(&f));
    Ok(())
}

async fn list(config: &Config, json: bool) -> Result<()> {
    let (found, failures) = all_tasks(config, None).await?;
    if json {
        let all: Vec<serde_json::Value> = found
            .iter()
            .flat_map(|(h, _, list)| {
                list.iter().map(|f| {
                    let mut v = serde_json::to_value(f).unwrap_or_default();
                    v["host"] = h.name.clone().into();
                    v
                })
            })
            .collect();
        return output::print_json(&all);
    }
    let mut rows: Vec<(chrono::DateTime<chrono::Utc>, Vec<String>)> = Vec::new();
    for (host, _, list) in &found {
        for f in list {
            let waiting = f
                .decisions
                .iter()
                .filter(|d| d.status == DecisionStatus::Pending)
                .count();
            let mut status = word(&f.status);
            if waiting > 0 {
                status.push_str(&format!(" · {waiting} waiting on you"));
            }
            rows.push((
                f.updated_at,
                vec![
                    format!("{}:{}", host.name, f.id),
                    one_line(&f.title, 48),
                    status,
                    ago(&f.updated_at),
                ],
            ));
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    if rows.is_empty() {
        println!("no tasks yet; `otter task new <title>` writes one down");
    } else {
        output::table(
            &["TASK", "TITLE", "STATUS", "UPDATED"],
            rows.into_iter().map(|(_, r)| r).collect(),
        );
    }
    for f in failures {
        eprintln!("could not reach {f}");
    }
    Ok(())
}

fn show(host: &str, f: &Task) {
    println!("{}  ({host}:{})", f.title, f.id);
    let mut status = word(&f.status);
    if let Some(r) = &f.status_reason {
        status.push_str(&format!(" — {r}"));
    }
    println!("status: {status}");
    if let Some(next) = &f.next {
        println!("next: {next}");
    }
    if let Some(ws) = &f.workspace_id {
        println!("workspace: {ws}");
    }
    let pending: Vec<_> = f
        .decisions
        .iter()
        .filter(|d| d.status == DecisionStatus::Pending)
        .collect();
    if !pending.is_empty() {
        println!("\nWaiting on you:");
        for d in pending {
            print!("{}", waiting(d));
        }
        println!(
            "  answer with `otter task decide {} <decision> [--deny | --answer …]`",
            f.id
        );
    }
    if !f.steps.is_empty() {
        println!("\nTasks:");
        for t in &f.steps {
            println!("  [{}] {}", word(&t.status), one_line(&t.title, 70));
            // Why it failed (D-076).
            if let (otter_core::task::StepStatus::Failed, Some(e)) = (t.status, &t.last_error) {
                println!("      last error: {}", one_line(e, 90));
            }
        }
    }
    if let Some(run) = f.runs.last() {
        let conv = run
            .conversation_id
            .as_ref()
            .map(|c| format!(" · conversation {c}"))
            .unwrap_or_default();
        println!(
            "\nLatest run: {} {}{conv}",
            word(&run.state),
            ago(&run.started_at)
        );
        // A live run: when its agent was last heard from (D-141).
        if run.tools_running > 0 {
            println!("  running a tool");
        } else if let Some(at) = &run.heard_at {
            println!("  last heard from {}", ago(at));
        }
        // Each line's time when the daemon kept it (D-141).
        let timed = run.activity_at.len() == run.activity.len();
        for (i, a) in run.activity.iter().enumerate().rev().take(3).rev() {
            match run.activity_at.get(i).filter(|_| timed) {
                Some(at) => println!("  {}  {}", one_line(a, 80), ago(at)),
                None => println!("  {}", one_line(a, 90)),
            }
        }
    }
    let recent: Vec<_> = f.messages.iter().rev().take(5).rev().collect();
    if !recent.is_empty() {
        println!("\nLatest messages:");
        for m in recent {
            println!("  {:<6} {}", word(&m.role), one_line(&m.text, 90));
        }
    }
}

// ---------------------------------------------------------------------------
// The coding runtime and its conversations
// ---------------------------------------------------------------------------

pub async fn runtime_status(config: &Config, host: Option<String>, json: bool) -> Result<()> {
    let mut all = Vec::new();
    for entry in hosts(config, host.as_deref())? {
        let result = async {
            let mut conn = connect(config, &entry).await?;
            Ok::<_, anyhow::Error>(conn.runtime_capabilities().await?)
        }
        .await;
        all.push((entry.name.clone(), result));
    }
    if json {
        let v: BTreeMap<_, _> = all
            .into_iter()
            .map(|(h, r)| {
                let v = match r {
                    Ok(c) => serde_json::to_value(c).unwrap_or_default(),
                    Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
                };
                (h, v)
            })
            .collect();
        return output::print_json(&v);
    }
    for (host, result) in all {
        match result {
            Err(e) => println!("{host}: unreachable — {e:#}"),
            Ok(c) => {
                let ready = if c.available { "ready" } else { "not ready" };
                let tested = c
                    .tested_version
                    .map(|v| format!(", tested on {v}"))
                    .unwrap_or_default();
                println!("{host}: {} via {} — {ready}{tested}", c.provider, c.backend);
                for n in &c.notes {
                    println!("  {n}");
                }
                let f = &c.features;
                let can: Vec<&str> = [
                    (f.interrupt_turn, "interrupt"),
                    (f.redirect, "redirect"),
                    (f.pause, "pause"),
                    (f.questions, "questions"),
                    (f.permission_requests, "permission requests"),
                    (f.resume, "resume"),
                    (f.tool_results, "tool results"),
                ]
                .into_iter()
                .filter_map(|(on, name)| on.then_some(name))
                .collect();
                if !can.is_empty() {
                    println!("  can: {}", can.join(", "));
                }
            }
        }
    }
    Ok(())
}

/// `otter runtime test` (D-124): each host's coding agent, started for one
/// reply. Fails (exit 1) when one isn't ready.
pub async fn runtime_test(
    config: &Config,
    host: Option<String>,
    backend: Option<String>,
    model: Option<String>,
    json: bool,
) -> Result<()> {
    use otter_protocol::conversation::StepState;
    let mut all = Vec::new();
    for entry in hosts(config, host.as_deref())? {
        if !json {
            println!("{}: testing the coding agent…", entry.name);
        }
        let result = async {
            let mut conn = connect(config, &entry).await?;
            Ok::<_, anyhow::Error>(
                conn.runtime_test(backend.as_deref(), model.as_deref())
                    .await?,
            )
        }
        .await;
        all.push((entry.name.clone(), result));
    }
    let failed = all
        .iter()
        .filter(|(_, r)| !r.as_ref().is_ok_and(|t| t.ready))
        .count();
    if json {
        let v: BTreeMap<_, _> = all
            .into_iter()
            .map(|(h, r)| {
                let v = match r {
                    Ok(t) => serde_json::to_value(t).unwrap_or_default(),
                    Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
                };
                (h, v)
            })
            .collect();
        output::print_json(&v)?;
    } else {
        for (host, result) in all {
            match result {
                Err(e) => println!("{host}: not tested — {e:#}"),
                Ok(t) => {
                    let ready = if t.ready { "ready" } else { "not ready" };
                    println!("{host}: claude via {} — {ready}", t.backend);
                    for s in &t.steps {
                        let mark = match s.state {
                            StepState::Passed => "✓",
                            StepState::Failed => "✗",
                            StepState::Skipped => "–",
                        };
                        println!("  {mark} {}: {}", s.name, s.detail);
                    }
                }
            }
        }
    }
    if failed > 0 {
        bail!("the coding agent isn't ready on {failed} host(s)");
    }
    Ok(())
}

pub async fn conversation_command(
    config: &Config,
    cmd: ConversationCommand,
    json: bool,
) -> Result<()> {
    match cmd {
        ConversationCommand::List { task, host } => {
            let mut rows = Vec::new();
            let mut all = Vec::new();
            let sources: Vec<(HostEntry, Connection, Option<String>)> = match task {
                Some(f) => {
                    let found = find_task(config, &f).await?;
                    let id = found.task.id.to_string();
                    vec![(found.host, found.conn, Some(id))]
                }
                None => {
                    let mut v = Vec::new();
                    for entry in hosts(config, host.as_deref())? {
                        let conn = connect(config, &entry).await?;
                        v.push((entry, conn, None));
                    }
                    v
                }
            };
            for (entry, mut conn, task) in sources {
                for c in conn.conversation_list(task.as_deref()).await? {
                    let conv = &c.conversation;
                    rows.push(vec![
                        format!("{}:{}", entry.name, conv.id),
                        conv.task_id
                            .as_ref()
                            .map(|f| f.to_string())
                            .unwrap_or_default(),
                        status_of(&c),
                        conv.turns.len().to_string(),
                        ago(&conv.updated_at),
                    ]);
                    all.push((entry.name.clone(), c));
                }
            }
            if json {
                let v: Vec<serde_json::Value> = all
                    .into_iter()
                    .map(|(h, c)| {
                        let mut v = serde_json::to_value(c).unwrap_or_default();
                        v["host"] = h.into();
                        v
                    })
                    .collect();
                return output::print_json(&v);
            }
            if rows.is_empty() {
                println!("no conversations yet");
            } else {
                output::table(
                    &["CONVERSATION", "TASK", "STATUS", "TURNS", "UPDATED"],
                    rows,
                );
            }
            Ok(())
        }
        ConversationCommand::Show { conversation } => {
            let (host, _, c) = find_conversation(config, &conversation).await?;
            if json {
                return output::print_json(&c);
            }
            show_conversation(&host.name, &c);
            Ok(())
        }
        ConversationCommand::History {
            conversation,
            after,
            limit,
        } => {
            let (_, mut conn, c) = find_conversation(config, &conversation).await?;
            let after = match after {
                None => None,
                Some(a) => {
                    let (log_id, seq) = a
                        .rsplit_once(':')
                        .with_context(|| format!("expected log_id:seq, got `{a}`"))?;
                    Some(HistoryCursor {
                        log_id: log_id.to_owned(),
                        seq: seq.parse().with_context(|| format!("bad seq in `{a}`"))?,
                    })
                }
            };
            let page = conn
                .conversation_history(HistoryQuery {
                    conversation: c.conversation.id.to_string(),
                    after,
                    limit,
                })
                .await?;
            if json {
                return output::print_json(&page);
            }
            for r in &page.records {
                let seq = r["seq"].as_u64().unwrap_or_default();
                let at = r["at"].as_str().unwrap_or("");
                let what = match r["kind"].as_str() {
                    Some("create") => "created".to_owned(),
                    _ => r["op"]["type"]
                        .as_str()
                        .or_else(|| r["type"].as_str())
                        .unwrap_or("change")
                        .replace('_', " "),
                };
                let recovery = if r["recovery"].as_bool() == Some(true) {
                    " (recovery)"
                } else {
                    ""
                };
                println!("{seq:>5}  {at}  {what}{recovery}");
            }
            if let Some(next) = page.next {
                println!("more: --after {}:{}", next.log_id, next.seq);
            }
            Ok(())
        }
    }
}

/// What a conversation is doing, else why not.
fn status_of(c: &ConversationView) -> String {
    let conv = &c.conversation;
    if let Some(r) = &c.read_only {
        return format!("read-only: {r}");
    }
    if let Some(t) = conv
        .turns
        .iter()
        .rev()
        .find(|t| !t.is_over() && t.state != TurnState::Queued)
    {
        return turn_word(t);
    }
    let queued = conv
        .turns
        .iter()
        .filter(|t| t.state == TurnState::Queued)
        .count();
    let lifecycle = word(&conv.lifecycle);
    match (lifecycle.as_str(), queued) {
        ("open", 0) => conv
            .turns
            .last()
            .map(turn_word)
            .unwrap_or_else(|| "not started".into()),
        ("open", n) => format!("{n} queued"),
        (l, 0) => l.to_owned(),
        (l, n) => format!("{l}, {n} queued"),
    }
}

/// A turn's state in words: accepted is not delivered, finished is not done.
fn turn_word(t: &Turn) -> String {
    let mut s = match t.outcome {
        Some(o) => word(&o),
        None => word(&t.state),
    };
    match t.delivery {
        Delivery::Queued if t.outcome.is_some() => s.push_str(" (never sent)"),
        Delivery::Unknown => s.push_str(" (delivery unknown)"),
        _ => {}
    }
    if t.redirect {
        s.push_str(" · redirect");
    }
    // What it means and what to do, not a bare category (D-076).
    if let Some(e) = t.error {
        let x = e.explain();
        s.push_str(&format!(" · {} {}", x.what, x.next));
    }
    s
}

fn show_conversation(host: &str, c: &ConversationView) {
    let conv = &c.conversation;
    println!("{host}:{}  {} via {}", conv.id, conv.provider, conv.backend);
    let model = conv.model.as_deref().unwrap_or("default model");
    println!(
        "{} · run {} · {model}{}",
        status_of(c),
        conv.generation,
        if c.resumable { " · resumable" } else { "" }
    );
    for t in &conv.turns {
        println!(
            "\n▸ {} [{}] {}",
            t.id,
            turn_word(t),
            one_line(&t.text(), 80)
        );
        if let Some(r) = &t.reason {
            println!("    {r}");
        }
        for tool in conv.tools.iter().filter(|x| x.turn_id == t.id) {
            println!(
                "    {:<8} {:<20} {}",
                tool.name,
                word(&tool.status),
                one_line(&tool.summary, 60)
            );
        }
        for m in conv.messages.iter().filter(|m| m.turn_id == t.id) {
            let cut = if word(&m.lifecycle) == "interrupted" {
                " (cut off)"
            } else {
                ""
            };
            println!("    “{}”{cut}", one_line(&m.text(), 90));
        }
    }
    let waiting: Vec<_> = conv
        .interactions
        .iter()
        .filter(|i| word(&i.status) == "pending")
        .collect();
    if !waiting.is_empty() {
        println!("\n{} waiting on you — see `otter task show`", waiting.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_split_off_but_titles_keep_their_colons() {
        let known = |h: &str| h == "dev";
        assert_eq!(split_host("dev:feat_1", known), (Some("dev"), "feat_1"));
        assert_eq!(split_host("feat_1", known), (None, "feat_1"));
        assert_eq!(
            split_host("Bug: the CSV export", known),
            (None, "Bug: the CSV export")
        );
    }

    #[test]
    fn a_decision_waiting_on_you_says_why_with_the_whole_command() {
        use otter_core::task::{DecisionKind, Risk};
        let d = DecisionRequest {
            id: otter_core::DecisionId::generate(),
            step_id: None,
            run_id: None,
            kind: DecisionKind::ToolPermission,
            risk: Risk::High,
            summary: "Run `python3 - <<'E' import re p='a.py' …`".into(),
            detail: Some("deletes or destroys something (`rm`)".into()),
            command: Some("python3 - <<'E'\nimport re\nE\nrm -rf /opt/shared".into()),
            options: vec![],
            status: DecisionStatus::Pending,
            decided_by: None,
            answer: None,
            rationale: None,
            created_at: chrono::Utc::now(),
            decided_at: None,
            user_only: true,
            grant: None,
        };
        let shown = waiting(&d);
        assert!(
            shown.contains(
                "(tool permission)\n      deletes or destroys something (`rm`)\n      the whole command:\n        python3 - <<'E'\n        import re\n        E\n        rm -rf /opt/shared\n"
            ),
            "{shown}"
        );
        // A decision with neither is its one line, as before.
        let plain = DecisionRequest {
            detail: None,
            command: None,
            ..d
        };
        assert_eq!(waiting(&plain).lines().count(), 1);
    }
}
