//! Features and the coding agent's conversations, from the terminal (D-043,
//! D-055–D-059): the same commands and state the desktop app uses.
//!
//! Features are addressed like workspaces: `[host:]feature`, where `feature`
//! is an id (`ft_…`), a title, or the start of one title.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use otter_client::Connection;
use otter_core::conversation::{Delivery, Turn, TurnState};
use otter_core::feature::{DecisionStatus, Feature, FeatureAction};
use otter_protocol::conversation::{ConversationView, HistoryCursor, HistoryQuery};
use otter_protocol::feature::FeatureCreate;
use tokio::task::JoinSet;

use crate::config::{Config, HostEntry};
use crate::output;
use crate::target::connect;

#[derive(Subcommand)]
pub enum FeatureCommand {
    /// Features on every host, newest first.
    #[command(visible_alias = "ls")]
    List,
    /// A feature: where it stands, what waits on you, the latest messages.
    Show {
        feature: String,
    },
    /// Write down a feature (a draft until `otter feature start`).
    New {
        title: String,
        /// What should be built, in your words (default: the title).
        #[arg(long)]
        request: Option<String>,
        /// The workspace to do the work in.
        #[arg(long)]
        workspace: Option<String>,
        /// The host (default: the default host, else the only one).
        #[arg(long)]
        host: Option<String>,
    },
    /// Hand a draft to Otter.
    Start {
        feature: String,
    },
    /// A message to Otter, which passes on to the coding agent what it should
    /// know — when it's useful, not necessarily now.
    #[command(visible_alias = "send")]
    Message {
        feature: String,
        text: String,
    },
    /// Stop the coding agent's current work now and have it do this next.
    Redirect {
        feature: String,
        text: String,
    },
    /// Stop the coding agent's current work, and wait for you.
    Interrupt {
        feature: String,
    },
    /// Let the current work finish, then hold.
    Pause {
        feature: String,
    },
    Resume {
        feature: String,
    },
    Cancel {
        feature: String,
    },
    /// Answer what a feature waits on you for: approve (default) or deny, or
    /// answer a question.
    Decide {
        feature: String,
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
    /// The coding agent's conversations (a feature's, if given).
    #[command(visible_alias = "ls")]
    List {
        /// `[host:]feature`
        #[arg(long)]
        feature: Option<String>,
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

/// Every host's features (skipping hosts that can't be reached, said so).
async fn all_features(
    config: &Config,
    host: Option<&str>,
) -> Result<(Vec<(HostEntry, Connection, Vec<Feature>)>, Vec<String>)> {
    let mut tasks = JoinSet::new();
    for host in hosts(config, host)? {
        let transport = config.transport(&host);
        tasks.spawn(async move {
            let result = async {
                let mut conn = Connection::connect(&transport).await?;
                let list = conn.feature_list().await?;
                Ok::<_, otter_client::ClientError>((conn, list))
            }
            .await;
            (host, result)
        });
    }
    let mut found = Vec::new();
    let mut failures = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let (host, result) = joined?;
        match result {
            Ok((conn, list)) => found.push((host, conn, list)),
            Err(e) => failures.push(format!("{}: {e}", host.name)),
        }
    }
    found.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    Ok((found, failures))
}

/// A feature with a connection to its host.
pub struct FoundFeature {
    pub host: HostEntry,
    pub conn: Connection,
    pub feature: Feature,
}

/// Which of `list` `name` means: its id, its title, or the start of one
/// title (ignoring case).
fn pick<'a>(list: &'a [Feature], name: &str) -> Vec<&'a Feature> {
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

pub async fn find_feature(config: &Config, target: &str) -> Result<FoundFeature> {
    let (host, name) = split(config, target);
    if name.is_empty() {
        bail!("expected [host:]feature, got `{target}`");
    }
    let (found, failures) = all_features(config, host).await?;
    let mut matches = Vec::new();
    for (host, conn, list) in found {
        let picked: Vec<Feature> = pick(&list, name).into_iter().cloned().collect();
        if picked.len() == 1 {
            let feature = picked.into_iter().next().unwrap();
            matches.push(FoundFeature {
                host,
                conn,
                feature,
            });
        } else if picked.len() > 1 {
            let names: Vec<String> = picked
                .iter()
                .map(|f| format!("{} ({})", f.title, f.id))
                .collect();
            bail!(
                "`{name}` matches several features on {}: {}",
                host.name,
                names.join(", ")
            );
        }
    }
    match matches.len() {
        1 => Ok(matches.pop().unwrap()),
        0 => {
            let mut msg = format!("no feature `{name}`");
            if !failures.is_empty() {
                msg.push_str(&format!(" (could not reach: {})", failures.join("; ")));
            }
            bail!(msg)
        }
        _ => {
            let options: Vec<String> = matches
                .iter()
                .map(|m| format!("{}:{}", m.host.name, m.feature.id))
                .collect();
            bail!(
                "`{name}` is a feature on several hosts; use one of: {}",
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
fn command_id() -> String {
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
// Features
// ---------------------------------------------------------------------------

pub async fn feature_command(config: &Config, cmd: FeatureCommand, json: bool) -> Result<()> {
    match cmd {
        FeatureCommand::List => list(config, json).await,
        FeatureCommand::Show { feature } => {
            let found = find_feature(config, &feature).await?;
            if json {
                return output::print_json(&found.feature);
            }
            show(&found.host.name, &found.feature);
            Ok(())
        }
        FeatureCommand::New {
            title,
            request,
            workspace,
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
                .feature_create(FeatureCreate {
                    command_id: command_id(),
                    request: request.unwrap_or_else(|| title.clone()),
                    title,
                    workspace,
                })
                .await?;
            if json {
                return output::print_json(&f);
            }
            println!(
                "feature {} ({}) on {} — a draft; `otter feature start {}` hands it to Otter",
                f.title, f.id, entry.name, f.id
            );
            Ok(())
        }
        FeatureCommand::Message { feature, text } => {
            let mut found = find_feature(config, &feature).await?;
            let id = found.feature.id.to_string();
            let f = found.conn.feature_send(&command_id(), &id, &text).await?;
            if json {
                return output::print_json(&f);
            }
            println!(
                "Otter has your message for {}. It passes on to the coding agent what it should \
                 know, when it's useful; `otter conversation list --feature {}` shows when it's \
                 delivered.",
                f.title, f.id
            );
            Ok(())
        }
        FeatureCommand::Redirect { feature, text } => {
            act(
                config,
                &feature,
                FeatureAction::Redirect { text },
                json,
                |f| {
                    format!(
                        "{}: the coding agent's current work is stopping; yours goes next ({})",
                        f.title,
                        word(&f.status)
                    )
                },
            )
            .await
        }
        FeatureCommand::Interrupt { feature } => {
            act(config, &feature, FeatureAction::Interrupt, json, |f| {
                format!(
                    "{}: the coding agent's current work is stopping; then it waits for you \
                     (`otter feature resume`)",
                    f.title
                )
            })
            .await
        }
        FeatureCommand::Start { feature } => {
            act(config, &feature, FeatureAction::Start, json, |f| {
                format!("{}: {}", f.title, word(&f.status))
            })
            .await
        }
        FeatureCommand::Pause { feature } => {
            act(config, &feature, FeatureAction::Pause, json, |f| {
                format!(
                    "{}: {} — what's running finishes, nothing new starts",
                    f.title,
                    word(&f.status)
                )
            })
            .await
        }
        FeatureCommand::Resume { feature } => {
            act(config, &feature, FeatureAction::Resume, json, |f| {
                format!("{}: {}", f.title, word(&f.status))
            })
            .await
        }
        FeatureCommand::Cancel { feature } => {
            act(config, &feature, FeatureAction::Cancel, json, |f| {
                format!("{}: {}", f.title, word(&f.status))
            })
            .await
        }
        FeatureCommand::Decide {
            feature,
            decision,
            deny,
            answer,
            answers,
        } => {
            let found = find_feature(config, &feature).await?;
            let d = {
                let hits: Vec<_> = found
                    .feature
                    .decisions
                    .iter()
                    .filter(|d| d.id.as_str() == decision)
                    .chain(found.feature.decisions.iter().filter(|d| {
                        d.id.as_str() != decision && d.id.as_str().starts_with(&decision)
                    }))
                    .collect();
                match hits.as_slice() {
                    [] => bail!("{} has no decision `{decision}`", found.feature.title),
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
            let action = FeatureAction::Decide {
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
    feature: &str,
    action: FeatureAction,
    json: bool,
    say: impl FnOnce(&Feature) -> String,
) -> Result<()> {
    let found = find_feature(config, feature).await?;
    act_found(found, action, json, say).await
}

async fn act_found(
    mut found: FoundFeature,
    action: FeatureAction,
    json: bool,
    say: impl FnOnce(&Feature) -> String,
) -> Result<()> {
    let id = found.feature.id.to_string();
    let f = found.conn.feature_act(&command_id(), &id, action).await?;
    if json {
        return output::print_json(&f);
    }
    println!("{}", say(&f));
    Ok(())
}

async fn list(config: &Config, json: bool) -> Result<()> {
    let (found, failures) = all_features(config, None).await?;
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
        println!("no features yet; `otter feature new <title>` writes one down");
    } else {
        output::table(
            &["FEATURE", "TITLE", "STATUS", "UPDATED"],
            rows.into_iter().map(|(_, r)| r).collect(),
        );
    }
    for f in failures {
        eprintln!("could not reach {f}");
    }
    Ok(())
}

fn show(host: &str, f: &Feature) {
    println!("{}  ({host}:{})", f.title, f.id);
    let mut status = word(&f.status);
    if let Some(r) = &f.status_reason {
        status.push_str(&format!(" — {r}"));
    }
    println!("status: {status}");
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
            println!("  {}  {} ({})", d.id, d.summary, word(&d.kind));
            if !d.options.is_empty() {
                println!("      options: {}", d.options.join(" | "));
            }
        }
        println!(
            "  answer with `otter feature decide {} <decision> [--deny | --answer …]`",
            f.id
        );
    }
    if !f.tasks.is_empty() {
        println!("\nTasks:");
        for t in &f.tasks {
            println!("  [{}] {}", word(&t.status), one_line(&t.title, 70));
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
        for a in run.activity.iter().rev().take(3).rev() {
            println!("  {}", one_line(a, 90));
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

pub async fn conversation_command(
    config: &Config,
    cmd: ConversationCommand,
    json: bool,
) -> Result<()> {
    match cmd {
        ConversationCommand::List { feature, host } => {
            let mut rows = Vec::new();
            let mut all = Vec::new();
            let sources: Vec<(HostEntry, Connection, Option<String>)> = match feature {
                Some(f) => {
                    let found = find_feature(config, &f).await?;
                    let id = found.feature.id.to_string();
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
            for (entry, mut conn, feature) in sources {
                for c in conn.conversation_list(feature.as_deref()).await? {
                    let conv = &c.conversation;
                    rows.push(vec![
                        format!("{}:{}", entry.name, conv.id),
                        conv.feature_id
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
                    &["CONVERSATION", "FEATURE", "STATUS", "TURNS", "UPDATED"],
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
    if let Some(e) = t.error {
        s.push_str(&format!(" · {}", word(&e)));
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
        println!(
            "\n{} waiting on you — see `otter feature show`",
            waiting.len()
        );
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
}
