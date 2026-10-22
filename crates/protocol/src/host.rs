//! Host-level views: resource usage (`host.metrics`), listening ports
//! (`host.ports`), and the host's settings (`settings.get` / `settings.set`).

use otter_core::{ExecutionId, SessionId, TaskId, Timestamp, WorkspaceId};
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
    /// The coding agent's settings (absent from daemons before D-057).
    #[serde(default)]
    pub coding: CodingSettings,
    /// The controller in use now (what "automatic" chose, or the
    /// environment's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Otter answers with its tools (D-089) — on, off; absent from an otterd
    /// before it. On by default (D-096).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otter_tools: Option<bool>,
    /// `OTTER_BRAIN_V2` in otterd's environment, which overrides the setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otter_tools_from_env: Option<String>,
    /// How Otter answers on this host now: "with tools (…)", or "without
    /// tools…" and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otter_answers: Option<String>,
    /// A task's own workspace is deleted once its pull request is merged
    /// (D-189) — on, off; absent from an otterd before it. On by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_merged_workspaces: Option<bool>,
    /// Otter's own git on this host (D-133); absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitView>,
    /// Otter's tools and how far each may go on this host (D-191); empty
    /// from an otterd before it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolInfo>,
}

/// One of Otter's tools as Settings shows it (D-191).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    /// What it's for, in a line.
    pub description: String,
    /// `look`, `place`, `delegate`, `wait`, `deliver`, `remember`, `task`.
    pub group: String,
    /// Its level on this host now.
    pub level: otter_core::tools::ToolLevel,
    /// Its level when it isn't set.
    pub default_level: otter_core::tools::ToolLevel,
}

/// Otter's own git on a host (D-133): who commits in the repositories it
/// manages, the SSH key they push with, and the GitHub account its own `gh`
/// uses. The host's own git, ssh and gh are left as they are.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Otter's key on this host, once made: its public half. The private
    /// half never leaves the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_key: Option<SshKey>,
    /// The GitHub account the key signs in as, once checked. From then on
    /// Otter's repositories use the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_account: Option<String>,
    /// Who Otter's own gh is signed in as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// A browser sign-in under way, or how the last one ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<GitHubLogin>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshKey {
    /// `ssh-ed25519 AAAA… otter@host`, to add to GitHub.
    pub public: String,
    /// `SHA256:…`, as GitHub lists it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// A browser sign-in to GitHub (`gh`'s device flow).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GitHubLogin {
    /// Waiting: enter `code` at `url`.
    Pending {
        code: String,
        url: String,
    },
    Failed {
        error: String,
    },
}

/// `settings.set {git}`: who commits in Otter's repositories; `""` clears
/// (the host's own git config applies again).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl GitIdentity {
    /// The same identity, `""` and none being the same (no identity).
    pub fn same_as(&self, other: &GitIdentity) -> bool {
        let set = |v: &Option<String>| v.clone().filter(|s| !s.is_empty());
        set(&self.name) == set(&other.name) && set(&self.email) == set(&other.email)
    }
}

/// `git.key_create`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitKeyCreate {
    /// Make a new one even if there is a key (the old one's gone).
    #[serde(default)]
    pub replace: bool,
}

/// `git.github_login`: with a token, signs in with it at once; without,
/// starts a browser sign-in (its code in [`GitView::login`]).
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubSignIn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl std::fmt::Debug for GitHubSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubSignIn")
            .field("token", &self.token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// `git.check` / `git.key_add`: whether the key and gh work with GitHub.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitCheck {
    pub ssh: CheckLine,
    pub github: CheckLine,
    /// The view after the check (the key may now be in use).
    pub git: GitView,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckLine {
    pub ok: bool,
    /// What was seen, or what to do.
    pub detail: String,
    /// The GitHub account it answered as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
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
    /// The coding agent's settings (D-057); fields left out stay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding: Option<CodingSettings>,
    /// Otter answers with its tools (D-089); left out: stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otter_tools: Option<bool>,
    /// Delete a task's own workspace once its pull request is merged
    /// (D-189); left out: stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_merged_workspaces: Option<bool>,
    /// Who commits in Otter's repositories (D-133); left out: stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitIdentity>,
    /// Levels for Otter's tools (D-191), by name; `null` takes the default
    /// again. Left out: stays.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub tool_levels: std::collections::BTreeMap<String, Option<otter_core::tools::ToolLevel>>,
}

/// How the coding agent runs on this host (D-057) — apart from Otter's own
/// model above. Applies to new conversations; one keeps what it began with.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodingSettings {
    /// `legacy_cli` (Claude Code's stream-json) or `sdk` (the Claude Agent
    /// SDK worker); absent: `sdk` (D-124). In an update, `""` resets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// The Claude model for coding (absent: the provider's default). In an
    /// update, `""` resets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cleared_identity_is_none() {
        let id = |n: Option<&str>, e: Option<&str>| GitIdentity {
            name: n.map(String::from),
            email: e.map(String::from),
        };
        assert!(id(Some(""), Some("")).same_as(&id(None, None)));
        assert!(id(Some("Ada"), Some("a@b.c")).same_as(&id(Some("Ada"), Some("a@b.c"))));
        // A host that still has one isn't cleared yet.
        assert!(!id(Some(""), Some("")).same_as(&id(Some("Ada"), None)));
    }
}

/// `host.processes` (D-161): the processes Otter started or must answer for,
/// each with what owns it. Never a command line, which can carry a token.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HostProcesses {
    pub sampled_at: Timestamp,
    pub processes: Vec<OtterProcess>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OtterProcess {
    pub pid: u32,
    /// Seconds since the epoch, as the system reports it: with the pid it
    /// names the process (an id alone can be reused).
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// An otterd's `--home`: which one it is (a directory, never a secret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// Percent of one CPU, as of the last sample.
    pub cpu_percent: f32,
    pub memory: u64,
    /// How long it has used 25% of a CPU or more, in whole seconds; 0 when
    /// it isn't busy now.
    #[serde(default)]
    pub busy_secs: u32,
    pub owner: ProcessOwner,
    /// `host.processes.stop` accepts it.
    #[serde(default)]
    pub stoppable: bool,
}

/// What a process belongs to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessOwner {
    /// An otterd: the one answering (`this`), or another on the host.
    Daemon {
        #[serde(default)]
        this: bool,
    },
    /// In the process tree of a session's running execution.
    Execution {
        workspace_id: WorkspaceId,
        session_id: SessionId,
        execution_id: ExecutionId,
    },
    /// In the process tree of a coding agent's run.
    Run {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
        workspace_id: WorkspaceId,
    },
    /// Started in a workspace that is gone or archived, owned by nothing
    /// that runs.
    Orphan { why: String },
}

/// `host.processes.stop`: end another otterd, or an orphan, by
/// the pid and start time `host.processes` gave. Anything else is refused.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessStop {
    pub pid: u32,
    pub started: u64,
    /// SIGKILL instead of SIGTERM.
    #[serde(default)]
    pub force: bool,
}
