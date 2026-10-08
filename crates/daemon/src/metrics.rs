//! Host resource usage for the host page (`host.metrics`), and the TCP ports
//! listening on the host (`host.ports`).
//!
//! A sampler measures every [`INTERVAL`] and keeps [`HISTORY`] samples, so a
//! client opening the page sees the last minutes at once. Measurements come
//! from `sysinfo` (Linux and macOS); nothing is persisted.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use otter_protocol::host::{
    DiskSpace, HostMetrics, ListeningPort, MemoryUsage, MetricsSample, ProcessUsage,
};
use sysinfo::{Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System};

pub const INTERVAL: Duration = Duration::from_secs(5);
/// Ten minutes of samples.
const HISTORY: usize = 120;
const TOP: usize = 8;

#[derive(Clone, Default)]
pub struct Sampler {
    latest: Arc<Mutex<Option<HostMetrics>>>,
}

struct Probe {
    sys: System,
    disks: Disks,
    networks: Networks,
    history: VecDeque<MetricsSample>,
}

impl Sampler {
    /// Start sampling in the background.
    pub fn start() -> Sampler {
        let sampler = Sampler::default();
        let latest = sampler.latest.clone();
        std::thread::Builder::new()
            .name("metrics".into())
            .spawn(move || {
                let mut probe = Probe {
                    sys: System::new(),
                    disks: Disks::new_with_refreshed_list(),
                    networks: Networks::new_with_refreshed_list(),
                    history: VecDeque::with_capacity(HISTORY),
                };
                // CPU usage and rates need a previous measurement.
                probe.refresh();
                loop {
                    std::thread::sleep(INTERVAL);
                    let metrics = probe.sample();
                    *latest.lock().unwrap() = Some(metrics);
                }
            })
            .expect("spawning the metrics thread");
        sampler
    }

    /// The latest measurement; `None` until the first interval has passed.
    pub fn latest(&self) -> Option<HostMetrics> {
        self.latest.lock().unwrap().clone()
    }
}

impl Probe {
    fn refresh(&mut self) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        self.disks.refresh(true);
        self.networks.refresh(true);
    }

    fn sample(&mut self) -> HostMetrics {
        self.refresh();
        let secs = INTERVAL.as_secs().max(1);
        let (mut rx, mut tx) = (0u64, 0u64);
        for (name, data) in self.networks.list() {
            if name.starts_with("lo") {
                continue;
            }
            rx += data.received();
            tx += data.transmitted();
        }
        let (mut read, mut written) = (0u64, 0u64);
        for disk in self.disks.list() {
            let usage = disk.usage();
            read += usage.read_bytes;
            written += usage.written_bytes;
        }
        let now = chrono::Utc::now();
        let used = self.sys.used_memory();
        self.history.push_back(MetricsSample {
            at: now,
            cpu_percent: self.sys.global_cpu_usage(),
            memory_used: used,
            net_rx_bps: rx / secs,
            net_tx_bps: tx / secs,
            disk_read_bps: read / secs,
            disk_write_bps: written / secs,
        });
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }

        let mut top: Vec<ProcessUsage> = self
            .sys
            .processes()
            .values()
            .map(|p| ProcessUsage {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu_percent: p.cpu_usage(),
                memory: p.memory(),
            })
            .collect();
        top.sort_by(|a, b| {
            b.cpu_percent
                .total_cmp(&a.cpu_percent)
                .then(b.memory.cmp(&a.memory))
        });
        top.truncate(TOP);

        let load = System::load_average();
        HostMetrics {
            sampled_at: now,
            cpus: self.sys.cpus().len() as u32,
            load: [load.one, load.five, load.fifteen],
            uptime_secs: System::uptime(),
            memory: MemoryUsage {
                total: self.sys.total_memory(),
                used,
                available: self.sys.available_memory(),
                swap_total: self.sys.total_swap(),
                swap_used: self.sys.used_swap(),
            },
            disks: disks(&self.disks),
            top,
            history: self.history.iter().cloned().collect(),
            interval_secs: secs as u32,
        }
    }
}

/// Filesystems worth showing: real mounts, each storage pool once.
fn disks(disks: &Disks) -> Vec<DiskSpace> {
    let mut out: Vec<DiskSpace> = Vec::new();
    for d in disks.list() {
        let mount = d.mount_point().to_string_lossy().into_owned();
        let pseudo = [
            "/boot",
            "/snap",
            "/run",
            "/dev",
            "/proc",
            "/sys",
            "/var/lib/docker",
        ];
        if d.total_space() == 0
            || pseudo.iter().any(|p| mount == *p || mount.starts_with(&format!("{p}/")))
            // macOS: the system volumes share the data volume's container.
            || (mount.starts_with("/System/Volumes/") && mount != "/System/Volumes/Data")
        {
            continue;
        }
        // APFS volumes (and bind mounts) of one pool report the same numbers.
        if out
            .iter()
            .any(|o| o.total == d.total_space() && o.available == d.available_space())
        {
            continue;
        }
        out.push(DiskSpace {
            mount,
            file_system: d.file_system().to_string_lossy().into_owned(),
            total: d.total_space(),
            available: d.available_space(),
        });
    }
    out.sort_by(|a, b| a.mount.cmp(&b.mount));
    out
}

// ---------------------------------------------------------------------------
// Listening ports
// ---------------------------------------------------------------------------

/// TCP ports listening on this host, one entry per port (IPv4 and IPv6
/// listeners merged), sorted by port.
pub async fn listening_ports() -> Vec<ListeningPort> {
    let found = if cfg!(target_os = "linux") {
        run("ss", &["-ltnpH"]).await.map(|o| parse_ss(&o))
    } else {
        run("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"])
            .await
            .map(|o| parse_lsof(&o))
    };
    let mut by_port: HashMap<u16, ListeningPort> = HashMap::new();
    for p in found.unwrap_or_default() {
        let entry = by_port.entry(p.port).or_insert_with(|| p.clone());
        // Prefer the widest bind and any known process.
        if entry.process.is_none() && p.process.is_some() {
            entry.process = p.process.clone();
            entry.pid = p.pid;
        }
        if matches!(p.address.as_str(), "0.0.0.0" | "::" | "*") {
            entry.address = p.address.clone();
        }
    }
    let mut ports: Vec<ListeningPort> = by_port.into_values().collect();
    ports.sort_by_key(|p| p.port);
    ports
}

async fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Split `addr:port`, `[v6]:port` or `*:port`.
fn split_addr(s: &str) -> Option<(String, u16)> {
    let (addr, port) = s.rsplit_once(':')?;
    let addr = addr.trim_start_matches('[').trim_end_matches(']');
    let addr = addr.split('%').next().unwrap_or(addr);
    Some((addr.to_owned(), port.parse().ok()?))
}

/// `ss -ltnpH`: `LISTEN 0 4096 127.0.0.1:5173 0.0.0.0:* users:(("node",pid=12,fd=20))`.
fn parse_ss(out: &str) -> Vec<ListeningPort> {
    out.lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let (address, port) = split_addr(cols.get(3)?)?;
            let users = cols.get(5).copied().unwrap_or("");
            let process = users
                .split("((\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .map(str::to_owned);
            let pid = users
                .split("pid=")
                .nth(1)
                .and_then(|s| s.split([',', ')']).next())
                .and_then(|s| s.parse().ok());
            Some(ListeningPort {
                port,
                address,
                process,
                pid,
            })
        })
        .collect()
}

/// `lsof -F pcn`: `p<pid>` / `c<command>` / `n<addr:port>` field lines.
fn parse_lsof(out: &str) -> Vec<ListeningPort> {
    let mut ports = Vec::new();
    let (mut pid, mut command) = (None, None);
    for line in out.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().ok(),
            "c" => command = Some(value.to_owned()),
            "n" => {
                if let Some((address, port)) = split_addr(value) {
                    ports.push(ListeningPort {
                        port,
                        address,
                        process: command.clone(),
                        pid,
                    });
                }
            }
            _ => {}
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ss() {
        let out = "LISTEN 0 4096 127.0.0.1:5173 0.0.0.0:* users:((\"node\",pid=1234,fd=20))\n\
                   LISTEN 0 128 [::]:22 [::]:*\n\
                   LISTEN 0 511 [fe80::1%eth0]:8080 [::]:* users:((\"python3\",pid=7,fd=3))\n";
        let p = parse_ss(out);
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].port, p[0].address.as_str()), (5173, "127.0.0.1"));
        assert_eq!(p[0].process.as_deref(), Some("node"));
        assert_eq!(p[0].pid, Some(1234));
        assert_eq!(
            (p[1].port, p[1].address.as_str(), p[1].process.as_deref()),
            (22, "::", None)
        );
        assert_eq!((p[2].address.as_str(), p[2].pid), ("fe80::1", Some(7)));
    }

    #[test]
    fn parses_lsof() {
        let out = "p501\ncnode\nn127.0.0.1:3000\nn[::1]:3000\np77\ncpostgres\nn*:5432\n";
        let p = parse_lsof(out);
        assert_eq!(p.len(), 3);
        assert_eq!(
            (p[0].port, p[0].process.as_deref(), p[0].pid),
            (3000, Some("node"), Some(501))
        );
        assert_eq!((p[1].address.as_str(), p[1].port), ("::1", 3000));
        assert_eq!(
            (p[2].address.as_str(), p[2].port, p[2].process.as_deref()),
            ("*", 5432, Some("postgres"))
        );
    }

    #[test]
    fn samples_this_host() {
        let mut probe = Probe {
            sys: System::new(),
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            history: VecDeque::new(),
        };
        probe.refresh();
        let m = probe.sample();
        assert!(m.cpus > 0 && m.memory.total > 0, "{m:?}");
        assert!(!m.disks.is_empty(), "{:?}", m.disks);
        assert_eq!(m.history.len(), 1);
    }
}
