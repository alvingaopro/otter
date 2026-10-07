//! `workd dial`: bridge stdin/stdout to the daemon socket, starting the
//! daemon first if it isn't running.
//!
//! This is how clients reach the daemon over SSH (`ssh host workd dial`): the
//! SSH channel carries the connection and the socket never leaves the host.

use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::paths::Paths;

const START_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn dial(paths: &Paths) -> Result<()> {
    let stream = connect_or_start(paths).await?;
    let (mut sock_r, mut sock_w) = stream.into_split();

    // stdin → daemon. On EOF, half-close so the daemon sees the client left,
    // but keep relaying the daemon's remaining output.
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sock_w.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sock_w.shutdown().await;
    });

    // daemon → stdout, flushing every chunk (interactive traffic).
    let mut stdout = tokio::io::stdout();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = sock_r.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        if stdout.write_all(&buf[..n]).await.is_err() || stdout.flush().await.is_err() {
            break;
        }
    }
    Ok(())
}

async fn connect_or_start(paths: &Paths) -> Result<UnixStream> {
    if let Ok(s) = UnixStream::connect(&paths.socket).await {
        return Ok(s);
    }
    paths.ensure_dirs()?;
    spawn_daemon(paths)?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(s) = UnixStream::connect(&paths.socket).await {
            return Ok(s);
        }
        if tokio::time::Instant::now() > deadline {
            bail!(
                "workd did not start within {}s; see {}",
                START_TIMEOUT.as_secs(),
                paths.log_file.display()
            );
        }
    }
}

/// Start `workd serve` fully detached (own session, stdio to the log file) so
/// it outlives this SSH connection.
fn spawn_daemon(paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe().context("locating workd executable")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_file)
        .with_context(|| format!("opening {}", paths.log_file.display()))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--home")
        .arg(&paths.home)
        .arg("serve")
        .current_dir(&paths.home)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting workd serve")?;
    Ok(())
}
