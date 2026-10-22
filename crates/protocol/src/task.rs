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
    /// Where the work is done: a workspace on this host, or one made for the
    /// task (D-179). Without one, the task blocks when it starts until one is
    /// chosen (`task.act` `set_workspace`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceChoice>,
    /// Only an answer is wanted: the coding agent only reads (D-092). Needs
    /// the coding agent on the SDK runtime.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub answer_only: bool,
}

/// Where a new task works (D-179). A plain string is a workspace by id or
/// name, as it always was.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum WorkspaceChoice {
    /// A workspace on this host, by id or name.
    Named(String),
    /// A workspace made for the task from a repository.
    New { new: NewWorkspace },
    /// An empty directory made for the task: `{"scratch": {}}`.
    Scratch { scratch: Scratch },
}

/// An empty directory has nothing to say about itself (room to grow).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Scratch {}

/// The repository a task's own workspace is made from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewWorkspace {
    pub repository: String,
    /// Revision to branch from (default: the repository's default branch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Name for the task's branch (default: made from the title).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl From<&str> for WorkspaceChoice {
    fn from(s: &str) -> Self {
        WorkspaceChoice::Named(s.to_owned())
    }
}

impl From<String> for WorkspaceChoice {
    fn from(s: String) -> Self {
        WorkspaceChoice::Named(s)
    }
}

/// A message from the developer to the task's Control Agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskSend {
    pub command_id: String,
    pub task: String,
    /// What the developer said; may be empty when `images` has something.
    pub text: String,
    /// Pictures that go with it (pasted screenshots). At most
    /// [`MAX_IMAGES`], [`MAX_IMAGE_BYTES`] each and [`MAX_IMAGES_TOTAL`] in all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<TaskImage>,
}

/// Most pictures on one message.
pub const MAX_IMAGES: usize = 4;
/// Largest picture, decoded, in bytes.
pub const MAX_IMAGE_BYTES: usize = 700 * 1024;
/// Largest total of one message's pictures, decoded, in bytes.
pub const MAX_IMAGES_TOTAL: usize = 2 * 1024 * 1024;

/// A picture on a message: its type and content, base64.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskImage {
    /// `image/png`, `image/jpeg` or `image/webp`.
    pub media_type: String,
    pub data: String,
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

#[cfg(test)]
mod workspace_choice_tests {
    use super::*;

    fn parse(v: serde_json::Value) -> WorkspaceChoice {
        serde_json::from_value(v).unwrap()
    }

    /// What an older client sends still means what it did (D-179).
    #[test]
    fn a_plain_string_is_still_a_workspace_by_name() {
        assert_eq!(parse(serde_json::json!("proj")), "proj".into());
        let c: TaskCreate = serde_json::from_value(serde_json::json!({
            "command_id": "c", "title": "t", "workspace": "scratch"
        }))
        .unwrap();
        assert_eq!(c.workspace, Some("scratch".into()));
    }

    #[test]
    fn a_workspace_can_be_made_from_a_repository_or_be_empty() {
        assert_eq!(
            parse(serde_json::json!({"new": {"repository": "git@h:a/b.git", "base": "dev"}})),
            WorkspaceChoice::New {
                new: NewWorkspace {
                    repository: "git@h:a/b.git".into(),
                    base: Some("dev".into()),
                    branch: None,
                }
            }
        );
        assert_eq!(
            parse(serde_json::json!({"scratch": {}})),
            WorkspaceChoice::Scratch {
                scratch: Scratch {}
            }
        );
        let back = serde_json::to_value(WorkspaceChoice::New {
            new: NewWorkspace {
                repository: "r".into(),
                base: None,
                branch: None,
            },
        })
        .unwrap();
        assert_eq!(back, serde_json::json!({"new": {"repository": "r"}}));
    }
}
