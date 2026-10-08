//! Host-level views: resource usage (`host.metrics`) and listening ports
//! (`host.ports`). Plain measurements; nothing here is persisted.

use otter_core::Timestamp;
use serde::{Deserialize, Serialize};

/// Usage now, plus recent history for charts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HostMetrics {
    pub sampled_at: Timestamp,
    /// Logical CPUs.
    pub cpus: u32,
    /// 1, 5 and 15 minute load averages.
    pub load: [f64; 3],
    pub uptime_secs: u64,
    pub memory: MemoryUsage,
    pub disks: Vec<DiskSpace>,
    /// Busiest processes right now, by CPU.
    pub top: Vec<ProcessUsage>,
    /// One sample per interval, oldest first (about the last 10 minutes).
    pub history: Vec<MetricsSample>,
    pub interval_secs: u32,
}

/// Bytes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryUsage {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskSpace {
    pub mount: String,
    pub file_system: String,
    pub total: u64,
    pub available: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProcessUsage {
    pub pid: u32,
    pub name: String,
    /// Percent of one CPU (can exceed 100 on several cores).
    pub cpu_percent: f32,
    pub memory: u64,
}

/// One point of history. Rates are per second over the preceding interval.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MetricsSample {
    pub at: Timestamp,
    /// All CPUs, 0–100.
    pub cpu_percent: f32,
    pub memory_used: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
}

/// A TCP port something listens on.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListeningPort {
    pub port: u16,
    /// Bound address (`127.0.0.1`, `0.0.0.0`, `::`, …).
    pub address: String,
    /// Process name, when the daemon may see it (its own user's processes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// Usage over a longer range (`host.history`), from what the daemon has
/// recorded: per-minute points for up to 7 days, hourly for up to 90.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HostHistory {
    /// Seconds each point covers.
    pub resolution_secs: u32,
    /// Oldest first. Missing stretches (the daemon wasn't running) are
    /// simply absent; compare `at` to find gaps.
    pub points: Vec<TrendPoint>,
}

/// One period of history: averages, and peaks where a spike matters.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrendPoint {
    /// Start of the period.
    pub at: Timestamp,
    pub cpu_avg: f32,
    pub cpu_max: f32,
    pub memory_avg: u64,
    pub memory_max: u64,
    pub load_avg: f32,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryQuery {
    /// `1h`, `24h`, `7d` or `30d`.
    pub range: String,
}
