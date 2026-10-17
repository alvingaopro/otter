//! Execution backends (design §12).
//!
//! A backend owns concrete processes for [`otter_core::Execution`]s and keeps
//! them alive independently of the daemon and of any client. tmux is the V1
//! backend; nothing outside this module knows about tmux.

pub mod tmux;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

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
    output: mpsc::UnboundedReceiver<(u64, Vec<u8>)>,
    input: Option<mpsc::UnboundedSender<StreamInput>>,
    flow: Arc<Flow>,
}

pub enum StreamInput {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Output not yet taken by the client, shared by a stream's two ends. When
/// too much is waiting, the backend drops it and sends a fresh snapshot
/// instead, under a new generation; what's still queued from an older one
/// is skipped.
#[derive(Default)]
pub struct Flow {
    pub queued: AtomicUsize,
    pub generation: AtomicU64,
}

impl Stream {
    pub fn new(
        output: mpsc::UnboundedReceiver<(u64, Vec<u8>)>,
        input: mpsc::UnboundedSender<StreamInput>,
        flow: Arc<Flow>,
    ) -> Self {
        Stream {
            output,
            input: Some(input),
            flow,
        }
    }

    /// What to show next; `None` once the attach is over.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        loop {
            let (generation, data) = self.output.recv().await?;
            self.flow.queued.fetch_sub(data.len(), Ordering::Relaxed);
            if generation == self.flow.generation.load(Ordering::Relaxed) {
                return Some(data);
            }
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
