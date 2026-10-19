//! Interactive attach: bridges a client connection to a backend attach client
//! (for tmux: `tmux attach-session`) running inside a PTY owned by the daemon.
//!
//! ```text
//! client ──frames──► daemon ──bytes──► PTY master ──► tmux client ──► session
//!        ◄─frames──         ◄─bytes───            ◄──
//! ```
//!
//! A client that keeps the history itself gets a stream instead (D-118): the
//! backend's snapshot of the session's terminal, then the process's own
//! output, so the client's terminal is the process's terminal.
//!
//! The attach ends when the client detaches or disconnects, or when the
//! session's process exits or is stopped. In every case the backend session
//! itself keeps running unless something else stopped it.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use otter_protocol::frame::{
    AttachExit, AttachExitReason, Control, Frame, read_frame, write_frame,
};
use otter_protocol::{AttachReady, Event, SessionAttach};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast::error::RecvError, mpsc};

use crate::backend::{Stream, StreamInput, StreamOutput};
use crate::daemon::Daemon;
use crate::server::respond;

/// How long to wait for a graceful detach before killing the attach client.
const DETACH_GRACE: Duration = Duration::from_secs(2);
/// Output in one frame, at most; a snapshot can be larger (frames take 4 MiB).
const FRAME_DATA: usize = 1 << 20;

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
    let (mut seen, mut events) = daemon.events.subscribe();
    let target = match daemon.attach_target(&params.session).await {
        Ok(t) => t,
        Err(e) => {
            respond(&mut w, id, Err(e)).await?;
            return Ok(());
        }
    };

    // The client's terminal keeps the history if it asked and the backend
    // can (D-118); otherwise it sees a terminal client's screen.
    let stream = match params.history {
        Some(lines) => match daemon
            .backend
            .open_stream(&target.backend_ref, params.cols, params.rows, lines)
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                tracing::warn!("stream attach failed, attaching a terminal client: {e:#}");
                None
            }
        },
        None => None,
    };
    let (mut viewer, ready) = match stream {
        Some(stream) => (
            Viewer::Stream(Some(stream)),
            AttachReady {
                history: true,
                controls: params.controls,
                ..target.ready.clone()
            },
        ),
        None => {
            let term = params.term.as_deref().unwrap_or("xterm-256color");
            let attach_cmd = daemon.backend.attach_command(&target.backend_ref, term);
            match open_pty(&attach_cmd, params.cols, params.rows) {
                Ok(pty) => (Viewer::pty(pty), target.ready.clone()),
                Err(e) => {
                    respond(
                        &mut w,
                        id,
                        Err(otter_protocol::RpcError::internal(format!("{e:#}"))),
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
    };
    respond(&mut w, id, Ok(serde_json::to_value(&ready)?)).await?;
    // Looking at the session settles what was only to be seen (D-099).
    daemon
        .engage(
            &target.workspace_id,
            &target.ready.session_id,
            crate::attention::Engaged::Looked,
        )
        .await;
    tracing::info!(execution = %target.ready.execution_id, history = ready.history, "attach started");

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
            out = viewer.recv() => match out {
                Some(out) => {
                    let frames = match out {
                        StreamOutput::Data(mut data) => {
                            // A terminal client's notices; a stream is the
                            // program's own output, all of it.
                            if matches!(viewer, Viewer::Pty { .. }) {
                                daemon.backend.filter_attach_output(&mut data);
                            }
                            data.chunks(FRAME_DATA).map(|d| Frame::Data(d.to_vec())).collect()
                        }
                        // Only to a client that takes them.
                        _ if !ready.controls => Vec::new(),
                        StreamOutput::Window(size) => vec![Frame::Control(Control::Window {
                            size: size.map(|(cols, rows)| [cols, rows]),
                        })],
                        StreamOutput::Snapshot { lines, more } => {
                            vec![Frame::Control(Control::Snapshot { lines, more })]
                        }
                    };
                    for frame in frames {
                        if client_gone {
                            break;
                        }
                        if write_frame(&mut w, &frame).await.is_err() {
                            client_gone = true;
                            viewer.kill();
                        }
                    }
                }
                // The attach client exited (for a PTY: and the PTY closed).
                None => break,
            },
            frame = frame_rx.recv(), if !client_gone => match frame.flatten() {
                Some(Frame::Data(data)) => viewer.input(data),
                Some(Frame::Resize { cols, rows }) => viewer.resize(cols, rows),
                Some(Frame::Detach) => {
                    ending.get_or_insert(exit(AttachExitReason::Detached, None));
                    begin_detach(&daemon, &mut viewer, &mut detach_deadline);
                }
                Some(Frame::Control(control)) => {
                    if ready.controls {
                        viewer.control(control);
                    }
                }
                Some(Frame::Exit(_)) => {}
                None => {
                    // Client disconnected without detaching.
                    client_gone = true;
                    viewer.kill();
                }
            },
            ev = events.recv() => {
                let ended = match ev {
                    Ok(rec) => {
                        seen = rec.seq;
                        ends_attach(&rec.event, &target.workspace_id, &target.ready)
                    }
                    // Fell behind: check what was missed in the log, so an
                    // exit can't slip by and leave the attach hanging. (If they
                    // were rotated away, the attach still ends when its
                    // terminal closes.)
                    Err(RecvError::Lagged(_)) => {
                        let missed = daemon.events.since(seen).ok().flatten().unwrap_or_default();
                        seen = missed.last().map_or(seen, |r| r.seq);
                        missed.iter().find_map(|r| {
                            ends_attach(&r.event, &target.workspace_id, &target.ready)
                        })
                    }
                    Err(RecvError::Closed) => Some(exit(AttachExitReason::Ended, None)),
                };
                if let Some(reason) = ended {
                    ending.get_or_insert(reason);
                    begin_detach(&daemon, &mut viewer, &mut detach_deadline);
                }
            }
            _ = deadline => {
                detach_deadline = None;
                viewer.kill();
            }
        }
    }

    viewer.close();
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
    workspace_id: &otter_core::WorkspaceId,
    target: &otter_protocol::AttachReady,
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
    viewer: &mut Viewer,
    deadline: &mut Option<tokio::time::Instant>,
) {
    if deadline.is_some() {
        return;
    }
    *deadline = Some(tokio::time::Instant::now() + DETACH_GRACE);
    match viewer {
        Viewer::Pty { tty: Some(tty), .. } => {
            let daemon = daemon.clone();
            let tty = tty.clone();
            tokio::spawn(async move {
                if let Err(e) = daemon.backend.detach_client(&tty).await {
                    tracing::debug!("graceful detach failed: {e:#}");
                }
            });
        }
        Viewer::Pty { tty: None, .. } => {}
        Viewer::Stream(stream) => {
            if let Some(stream) = stream {
                stream.detach();
            }
        }
    }
}

/// What the client is looking through: a terminal client in a PTY, or a
/// stream (`None` once killed).
enum Viewer {
    Pty {
        master: Box<dyn portable_pty::MasterPty + Send>,
        out_rx: mpsc::Receiver<Output>,
        in_tx: std::sync::mpsc::Sender<Vec<u8>>,
        child: Box<dyn portable_pty::Child + Send + Sync>,
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
        tty: Option<String>,
    },
    Stream(Option<Stream>),
}

impl Viewer {
    fn pty(pty: Pty) -> Self {
        let Pty {
            master,
            mut reader,
            mut writer,
            child,
            tty,
        } = pty;
        let killer = child.clone_killer();

        // PTY output → channel (blocking reads on a dedicated thread).
        let (out_tx, out_rx) = mpsc::channel::<Output>(64);
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

        Viewer::Pty {
            master,
            out_rx,
            in_tx,
            child,
            killer,
            tty,
        }
    }

    /// Output for the client; `None` once the attach client is gone.
    async fn recv(&mut self) -> Option<StreamOutput> {
        match self {
            Viewer::Pty { out_rx, .. } => match out_rx.recv().await {
                Some(Output::Data(data)) => Some(StreamOutput::Data(data)),
                Some(Output::Eof) | None => None,
            },
            Viewer::Stream(Some(stream)) => stream.recv().await,
            Viewer::Stream(None) => None,
        }
    }

    fn input(&self, data: Vec<u8>) {
        match self {
            Viewer::Pty { in_tx, .. } => {
                let _ = in_tx.send(data);
            }
            Viewer::Stream(Some(stream)) => stream.send(StreamInput::Data(data)),
            Viewer::Stream(None) => {}
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        match self {
            Viewer::Pty { master, .. } => {
                let _ = master.resize(pty_size(cols, rows));
            }
            Viewer::Stream(Some(stream)) => stream.send(StreamInput::Resize { cols, rows }),
            Viewer::Stream(None) => {}
        }
    }

    /// A control frame from the client (only a stream takes them).
    fn control(&self, control: Control) {
        let Viewer::Stream(Some(stream)) = self else {
            return;
        };
        match control {
            Control::View {
                visible,
                cols,
                rows,
            } => stream.send(StreamInput::View {
                visible,
                cols,
                rows,
            }),
            Control::History { lines } => stream.send(StreamInput::History(lines)),
            Control::Window { .. } | Control::Snapshot { .. } | Control::Unknown => {}
        }
    }

    fn kill(&mut self) {
        match self {
            Viewer::Pty { killer, .. } => {
                let _ = killer.kill();
            }
            // Dropping a stream ends its client at once.
            Viewer::Stream(stream) => *stream = None,
        }
    }

    /// Release everything once the attach is over.
    fn close(self) {
        if let Viewer::Pty { mut child, .. } = self {
            tokio::task::spawn_blocking(move || {
                let _ = child.wait();
            });
        }
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
    // Not `take_writer()`: that writer sends "\n" + EOF into the PTY when
    // dropped, which the attach client forwards to the session — a shell would
    // exit if the daemon stopped mid-attach. A plain dup of the fd just closes.
    let fd = pair.master.as_raw_fd().context("pty has no fd")?;
    // SAFETY: `fd` belongs to `pair.master`, which is alive here.
    let writer = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .context("pty writer")?;
    let writer: Box<dyn Write + Send> = Box::new(std::fs::File::from(writer));
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
