//! A workspace's work as seen from outside its terminals: its Git changes
//! (`workspace.changes`, `workspace.diff`, D-102) and the ports its sessions
//! listen on (`workspace.ports`, D-104).

use otter_core::SessionId;
use serde::{Deserialize, Serialize};

/// Most changed files `workspace.changes` lists; `more` counts the rest.
pub const MAX_CHANGED_FILES: usize = 500;
/// Most of a diff `workspace.diff` returns.
pub const MAX_DIFF_BYTES: usize = 512 * 1024;

/// `workspace.changes`: the work in Git terms. Outside a Git work tree only
/// `git: false` is set.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceChanges {
    pub git: bool,
    /// The branch checked out; absent when HEAD is detached or unborn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// What the changes are counted from: the workspace's base branch (or the
    /// branch's upstream); absent when neither is known, and then only what
    /// isn't committed counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Commits on HEAD that the base doesn't have.
    #[serde(default)]
    pub ahead: u32,
    /// Commits on the base that HEAD doesn't have.
    #[serde(default)]
    pub behind: u32,
    /// Commits on HEAD that no remote has: work not pushed anywhere (D-152).
    /// Absent from an older otterd, and when there are no commits yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpushed: Option<u32>,
    /// Changed since the base, committed or not, new files included; paths
    /// relative to the workspace root. At most [`MAX_CHANGED_FILES`].
    #[serde(default)]
    pub files: Vec<ChangedFile>,
    /// Changed files past the ones listed.
    #[serde(default)]
    pub more: u32,
    /// Lines added and removed in text files, all of them.
    #[serde(default)]
    pub insertions: u64,
    #[serde(default)]
    pub deletions: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub change: ChangeKind,
    /// Lines added and removed; absent for a binary file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insertions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    /// Its change, or part of it, isn't committed yet.
    #[serde(default)]
    pub uncommitted: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    /// New and not added to Git.
    Untracked,
}

/// `workspace.diff`: one changed file, as a unified diff since the base.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceDiff {
    pub workspace: String,
    /// Relative to the workspace root, as `workspace.changes` lists it.
    pub path: String,
    /// Lines of context around each change (git's default, 3, if absent); a
    /// large number gives the whole file, for a client to fold (D-111).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileDiff {
    pub text: String,
    /// Cut at [`MAX_DIFF_BYTES`].
    #[serde(default)]
    pub truncated: bool,
}

/// A TCP port a process of one of the workspace's sessions listens on.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPort {
    pub session_id: SessionId,
    pub port: u16,
    /// Bound address (`127.0.0.1`, `0.0.0.0`, `::`, …).
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}
