//! `workctl attach`: an interactive terminal on a session.
//!
//! The local terminal is put in raw mode and bridged to the session through
//! the daemon. `Ctrl-]` detaches; the session keeps running.

use std::io::{IsTerminal, Read, Write};

use anyhow::{Result, bail};
use tokio::sync::mpsc;
use workd_client::Connection;
use workd_protocol::frame::{AttachExit, AttachExitReason, Frame};
use workd_protocol::{SessionAttach, SessionRef};

/// Ctrl-]
pub const DETACH_KEY: u8 = 0x1d;

/// How long to wait for the daemon to confirm a detach before giving up on the
/// connection (e.g. the network is gone and nothing will ever answer).
const DETACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Escape sequences that undo common terminal modes, used if the connection
/// drops before the remote side could restore the terminal itself.
const RESET_TERMINAL: &str =
    "\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[0m";

/// The local terminal size, falling back to 80x24 when it is unknown (some
/// PTYs report 0x0).
fn terminal_size() -> (u16, u16) {
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => (cols, rows),
        _ => (80, 24),
    }
}

struct RawMode;

impl RawMode {
    fn enable() -> Result<RawMode> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub async fn run(conn: Connection, workspace_id: &str, session: &str, label: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("attach needs an interactive terminal (try `workctl logs` / `workctl send`)");
    }
    let (cols, rows) = terminal_size();
    let term = std::env::var("TERM").ok().filter(|t| !t.is_empty());
    let (_ready, mut reader, mut writer) = conn
        .attach(SessionAttach {
            session: SessionRef {
                workspace: workspace_id.to_owned(),
                session: session.to_owned(),
            },
            cols,
            rows,
            term,
        })
        .await?;

    let raw = RawMode::enable()?;

    // Local keyboard → channel (blocking reads on a plain thread).
    let (key_tx, mut key_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if key_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Remote frames → channel (frame reads aren't cancel-safe).
    let (frame_tx, mut frame_rx) = mpsc::channel::<Option<Frame>>(64);
    tokio::spawn(async move {
        loop {
            let frame = reader.next().await.ok().flatten();
            let done = frame.is_none();
            if frame_tx.send(frame).await.is_err() || done {
                break;
            }
        }
    });

    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdout = std::io::stdout();
    let mut detach_sent: Option<tokio::time::Instant> = None;

    let outcome: Option<AttachExit> = loop {
        let give_up = async {
            match detach_sent {
                Some(at) => tokio::time::sleep_until(at + DETACH_TIMEOUT).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            keys = key_rx.recv(), if detach_sent.is_none() => {
                let Some(keys) = keys else {
                    let _ = writer.send(&Frame::Detach).await;
                    detach_sent = Some(tokio::time::Instant::now());
                    continue;
                };
                match keys.iter().position(|&b| b == DETACH_KEY) {
                    Some(pos) => {
                        if pos > 0 {
                            let _ = writer.send(&Frame::Data(keys[..pos].to_vec())).await;
                        }
                        let _ = writer.send(&Frame::Detach).await;
                        detach_sent = Some(tokio::time::Instant::now());
                    }
                    None => {
                        let _ = writer.send(&Frame::Data(keys)).await;
                    }
                }
            }
            _ = winch.recv() => {
                let (cols, rows) = terminal_size();
                let _ = writer.send(&Frame::Resize { cols, rows }).await;
            }
            frame = frame_rx.recv() => match frame.flatten() {
                Some(Frame::Data(data)) => {
                    let _ = stdout.write_all(&data);
                    let _ = stdout.flush();
                }
                Some(Frame::Exit(exit)) => break Some(exit),
                Some(_) => {}
                None => break None,
            },
            _ = give_up => break None,
        }
    };

    drop(raw);
    let message = match outcome {
        Some(AttachExit {
            reason: AttachExitReason::Detached,
            ..
        }) => format!("[detached from {label}]"),
        Some(AttachExit {
            reason: AttachExitReason::Exited,
            exit_code,
            ..
        }) => match exit_code {
            Some(code) => format!("[{label} exited with status {code}]"),
            None => format!("[{label} exited]"),
        },
        Some(AttachExit {
            reason: AttachExitReason::Ended,
            ..
        }) => format!("[{label} ended]"),
        Some(AttachExit {
            reason: AttachExitReason::Error,
            message,
            ..
        }) => format!("[attach error: {}]", message.unwrap_or_default()),
        None => {
            print!("{RESET_TERMINAL}");
            format!("[connection to {label} lost; the session keeps running]")
        }
    };
    let _ = stdout.flush();
    eprintln!("{message}");
    Ok(())
}
