//! Features (D-043): durable product work, owned by this daemon so it
//! survives any client — the desktop app may be closed for days.
//!
//! Kept apart from workspace state (`state.json`):
//!
//! ```text
//! state/features/<feature-id>/feature.json    the feature, rewritten atomically
//! state/features/<feature-id>/history.jsonl   what happened, append-only
//! ```
//!
//! Every change goes through [`FeatureStore::apply`]: on a copy, then the
//! document is saved (atomically, with the id of the command that caused it),
//! then the new history lines are appended, then `FeatureChanged` goes out on
//! the event log. A command id already in the document is not applied again,
//! which makes a retried delivery harmless. A crash between the save and the
//! append loses history lines, never state; loading notices and says so.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use otter_core::feature::{
    Decider, DecisionStatus, FEATURE_EVENT_SCHEMA, FEATURE_SCHEMA, Feature, FeatureAction,
    FeatureEvent, FeatureEventRecord, FeatureStatus, Message, MessageRole, TaskStatus,
};
use otter_core::{FeatureId, MessageId, Timestamp, WorkspaceId};
use otter_protocol::feature::{FeatureAct, FeatureCreate, FeatureEvents, FeatureSend};
use otter_protocol::{Event, RpcError};
use serde::{Deserialize, Serialize};

use crate::daemon::{Daemon, RpcResult, workspace_not_found};

/// Command ids remembered per feature (a retry comes soon after the original).
const REMEMBERED_COMMANDS: usize = 256;
const MAX_TITLE: usize = 200;
const MAX_TEXT: usize = 20_000;
const DEFAULT_HISTORY_LIMIT: usize = 1000;

/// `feature.json`: the feature plus the daemon's bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Doc {
    #[serde(flatten)]
    feature: Feature,
    /// Commands already applied, oldest first.
    #[serde(default)]
    commands: VecDeque<String>,
}

/// One change, as a step of [`FeatureStore::apply`] records it.
pub struct Change {
    pub event: FeatureEvent,
    pub correlation_id: Option<String>,
}

impl Change {
    pub fn new(event: FeatureEvent) -> Self {
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
    pub feature: Feature,
    /// A repeat of a command already applied: nothing changed.
    pub duplicate: bool,
}

pub struct FeatureStore {
    dir: PathBuf,
    docs: BTreeMap<FeatureId, Doc>,
}

impl FeatureStore {
    /// Load every feature under `dir`. One that can't be read (a newer
    /// schema, a broken file) is left on disk untouched and skipped.
    pub fn load(dir: &Path) -> Result<FeatureStore> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let mut docs = BTreeMap::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path().join("feature.json");
            if !path.exists() {
                continue;
            }
            match read_doc(&path) {
                Ok(mut doc) => {
                    let logged = last_seq(&history_path(dir, &doc.feature.id));
                    if logged < doc.feature.history_seq {
                        tracing::warn!(
                            feature = %doc.feature.id,
                            "history ends at {logged}, state at {}: lines were lost in a crash",
                            doc.feature.history_seq
                        );
                    }
                    doc.feature.history_seq = doc.feature.history_seq.max(logged);
                    docs.insert(doc.feature.id.clone(), doc);
                }
                Err(e) => tracing::warn!("skipping {}: {e:#}", path.display()),
            }
        }
        Ok(FeatureStore {
            dir: dir.to_path_buf(),
            docs,
        })
    }

    /// Newest activity first.
    pub fn list(&self) -> Vec<Feature> {
        let mut out: Vec<Feature> = self.docs.values().map(|d| d.feature.clone()).collect();
        out.sort_by_key(|f| std::cmp::Reverse(f.updated_at));
        out
    }

    pub fn get(&self, id: &str) -> Option<&Feature> {
        self.docs.get(&FeatureId::from(id)).map(|d| &d.feature)
    }

    /// The feature a command was applied to, if it was.
    pub fn applied(&self, command_id: &str) -> Option<&Feature> {
        self.docs
            .values()
            .find(|d| d.commands.iter().any(|c| c == command_id))
            .map(|d| &d.feature)
    }

    /// Add a new feature.
    pub fn insert(
        &mut self,
        feature: Feature,
        command_id: Option<&str>,
        changes: Vec<Change>,
    ) -> Result<Applied> {
        let doc = Doc {
            feature,
            commands: VecDeque::new(),
        };
        self.commit(doc, command_id, changes)
    }

    /// Change a feature: `step` works on a copy and returns what happened; an
    /// error leaves everything as it was. A `command_id` already applied to
    /// this feature returns it unchanged.
    pub fn apply(
        &mut self,
        id: &FeatureId,
        command_id: Option<&str>,
        step: impl FnOnce(&mut Feature, Timestamp) -> RpcResult<Vec<Change>>,
    ) -> RpcResult<Applied> {
        let doc = self
            .docs
            .get(id)
            .ok_or_else(|| feature_not_found(id.as_str()))?;
        if let Some(cmd) = command_id
            && doc.commands.iter().any(|c| c == cmd)
        {
            return Ok(Applied {
                feature: doc.feature.clone(),
                duplicate: true,
            });
        }
        let mut doc = doc.clone();
        let now = Utc::now();
        let changes = step(&mut doc.feature, now)?;
        doc.feature.updated_at = now;
        self.commit(doc, command_id, changes)
            .map_err(|e| RpcError::internal(format!("saving feature: {e:#}")))
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
            doc.feature.history_seq += 1;
            records.push(FeatureEventRecord {
                seq: doc.feature.history_seq,
                ts: now,
                v: FEATURE_EVENT_SCHEMA,
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
        let dir = self.dir.join(doc.feature.id.as_str());
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        write_doc(&dir.join("feature.json"), &doc)?;
        append_history(&dir.join("history.jsonl"), &records)?;
        let feature = doc.feature.clone();
        self.docs.insert(feature.id.clone(), doc);
        Ok(Applied {
            feature,
            duplicate: false,
        })
    }

    /// A feature's history with `seq > after`, oldest first.
    pub fn history(
        &self,
        id: &FeatureId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<FeatureEventRecord>> {
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
            let Ok(rec) = serde_json::from_str::<FeatureEventRecord>(&line) else {
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

fn history_path(dir: &Path, id: &FeatureId) -> PathBuf {
    dir.join(id.as_str()).join("history.jsonl")
}

fn read_doc(path: &Path) -> Result<Doc> {
    let bytes = std::fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let schema = value["schema"].as_u64().unwrap_or(1) as u32;
    if schema > FEATURE_SCHEMA {
        anyhow::bail!("written by a newer otterd (feature schema {schema})");
    }
    let mut doc: Doc = serde_json::from_value(migrate(value, schema))?;
    doc.feature.schema = FEATURE_SCHEMA;
    Ok(doc)
}

/// Bring an older document up to [`FEATURE_SCHEMA`]. Schema 1 is the first;
/// later versions add their steps here (`if schema < 2 { … }`).
fn migrate(value: serde_json::Value, _schema: u32) -> serde_json::Value {
    value
}

fn write_doc(path: &Path, doc: &Doc) -> Result<()> {
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

fn append_history(path: &Path, records: &[FeatureEventRecord]) -> Result<()> {
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

fn last_seq(path: &Path) -> u64 {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(|l| l.ok())
        .filter_map(|l| serde_json::from_str::<FeatureEventRecord>(&l).ok())
        .map(|r| r.seq)
        .max()
        .unwrap_or(0)
}

pub(crate) fn feature_not_found(r: &str) -> RpcError {
    RpcError::not_found(format!("no feature `{r}`"))
}

fn check_command_id(id: &str) -> RpcResult<()> {
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Err(RpcError::invalid(
            "command_id must be 1–128 printable characters",
        ));
    }
    Ok(())
}

fn check_text(what: &str, text: &str, max: usize) -> RpcResult<()> {
    if text.len() > max {
        return Err(RpcError::invalid(format!(
            "{what} is too long (max {max} bytes)"
        )));
    }
    Ok(())
}

/// Where a paused, failed or unblocked feature picks up: planning until
/// there is a plan, verifying once every task is done, else implementing.
pub fn resume_status(f: &Feature) -> FeatureStatus {
    if f.tasks.is_empty() {
        FeatureStatus::Planning
    } else if f
        .tasks
        .iter()
        .all(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Skipped))
    {
        FeatureStatus::Verifying
    } else {
        FeatureStatus::Implementing
    }
}

pub fn message(role: MessageRole, text: String, correlation: Option<String>) -> (Message, Change) {
    let m = Message {
        id: MessageId::generate(),
        role,
        text,
        at: Utc::now(),
        correlation_id: correlation.clone(),
    };
    let change = Change {
        event: FeatureEvent::MessageAdded {
            message_id: m.id.clone(),
            role,
        },
        correlation_id: correlation,
    };
    (m, change)
}

/// Apply a developer's action to a feature (the deterministic part; the
/// controller reacts to the new state).
pub fn apply_action(
    f: &mut Feature,
    action: &FeatureAction,
    command_id: &str,
    now: Timestamp,
) -> RpcResult<Vec<Change>> {
    let mut changes = vec![
        Change::new(FeatureEvent::CommandAccepted {
            command_id: command_id.to_owned(),
            action: action.name().to_owned(),
        })
        .because(command_id),
    ];
    let to = |f: &mut Feature, status: FeatureStatus, reason: Option<String>| {
        f.transition(status, reason, now)
            .map(|e| Change::new(e).because(command_id))
            .map_err(RpcError::conflict)
    };
    match action {
        FeatureAction::Start => changes.push(to(f, FeatureStatus::Planning, None)?),
        FeatureAction::Pause => {
            changes.push(to(f, FeatureStatus::Paused, Some("Paused by you".into()))?)
        }
        FeatureAction::Resume => {
            if f.status != FeatureStatus::Paused {
                return Err(RpcError::conflict("only a paused feature can be resumed"));
            }
            let next = resume_status(f);
            changes.push(to(f, next, None)?);
        }
        FeatureAction::Retry => {
            if f.status != FeatureStatus::Failed {
                return Err(RpcError::conflict("only a failed feature can be retried"));
            }
            f.budget.iterations_used = 0;
            for t in &mut f.tasks {
                if t.status == TaskStatus::Failed {
                    t.status = TaskStatus::Pending;
                    t.attempts = 0;
                }
            }
            let next = resume_status(f);
            changes.push(to(f, next, Some("Retrying with a fresh budget".into()))?);
        }
        FeatureAction::Cancel => {
            changes.push(to(
                f,
                FeatureStatus::Cancelled,
                Some("Cancelled by you".into()),
            )?);
            for d in f.decisions.iter_mut() {
                if d.status == DecisionStatus::Pending {
                    d.status = DecisionStatus::Cancelled;
                    d.decided_at = Some(now);
                    changes.push(
                        Change::new(FeatureEvent::DecisionResolved {
                            decision_id: d.id.clone(),
                            status: DecisionStatus::Cancelled,
                            by: Decider::User,
                        })
                        .because(command_id),
                    );
                }
            }
        }
        FeatureAction::Decide {
            decision_id,
            approve,
            answer,
        } => {
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
            changes.push(
                Change::new(FeatureEvent::DecisionResolved {
                    decision_id: decision_id.clone(),
                    status: d.status,
                    by: Decider::User,
                })
                .because(command_id),
            );
            if f.status == FeatureStatus::Blocked && f.pending_decisions().next().is_none() {
                let next = resume_status(f);
                changes.push(to(f, next, None)?);
            }
        }
        FeatureAction::Accept => {
            if f.status != FeatureStatus::Review {
                return Err(RpcError::conflict(
                    "only a feature in review can be accepted",
                ));
            }
            changes.push(to(f, FeatureStatus::Done, Some("Accepted by you".into()))?);
        }
        FeatureAction::TakeOver => {
            if !f.runs.iter().any(|r| r.provider_session_id.is_some()) {
                return Err(RpcError::conflict("no agent conversation to take over yet"));
            }
            if f.status != FeatureStatus::Paused {
                changes.push(to(f, FeatureStatus::Paused, Some("You took over".into()))?);
            }
        }
        FeatureAction::HandBack => {
            if f.status != FeatureStatus::Paused {
                return Err(RpcError::conflict(
                    "only a paused feature can be handed back",
                ));
            }
            let next = resume_status(f);
            changes.push(to(
                f,
                next,
                Some("Handed back to the Control Agent".into()),
            )?);
        }
        FeatureAction::RequestChanges { note } => {
            if f.status != FeatureStatus::Review {
                return Err(RpcError::conflict(
                    "only a feature in review can be sent back",
                ));
            }
            if let Some(note) = note.as_ref().filter(|n| !n.trim().is_empty()) {
                check_text("note", note, MAX_TEXT)?;
                let (m, c) = message(MessageRole::User, note.clone(), Some(command_id.to_owned()));
                f.messages.push(m);
                changes.push(c);
            }
            changes.push(to(
                f,
                FeatureStatus::Implementing,
                Some("Changes requested".into()),
            )?);
        }
    }
    Ok(changes)
}

impl Daemon {
    pub(crate) async fn feature_list(&self) -> Vec<Feature> {
        self.features.lock().await.list()
    }

    pub(crate) async fn feature_get(&self, id: &str) -> RpcResult<Feature> {
        self.features
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| feature_not_found(id))
    }

    pub(crate) async fn feature_create(&self, p: FeatureCreate) -> RpcResult<Feature> {
        check_command_id(&p.command_id)?;
        let title = p.title.trim().to_owned();
        if title.is_empty() {
            return Err(RpcError::invalid("a feature needs a title"));
        }
        check_text("title", &title, MAX_TITLE)?;
        check_text("request", &p.request, MAX_TEXT)?;
        let workspace_id: Option<WorkspaceId> = match &p.workspace {
            Some(w) => Some(
                self.store
                    .lock()
                    .await
                    .workspace(w)
                    .map(|ws| ws.id.clone())
                    .ok_or_else(|| workspace_not_found(w))?,
            ),
            None => None,
        };
        let mut features = self.features.lock().await;
        if let Some(f) = features.applied(&p.command_id) {
            return Ok(f.clone());
        }
        let mut f = Feature::new(title.clone(), p.request.trim().to_owned(), Utc::now());
        f.workspace_id = workspace_id;
        let mut changes =
            vec![Change::new(FeatureEvent::FeatureCreated { title }).because(&p.command_id)];
        if !f.request.is_empty() {
            let (m, c) = message(
                MessageRole::User,
                f.request.clone(),
                Some(p.command_id.clone()),
            );
            f.messages.push(m);
            changes.push(c);
        }
        let applied = features
            .insert(f, Some(&p.command_id), changes)
            .map_err(|e| RpcError::internal(format!("saving feature: {e:#}")))?;
        drop(features);
        self.feature_changed(&applied);
        Ok(applied.feature)
    }

    pub(crate) async fn feature_send(&self, p: FeatureSend) -> RpcResult<Feature> {
        check_command_id(&p.command_id)?;
        let text = p.text.trim().to_owned();
        if text.is_empty() {
            return Err(RpcError::invalid("the message is empty"));
        }
        check_text("message", &text, MAX_TEXT)?;
        let cmd = p.command_id.clone();
        let applied = self.features.lock().await.apply(
            &FeatureId::from(p.feature.as_str()),
            Some(&cmd),
            |f, _| {
                if f.status.is_terminal() {
                    return Err(RpcError::conflict(format!(
                        "the feature is {}",
                        f.status.as_str()
                    )));
                }
                let (m, c) = message(MessageRole::User, text, Some(cmd.clone()));
                f.messages.push(m);
                Ok(vec![c])
            },
        )?;
        self.feature_changed(&applied);
        Ok(applied.feature)
    }

    pub(crate) async fn feature_act(
        self: std::sync::Arc<Self>,
        p: FeatureAct,
    ) -> RpcResult<Feature> {
        check_command_id(&p.command_id)?;
        let id = FeatureId::from(p.feature.as_str());
        if p.action == FeatureAction::HandBack {
            self.check_handed_back(&id).await?;
        }
        // Taking over opens a session first and records the command only
        // once that worked, so a failed attempt can be retried as is.
        if p.action == FeatureAction::TakeOver {
            if let Some(f) = self.features.lock().await.applied(&p.command_id) {
                return Ok(f.clone());
            }
            let f = self.feature_get(id.as_str()).await?;
            if f.status.is_terminal() {
                return Err(RpcError::conflict(format!(
                    "the feature is {}",
                    f.status.as_str()
                )));
            }
            self.take_over(&id).await?;
        }
        let applied = self
            .features
            .lock()
            .await
            .apply(&id, Some(&p.command_id), |f, now| {
                apply_action(f, &p.action, &p.command_id, now)
            })?;
        self.feature_changed(&applied);
        if applied.duplicate {
            return Ok(applied.feature);
        }
        // What the action means for a run in progress.
        use otter_core::feature::RunState;
        match &p.action {
            FeatureAction::Decide {
                decision_id,
                approve,
                answer,
            } => {
                self.forward_decision(&id, decision_id, *approve, answer.clone())
                    .await
            }
            FeatureAction::Pause => self.stop_runs(&id, RunState::Cancelled, "Paused").await,
            FeatureAction::Cancel => self.stop_runs(&id, RunState::Cancelled, "Cancelled").await,
            _ => return Ok(applied.feature),
        }
        self.feature_get(id.as_str()).await
    }

    /// Before handing back: the developer's session must be over, so only
    /// one process ever drives the conversation.
    async fn check_handed_back(&self, id: &FeatureId) -> RpcResult<()> {
        let f = self.feature_get(id.as_str()).await?;
        let Some(ws) = &f.workspace_id else {
            return Ok(());
        };
        let store = self.store.lock().await;
        let Some(ws) = store.workspace(ws.as_str()) else {
            return Ok(());
        };
        let busy = f
            .tasks
            .iter()
            .filter_map(|t| t.session_id.as_ref())
            .any(|s| {
                ws.session(s.as_str())
                    .and_then(|s| s.current_execution())
                    .is_some_and(|e| e.is_running())
            });
        if busy {
            return Err(RpcError::conflict(
                "quit the agent in your session first, then hand back",
            ));
        }
        Ok(())
    }

    pub(crate) async fn feature_events(
        &self,
        p: &FeatureEvents,
    ) -> RpcResult<Vec<FeatureEventRecord>> {
        let features = self.features.lock().await;
        let id = FeatureId::from(p.feature.as_str());
        if features.get(id.as_str()).is_none() {
            return Err(feature_not_found(&p.feature));
        }
        let limit = p.limit.map_or(DEFAULT_HISTORY_LIMIT, |l| l as usize);
        features
            .history(&id, p.after.unwrap_or(0), limit)
            .map_err(|e| RpcError::internal(format!("reading history: {e:#}")))
    }

    /// Tell subscribers (and the controller) that a feature changed.
    pub(crate) fn feature_changed(&self, applied: &Applied) {
        if applied.duplicate {
            return;
        }
        self.events.emit(Event::FeatureChanged {
            feature_id: applied.feature.id.clone(),
            history_seq: applied.feature.history_seq,
            status: applied.feature.status,
        });
        self.feature_wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::feature::{DecisionKind, DecisionRequest, Risk, Task};
    use otter_core::{DecisionId, TaskId};

    fn store() -> (tempfile::TempDir, FeatureStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FeatureStore::load(&dir.path().join("features")).unwrap();
        (dir, s)
    }

    fn new_feature(s: &mut FeatureStore, cmd: &str) -> Feature {
        let f = Feature::new("Export CSV".into(), "please".into(), Utc::now());
        s.insert(
            f,
            Some(cmd),
            vec![Change::new(FeatureEvent::FeatureCreated {
                title: "Export CSV".into(),
            })],
        )
        .unwrap()
        .feature
    }

    #[test]
    fn state_and_history_survive_a_reload() {
        let (dir, mut s) = store();
        let f = new_feature(&mut s, "c1");
        s.apply(&f.id, Some("c2"), |f, now| {
            apply_action(f, &FeatureAction::Start, "c2", now)
        })
        .unwrap();
        s.apply(&f.id, Some("c3"), |f, _| {
            let (m, c) = message(MessageRole::User, "hello".into(), Some("c3".into()));
            f.messages.push(m);
            Ok(vec![c])
        })
        .unwrap();

        let again = FeatureStore::load(&dir.path().join("features")).unwrap();
        let g = again.get(f.id.as_str()).unwrap();
        assert_eq!(g.status, FeatureStatus::Planning);
        assert_eq!(g.messages.last().unwrap().text, "hello");
        let h = again.history(&f.id, 0, 100).unwrap();
        let seqs: Vec<u64> = h.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        assert_eq!(g.history_seq, 4);
        assert!(h.iter().all(|r| r.v == FEATURE_EVENT_SCHEMA));
        assert_eq!(h[3].correlation_id.as_deref(), Some("c3"));
        // Cursor replay: only what came after.
        assert_eq!(again.history(&f.id, 2, 100).unwrap().len(), 2);
        assert_eq!(again.history(&f.id, 4, 100).unwrap().len(), 0);
    }

    #[test]
    fn a_repeated_command_is_applied_once() {
        let (_dir, mut s) = store();
        let f = new_feature(&mut s, "c1");
        let send = |s: &mut FeatureStore| {
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
        assert_eq!(second.feature.history_seq, 2);
        let g = s.get(f.id.as_str()).unwrap();
        assert_eq!(g.messages.iter().filter(|m| m.text == "once").count(), 1);
        assert_eq!(s.applied("c1").unwrap().id, f.id);
    }

    #[test]
    fn a_failed_step_changes_nothing() {
        let (_dir, mut s) = store();
        let f = new_feature(&mut s, "c1");
        let err = s
            .apply(&f.id, Some("c2"), |f, now| {
                apply_action(f, &FeatureAction::Accept, "c2", now)
            })
            .err()
            .unwrap();
        assert_eq!(err.code, otter_protocol::ErrorCode::Conflict);
        let g = s.get(f.id.as_str()).unwrap();
        assert_eq!(g.status, FeatureStatus::Draft);
        assert_eq!(g.history_seq, 1);
        // Not remembered as applied: a corrected retry may use it.
        assert!(s.applied("c2").is_none());
    }

    #[test]
    fn deciding_the_last_pending_decision_unblocks() {
        let (_dir, mut s) = store();
        let f = new_feature(&mut s, "c1");
        let dec = DecisionId::generate();
        let d2 = dec.clone();
        s.apply(&f.id, None, move |f, now| {
            f.transition(FeatureStatus::Planning, None, now).unwrap();
            f.tasks.push(Task {
                id: TaskId::generate(),
                title: "t".into(),
                detail: None,
                status: TaskStatus::Running,
                depends_on: vec![],
                workspace_id: None,
                session_id: None,
                attempts: 1,
                last_error: None,
            });
            f.transition(FeatureStatus::Implementing, None, now)
                .unwrap();
            f.transition(FeatureStatus::Blocked, None, now).unwrap();
            f.decisions.push(DecisionRequest {
                id: d2,
                task_id: None,
                run_id: None,
                kind: DecisionKind::ToolPermission,
                risk: Risk::High,
                summary: "rm -rf build".into(),
                detail: None,
                options: vec![],
                status: DecisionStatus::Pending,
                decided_by: None,
                answer: None,
                rationale: None,
                created_at: now,
                decided_at: None,
                user_only: true,
            });
            Ok(vec![])
        })
        .unwrap();
        let action = FeatureAction::Decide {
            decision_id: dec.clone(),
            approve: false,
            answer: None,
        };
        let out = s
            .apply(&f.id, Some("c9"), |f, now| {
                apply_action(f, &action, "c9", now)
            })
            .unwrap();
        assert_eq!(out.feature.status, FeatureStatus::Implementing);
        assert_eq!(out.feature.decisions[0].status, DecisionStatus::Denied);
        assert_eq!(out.feature.decisions[0].decided_by, Some(Decider::User));
        // Deciding again conflicts.
        let again = s.apply(&f.id, Some("c10"), |f, now| {
            apply_action(f, &action, "c10", now)
        });
        assert!(again.is_err());
    }

    #[test]
    fn a_newer_schema_is_skipped_not_clobbered() {
        let (dir, mut s) = store();
        let f = new_feature(&mut s, "c1");
        let path = dir
            .path()
            .join("features")
            .join(f.id.as_str())
            .join("feature.json");
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["schema"] = serde_json::json!(FEATURE_SCHEMA + 1);
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let again = FeatureStore::load(&dir.path().join("features")).unwrap();
        assert!(again.get(f.id.as_str()).is_none());
        // Still on disk for the newer otterd.
        assert!(path.exists());
    }

    #[test]
    fn a_torn_history_line_is_ignored() {
        let (dir, mut s) = store();
        let f = new_feature(&mut s, "c1");
        let path = history_path(&dir.path().join("features"), &f.id);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"seq":2,"ts":"2026"#).unwrap();
        let again = FeatureStore::load(&dir.path().join("features")).unwrap();
        assert_eq!(again.history(&f.id, 0, 10).unwrap().len(), 1);
        assert_eq!(again.get(f.id.as_str()).unwrap().history_seq, 1);
    }
}
