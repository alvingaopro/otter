//! Triggers (D-083): work that starts without a person at the keyboard, on
//! a schedule kept here or fired by hand, under a policy the developer set
//! in advance. Kept in `state/triggers.json`.
//!
//! - **One task per occurrence:** an occurrence's task is created
//!   and started with command ids derived from the trigger and the
//!   occurrence (`Trigger::command_id`), so firing it again — a retry, a
//!   restart, a script run twice — finds the same task.
//! - **Missed occurrences** (otterd was down) fire once, the latest.
//! - **No overlap:** an occurrence whose trigger's last task is still at
//!   work is skipped, and says so in the trigger's history.
//! - **The policy** is applied to the task as it's made: budget caps,
//!   publishing allowed to the workspace's branch, a wait limit (enforced
//!   by the controller, `waited_too_long`).

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use otter_core::task::{TaskAction, TaskStatus};
use otter_core::trigger::{Firing, Schedule, Started, Trigger};
use otter_core::{Timestamp, TriggerId};
use otter_protocol::{RpcError, TriggerCreate};

use crate::daemon::{Daemon, RpcResult, workspace_not_found};

/// Occurrences kept in a trigger's history.
const HISTORY: usize = 20;

pub(crate) struct TriggerStore {
    path: PathBuf,
    pub list: Vec<Trigger>,
}

impl TriggerStore {
    pub fn load(state_dir: &std::path::Path) -> anyhow::Result<TriggerStore> {
        let path = state_dir.join("triggers.json");
        Ok(TriggerStore {
            list: crate::settings::read_json(&path)?,
            path,
        })
    }

    pub fn empty(state_dir: &std::path::Path) -> TriggerStore {
        TriggerStore {
            path: state_dir.join("triggers.json"),
            list: vec![],
        }
    }

    fn save(&self) -> RpcResult<()> {
        let bytes =
            serde_json::to_vec_pretty(&self.list).map_err(|e| RpcError::internal(e.to_string()))?;
        crate::settings::write_private(&self.path, &bytes)
            .map_err(|e| RpcError::internal(format!("saving triggers: {e:#}")))
    }

    fn find(&self, r: &str) -> Option<usize> {
        self.list
            .iter()
            .position(|t| t.id.as_str() == r)
            .or_else(|| self.list.iter().position(|t| t.name == r))
    }
}

/// How often due triggers are looked for (`OTTER_TRIGGER_TICK_MS`, 15 s).
fn tick() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("OTTER_TRIGGER_TICK_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15_000),
    )
}

impl Daemon {
    pub(crate) async fn trigger_create(&self, p: TriggerCreate) -> RpcResult<Trigger> {
        let name = p.name.trim().to_owned();
        otter_core::validate_name("trigger", &name).map_err(RpcError::invalid)?;
        if p.request.trim().is_empty() {
            return Err(RpcError::invalid(
                "a trigger needs a request: what its tasks do",
            ));
        }
        match &p.schedule {
            Schedule::Every { minutes } if *minutes == 0 => {
                return Err(RpcError::invalid("every 0 minutes isn't a schedule"));
            }
            Schedule::Daily { at } if chrono::NaiveTime::parse_from_str(at, "%H:%M").is_err() => {
                return Err(RpcError::invalid(format!(
                    "`{at}` isn't a time (HH:MM, UTC)"
                )));
            }
            _ => {}
        }
        let workspace_id = self
            .store
            .lock()
            .await
            .workspace(&p.workspace)
            .map(|w| w.id.clone())
            .ok_or_else(|| workspace_not_found(&p.workspace))?;
        let mut store = self.triggers.lock().await;
        if store.list.iter().any(|t| t.name == name) {
            return Err(RpcError::conflict(format!(
                "a trigger `{name}` exists already"
            )));
        }
        let now = Utc::now();
        let t = Trigger {
            id: TriggerId::generate(),
            title: p
                .title
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| name.clone()),
            name,
            workspace_id,
            request: p.request.trim().to_owned(),
            next_at: p.schedule.next_after(now, now),
            schedule: p.schedule,
            policy: p.policy,
            enabled: true,
            created_at: now,
            history: vec![],
        };
        store.list.push(t.clone());
        store.save()?;
        Ok(t)
    }

    pub(crate) async fn trigger_list(&self) -> Vec<Trigger> {
        self.triggers.lock().await.list.clone()
    }

    pub(crate) async fn trigger_delete(&self, r: &str) -> RpcResult<()> {
        let mut store = self.triggers.lock().await;
        let i = store
            .find(r)
            .ok_or_else(|| RpcError::not_found(format!("no trigger `{r}`")))?;
        store.list.remove(i);
        store.save()
    }

    /// Fire `r` for `occurrence` (by hand: now, or the one a script names).
    pub(crate) async fn trigger_run(
        self: &Arc<Self>,
        r: &str,
        occurrence: Option<Timestamp>,
    ) -> RpcResult<Trigger> {
        let t = {
            let store = self.triggers.lock().await;
            let i = store
                .find(r)
                .ok_or_else(|| RpcError::not_found(format!("no trigger `{r}`")))?;
            store.list[i].clone()
        };
        self.fire(&t, occurrence.unwrap_or_else(Utc::now)).await?;
        let store = self.triggers.lock().await;
        Ok(store
            .list
            .iter()
            .find(|x| x.id == t.id)
            .cloned()
            .unwrap_or(t))
    }

    /// Fire due triggers until the daemon stops.
    pub(crate) async fn run_triggers(self: Arc<Self>) {
        let mut shutdown = self.shutdown_signal();
        loop {
            let now = Utc::now();
            let due: Vec<(Trigger, Timestamp)> = {
                let store = self.triggers.lock().await;
                store
                    .list
                    .iter()
                    .filter(|t| t.enabled)
                    .filter_map(|t| {
                        let mut at = t.next_at.filter(|n| *n <= now)?;
                        // Missed while otterd was down: the latest only.
                        while let Some(n) = t
                            .schedule
                            .next_after(at, t.created_at)
                            .filter(|n| *n <= now)
                        {
                            at = n;
                        }
                        Some((t.clone(), at))
                    })
                    .collect()
            };
            for (t, at) in due {
                if let Err(e) = self.fire(&t, at).await {
                    tracing::warn!(trigger = %t.name, "firing: {}", e.message);
                }
                let mut store = self.triggers.lock().await;
                if let Some(x) = store.list.iter_mut().find(|x| x.id == t.id) {
                    x.next_at = x.schedule.next_after(now, x.created_at);
                }
                if let Err(e) = store.save() {
                    tracing::warn!("{}", e.message);
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(tick()) => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    /// One occurrence: the trigger's task, made and started once (D-083).
    async fn fire(self: &Arc<Self>, t: &Trigger, occurrence: Timestamp) -> RpcResult<()> {
        let command = t.command_id(occurrence);
        // Already fired for this occurrence: nothing more.
        if t.history.iter().any(|h| h.occurrence == occurrence) {
            return Ok(());
        }
        // Made but not recorded (otterd stopped half way): each step below
        // finds what it already did, and the rest is done.
        let made = self.tasks.lock().await.applied(&command).is_some();
        // Its last task still at work: this occurrence is skipped.
        let last = t.history.iter().rev().find_map(|h| h.task_id.clone());
        if !made
            && let Some(id) = last
            && let Ok(f) = self.task_get(id.as_str()).await
            && (f.status.is_active() || f.status == TaskStatus::Blocked)
        {
            return self
                .record_firing(
                    t,
                    occurrence,
                    None,
                    Some(format!("“{}” is still at work", f.title)),
                )
                .await;
        }
        let f = self
            .task_create(otter_protocol::task::TaskCreate {
                command_id: command.clone(),
                title: t.title.clone(),
                request: t.request.clone(),
                workspace: Some(t.workspace_id.to_string()),
                answer_only: false,
            })
            .await?;
        // Its policy, set as it's made (D-083).
        let publish = if t.policy.publish {
            let store = self.store.lock().await;
            match store.workspace(t.workspace_id.as_str()).map(|w| &w.source) {
                Some(otter_core::WorkspaceSource::Git(g)) if !g.branch.is_empty() => {
                    Some(format!("git push origin {}", g.branch))
                }
                _ => None,
            }
        } else {
            None
        };
        let started = Started {
            trigger_id: t.id.clone(),
            trigger: t.name.clone(),
            occurrence,
            policy: t.policy.clone(),
        };
        let policy = t.policy.clone();
        let applied = self.tasks.lock().await.apply(
            &f.id,
            Some(&format!("{command}:policy")),
            move |f, _| {
                f.started_by = Some(started);
                if let Some(n) = policy.max_iterations {
                    f.budget.max_iterations = n;
                }
                if let Some(n) = policy.max_minutes {
                    f.budget.max_minutes = n;
                }
                if let Some(cmd) = publish
                    && !f.allowed_commands.contains(&cmd)
                {
                    f.allowed_commands.push(cmd);
                }
                Ok(vec![])
            },
        );
        let applied = applied?;
        if !applied.duplicate {
            self.task_changed(&applied);
        }
        self.clone()
            .task_act(otter_protocol::task::TaskAct {
                command_id: format!("{command}:start"),
                task: f.id.to_string(),
                action: TaskAction::Start,
            })
            .await?;
        self.record_firing(t, occurrence, Some(f.id), None).await
    }

    async fn record_firing(
        &self,
        t: &Trigger,
        occurrence: Timestamp,
        task_id: Option<otter_core::TaskId>,
        skipped: Option<String>,
    ) -> RpcResult<()> {
        let mut store = self.triggers.lock().await;
        if let Some(x) = store.list.iter_mut().find(|x| x.id == t.id)
            && !x.history.iter().any(|h| h.occurrence == occurrence)
        {
            x.history.push(Firing {
                occurrence,
                at: Utc::now(),
                task_id,
                skipped,
            });
            let extra = x.history.len().saturating_sub(HISTORY);
            x.history.drain(..extra);
        }
        store.save()
    }
}
