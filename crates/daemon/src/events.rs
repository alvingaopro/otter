//! Event emitter (design §20): appends structured events to
//! `state/events.jsonl` and broadcasts them to subscribers.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use tokio::sync::broadcast;
use workd_protocol::{Event, EventRecord};

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

    pub fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.tx.subscribe()
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
