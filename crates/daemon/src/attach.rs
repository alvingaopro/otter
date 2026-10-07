//! Interactive attach: bridges a client connection to a backend attach client
//! (for tmux: `tmux attach-session`) running inside a PTY owned by the daemon.
//!
//! ```text
//! client ──frames──► daemon ──bytes──► PTY master ──► tmux client ──► session
//!        ◄─frames──         ◄─bytes───            ◄──
//! ```
//!
//! The attach ends when the client detaches or disconnects, or when the
//! session's process exits or is stopped. In every case the backend session
//! itself keeps running unless something else stopped it.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast::error::RecvError, mpsc};
use workd_protocol::frame::{AttachExit, AttachExitReason, Frame, read_frame, write_frame};
use workd_protocol::{Event, SessionAttach};

use crate::daemon::Daemon;
use crate::server::respond;

/// How long to wait for a graceful detach before killing the attach client.
const DETACH_GRACE: Duration = Duration::from_secs(2);

enum Output {
    Data(Vec<u8>),
    Eof,
}

pub async fn run(
    daemon: Arc<Daemon>,
    id: u64,
    params: SessionAttach,
    r: BufReader<OwnedReadHalf>,
    mut w: OwnedWriteHalf,
) -> Result<()> {
    // Subscribe before resolving the target so an exit between the two can't
    // be missed.
    let (_, mut events) = daemon.events.subscribe();
    let target = match daemon.attach_target(&params.session).await {
        Ok(t) => t,
        Err(e) => {
            respond(&mut w, id, Err(e)).await?;
            return Ok(());
        }
    };

    let term = params.term.as_deref().unwrap_or("xterm-256color");
    let attach_cmd = daemon.backend.attach_command(&target.backend_ref, term);
    let pty = match open_pty(&attach_cmd, params.cols, params.rows) {
        Ok(p) => p,
        Err(e) => {
            respond(
                &mut w,
                id,
                Err(workd_protocol::RpcError::internal(format!("{e:#}"))),
            )
            .await?;
            return Ok(());
        }
    };
    respond(&mut w, id, Ok(serde_json::to_value(&target.ready)?)).await?;
    // Looking at the session counts as seeing whatever it was asking for.
    daemon
        .engage(&target.workspace_id, &target.ready.session_id)
        .await;
    tracing::info!(execution = %target.ready.execution_id, "attach started");

    let Pty {
        master,
        mut reader,
        mut writer,
        mut child,
        tty,
    } = pty;
    let mut killer = child.clone_killer();

    // PTY output → channel (blocking reads on a dedicated thread).
    let (out_tx, mut out_rx) = mpsc::channel::<Output>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx
                        .blocking_send(Output::Data(buf[..n].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        let _ = out_tx.blocking_send(Output::Eof);
    });

    // Channel → PTY input (blocking writes on a dedicated thread).
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for data in in_rx {
            if writer
                .write_all(&data)
                .and_then(|_| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });

    // Client frames → channel. Frame reads are not cancel-safe, so they get
    // their own task instead of living in the select below.
    let (frame_tx, mut frame_rx) = mpsc::channel::<Option<Frame>>(64);
    tokio::spawn(async move {
        let mut r = r;
        loop {
            let frame = read_frame(&mut r).await.ok().flatten();
            let done = frame.is_none();
            if frame_tx.send(frame).await.is_err() || done {
                return;
            }
        }
    });

    let mut ending: Option<AttachExit> = None;
    let mut client_gone = false;
    let mut detach_deadline: Option<tokio::time::Instant> = None;

    loop {
        let deadline = async {
            match detach_deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            out = out_rx.recv() => match out {
                Some(Output::Data(mut data)) => {
                    daemon.backend.filter_attach_output(&mut data);
                    if !client_gone && !data.is_empty() && write_frame(&mut w, &Frame::Data(data)).await.is_err() {
                        client_gone = true;
                        let _ = killer.kill();
                    }
                }
                // The attach client exited and the PTY closed.
                Some(Output::Eof) | None => break,
            },
            frame = frame_rx.recv(), if !client_gone => match frame.flatten() {
                Some(Frame::Data(data)) => { let _ = in_tx.send(data); }
                Some(Frame::Resize { cols, rows }) => {
                    let _ = master.resize(pty_size(cols, rows));
                }
                Some(Frame::Detach) => {
                    ending.get_or_insert(exit(AttachExitReason::Detached, None));
                    begin_detach(&daemon, tty.as_deref(), &mut detach_deadline);
                }
                Some(Frame::Exit(_)) => {}
                None => {
                    // Client disconnected without detaching.
                    client_gone = true;
                    let _ = killer.kill();
                }
            },
            ev = events.recv() => {
                let ended = match ev {
                    Ok(rec) => ends_attach(&rec.event, &target.workspace_id, &target.ready),
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => Some(exit(AttachExitReason::Ended, None)),
                };
                if let Some(reason) = ended {
                    ending.get_or_insert(reason);
                    begin_detach(&daemon, tty.as_deref(), &mut detach_deadline);
                }
            }
            _ = deadline => {
                detach_deadline = None;
                let _ = killer.kill();
            }
        }
    }

    tokio::task::spawn_blocking(move || {
        let _ = child.wait();
    });
    drop(in_tx);
    drop(master);
    if !client_gone {
        let ending = ending.unwrap_or_else(|| exit(AttachExitReason::Ended, None));
        let _ = write_frame(&mut w, &Frame::Exit(ending)).await;
    }
    tracing::info!(execution = %target.ready.execution_id, "attach ended");
    Ok(())
}

/// Whether `event` means the attached execution is over.
fn ends_attach(
    event: &Event,
    workspace_id: &workd_core::WorkspaceId,
    target: &workd_protocol::AttachReady,
) -> Option<AttachExit> {
    match event {
        Event::ExecutionExited {
            execution_id,
            exit_code,
            ..
        } if *execution_id == target.execution_id => {
            Some(exit(AttachExitReason::Exited, *exit_code))
        }
        Event::ExecutionLost { execution_id, .. } if *execution_id == target.execution_id => {
            Some(exit(AttachExitReason::Ended, None))
        }
        Event::SessionStopped { session_id, .. } | Event::SessionDeleted { session_id, .. }
            if *session_id == target.session_id =>
        {
            Some(exit(AttachExitReason::Ended, None))
        }
        Event::WorkspaceDeleted {
            workspace_id: ws, ..
        } if ws == workspace_id => Some(exit(AttachExitReason::Ended, None)),
        _ => None,
    }
}

/// Ask the attach client to detach so it restores the user's terminal, and
/// arm a deadline after which it is killed instead.
fn begin_detach(
    daemon: &Arc<Daemon>,
    tty: Option<&str>,
    deadline: &mut Option<tokio::time::Instant>,
) {
    if deadline.is_some() {
        return;
    }
    *deadline = Some(tokio::time::Instant::now() + DETACH_GRACE);
    if let Some(tty) = tty {
        let daemon = daemon.clone();
        let tty = tty.to_owned();
        tokio::spawn(async move {
            if let Err(e) = daemon.backend.detach_client(&tty).await {
                tracing::debug!("graceful detach failed: {e:#}");
            }
        });
    }
}

fn exit(reason: AttachExitReason, exit_code: Option<i32>) -> AttachExit {
    AttachExit {
        reason,
        exit_code,
        message: None,
    }
}

struct Pty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    tty: Option<String>,
}

fn open_pty(cmd: &crate::backend::AttachCommand, cols: u16, rows: u16) -> Result<Pty> {
    let pair = native_pty_system()
        .openpty(pty_size(cols, rows))
        .context("opening pty")?;
    let mut builder = CommandBuilder::new(&cmd.program);
    builder.args(&cmd.args);
    builder.env_clear();
    for (k, v) in &cmd.env {
        builder.env(k, v);
    }
    if let Some(home) = cmd.env.get("HOME") {
        builder.cwd(home);
    }
    let child = pair
        .slave
        .spawn_command(builder)
        .context("starting attach client")?;
    drop(pair.slave);
    let tty = pair
        .master
        .tty_name()
        .map(|p| p.to_string_lossy().into_owned());
    let reader = pair.master.try_clone_reader().context("pty reader")?;
    let writer = pair.master.take_writer().context("pty writer")?;
    Ok(Pty {
        master: pair.master,
        reader,
        writer,
        child,
        tty,
    })
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    // Some clients report 0x0 when they can't tell; don't shrink the session
    // window to nothing because of it.
    let (cols, rows) = if cols == 0 || rows == 0 {
        (80, 24)
    } else {
        (cols, rows)
    };
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
