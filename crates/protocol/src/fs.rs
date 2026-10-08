//! Files on a host, for uploading and downloading (`fs.*`) and for pasting
//! images into a session (`session.paste_image`). Content travels base64 in
//! chunks so large files stream and can show progress.

use otter_core::Timestamp;
use serde::{Deserialize, Serialize};

/// Largest chunk the daemon reads or writes at once.
pub const MAX_CHUNK: usize = 1024 * 1024;

/// A path in a workspace: relative to its root, or absolute.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsPath {
    pub workspace: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DirListing {
    /// The directory listed (absolute).
    pub path: String,
    /// Its parent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The workspace root (absolute), so clients can show paths below it.
    pub root: String,
    /// Directories first, then files, each by name.
    pub entries: Vec<DirEntry>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<Timestamp>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsRead {
    #[serde(flatten)]
    pub at: FsPath,
    #[serde(default)]
    pub offset: u64,
    /// At most [`MAX_CHUNK`].
    pub len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileChunk {
    /// Base64.
    pub data: String,
    /// The whole file's size.
    pub size: u64,
    pub eof: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsWrite {
    #[serde(flatten)]
    pub at: FsPath,
    #[serde(default)]
    pub offset: u64,
    /// Base64, at most [`MAX_CHUNK`] decoded.
    pub data: String,
    /// Start the file afresh (the first chunk). Without it the file must
    /// exist and the chunk goes at `offset`.
    #[serde(default)]
    pub create: bool,
    /// With `create`: replace an existing file rather than fail.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PasteImage {
    pub workspace: String,
    pub session: String,
    /// A PNG, base64.
    pub png: String,
}
