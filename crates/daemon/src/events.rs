//! Event emitter (design §20): appends structured events to
//! `state/events.jsonl` and broadcasts them to subscribers.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use otter_protocol::{Event, EventRecord};
use tokio::sync::broadcast;

pub struct EventLog {
    path: PathBuf,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<EventRecord>,
}

struct Inner {
    file: std::fs::File,
    seq: u64,
}

impl EventLog {
    pub fn open(path: &Path) -> Result<Self> {
        let seq = last_seq(path)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let (tx, _) = broadcast::channel(1024);
        Ok(EventLog {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner { file, seq }),
            tx,
        })
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
        if let Err(e) = inner.file.write_all(&line) {
            tracing::error!("failed to append event: {e}");
        }
        // No subscribers is fine.
        let _ = self.tx.send(record.clone());
        record
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
    /// the latest one because the log was reset). Reads the whole log, so it is
    /// for reconnects and catching up, not per event.
    pub fn since(&self, after: u64) -> Result<Option<Vec<EventRecord>>> {
        let head = self.head();
        if after > head {
            return Ok(None);
        }
        if after == head {
            return Ok(Some(Vec::new()));
        }
        let records = read_records(&self.path)?;
        if records.first().is_none_or(|r| r.seq > after + 1) {
            return Ok(None);
        }
        Ok(Some(
            records.into_iter().filter(|r| r.seq > after).collect(),
        ))
    }

    /// The most recent `limit` events, oldest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<EventRecord>> {
        let mut records = read_records(&self.path)?;
        let skip = records.len().saturating_sub(limit);
        Ok(records.split_off(skip))
    }
}

/// Highest sequence number in the log, reading only the `seq` field so lines
/// with unknown event types still count.
fn last_seq(path: &Path) -> Result<u64> {
    #[derive(serde::Deserialize)]
    struct Seq {
        seq: u64,
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str::<Seq>(l).ok())
        .map(|s| s.seq)
        .max()
        .unwrap_or(0))
}

fn read_records(path: &Path) -> Result<Vec<EventRecord>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut out = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Skip lines a future/older version can't parse instead of failing.
        match serde_json::from_str(&line) {
            Ok(rec) => out.push(rec),
            Err(e) => tracing::debug!("skipping unreadable event line: {e}"),
        }
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

    #[test]
    fn since_replays_after_the_cursor_and_rejects_unknown_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open(&path).unwrap();
        assert_eq!(log.since(0).unwrap(), Some(vec![]));
        for _ in 0..3 {
            started(&log);
        }
        let seqs = |after| {
            log.since(after)
                .unwrap()
                .map(|v| v.iter().map(|r| r.seq).collect::<Vec<_>>())
        };
        assert_eq!(seqs(0), Some(vec![1, 2, 3]));
        assert_eq!(seqs(2), Some(vec![3]));
        assert_eq!(seqs(3), Some(vec![]));
        assert_eq!(seqs(4), None, "cursor ahead of the log");

        // Sequence numbers continue across reopening.
        drop(log);
        let log = EventLog::open(&path).unwrap();
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
