//! Event emitter (design §20): appends structured events to the event log and
//! broadcasts them to subscribers.
//!
//! The log is bounded (D-037). Events go to `state/events.jsonl`; once that
//! passes [`Limits::segment_bytes`] it is renamed to `state/events.<seq>.jsonl`
//! (`<seq>` = its first event) and a new one is started. Only the newest
//! [`Limits::keep_segments`] rotated segments are kept. The segment names are
//! the index: replay from a cursor reads only the segments that can hold
//! events after it, and recent cursors are served from memory.
//!
//! `state/events.id` names the log. It is generated with the log and changes
//! whenever the log starts over (files removed), so a cursor from another log
//! is refused even if the new log has grown past it.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use otter_protocol::{Event, EventRecord};
use rand::Rng;
use tokio::sync::broadcast;

/// Events kept in memory for replaying recent cursors (and lagging
/// subscribers) without reading the log. At least the broadcast capacity.
const TAIL: usize = 2048;
const BROADCAST: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Rotate the active file once it reaches this size.
    pub segment_bytes: u64,
    /// Rotated segments kept besides the active file.
    pub keep_segments: usize,
}

impl Default for Limits {
    /// About 25k events per segment; at most ~16 MiB on disk.
    fn default() -> Self {
        Limits {
            segment_bytes: 4 << 20,
            keep_segments: 3,
        }
    }
}

impl Limits {
    /// The defaults, with `OTTER_EVENTS_SEGMENT_BYTES` overriding the segment
    /// size (tests use it to force rotation).
    pub fn from_env() -> Self {
        let mut limits = Limits::default();
        if let Some(n) = std::env::var("OTTER_EVENTS_SEGMENT_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|n| *n > 0)
        {
            limits.segment_bytes = n;
        }
        limits
    }
}

pub struct EventLog {
    path: PathBuf,
    log_id: String,
    limits: Limits,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<EventRecord>,
}

struct Inner {
    file: std::fs::File,
    /// The latest event's `seq`.
    seq: u64,
    /// Size of the active file.
    active_bytes: u64,
    /// The active file's first event, if it has one.
    active_first: Option<u64>,
    /// First `seq` of each rotated segment, oldest first.
    segments: Vec<u64>,
    /// The newest events, a suffix of the retained log.
    tail: VecDeque<EventRecord>,
}

impl Inner {
    /// The oldest retained event's `seq` (lower bound).
    fn oldest(&self) -> Option<u64> {
        self.segments.first().copied().or(self.active_first)
    }
}

impl EventLog {
    #[cfg(test)]
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, Limits::default())
    }

    pub fn open_with(path: &Path, limits: Limits) -> Result<Self> {
        let segments = list_segments(path)?;
        let exists = path.exists();
        let id_path = path.with_extension("id");
        // A log without files starts over under a new id; otherwise keep the
        // recorded one (generating it once for logs from older versions).
        let recorded = if exists || !segments.is_empty() {
            std::fs::read_to_string(&id_path)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        let log_id = match recorded {
            Some(id) => id,
            None => {
                let id = new_log_id();
                write_atomic(&id_path, format!("{id}\n").as_bytes())?;
                id
            }
        };

        let active = scan(path)?;
        let mut seq = active.last;
        // The active file may be empty right after a rotation.
        for first in segments.iter().rev() {
            if seq.is_some() {
                break;
            }
            seq = scan(&segment_path(path, *first))?.last;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let skip = active.records.len().saturating_sub(TAIL);
        let (tx, _) = broadcast::channel(BROADCAST);
        Ok(EventLog {
            path: path.to_path_buf(),
            log_id,
            limits,
            inner: Mutex::new(Inner {
                file,
                seq: seq.unwrap_or(0),
                active_bytes: active.bytes,
                active_first: active.first,
                segments,
                tail: active.records.into_iter().skip(skip).collect(),
            }),
            tx,
        })
    }

    /// Identifies this log; clients echo it with their cursor.
    pub fn log_id(&self) -> &str {
        &self.log_id
    }

    pub fn emit(&self, event: Event) -> EventRecord {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        let record = EventRecord {
            seq: inner.seq,
            ts: chrono::Utc::now(),
            event,
        };
        tracing::info!(seq = record.seq, kind = record.event.kind(), "event");
        let mut line = serde_json::to_vec(&record).expect("event serializes");
        line.push(b'\n');
        if inner.active_bytes > 0
            && inner.active_bytes + line.len() as u64 > self.limits.segment_bytes
        {
            self.rotate(&mut inner);
        }
        match inner.file.write_all(&line) {
            Ok(()) => {
                inner.active_bytes += line.len() as u64;
                inner.active_first.get_or_insert(record.seq);
            }
            Err(e) => tracing::error!("failed to append event: {e}"),
        }
        inner.tail.push_back(record.clone());
        if inner.tail.len() > TAIL {
            inner.tail.pop_front();
        }
        // No subscribers is fine.
        let _ = self.tx.send(record.clone());
        record
    }

    /// Move the active file aside as a segment, start a new one and drop the
    /// oldest segments beyond the limit.
    fn rotate(&self, inner: &mut Inner) {
        let first = inner.active_first.unwrap_or(inner.seq);
        let segment = segment_path(&self.path, first);
        if let Err(e) = std::fs::rename(&self.path, &segment) {
            tracing::error!("rotating {}: {e}", self.path.display());
            return;
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => inner.file = file,
            // Keep appending to the renamed file; it's still the newest.
            Err(e) => {
                tracing::error!("opening {}: {e}", self.path.display());
                let _ = std::fs::rename(&segment, &self.path);
                return;
            }
        }
        inner.segments.push(first);
        inner.active_bytes = 0;
        inner.active_first = None;
        while inner.segments.len() > self.limits.keep_segments {
            let oldest = inner.segments.remove(0);
            if let Err(e) = std::fs::remove_file(segment_path(&self.path, oldest)) {
                tracing::warn!("removing event segment {oldest}: {e}");
            }
        }
        // Serve from memory only what is still retained on disk, so a cursor
        // is accepted or refused the same way before and after a restart.
        if let Some(oldest) = inner.oldest() {
            inner.tail.retain(|r| r.seq >= oldest);
        }
        tracing::info!(first, "rotated the event log");
    }

    /// The latest event's `seq`, and a receiver for exactly the events after it.
    pub fn subscribe(&self) -> (u64, broadcast::Receiver<EventRecord>) {
        // Under the lock, so no event falls between the two.
        let inner = self.inner.lock().unwrap();
        (inner.seq, self.tx.subscribe())
    }

    /// The latest event's `seq`.
    pub fn head(&self) -> u64 {
        self.inner.lock().unwrap().seq
    }

    /// Logged events with `seq > after`, oldest first; `None` if the log can't
    /// serve that cursor (it predates the oldest retained event, or is ahead of
    /// the latest one because the log was reset). Recent cursors come from
    /// memory; older ones read only the segments from the cursor on.
    pub fn since(&self, after: u64) -> Result<Option<Vec<EventRecord>>> {
        // Held while reading so rotation can't move files underneath.
        let inner = self.inner.lock().unwrap();
        if after > inner.seq {
            return Ok(None);
        }
        if after == inner.seq {
            return Ok(Some(Vec::new()));
        }
        if inner.tail.front().is_some_and(|r| r.seq <= after + 1) {
            return Ok(Some(
                inner
                    .tail
                    .iter()
                    .filter(|r| r.seq > after)
                    .cloned()
                    .collect(),
            ));
        }
        if inner.oldest().is_none_or(|oldest| oldest > after + 1) {
            return Ok(None);
        }
        // The last segment starting at or before the first wanted event, and
        // everything after it.
        let from = inner
            .segments
            .iter()
            .rposition(|first| *first <= after + 1)
            .unwrap_or(0);
        let mut out = Vec::new();
        for first in &inner.segments[from..] {
            let records = scan(&segment_path(&self.path, *first))?.records;
            out.extend(records.into_iter().filter(|r| r.seq > after));
        }
        out.extend(
            scan(&self.path)?
                .records
                .into_iter()
                .filter(|r| r.seq > after),
        );
        Ok(Some(out))
    }

    /// The most recent `limit` events, oldest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<EventRecord>> {
        let inner = self.inner.lock().unwrap();
        if inner.tail.len() >= limit {
            let skip = inner.tail.len() - limit;
            return Ok(inner.tail.iter().skip(skip).cloned().collect());
        }
        // Newest file first, until there are enough.
        let mut files = vec![self.path.clone()];
        files.extend(
            inner
                .segments
                .iter()
                .rev()
                .map(|first| segment_path(&self.path, *first)),
        );
        let mut out: Vec<EventRecord> = Vec::new();
        for file in files {
            let mut records = scan(&file)?.records;
            records.append(&mut out);
            out = records;
            if out.len() >= limit {
                break;
            }
        }
        let skip = out.len().saturating_sub(limit);
        Ok(out.split_off(skip))
    }
}

fn new_log_id() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut rng = rand::rng();
    let suffix: String = (0..12)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    format!("log_{suffix}")
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// `events.jsonl` → `events.<first>.jsonl`.
fn segment_path(active: &Path, first: u64) -> PathBuf {
    active.with_extension(format!("{first}.jsonl"))
}

/// First `seq` of each rotated segment next to `active`, ascending.
fn list_segments(active: &Path) -> Result<Vec<u64>> {
    let (Some(dir), Some(stem)) = (active.parent(), active.file_stem().and_then(|s| s.to_str()))
    else {
        return Ok(Vec::new());
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let prefix = format!("{stem}.");
    let mut out: Vec<u64> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.strip_prefix(&prefix)?
                .strip_suffix(".jsonl")?
                .parse()
                .ok()
        })
        .collect();
    out.sort_unstable();
    Ok(out)
}

#[derive(Default)]
struct Scan {
    records: Vec<EventRecord>,
    /// Highest and first `seq`, counting lines that only parse as far as
    /// `seq`.
    last: Option<u64>,
    first: Option<u64>,
    bytes: u64,
}

fn scan(path: &Path) -> Result<Scan> {
    #[derive(serde::Deserialize)]
    struct Seq {
        seq: u64,
    }
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Scan::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut out = Scan::default();
    for line in std::io::BufReader::new(file).split(b'\n') {
        let line = line?;
        out.bytes += line.len() as u64 + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // Skip lines a future/older version can't parse instead of failing.
        let seq = match serde_json::from_slice::<EventRecord>(&line) {
            Ok(rec) => {
                let seq = rec.seq;
                out.records.push(rec);
                seq
            }
            Err(e) => {
                tracing::debug!("skipping unreadable event line: {e}");
                match serde_json::from_slice::<Seq>(&line) {
                    Ok(s) => s.seq,
                    Err(_) => continue,
                }
            }
        };
        out.first.get_or_insert(seq);
        out.last = out.last.max(Some(seq));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(log: &EventLog) -> EventRecord {
        log.emit(Event::DaemonStarted {
            version: "test".into(),
        })
    }

    fn seqs(log: &EventLog, after: u64) -> Option<Vec<u64>> {
        log.since(after)
            .unwrap()
            .map(|v| v.iter().map(|r| r.seq).collect())
    }

    /// Small segments: each `started` line is ~90 bytes, so three per segment.
    const SMALL: Limits = Limits {
        segment_bytes: 300,
        keep_segments: 2,
    };

    #[test]
    fn since_replays_after_the_cursor_and_rejects_unknown_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open(&path).unwrap();
        assert_eq!(log.since(0).unwrap(), Some(vec![]));
        for _ in 0..3 {
            started(&log);
        }
        assert_eq!(seqs(&log, 0), Some(vec![1, 2, 3]));
        assert_eq!(seqs(&log, 2), Some(vec![3]));
        assert_eq!(seqs(&log, 3), Some(vec![]));
        assert_eq!(seqs(&log, 4), None, "cursor ahead of the log");

        // Sequence numbers and the log id continue across reopening.
        let id = log.log_id().to_owned();
        drop(log);
        let log = EventLog::open(&path).unwrap();
        assert_eq!(log.log_id(), id);
        assert_eq!(started(&log).seq, 4);
    }

    #[test]
    fn since_rejects_cursors_older_than_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        // As if events 1..=9 had been rotated away.
        std::fs::write(
            &path,
            "{\"seq\":10,\"ts\":\"2026-10-07T00:00:00Z\",\"type\":\"DaemonStarted\",\"version\":\"x\"}\n",
        )
        .unwrap();
        let log = EventLog::open(&path).unwrap();
        started(&log);
        assert_eq!(log.since(5).unwrap(), None);
        assert_eq!(log.since(9).unwrap().unwrap().len(), 2);
    }

    #[test]
    fn rotation_bounds_the_log_and_replay_crosses_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open_with(&path, SMALL).unwrap();
        for _ in 0..20 {
            started(&log);
        }
        // Three per segment: 19..=20 active, two segments kept (13.., 16..).
        assert_eq!(list_segments(&path).unwrap(), vec![13, 16]);
        let total: u64 = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= 3 * SMALL.segment_bytes + 64, "{total} bytes");

        // Replay boundaries: from just before the oldest retained event, and
        // across a segment boundary; anything older is refused.
        assert_eq!(seqs(&log, 12), Some((13..=20).collect()));
        assert_eq!(seqs(&log, 15), Some((16..=20).collect()));
        assert_eq!(seqs(&log, 11), None, "rotated away");
        assert_eq!(seqs(&log, 0), None, "rotated away");
        assert_eq!(
            log.recent(5)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            (16..=20).collect::<Vec<_>>()
        );

        // The same answers from disk after reopening (no in-memory tail for
        // the rotated segments), and the sequence continues.
        drop(log);
        let log = EventLog::open_with(&path, SMALL).unwrap();
        assert_eq!(seqs(&log, 12), Some((13..=20).collect()));
        assert_eq!(seqs(&log, 15), Some((16..=20).collect()));
        assert_eq!(seqs(&log, 11), None);
        assert_eq!(
            log.recent(100)
                .unwrap()
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>(),
            (13..=20).collect::<Vec<_>>()
        );
        assert_eq!(started(&log).seq, 21);
    }

    #[test]
    fn head_survives_a_rotation_without_a_new_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open_with(&path, SMALL).unwrap();
        for _ in 0..4 {
            started(&log);
        }
        drop(log);
        // As if the daemon stopped right after rotating: empty active file.
        std::fs::rename(&path, segment_path(&path, 4)).unwrap();
        let log = EventLog::open_with(&path, SMALL).unwrap();
        assert_eq!(log.head(), 4);
        assert_eq!(seqs(&log, 2), Some(vec![3, 4]));
        assert_eq!(started(&log).seq, 5);
    }

    #[test]
    fn a_log_that_starts_over_gets_a_new_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open_with(&path, SMALL).unwrap();
        for _ in 0..8 {
            started(&log);
        }
        let id = log.log_id().to_owned();
        assert!(id.starts_with("log_"), "{id}");
        drop(log);

        // The log files go away (the id file stays): a new log, a new id.
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "jsonl") {
                std::fs::remove_file(p).unwrap();
            }
        }
        let log = EventLog::open_with(&path, SMALL).unwrap();
        assert_ne!(log.log_id(), id);
        assert_eq!(log.head(), 0);

        // A log from an older version (no id file) gets one, once.
        drop(log);
        std::fs::remove_file(path.with_extension("id")).unwrap();
        let log = EventLog::open(&path).unwrap();
        let upgraded = log.log_id().to_owned();
        drop(log);
        assert_eq!(EventLog::open(&path).unwrap().log_id(), upgraded);
    }

    #[test]
    fn subscribe_hands_over_exactly_after_head() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::open(&dir.path().join("events.jsonl")).unwrap();
        started(&log);
        let (head, mut rx) = log.subscribe();
        assert_eq!(head, 1);
        started(&log);
        assert_eq!(rx.try_recv().unwrap().seq, 2);
    }
}
