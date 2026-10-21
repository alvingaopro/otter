//! The processes Otter started or must answer for (`host.processes`, D-156),
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
    let mut out: Vec<OtterProcess> = procs
        .iter()
        .filter_map(|p| {
            let (owner, stoppable) = if p.pid == roots.own_pid {
                (ProcessOwner::Daemon { this: true }, false)
            } else if p.otterd {
                (ProcessOwner::Daemon { this: false }, true)
            } else if let Some(o) = owned(p.pid) {
                (o, false)
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
    out.sort_by_key(|p| p.pid);
    out.truncate(MAX_ROWS);
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
    /// stoppable, by pid and start time, found again now; never this daemon.
    pub(crate) async fn host_process_stop(&self, p: ProcessStop) -> RpcResult<()> {
        if p.pid == std::process::id() || p.pid <= 1 {
            return Err(RpcError::conflict("that is this otterd"));
        }
        let procs = self.metrics.processes();
        let roots = self.process_roots().await;
        let found = classify(&procs, &roots)
            .into_iter()
            .find(|r| r.pid == p.pid && r.started == p.started);
        match found {
            Some(r) if r.stoppable => {}
            Some(_) => {
                return Err(RpcError::conflict(
                    "that process belongs to a session or a run: stop it there",
                ));
            }
            None => {
                return Err(RpcError::conflict(
                    "no such process of Otter's (it ended, or is not Otter's to stop)",
                ));
            }
        }
        // The same process still, by its start time (read again, not the sample's).
        if crate::runtime::process_started(p.pid) != Some(p.started) {
            return Err(RpcError::conflict("that process has ended"));
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
        // SAFETY: plain signals to a process this daemon just identified by
        // pid and start time as an otterd of another build or an orphan of
        // its own workspaces. Its group is signalled only when it leads one.
        unsafe {
            if libc::getpgid(pid) == pid && pid != libc::getpgid(0) {
                libc::killpg(pid, signal);
            } else {
                libc::kill(pid, signal);
            }
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
}
