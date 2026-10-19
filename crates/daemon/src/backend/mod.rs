//! Execution backends (design §12).
//!
//! A backend owns concrete processes for [`otter_core::Execution`]s and keeps
//! them alive independently of the daemon and of any client. tmux is the V1
//! backend; nothing outside this module knows about tmux.

mod queries;
pub mod tmux;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{Notify, mpsc};

use crate::env::EnvMap;

pub struct LaunchSpec {
    /// Handle to create the process under (the execution id).
    pub backend_ref: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Complete environment for the process.
    pub env: EnvMap,
}

pub struct Launched {
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessState {
    Alive { pid: Option<u32> },
    Dead { exit_code: Option<i32> },
}

/// How to run an interactive client attached to a process. The daemon runs it
/// inside a PTY and bridges that PTY to the remote user.
pub struct AttachCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: EnvMap,
}

/// An attach where the client's terminal is the process's terminal (D-118):
/// first a snapshot that rebuilds it (screen, history, cursor, modes), then
/// the process's own output, so the client keeps the history itself.
pub struct Stream {
    flow: Arc<Flow>,
    /// Typing, to the backend's writer; dropping it detaches.
    input: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// Size, view and history, to the backend's reader.
    controls: mpsc::UnboundedSender<StreamInput>,
}

/// How a stream attach starts.
#[derive(Clone, Copy, Debug)]
pub struct StreamOptions {
    pub cols: u16,
    pub rows: u16,
    /// Lines of history to send with the first snapshot.
    pub history: u32,
    /// The client's terminal is the user's own (D-119): no queries reach it,
    /// it's never reset, and catching it up sends no history again.
    pub own_terminal: bool,
}

pub enum StreamInput {
    Data(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// The client's terminal is on screen or not (D-119), at this size.
    View {
        visible: bool,
        cols: u16,
        rows: u16,
    },
    /// Rebuild the client's terminal with up to this many lines of history.
    History(u32),
}

/// What a stream gives the client.
#[derive(Debug, PartialEq)]
pub enum StreamOutput {
    Data(Vec<u8>),
    /// Another client sizes the window, and output is laid out for that size;
    /// `None` once it's the client's own again.
    Window(Option<(u16, u16)>),
    /// A snapshot ended, with this many lines of history; `more`: the
    /// session holds earlier ones.
    Snapshot {
        lines: u32,
        more: bool,
    },
}

/// Output waiting for the client, between a stream's two ends. The backend
/// queues it and may drop all of it (the client fell behind, and a fresh
/// snapshot will do instead), so what a stuck client costs is bounded.
#[derive(Default)]
pub struct Flow {
    queue: Mutex<Queue>,
    /// Output was queued, or there won't be more.
    queued: Notify,
    /// The client took everything and waits for more, or it left.
    client: Notify,
}

#[derive(Default)]
struct Queue {
    /// Each with the bytes it counts as output waiting (snapshots don't).
    chunks: VecDeque<(StreamOutput, usize)>,
    bytes: usize,
    /// The client found nothing queued and waits.
    waiting: bool,
    /// No more output is coming.
    done: bool,
    /// The client dropped the stream.
    left: bool,
}

impl Flow {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue the program's output for the client; `false` once it has left.
    pub fn push_output(&self, data: Vec<u8>) -> bool {
        let n = data.len();
        self.push_counted(StreamOutput::Data(data), n)
    }

    /// Queue something that isn't the program's output (a snapshot, a
    /// notice): it doesn't count towards falling behind.
    pub fn push(&self, out: StreamOutput) -> bool {
        self.push_counted(out, 0)
    }

    fn push_counted(&self, out: StreamOutput, counted: usize) -> bool {
        let mut q = self.queue();
        if q.left {
            return false;
        }
        q.bytes += counted;
        q.chunks.push_back((out, counted));
        drop(q);
        self.queued.notify_waiters();
        true
    }

    /// Bytes of the program's output queued and not yet taken.
    pub fn bytes(&self) -> usize {
        self.queue().bytes
    }

    /// Drop everything queued. The client counts as waiting again only once
    /// it comes back for more.
    pub fn clear(&self) {
        let mut q = self.queue();
        q.chunks.clear();
        q.bytes = 0;
        q.waiting = false;
    }

    /// No more output: the client gets what's queued, then the end.
    pub fn done(&self) {
        self.queue().done = true;
        self.queued.notify_waiters();
    }

    /// Wait until the client leaves (`false`) or, with `for_more`, until it
    /// took everything queued and waits for more (`true`).
    pub async fn client(&self, for_more: bool) -> bool {
        loop {
            let changed = self.client.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let q = self.queue();
                if q.left {
                    return false;
                }
                if for_more && q.waiting && q.chunks.is_empty() {
                    return true;
                }
            }
            changed.await;
        }
    }
}

impl Stream {
    pub fn new(
        flow: Arc<Flow>,
        input: mpsc::UnboundedSender<Vec<u8>>,
        controls: mpsc::UnboundedSender<StreamInput>,
    ) -> Self {
        Stream {
            flow,
            input: Some(input),
            controls,
        }
    }

    /// What to give the client next; `None` once the attach is over.
    pub async fn recv(&mut self) -> Option<StreamOutput> {
        loop {
            let queued = self.flow.queued.notified();
            tokio::pin!(queued);
            queued.as_mut().enable();
            {
                let mut q = self.flow.queue();
                if let Some((out, counted)) = q.chunks.pop_front() {
                    q.bytes -= counted;
                    q.waiting = false;
                    return Some(out);
                }
                if q.done {
                    return None;
                }
                if !q.waiting {
                    q.waiting = true;
                    drop(q);
                    self.flow.client.notify_waiters();
                }
            }
            queued.await;
        }
    }

    pub fn send(&self, input: StreamInput) {
        match input {
            StreamInput::Data(data) => {
                if let Some(tx) = &self.input {
                    let _ = tx.send(data);
                }
            }
            other => {
                let _ = self.controls.send(other);
            }
        }
    }

    /// Leave without touching the process; output ends soon after. Dropping
    /// the stream instead ends it at once.
    pub fn detach(&mut self) {
        self.input = None;
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut q = self.flow.queue();
        q.left = true;
        q.chunks.clear();
        q.bytes = 0;
        drop(q);
        self.flow.client.notify_waiters();
    }
}

#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    fn name(&self) -> &'static str;

    async fn launch(&self, spec: &LaunchSpec) -> Result<Launched>;

    /// Snapshot of every process this backend currently holds, keyed by
    /// backend ref. Processes that exited but whose output is still held are
    /// reported as [`ProcessState::Dead`]; forgotten ones are absent.
    async fn inspect(&self) -> Result<HashMap<String, ProcessState>>;

    /// Terminate the process and release everything held for it. Succeeds if
    /// it is already gone.
    async fn terminate(&self, backend_ref: &str) -> Result<()>;

    /// Screen contents plus up to `lines` lines of scrollback (`None` = all).
    async fn capture(&self, backend_ref: &str, lines: Option<u32>) -> Result<String>;

    /// Type `text` into the process's terminal, optionally followed by Enter.
    async fn send_text(&self, backend_ref: &str, text: &str, enter: bool) -> Result<()>;

    fn attach_command(&self, backend_ref: &str, term: &str) -> AttachCommand;

    /// A stream attach; `None` if this backend has none (the PTY attach is
    /// used).
    async fn open_stream(&self, _backend_ref: &str, _how: StreamOptions) -> Result<Option<Stream>> {
        Ok(None)
    }

    /// Remove backend-specific chatter (e.g. tmux's "[detached …]") from
    /// attach output so backend details don't leak to the user.
    fn filter_attach_output(&self, _data: &mut Vec<u8>) {}

    /// Gracefully detach the attach client running on terminal `tty`.
    async fn detach_client(&self, tty: &str) -> Result<()>;
}
