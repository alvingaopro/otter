//! Otter's conversation on a host (D-093): the developer and Otter about
//! any task here, apart from each task's own conversation.
//!
//! ```text
//! state/otter/chat.json      the conversation, rewritten atomically
//! state/otter/history.jsonl  Otter's tool calls, appended
//! ```
//!
//! Kept the way a task is (`tasks.rs`: a private temporary file,
//! synced, renamed; history appended and synced) rather than as a D-058
//! journal — D-093 says why. Commands are applied once.

use std::collections::VecDeque;
use std::io::BufRead;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use otter_core::task::{Message, MessageRole, OtterChat, TASK_EVENT_SCHEMA, TaskEventRecord};
use otter_core::{MessageId, Timestamp};
use otter_protocol::otter::{OtterEvents, OtterSend};
use otter_protocol::{Event, RpcError};
use serde::{Deserialize, Serialize};

use crate::daemon::{Daemon, RpcResult};
use crate::tasks::{
    Change, MAX_TEXT, append_history, check_command_id, check_images, check_text, last_seq,
    message, message_with_id, write_doc,
};

/// Commands remembered, so a repeat is applied once.
const REMEMBERED_COMMANDS: usize = 256;
/// Messages a client gets with `otter.get` (the newest).
const SHOWN: usize = 200;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(flatten)]
    chat: OtterChat,
    #[serde(default)]
    commands: VecDeque<String>,
}

/// What an applied command produced.
pub struct Applied {
    pub chat: OtterChat,
    /// A repeat of a command already applied: nothing changed.
    pub duplicate: bool,
}

pub struct ChatStore {
    dir: PathBuf,
    doc: Doc,
}

impl ChatStore {
    pub fn load(dir: &Path) -> Result<ChatStore> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("chat.json");
        let mut doc: Doc = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Doc::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let logged = last_seq(&dir.join("history.jsonl"));
        doc.chat.history_seq = doc.chat.history_seq.max(logged);
        Ok(ChatStore {
            dir: dir.to_path_buf(),
            doc,
        })
    }

    /// An empty conversation that writes to `dir` (its file couldn't be read).
    pub fn empty(dir: &Path) -> ChatStore {
        ChatStore {
            dir: dir.to_path_buf(),
            doc: Doc::default(),
        }
    }

    pub fn get(&self) -> &OtterChat {
        &self.doc.chat
    }

    /// Change the conversation: `step` works on a copy; an error leaves it as
    /// it was. A `command_id` already applied returns it unchanged.
    pub fn apply(
        &mut self,
        command_id: Option<&str>,
        step: impl FnOnce(&mut OtterChat, Timestamp) -> RpcResult<Vec<Change>>,
    ) -> RpcResult<Applied> {
        if let Some(cmd) = command_id
            && self.doc.commands.iter().any(|c| c == cmd)
        {
            return Ok(Applied {
                chat: self.doc.chat.clone(),
                duplicate: true,
            });
        }
        let mut doc = self.doc.clone();
        let now = Utc::now();
        let changes = step(&mut doc.chat, now)?;
        let mut records = Vec::with_capacity(changes.len());
        for c in changes {
            doc.chat.history_seq += 1;
            records.push(TaskEventRecord {
                seq: doc.chat.history_seq,
                ts: now,
                v: TASK_EVENT_SCHEMA,
                correlation_id: c.correlation_id,
                text: c.event.text(),
                event: c.event,
            });
        }
        if let Some(cmd) = command_id {
            doc.commands.push_back(cmd.to_owned());
            while doc.commands.len() > REMEMBERED_COMMANDS {
                doc.commands.pop_front();
            }
        }
        let saved = write_doc(&self.dir.join("chat.json"), &doc)
            .and_then(|()| append_history(&self.dir.join("history.jsonl"), &records));
        saved.map_err(|e| RpcError::internal(format!("saving Otter's conversation: {e:#}")))?;
        self.doc = doc;
        Ok(Applied {
            chat: self.doc.chat.clone(),
            duplicate: false,
        })
    }

    /// The history with `seq > after`, oldest first.
    pub fn history(&self, after: u64, limit: usize) -> Result<Vec<TaskEventRecord>> {
        let path = self.dir.join("history.jsonl");
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for line in std::io::BufReader::new(file).lines() {
            let Ok(rec) = serde_json::from_str::<TaskEventRecord>(&line?) else {
                continue;
            };
            if rec.seq > after {
                out.push(rec);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// The developer's messages Otter hasn't answered yet, oldest first: those
/// after its last reply to one of them.
pub fn unanswered(chat: &OtterChat) -> Vec<Message> {
    let answered = chat.messages.iter().rposition(|m| {
        m.role == MessageRole::Controller
            && m.correlation_id
                .as_deref()
                .is_some_and(|c| c.starts_with("msg_"))
    });
    chat.messages
        .iter()
        .skip(answered.map_or(0, |i| i + 1))
        .filter(|m| m.role == MessageRole::User)
        .cloned()
        .collect()
}

impl Daemon {
    /// Tell clients (and the controller) the conversation changed.
    pub(crate) fn chat_changed(&self, applied: &Applied) {
        if applied.duplicate {
            return;
        }
        self.events.emit(Event::OtterChanged {
            history_seq: applied.chat.history_seq,
        });
        self.task_wake.notify_one();
    }

    pub(crate) async fn otter_get(&self) -> OtterChat {
        let chat = self.chat.lock().await.get().clone();
        let skip = chat.messages.len().saturating_sub(SHOWN);
        OtterChat {
            messages: chat.messages[skip..].to_vec(),
            ..chat
        }
    }

    pub(crate) async fn otter_send(&self, p: OtterSend) -> RpcResult<OtterChat> {
        check_command_id(&p.command_id)?;
        let text = p.text.trim().to_owned();
        if text.is_empty() && p.images.is_empty() {
            return Err(RpcError::invalid("the message is empty"));
        }
        check_text("message", &text, MAX_TEXT)?;
        let pictures = check_images(&p.images)?;
        // Pictures as a task keeps them (D-171), beside the conversation:
        // written first, taken away again if no message was added (D-202).
        let message_id = MessageId::generate();
        let dir = self.chat_artifacts();
        let names: Vec<String> = pictures
            .iter()
            .enumerate()
            .map(|(n, (ext, _))| format!("attachment-{message_id}-{n}.{ext}"))
            .collect();
        if !pictures.is_empty() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| RpcError::internal(e.to_string()))?;
            for (name, (_, bytes)) in names.iter().zip(&pictures) {
                crate::settings::write_private(&dir.join(name), bytes)
                    .map_err(|e| RpcError::internal(format!("{e:#}")))?;
            }
        }
        let mut added = false;
        let applied = self
            .chat
            .lock()
            .await
            .apply(Some(&p.command_id), |chat, _| {
                let text = if text.is_empty() {
                    otter_core::task::PICTURES_ONLY.to_owned()
                } else {
                    text
                };
                let (mut m, c) = message_with_id(
                    message_id.clone(),
                    MessageRole::User,
                    text,
                    Some(p.command_id.clone()),
                );
                m.attachments = names.clone();
                chat.messages.push(m);
                added = true;
                Ok(vec![c])
            });
        if !(applied.is_ok() && added) {
            for name in &names {
                let _ = std::fs::remove_file(dir.join(name));
            }
        }
        let applied = applied?;
        self.chat_changed(&applied);
        Ok(self.otter_get().await)
    }

    pub(crate) async fn otter_events(&self, p: &OtterEvents) -> RpcResult<Vec<TaskEventRecord>> {
        let limit = p.limit.map_or(1000, |l| l as usize);
        self.chat
            .lock()
            .await
            .history(p.after.unwrap_or(0), limit)
            .map_err(|e| RpcError::internal(format!("reading the history: {e:#}")))
    }

    /// Where the pictures of Otter's conversation on the host are kept.
    pub(crate) fn chat_artifacts(&self) -> PathBuf {
        self.paths.state_dir.join("otter").join("artifacts")
    }

    /// Post Otter's reply to the developer's last message `to`.
    pub(crate) async fn chat_reply(&self, to: &Message, text: String) -> RpcResult<()> {
        let applied = self.chat.lock().await.apply(None, |chat, _| {
            let (m, c) = message(MessageRole::Controller, text, Some(to.id.to_string()));
            chat.messages.push(m);
            Ok(vec![c])
        })?;
        self.chat_changed(&applied);
        Ok(())
    }
}
