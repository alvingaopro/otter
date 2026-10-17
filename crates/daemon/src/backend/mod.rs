//! Execution backends (design §12).
//!
//! A backend owns concrete processes for [`otter_core::Execution`]s and keeps
//! them alive independently of the daemon and of any client. tmux is the V1
//! backend; nothing outside this module knows about tmux.

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
    input: Option<mpsc::UnboundedSender<StreamInput>>,
}

pub enum StreamInput {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
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
    chunks: VecDeque<Vec<u8>>,
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

    /// Queue output for the client; `false` once it has left.
    pub fn push(&self, data: Vec<u8>) -> bool {
        let mut q = self.queue();
        if q.left {
            return false;
        }
        q.bytes += data.len();
        q.chunks.push_back(data);
        drop(q);
        self.queued.notify_waiters();
        true
    }

    /// Bytes queued and not yet taken.
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
    pub fn new(flow: Arc<Flow>, input: mpsc::UnboundedSender<StreamInput>) -> Self {
        Stream {
            flow,
            input: Some(input),
        }
    }

    /// What to show next; `None` once the attach is over.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            let queued = self.flow.queued.notified();
            tokio::pin!(queued);
            queued.as_mut().enable();
            {
                let mut q = self.flow.queue();
                if let Some(data) = q.chunks.pop_front() {
                    q.bytes -= data.len();
                    q.waiting = false;
                    return Some(data);
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
        if let Some(tx) = &self.input {
            let _ = tx.send(input);
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

    /// A stream attach at `cols`×`rows` with up to `history` lines of
    /// history; `None` if this backend has none (the PTY attach is used).
    async fn open_stream(
        &self,
        _backend_ref: &str,
        _cols: u16,
        _rows: u16,
        _history: u32,
    ) -> Result<Option<Stream>> {
        Ok(None)
    }

    /// Remove backend-specific chatter (e.g. tmux's "[detached …]") from
    /// attach output so backend details don't leak to the user.
    fn filter_attach_output(&self, _data: &mut Vec<u8>) {}

    /// Gracefully detach the attach client running on terminal `tty`.
    async fn detach_client(&self, tty: &str) -> Result<()>;
}
