//! `otter` — the Otter control-plane CLI.
//!
//! Talks to `otterd` on each registered host (over SSH) and presents
//! workspaces independently of where they run.

mod attach;
mod dashboard;
mod output;
mod target;

use std::collections::HashMap;
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use otter_client::Connection;
pub(crate) use otter_client::config;
use otter_core::{Brief, EnvironmentKind, SessionKind, Workspace, WorkspaceSource, WorkspaceState};
use otter_protocol::{SessionCreate, SessionSpec, SourceSpec, WorkspaceCreate};
use tokio::task::JoinSet;

use crate::config::{Config, DEFAULT_REMOTE_OTTERD, HostEntry, HostTransport};
use crate::target::{Target, connect, find};

#[derive(Parser)]
#[command(
    name = "otter",
    version,
    about = "Manage Otter workspaces across hosts"
)]
struct Cli {
    /// Print machine-readable JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage hosts.
    #[command(subcommand)]
    Host(HostCommand),
    /// Manage workspaces.
    #[command(subcommand, visible_alias = "ws")]
    Workspace(WorkspaceCommand),
    /// Manage sessions.
    #[command(subcommand)]
    Session(SessionCommand),

    /// What needs you, what's working, what's done — across all hosts.
    #[command(visible_alias = "list")]
    Ls {
        /// Keep the view updated as things change.
        #[arg(short, long)]
        watch: bool,
        /// Plain table instead of the grouped view.
        #[arg(long)]
        table: bool,
        /// Include archived workspaces.
        #[arg(short, long)]
        archived: bool,
    },
    /// Mark what a workspace (or one session) was asking for as handled.
    Ack {
        /// `[host:]workspace[/session]`
        target: String,
    },
    /// Create a workspace (= `workspace create`).
    New(WorkspaceCreateArgs),
    /// Start a new session in a workspace (= `session create`).
    Start(SessionCreateArgs),
    /// Attach your terminal to a session. Detach with Ctrl-].
    Attach {
        /// `[host:]workspace[/session]` (default: first session).
        target: String,
    },
    /// Print a session's screen and scrollback.
    Logs {
        /// `[host:]workspace[/session]`
        target: String,
        /// Scrollback lines above the screen (default: all).
        #[arg(short = 'n', long)]
        lines: Option<u32>,
    },
    /// Type text into a session.
    Send {
        /// `[host:]workspace[/session]`
        target: String,
        text: String,
        /// Don't press Enter after the text.
        #[arg(long)]
        no_enter: bool,
    },
    /// Show recent events.
    Events {
        /// Only this host.
        #[arg(long)]
        host: Option<String>,
        /// Number of recent events per host.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
        /// Keep streaming new events.
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Subcommand)]
enum HostCommand {
    /// Register a host. Uses your SSH config: if `ssh <destination>` works,
    /// otter can connect.
    Add {
        name: String,
        /// SSH destination (default: the host name).
        #[arg(long, conflicts_with = "local")]
        ssh: Option<String>,
        /// Extra argument for ssh (repeatable), e.g. `--ssh-arg=-p2222`.
        #[arg(long = "ssh-arg", allow_hyphen_values = true)]
        ssh_args: Vec<String>,
        /// This machine, without SSH.
        #[arg(long)]
        local: bool,
        /// Path of otterd on the host.
        #[arg(long)]
        otterd_path: Option<String>,
        /// Daemon state directory on the host (default ~/.otter).
        #[arg(long)]
        home: Option<String>,
        /// Make this the default host.
        #[arg(long)]
        default: bool,
        /// Register without checking connectivity.
        #[arg(long)]
        no_check: bool,
    },
    /// List registered hosts.
    #[command(visible_alias = "ls")]
    List,
    /// Forget a host (does not touch anything on it).
    #[command(visible_alias = "rm")]
    Remove { name: String },
    /// Set the default host.
    Default { name: String },
    /// Show daemon status and capabilities.
    Status { name: Option<String> },
    /// Install (or update) otterd and otter on a host from the release
    /// matching this otter, then restart its daemon. Sessions keep running.
    Install { name: String },
    /// Stop the daemon on a host. Sessions keep running; the next command
    /// starts it again.
    Shutdown { name: String },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Create a workspace with a shell session.
    Create(WorkspaceCreateArgs),
    /// List workspaces on all hosts.
    #[command(visible_alias = "ls")]
    List {
        /// Include archived workspaces.
        #[arg(short, long)]
        archived: bool,
    },
    /// Show a workspace and its sessions.
    Show { target: String },
    /// Retry a failed workspace, or reload its environment (e.g. after
    /// editing .envrc; restart sessions to pick it up).
    Prepare { target: String },
    /// Put a workspace away: stop its sessions and clear what it asks for,
    /// keeping its files, branch and sessions. Hidden from `otter ls`.
    Archive { target: String },
    /// Bring an archived workspace back. Its sessions stay stopped until you
    /// restart them.
    #[command(visible_alias = "restore")]
    Unarchive { target: String },
    /// Show or edit why a workspace exists: its title, goal, description,
    /// constraints, references and decisions.
    Brief(BriefArgs),
    /// Stop all sessions and delete the workspace and its directory. Git
    /// branches are kept.
    #[command(visible_alias = "rm")]
    Delete {
        target: String,
        /// Don't ask for confirmation.
        #[arg(short, long)]
        yes: bool,
        /// Discard uncommitted changes in a Git worktree.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args)]
struct BriefArgs {
    target: String,
    /// Short title (`""` clears it).
    #[arg(long)]
    title: Option<String>,
    /// Why this workspace exists (`""` clears it).
    #[arg(long)]
    goal: Option<String>,
    /// Longer description (`""` clears it).
    #[arg(long)]
    description: Option<String>,
    /// Add a constraint (repeatable).
    #[arg(long = "constraint", value_name = "TEXT")]
    constraints: Vec<String>,
    /// Add a reference, e.g. an issue (repeatable).
    #[arg(long = "reference", value_name = "TEXT")]
    references: Vec<String>,
    /// Add a decision (repeatable).
    #[arg(long = "decision", value_name = "TEXT")]
    decisions: Vec<String>,
    /// Start from an empty brief instead of adding to the current one.
    #[arg(long)]
    clear: bool,
}

impl BriefArgs {
    fn edits(&self) -> bool {
        self.clear
            || self.title.is_some()
            || self.goal.is_some()
            || self.description.is_some()
            || !self.constraints.is_empty()
            || !self.references.is_empty()
            || !self.decisions.is_empty()
    }

    fn apply(self, current: &Brief) -> Brief {
        let mut b = if self.clear {
            Brief::default()
        } else {
            current.clone()
        };
        if let Some(t) = self.title {
            b.title = Some(t);
        }
        if let Some(g) = self.goal {
            b.goal = Some(g);
        }
        if let Some(d) = self.description {
            b.description = Some(d);
        }
        b.constraints.extend(self.constraints);
        b.references.extend(self.references);
        b.decisions.extend(self.decisions);
        b
    }
}

#[derive(Args)]
struct WorkspaceCreateArgs {
    name: String,
    /// Host to create it on.
    #[arg(long)]
    host: Option<String>,
    /// Why this workspace exists.
    #[arg(long)]
    goal: Option<String>,
    /// Git repository to work in (cloned once per host, one worktree per
    /// workspace).
    #[arg(long, conflicts_with = "dir")]
    repo: Option<String>,
    /// Branch: checked out if it exists, otherwise created (default:
    /// otterd/<name>).
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    /// Revision to branch from (default: the remote's default branch).
    #[arg(long, requires = "repo")]
    base: Option<String>,
    /// Use an existing directory on the host instead (never deleted by otterd).
    #[arg(long)]
    dir: Option<String>,
    /// Don't start a shell.
    #[arg(long)]
    no_shell: bool,
    /// Don't start a coding agent (by default a workspace gets Codex and a
    /// shell).
    #[arg(long)]
    no_agent: bool,
    /// Which coding agent to start: `codex` (default) or `claude` (Claude
    /// Code).
    #[arg(
        long,
        value_name = "PROVIDER",
        default_value = "codex",
        conflicts_with = "no_agent"
    )]
    agent: String,
    /// Initial prompt for the agent.
    #[arg(long, conflicts_with = "no_agent")]
    prompt: Option<String>,
    /// Return immediately instead of waiting until the workspace is ready.
    #[arg(long)]
    no_wait: bool,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Start a new session in a workspace.
    #[command(visible_alias = "start")]
    Create(SessionCreateArgs),
    /// Stop a session's process.
    Stop { target: String },
    /// Start a fresh process for a session.
    Restart { target: String },
    /// Stop a session and remove it from its workspace.
    #[command(visible_alias = "rm")]
    Delete { target: String },
}

#[derive(Args)]
struct SessionCreateArgs {
    /// `[host:]workspace`
    workspace: String,
    /// Session name (default: derived from the command or agent).
    #[arg(long)]
    name: Option<String>,
    /// terminal (interactive, default), service (long-running) or task (runs
    /// to completion).
    #[arg(long, default_value = "terminal", conflicts_with = "agent")]
    kind: SessionKind,
    /// Start a coding agent instead of a command (`--agent` alone: codex).
    #[arg(long, num_args = 0..=1, default_missing_value = "codex")]
    agent: Option<String>,
    /// Initial prompt for the agent.
    #[arg(long, requires = "agent")]
    prompt: Option<String>,
    /// Command line (default: your login shell). Everything after `--`.
    #[arg(last = true, conflicts_with = "agent")]
    command: Vec<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("otter: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let mut config = Config::load(&Config::dir_from_env()?)?;
    let json = cli.json;
    match cli.command {
        Command::Host(cmd) => host_command(&mut config, cmd, json).await,
        Command::Workspace(cmd) => workspace_command(&config, cmd, json).await,
        Command::Session(cmd) => session_command(&config, cmd, json).await,
        Command::Ls {
            watch,
            table,
            archived,
        } => {
            if watch {
                dashboard::watch(&config, archived).await
            } else if table {
                list_workspaces(&config, json, archived).await
            } else {
                dashboard::show(&config, json, archived).await
            }
        }
        Command::Ack { target } => {
            let target = Target::parse(&target)?;
            let mut found = find(&config, &target).await?;
            let session = match &target.session {
                Some(s) => Some(found.session(Some(s))?.id.to_string()),
                None => None,
            };
            let n = found
                .conn
                .attention_resolve(found.workspace.id.as_str(), session.as_deref())
                .await?;
            println!("{n} item(s) resolved");
            Ok(())
        }
        Command::New(args) => create_workspace(&config, args, json).await,
        Command::Start(args) => create_session(&config, args, json).await,
        Command::Attach { target } => {
            let target = Target::parse(&target)?;
            let found = find(&config, &target).await?;
            let session = found.session(target.session.as_deref())?;
            let label = found.label(&session);
            let ws_id = found.workspace.id.to_string();
            let host = attach::Host {
                transport: config.transport(&found.host),
                name: found.host.name.clone(),
            };
            attach::run(found.conn, host, &ws_id, session.id.as_str(), &label).await
        }
        Command::Logs { target, lines } => {
            let target = Target::parse(&target)?;
            let mut found = find(&config, &target).await?;
            let session = found.session(target.session.as_deref())?;
            let text = found
                .conn
                .session_read(found.workspace.id.as_str(), session.id.as_str(), lines)
                .await?;
            print!("{text}");
            Ok(())
        }
        Command::Send {
            target,
            text,
            no_enter,
        } => {
            let target = Target::parse(&target)?;
            let mut found = find(&config, &target).await?;
            let session = found.session(target.session.as_deref())?;
            found
                .conn
                .session_write(
                    found.workspace.id.as_str(),
                    session.id.as_str(),
                    &text,
                    !no_enter,
                )
                .await?;
            Ok(())
        }
        Command::Events {
            host,
            limit,
            follow,
        } => events(&config, host, limit, follow, json).await,
    }
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

async fn host_command(config: &mut Config, cmd: HostCommand, json: bool) -> Result<()> {
    match cmd {
        HostCommand::Add {
            name,
            ssh,
            ssh_args,
            local,
            otterd_path,
            home,
            default,
            no_check,
        } => {
            let transport = if local {
                HostTransport::Local { otterd_path, home }
            } else {
                HostTransport::Ssh {
                    destination: ssh.unwrap_or_else(|| name.clone()),
                    ssh_args,
                    otterd_path: otterd_path.unwrap_or_else(|| DEFAULT_REMOTE_OTTERD.to_owned()),
                    home,
                }
            };
            let entry = HostEntry {
                name: name.clone(),
                transport,
            };
            if !no_check {
                let mut conn = connect(config, &entry)
                    .await
                    .context("could not reach otterd (register anyway with --no-check)")?;
                let status = conn.host_status().await?;
                println!(
                    "{name}: otterd {} on {} ({}/{})",
                    status.otterd_version, status.hostname, status.os, status.arch
                );
            }
            config.add_host(entry)?;
            if default {
                config.default_host = Some(name.clone());
            }
            config.save()?;
            println!("added host {name}");
            Ok(())
        }
        HostCommand::List => {
            if json {
                return output::print_json(&config.hosts);
            }
            let rows = config
                .hosts
                .iter()
                .map(|h| {
                    let default = if config.default_host.as_deref() == Some(&h.name) {
                        "*"
                    } else {
                        ""
                    };
                    vec![format!("{}{default}", h.name), h.describe()]
                })
                .collect();
            output::table(&["HOST", "TRANSPORT"], rows);
            Ok(())
        }
        HostCommand::Remove { name } => {
            config.remove_host(&name)?;
            config.save()?;
            println!("removed host {name}");
            Ok(())
        }
        HostCommand::Install { name } => {
            let host = config.host(&name)?.clone();
            let version = env!("CARGO_PKG_VERSION");
            println!("installing otterd {version} on {name}…");
            let out =
                otter_client::install::install(&config.transport(&host), version, true).await?;
            println!("{out}");
            Ok(())
        }
        HostCommand::Default { name } => {
            config.host(&name)?;
            config.default_host = Some(name.clone());
            config.save()?;
            println!("default host is now {name}");
            Ok(())
        }
        HostCommand::Status { name } => {
            let hosts: Vec<HostEntry> = match name {
                Some(n) => vec![config.host(&n)?.clone()],
                None => config.hosts.clone(),
            };
            if hosts.is_empty() {
                bail!("no hosts registered; add one with `otter host add <name>`");
            }
            let mut statuses = Vec::new();
            for host in hosts {
                let result = async {
                    let mut conn = connect(config, &host).await?;
                    Ok::<_, anyhow::Error>(conn.host_status().await?)
                }
                .await;
                statuses.push((host, result));
            }
            if json {
                let v: Vec<_> = statuses
                    .iter()
                    .map(|(h, r)| match r {
                        Ok(s) => serde_json::json!({"host": h.name, "status": s}),
                        Err(e) => serde_json::json!({"host": h.name, "error": format!("{e:#}")}),
                    })
                    .collect();
                return output::print_json(&v);
            }
            for (host, result) in statuses {
                output::host_status(&host, result);
            }
            Ok(())
        }
        HostCommand::Shutdown { name } => {
            let host = config.host(&name)?.clone();
            connect(config, &host).await?.shutdown().await?;
            println!("otterd on {name} stopped; sessions keep running");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

async fn workspace_command(config: &Config, cmd: WorkspaceCommand, json: bool) -> Result<()> {
    match cmd {
        WorkspaceCommand::Create(args) => create_workspace(config, args, json).await,
        WorkspaceCommand::List { archived } => list_workspaces(config, json, archived).await,
        WorkspaceCommand::Show { target } => {
            let found = find(config, &Target::parse(&target)?).await?;
            if json {
                return output::print_json(&found.workspace);
            }
            output::workspace_details(&found.host.name, &found.workspace);
            Ok(())
        }
        WorkspaceCommand::Prepare { target } => {
            let mut found = find(config, &Target::parse(&target)?).await?;
            let ws = found
                .conn
                .workspace_prepare(found.workspace.id.as_str())
                .await?;
            let ws = wait_until_prepared(&mut found.conn, ws).await?;
            if json {
                return output::print_json(&ws);
            }
            report_prepared(&found.host.name, &ws)
        }
        WorkspaceCommand::Archive { target } => {
            let mut found = find(config, &Target::parse(&target)?).await?;
            let ws = found
                .conn
                .workspace_archive(found.workspace.id.as_str())
                .await?;
            if json {
                return output::print_json(&ws);
            }
            println!(
                "archived workspace {} on {} (files kept in {}; `otter ws unarchive {}` to bring it back)",
                ws.name, found.host.name, ws.root, ws.name
            );
            Ok(())
        }
        WorkspaceCommand::Unarchive { target } => {
            let mut found = find(config, &Target::parse(&target)?).await?;
            let ws = found
                .conn
                .workspace_unarchive(found.workspace.id.as_str())
                .await?;
            let ws = wait_until_prepared(&mut found.conn, ws).await?;
            if json {
                return output::print_json(&ws);
            }
            report_prepared(&found.host.name, &ws)?;
            if let Some(s) = ws
                .sessions
                .iter()
                .find(|s| s.status() == otter_core::SessionStatus::Stopped)
            {
                println!(
                    "(sessions stay stopped: `otter session restart {}/{}` to continue)",
                    ws.name, s.name
                );
            }
            Ok(())
        }
        WorkspaceCommand::Brief(args) => {
            let mut found = find(config, &Target::parse(&args.target)?).await?;
            let ws = if args.edits() {
                let brief = args.apply(&found.workspace.brief);
                found
                    .conn
                    .workspace_set_brief(found.workspace.id.as_str(), brief)
                    .await?
            } else {
                found.workspace
            };
            if json {
                return output::print_json(&ws.brief);
            }
            output::brief(&ws.name, &ws.brief);
            Ok(())
        }
        WorkspaceCommand::Delete { target, yes, force } => {
            let mut found = find(config, &Target::parse(&target)?).await?;
            let ws = &found.workspace;
            if !yes {
                if !std::io::stdin().is_terminal() {
                    bail!("refusing to delete without confirmation; pass --yes");
                }
                let prompt = format!(
                    "Delete workspace `{}` on {} ({} session(s), directory {})? [y/N] ",
                    ws.name,
                    found.host.name,
                    ws.sessions.len(),
                    ws.root
                );
                if !output::confirm(&prompt)? {
                    bail!("aborted");
                }
            }
            let id = ws.id.to_string();
            let name = ws.name.clone();
            found.conn.workspace_delete(&id, force).await?;
            println!("deleted workspace {name}");
            Ok(())
        }
    }
}

async fn create_workspace(config: &Config, args: WorkspaceCreateArgs, json: bool) -> Result<()> {
    let host = match &args.host {
        Some(h) => config.host(h)?.clone(),
        None => config.default_host()?.clone(),
    };
    let mut conn = connect(config, &host).await?;
    let sessions = default_sessions(&args);
    let brief = args.goal.map(|goal| Brief {
        goal: Some(goal),
        ..Default::default()
    });
    let source = match (args.repo, args.dir) {
        (Some(repository), _) => SourceSpec::Git {
            repository,
            branch: args.branch,
            base: args.base,
        },
        (None, Some(path)) => SourceSpec::Directory { path },
        (None, None) => SourceSpec::Empty,
    };
    let ws = conn
        .workspace_create(WorkspaceCreate {
            name: args.name,
            brief,
            source,
            sessions: Some(sessions),
        })
        .await?;
    let ws = if args.no_wait {
        ws
    } else {
        wait_until_prepared(&mut conn, ws).await?
    };
    if json {
        return output::print_json(&ws);
    }
    report_prepared(&host.name, &ws)
}

/// Codex + a shell, as in design §27, minus what was opted out of.
fn default_sessions(args: &WorkspaceCreateArgs) -> Vec<SessionSpec> {
    let mut sessions = Vec::new();
    if !args.no_agent {
        let mut agent = SessionSpec::agent(&args.agent);
        agent.prompt = args.prompt.clone();
        sessions.push(agent);
    }
    if !args.no_shell {
        sessions.push(SessionSpec::shell());
    }
    sessions
}

/// Poll until the workspace leaves `preparing`, echoing progress to stderr.
async fn wait_until_prepared(conn: &mut Connection, mut ws: Workspace) -> Result<Workspace> {
    let mut last = None;
    while ws.state == WorkspaceState::Preparing {
        if ws.state_message != last {
            if let Some(msg) = &ws.state_message {
                eprintln!("{}: {msg}…", ws.name);
            }
            last = ws.state_message.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        ws = conn.workspace_get(ws.id.as_str()).await?;
    }
    Ok(ws)
}

fn report_prepared(host: &str, ws: &Workspace) -> Result<()> {
    match ws.state {
        WorkspaceState::Failed => {
            bail!(
                "workspace {} failed: {}\n(fix the problem and run `otter ws prepare {}`, or delete it)",
                ws.name,
                ws.state_message.as_deref().unwrap_or("unknown error"),
                ws.name
            )
        }
        WorkspaceState::Preparing => {
            println!("workspace {} on {host} is preparing", ws.name);
        }
        _ => {
            println!("workspace {} ready on {host}", ws.name);
            println!("  {}", ws.root);
            if let WorkspaceSource::Git(g) = &ws.source {
                println!(
                    "  branch {} (from {})",
                    g.branch,
                    g.base.as_deref().unwrap_or("?")
                );
            }
            if ws.environment.kind != EnvironmentKind::None {
                println!("  environment: {}", ws.environment.kind.as_str());
            }
            for s in &ws.sessions {
                println!("  {}  {}", s.name, output::session_status(s));
                if let Some(e) = &s.launch_error {
                    println!("    error: {e}");
                }
            }
        }
    }
    Ok(())
}

async fn list_workspaces(config: &Config, json: bool, archived: bool) -> Result<()> {
    if config.hosts.is_empty() {
        bail!("no hosts registered; add one with `otter host add <name>`");
    }
    let mut tasks = JoinSet::new();
    for (i, host) in config.hosts.iter().enumerate() {
        let transport = config.transport(host);
        tasks.spawn(async move {
            let result = async {
                let mut conn = Connection::connect(&transport).await?;
                conn.workspace_list().await
            }
            .await;
            (i, result)
        });
    }
    let mut results: Vec<_> = tasks.join_all().await;
    results.sort_by_key(|(i, _)| *i);
    if !archived {
        for (_, r) in &mut results {
            if let Ok(list) = r {
                list.retain(|w| w.state != WorkspaceState::Archived);
            }
        }
    }

    if json {
        let v: Vec<_> = results
            .iter()
            .map(|(i, r)| {
                let host = &config.hosts[*i].name;
                match r {
                    Ok(list) => serde_json::json!({"host": host, "workspaces": list}),
                    Err(e) => serde_json::json!({"host": host, "error": e.to_string()}),
                }
            })
            .collect();
        return output::print_json(&v);
    }

    let mut rows = Vec::new();
    for (i, result) in &results {
        let host = &config.hosts[*i].name;
        match result {
            Ok(list) => {
                for ws in list {
                    rows.push(vec![
                        ws.name.clone(),
                        host.clone(),
                        ws.activity().label().to_lowercase(),
                        output::sessions_summary(ws),
                    ]);
                }
            }
            Err(e) => eprintln!("warning: {host}: {e}"),
        }
    }
    if rows.is_empty() {
        println!("no workspaces (create one with `otter new <name>`)");
        return Ok(());
    }
    output::table(&["WORKSPACE", "HOST", "STATE", "SESSIONS"], rows);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

async fn session_command(config: &Config, cmd: SessionCommand, json: bool) -> Result<()> {
    match cmd {
        SessionCommand::Create(args) => create_session(config, args, json).await,
        SessionCommand::Stop { target } => {
            session_op(config, &target, json, |mut conn, ws, s| async move {
                conn.session_stop(&ws, &s).await
            })
            .await
        }
        SessionCommand::Restart { target } => {
            session_op(config, &target, json, |mut conn, ws, s| async move {
                conn.session_restart(&ws, &s).await
            })
            .await
        }
        SessionCommand::Delete { target } => {
            let target = Target::parse(&target)?;
            if target.session.is_none() {
                bail!("name the session to delete: workspace/session");
            }
            let mut found = find(config, &target).await?;
            let session = found.session(target.session.as_deref())?;
            found
                .conn
                .session_delete(found.workspace.id.as_str(), session.id.as_str())
                .await?;
            println!("deleted session {}", found.label(&session));
            Ok(())
        }
    }
}

async fn session_op<F, Fut>(config: &Config, target: &str, json: bool, op: F) -> Result<()>
where
    F: FnOnce(Connection, String, String) -> Fut,
    Fut: std::future::Future<Output = otter_client::Result<otter_core::Session>>,
{
    let target = Target::parse(target)?;
    let found = find(config, &target).await?;
    let session = found.session(target.session.as_deref())?;
    let label = found.label(&session);
    let updated = op(
        found.conn,
        found.workspace.id.to_string(),
        session.id.to_string(),
    )
    .await?;
    if json {
        return output::print_json(&updated);
    }
    println!("{label}: {}", output::session_status(&updated));
    Ok(())
}

async fn create_session(config: &Config, args: SessionCreateArgs, json: bool) -> Result<()> {
    let target = Target::parse(&args.workspace)?;
    if target.session.is_some() {
        bail!("give the session name with --name, not workspace/session");
    }
    let mut found = find(config, &target).await?;
    let command = (!args.command.is_empty()).then(|| join_command(&args.command));
    let spec = match args.agent {
        Some(provider) => SessionSpec {
            name: args.name,
            prompt: args.prompt,
            ..SessionSpec::agent(&provider)
        },
        None => SessionSpec {
            name: args.name,
            kind: args.kind,
            command,
            provider: None,
            prompt: None,
        },
    };
    let session = found
        .conn
        .session_create(SessionCreate {
            workspace: found.workspace.id.to_string(),
            spec,
        })
        .await?;
    if json {
        return output::print_json(&session);
    }
    println!(
        "started {}  {}",
        found.label(&session),
        output::session_status(&session)
    );
    Ok(())
}

/// Turn argv after `--` into a shell command line. A single argument is used
/// verbatim (so `-- 'npm run dev && x'` works); several are quoted.
fn join_command(argv: &[String]) -> String {
    if argv.len() == 1 {
        return argv[0].clone();
    }
    argv.iter()
        .map(|a| otter_client::sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Stream a host's events after `cursor` into `tx`, reconnecting (and resuming
/// from the last event seen) whenever the connection drops.
async fn follow_events(
    transport: otter_client::Transport,
    host: String,
    mut cursor: u64,
    tx: tokio::sync::mpsc::Sender<(String, otter_protocol::EventRecord)>,
) {
    loop {
        let stream = match Connection::connect(&transport).await {
            Ok(conn) => conn.subscribe(Some(cursor)).await,
            Err(e) => Err(e),
        };
        match stream {
            Ok(mut stream) => {
                while let Ok(Some(rec)) = stream.next().await {
                    cursor = rec.seq;
                    if tx.send((host.clone(), rec)).await.is_err() {
                        return;
                    }
                }
            }
            Err(otter_client::ClientError::Rpc(e))
                if e.code == otter_protocol::ErrorCode::CursorExpired =>
            {
                eprintln!("otter: {host}: {e}; some events were missed, following from now");
                if let Ok(mut conn) = Connection::connect(&transport).await
                    && let Ok(snapshot) = conn.snapshot().await
                {
                    cursor = snapshot.seq;
                }
            }
            Err(_) => {}
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn events(
    config: &Config,
    host: Option<String>,
    limit: u32,
    follow: bool,
    json: bool,
) -> Result<()> {
    let hosts: Vec<HostEntry> = match host {
        Some(h) => vec![config.host(&h)?.clone()],
        None => config.hosts.clone(),
    };
    if hosts.is_empty() {
        bail!("no hosts registered");
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, otter_protocol::EventRecord)>(256);
    let mut names: HashMap<String, String> = HashMap::new();
    let mut backlog = Vec::new();
    for host in &hosts {
        let mut conn = connect(config, host).await?;
        let snapshot = conn.snapshot().await?;
        for ws in snapshot.workspaces {
            for s in &ws.sessions {
                names.insert(s.id.to_string(), s.name.clone());
            }
            names.insert(ws.id.to_string(), ws.name);
        }
        let mut cursor = snapshot.seq;
        for rec in conn.events_list(Some(limit)).await? {
            cursor = cursor.max(rec.seq);
            backlog.push((host.name.clone(), rec));
        }
        if follow {
            tokio::spawn(follow_events(
                config.transport(host),
                host.name.clone(),
                cursor,
                tx.clone(),
            ));
        }
    }
    drop(tx);
    backlog.sort_by_key(|(_, r)| r.ts);
    for (host, rec) in backlog {
        output::event_line(&host, &rec, &mut names, json);
    }
    if follow {
        while let Some((host, rec)) = rx.recv().await {
            output::event_line(&host, &rec, &mut names, json);
        }
    }
    Ok(())
}
