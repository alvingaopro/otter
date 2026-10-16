//! Otter's memory (D-113): what Otter noted, so that a decision outlasts
//! the recent messages a turn is given.
//!
//! ```text
//! state/otter/memory.md        across every conversation on this host
//! state/otter/chat-memory.md   Otter's conversation on the host (D-093)
//! state/tasks/<id>/memory.md   a task's conversation (deleted with it)
//! ```
//!
//! Markdown, read afresh for every turn (the developer may edit a file by
//! hand), written whole and atomically, redacted (invariant 13) and bounded:
//! every turn is given it. What a memory says is the developer's content:
//! never in `events.jsonl` or a history, which only say Otter changed it.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use otter_core::TaskId;
use otter_core::redact::redact;
use otter_protocol::RpcError;
use otter_protocol::otter::{MemoryGet, MemoryScope, MemorySet, OtterMemory};

use crate::daemon::{Daemon, RpcResult};
use crate::tasks::task_not_found;

/// The most one memory holds, in characters.
pub const MAX_MEMORY: usize = 8_000;

/// One of Otter's memories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Memory {
    /// Across every conversation on this host.
    Host,
    /// Otter's conversation on the host.
    Chat,
    /// A task's conversation.
    Task(TaskId),
}

impl Memory {
    /// A conversation's memory, or the host's.
    pub fn of(scope: MemoryScope, task: Option<&TaskId>) -> Memory {
        match (scope, task) {
            (MemoryScope::Host, _) => Memory::Host,
            (MemoryScope::Conversation, Some(id)) => Memory::Task(id.clone()),
            (MemoryScope::Conversation, None) => Memory::Chat,
        }
    }

    fn path(&self, d: &Daemon) -> PathBuf {
        let otter = d.paths.state_dir.join("otter");
        match self {
            Memory::Host => otter.join("memory.md"),
            Memory::Chat => otter.join("chat-memory.md"),
            Memory::Task(id) => d.paths.tasks_dir.join(id.as_str()).join("memory.md"),
        }
    }
}

/// What a memory file says ("" when there is none).
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Replace a memory file: a private temporary file, synced, renamed.
fn write(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// What a memory may say: redacted, ending in a newline, not too long.
pub fn clean(text: &str) -> RpcResult<String> {
    let text = redact(text.trim());
    let n = text.chars().count();
    if n > MAX_MEMORY {
        return Err(RpcError::invalid(format!(
            "a memory holds at most {MAX_MEMORY} characters; this is {n}"
        )));
    }
    Ok(if text.is_empty() {
        String::new()
    } else {
        format!("{text}\n")
    })
}

impl Daemon {
    /// What a memory says now ("" when nothing was noted).
    pub(crate) fn memory(&self, m: &Memory) -> String {
        read(&m.path(self))
    }

    /// Otter's memories as a turn in a conversation is given them: the
    /// host's, then the task's (or, without one, Otter's conversation's on
    /// the host).
    pub(crate) fn memories(&self, task: Option<&TaskId>) -> String {
        crate::turn::memories(
            &self.memory(&Memory::Host),
            &self.memory(&Memory::of(MemoryScope::Conversation, task)),
        )
    }

    /// Change a memory: `change` gets what it says and returns what it
    /// should say instead (`None`: leave it). Changes are one at a time, so
    /// Otter's notes and the developer's edits never overwrite each other.
    /// Returns what it says afterwards.
    pub(crate) async fn change_memory<E: From<RpcError>>(
        &self,
        m: &Memory,
        change: impl FnOnce(&str) -> Result<Option<String>, E>,
    ) -> Result<String, E> {
        let _one_at_a_time = self.memory_lock.lock().await;
        let path = m.path(self);
        let now = read(&path);
        let Some(next) = change(&now)? else {
            return Ok(now);
        };
        let next = clean(&next)?;
        if next == now {
            return Ok(now);
        }
        match m {
            // A task's directory is the task's: never made again for it.
            Memory::Task(id) => {
                if self.tasks.lock().await.get(id.as_str()).is_none() {
                    return Err(task_not_found(id.as_str()).into());
                }
            }
            Memory::Host | Memory::Chat => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(dir);
                }
            }
        }
        write(&path, &next)
            .map_err(|e| RpcError::internal(format!("saving Otter's memory: {e:#}")))?;
        Ok(next)
    }

    /// The task a client names, if it names one that is here.
    async fn memory_task(&self, task: Option<&str>) -> RpcResult<Option<TaskId>> {
        let Some(t) = task else {
            return Ok(None);
        };
        Ok(Some(self.task_get(t).await?.id))
    }

    pub(crate) async fn memory_get(&self, p: &MemoryGet) -> RpcResult<OtterMemory> {
        let task = self.memory_task(p.task.as_deref()).await?;
        Ok(OtterMemory {
            host: self.memory(&Memory::Host),
            conversation: self.memory(&Memory::of(MemoryScope::Conversation, task.as_ref())),
            max_chars: MAX_MEMORY as u32,
        })
    }

    /// The developer's edit: all of what one memory says.
    pub(crate) async fn memory_set(&self, p: MemorySet) -> RpcResult<OtterMemory> {
        let task = self.memory_task(p.task.as_deref()).await?;
        let m = Memory::of(p.memory, task.as_ref());
        self.change_memory(&m, |now: &str| {
            if let Some(was) = &p.previous
                && was.trim() != now.trim()
            {
                return Err(RpcError::conflict(
                    "Otter changed this memory since you opened it; look again",
                ));
            }
            Ok(Some(p.text.clone()))
        })
        .await?;
        self.memory_get(&MemoryGet { task: p.task }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_is_redacted_and_bounded() {
        let t = clean(
            "- deploys go through staging\n- key: ANTHROPIC_API_KEY=sk-ant-abc123def456ghi\n",
        )
        .unwrap();
        assert!(t.starts_with("- deploys go through staging\n"), "{t}");
        assert!(!t.contains("sk-ant-abc123def456ghi"), "{t}");
        assert!(t.ends_with('\n'));
        assert_eq!(clean("  \n").unwrap(), "");
        assert!(clean(&"x".repeat(MAX_MEMORY + 1)).is_err());
        assert!(
            clean(&"é".repeat(MAX_MEMORY)).is_ok(),
            "characters, not bytes"
        );
    }

    #[test]
    fn a_memory_file_is_replaced_whole_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        assert_eq!(read(&path), "");
        write(&path, "- one\n").unwrap();
        write(&path, "- two\n").unwrap();
        assert_eq!(read(&path), "- two\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!dir.path().join("memory.md.tmp").exists());
    }
}
