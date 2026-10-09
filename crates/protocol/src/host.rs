//! Host-level views: resource usage (`host.metrics`), listening ports
//! (`host.ports`), and the host's settings (`settings.get` / `settings.set`).

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

/// A host's settings as a client sees them (`settings.get`, D-048): secrets
/// only say whether they are set.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// One of `controllers`; absent: automatic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `OTTER_CONTROLLER` in otterd's environment, which overrides the
    /// setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_from_env: Option<String>,
    pub secrets: Vec<SecretState>,
    /// What the Control Agent can be (D-052). Empty from an older otterd.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controllers: Vec<ControllerInfo>,
    /// The controller in use now (what "automatic" chose, or the
    /// environment's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
}

/// One choice of Control Agent: a model provider, Claude Code, or none.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ControllerInfo {
    pub id: String,
    pub label: String,
    /// The secret holding its API key, if it takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Whether it asks a model (and so has models to choose from).
    pub models: bool,
    /// The model used when none is chosen; absent: one must be chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

/// `settings.models`: the models a controller offers.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelsQuery {
    pub controller: String,
}

/// One model a provider offers (`settings.models`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    /// What to put in Settings' model.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SecretState {
    pub name: String,
    /// What it is used for.
    pub purpose: String,
    pub set: bool,
}

/// `settings.set`: fields left out stay as they are. `controller` or
/// `model` `""` resets to automatic / the default; a secret `null` clears it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SettingsUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub secrets: std::collections::BTreeMap<String, Option<String>>,
}
