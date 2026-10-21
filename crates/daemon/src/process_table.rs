//! The processes Otter started or must answer for (`host.processes`, D-161),
//! and ending the ones that nothing owns (`host.processes.stop`).
//!
//! Ownership is found as `workspace.ports` finds it: by walking each process's
//! parents up to a session's running execution or a coding agent's run. What
//! is left is Otter's only by two conservative rules: it is an `otterd` that is
//! not this one, or it works in a workspace directory of this host whose
//! workspace is gone or archived. A command line is never read out: it can
//! carry a token.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use otter_core::{ExecutionId, SessionId, TaskId, WorkspaceId, WorkspaceState};
use otter_protocol::host::{HostProcesses, OtterProcess, ProcessOwner, ProcessStop};
use otter_protocol::{ErrorCode, RpcError};

use crate::daemon::{Daemon, RpcResult};
use crate::metrics::ProcSample;

/// Most rows one answer carries.
const MAX_ROWS: usize = 1000;
/// Levels of parents walked to find an owner.
const DEPTH: usize = 64;

/// What owns processes, as the daemon knows it now.
#[derive(Default)]
pub(crate) struct Roots {
    pub executions: HashMap<u32, (WorkspaceId, SessionId, ExecutionId)>,
    pub runs: HashMap<u32, (Option<TaskId>, WorkspaceId)>,
    /// Workspaces that exist and aren't archived.
    pub live: HashSet<String>,
    pub workspaces_dir: std::path::PathBuf,
    pub own_pid: u32,
}

/// The id of the workspace directory `cwd` is in, if it is in one.
fn workspace_of(cwd: &str, dir: &Path) -> Option<String> {
    let rest = Path::new(cwd).strip_prefix(dir).ok()?;
    Some(
        rest.components()
            .next()?
            .as_os_str()
            .to_string_lossy()
            .into_owned(),
    )
}

pub(crate) fn classify(procs: &[ProcSample], roots: &Roots) -> Vec<OtterProcess> {
    let parents: HashMap<u32, Option<u32>> = procs.iter().map(|p| (p.pid, p.parent)).collect();
    let owned = |pid: u32| -> Option<ProcessOwner> {
        let mut at = pid;
        for _ in 0..DEPTH {
            if let Some((w, s, e)) = roots.executions.get(&at) {
                return Some(ProcessOwner::Execution {
                    workspace_id: w.clone(),
                    session_id: s.clone(),
                    execution_id: e.clone(),
                });
            }
            if let Some((t, w)) = roots.runs.get(&at) {
                return Some(ProcessOwner::Run {
                    task_id: t.clone(),
                    workspace_id: w.clone(),
                });
            }
            at = (*parents.get(&at)?)?;
            if at <= 1 {
                return None;
            }
        }
        None
    };
    // What a stop must never reach: what holds owned work up (the parents of
    // a session's or a run's process), and the backend's server.
    let mut protected: HashSet<u32> = HashSet::new();
    for root in roots.executions.keys().chain(roots.runs.keys()) {
        let mut at = *root;
        for _ in 0..DEPTH {
            match parents.get(&at).copied().flatten() {
                Some(up) if up > 1 && protected.insert(up) => at = up,
                _ => break,
            }
        }
    }
    let mut out: Vec<OtterProcess> = procs
        .iter()
        .filter_map(|p| {
            let (owner, stoppable) = if p.pid == roots.own_pid {
                (ProcessOwner::Daemon { this: true }, false)
            } else if let Some(o) = owned(p.pid) {
                (o, false)
            } else if protected.contains(&p.pid) || p.name.starts_with("tmux") {
                // Holds owned work up, or is the backend's own server (kept
                // by this otterd's sessions, exited ones' panes included).
                return None;
            } else if p.otterd {
                (ProcessOwner::Daemon { this: false }, true)
            } else {
                let id = workspace_of(p.cwd.as_deref()?, &roots.workspaces_dir)?;
                if roots.live.contains(&id) {
                    return None;
                }
                (
                    ProcessOwner::Orphan {
                        why: "its workspace is gone or archived, and no session or run owns it"
                            .into(),
                    },
                    true,
                )
            };
            Some(OtterProcess {
                pid: p.pid,
                started: p.started,
                parent: p.parent,
                name: p.name.clone(),
                exe: p.exe.clone(),
                cwd: p.cwd.clone(),
                cpu_percent: p.cpu,
                memory: p.memory,
                busy_secs: p.busy_samples * crate::metrics::INTERVAL.as_secs() as u32,
                owner,
                stoppable,
            })
        })
        .collect();
    // Strays are never cut: the rows of what is owned are.
    out.sort_by_key(|p| (!p.stoppable, p.pid));
    let strays = out.iter().take_while(|p| p.stoppable).count();
    out.truncate(strays.max(MAX_ROWS));
    out.sort_by_key(|p| p.pid);
    out
}

impl Daemon {
    async fn process_roots(&self) -> Roots {
        let mut roots = Roots {
            workspaces_dir: self.paths.workspaces_dir.clone(),
            own_pid: std::process::id(),
            ..Roots::default()
        };
        let store = self.store.lock().await;
        for ws in &store.state.workspaces {
            if ws.state != WorkspaceState::Archived {
                roots.live.insert(ws.id.to_string());
            }
            for s in &ws.sessions {
                if let Some(e) = s.current_execution().filter(|e| e.is_running())
                    && let Some(pid) = e.pid
                {
                    roots
                        .executions
                        .insert(pid, (ws.id.clone(), s.id.clone(), e.id.clone()));
                }
            }
        }
        drop(store);
        for c in self.conversations.list(None) {
            if let Some(p) = &c.process
                && c.active_run.is_some()
                && p.generation == c.generation
            {
                roots
                    .runs
                    .insert(p.pid, (c.task_id.clone(), c.workspace_id.clone()));
            }
        }
        roots
    }

    /// `host.processes`.
    pub(crate) async fn host_processes(&self) -> RpcResult<HostProcesses> {
        let procs = self.metrics.processes();
        if procs.is_empty() {
            return Err(RpcError::new(
                ErrorCode::Unavailable,
                "measuring; try again in a few seconds",
            ));
        }
        let roots = self.process_roots().await;
        Ok(HostProcesses {
            sampled_at: chrono::Utc::now(),
            processes: classify(&procs, &roots),
        })
    }

    /// `host.processes.stop`: only what `host.processes` lists as
    /// stoppable, by pid and start time, read again now (not as last
    /// sampled) and classified again; never this daemon.
    pub(crate) async fn host_process_stop(&self, p: ProcessStop) -> RpcResult<()> {
        if p.pid == std::process::id() {
            return Err(RpcError::conflict("that is this otterd"));
        }
        let not_ours = || {
            RpcError::conflict("no such process of Otter's (it ended, or is not Otter's to stop)")
        };
        if p.pid <= 1 {
            return Err(not_ours());
        }
        // The sample for how the others sit in the tree; the target as it is
        // now: what it is running and where, not what it was then.
        let mut procs = self.metrics.processes();
        if procs.is_empty() {
            // Without the tree the owners can't be found.
            return Err(RpcError::new(
                ErrorCode::Unavailable,
                "measuring; try again in a few seconds",
            ));
        }
        let target = tokio::task::spawn_blocking(move || crate::metrics::fresh(p.pid))
            .await
            .ok()
            .flatten()
            .filter(|t| t.started == p.started)
            .ok_or_else(&not_ours)?;
        procs.retain(|s| s.pid != p.pid);
        procs.push(target);
        let roots = self.process_roots().await;
        match classify(&procs, &roots)
            .into_iter()
            .find(|r| r.pid == p.pid)
        {
            Some(r) if r.stoppable => {}
            Some(_) => {
                return Err(RpcError::conflict(
                    "that process belongs to a session or a run: stop it there",
                ));
            }
            None => return Err(not_ours()),
        }
        let signal = if p.force {
            libc::SIGKILL
        } else {
            libc::SIGTERM
        };
        tracing::warn!(
            pid = p.pid,
            force = p.force,
            "stopping a process by request"
        );
        let pid = p.pid as libc::pid_t;
        // SAFETY: plain signals to a process this daemon just read again and
        // identified by pid and start time as an otterd of another process
        // tree or an orphan of its own workspaces. Its group is signalled only
        // when it leads one other than this daemon's.
        let result = unsafe {
            if libc::getpgid(pid) == pid && pid != libc::getpgid(0) {
                libc::killpg(pid, signal)
            } else {
                libc::kill(pid, signal)
            }
        };
        if result != 0 {
            return Err(match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => RpcError::conflict("that process has ended"),
                Some(libc::EPERM) => {
                    RpcError::conflict("that process is not this user's: otterd may not end it")
                }
                _ => RpcError::internal("the signal could not be sent"),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn proc(pid: u32, parent: Option<u32>, cwd: Option<&str>, otterd: bool) -> ProcSample {
        ProcSample {
            pid,
            parent,
            name: if otterd { "otterd" } else { "x" }.into(),
            exe: None,
            cwd: cwd.map(str::to_owned),
            cpu: 1.0,
            memory: 1,
            started: 100 + pid as u64,
            otterd,
            busy_samples: 0,
        }
    }

    fn roots() -> Roots {
        let mut r = Roots {
            workspaces_dir: PathBuf::from("/h/workspaces"),
            own_pid: 10,
            ..Roots::default()
        };
        r.live.insert("ws_live".into());
        r.executions.insert(
            20,
            (
                WorkspaceId::generate(),
                SessionId::generate(),
                ExecutionId::generate(),
            ),
        );
        r
    }

    #[test]
    fn it_finds_owners_by_ancestry_and_leaves_the_rest_alone() {
        let procs = vec![
            proc(10, Some(1), None, true),
            // A session's shell, and what it started.
            proc(20, Some(5), Some("/h/workspaces/ws_live"), false),
            proc(21, Some(20), Some("/h/workspaces/ws_live"), false),
            // Another otterd.
            proc(30, Some(1), None, true),
            // In a workspace that is gone, owned by nothing.
            proc(40, Some(1), Some("/h/workspaces/ws_gone/src"), false),
            // In a live workspace, owned by nothing: left alone.
            proc(41, Some(1), Some("/h/workspaces/ws_live"), false),
            // Somebody's own work elsewhere.
            proc(50, Some(1), Some("/home/u/project"), false),
        ];
        let rows = classify(&procs, &roots());
        let by = |pid| rows.iter().find(|r| r.pid == pid);
        assert!(matches!(
            by(10).unwrap().owner,
            ProcessOwner::Daemon { this: true }
        ));
        assert!(!by(10).unwrap().stoppable);
        assert!(matches!(
            by(20).unwrap().owner,
            ProcessOwner::Execution { .. }
        ));
        assert!(matches!(
            by(21).unwrap().owner,
            ProcessOwner::Execution { .. }
        ));
        assert!(!by(21).unwrap().stoppable);
        assert!(matches!(
            by(30).unwrap().owner,
            ProcessOwner::Daemon { this: false }
        ));
        assert!(by(30).unwrap().stoppable);
        assert!(matches!(by(40).unwrap().owner, ProcessOwner::Orphan { .. }));
        assert!(by(40).unwrap().stoppable);
        assert!(by(41).is_none() && by(50).is_none());
    }

    #[test]
    fn what_holds_owned_work_up_and_the_backends_server_are_never_strays() {
        let mut tmux = proc(11, Some(1), Some("/h/workspaces/ws_gone"), false);
        tmux.name = "tmux".into();
        let procs = vec![
            proc(10, Some(1), None, true),
            // The backend's server: daemonized, a child of init, in a deleted
            // workspace's folder (an otterd started from inside one).
            tmux,
            // The parent of a session's process: in a gone folder, but it
            // holds the session up.
            proc(6, Some(1), Some("/h/workspaces/ws_gone"), false),
            proc(20, Some(6), Some("/h/workspaces/ws_live"), false),
            // A worker this otterd started that outlived its run: a stray.
            proc(30, Some(10), Some("/h/workspaces/ws_gone"), false),
        ];
        let rows = classify(&procs, &roots());
        let listed: Vec<u32> = rows.iter().map(|r| r.pid).collect();
        assert_eq!(listed, [10, 20, 30], "{rows:?}");
        assert!(rows.iter().find(|r| r.pid == 30).unwrap().stoppable);
    }

    #[test]
    fn strays_are_not_cut_with_the_rows_of_what_is_owned() {
        let mut roots = roots();
        let mut procs = vec![proc(900_000, Some(1), Some("/h/workspaces/ws_gone"), false)];
        for i in 0..(MAX_ROWS as u32 + 50) {
            roots.executions.insert(
                1000 + i,
                (
                    WorkspaceId::generate(),
                    SessionId::generate(),
                    ExecutionId::generate(),
                ),
            );
            procs.push(proc(1000 + i, Some(5), None, false));
        }
        let rows = classify(&procs, &roots);
        assert!(rows.iter().any(|r| r.pid == 900_000 && r.stoppable));
        assert!(rows.len() <= MAX_ROWS + 1);
    }
}
