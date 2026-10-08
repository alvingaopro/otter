//! Filesystem layout of a host (design §13):
//!
//! ```text
//! ~/.otter/
//!     run/            daemon socket, lock, pid, tmux socket + config
//!     state/          state.json, events.jsonl (+ rotated events.<seq>.jsonl, events.id)
//!     logs/           workd.log
//!     workspaces/     <workspace-id>/ …
//! ```

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Unix socket paths are limited to ~108 bytes on Linux and 104 on macOS.
const MAX_SOCKET_PATH: usize = 100;

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub run_dir: PathBuf,
    pub socket: PathBuf,
    pub lock_file: PathBuf,
    pub pid_file: PathBuf,
    pub tmux_socket: PathBuf,
    pub tmux_conf: PathBuf,
    /// Short-lived files carrying environments to launched processes.
    pub env_dir: PathBuf,
    pub state_dir: PathBuf,
    pub state_file: PathBuf,
    pub events_file: PathBuf,
    pub log_dir: PathBuf,
    pub log_file: PathBuf,
    pub workspaces_dir: PathBuf,
}

impl Paths {
    pub fn new(home: PathBuf) -> Self {
        let run_dir = home.join("run");
        let state_dir = home.join("state");
        let log_dir = home.join("logs");
        Paths {
            socket: run_dir.join("workd.sock"),
            lock_file: run_dir.join("workd.lock"),
            pid_file: run_dir.join("workd.pid"),
            tmux_socket: run_dir.join("tmux.sock"),
            tmux_conf: run_dir.join("tmux.conf"),
            env_dir: run_dir.join("env"),
            state_file: state_dir.join("state.json"),
            events_file: state_dir.join("events.jsonl"),
            log_file: log_dir.join("workd.log"),
            workspaces_dir: home.join("workspaces"),
            run_dir,
            state_dir,
            log_dir,
            home,
        }
    }

    /// Resolve the home directory: explicit `--home` / `OTTER_HOME` (or the
    /// pre-rename `WORKD_HOME`), else `$HOME/.otter` — or `$HOME/.workd` when
    /// only that exists, so a host keeps its workspaces and running sessions
    /// across the rename (D-024).
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let explicit = explicit.or_else(|| std::env::var_os("WORKD_HOME").map(PathBuf::from));
        let home = match explicit {
            Some(p) => absolutize(&p)?,
            None => {
                let user_home =
                    PathBuf::from(std::env::var_os("HOME").context("$HOME is not set")?);
                let home = user_home.join(".otter");
                let legacy = user_home.join(".workd");
                if !home.exists() && legacy.exists() {
                    legacy
                } else {
                    home
                }
            }
        };
        let paths = Paths::new(home);
        for sock in [&paths.socket, &paths.tmux_socket] {
            if sock.as_os_str().len() > MAX_SOCKET_PATH {
                bail!(
                    "socket path {} is too long for a Unix socket; use a shorter OTTER_HOME",
                    sock.display()
                );
            }
        }
        Ok(paths)
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.home,
            &self.run_dir,
            &self.env_dir,
            &self.state_dir,
            &self.log_dir,
            &self.workspaces_dir,
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        // The run dir guards the daemon socket; keep it private even if it
        // pre-existed with looser permissions.
        std::fs::set_permissions(&self.run_dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    /// The directory a new empty workspace lives in.
    pub fn workspace_dir(&self, id: &str) -> PathBuf {
        self.workspaces_dir.join(id)
    }
}

fn absolutize(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}
