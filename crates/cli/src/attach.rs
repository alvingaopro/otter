//! `otter attach`: an interactive terminal on a session.
//!
//! The local terminal is put in raw mode and bridged to the session through
//! the daemon. `Ctrl-]` detaches; the session keeps running.
//!
//! With `--scrollback` (D-119) the local terminal is the session's terminal:
//! it gets the program's own output, after the session's history, and keeps
//! it as its own scrollback. The daemon keeps the program's queries away
//! from it, and on the way out the program's modes are undone here.
//!
//! While attached, sign-in pages that tools on the host open (browser login,
//! D-032) open in this machine's browser, as they do with the app.

use std::io::{IsTerminal, Read, Write};

use anyhow::{Result, bail};
use std::sync::Arc;

use otter_client::login::{self, Logins, Opener};
use otter_client::{Connection, Transport};
use otter_protocol::frame::{AttachExit, AttachExitReason, Frame};
use otter_protocol::{SessionAttach, SessionRef};
use tokio::sync::mpsc;

/// Ctrl-]
pub const DETACH_KEY: u8 = 0x1d;
/// Ctrl-] as a terminal sends it once a program asked for more of the
/// keyboard: the kitty protocol, xterm's modifyOtherKeys.
const DETACH_KEYS: [&[u8]; 3] = [&[DETACH_KEY], b"\x1b[93;5u", b"\x1b[27;5;93~"];
/// Lines of history put in this terminal's scrollback with `--scrollback`.
const SCROLLBACK_LINES: u32 = 5000;

/// How long to wait for the daemon to confirm a detach before giving up on the
/// connection (e.g. the network is gone and nothing will ever answer).
const DETACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Escape sequences that undo common terminal modes, used if the connection
/// drops before the remote side could restore the terminal itself.
const RESET_TERMINAL: &str =
    "\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[0m";
/// What a program may have set in this terminal, directly, with
/// `--scrollback`, undone: besides the above, synchronized output, mouse
/// encoding, cursor and keypad keys, the scroll region (cursor kept), insert
/// and wrap, the keyboard protocols, the cursor's shape and colour.
const RESET_STREAM: &str = "\x1b[?2026l\x1b[?1005l\x1b[?1l\x1b>\x1b7\x1b[r\x1b8\x1b[4l\x1b[?7h\
    \x1b[<u\x1b[>4;0m\x1b[0 q\x1b]112\x07";

/// Where the first detach key in `keys` is, and how long it is.
fn detach_key(keys: &[u8]) -> Option<(usize, usize)> {
    DETACH_KEYS
        .iter()
        .filter_map(|key| {
            keys.windows(key.len())
                .position(|w| w == *key)
                .map(|at| (at, key.len()))
        })
        .min()
}

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

/// Where the session is: its host's name and how to reach it.
pub struct Host {
    pub name: String,
    pub transport: Transport,
}

pub async fn run(
    conn: Connection,
    host: Host,
    workspace_id: &str,
    session: &str,
    label: &str,
    scrollback: bool,
) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("attach needs an interactive terminal (try `otter logs` / `otter send`)");
    }
    // Inside an Otter session this machine is itself a host: its "browser"
    // would hand the page straight back. Otherwise a failure here only means
    // pages print their URL instead.
    let logins = if std::env::var_os("OTTER_SESSION_ID").is_some() {
        None
    } else {
        let open: Opener = Arc::new(login::system_open);
        Logins::start(host.transport, &host.name, open).await.ok()
    };
    let (cols, rows) = terminal_size();
    let term = std::env::var("TERM").ok().filter(|t| !t.is_empty());
    let (ready, mut reader, mut writer) = conn
        .attach(SessionAttach {
            session: SessionRef {
                workspace: workspace_id.to_owned(),
                session: session.to_owned(),
            },
            cols,
            rows,
            term,
            // Without `--scrollback`, the session's screen; the wheel scrolls
            // the session's history.
            history: scrollback.then_some(SCROLLBACK_LINES),
            controls: false,
            own_terminal: scrollback,
        })
        .await?;
    // An older host gives the screen either way.
    let stream = ready.history;

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
                match detach_key(&keys).map(|(at, _)| at) {
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

    if stream {
        // The program's modes, then below its screen: the session stays in
        // the scrollback above.
        let (_, rows) = terminal_size();
        print!("{RESET_TERMINAL}{RESET_STREAM}\x1b[{rows};1H\r\n");
        let _ = stdout.flush();
    }
    drop(raw);
    if let Some(logins) = logins {
        logins.stop().await;
    }
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
            if !stream {
                print!("{RESET_TERMINAL}");
            }
            format!("[connection to {label} lost; the session keeps running]")
        }
    };
    let _ = stdout.flush();
    eprintln!("{message}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_close_bracket_detaches_however_it_comes() {
        assert_eq!(detach_key(b"ab\x1dcd"), Some((2, 1)));
        assert_eq!(detach_key(b"x\x1b[93;5u"), Some((1, 7)));
        assert_eq!(detach_key(b"\x1b[27;5;93~"), Some((0, 10)));
        assert_eq!(detach_key(b"\x1b[93u"), None, "] alone");
        assert_eq!(detach_key(b"plain"), None);
    }
}
