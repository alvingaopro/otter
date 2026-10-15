//! Task requests (D-043). Results are [`otter_core::task::Task`]s
//! and their history ([`otter_core::task::TaskEventRecord`]).
//!
//! Every request that changes something carries a `command_id` chosen by
//! the client (unique per intent, e.g. a random string). The daemon applies
//! a command once: a repeat — a retry after a dropped connection — gets the
//! task as it is, without applying anything again.

use otter_core::task::TaskAction;
use serde::{Deserialize, Serialize};

/// Identifies a task by id.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskRef {
    pub task: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCreate {
    pub command_id: String,
    pub title: String,
    /// What should be built, as the developer puts it. Becomes the first
    /// message of the conversation.
    #[serde(default)]
    pub request: String,
    /// A workspace (id or name) on this host to do the work in. Without
    /// one, the task blocks when it starts until one is chosen
    /// (`task.act` `set_workspace`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Only an answer is wanted: the coding agent only reads (D-092). Needs
    /// the coding agent on the SDK runtime.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub answer_only: bool,
}

/// A message from the developer to the task's Control Agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskSend {
    pub command_id: String,
    pub task: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskAct {
    pub command_id: String,
    pub task: String,
    #[serde(flatten)]
    pub action: TaskAction,
}

/// A file the task's checks produced (a screenshot), by name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskArtifact {
    pub task: String,
    /// The file's name in the task's artifacts (no directories).
    pub name: String,
}

/// A task's history after a cursor.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskEvents {
    pub task: String,
    /// Return entries with `seq > after` (all when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}
