//! Host resource usage for the host page (`host.metrics`), and the TCP ports
//! listening on the host (`host.ports`).
//!
//! A sampler measures every [`INTERVAL`] and keeps [`HISTORY`] samples, so a
//! client opening the page sees the last minutes at once, and hands every
//! sample to the [`crate::history::Recorder`] for trends. Measurements come
//! from `sysinfo` (Linux and macOS).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use otter_protocol::host::{
    DiskSpace, HostHistory, HostMetrics, ListeningPort, MemoryUsage, MetricsSample, ProcessUsage,
};

use crate::history::Recorder;
use sysinfo::{Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub const INTERVAL: Duration = Duration::from_secs(5);
/// Ten minutes of samples.
const HISTORY: usize = 120;
const TOP: usize = 8;

#[derive(Clone)]
pub struct Sampler {
    latest: Arc<Mutex<Option<HostMetrics>>>,
    processes: Arc<Mutex<Vec<ProcSample>>>,
    recorder: Arc<Mutex<Recorder>>,
}

/// One process as of the last sample (D-161). Never its command line: only
/// what the sampler itself reads from it, like whether it is an otterd.
#[derive(Clone, Debug)]
pub struct ProcSample {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub exe: Option<String>,
    pub cwd: Option<String>,
    pub cpu: f32,
    pub memory: u64,
    /// Seconds since the epoch.
    pub started: u64,
    /// An otterd serving (`otterd serve`).
    pub otterd: bool,
    /// An otterd's `--home` (a directory, not a secret): which it is.
    pub home: Option<String>,
    /// Samples in a row at 25% of a CPU or more.
    pub busy_samples: u32,
}

/// The `--home` an otterd was started with, as `--home dir` or `--home=dir`.
pub(crate) fn home_of(cmd: &[std::ffi::OsString]) -> Option<String> {
    let mut args = cmd.iter();
    while let Some(a) = args.next() {
        if a == "--home" {
            return args.next().map(|h| h.to_string_lossy().into_owned());
        }
        if let Some(h) = a.to_str().and_then(|a| a.strip_prefix("--home=")) {
            return Some(h.to_owned());
        }
    }
    None
}

/// An otterd serving: its executable (or what it was started as) is
/// `otterd` and it was asked to `serve`. Not one of its own sessions on the
/// way to exec (`otterd internal-exec … -- argv`), whose argv can say `serve`.
pub(crate) fn otterd_serving(exe: Option<&std::path::Path>, cmd: &[std::ffi::OsString]) -> bool {
    let named = |p: &std::path::Path| p.file_name().is_some_and(|f| f == "otterd");
    (exe.is_some_and(named) || cmd.first().is_some_and(|c| named(std::path::Path::new(c))))
        && cmd.iter().any(|a| a == "serve")
        && !cmd.iter().any(|a| a == "internal-exec")
}

fn sample_of(p: &sysinfo::Process, busy_samples: u32) -> ProcSample {
    ProcSample {
        pid: p.pid().as_u32(),
        parent: p.parent().map(|p| p.as_u32()),
        // The executable's name when there is one: sysinfo names a process
        // once, as it was when first seen (a session's shell began as an
        // `otterd internal-exec`).
        name: p
            .exe()
            .and_then(|e| e.file_name())
            .unwrap_or_else(|| p.name())
            .to_string_lossy()
            .into_owned(),
        exe: p.exe().map(|e| e.to_string_lossy().into_owned()),
        cwd: p.cwd().map(|c| c.to_string_lossy().into_owned()),
        cpu: p.cpu_usage(),
        memory: p.memory(),
        started: p.start_time(),
        otterd: otterd_serving(p.exe(), p.cmd()),
        home: otterd_serving(p.exe(), p.cmd())
            .then(|| home_of(p.cmd()))
            .flatten(),
        busy_samples,
    }
}

/// One process, read now: what a stop is decided on, not the last sample.
pub fn fresh(pid: u32) -> Option<ProcSample> {
    let mut sys = System::new();
    let target = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[target]),
        true,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_exe(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    sys.process(target)
        .filter(|p| p.thread_kind().is_none() && !is_thread(pid))
        .map(|p| sample_of(p, 0))
}

/// Whether `pid` is a thread of another process (Linux: its thread group
/// isn't itself). A signal to a thread's id reaches its whole process, which
/// may be this daemon.
pub fn is_thread(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("Tgid:")
                    .and_then(|t| t.trim().parse::<u32>().ok())
            })
        })
        .is_some_and(|tgid| tgid != pid)
}

/// What counts as busy.
const BUSY_CPU: f32 = 25.0;

struct Probe {
    busy: HashMap<(u32, u64), u32>,
    sys: System,
    disks: Disks,
    networks: Networks,
    history: VecDeque<MetricsSample>,
}

impl Sampler {
    /// Start sampling in the background, recording into `history_dir`.
    pub fn start(history_dir: std::path::PathBuf) -> Sampler {
        let sampler = Sampler {
            latest: Arc::default(),
            processes: Arc::default(),
            recorder: Arc::new(Mutex::new(Recorder::open(history_dir, chrono::Utc::now()))),
        };
        let latest = sampler.latest.clone();
        let processes = sampler.processes.clone();
        let recorder = sampler.recorder.clone();
        std::thread::Builder::new()
            .name("metrics".into())
            .spawn(move || {
                let mut probe = Probe {
                    busy: HashMap::new(),
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
                    if let Some(last) = metrics.history.last() {
                        recorder
                            .lock()
                            .unwrap()
                            .record(last, metrics.load[0] as f32);
                    }
                    *latest.lock().unwrap() = Some(metrics);
                    *processes.lock().unwrap() = probe.processes();
                }
            })
            .expect("spawning the metrics thread");
        sampler
    }

    /// Recorded usage over a range (`1h`, `24h`, `7d`, `30d`).
    pub fn history(&self, range: &str) -> Option<HostHistory> {
        self.recorder
            .lock()
            .unwrap()
            .history(range, chrono::Utc::now())
    }

    /// Every process as of the last sample; empty until the first interval
    /// has passed.
    pub fn processes(&self) -> Vec<ProcSample> {
        self.processes.lock().unwrap().clone()
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
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                // Read again each time: a process execs and changes its
                // directory, and a name read once is the name it had then.
                .with_exe(UpdateKind::Always)
                .with_cwd(UpdateKind::Always)
                .with_cmd(UpdateKind::Always),
        );
        self.disks.refresh(true);
        self.networks.refresh(true);
    }

    /// The processes now, with how long each has been busy.
    fn processes(&mut self) -> Vec<ProcSample> {
        let mut busy = HashMap::new();
        let out: Vec<ProcSample> = self
            .sys
            .processes()
            .values()
            // On Linux sysinfo lists every thread as a process of its own,
            // with the same executable and arguments: an otterd's threads
            // would each be "another otterd". Only processes.
            .filter(|p| p.thread_kind().is_none())
            .map(|p| {
                let (pid, started) = (p.pid().as_u32(), p.start_time());
                let n = match p.cpu_usage() >= BUSY_CPU {
                    true => self.busy.get(&(pid, started)).copied().unwrap_or(0) + 1,
                    false => 0,
                };
                if n > 0 {
                    busy.insert((pid, started), n);
                }
                sample_of(p, n)
            })
            .collect();
        self.busy = busy;
        out
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

/// Each process's parent, to tell which session a listening port belongs to
/// (D-104). Read on request only, never in the reconcile loop.
pub fn parent_pids() -> HashMap<u32, u32> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    sys.processes()
        .iter()
        .filter_map(|(pid, p)| Some((pid.as_u32(), p.parent()?.as_u32())))
        .collect()
}

/// The session whose process tree `pid` is in: walk up its parents until a
/// session's process (`roots`), at most a few dozen levels.
pub fn owner_of<T: Clone>(
    pid: u32,
    roots: &HashMap<u32, T>,
    parents: &HashMap<u32, u32>,
) -> Option<T> {
    let mut p = pid;
    for _ in 0..64 {
        if let Some(owner) = roots.get(&p) {
            return Some(owner.clone());
        }
        p = *parents.get(&p)?;
        if p <= 1 {
            return None;
        }
    }
    None
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
    fn an_otterd_is_told_by_its_executable_and_serve_not_by_a_sessions_argv() {
        use std::ffi::OsString;
        use std::path::Path;
        let cmd = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        let otterd = Some(Path::new("/home/u/.otter/bin/otterd"));
        assert!(otterd_serving(otterd, &cmd(&["otterd", "serve"])));
        // A session on its way to exec, whose argv says serve.
        assert!(!otterd_serving(
            otterd,
            &cmd(&["otterd", "internal-exec", "/tmp/env", "--", "hugo", "serve"])
        ));
        // After the exec: not an otterd at all.
        assert!(!otterd_serving(
            Some(Path::new("/usr/bin/hugo")),
            &cmd(&["hugo", "serve"])
        ));
        // An otterd that is not serving (a dial, a doctor).
        assert!(!otterd_serving(otterd, &cmd(&["otterd", "dial"])));
        // The executable unreadable: what it was started as.
        assert!(otterd_serving(None, &cmd(&["/x/otterd", "serve"])));
    }

    #[test]
    fn an_otterds_home_is_read_from_its_arguments() {
        use std::ffi::OsString;
        let cmd = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            home_of(&cmd(&["otterd", "--home", "/h/x", "serve"])).as_deref(),
            Some("/h/x")
        );
        assert_eq!(
            home_of(&cmd(&["otterd", "--home=/h/y", "serve"])).as_deref(),
            Some("/h/y")
        );
        assert_eq!(home_of(&cmd(&["otterd", "serve"])), None);
        assert_eq!(home_of(&cmd(&["otterd", "serve", "--home"])), None);
    }

    #[test]
    fn samples_this_host() {
        let mut probe = Probe {
            busy: HashMap::new(),
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
