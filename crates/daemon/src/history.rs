//! Recorded usage, for trends (`host.history`).
//!
//! The sampler's 5-second samples are folded into one point per minute
//! (averages, and peaks for CPU and memory), appended to
//! `state/metrics/minutes.jsonl` and kept for [`MINUTES_KEPT`]. Older minutes
//! are rolled up into hourly points in `hours.jsonl`, kept for
//! [`HOURS_KEPT`]. Both files are reloaded at start, so history survives
//! daemon restarts and upgrades; a stretch when the daemon wasn't running is
//! simply missing. Files are rewritten (atomically) when trimmed, so they
//! stay small: about 10k minute lines and 2k hour lines at most.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{Duration, DurationRound};
use otter_core::Timestamp;
use otter_protocol::host::{HostHistory, MetricsSample, TrendPoint};

pub const MINUTES_KEPT: Duration = Duration::days(7);
pub const HOURS_KEPT: Duration = Duration::days(90);
/// Rewrite a file once it holds this many lines beyond what is kept.
const SLACK: usize = 1440;

pub struct Recorder {
    dir: PathBuf,
    minutes: VecDeque<TrendPoint>,
    hours: VecDeque<TrendPoint>,
    minute: Option<Acc>,
    /// Minutes rolling out of the minute window, for the hour in progress.
    rolling: Option<Acc>,
    minute_lines: usize,
    hour_lines: usize,
}

/// Sums for one period.
#[derive(Clone)]
struct Acc {
    start: Timestamp,
    n: u32,
    cpu: f64,
    cpu_max: f32,
    memory: u128,
    memory_max: u64,
    load: f64,
    rx: u128,
    tx: u128,
    read: u128,
    write: u128,
}

impl Acc {
    fn new(start: Timestamp) -> Acc {
        Acc {
            start,
            n: 0,
            cpu: 0.0,
            cpu_max: 0.0,
            memory: 0,
            memory_max: 0,
            load: 0.0,
            rx: 0,
            tx: 0,
            read: 0,
            write: 0,
        }
    }

    fn add_sample(&mut self, s: &MetricsSample, load: f32) {
        self.add(&TrendPoint {
            at: s.at,
            cpu_avg: s.cpu_percent,
            cpu_max: s.cpu_percent,
            memory_avg: s.memory_used,
            memory_max: s.memory_used,
            load_avg: load,
            net_rx_bps: s.net_rx_bps,
            net_tx_bps: s.net_tx_bps,
            disk_read_bps: s.disk_read_bps,
            disk_write_bps: s.disk_write_bps,
        });
    }

    fn add(&mut self, p: &TrendPoint) {
        self.n += 1;
        self.cpu += f64::from(p.cpu_avg);
        self.cpu_max = self.cpu_max.max(p.cpu_max);
        self.memory += u128::from(p.memory_avg);
        self.memory_max = self.memory_max.max(p.memory_max);
        self.load += f64::from(p.load_avg);
        self.rx += u128::from(p.net_rx_bps);
        self.tx += u128::from(p.net_tx_bps);
        self.read += u128::from(p.disk_read_bps);
        self.write += u128::from(p.disk_write_bps);
    }

    fn point(&self) -> TrendPoint {
        let n = self.n.max(1);
        let avg = |v: u128| (v / u128::from(n)) as u64;
        TrendPoint {
            at: self.start,
            cpu_avg: (self.cpu / f64::from(n)) as f32,
            cpu_max: self.cpu_max,
            memory_avg: avg(self.memory),
            memory_max: self.memory_max,
            load_avg: (self.load / f64::from(n)) as f32,
            net_rx_bps: avg(self.rx),
            net_tx_bps: avg(self.tx),
            disk_read_bps: avg(self.read),
            disk_write_bps: avg(self.write),
        }
    }
}

fn floor(t: Timestamp, period: Duration) -> Timestamp {
    t.duration_trunc(period).unwrap_or(t)
}

fn load(path: &Path) -> VecDeque<TrendPoint> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn rewrite(path: &Path, points: &VecDeque<TrendPoint>) -> std::io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    let mut out = Vec::new();
    for p in points {
        serde_json::to_writer(&mut out, p).expect("trend point serializes");
        out.push(b'\n');
    }
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

fn append(path: &Path, p: &TrendPoint) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_vec(p).expect("trend point serializes");
    line.push(b'\n');
    f.write_all(&line)
}

impl Recorder {
    /// Load what was recorded in `dir` (created if missing).
    pub fn open(dir: PathBuf, now: Timestamp) -> Recorder {
        let _ = std::fs::create_dir_all(&dir);
        let minutes = load(&dir.join("minutes.jsonl"));
        let hours = load(&dir.join("hours.jsonl"));
        let mut r = Recorder {
            minute_lines: minutes.len(),
            hour_lines: hours.len(),
            dir,
            minutes,
            hours,
            minute: None,
            rolling: None,
        };
        r.trim(now, true);
        r
    }

    /// Take one sample (with the 1-minute load average).
    pub fn record(&mut self, s: &MetricsSample, load: f32) {
        let minute = floor(s.at, Duration::minutes(1));
        if let Some(acc) = &self.minute
            && acc.start != minute
        {
            let done = acc.point();
            if let Err(e) = append(&self.dir.join("minutes.jsonl"), &done) {
                tracing::warn!("recording usage: {e}");
            }
            self.minutes.push_back(done);
            self.minute_lines += 1;
            self.minute = None;
            self.trim(s.at, false);
        }
        self.minute
            .get_or_insert_with(|| Acc::new(minute))
            .add_sample(s, load);
    }

    /// Roll minutes older than the minute window into hours, drop hours past
    /// theirs, and rewrite files that have grown well past what is kept.
    fn trim(&mut self, now: Timestamp, force: bool) {
        let minute_cutoff = now - MINUTES_KEPT;
        let mut new_hours = false;
        while self.minutes.front().is_some_and(|p| p.at < minute_cutoff) {
            let p = self.minutes.pop_front().expect("checked");
            let hour = floor(p.at, Duration::hours(1));
            if let Some(acc) = &self.rolling
                && acc.start != hour
            {
                self.push_hour(acc.point());
                self.rolling = None;
                new_hours = true;
            }
            self.rolling.get_or_insert_with(|| Acc::new(hour)).add(&p);
        }
        let hour_cutoff = now - HOURS_KEPT;
        while self.hours.front().is_some_and(|p| p.at < hour_cutoff) {
            self.hours.pop_front();
        }
        if force || self.minute_lines > self.minutes.len() + SLACK {
            if let Err(e) = rewrite(&self.dir.join("minutes.jsonl"), &self.minutes) {
                tracing::warn!("rewriting minutes.jsonl: {e}");
            }
            self.minute_lines = self.minutes.len();
        }
        if force || new_hours && self.hour_lines > self.hours.len() + SLACK {
            if let Err(e) = rewrite(&self.dir.join("hours.jsonl"), &self.hours) {
                tracing::warn!("rewriting hours.jsonl: {e}");
            }
            self.hour_lines = self.hours.len();
        }
    }

    fn push_hour(&mut self, p: TrendPoint) {
        if let Err(e) = append(&self.dir.join("hours.jsonl"), &p) {
            tracing::warn!("recording usage: {e}");
        }
        self.hours.push_back(p);
        self.hour_lines += 1;
    }

    /// Recorded usage over `range` (`1h`, `24h`, `7d`, `30d`), at a resolution
    /// that keeps a few hundred points.
    pub fn history(&self, range: &str, now: Timestamp) -> Option<HostHistory> {
        let (span, resolution) = match range {
            "1h" => (Duration::hours(1), Duration::minutes(1)),
            "24h" => (Duration::hours(24), Duration::minutes(5)),
            "7d" => (Duration::days(7), Duration::minutes(30)),
            "30d" => (Duration::days(30), Duration::hours(1)),
            _ => return None,
        };
        let since = now - span;
        let source = self
            .hours
            .iter()
            .chain(self.minutes.iter())
            .filter(|p| p.at >= since);
        Some(HostHistory {
            resolution_secs: resolution.num_seconds() as u32,
            points: bucket(source, resolution),
        })
    }
}

/// Combine consecutive points into one per `resolution` period.
fn bucket<'a>(
    points: impl Iterator<Item = &'a TrendPoint>,
    resolution: Duration,
) -> Vec<TrendPoint> {
    let mut out = Vec::new();
    let mut acc: Option<Acc> = None;
    for p in points {
        let start = floor(p.at, resolution);
        if let Some(a) = &acc
            && a.start != start
        {
            out.push(a.point());
            acc = None;
        }
        acc.get_or_insert_with(|| Acc::new(start)).add(p);
    }
    if let Some(a) = acc {
        out.push(a.point());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn sample(t: Timestamp, cpu: f32) -> MetricsSample {
        MetricsSample {
            at: t,
            cpu_percent: cpu,
            memory_used: (cpu as u64) * 1000,
            net_rx_bps: 100,
            net_tx_bps: 10,
            disk_read_bps: 0,
            disk_write_bps: 5,
        }
    }

    #[test]
    fn minutes_average_and_peak_and_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = at("2026-10-08T10:00:00Z");
        let mut r = Recorder::open(dir.path().to_path_buf(), t0);
        // Minute 10:00: 10%, 30% → avg 20, peak 30. Minute 10:01 closes it.
        r.record(&sample(t0, 10.0), 1.0);
        r.record(&sample(t0 + Duration::seconds(30), 30.0), 3.0);
        r.record(&sample(t0 + Duration::seconds(65), 50.0), 1.0);
        let h = r.history("1h", t0 + Duration::minutes(2)).unwrap();
        assert_eq!(h.resolution_secs, 60);
        assert_eq!(h.points.len(), 1);
        let p = &h.points[0];
        assert_eq!(
            (p.cpu_avg, p.cpu_max, p.memory_max, p.load_avg),
            (20.0, 30.0, 30_000, 2.0)
        );

        // Reopening reloads what was recorded.
        drop(r);
        let r = Recorder::open(dir.path().to_path_buf(), t0 + Duration::minutes(3));
        assert_eq!(
            r.history("1h", t0 + Duration::minutes(3))
                .unwrap()
                .points
                .len(),
            1
        );
        assert!(r.history("2w", t0).is_none());
    }

    #[test]
    fn old_minutes_roll_up_into_hours_and_old_hours_expire() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = at("2026-09-01T00:00:00Z");
        let mut r = Recorder::open(dir.path().to_path_buf(), t0);
        // Two hours of minutes, a week and a half ago.
        for m in 0..=120 {
            r.record(&sample(t0 + Duration::minutes(m), m as f32 % 50.0), 1.0);
        }
        let later = t0 + Duration::days(10);
        r.trim(later, false);
        assert!(r.minutes.is_empty());
        // The first hour completed; the second is still rolling.
        assert_eq!(r.hours.len(), 1);
        assert_eq!(r.hours[0].at, t0);
        let month = r.history("30d", later).unwrap();
        assert_eq!((month.resolution_secs, month.points.len()), (3600, 1));
        // Hours expire after 90 days.
        r.trim(t0 + Duration::days(91), false);
        assert!(r.hours.is_empty());
    }

    #[test]
    fn day_view_buckets_minutes_into_five() {
        let mut out = Vec::new();
        let t0 = at("2026-10-08T10:00:00Z");
        for m in 0..10 {
            out.push(TrendPoint {
                at: t0 + Duration::minutes(m),
                cpu_avg: m as f32,
                cpu_max: m as f32,
                memory_avg: 0,
                memory_max: 0,
                load_avg: 0.0,
                net_rx_bps: 0,
                net_tx_bps: 0,
                disk_read_bps: 0,
                disk_write_bps: 0,
            });
        }
        let b = bucket(out.iter(), Duration::minutes(5));
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].cpu_avg, b[0].cpu_max), (2.0, 4.0));
        assert_eq!((b[1].cpu_avg, b[1].cpu_max), (7.0, 9.0));
    }
}
