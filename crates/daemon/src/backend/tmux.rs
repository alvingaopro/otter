//! tmux execution backend.
//!
//! Otter runs its own tmux server (`~/.otter/run/tmux.sock`, own config) so it
//! never touches the user's tmux sessions. One tmux session holds exactly one
//! execution and is named after the execution id. tmux is configured to be
//! invisible: no status bar, no prefix key; `remain-on-exit` keeps exited
//! processes around so their exit status and final output can be read.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::process::Command;

use super::{AttachCommand, ExecutionBackend, LaunchSpec, Launched, ProcessState};
use crate::env::{EnvMap, self_exe, which, write_env_file};
use crate::paths::Paths;

/// tmux can report a pane as dead slightly before it has collected the exit
/// status. Wait this long for the status before reporting an unknown one.
const EXIT_STATUS_GRACE: Duration = Duration::from_secs(2);

/// Shown (by tmux) under the output of an exited process; stripped from
/// captures.
const EXIT_MARKER: &str = "[process exited, status";
/// What tmux before 3.3 draws under an exited process instead.
const OLD_EXIT_MARKER: &str = "Pane is dead";

/// Initial window size before any client attaches.
const DEFAULT_COLS: u16 = 160;
const DEFAULT_ROWS: u16 = 48;

pub struct TmuxBackend {
    tmux: PathBuf,
    socket: PathBuf,
    conf: PathBuf,
    /// Where per-launch environment files go (see `launch`).
    env_dir: PathBuf,
    /// This otterd binary, used as the exec shim.
    otterd: PathBuf,
    /// Environment for tmux client commands (and thus for a freshly started
    /// tmux server).
    env: EnvMap,
    /// Dead panes whose exit status isn't available yet, and when they were
    /// first seen.
    awaiting_status: Mutex<HashMap<String, Instant>>,
}

impl TmuxBackend {
    pub async fn new(paths: &Paths, env: &EnvMap) -> Result<Self> {
        let tmux = which("tmux", env).unwrap_or_else(|| PathBuf::from("tmux"));
        let backend = TmuxBackend {
            tmux,
            socket: paths.tmux_socket.clone(),
            conf: paths.tmux_conf.clone(),
            env_dir: paths.env_dir.clone(),
            otterd: self_exe()?,
            env: env.clone(),
            awaiting_status: Mutex::new(HashMap::new()),
        };
        let term = if backend.has_terminfo("tmux-256color").await {
            "tmux-256color"
        } else {
            "screen-256color"
        };
        // `remain-on-exit-format` is tmux 3.3+; on 3.2 an unknown option in
        // the config makes tmux show an error over the first window.
        let exit_format = backend
            .version()
            .await
            .and_then(|v| parse_version(&v))
            .is_none_or(|v| v >= (3, 3));
        std::fs::write(&backend.conf, config(term, exit_format))
            .with_context(|| format!("writing {}", backend.conf.display()))?;
        // Apply config changes to an already-running server (e.g. after a
        // daemon upgrade). Fails harmlessly when no server is running.
        let _ = backend
            .run(&["source-file", &backend.conf.to_string_lossy()])
            .await;
        Ok(backend)
    }

    /// `tmux -V`, if tmux is installed.
    pub async fn version(&self) -> Option<String> {
        let out = Command::new(&self.tmux)
            .arg("-V")
            .envs(&self.env)
            .output()
            .await
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    async fn has_terminfo(&self, name: &str) -> bool {
        Command::new("infocmp")
            .arg(name)
            .envs(&self.env)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.tmux);
        // -u: without a UTF-8 locale (common for daemons started over plain
        // SSH) tmux mangles output, e.g. rewriting tabs as `_`.
        cmd.arg("-u")
            .arg("-S")
            .arg(&self.socket)
            .arg("-f")
            .arg(&self.conf)
            .env_clear()
            .envs(&self.env)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let out = self
            .command()
            .args(args)
            .output()
            .await
            .with_context(|| format!("running {}", self.tmux.display()))?;
        if !out.status.success() {
            bail!(
                "tmux {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn target_session(backend_ref: &str) -> String {
    // `=` forces an exact match instead of tmux's prefix matching.
    format!("={backend_ref}")
}

fn target_pane(backend_ref: &str) -> String {
    format!("={backend_ref}:")
}

fn is_no_server(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    msg.contains("no server running") || msg.contains("error connecting to")
}

fn is_missing_session(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    is_no_server(err) || msg.contains("can't find session") || msg.contains("session not found")
}

impl TmuxBackend {
    async fn launch_with_env_file(
        &self,
        spec: &LaunchSpec,
        env_file: &std::path::Path,
    ) -> Result<Launched> {
        let cols = DEFAULT_COLS.to_string();
        let rows = DEFAULT_ROWS.to_string();
        let cwd = spec.cwd.to_string_lossy();
        let mut args: Vec<String> = [
            "new-session",
            "-d",
            "-s",
            &spec.backend_ref,
            "-c",
            &cwd,
            "-x",
            &cols,
            "-y",
            &rows,
            "-P",
            "-F",
            "#{pane_pid}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.push("--".into());
        args.push(self.otterd.to_string_lossy().into_owned());
        args.push("internal-exec".into());
        args.push(env_file.to_string_lossy().into_owned());
        args.push("--".into());
        args.extend(spec.argv.iter().cloned());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&argv).await?;
        Ok(Launched {
            pid: out.trim().parse().ok(),
        })
    }

    async fn inspect_panes(&self) -> Result<HashMap<String, ProcessState>> {
        let out = match self
            .run(&[
                "list-panes",
                "-a",
                "-F",
                // `|` never appears in the fields and is never escaped.
                "#{session_name}|#{pane_dead}|#{pane_dead_status}|#{pane_dead_signal}|#{pane_pid}|#{window_activity}",
            ])
            .await
        {
            Ok(out) => out,
            Err(e) if is_no_server(&e) => return Ok(HashMap::new()),
            Err(e) => return Err(e),
        };
        let mut map = HashMap::new();
        let mut awaiting = self.awaiting_status.lock().unwrap();
        let mut still_awaiting = HashMap::new();
        for line in out.lines() {
            let f: Vec<&str> = line.split('|').collect();
            if f.len() < 5 || map.contains_key(f[0]) {
                // One pane per session; keep the first if there are more.
                continue;
            }
            let pid = f[4].parse().ok();
            let last_output = f.get(5).and_then(|t| t.parse().ok());
            let state = if f[1] == "1" {
                let exit_code = f[2]
                    .parse::<i32>()
                    .ok()
                    .or_else(|| f[3].parse::<i32>().ok().map(|sig| 128 + sig))
                    .or_else(|| pid.and_then(zombie_exit_code));
                let since = awaiting.get(f[0]).copied().unwrap_or_else(Instant::now);
                if exit_code.is_none() && since.elapsed() < EXIT_STATUS_GRACE {
                    still_awaiting.insert(f[0].to_owned(), since);
                    ProcessState::Alive { pid, last_output }
                } else {
                    ProcessState::Dead { exit_code }
                }
            } else {
                ProcessState::Alive { pid, last_output }
            };
            map.insert(f[0].to_owned(), state);
        }
        *awaiting = still_awaiting;
        Ok(map)
    }
}

#[async_trait]
impl ExecutionBackend for TmuxBackend {
    fn name(&self) -> &'static str {
        "tmux"
    }

    async fn launch(&self, spec: &LaunchSpec) -> Result<Launched> {
        // The environment travels in a private file read (and deleted) by
        // `otterd internal-exec`, never on tmux's command line: the tmux server
        // keeps the argv of the client that started it, and argv is visible to
        // every user on the host. The shim also gives the process exactly
        // `spec.env` rather than tmux's global environment merged with it.
        let env_file = self.env_dir.join(format!("{}.json", spec.backend_ref));
        write_env_file(&env_file, &spec.env)?;
        let result = self.launch_with_env_file(spec, &env_file).await;
        if result.is_err() {
            let _ = std::fs::remove_file(&env_file);
        }
        result
    }

    async fn inspect(&self) -> Result<HashMap<String, ProcessState>> {
        self.inspect_panes().await
    }

    async fn terminate(&self, backend_ref: &str) -> Result<()> {
        match self
            .run(&["kill-session", "-t", &target_session(backend_ref)])
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_missing_session(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn capture(&self, backend_ref: &str, lines: Option<u32>) -> Result<String> {
        let start = match lines {
            Some(n) => format!("-{n}"),
            None => "-".to_owned(),
        };
        let out = self
            .run(&[
                "capture-pane",
                "-p",
                "-J",
                "-t",
                &target_pane(backend_ref),
                "-S",
                &start,
            ])
            .await?;
        let mut trimmed: Vec<&str> = out.lines().map(str::trim_end).collect();
        // Drop the exited-process marker tmux draws under the output.
        while trimmed.last().is_some_and(|l| l.is_empty()) {
            trimmed.pop();
        }
        if trimmed
            .last()
            .is_some_and(|l| l.starts_with(EXIT_MARKER) || l.starts_with(OLD_EXIT_MARKER))
        {
            trimmed.pop();
        }
        let end = trimmed
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        let mut text = trimmed[..end].join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        Ok(text)
    }

    async fn send_text(&self, backend_ref: &str, text: &str, enter: bool) -> Result<()> {
        let target = target_pane(backend_ref);
        if !text.is_empty() {
            self.run(&["send-keys", "-t", &target, "-l", "--", text])
                .await?;
        }
        if enter {
            self.run(&["send-keys", "-t", &target, "Enter"]).await?;
        }
        Ok(())
    }

    fn attach_command(&self, backend_ref: &str, term: &str) -> AttachCommand {
        let mut env = self.env.clone();
        env.insert("TERM".into(), term.into());
        AttachCommand {
            program: self.tmux.clone(),
            args: vec![
                "-u".into(),
                "-S".into(),
                self.socket.to_string_lossy().into_owned(),
                "-f".into(),
                self.conf.to_string_lossy().into_owned(),
                "attach-session".into(),
                "-t".into(),
                target_session(backend_ref),
            ],
            env,
        }
    }

    async fn detach_client(&self, tty: &str) -> Result<()> {
        self.run(&["detach-client", "-t", tty]).await.map(|_| ())
    }

    fn filter_attach_output(&self, data: &mut Vec<u8>) {
        strip_client_notices(data);
    }
}

/// Messages the tmux client prints when it leaves a session. They name tmux
/// sessions (i.e. execution ids), which are none of the user's business.
const CLIENT_NOTICES: &[&[u8]] = &[
    b"[detached (from session ",
    b"[exited]",
    b"[server exited]",
    b"[lost tty]",
    b"[terminated]",
];

fn strip_client_notices(data: &mut Vec<u8>) {
    for notice in CLIENT_NOTICES {
        while let Some(start) = find(data, notice) {
            let Some(close) = data[start..].iter().position(|&b| b == b']') else {
                break;
            };
            let mut end = start + close + 1;
            if data[end..].starts_with(b"\r\n") {
                end += 2;
            }
            data.drain(start..end);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Exit status of a zombie process, from `/proc/<pid>/stat` (Linux).
///
/// tmux occasionally marks a pane dead without ever reaping its process
/// (observed with tmux 3.4 when the process exits right after starting while
/// other clients are connecting), so it never learns the exit status. The
/// kernel still has it.
fn zombie_exit_code(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name, starting at field 3.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if fields.first() != Some(&"Z") {
        return None;
    }
    let status: i32 = fields.get(52 - 3)?.parse().ok()?;
    Some(if status & 0x7f == 0 {
        (status >> 8) & 0xff
    } else {
        128 + (status & 0x7f)
    })
}

/// `tmux 3.2a` → (3, 2); `tmux next-3.4` → (3, 4); `None` if unparseable
/// (e.g. a build from master, assumed recent).
fn parse_version(v: &str) -> Option<(u32, u32)> {
    let num = v.split_whitespace().nth(1)?.trim_start_matches("next-");
    let mut parts = num.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((major, minor.parse().ok()?))
}

/// The tmux config; `exit_format` sets our exited-process marker (tmux 3.3+).
fn config(default_terminal: &str, exit_format: bool) -> String {
    let exit_format = if exit_format {
        format!(
            "set -g remain-on-exit-format \"{EXIT_MARKER} #{{pane_dead_status}}#{{pane_dead_signal}}]\"\n"
        )
    } else {
        String::new()
    };
    format!(
        "\
# Generated by otterd on every start — do not edit.
# tmux is an implementation detail of otterd (docs/design.md §12): no status
# bar, no prefix key, and processes are kept after they exit so their exit
# status and final output stay readable.
set -g status off
set -g prefix None
set -g prefix2 None
set -g escape-time 0
set -g history-limit 50000
set -g default-size {DEFAULT_COLS}x{DEFAULT_ROWS}
set -g default-terminal \"{default_terminal}\"
set -as terminal-features \",xterm*:RGB\"
set -g focus-events on
set -g set-titles off
set -g allow-rename off
set -g remain-on-exit on
{exit_format}set -g window-size latest
# The wheel scrolls a session's history (tmux keeps it; the client only sees
# the screen), and text copied there reaches the client's clipboard.
set -g mouse on
set -g set-clipboard on
set -as terminal-features \",xterm*:clipboard\"
set -g automatic-rename off
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_format_only_for_tmux_that_has_it() {
        assert!(config("tmux-256color", true).contains("set -g mouse on"));
        assert_eq!(parse_version("tmux 3.2a"), Some((3, 2)));
        assert_eq!(parse_version("tmux 3.6a"), Some((3, 6)));
        assert_eq!(parse_version("tmux next-3.4"), Some((3, 4)));
        assert_eq!(parse_version("tmux master"), None);
        assert!(!config("tmux-256color", false).contains("remain-on-exit-format"));
        assert!(config("tmux-256color", true).contains("remain-on-exit-format \"[process exited"));
        assert!(
            config("tmux-256color", false).contains("set -g remain-on-exit on\nset -g window-size")
        );
    }

    #[test]
    fn strips_detach_notice() {
        let mut data = b"\x1b(B\x1b>[detached (from session exec_abc)]\r\nafter".to_vec();
        strip_client_notices(&mut data);
        assert_eq!(data, b"\x1b(B\x1b>after");
        let mut data = b"bye\r\n[exited]\r\n".to_vec();
        strip_client_notices(&mut data);
        assert_eq!(data, b"bye\r\n");
    }
}
