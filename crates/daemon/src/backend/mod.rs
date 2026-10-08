//! Execution backends (design §12).
//!
//! A backend owns concrete processes for [`otter_core::Execution`]s and keeps
//! them alive independently of the daemon and of any client. tmux is the V1
//! backend; nothing outside this module knows about tmux.

pub mod tmux;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

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

    /// Remove backend-specific chatter (e.g. tmux's "[detached …]") from
    /// attach output so backend details don't leak to the user.
    fn filter_attach_output(&self, _data: &mut Vec<u8>) {}

    /// Gracefully detach the attach client running on terminal `tty`.
    async fn detach_client(&self, tty: &str) -> Result<()>;
}
