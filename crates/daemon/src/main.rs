//! `otterd` — the Otter host daemon (docs/design.md §24).

mod agents;
mod attach;
mod attention;
mod backend;
mod daemon;
mod dial;
mod env;
mod environment;
mod events;
mod files;
mod git;
mod history;
mod login;
mod metrics;
mod paths;
mod reconcile;
mod server;
mod sessions;
mod store;
mod workspaces;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use otter_protocol::Event;
use tokio::net::UnixListener;

use crate::backend::tmux::TmuxBackend;
use crate::daemon::Daemon;
use crate::events::EventLog;
use crate::paths::Paths;
use crate::store::Store;

#[derive(Parser)]
#[command(name = "otterd", version, about = "Otter host daemon")]
struct Cli {
    /// State directory (default: ~/.otter).
    #[arg(long, global = true, env = "OTTER_HOME")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Serve,
    /// Connect stdin/stdout to the daemon, starting it if needed. This is what
    /// clients run over SSH.
    Dial,
    /// Print the version.
    Version,
    /// Open a sign-in page in the browser of a connected Otter app or attached
    /// terminal (what the
    /// xdg-open/www-browser stand-ins run).
    #[command(hide = true)]
    OpenUrl { url: String },
    #[command(hide = true)]
    InternalDumpEnv,
    #[command(hide = true)]
    InternalExec {
        env_file: PathBuf,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// What an agent's hooks call (agents::run_hook).
    #[command(hide = true)]
    InternalAgentHook { provider: String, file: PathBuf },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::InternalDumpEnv => {
            env::dump_env();
            return Ok(());
        }
        Command::InternalExec { env_file, argv } => {
            return env::exec_with_env_file(env_file, argv);
        }
        Command::InternalAgentHook { provider, file } => {
            agents::run_hook(provider, file);
            return Ok(());
        }
        _ => {}
    }
    if let Command::Version = cli.command {
        println!("otterd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let paths = Paths::resolve(cli.home)?;
    let runtime = tokio::runtime::Runtime::new()?;
    match cli.command {
        Command::Serve => {
            init_logging();
            runtime.block_on(serve(paths))
        }
        Command::Dial => {
            runtime.block_on(dial::dial(&paths))?;
            // Don't wait for the blocking stdin reader thread.
            std::process::exit(0);
        }
        Command::OpenUrl { url } => match runtime.block_on(open_url(&paths, url)) {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("otter: {e:#}");
                std::process::exit(1);
            }
        },
        Command::Version
        | Command::InternalDumpEnv
        | Command::InternalExec { .. }
        | Command::InternalAgentHook { .. } => {
            unreachable!()
        }
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("OTTER_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

async fn serve(paths: Paths) -> Result<()> {
    paths.ensure_dirs()?;
    let Some(_lock) = acquire_lock(&paths).await? else {
        tracing::info!(
            "another otterd is already running for {}",
            paths.home.display()
        );
        return Ok(());
    };
    std::fs::write(&paths.pid_file, format!("{}\n", std::process::id()))?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), home = %paths.home.display(), "otterd starting");

    let shell = env::user_shell();
    let resolved = env::resolve_login_env(&shell).await;
    tracing::info!(source = %resolved.source, vars = resolved.vars.len(), "resolved environment");

    let backend = Arc::new(TmuxBackend::new(&paths, &resolved.vars).await?);
    match backend.version().await {
        Some(v) => tracing::info!("using {v}"),
        None => tracing::warn!("tmux not found; sessions cannot be started"),
    }
    let store = Store::load(&paths.state_file)?;
    let events = EventLog::open_with(&paths.events_file, events::Limits::from_env())?;
    let daemon = Arc::new(Daemon::new(
        paths.clone(),
        store,
        backend,
        events,
        resolved,
        shell,
    ));

    // Recover from a previous daemon: adopt still-running processes, record
    // exits and losses that happened while we were down.
    if let Err(e) = daemon.reconcile().await {
        tracing::warn!("initial reconcile failed: {e:#}");
    }
    daemon.resume_workspaces().await;

    // We hold the lock, so any socket file left behind is stale.
    let _ = std::fs::remove_file(&paths.socket);
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("binding {}", paths.socket.display()))?;
    std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
    daemon.events.emit(Event::DaemonStarted {
        version: env!("CARGO_PKG_VERSION").to_owned(),
    });
    tracing::info!(socket = %paths.socket.display(), "listening");

    // Observe process exits.
    let poller = {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(e) = daemon.reconcile().await {
                    tracing::warn!("reconcile failed: {e:#}");
                }
            }
        })
    };

    // Stop on SIGTERM / SIGINT (managed processes keep running).
    {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut int)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            tracing::info!("signal received");
            daemon.request_shutdown();
        });
    }

    let result = server::serve(daemon, listener).await;
    poller.abort();
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.pid_file);
    tracing::info!("otterd stopped");
    result
}

/// Take the single-instance lock, waiting briefly for a previous daemon that
/// is shutting down. `None` means another daemon is running.
async fn acquire_lock(paths: &Paths) -> Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock_file)
        .with_context(|| format!("opening {}", paths.lock_file.display()))?;
    for _ in 0..50 {
        // SAFETY: valid fd owned by `file`.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(file));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            bail!("locking {}: {err}", paths.lock_file.display());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(None)
}

/// `otterd open-url`: ask the running daemon to open a sign-in page on the Mac.
async fn open_url(paths: &Paths, url: String) -> Result<()> {
    use otter_protocol::wire::{read_json, write_json};
    use otter_protocol::{BrowserOpen, ClientMessage, Request, ServerMessage};
    let stream = tokio::net::UnixStream::connect(&paths.socket)
        .await
        .map_err(|_| anyhow::anyhow!("otterd is not running"))?;
    let (r, mut w) = stream.into_split();
    let mut r = tokio::io::BufReader::new(r);
    let _hello: Option<ServerMessage> = read_json(&mut r).await?;
    let msg = ClientMessage {
        id: 1,
        request: Request::BrowserOpen(BrowserOpen {
            url,
            session: std::env::var("OTTER_SESSION_ID").ok(),
        }),
    };
    write_json(&mut w, &msg).await?;
    match read_json::<_, ServerMessage>(&mut r).await? {
        Some(ServerMessage::Response { error: None, .. }) => Ok(()),
        Some(ServerMessage::Response { error: Some(e), .. }) => anyhow::bail!("{}", e.message),
        _ => anyhow::bail!("no answer from otterd"),
    }
}
