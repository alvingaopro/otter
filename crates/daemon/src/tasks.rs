//! Tasks (D-043): durable product work, owned by this daemon so it
//! survives any client — the desktop app may be closed for days.
//!
//! Kept apart from workspace state (`state.json`):
//!
//! ```text
//! state/tasks/<task-id>/task.json          the task, rewritten atomically
//! state/tasks/<task-id>/history.jsonl      what happened, append-only
//! ```
//!
//! Every change goes through [`TaskStore::apply`]: on a copy, then the
//! document is saved (atomically, with the id of the command that caused it),
//! then the new history lines are appended, then `TaskChanged` goes out on
//! the event log. A command id already in the document is not applied again,
//! which makes a retried delivery harmless. A crash between the save and the
//! append loses history lines, never state; loading notices and says so.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use otter_core::task::{
    Decider, DecisionKind, DecisionStatus, Message, MessageRole, StepStatus, TASK_EVENT_SCHEMA,
    TASK_SCHEMA, Task, TaskAction, TaskEvent, TaskEventRecord, TaskStatus,
};
use otter_core::{MessageId, TaskId, Timestamp, WorkspaceId};
use otter_protocol::task::{
    MAX_IMAGE_BYTES, MAX_IMAGES, MAX_IMAGES_TOTAL, TaskAct, TaskCreate, TaskEvents, TaskImage,
    TaskSend, WorkspaceChoice,
};
use otter_protocol::{Event, RpcError, SourceSpec};
use serde::{Deserialize, Serialize};

use crate::daemon::{Daemon, RpcResult, workspace_not_found};

/// Command ids remembered per task (a retry comes soon after the original).
const REMEMBERED_COMMANDS: usize = 256;
/// A repository's address, at most.
const MAX_REPOSITORY: usize = 1024;
const MAX_TITLE: usize = 200;
pub(crate) const MAX_TEXT: usize = 20_000;
const DEFAULT_HISTORY_LIMIT: usize = 1000;

/// `task.json`: the task plus the daemon's bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Doc {
    #[serde(flatten)]
    task: Task,
    /// Commands already applied, oldest first.
    #[serde(default)]
    commands: VecDeque<String>,
}

/// One change, as a step of [`TaskStore::apply`] records it.
pub struct Change {
    pub event: TaskEvent,
    pub correlation_id: Option<String>,
}

impl Change {
    pub fn new(event: TaskEvent) -> Self {
        Change {
            event,
            correlation_id: None,
        }
    }

    pub fn because(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }
}

/// What an applied command produced.
pub struct Applied {
    pub task: Task,
    /// A repeat of a command already applied: nothing changed.
    pub duplicate: bool,
    /// Its status before; `None` for a new task.
    pub from: Option<TaskStatus>,
}

pub struct TaskStore {
    dir: PathBuf,
    docs: BTreeMap<TaskId, Doc>,
}

/// A task's document, in its directory under `state/tasks/`.
const DOC: &str = "task.json";

/// Tasks were features before D-107: `state/features/<id>/feature.json`.
/// Moved once, at start, to `state/tasks/<id>/task.json`; history and
/// artifacts go with their directory. A task already under `state/tasks`
/// (an older otterd was started on this host after a newer one) keeps
/// what's there; the older copy stays under `state/tasks`, said in the
/// log.
pub fn move_from_features(state_dir: &Path) {
    let old = state_dir.join("features");
    let new = state_dir.join("tasks");
    if old.is_dir() {
        if !new.exists() {
            if let Err(e) = std::fs::rename(&old, &new) {
                tracing::warn!("moving {} to {}: {e}", old.display(), new.display());
            }
        } else if let Ok(entries) = std::fs::read_dir(&old) {
            for entry in entries.flatten() {
                let to = new.join(entry.file_name());
                if to.exists() {
                    tracing::warn!(
                        "{} is also under {}: kept there, this one left",
                        entry.path().display(),
                        new.display()
                    );
                } else if let Err(e) = std::fs::rename(entry.path(), &to) {
                    tracing::warn!("moving {}: {e}", entry.path().display());
                }
            }
            let _ = std::fs::remove_dir(&old);
        }
    }
    let Ok(entries) = std::fs::read_dir(&new) else {
        return;
    };
    for entry in entries.flatten() {
        let (from, to) = (entry.path().join("feature.json"), entry.path().join(DOC));
        if from.exists()
            && !to.exists()
            && let Err(e) = std::fs::rename(&from, &to)
        {
            tracing::warn!("moving {}: {e}", from.display());
        }
    }
}

impl TaskStore {
    /// Load every task under `dir`. One that can't be read (a newer
    /// schema, a broken file) is left on disk untouched and skipped.
    pub fn load(dir: &Path) -> Result<TaskStore> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let mut docs = BTreeMap::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path().join(DOC);
            if !path.exists() {
                continue;
            }
            match read_doc(&path) {
                Ok(mut doc) => {
                    let logged = last_seq(&history_path(dir, &doc.task.id));
                    if logged < doc.task.history_seq {
                        tracing::warn!(
                            task = %doc.task.id,
                            "history ends at {logged}, state at {}: lines were lost in a crash",
                            doc.task.history_seq
                        );
                    }
                    doc.task.history_seq = doc.task.history_seq.max(logged);
                    docs.insert(doc.task.id.clone(), doc);
                }
                Err(e) => tracing::warn!("skipping {}: {e:#}", path.display()),
            }
        }
        Ok(TaskStore {
            dir: dir.to_path_buf(),
            docs,
        })
    }

    /// Newest activity first.
    pub fn list(&self) -> Vec<Task> {
        let mut out: Vec<Task> = self.docs.values().map(|d| d.task.clone()).collect();
        out.sort_by_key(|f| std::cmp::Reverse(f.updated_at));
        out
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.docs.get(&TaskId::from(id)).map(|d| &d.task)
    }

    /// The task a command was applied to, if it was.
    pub fn applied(&self, command_id: &str) -> Option<&Task> {
        self.docs
            .values()
            .find(|d| d.commands.iter().any(|c| c == command_id))
            .map(|d| &d.task)
    }

    /// Remove a task and everything kept for it (history, artifacts).
    /// Its directory is renamed away first, so a crash halfway leaves no
    /// half-task behind (`load` skips what has no `task.json`).
    pub fn remove(&mut self, id: &TaskId) -> Result<Option<Task>> {
        let Some(doc) = self.docs.remove(id) else {
            return Ok(None);
        };
        let dir = self.dir.join(id.as_str());
        let gone = self.dir.join(format!(".deleted-{}", id.as_str()));
        match std::fs::rename(&dir, &gone) {
            Ok(()) => std::fs::remove_dir_all(&gone)
                .with_context(|| format!("removing {}", gone.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                self.docs.insert(id.clone(), doc);
                return Err(e).with_context(|| format!("removing {}", dir.display()));
            }
        }
        Ok(Some(doc.task))
    }

    /// Add a new task.
    pub fn insert(
        &mut self,
        task: Task,
        command_id: Option<&str>,
        changes: Vec<Change>,
    ) -> Result<Applied> {
        let doc = Doc {
            task,
            commands: VecDeque::new(),
        };
        self.commit(doc, command_id, changes)
    }

    /// Change a task: `step` works on a copy and returns what happened; an
    /// error leaves everything as it was. A `command_id` already applied to
    /// this task returns it unchanged.
    pub fn apply(
        &mut self,
        id: &TaskId,
        command_id: Option<&str>,
        step: impl FnOnce(&mut Task, Timestamp) -> RpcResult<Vec<Change>>,
    ) -> RpcResult<Applied> {
        let doc = self
            .docs
            .get(id)
            .ok_or_else(|| task_not_found(id.as_str()))?;
        if let Some(cmd) = command_id
            && doc.commands.iter().any(|c| c == cmd)
        {
            return Ok(Applied {
                task: doc.task.clone(),
                duplicate: true,
                from: Some(doc.task.status),
            });
        }
        let mut doc = doc.clone();
        let from = doc.task.status;
        let now = Utc::now();
        let changes = step(&mut doc.task, now)?;
        doc.task.updated_at = now;
        self.commit(doc, command_id, changes)
            .map(|a| Applied {
                from: Some(from),
                ..a
            })
            .map_err(|e| RpcError::internal(format!("saving task: {e:#}")))
    }

    fn commit(
        &mut self,
        mut doc: Doc,
        command_id: Option<&str>,
        changes: Vec<Change>,
    ) -> Result<Applied> {
        let now = Utc::now();
        let mut records = Vec::with_capacity(changes.len());
        for c in changes {
            doc.task.history_seq += 1;
            records.push(TaskEventRecord {
                seq: doc.task.history_seq,
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
        let dir = self.dir.join(doc.task.id.as_str());
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        write_doc(&dir.join(DOC), &doc)?;
        append_history(&dir.join("history.jsonl"), &records)?;
        let task = doc.task.clone();
        self.docs.insert(task.id.clone(), doc);
        Ok(Applied {
            task,
            duplicate: false,
            from: None,
        })
    }

    /// A task's history with `seq > after`, oldest first.
    pub fn history(&self, id: &TaskId, after: u64, limit: usize) -> Result<Vec<TaskEventRecord>> {
        let path = history_path(&self.dir, id);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for line in std::io::BufReader::new(file).lines() {
            let line = line?;
            // A torn last line (crash mid-append) is skipped.
            let Ok(rec) = serde_json::from_str::<TaskEventRecord>(&line) else {
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

fn history_path(dir: &Path, id: &TaskId) -> PathBuf {
    dir.join(id.as_str()).join("history.jsonl")
}

fn read_doc(path: &Path) -> Result<Doc> {
    let bytes = std::fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let schema = value["schema"].as_u64().unwrap_or(1) as u32;
    if schema > TASK_SCHEMA {
        anyhow::bail!("written by a newer otterd (task schema {schema})");
    }
    let mut doc: Doc = serde_json::from_value(migrate(value, schema))?;
    doc.task.schema = TASK_SCHEMA;
    Ok(doc)
}

/// Bring an older document up to [`TASK_SCHEMA`]. Schema 1 is the first;
/// later versions add their steps here (`if schema < 2 { … }`).
fn migrate(value: serde_json::Value, _schema: u32) -> serde_json::Value {
    value
}

/// Write a document atomically: a private temporary file, synced, renamed.
pub(crate) fn write_doc<T: Serialize>(path: &Path, doc: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    serde_json::to_writer_pretty(&mut f, doc)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Append history records, synced.
pub(crate) fn append_history(path: &Path, records: &[TaskEventRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let mut buf = Vec::new();
    for r in records {
        serde_json::to_writer(&mut buf, r)?;
        buf.push(b'\n');
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(&buf)?;
    f.sync_data()?;
    Ok(())
}

/// The last record's `seq` in a history file (0: none).
pub(crate) fn last_seq(path: &Path) -> u64 {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(|l| l.ok())
        .filter_map(|l| serde_json::from_str::<TaskEventRecord>(&l).ok())
        .map(|r| r.seq)
        .max()
        .unwrap_or(0)
}

pub(crate) fn task_not_found(r: &str) -> RpcError {
    RpcError::not_found(format!("no task `{r}`"))
}

/// The pictures of a message, checked: each one's file extension and bytes.
/// Their type must be one we know and the bytes must be that type.
fn check_images(images: &[TaskImage]) -> RpcResult<Vec<(&'static str, Vec<u8>)>> {
    use base64::Engine;
    if images.len() > MAX_IMAGES {
        return Err(RpcError::invalid(format!(
            "at most {MAX_IMAGES} pictures on a message"
        )));
    }
    let mut total = 0;
    let mut out = vec![];
    for image in images {
        let (ext, magic): (&str, fn(&[u8]) -> bool) = match image.media_type.as_str() {
            "image/png" => ("png", |b| b.starts_with(b"\x89PNG\r\n\x1a\n")),
            "image/jpeg" => ("jpg", |b| b.starts_with(&[0xff, 0xd8, 0xff])),
            "image/webp" => ("webp", |b| {
                b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP"
            }),
            other => {
                return Err(RpcError::invalid(format!(
                    "a picture can be png, jpeg or webp, not `{other}`"
                )));
            }
        };
        // A bound on the text first, so a huge string is never decoded.
        if image.data.len() > MAX_IMAGE_BYTES / 3 * 4 + 4 {
            return Err(RpcError::invalid(format!(
                "a picture is too large (max {} KB)",
                MAX_IMAGE_BYTES / 1024
            )));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(image.data.as_bytes())
            .map_err(|_| RpcError::invalid("a picture isn't valid base64"))?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(RpcError::invalid(format!(
                "a picture is too large (max {} KB)",
                MAX_IMAGE_BYTES / 1024
            )));
        }
        if !magic(&bytes) {
            return Err(RpcError::invalid(format!(
                "a picture isn't a {}",
                image.media_type
            )));
        }
        total += bytes.len();
        if total > MAX_IMAGES_TOTAL {
            return Err(RpcError::invalid(format!(
                "the pictures are too large together (max {} MB)",
                MAX_IMAGES_TOTAL >> 20
            )));
        }
        out.push((ext, bytes));
    }
    Ok(out)
}

pub(crate) fn check_command_id(id: &str) -> RpcResult<()> {
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Err(RpcError::invalid(
            "command_id must be 1–128 printable characters",
        ));
    }
    Ok(())
}

pub(crate) fn check_text(what: &str, text: &str, max: usize) -> RpcResult<()> {
    if text.len() > max {
        return Err(RpcError::invalid(format!(
            "{what} is too long (max {max} bytes)"
        )));
    }
    Ok(())
}

/// A branch or revision the model or a client gave: short, one word, and not
/// an option for Git (D-179).
fn check_revision(what: &str, v: &str) -> RpcResult<()> {
    check_text(what, v, 200)?;
    if v.is_empty() || v.starts_with('-') || v.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(RpcError::invalid(format!(
            "{what} must be one word and not start with `-`"
        )));
    }
    Ok(())
}

/// Where a paused, failed or unblocked task picks up: planning until
/// there is a plan, verifying once every step is done, else implementing.
pub fn resume_status(f: &Task) -> TaskStatus {
    // Only the planning turn (D-064), or planning again (D-084): still
    // planning — the work there is was planned for the old goal.
    if f.replan || f.steps.iter().all(|t| t.planning) {
        TaskStatus::Planning
    } else if f
        .steps
        .iter()
        .all(|t| matches!(t.status, StepStatus::Done | StepStatus::Skipped))
    {
        TaskStatus::Verifying
    } else {
        TaskStatus::Implementing
    }
}

/// Where resuming a paused task takes it: back to where the pause found
/// it (D-168). In review it stays in review; while a decision only the
/// developer may make is still pending it is blocked on it, as before; else
/// it picks up where its work stands ([`resume_status`]). The reason, if
/// it's the one from before the pause.
pub fn resume_to(f: &Task) -> (TaskStatus, Option<String>) {
    let before = f.paused_from.as_ref();
    if let Some(p) = before
        && p.status == TaskStatus::Review
        && !f.replan
    {
        return (TaskStatus::Review, p.reason.clone());
    }
    if let Some(d) = f.pending_decisions().find(|d| d.user_only) {
        let reason = before
            .filter(|p| p.status == TaskStatus::Blocked)
            .and_then(|p| p.reason.clone())
            .unwrap_or_else(|| format!("Needs your approval: {}", d.summary));
        return (TaskStatus::Blocked, Some(reason));
    }
    (resume_status(f), None)
}

pub fn message(role: MessageRole, text: String, correlation: Option<String>) -> (Message, Change) {
    message_with_id(MessageId::generate(), role, text, correlation)
}

/// [`message`] with an id chosen beforehand (a message whose files are
/// named for it).
pub fn message_with_id(
    id: MessageId,
    role: MessageRole,
    text: String,
    correlation: Option<String>,
) -> (Message, Change) {
    let m = Message {
        id,
        role,
        text,
        at: Utc::now(),
        correlation_id: correlation.clone(),
        attachments: vec![],
    };
    let change = Change {
        event: TaskEvent::MessageAdded {
            message_id: m.id.clone(),
            role,
        },
        correlation_id: correlation,
    };
    (m, change)
}

/// Apply a developer's action to a task (the deterministic part; the
/// controller reacts to the new state).
pub fn apply_action(
    f: &mut Task,
    action: &TaskAction,
    command_id: &str,
    now: Timestamp,
) -> RpcResult<Vec<Change>> {
    let mut changes = vec![
        Change::new(TaskEvent::CommandAccepted {
            command_id: command_id.to_owned(),
            action: action.name().to_owned(),
        })
        .because(command_id),
    ];
    let to = |f: &mut Task, status: TaskStatus, reason: Option<String>| {
        f.transition(status, reason, now)
            .map(|e| Change::new(e).because(command_id))
            .map_err(RpcError::conflict)
    };
    match action {
        TaskAction::Start => changes.push(to(f, TaskStatus::Planning, None)?),
        TaskAction::Pause => changes.push(to(f, TaskStatus::Paused, Some("Paused by you".into()))?),
        TaskAction::Resume => {
            if f.status != TaskStatus::Paused {
                return Err(RpcError::conflict("only a paused task can be resumed"));
            }
            let (next, reason) = resume_to(f);
            changes.push(to(f, next, reason)?);
        }
        TaskAction::Retry => {
            if f.status != TaskStatus::Failed {
                return Err(RpcError::conflict("only a failed task can be retried"));
            }
            f.budget.iterations_used = 0;
            for t in &mut f.steps {
                if t.status == StepStatus::Failed {
                    t.status = StepStatus::Pending;
                    t.attempts = 0;
                }
            }
            let next = resume_status(f);
            changes.push(to(f, next, Some("Retrying with a fresh budget".into()))?);
        }
        TaskAction::Cancel => {
            changes.push(to(
                f,
                TaskStatus::Cancelled,
                Some("Cancelled by you".into()),
            )?);
            // What stays (D-068): Otter removes nothing of the work.
            let mut kept =
                String::from("Cancelled. Nothing is removed: the workspace keeps the work's files");
            if let Some(d) = &f.delivery {
                kept.push_str(&format!(" and its branch `{}`", d.branch));
                if let Some(url) = &d.pr_url {
                    kept.push_str(&format!(
                        "; the pull request stays open ({url}) — close it on the forge if you don't want it"
                    ));
                }
            }
            kept.push_str(". Remove the workspace when you no longer need the work.");
            let (m, c) = message(MessageRole::Controller, kept, Some(command_id.to_owned()));
            f.messages.push(m);
            changes.push(c);
            for d in f.decisions.iter_mut() {
                if d.status == DecisionStatus::Pending {
                    d.status = DecisionStatus::Cancelled;
                    d.decided_at = Some(now);
                    changes.push(
                        Change::new(TaskEvent::DecisionResolved {
                            decision_id: d.id.clone(),
                            status: DecisionStatus::Cancelled,
                            by: Decider::User,
                        })
                        .because(command_id),
                    );
                }
            }
        }
        TaskAction::Decide {
            decision_id,
            approve,
            answer,
            answers,
        } => {
            // A form's answers, as one line for the record.
            let answer = match answers {
                Some(a) if !a.is_empty() => Some(
                    a.iter()
                        .map(|(q, v)| format!("{q} {v}"))
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
                _ => answer.clone(),
            };
            let answer = &answer;
            let d = f
                .decision_mut(decision_id)
                .ok_or_else(|| RpcError::not_found(format!("no decision `{decision_id}`")))?;
            if d.status != DecisionStatus::Pending {
                return Err(RpcError::conflict("that decision has already been made"));
            }
            d.status = match (answer, approve) {
                (Some(_), _) => DecisionStatus::Answered,
                (None, true) => DecisionStatus::Approved,
                (None, false) => DecisionStatus::Denied,
            };
            d.answer = answer.clone();
            d.decided_by = Some(Decider::User);
            d.decided_at = Some(now);
            // Allowed once, allowed for the rest of the task (D-066).
            let grant = d
                .grant
                .clone()
                .filter(|_| d.status == DecisionStatus::Approved);
            let (kind, step, status) = (d.kind, d.step_id.clone(), d.status);
            changes.push(
                Change::new(TaskEvent::DecisionResolved {
                    decision_id: decision_id.clone(),
                    status: d.status,
                    by: Decider::User,
                })
                .because(command_id),
            );
            if let Some(g) = grant
                && !f.grants.contains(&g)
            {
                f.grants.push(g);
            }
            changes.extend(f.resume_waiting_runs(now).into_iter().map(Change::new));
            // Cut-off work (D-061): either way its step is ready again, and
            // its next run starts over. Declined, nothing starts until the
            // developer resumes.
            if kind == DecisionKind::Recovery {
                if let Some(t) = step.as_ref().and_then(|t| f.step_mut(t))
                    && t.status == StepStatus::Blocked
                {
                    t.status = StepStatus::Pending;
                    changes.push(
                        Change::new(TaskEvent::StepChanged {
                            step_id: t.id.clone(),
                            status: StepStatus::Pending,
                        })
                        .because(command_id),
                    );
                }
                if status == DecisionStatus::Denied && f.status.can_become(TaskStatus::Paused) {
                    let why = "Not started over: resume when you're ready";
                    changes.push(to(f, TaskStatus::Paused, Some(why.into()))?);
                }
            }
            if f.status == TaskStatus::Blocked && f.pending_decisions().next().is_none() {
                let next = resume_status(f);
                changes.push(to(f, next, None)?);
            }
        }
        TaskAction::Accept { override_gates } => {
            if f.status != TaskStatus::Review {
                return Err(RpcError::conflict("only a task in review can be accepted"));
            }
            // Done needs its gates (D-047); the developer may override, on record.
            if f.gates.is_empty() && !override_gates {
                return Err(RpcError::conflict(
                    "the gates haven't been checked yet; try again in a moment",
                ));
            }
            let open: Vec<String> = f
                .gates
                .iter()
                .filter(|g| !g.status.clear() && g.name != crate::delivery::ACCEPTANCE_GATE)
                .map(|g| format!("{} ({:?})", g.name, g.status).to_lowercase())
                .collect();
            if !open.is_empty() {
                if !override_gates {
                    return Err(RpcError::conflict(format!(
                        "gates not met: {}",
                        open.join(", ")
                    )));
                }
                changes.push(Change::new(
                    f.reason(format!("Accepted by you despite: {}", open.join(", "))),
                ));
            }
            for g in &mut f.gates {
                if g.name == crate::delivery::ACCEPTANCE_GATE {
                    g.status = otter_core::task::GateStatus::Passed;
                }
            }
            changes.push(to(f, TaskStatus::Done, Some("Accepted by you".into()))?);
        }
        TaskAction::SetWorkspace { workspace } => {
            // `workspace` is an id by now (resolved in `task_act`).
            if f.live_run().is_some() {
                return Err(RpcError::conflict(
                    "an agent is working in the current workspace; pause first",
                ));
            }
            f.workspace_id = Some(WorkspaceId::from(workspace.as_str()));
            if f.status == TaskStatus::Blocked && f.pending_decisions().next().is_none() {
                let next = resume_status(f);
                changes.push(to(f, next, None)?);
            }
        }
        // The preview's session is started by `task_act`.
        TaskAction::Preview => {}
        // Made into `SetWorkspace` by `task_act`.
        TaskAction::OwnWorkspace => {
            return Err(RpcError::invalid(
                "own_workspace is resolved before it applies",
            ));
        }
        TaskAction::Interrupt => {
            if f.live_run().is_none() {
                return Err(RpcError::conflict(
                    "the coding agent isn't working on anything",
                ));
            }
            changes.push(to(
                f,
                TaskStatus::Paused,
                Some("Interrupted by you: say what to do next".into()),
            )?);
        }
        TaskAction::Redirect { text } => {
            let text = text.trim();
            if text.is_empty() {
                return Err(RpcError::invalid("say what to do instead"));
            }
            check_text("redirect", text, MAX_TEXT)?;
            if f.live_run().is_none() {
                return Err(RpcError::conflict(
                    "the coding agent isn't working on anything: send a message instead",
                ));
            }
            let (m, c) = message(
                MessageRole::User,
                text.to_owned(),
                Some(command_id.to_owned()),
            );
            f.messages.push(m);
            changes.push(c);
        }
        TaskAction::RequestChanges { note } => {
            if let Some(note) = note.as_ref().filter(|n| !n.trim().is_empty()) {
                check_text("note", note, MAX_TEXT)?;
                let (m, c) = message(MessageRole::User, note.clone(), Some(command_id.to_owned()));
                f.messages.push(m);
                changes.push(c);
            }
            changes.extend(replan(f, "The goal changed: planning again", now));
        }
    }
    Ok(changes)
}

/// The goal moved (D-050): plan again from where the work stands — what is
/// done stays done. Nothing to do for a cancelled task. The coding agent
/// plans again, in its conversation (D-084): the planning turn is due again,
/// and a plan waiting for the developer's OK is moot.
pub fn replan(f: &mut Task, why: &str, now: Timestamp) -> Vec<Change> {
    if f.status.is_terminal() {
        return vec![];
    }
    // Revising a plan: there is work planned (an answered question had none).
    f.replan = f.steps.iter().any(|t| !t.planning);
    let mut changes = Vec::new();
    // A plan sent back but not yet looked at again (`Pending` with the
    // review's points) is a fresh look now, at the new goal.
    if crate::controller::plan_turn()
        && let Some(t) = f.steps.iter_mut().find(|t| t.planning)
        && matches!(
            t.status,
            StepStatus::Done | StepStatus::Failed | StepStatus::Skipped | StepStatus::Pending
        )
    {
        t.status = StepStatus::Pending;
        t.detail = None;
        t.last_error = None;
        changes.push(Change::new(TaskEvent::StepChanged {
            step_id: t.id.clone(),
            status: StepStatus::Pending,
        }));
    }
    // A plan turned down was turned down for the old goal (D-158).
    crate::controller::forget_decline(f);
    for d in f.decisions.iter_mut().filter(|d| {
        d.kind == DecisionKind::PlanApproval
            && d.run_id.is_none()
            && d.status == DecisionStatus::Pending
    }) {
        d.status = DecisionStatus::Cancelled;
        d.decided_at = Some(now);
    }
    // The last round's verdict and report are about the old goal.
    f.report = None;
    f.gates.clear();
    f.last_verification = None;
    if f.status != TaskStatus::Planning
        && let Ok(e) = f.transition(TaskStatus::Planning, Some(why.into()), now)
    {
        changes.push(Change::new(e));
    }
    changes
}

impl Daemon {
    pub(crate) async fn task_list(&self) -> Vec<Task> {
        self.tasks.lock().await.list()
    }

    pub(crate) async fn task_get(&self, id: &str) -> RpcResult<Task> {
        self.tasks
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| task_not_found(id))
    }

    pub(crate) async fn task_create(self: &Arc<Self>, p: TaskCreate) -> RpcResult<Task> {
        check_command_id(&p.command_id)?;
        let title = p.title.trim().to_owned();
        if title.is_empty() {
            return Err(RpcError::invalid("a task needs a title"));
        }
        check_text("title", &title, MAX_TITLE)?;
        check_text("request", &p.request, MAX_TEXT)?;
        // A workspace made for the task (D-179) is made after the task, so
        // a repeat of the command finds the task and goes on from there:
        // nothing here makes a workspace before the applied check.
        let made = match &p.workspace {
            Some(WorkspaceChoice::New { new }) => {
                let repository = new.repository.trim();
                if repository.is_empty() {
                    return Err(RpcError::invalid("a repository must not be empty"));
                }
                check_text("repository", repository, MAX_REPOSITORY)?;
                for (what, v) in [("base", &new.base), ("branch", &new.branch)] {
                    if let Some(v) = v {
                        check_revision(what, v)?;
                    }
                }
                Some(SourceSpec::Git {
                    repository: self.stored_repository(repository).await,
                    branch: new.branch.clone(),
                    base: new.base.clone(),
                })
            }
            Some(WorkspaceChoice::Scratch { .. }) => Some(SourceSpec::Empty),
            _ => None,
        };
        let workspace_id: Option<WorkspaceId> = match &p.workspace {
            Some(WorkspaceChoice::Named(w)) => Some(
                self.store
                    .lock()
                    .await
                    .workspace(w)
                    .map(|ws| ws.id.clone())
                    .ok_or_else(|| workspace_not_found(w))?,
            ),
            _ => None,
        };
        if p.answer_only {
            self.can_answer_only()?;
        }
        let created = {
            let mut tasks = self.tasks.lock().await;
            if let Some(f) = tasks.applied(&p.command_id) {
                f.clone()
            } else {
                let mut f = Task::new(title.clone(), p.request.trim().to_owned(), Utc::now());
                f.workspace_id = workspace_id;
                f.answer_only = p.answer_only;
                let mut changes =
                    vec![Change::new(TaskEvent::TaskCreated { title }).because(&p.command_id)];
                if !f.request.is_empty() {
                    let (m, c) = message(
                        MessageRole::User,
                        f.request.clone(),
                        Some(p.command_id.clone()),
                    );
                    f.messages.push(m);
                    changes.push(c);
                }
                let applied = tasks
                    .insert(f, Some(&p.command_id), changes)
                    .map_err(|e| RpcError::internal(format!("saving task: {e:#}")))?;
                drop(tasks);
                self.task_changed(&applied);
                applied.task
            }
        };
        let Some(source) = made else {
            return Ok(created);
        };
        if created.workspace_id.is_some() {
            return Ok(created);
        }
        // The task is made; if its workspace can't be, it stays a task
        // without one, with the reason on it, rather than an error that a
        // retry would answer with a second task (D-179).
        let set = async {
            let ws = self.workspace_for_task(&created, source).await?;
            self.clone()
                .task_act(TaskAct {
                    command_id: format!("{:.100}:workspace", p.command_id),
                    task: created.id.to_string(),
                    action: TaskAction::SetWorkspace {
                        workspace: ws.id.to_string(),
                    },
                })
                .await
        };
        match set.await {
            Ok(f) => Ok(f),
            Err(e) => {
                tracing::warn!(task = %created.id, "no workspace for a new task: {}", e.message);
                let said = format!(
                    "I couldn't make or choose a workspace for this task ({}). Choose one to go on.",
                    otter_core::redact::redact(
                        e.message.lines().next().unwrap_or("no reason given")
                    )
                );
                let applied = self.tasks.lock().await.apply(&created.id, None, |f, _| {
                    let (m, c) = message(MessageRole::Controller, said.clone(), None);
                    f.messages.push(m);
                    Ok(vec![c])
                });
                match applied {
                    Ok(a) => self.task_changed(&a),
                    Err(e) => {
                        tracing::warn!(task = %created.id, "saying why it has no workspace: {}", e.message)
                    }
                }
                self.task_get(created.id.as_str()).await
            }
        }
    }

    /// A repository as stored, when `given` is a stored one as it is shown
    /// (without its credentials, D-075): what Otter reads in a listing, put
    /// back, names the same backing repository (D-108) and authenticates.
    async fn stored_repository(&self, given: &str) -> String {
        let store = self.store.lock().await;
        store
            .state
            .workspaces
            .iter()
            .find_map(|w| match &w.source {
                otter_core::WorkspaceSource::Git(g)
                    if g.repository != given
                        && otter_core::redact::url_credentials(&g.repository) == given =>
                {
                    Some(g.repository.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| given.to_owned())
    }

    /// Whether a task here can be answer-only (D-092): its coding agent
    /// must run on the SDK runtime, where every tool call reaches Otter's
    /// policy. Never an answer-only task that can't be kept to reading.
    pub(crate) fn can_answer_only(&self) -> RpcResult<()> {
        let backend = self.settings.read().unwrap().coding_backend();
        if backend == "sdk" {
            Ok(())
        } else {
            Err(RpcError::invalid(
                "an answer-only task needs the coding agent on the SDK runtime (Settings), where Otter can keep it to reading",
            ))
        }
    }

    pub(crate) async fn task_send(&self, p: TaskSend) -> RpcResult<Task> {
        check_command_id(&p.command_id)?;
        let text = p.text.trim().to_owned();
        if text.is_empty() && p.images.is_empty() {
            return Err(RpcError::invalid("the message is empty"));
        }
        check_text("message", &text, MAX_TEXT)?;
        let pictures = check_images(&p.images)?;
        let cmd = p.command_id.clone();
        let id = TaskId::from(p.task.as_str());
        // Named for the message, so no command can overwrite another's files.
        // Written first (a message never points at files that aren't there),
        // and taken away again if no message was added: the command was
        // refused, or it was one already done (a lost reply, sent again).
        let message_id = MessageId::generate();
        let names: Vec<String> = pictures
            .iter()
            .enumerate()
            .map(|(n, (ext, _))| format!("attachment-{message_id}-{n}.{ext}"))
            .collect();
        let kept = if pictures.is_empty() {
            vec![]
        } else {
            let task = self.task_get(&p.task).await?;
            let dir = self.artifacts_dir(&task.id);
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| RpcError::internal(e.to_string()))?;
            let paths: Vec<_> = names.iter().map(|name| dir.join(name)).collect();
            for (path, (_, bytes)) in paths.iter().zip(&pictures) {
                if let Err(e) = tokio::fs::write(path, bytes).await {
                    for path in &paths {
                        let _ = tokio::fs::remove_file(path).await;
                    }
                    return Err(RpcError::internal(e.to_string()));
                }
            }
            paths
        };
        let mut added = false;
        let applied = self.tasks.lock().await.apply(&id, Some(&cmd), |f, _| {
            if f.status.is_terminal() {
                return Err(RpcError::conflict(format!(
                    "the task is {}",
                    f.status.as_str()
                )));
            }
            let text = if text.is_empty() {
                otter_core::task::PICTURES_ONLY.to_owned()
            } else {
                text
            };
            let (mut m, c) = message_with_id(
                message_id.clone(),
                MessageRole::User,
                text,
                Some(cmd.clone()),
            );
            m.attachments = names.clone();
            f.messages.push(m);
            added = true;
            Ok(vec![c])
        });
        let applied = match applied {
            Ok(applied) if added || kept.is_empty() => applied,
            other => {
                for path in &kept {
                    let _ = tokio::fs::remove_file(path).await;
                }
                other?
            }
        };
        self.task_changed(&applied);
        // Otter reads it first and decides what reaches the coding agent
        // (controller.rs, D-059).
        Ok(applied.task)
    }

    /// A worktree of the task's Git workspace's repository, on a branch
    /// of the task's own, from the same base (D-080).
    async fn own_workspace(
        self: &std::sync::Arc<Self>,
        id: &TaskId,
    ) -> RpcResult<otter_core::Workspace> {
        let f = self.task_get(id.as_str()).await?;
        // Its work so far would stay behind: only before it has any.
        if !f.runs.is_empty() {
            return Err(RpcError::conflict(
                "it has worked in its workspace already, and its work is there: give it its own before it starts",
            ));
        }
        let ws_id = f
            .workspace_id
            .clone()
            .ok_or_else(|| RpcError::conflict("the task has no workspace yet"))?;
        let (name, repository, base) = {
            let store = self.store.lock().await;
            let ws = store
                .workspace(ws_id.as_str())
                .ok_or_else(|| workspace_not_found(ws_id.as_str()))?;
            let otter_core::WorkspaceSource::Git(g) = &ws.source else {
                return Err(RpcError::conflict(
                    "only a Git workspace can be given to a task of its own (a worktree of its repository)",
                ));
            };
            (ws.name.clone(), g.repository.clone(), g.base.clone())
        };
        self.make_workspace_for(
            &f,
            &name,
            SourceSpec::Git {
                repository,
                branch: None,
                base,
            },
        )
        .await
    }

    /// The workspace made for a task from a source (D-179): the one
    /// already made for it if there is one, so a repeated command makes no
    /// second.
    pub(crate) async fn workspace_for_task(
        self: &Arc<Self>,
        f: &Task,
        source: SourceSpec,
    ) -> RpcResult<otter_core::Workspace> {
        {
            let store = self.store.lock().await;
            if let Some(w) = store
                .state
                .workspaces
                .iter()
                .find(|w| w.task_id.as_ref() == Some(&f.id))
            {
                return Ok(w.clone());
            }
        }
        let prefix = match &source {
            SourceSpec::Git { repository, .. } => crate::git::repo_name(repository),
            _ => "scratch".to_owned(),
        };
        self.make_workspace_for(f, &prefix, source).await
    }

    /// A workspace named `<prefix>-<title>`, on a branch of its own when it
    /// is a Git one, marked as made for the task so it is put away when the
    /// task ends (D-105). Shared by D-080 and D-179.
    async fn make_workspace_for(
        self: &Arc<Self>,
        f: &Task,
        prefix: &str,
        source: SourceSpec,
    ) -> RpcResult<otter_core::Workspace> {
        let slug = crate::git::slug(&f.title);
        let short: String =
            f.id.as_str()
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        let prefix: String = prefix.chars().take(40).collect();
        let source = match source {
            SourceSpec::Git {
                repository,
                branch,
                base,
            } => SourceSpec::Git {
                repository,
                branch: Some(branch.unwrap_or_else(|| format!("otter/{slug}-{short}"))),
                base,
            },
            other => other,
        };
        // A name taken meanwhile (two at once with the same title) is
        // looked for again.
        let mut last = None;
        for _ in 0..5 {
            let taken: Vec<String> = self
                .store
                .lock()
                .await
                .state
                .workspaces
                .iter()
                .map(|w| w.name.clone())
                .collect();
            let mut name = format!("{prefix}-{slug}");
            let mut n = 2;
            while taken.contains(&name) {
                name = format!("{prefix}-{slug}-{n}");
                n += 1;
            }
            let made = self
                .workspace_create_for(
                    otter_protocol::WorkspaceCreate {
                        name,
                        brief: Some(otter_core::Brief {
                            title: Some(f.title.clone()),
                            description: Some(format!(
                                "The task “{}”, in a workspace of its own",
                                f.title
                            )),
                            ..Default::default()
                        }),
                        source: source.clone(),
                        sessions: Some(vec![]),
                    },
                    Some(f.id.clone()),
                )
                .await;
            match made {
                Err(e) if e.code == otter_protocol::ErrorCode::AlreadyExists => last = Some(e),
                other => return other,
            }
        }
        Err(last.expect("tried at least once"))
    }

    /// When a task ends (done or cancelled), the workspace made for it
    /// (D-080) is archived (D-105): files, branch and conversations kept,
    /// nothing left running. Not while the task still asks something (a
    /// push to allow, say: answering it tries again), nor while something
    /// the developer may be using runs there — a shell, a service, a
    /// command, an agent at work or waiting on them; then it stays.
    pub(crate) async fn put_away_workspace(self: &std::sync::Arc<Self>, id: &TaskId) {
        let Ok(f) = self.task_get(id.as_str()).await else {
            return;
        };
        if f.pending_decisions().next().is_some() {
            return;
        }
        let Some(ws_id) = f.workspace_id.clone() else {
            return;
        };
        let name = {
            let store = self.store.lock().await;
            let Some(ws) = store.workspace(ws_id.as_str()) else {
                return;
            };
            if ws.task_id.as_ref() != Some(id) || ws.state == otter_core::WorkspaceState::Archived {
                return;
            }
            if let Some(s) = ws.sessions.iter().find(|s| in_use(s)) {
                tracing::info!(workspace = %ws.name, session = %s.name, "task ended; its workspace stays: in use");
                return;
            }
            ws.name.clone()
        };
        if let Err(e) = self.workspace_archive(ws_id.as_str()).await {
            tracing::warn!(workspace = %name, "archiving a finished task's workspace: {}", e.message);
            return;
        }
        let text = format!(
            "Its workspace {name} is archived now that the task is over: its files and branch are kept. Unarchive it to look at the work."
        );
        let applied = self.tasks.lock().await.apply(id, None, |f, _| {
            let (m, c) = message(MessageRole::Controller, text, None);
            f.messages.push(m);
            Ok(vec![c])
        });
        if let Ok(a) = applied {
            self.task_changed(&a);
        }
    }

    pub(crate) async fn task_act(self: std::sync::Arc<Self>, p: TaskAct) -> RpcResult<Task> {
        check_command_id(&p.command_id)?;
        let id = TaskId::from(p.task.as_str());
        let mut p = p;
        // A workspace of its own (D-080): made now, then chosen like any.
        if p.action == TaskAction::OwnWorkspace {
            if let Some(f) = self.tasks.lock().await.applied(&p.command_id) {
                return Ok(f.clone());
            }
            let ws = self.own_workspace(&id).await?;
            p.action = TaskAction::SetWorkspace {
                workspace: ws.id.to_string(),
            };
        }
        if let TaskAction::SetWorkspace { workspace } = &mut p.action {
            let store = self.store.lock().await;
            let ws = store
                .workspace(workspace)
                .ok_or_else(|| workspace_not_found(workspace))?;
            *workspace = ws.id.to_string();
        }
        if p.action == TaskAction::Preview {
            if let Some(f) = self.tasks.lock().await.applied(&p.command_id) {
                return Ok(f.clone());
            }
            self.start_preview(&id).await?;
        }
        let applied = self
            .tasks
            .lock()
            .await
            .apply(&id, Some(&p.command_id), |f, now| {
                apply_action(f, &p.action, &p.command_id, now)
            })?;
        self.task_changed(&applied);
        if applied.duplicate {
            return Ok(applied.task);
        }
        // Answered what an ended task still asked: its own workspace may be
        // put away now (D-105).
        if matches!(p.action, TaskAction::Decide { .. })
            && matches!(
                applied.task.status,
                TaskStatus::Done | TaskStatus::Cancelled
            )
        {
            self.finished.lock().unwrap().push(id.clone());
            self.task_wake.notify_one();
        }
        // What the action means for a run in progress.
        use otter_core::task::RunState;
        match &p.action {
            TaskAction::Decide {
                decision_id,
                approve,
                answer,
                answers,
            } => {
                let by = otter_core::task::Decider::User;
                let reply = crate::runs::decision_reply(
                    *approve,
                    answer.clone(),
                    answers.clone(),
                    by,
                    None,
                );
                self.forward_decision(&id, decision_id, reply, by).await
            }
            // The coding agent's turn is interrupted and the run ends; its
            // conversation is held (pause) or closed (cancel) (D-059).
            TaskAction::Pause => self.hold_runs(&id, false, "Paused").await,
            TaskAction::Cancel => self.hold_runs(&id, true, "Cancelled").await,
            TaskAction::Interrupt => self.interrupt_runs(&id).await,
            TaskAction::Redirect { text } => {
                // The turn carries the message's own id, as guidance would
                // (D-059): passed on once, whichever way it goes.
                let message = applied
                    .task
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.correlation_id.as_deref() == Some(p.command_id.as_str()))
                    .map(|m| m.id.to_string())
                    .unwrap_or_else(|| p.command_id.clone());
                self.redirect_runs(&id, &message, text.trim(), vec![]).await
            }
            // The plan is about to change: the step in progress may not be in it.
            TaskAction::RequestChanges { .. } => {
                self.stop_runs(&id, RunState::Cancelled, "The goal changed")
                    .await
            }
            _ => return Ok(applied.task),
        }
        self.task_get(id.as_str()).await
    }

    /// Start (or restart) the app's preview for the developer: the first
    /// browser check's preview command, as a service session on a free
    /// port. A client reaches it by forwarding that one port (D-046).
    async fn start_preview(self: &std::sync::Arc<Self>, id: &TaskId) -> RpcResult<()> {
        use otter_core::{SessionKind, SessionSpec};
        let f = self.task_get(id.as_str()).await?;
        let (root, _) = self.task_workspace(&f).await?;
        let check = crate::browser::load_checks(&root)
            .map_err(|e| RpcError::invalid(format!("{e:#}")))?
            .into_iter()
            .next()
            .ok_or_else(|| {
                RpcError::conflict(format!(
                    "the project declares no preview ({})",
                    crate::browser::CHECKS_FILE
                ))
            })?;
        match self
            .allowance(&f, &check.preview.command, "to preview the app", &root)
            .await?
        {
            crate::controller::Allowance::Allowed => {}
            crate::controller::Allowance::Pending => {
                return Err(RpcError::conflict(format!(
                    "allow `{}` first (see Approvals), then preview again",
                    check.preview.command
                )));
            }
            crate::controller::Allowance::Denied(why) => {
                return Err(RpcError::conflict(format!(
                    "the preview isn't allowed: {why}"
                )));
            }
        }
        let ws = f.workspace_id.clone().expect("checked by task_workspace");
        // One preview at a time: replace the last one.
        if let Some(old) = &f.preview {
            let _ = self
                .session_delete(&otter_protocol::SessionRef {
                    workspace: ws.to_string(),
                    session: old.session_id.to_string(),
                })
                .await;
        }
        let port = crate::browser::free_port()
            .map_err(|e| RpcError::internal(format!("no free port: {e:#}")))?;
        let session = self
            .session_create(otter_protocol::SessionCreate {
                workspace: ws.to_string(),
                spec: SessionSpec {
                    name: Some(format!("preview-{port}")),
                    kind: SessionKind::Service,
                    command: Some(check.preview.command.replace("{port}", &port.to_string())),
                    provider: None,
                    prompt: None,
                    resume: None,
                },
            })
            .await?;
        let link = otter_core::task::PreviewLink {
            session_id: session.id,
            port,
            path: check.preview.ready_path.clone(),
        };
        let applied = self.tasks.lock().await.apply(id, None, move |f, _| {
            f.preview = Some(link);
            Ok(vec![])
        })?;
        self.task_changed(&applied);
        Ok(())
    }

    /// A file a task's checks produced, by name, from its artifacts.
    pub(crate) async fn task_artifact(
        &self,
        p: &otter_protocol::task::TaskArtifact,
    ) -> RpcResult<otter_protocol::fs::FileChunk> {
        use base64::Engine;
        const MAX: u64 = 8 << 20;
        let f = self.task_get(&p.task).await?;
        let name = p.name.trim_start_matches("artifact:");
        if name.is_empty() || name.contains('/') || name.contains("..") {
            return Err(RpcError::invalid("not an artifact name"));
        }
        let path = self.artifacts_dir(&f.id).join(name);
        let meta = std::fs::metadata(&path)
            .map_err(|_| RpcError::not_found(format!("no artifact `{name}`")))?;
        if meta.len() > MAX {
            return Err(RpcError::invalid("the artifact is too large"));
        }
        let bytes = std::fs::read(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        Ok(otter_protocol::fs::FileChunk {
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            size: meta.len(),
            eof: true,
        })
    }

    pub(crate) async fn task_events(&self, p: &TaskEvents) -> RpcResult<Vec<TaskEventRecord>> {
        let tasks = self.tasks.lock().await;
        let id = TaskId::from(p.task.as_str());
        if tasks.get(id.as_str()).is_none() {
            return Err(task_not_found(&p.task));
        }
        let limit = p.limit.map_or(DEFAULT_HISTORY_LIMIT, |l| l as usize);
        tasks
            .history(&id, p.after.unwrap_or(0), limit)
            .map_err(|e| RpcError::internal(format!("reading history: {e:#}")))
    }

    /// Delete a task that isn't running. A task being worked on
    /// (planning, implementing, verifying, or with a run still going) must
    /// be paused or cancelled first. The workspace it used stays.
    pub(crate) async fn task_delete(&self, id: &str) -> RpcResult<()> {
        let id = TaskId::from(id);
        let mut tasks = self.tasks.lock().await;
        let f = tasks
            .get(id.as_str())
            .ok_or_else(|| task_not_found(id.as_str()))?;
        if f.status.is_active() || f.live_run().is_some() {
            return Err(RpcError::conflict(format!(
                "the task is {}: pause or cancel it first",
                f.status.as_str()
            )));
        }
        // How it ended stays in the log; its history goes with it (D-134).
        let (status, why) = (
            f.status.as_str(),
            otter_core::redact::redact(f.status_reason.as_deref().unwrap_or_default()),
        );
        tasks
            .remove(&id)
            .map_err(|e| RpcError::internal(format!("deleting task: {e:#}")))?;
        drop(tasks);
        self.conversations.remove_for_task(&id);
        tracing::info!(task = %id, status, reason = %why, "task deleted");
        self.events.emit(Event::TaskDeleted { task_id: id });
        Ok(())
    }

    /// Tell subscribers (and the controller) that a task changed.
    pub(crate) fn task_changed(&self, applied: &Applied) {
        if applied.duplicate {
            return;
        }
        // Just ended: the controller puts its own workspace away (D-105).
        if applied.from.is_some_and(|s| s != applied.task.status)
            && matches!(
                applied.task.status,
                TaskStatus::Done | TaskStatus::Cancelled
            )
        {
            self.finished.lock().unwrap().push(applied.task.id.clone());
        }
        self.events.emit(Event::TaskChanged {
            task_id: applied.task.id.clone(),
            history_seq: applied.task.history_seq,
            status: applied.task.status,
        });
        self.task_wake.notify_one();
    }
}

/// Something the developer may be using: a shell, a service or a command
/// running, or an agent at work or waiting on them.
fn in_use(s: &otter_core::Session) -> bool {
    s.current_execution().is_some_and(|e| e.is_running())
        && s.agent.as_ref().is_none_or(|a| {
            matches!(
                a.state,
                otter_core::AgentState::Working
                    | otter_core::AgentState::Starting
                    | otter_core::AgentState::Blocked
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::task::{DecisionKind, DecisionRequest, Risk, Step};
    use otter_core::{DecisionId, StepId};

    fn store() -> (tempfile::TempDir, TaskStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = TaskStore::load(&dir.path().join("tasks")).unwrap();
        (dir, s)
    }

    fn new_task(s: &mut TaskStore, cmd: &str) -> Task {
        let f = Task::new("Export CSV".into(), "please".into(), Utc::now());
        s.insert(
            f,
            Some(cmd),
            vec![Change::new(TaskEvent::TaskCreated {
                title: "Export CSV".into(),
            })],
        )
        .unwrap()
        .task
    }

    fn step(title: &str, status: StepStatus, planning: bool) -> Step {
        Step {
            id: StepId::generate(),
            title: title.into(),
            detail: None,
            status,
            depends_on: vec![],
            workspace_id: None,
            session_id: None,
            attempts: 0,
            last_error: None,
            planning,
        }
    }

    /// Planning again goes back to the coding agent (D-084): its planning
    /// turn is due again, a plan waiting for the developer's OK is moot, and
    /// the task plans — even resumed or retried — until a plan is set.
    #[test]
    fn planning_again_is_the_agents_turn_again() {
        let now = Utc::now();
        // An answered question: nothing was planned, so nothing is revised.
        let mut f = Task::new("t".into(), "r".into(), now);
        f.status = TaskStatus::Done;
        f.steps = vec![step("Look", StepStatus::Done, true)];
        replan(&mut f, "more", now);
        assert_eq!(f.status, TaskStatus::Planning);
        assert!(!f.replan);
        assert_eq!(f.steps[0].status, StepStatus::Pending);
        // Work planned, a revised plan waiting on the developer.
        let mut f = Task::new("t".into(), "r".into(), now);
        f.status = TaskStatus::Blocked;
        let mut look = step("Look", StepStatus::Done, true);
        look.detail = Some("- vague\n".into());
        f.steps = vec![look, step("Build", StepStatus::Done, false)];
        f.decisions.push(DecisionRequest {
            id: DecisionId::generate(),
            step_id: Some(f.steps[0].id.clone()),
            run_id: None,
            kind: DecisionKind::PlanApproval,
            risk: Risk::Low,
            summary: "Go ahead?".into(),
            detail: None,
            command: None,
            options: vec![],
            status: DecisionStatus::Pending,
            decided_by: None,
            answer: None,
            rationale: None,
            created_at: now,
            decided_at: None,
            user_only: true,
            grant: None,
        });
        replan(&mut f, "changed", now);
        assert!(f.replan);
        assert_eq!(f.steps[0].status, StepStatus::Pending);
        assert_eq!(f.steps[0].detail, None, "a fresh look, not a revision");
        assert_eq!(f.decisions[0].status, DecisionStatus::Cancelled);
        // All the work done, but planning again: still planning.
        assert_eq!(resume_status(&f), TaskStatus::Planning);
        f.replan = false;
        f.steps[0].status = StepStatus::Done;
        assert_eq!(resume_status(&f), TaskStatus::Verifying);
        // Sent back by the review, then the goal changed before the agent
        // looked again: a fresh look, not the old points.
        f.steps[0].status = StepStatus::Pending;
        f.steps[0].detail = Some("- vague\n".into());
        replan(&mut f, "changed again", now);
        assert_eq!(f.steps[0].detail, None);
    }

    #[test]
    fn state_and_history_survive_a_reload() {
        let (dir, mut s) = store();
        let f = new_task(&mut s, "c1");
        s.apply(&f.id, Some("c2"), |f, now| {
            apply_action(f, &TaskAction::Start, "c2", now)
        })
        .unwrap();
        s.apply(&f.id, Some("c3"), |f, _| {
            let (m, c) = message(MessageRole::User, "hello".into(), Some("c3".into()));
            f.messages.push(m);
            Ok(vec![c])
        })
        .unwrap();

        let again = TaskStore::load(&dir.path().join("tasks")).unwrap();
        let g = again.get(f.id.as_str()).unwrap();
        assert_eq!(g.status, TaskStatus::Planning);
        assert_eq!(g.messages.last().unwrap().text, "hello");
        let h = again.history(&f.id, 0, 100).unwrap();
        let seqs: Vec<u64> = h.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        assert_eq!(g.history_seq, 4);
        assert!(h.iter().all(|r| r.v == TASK_EVENT_SCHEMA));
        assert_eq!(h[3].correlation_id.as_deref(), Some("c3"));
        // Cursor replay: only what came after.
        assert_eq!(again.history(&f.id, 2, 100).unwrap().len(), 2);
        assert_eq!(again.history(&f.id, 4, 100).unwrap().len(), 0);
    }

    #[test]
    fn a_repeated_command_is_applied_once() {
        let (_dir, mut s) = store();
        let f = new_task(&mut s, "c1");
        let send = |s: &mut TaskStore| {
            s.apply(&f.id, Some("c2"), |f, _| {
                let (m, c) = message(MessageRole::User, "once".into(), Some("c2".into()));
                f.messages.push(m);
                Ok(vec![c])
            })
            .unwrap()
        };
        assert!(!send(&mut s).duplicate);
        let second = send(&mut s);
        assert!(second.duplicate);
        assert_eq!(second.task.history_seq, 2);
        let g = s.get(f.id.as_str()).unwrap();
        assert_eq!(g.messages.iter().filter(|m| m.text == "once").count(), 1);
        assert_eq!(s.applied("c1").unwrap().id, f.id);
    }

    #[test]
    fn a_failed_step_changes_nothing() {
        let (_dir, mut s) = store();
        let f = new_task(&mut s, "c1");
        let err = s
            .apply(&f.id, Some("c2"), |f, now| {
                apply_action(
                    f,
                    &TaskAction::Accept {
                        override_gates: false,
                    },
                    "c2",
                    now,
                )
            })
            .err()
            .unwrap();
        assert_eq!(err.code, otter_protocol::ErrorCode::Conflict);
        let g = s.get(f.id.as_str()).unwrap();
        assert_eq!(g.status, TaskStatus::Draft);
        assert_eq!(g.history_seq, 1);
        // Not remembered as applied: a corrected retry may use it.
        assert!(s.applied("c2").is_none());
    }

    #[test]
    fn deciding_the_last_pending_decision_unblocks() {
        let (_dir, mut s) = store();
        let f = new_task(&mut s, "c1");
        let dec = DecisionId::generate();
        let d2 = dec.clone();
        s.apply(&f.id, None, move |f, now| {
            f.transition(TaskStatus::Planning, None, now).unwrap();
            f.steps.push(Step {
                id: StepId::generate(),
                title: "t".into(),
                detail: None,
                status: StepStatus::Running,
                depends_on: vec![],
                workspace_id: None,
                session_id: None,
                attempts: 1,
                last_error: None,
                planning: false,
            });
            f.transition(TaskStatus::Implementing, None, now).unwrap();
            f.transition(TaskStatus::Blocked, None, now).unwrap();
            f.decisions.push(DecisionRequest {
                id: d2,
                step_id: None,
                run_id: None,
                kind: DecisionKind::ToolPermission,
                risk: Risk::High,
                summary: "rm -rf build".into(),
                detail: None,
                command: None,
                options: vec![],
                status: DecisionStatus::Pending,
                decided_by: None,
                answer: None,
                rationale: None,
                created_at: now,
                decided_at: None,
                user_only: true,
                grant: None,
            });
            Ok(vec![])
        })
        .unwrap();
        let action = TaskAction::Decide {
            decision_id: dec.clone(),
            approve: false,
            answer: None,
            answers: None,
        };
        let out = s
            .apply(&f.id, Some("c9"), |f, now| {
                apply_action(f, &action, "c9", now)
            })
            .unwrap();
        assert_eq!(out.task.status, TaskStatus::Implementing);
        assert_eq!(out.task.decisions[0].status, DecisionStatus::Denied);
        assert_eq!(out.task.decisions[0].decided_by, Some(Decider::User));
        // Deciding again conflicts.
        let again = s.apply(&f.id, Some("c10"), |f, now| {
            apply_action(f, &action, "c10", now)
        });
        assert!(again.is_err());
    }

    /// A task blocked on a recovery decision for its blocked step.
    fn awaiting_recovery(s: &mut TaskStore, cmd: &str) -> (Task, DecisionId) {
        let f = new_task(s, cmd);
        let dec = DecisionId::generate();
        let d2 = dec.clone();
        let f = s
            .apply(&f.id, None, move |f, now| {
                f.transition(TaskStatus::Planning, None, now).unwrap();
                let step = StepId::generate();
                f.steps.push(Step {
                    id: step.clone(),
                    title: "t".into(),
                    detail: None,
                    status: StepStatus::Blocked,
                    depends_on: vec![],
                    workspace_id: None,
                    session_id: None,
                    attempts: 1,
                    last_error: Some("otterd restarted".into()),
                    planning: false,
                });
                f.transition(TaskStatus::Implementing, None, now).unwrap();
                f.transition(TaskStatus::Blocked, None, now).unwrap();
                f.decisions.push(DecisionRequest {
                    id: d2,
                    step_id: Some(step),
                    run_id: None,
                    kind: DecisionKind::Recovery,
                    risk: Risk::High,
                    summary: "Start “t” over?".into(),
                    detail: None,
                    command: None,
                    options: vec![],
                    status: DecisionStatus::Pending,
                    decided_by: None,
                    answer: None,
                    rationale: None,
                    created_at: now,
                    decided_at: None,
                    user_only: true,
                    grant: None,
                });
                Ok(vec![])
            })
            .unwrap()
            .task;
        (f, dec)
    }

    fn decide(s: &mut TaskStore, f: &Task, dec: &DecisionId, approve: bool) -> Task {
        let action = TaskAction::Decide {
            decision_id: dec.clone(),
            approve,
            answer: None,
            answers: None,
        };
        s.apply(&f.id, Some("c9"), |f, now| {
            apply_action(f, &action, "c9", now)
        })
        .unwrap()
        .task
    }

    #[test]
    fn starting_cut_off_work_over_frees_its_task_and_carries_on() {
        let (_dir, mut s) = store();
        let (f, dec) = awaiting_recovery(&mut s, "c1");
        let out = decide(&mut s, &f, &dec, true);
        assert_eq!(out.decisions[0].status, DecisionStatus::Approved);
        assert_eq!(out.decisions[0].decided_by, Some(Decider::User));
        assert_eq!(out.steps[0].status, StepStatus::Pending);
        assert_eq!(out.status, TaskStatus::Implementing);
    }

    #[test]
    fn declining_to_start_over_pauses_until_the_developer_resumes() {
        let (_dir, mut s) = store();
        let (f, dec) = awaiting_recovery(&mut s, "c1");
        let out = decide(&mut s, &f, &dec, false);
        assert_eq!(out.decisions[0].status, DecisionStatus::Denied);
        // Ready again, but nothing starts on its own.
        assert_eq!(out.steps[0].status, StepStatus::Pending);
        assert_eq!(out.status, TaskStatus::Paused);
        // Resuming carries on with the step.
        let out = s
            .apply(&f.id, Some("c10"), |f, now| {
                apply_action(f, &TaskAction::Resume, "c10", now)
            })
            .unwrap()
            .task;
        assert_eq!(out.status, TaskStatus::Implementing);
    }

    /// D-168 (the task bench's B10): a resume goes back to where the pause
    /// found the task — blocked on what only the developer may decide, or
    /// in review — not planning or verifying again.
    #[test]
    fn resuming_goes_back_to_where_the_pause_found_it() {
        let (_dir, mut s) = store();
        let act = |s: &mut TaskStore, f: &Task, a: TaskAction, c: &str| {
            s.apply(&f.id, Some(c), |f, now| apply_action(f, &a, c, now))
                .unwrap()
                .task
        };
        // Blocked on the developer, paused, resumed: blocked on it again,
        // for the same reason.
        let (f, _) = awaiting_recovery(&mut s, "c1");
        let f = s
            .apply(&f.id, None, |f, _| {
                f.status_reason = Some("Start “t” over?".into());
                Ok(vec![])
            })
            .unwrap()
            .task;
        let out = act(&mut s, &f, TaskAction::Pause, "c2");
        assert_eq!(out.status, TaskStatus::Paused);
        let out = act(&mut s, &f, TaskAction::Resume, "c3");
        assert_eq!(out.status, TaskStatus::Blocked);
        assert_eq!(out.status_reason.as_deref(), Some("Start “t” over?"));
        assert_eq!(out.paused_from, None);

        // In review with the publish asked: back in review, not verifying.
        let f = new_task(&mut s, "c4");
        let f = s
            .apply(&f.id, None, |f, now| {
                for to in [
                    TaskStatus::Planning,
                    TaskStatus::Implementing,
                    TaskStatus::Verifying,
                ] {
                    f.transition(to, None, now).unwrap();
                }
                f.transition(
                    TaskStatus::Review,
                    Some("Ready for your review".into()),
                    now,
                )
                .unwrap();
                f.decisions.push(DecisionRequest {
                    id: DecisionId::generate(),
                    step_id: None,
                    run_id: None,
                    kind: DecisionKind::ToolPermission,
                    risk: Risk::High,
                    summary: "Run `git push origin b` and open a pull request".into(),
                    detail: None,
                    command: None,
                    options: vec![],
                    status: DecisionStatus::Pending,
                    decided_by: None,
                    answer: None,
                    rationale: None,
                    created_at: now,
                    decided_at: None,
                    user_only: true,
                    grant: None,
                });
                Ok(vec![])
            })
            .unwrap()
            .task;
        act(&mut s, &f, TaskAction::Pause, "c5");
        let out = act(&mut s, &f, TaskAction::Resume, "c6");
        assert_eq!(out.status, TaskStatus::Review);
        assert_eq!(out.status_reason.as_deref(), Some("Ready for your review"));

        // A document paused before D-168 knows nothing of where it was: a
        // pending decision of the developer's still blocks it.
        let mut old = out.clone();
        old.transition(TaskStatus::Paused, None, Utc::now())
            .unwrap();
        old.paused_from = None;
        assert_eq!(
            resume_to(&old),
            (
                TaskStatus::Blocked,
                Some("Needs your approval: Run `git push origin b` and open a pull request".into())
            )
        );
    }

    #[test]
    fn a_newer_schema_is_skipped_not_clobbered() {
        let (dir, mut s) = store();
        let f = new_task(&mut s, "c1");
        let path = dir.path().join("tasks").join(f.id.as_str()).join(DOC);
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["schema"] = serde_json::json!(TASK_SCHEMA + 1);
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let again = TaskStore::load(&dir.path().join("tasks")).unwrap();
        assert!(again.get(f.id.as_str()).is_none());
        // Still on disk for the newer otterd.
        assert!(path.exists());
    }

    #[test]
    fn a_torn_history_line_is_ignored() {
        let (dir, mut s) = store();
        let f = new_task(&mut s, "c1");
        let path = history_path(&dir.path().join("tasks"), &f.id);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"seq":2,"ts":"2026"#).unwrap();
        let again = TaskStore::load(&dir.path().join("tasks")).unwrap();
        assert_eq!(again.history(&f.id, 0, 10).unwrap().len(), 1);
        assert_eq!(again.get(f.id.as_str()).unwrap().history_seq, 1);
    }
}
