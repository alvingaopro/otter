//! Resolving the environment launched processes run in (design §17, §19).
//!
//! The daemon is usually started from a non-interactive `ssh host otterd dial`,
//! whose environment is minimal (e.g. `~/.local/bin` is not on `PATH`). Rather
//! than depending on shell startup hooks inside each session, the daemon asks
//! the user's login shell for its environment once, explicitly, and launches
//! every managed process with it — so anything that works after `ssh host`
//! works inside Otter.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

pub type EnvMap = BTreeMap<String, String>;

const BEGIN: &str = "__WORKD_ENV_BEGIN__";
const END: &str = "__WORKD_ENV_END__";
const SELF_VAR: &str = "OTTER_SELF_EXE";

/// Variables that describe a particular terminal, shell process or SSH
/// connection rather than the user's environment.
const STRIP: &[&str] = &[
    "_",
    "COLUMNS",
    "LINES",
    "OLDPWD",
    "PWD",
    "SHLVL",
    "SSH_CLIENT",
    "SSH_CONNECTION",
    "SSH_TTY",
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TMUX",
    "TMUX_PANE",
    "WINDOWID",
    SELF_VAR,
];

#[derive(Clone, Debug)]
pub struct ResolvedEnv {
    pub vars: EnvMap,
    /// Human-readable description of where `vars` came from.
    pub source: String,
}

/// The user's login shell.
pub fn user_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_owned())
}

/// Ask `shell` for its login environment. Falls back to the daemon's own
/// environment if that fails.
pub async fn resolve_login_env(shell: &str) -> ResolvedEnv {
    match capture_login_env(shell).await {
        Ok(vars) => ResolvedEnv {
            vars,
            source: format!("login shell ({shell} -l)"),
        },
        Err(e) => {
            tracing::warn!(
                "could not capture login environment from {shell}: {e:#}; using daemon environment"
            );
            ResolvedEnv {
                vars: clean(std::env::vars().collect()),
                source: "daemon environment (login shell capture failed)".to_owned(),
            }
        }
    }
}

async fn capture_login_env(shell: &str) -> Result<EnvMap> {
    let exe = self_exe()?;
    // The shell re-executes otterd, which prints its inherited environment as
    // JSON between markers; anything profile scripts print is ignored.
    let mut cmd = Command::new(shell);
    cmd.arg("-l")
        .arg("-c")
        .arg(format!("exec \"${SELF_VAR}\" internal-dump-env"))
        .env(SELF_VAR, &exe)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .context("login shell timed out")??;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let start = stdout
        .find(BEGIN)
        .context("no environment markers in shell output")?
        + BEGIN.len();
    let end = stdout[start..]
        .find(END)
        .context("unterminated environment output")?
        + start;
    let vars: EnvMap = serde_json::from_str(&stdout[start..end]).context("parsing environment")?;
    if !vars.contains_key("PATH") {
        bail!("captured environment has no PATH");
    }
    Ok(clean(vars))
}

/// Variables the terminal multiplexer sets for the process; they are kept when
/// replacing the environment in [`exec_with_env_file`].
const PASS_THROUGH: &[&str] = &["TERM", "TMUX", "TMUX_PANE", "COLORTERM"];

/// Write `env` to a private file for [`exec_with_env_file`]. Environments are
/// passed this way rather than on a command line, where they would be visible
/// to every user on the host.
pub fn write_env_file(path: &std::path::Path, env: &EnvMap) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(&serde_json::to_vec(env)?)?;
    Ok(())
}

/// Entry point for `otterd internal-exec <env-file> -- <argv>`: replace the
/// environment with the one in `env_file` (deleting the file), then exec.
pub fn exec_with_env_file(env_file: &std::path::Path, argv: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let bytes =
        std::fs::read(env_file).with_context(|| format!("reading {}", env_file.display()))?;
    let _ = std::fs::remove_file(env_file);
    let mut env: EnvMap = serde_json::from_slice(&bytes).context("parsing environment file")?;
    for k in PASS_THROUGH {
        if let Ok(v) = std::env::var(k) {
            env.insert((*k).to_owned(), v);
        }
    }
    let (program, args) = argv.split_first().context("no command given")?;
    let err = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .envs(&env)
        .exec();
    Err(err).with_context(|| format!("executing {program}"))
}

/// Path of the running otterd binary, usable for re-executing it even after an
/// upgrade replaced the file.
pub fn self_exe() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("locating otterd executable")?;
    let s = exe.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(stripped) => stripped.into(),
        None => exe,
    })
}

/// Entry point for `otterd internal-dump-env`.
pub fn dump_env() {
    let vars: EnvMap = std::env::vars().collect();
    print!("{BEGIN}{}{END}", serde_json::to_string(&vars).unwrap());
}

fn clean(mut vars: EnvMap) -> EnvMap {
    for k in STRIP {
        vars.remove(*k);
    }
    vars
}

/// Find `program` on the `PATH` contained in `env`.
pub fn which(program: &str, env: &EnvMap) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = env.get("PATH")?;
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|p| {
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// How often to retry a spawn whose executable is busy.
const BUSY_RETRIES: u32 = 10;

fn busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ETXTBSY)
}

/// Spawn, retrying while the executable is "busy" (ETXTBSY): a file just
/// written (an agent's script, a test's fake) can briefly stay open for
/// writing in a child another thread is forking.
pub fn spawn_std(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    let mut attempt = 0;
    loop {
        match cmd.spawn() {
            Err(e) if busy(&e) && attempt < BUSY_RETRIES => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(20 * u64::from(attempt)));
            }
            r => return r,
        }
    }
}

/// [`spawn_std`] for tokio.
pub async fn spawn_tokio(
    cmd: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    let mut attempt = 0;
    loop {
        match cmd.spawn() {
            Err(e) if busy(&e) && attempt < BUSY_RETRIES => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(20 * u64::from(attempt))).await;
            }
            r => return r,
        }
    }
}
