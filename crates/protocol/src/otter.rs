//! Otter's conversation on a host (D-093): `otter.*` requests.

use serde::{Deserialize, Serialize};

/// The developer's message to Otter on this host.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OtterSend {
    /// Chosen by the client: the same command again adds nothing.
    pub command_id: String,
    pub text: String,
}

/// The conversation's history (Otter's tool calls) after a cursor.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OtterEvents {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Which of Otter's memories (D-113).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Across every conversation on this host.
    Host,
    /// One conversation: a task's, or Otter's on the host.
    Conversation,
}

/// Otter's memories as a conversation sees them: the host's, and that
/// conversation's — the task's when `task` is given, else Otter's on the
/// host.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryGet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

/// The developer's edit of one memory: all of what it says.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemorySet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub memory: MemoryScope,
    pub text: String,
    /// What it said when the edit began: if it says something else now
    /// (Otter noted something meanwhile), nothing changes (`conflict`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

/// Otter's memories, as `memory.get` and `memory.set` answer.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OtterMemory {
    pub host: String,
    pub conversation: String,
    /// The most one memory holds, in characters.
    pub max_chars: u32,
}

/// What Otter did on its tools across the host's tasks (D-193), newest
/// first: `otter.actions`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OtterActions {
    /// At most this many (default 100, at most 500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One thing Otter did (a tool that changes something, or one of the SDK's
/// own tools), as its task's history has it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OtterAction {
    pub task_id: otter_core::TaskId,
    pub task_title: String,
    /// Its place in the task's history.
    pub seq: u64,
    pub at: otter_core::Timestamp,
    pub tool: String,
    /// What started it: `developer` (their message), `event` (a watch, a
    /// sub-task, a change) or `auto` (the controller, where the tool is
    /// automatic).
    pub origin: String,
    /// The developer's message it acted on, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why it did nothing, when it didn't: a refusal's code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What undoes it, when something does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<Undo>,
}

/// What undoes one of Otter's actions: an action on a task.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Undo {
    pub task_id: otter_core::TaskId,
    pub action: otter_core::task::TaskAction,
}
