//! Runtime conversations on this host (D-055, D-058): the registry the
//! actors change and clients read, kept as a journal.
//!
//! ```text
//! state/conversations/<id>/manifest.json   schema, log id
//! state/conversations/<id>/journal.jsonl   every change, in order (the authority)
//! state/conversations/<id>/snapshot.json   the conversation at a journal position
//! ```
//!
//! A change ([`Op`]) is checked against the conversation, appended to its
//! journal and synced (`File::sync_all`: on macOS that is `F_FULLFSYNC`)
//! **before** it takes effect in memory or anyone is told. If the journal
//! can't be written, the change didn't happen and the conversation turns
//! read-only: what was committed stays readable, nothing more is accepted.
//! Snapshots are written now and then (to a temporary file, synced, renamed,
//! the directory synced); they only save replaying, and a broken one is
//! rebuilt from the journal.
//!
//! On load: the snapshot, then the journal after it. A last line cut short
//! (a crash mid-write) is cut off; anything wrong before that is corruption:
//! the conversation opens read-only, never with records skipped. A run still
//! live in the journal belonged to the daemon before this one: its end is
//! journaled as a recovery record (outcome unknown; a turn sent but never
//! confirmed is `delivery: unknown`, never resent), so a second crash
//! doesn't lose it.
//!
//! Fsync blocks the calling thread (a few milliseconds per change, more on
//! macOS); changes come a few per second per run at most, so this is
//! accepted rather than moved to a blocking pool. Each conversation has its
//! own lock: one conversation's disk write doesn't hold up another's.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use otter_core::conversation::{
    Applied, CONVERSATION_SCHEMA, Conversation, Op, Rejection, TurnOutcome,
};
use otter_core::model::Timestamp;
use otter_core::{ConversationId, FeatureId};
use serde::{Deserialize, Serialize};

/// Told about every committed change: (conversation, revision, feature).
pub type Notify = Box<dyn Fn(&ConversationId, u64, Option<&FeatureId>) + Send + Sync>;

/// A snapshot every this many records.
const SNAPSHOT_EVERY: u64 = 64;

/// One journal line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Record {
    /// Which journal (a conversation's, by its own random id).
    pub log_id: String,
    /// 1, 2, 3, … with no gaps.
    pub seq: u64,
    pub schema: u32,
    pub at: Timestamp,
    /// Written by the daemon recovering from a crash.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recovery: bool,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// The conversation as it began (or as it stood when it was migrated).
    Create {
        conversation: Box<Conversation>,
    },
    Op {
        op: Box<Op>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    id: ConversationId,
    log_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    log_id: String,
    /// The last record it includes.
    seq: u64,
    conversation: Conversation,
}

/// Where a write may fail, for tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fault {
    /// Nothing written.
    BeforeAppend,
    /// Half a line written, then the write fails.
    TornAppend,
    /// Written and synced, then the process "dies" before using it.
    AfterSync,
    /// The snapshot's rename fails.
    SnapshotRename,
}

struct Entry {
    conversation: Conversation,
    log_id: String,
    seq: u64,
    since_snapshot: u64,
    journal: Option<File>,
    /// Why it no longer accepts changes.
    read_only: Option<String>,
}

pub struct Conversations {
    dir: PathBuf,
    map: Mutex<HashMap<ConversationId, Arc<Mutex<Entry>>>>,
    notify: std::sync::OnceLock<Notify>,
    #[cfg(test)]
    fault: Mutex<Option<Fault>>,
}

fn new_log_id() -> String {
    format!(
        "log_{}",
        ConversationId::generate()
            .as_str()
            .trim_start_matches("conv_")
    )
}

fn mkdir_private(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("syncing {}", dir.display()))
}

/// Write `bytes` to `path` atomically: a synced temporary file, renamed,
/// the directory synced.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    sync_dir(path.parent().expect("a directory"))
}

fn open_journal(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

impl Conversations {
    /// None loaded (the directory couldn't be read): new ones are still kept.
    pub fn empty(dir: &Path) -> Conversations {
        Conversations {
            dir: dir.to_path_buf(),
            map: Mutex::new(HashMap::new()),
            notify: std::sync::OnceLock::new(),
            #[cfg(test)]
            fault: Mutex::new(None),
        }
    }

    /// Load every conversation under `dir` (see the module docs). One that
    /// can't be read at all (a newer schema, no usable record) is left on
    /// disk untouched and skipped.
    pub fn load(dir: &Path) -> Result<Conversations> {
        mkdir_private(dir)?;
        let this = Conversations::empty(dir);
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !path.is_dir() || !ConversationId::looks_like(name) {
                continue;
            }
            match this.open(&path) {
                Ok(Some(e)) => {
                    let id = e.conversation.id.clone();
                    this.map
                        .lock()
                        .unwrap()
                        .insert(id.clone(), Arc::new(Mutex::new(e)));
                    this.recover(&id);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("skipping {}: {e:#}", path.display()),
            }
        }
        Ok(this)
    }

    /// Read one conversation's directory.
    fn open(&self, dir: &Path) -> Result<Option<Entry>> {
        let journal_path = dir.join("journal.jsonl");
        if !journal_path.exists() {
            return self.migrate(dir);
        }
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)
            .context("reading the manifest")?;
        if manifest.schema > CONVERSATION_SCHEMA {
            tracing::warn!(conversation = %manifest.id, schema = manifest.schema, "skipping a conversation from a newer otterd");
            return Ok(None);
        }
        // The checkpoint, if it's usable; else everything from the start.
        let snapshot = std::fs::read(dir.join("snapshot.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Snapshot>(&b).ok())
            .filter(|s| s.log_id == manifest.log_id);
        let (mut conversation, mut seq) = match snapshot {
            Some(s) => (Some(s.conversation), s.seq),
            None => (None, 0),
        };
        let mut read_only = None;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&journal_path)?;
        let mut reader = std::io::BufReader::new(&mut file);
        let mut good_end = 0u64;
        let mut offset = 0u64;
        let mut line = String::new();
        let mut cut: Option<u64> = None;
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            offset += n as u64;
            let complete = line.ends_with('\n');
            let parsed = serde_json::from_str::<Record>(line.trim_end());
            let rec = match (parsed, complete) {
                (Ok(r), true) => r,
                // The last line, cut short by a crash: drop it.
                (_, false) => {
                    cut = Some(good_end);
                    break;
                }
                (Err(e), true) => {
                    // A broken line at the very end may still be a torn write.
                    if reader.fill_buf()?.is_empty() {
                        cut = Some(good_end);
                    } else {
                        read_only = Some(format!("record after #{seq} is unreadable: {e}"));
                    }
                    break;
                }
            };
            if rec.log_id != manifest.log_id {
                read_only = Some(format!("record #{} belongs to another journal", rec.seq));
                break;
            }
            if rec.seq <= seq {
                good_end = offset;
                continue;
            }
            if rec.seq != seq + 1 {
                read_only = Some(format!("records jump from #{seq} to #{}", rec.seq));
                break;
            }
            let applied = match rec.body {
                Body::Create { conversation: c } => {
                    conversation = Some(*c);
                    Ok(())
                }
                Body::Op { op } => match conversation.as_mut() {
                    Some(c) => c.apply(&op).map(|_| ()).map_err(|e| e.to_string()),
                    None => Err("a change before the conversation began".into()),
                },
            };
            if let Err(e) = applied {
                read_only = Some(format!("record #{} doesn't apply: {e}", rec.seq));
                break;
            }
            seq = rec.seq;
            good_end = offset;
        }
        drop(reader);
        if let Some(at) = cut {
            tracing::warn!(dir = %dir.display(), "cutting off a journal line left unfinished by a crash");
            file.set_len(at)?;
            file.sync_all()?;
        }
        let Some(conversation) = conversation else {
            bail!("the journal has no usable record");
        };
        if let Some(why) = &read_only {
            tracing::error!(conversation = %conversation.id, "the conversation's journal is damaged ({why}); it is read-only");
        }
        file.seek(std::io::SeekFrom::End(0))?;
        Ok(Some(Entry {
            conversation,
            log_id: manifest.log_id,
            seq,
            since_snapshot: 0,
            journal: read_only
                .is_none()
                .then(|| open_journal(&journal_path))
                .transpose()?,
            read_only,
        }))
    }

    /// A conversation from before the journal (a snapshot only): its
    /// journal starts with it. The old snapshot is kept as a backup; the
    /// journal's existence marks the migration done.
    fn migrate(&self, dir: &Path) -> Result<Option<Entry>> {
        let old = dir.join("snapshot.json");
        if !old.exists() {
            return Ok(None);
        }
        let mut c: Conversation = serde_json::from_slice(&std::fs::read(&old)?)
            .context("reading a snapshot from before the journal")?;
        if c.schema > CONVERSATION_SCHEMA {
            tracing::warn!(conversation = %c.id, "skipping a conversation from a newer otterd");
            return Ok(None);
        }
        tracing::info!(conversation = %c.id, "starting the conversation's journal");
        c.schema = CONVERSATION_SCHEMA;
        std::fs::copy(&old, dir.join("snapshot.v1.json"))?;
        let entry = self.start(dir, c, Utc::now())?;
        Ok(Some(entry))
    }

    /// Write a new conversation's directory: manifest, its first record.
    fn start(&self, dir: &Path, conversation: Conversation, at: Timestamp) -> Result<Entry> {
        mkdir_private(dir)?;
        let log_id = new_log_id();
        write_atomic(
            &dir.join("manifest.json"),
            &serde_json::to_vec_pretty(&Manifest {
                schema: CONVERSATION_SCHEMA,
                id: conversation.id.clone(),
                log_id: log_id.clone(),
            })?,
        )?;
        let mut journal = open_journal(&dir.join("journal.jsonl"))?;
        let rec = Record {
            log_id: log_id.clone(),
            seq: 1,
            schema: CONVERSATION_SCHEMA,
            at,
            recovery: false,
            body: Body::Create {
                conversation: Box::new(conversation.clone()),
            },
        };
        let mut line = serde_json::to_vec(&rec)?;
        line.push(b'\n');
        journal.write_all(&line)?;
        journal.sync_all()?;
        sync_dir(dir)?;
        sync_dir(&self.dir)?;
        let _ = std::fs::remove_file(dir.join("snapshot.json"));
        Ok(Entry {
            conversation,
            log_id,
            seq: 1,
            since_snapshot: 1,
            journal: Some(journal),
            read_only: None,
        })
    }

    /// End a run the previous daemon left live, as journaled recovery
    /// records.
    fn recover(&self, id: &ConversationId) {
        let Some(c) = self.get(id.as_str()) else {
            return;
        };
        if c.active_run.is_none() {
            return;
        }
        let at = Utc::now();
        // Queued turns stay queued: the next run delivers them, once (the
        // controller's continuation doesn't restate them, D-059).
        let op = Op::RunEnded {
            generation: c.generation,
            outcome: TurnOutcome::OutcomeUnknown,
            at,
        };
        if let Some(Err(e)) = self.commit(id, op, true) {
            tracing::warn!(conversation = %id, "recovering: {e}");
        }
    }

    /// Who to tell about changes (set once, by the daemon).
    pub fn on_change(&self, notify: Notify) {
        let _ = self.notify.set(notify);
    }

    fn changed(&self, id: &ConversationId, revision: u64, feature: Option<&FeatureId>) {
        if let Some(n) = self.notify.get() {
            n(id, revision, feature);
        }
    }

    fn entry(&self, id: &str) -> Option<Arc<Mutex<Entry>>> {
        self.map
            .lock()
            .unwrap()
            .get(&ConversationId::from(id))
            .cloned()
    }

    /// Begin a conversation (journaled before it exists for anyone).
    pub fn insert(&self, c: Conversation) -> Result<()> {
        let dir = self.dir.join(c.id.as_str());
        let entry = self.start(&dir, c.clone(), Utc::now())?;
        self.map
            .lock()
            .unwrap()
            .insert(c.id.clone(), Arc::new(Mutex::new(entry)));
        self.changed(&c.id, c.revision, c.feature_id.as_ref());
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Conversation> {
        Some(self.entry(id)?.lock().unwrap().conversation.clone())
    }

    /// Why a conversation no longer accepts changes, if it doesn't.
    pub fn read_only(&self, id: &str) -> Option<String> {
        self.entry(id)?.lock().unwrap().read_only.clone()
    }

    /// Newest activity first; only `feature`'s, if given.
    pub fn list(&self, feature: Option<&str>) -> Vec<Conversation> {
        let entries: Vec<_> = self.map.lock().unwrap().values().cloned().collect();
        let mut out: Vec<Conversation> = entries
            .iter()
            .map(|e| e.lock().unwrap().conversation.clone())
            .filter(|c| {
                feature.is_none_or(|f| c.feature_id.as_ref().is_some_and(|x| x.as_str() == f))
            })
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        out
    }

    /// Apply a change: journaled first (see the module docs). `None` if
    /// there's no such conversation; a rejected change changes nothing; a
    /// journal that can't be written is `Rejection::Storage`.
    pub fn apply(&self, id: &ConversationId, op: Op) -> Option<Result<Applied, Rejection>> {
        self.commit(id, op, false)
    }

    fn commit(
        &self,
        id: &ConversationId,
        op: Op,
        recovery: bool,
    ) -> Option<Result<Applied, Rejection>> {
        let entry = self.entry(id.as_str())?;
        let mut e = entry.lock().unwrap();
        if let Some(why) = &e.read_only {
            return Some(Err(Rejection::Storage(format!(
                "the conversation is read-only: {why}"
            ))));
        }
        // Checked on a copy: a change that doesn't apply, or can't be
        // recorded, leaves the conversation as it was.
        let mut next = e.conversation.clone();
        let applied = match next.apply(&op) {
            Ok(a) => a,
            Err(r) => return Some(Err(r)),
        };
        if next.revision == e.conversation.revision {
            // Nothing changed (a repeat): nothing to record.
            return Some(Ok(applied));
        }
        let rec = Record {
            log_id: e.log_id.clone(),
            seq: e.seq + 1,
            schema: CONVERSATION_SCHEMA,
            at: Utc::now(),
            recovery,
            body: Body::Op { op: Box::new(op) },
        };
        if let Err(err) = self.append(&mut e, &rec) {
            let why = format!("the journal couldn't be written ({err:#})");
            tracing::error!(conversation = %id, "{why}; the conversation is read-only");
            e.read_only = Some(why.clone());
            e.journal = None;
            return Some(Err(Rejection::Storage(why)));
        }
        e.seq = rec.seq;
        e.conversation = next;
        e.since_snapshot += 1;
        if e.since_snapshot >= SNAPSHOT_EVERY {
            self.snapshot(&mut e);
        }
        let (revision, feature) = (e.conversation.revision, e.conversation.feature_id.clone());
        drop(e);
        self.changed(id, revision, feature.as_ref());
        Some(Ok(applied))
    }

    fn append(&self, e: &mut Entry, rec: &Record) -> Result<()> {
        let mut line = serde_json::to_vec(rec)?;
        line.push(b'\n');
        #[cfg(test)]
        let fault = *self.fault.lock().unwrap();
        let journal = e.journal.as_mut().context("no journal")?;
        #[cfg(test)]
        match fault {
            Some(Fault::BeforeAppend) => bail!("injected: before append"),
            Some(Fault::TornAppend) => {
                journal.write_all(&line[..line.len() / 2])?;
                journal.sync_all()?;
                bail!("injected: torn append");
            }
            _ => {}
        }
        journal.write_all(&line)?;
        journal.sync_all()?;
        #[cfg(test)]
        if fault == Some(Fault::AfterSync) {
            bail!("injected: after sync");
        }
        Ok(())
    }

    /// Checkpoint (best effort: the journal is the authority).
    fn snapshot(&self, e: &mut Entry) {
        let dir = self.dir.join(e.conversation.id.as_str());
        let write = || -> Result<()> {
            #[cfg(test)]
            if *self.fault.lock().unwrap() == Some(Fault::SnapshotRename) {
                bail!("injected: snapshot rename");
            }
            write_atomic(
                &dir.join("snapshot.json"),
                &serde_json::to_vec(&Snapshot {
                    log_id: e.log_id.clone(),
                    seq: e.seq,
                    conversation: e.conversation.clone(),
                })?,
            )
        };
        match write() {
            Ok(()) => e.since_snapshot = 0,
            Err(err) => tracing::warn!(conversation = %e.conversation.id, "snapshot: {err:#}"),
        }
    }

    /// The journal's records after `after` (a seq), at most `limit` and
    /// about `max_bytes` (always at least one); and the journal's id.
    /// `None`: no such conversation.
    pub fn history(
        &self,
        id: &str,
        after: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Option<Result<(String, Vec<Record>)>> {
        let entry = self.entry(id)?;
        let log_id = entry.lock().unwrap().log_id.clone();
        let path = self.dir.join(id).join("journal.jsonl");
        let read = || -> Result<Vec<Record>> {
            let f = File::open(&path)?;
            let mut out = Vec::new();
            let mut bytes = 0;
            for line in std::io::BufReader::new(f).lines() {
                let line = line?;
                let Ok(rec) = serde_json::from_str::<Record>(&line) else {
                    break;
                };
                if rec.seq <= after {
                    continue;
                }
                bytes += line.len();
                if !out.is_empty() && (out.len() >= limit || bytes > max_bytes) {
                    break;
                }
                out.push(rec);
            }
            Ok(out)
        };
        Some(read().map(|r| (log_id, r)))
    }

    /// Forget a feature's conversations (when the feature is deleted).
    /// Only Otter's records go: the provider's own transcripts stay.
    pub fn remove_for_feature(&self, feature: &FeatureId) {
        let mut map = self.map.lock().unwrap();
        let ids: Vec<ConversationId> = map
            .iter()
            .filter(|(_, e)| {
                let e = e.lock().unwrap();
                e.conversation.feature_id.as_ref() == Some(feature)
                    && e.conversation.active_run.is_none()
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            map.remove(&id);
            let dir = self.dir.join(id.as_str());
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!("removing {}: {e}", dir.display());
            }
        }
    }

    #[cfg(test)]
    pub fn inject(&self, fault: Option<Fault>) {
        *self.fault.lock().unwrap() = fault;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::conversation::{Accepted, Command, Delivery, Initiator, InputBlock, TurnState};
    use otter_core::{RunId, TurnId};

    fn new_conv(convs: &Conversations) -> ConversationId {
        let mut c = Conversation::new("claude", "sdk", "ws_1".into(), Utc::now());
        c.feature_id = Some("ft_1".into());
        let id = c.id.clone();
        convs.insert(c).unwrap();
        id
    }

    fn accept(cmd: &str, text: &str, turn: &TurnId) -> Op {
        Op::Accept {
            command_id: cmd.into(),
            fingerprint: text.into(),
            command: Command::SendTurn {
                initiator: Initiator::Controller,
                input: vec![InputBlock::Text { text: text.into() }],
            },
            turn_id: Some(turn.clone()),
            at: Utc::now(),
        }
    }

    /// A conversation with a run that is sending turn `a`, with `b` queued.
    fn mid_turn(convs: &Conversations, id: &ConversationId) -> (TurnId, TurnId) {
        let (a, b) = (TurnId::generate(), TurnId::generate());
        let at = Utc::now();
        for op in [
            accept("cmd_1", "a", &a),
            accept("cmd_2", "b", &b),
            Op::Claim {
                run: RunId::from("run_1"),
                runtime: None,
                at,
            },
            Op::Sending {
                turn: a.clone(),
                generation: 1,
                at,
            },
        ] {
            convs.apply(id, op).unwrap().unwrap();
        }
        (a, b)
    }

    fn journal(dir: &Path, id: &ConversationId) -> Vec<Record> {
        std::fs::read_to_string(dir.join(id.as_str()).join("journal.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn changes_are_journaled_and_replayed_after_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let id = new_conv(&convs);
        let (sent, queued) = mid_turn(&convs, &id);
        let before = convs.get(id.as_str()).unwrap();
        let recs = journal(dir.path(), &id);
        assert_eq!(recs.len(), 5, "create + 4 changes");
        assert!(recs.iter().enumerate().all(|(i, r)| r.seq == i as u64 + 1));
        assert!(matches!(recs[0].body, Body::Create { .. }));

        // The daemon dies mid-turn.
        drop(convs);
        let again = Conversations::load(dir.path()).unwrap();
        let c = again.get(id.as_str()).unwrap();
        assert!(c.active_run.is_none());
        let t = c.turn(&sent).unwrap();
        assert_eq!(t.delivery, Delivery::Unknown, "never resent on its own");
        assert_eq!(t.outcome, Some(TurnOutcome::OutcomeUnknown));
        // Queued turns wait for the next run, to be delivered once (D-059).
        let q = c.turn(&queued).unwrap();
        assert_eq!(q.state, TurnState::Queued);
        assert_eq!(c.next_queued().unwrap().id, queued);
        // The recovery itself is journaled: a second crash keeps it.
        let recs = journal(dir.path(), &id);
        assert_eq!(recs.iter().filter(|r| r.recovery).count(), 1);
        drop(again);
        let third = Conversations::load(dir.path()).unwrap();
        let c3 = third.get(id.as_str()).unwrap();
        assert_eq!(c3, c);
        assert_eq!(
            journal(dir.path(), &id).len(),
            recs.len(),
            "nothing more to recover"
        );
        // Commands are still known: a retry gets its receipt back.
        let again = third
            .apply(&id, accept("cmd_1", "a", &TurnId::generate()))
            .unwrap()
            .unwrap();
        assert!(
            matches!(again, Applied::Accepted(Accepted::Repeat(r)) if r.durable && r.turn_id.as_ref() == Some(&sent))
        );
        assert_eq!(before.turns.len(), c3.turns.len(), "no duplicate turn");
    }

    #[test]
    fn snapshots_save_replaying_and_a_bad_one_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let id = new_conv(&convs);
        for i in 0..(SNAPSHOT_EVERY + 5) {
            convs
                .apply(
                    &id,
                    accept(&format!("cmd_{i}"), &format!("t{i}"), &TurnId::generate()),
                )
                .unwrap()
                .unwrap();
        }
        let snap = dir.path().join(id.as_str()).join("snapshot.json");
        assert!(snap.exists());
        let want = convs.get(id.as_str()).unwrap();
        drop(convs);
        assert_eq!(
            Conversations::load(dir.path())
                .unwrap()
                .get(id.as_str())
                .unwrap(),
            want
        );
        // A damaged snapshot: rebuilt from the journal.
        std::fs::write(&snap, b"{ not json").unwrap();
        assert_eq!(
            Conversations::load(dir.path())
                .unwrap()
                .get(id.as_str())
                .unwrap(),
            want
        );
    }

    #[test]
    fn a_torn_last_line_is_cut_off_and_damage_before_it_makes_it_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let id = new_conv(&convs);
        convs
            .apply(&id, accept("cmd_1", "a", &TurnId::generate()))
            .unwrap()
            .unwrap();
        let want = convs.get(id.as_str()).unwrap();
        drop(convs);
        let path = dir.path().join(id.as_str()).join("journal.jsonl");
        let good = std::fs::read(&path).unwrap();
        let mut torn = good.clone();
        torn.extend_from_slice(br#"{"log_id":"x","seq":3,"#);
        std::fs::write(&path, &torn).unwrap();
        let again = Conversations::load(dir.path()).unwrap();
        assert_eq!(again.get(id.as_str()).unwrap(), want);
        assert!(again.read_only(id.as_str()).is_none());
        assert_eq!(std::fs::read(&path).unwrap(), good, "the torn line is gone");
        // It goes on from there.
        again
            .apply(&id, accept("cmd_2", "b", &TurnId::generate()))
            .unwrap()
            .unwrap();
        drop(again);

        // A broken record before the last: read-only, nothing skipped.
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        lines[1] = "garbage".into();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let damaged = Conversations::load(dir.path()).unwrap();
        assert!(
            damaged
                .read_only(id.as_str())
                .unwrap()
                .contains("unreadable")
        );
        assert!(
            damaged.get(id.as_str()).unwrap().turns.is_empty(),
            "only what precedes the damage"
        );
        assert!(matches!(
            damaged.apply(&id, accept("cmd_3", "c", &TurnId::generate())),
            Some(Err(Rejection::Storage(_)))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            lines.join("\n") + "\n",
            "untouched"
        );
    }

    #[test]
    fn a_failed_write_changes_nothing_and_turns_the_conversation_read_only() {
        for fault in [Fault::BeforeAppend, Fault::TornAppend] {
            let dir = tempfile::tempdir().unwrap();
            let convs = Conversations::load(dir.path()).unwrap();
            let id = new_conv(&convs);
            let before = convs.get(id.as_str()).unwrap();
            convs.inject(Some(fault));
            let r = convs
                .apply(&id, accept("cmd_1", "a", &TurnId::generate()))
                .unwrap();
            assert!(matches!(r, Err(Rejection::Storage(_))), "{fault:?}");
            convs.inject(None);
            assert_eq!(
                convs.get(id.as_str()).unwrap(),
                before,
                "{fault:?}: not applied"
            );
            assert!(convs.read_only(id.as_str()).is_some());
            // After a restart the conversation is as it was, and writable.
            drop(convs);
            let again = Conversations::load(dir.path()).unwrap();
            assert_eq!(again.get(id.as_str()).unwrap(), before, "{fault:?}");
            assert!(again.read_only(id.as_str()).is_none());
            again
                .apply(&id, accept("cmd_1", "a", &TurnId::generate()))
                .unwrap()
                .unwrap();
        }
    }

    #[test]
    fn a_change_synced_before_a_crash_is_there_after_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let id = new_conv(&convs);
        let turn = TurnId::generate();
        convs.inject(Some(Fault::AfterSync));
        // The caller hears it failed (the "process died" before it could
        // use it) — but it was recorded.
        assert!(
            convs
                .apply(&id, accept("cmd_1", "a", &turn))
                .unwrap()
                .is_err()
        );
        drop(convs);
        let again = Conversations::load(dir.path()).unwrap();
        let c = again.get(id.as_str()).unwrap();
        assert_eq!(c.turns.len(), 1);
        assert_eq!(c.turns[0].id, turn);
        // The retry is recognized, not applied twice.
        let r = again
            .apply(&id, accept("cmd_1", "a", &TurnId::generate()))
            .unwrap()
            .unwrap();
        assert!(matches!(r, Applied::Accepted(Accepted::Repeat(_))));
        assert_eq!(again.get(id.as_str()).unwrap().turns.len(), 1);
    }

    #[test]
    fn a_snapshot_that_fails_costs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let id = new_conv(&convs);
        convs.inject(Some(Fault::SnapshotRename));
        for i in 0..(SNAPSHOT_EVERY + 2) {
            convs
                .apply(
                    &id,
                    accept(&format!("cmd_{i}"), &format!("t{i}"), &TurnId::generate()),
                )
                .unwrap()
                .unwrap();
        }
        let want = convs.get(id.as_str()).unwrap();
        assert!(!dir.path().join(id.as_str()).join("snapshot.json").exists());
        drop(convs);
        assert_eq!(
            Conversations::load(dir.path())
                .unwrap()
                .get(id.as_str())
                .unwrap(),
            want
        );
    }

    #[test]
    fn conversations_from_before_the_journal_are_migrated_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Conversation::new("claude", "legacy_cli", "ws_1".into(), Utc::now());
        c.schema = 1;
        let id = c.id.clone();
        let cdir = dir.path().join(id.as_str());
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(cdir.join("snapshot.json"), serde_json::to_vec(&c).unwrap()).unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let got = convs.get(id.as_str()).unwrap();
        assert_eq!(got.schema, CONVERSATION_SCHEMA);
        assert!(
            cdir.join("snapshot.v1.json").exists(),
            "the old one is kept"
        );
        assert_eq!(journal(dir.path(), &id).len(), 1);
        drop(convs);
        // Loaded again: not migrated again.
        let again = Conversations::load(dir.path()).unwrap();
        assert_eq!(again.get(id.as_str()).unwrap().id, id);
        assert_eq!(journal(dir.path(), &id).len(), 1);
        // An older otterd reads `snapshot.json` as a bare conversation: a
        // journaled one isn't that, so it is skipped, never overwritten.
        for i in 0..SNAPSHOT_EVERY {
            again
                .apply(&id, accept(&format!("cmd_{i}"), "t", &TurnId::generate()))
                .unwrap()
                .unwrap();
        }
        let snap = std::fs::read(cdir.join("snapshot.json")).unwrap();
        assert!(serde_json::from_slice::<Conversation>(&snap).is_err());
    }

    #[test]
    fn history_pages_through_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let id = new_conv(&convs);
        for i in 0..5 {
            convs
                .apply(&id, accept(&format!("cmd_{i}"), "t", &TurnId::generate()))
                .unwrap()
                .unwrap();
        }
        let (log, page) = convs.history(id.as_str(), 0, 2, 1 << 20).unwrap().unwrap();
        assert_eq!(page.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2]);
        let (_, rest) = convs
            .history(id.as_str(), 2, 100, 1 << 20)
            .unwrap()
            .unwrap();
        assert_eq!(rest.iter().map(|r| r.seq).collect::<Vec<_>>(), [3, 4, 5, 6]);
        assert!(rest.iter().all(|r| r.log_id == log));
        // A byte limit still returns one record.
        let (_, one) = convs.history(id.as_str(), 0, 100, 1).unwrap().unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn deleting_a_features_conversations_removes_their_records() {
        let dir = tempfile::tempdir().unwrap();
        let convs = Conversations::load(dir.path()).unwrap();
        let id = new_conv(&convs);
        assert_eq!(convs.list(Some("ft_1")).len(), 1);
        assert!(convs.list(Some("ft_2")).is_empty());
        convs.remove_for_feature(&"ft_1".into());
        assert!(convs.get(id.as_str()).is_none());
        assert!(!dir.path().join(id.as_str()).exists());
    }
}
