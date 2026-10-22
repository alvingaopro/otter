//! A task's own workspace goes once its work is merged (D-189).
//!
//! - **What is looked at:** the workspace made for a task (D-080),
//!   archived when the task ended (D-105), from Git, and still the task's;
//!   the task done or cancelled, asking nothing, with a pull request on
//!   GitHub. Anything else stays as it is: a workspace the developer
//!   unarchived, chose or attached, a task with no pull request, a host
//!   without `gh`.
//! - **When it goes:** GitHub says the pull request is merged, a day ago or
//!   more ([`grace`]), and nothing was committed in the workspace after the
//!   pull request's head.
//! - **How:** the ordinary delete, without force (D-108). What is only
//!   here (uncommitted changes, files Git doesn't track, commits on no
//!   remote) keeps it: the workspace is marked kept, the conversation says
//!   why once, and it isn't looked at again until it is unarchived. Never
//!   with force.
//! - **On a timer of its own:** every half hour, the first a minute after
//!   otterd starts (`OTTER_MERGED_SWEEP_MS`), one `gh pr view` for each
//!   workspace looked at. Not a watch (D-183): a watch only looks and says,
//!   and ends in Otter's turn; this acts, with no model. What it learns is
//!   kept in memory: a pull request closed without merging is asked about
//!   again a day later (it can be reopened), one still open on the next
//!   pass.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use otter_core::task::{MessageRole, Task, TaskStatus};
use otter_core::{TaskId, Workspace, WorkspaceId, WorkspaceSource, WorkspaceState};

use crate::daemon::Daemon;
use crate::delivery::{Git, GitHub, PrState};
use crate::tasks::message;
use crate::watches::{env_ms, jitter};
use crate::workspaces::Loses;

/// How long after its pull request is merged a workspace stays: the
/// developer may still look, as D-105 says they can.
pub(crate) fn grace() -> chrono::Duration {
    chrono::Duration::hours(24)
}

/// How often the workspaces are looked at (`OTTER_MERGED_SWEEP_MS`).
fn every() -> Duration {
    env_ms("OTTER_MERGED_SWEEP_MS", 30 * 60_000)
}

/// How long one look at GitHub may take.
const ASK_LIMIT: Duration = Duration::from_secs(60);

/// When a pull request closed without merging is asked about again.
const CLOSED_AGAIN: Duration = Duration::from_secs(24 * 3600);

/// A task's archived workspace, and the pull request its work went to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Candidate {
    pub task: TaskId,
    pub workspace: WorkspaceId,
    pub name: String,
    pub root: String,
    pub pr: u64,
    /// `owner/name`, from the pull request's URL.
    pub repo: String,
}

/// Whether `ws`, `f`'s workspace, is one to ask GitHub about.
pub(crate) fn candidate(f: &Task, ws: &Workspace) -> Option<Candidate> {
    let mine = ws.task_id.as_ref() == Some(&f.id) && f.workspace_id.as_ref() == Some(&ws.id);
    let put_away = ws.state == WorkspaceState::Archived && ws.kept.is_none();
    let over = matches!(f.status, TaskStatus::Done | TaskStatus::Cancelled)
        && f.pending_decisions().next().is_none();
    if !mine || !put_away || !over || !matches!(ws.source, WorkspaceSource::Git(_)) {
        return None;
    }
    let d = f.delivery.as_ref().filter(|d| !d.declined)?;
    Some(Candidate {
        task: f.id.clone(),
        workspace: ws.id.clone(),
        name: ws.name.clone(),
        root: ws.root.clone(),
        pr: d.pr_number?,
        repo: otter_core::watch::repo_of(d.pr_url.as_deref()?)?,
    })
}

/// What GitHub's answer means for a workspace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Merged, the grace passed: it goes.
    Merged,
    /// Merged within the grace, or GitHub didn't say when: not yet.
    Soon,
    /// Still open.
    Open,
    /// Closed without merging: it stays.
    Closed,
}

pub(crate) fn verdict(pr: &PrState, now: DateTime<Utc>) -> Verdict {
    match pr.state.as_str() {
        "MERGED" => match pr.merged_at {
            Some(at) if now - at >= grace() => Verdict::Merged,
            _ => Verdict::Soon,
        },
        "CLOSED" => Verdict::Closed,
        _ => Verdict::Open,
    }
}

/// Why a workspace stays, in the task's conversation.
fn kept_because(loses: &Loses) -> String {
    match loses {
        Loses::Uncommitted { untracked: true } => "it holds files Git doesn't track".into(),
        Loses::Uncommitted { untracked: false } => "it has uncommitted changes".into(),
        Loses::Unpushed { branch, commits: n } => format!(
            "its branch `{branch}` has {n} commit{} not pushed anywhere",
            if *n == 1 { "" } else { "s" }
        ),
    }
}

impl Daemon {
    /// Look at the tasks' archived workspaces until the daemon stops.
    pub(crate) async fn run_merged(self: std::sync::Arc<Self>) {
        let mut shutdown = self.shutdown_signal();
        let mut later: HashMap<WorkspaceId, Instant> = HashMap::new();
        let mut wait = every().min(Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => return,
            }
            self.merged_pass(&mut later).await;
            wait = jitter(every());
        }
    }

    /// One look at every workspace that may go. `later`: when one not due
    /// is asked about again.
    async fn merged_pass(&self, later: &mut HashMap<WorkspaceId, Instant>) {
        if !self.settings.read().unwrap().delete_merged_workspaces() {
            return;
        }
        let tasks = self.tasks.lock().await.list();
        let candidates: Vec<Candidate> = {
            let store = self.store.lock().await;
            tasks
                .iter()
                .filter_map(|f| candidate(f, store.workspace(f.workspace_id.as_ref()?.as_str())?))
                .collect()
        };
        later.retain(|id, _| candidates.iter().any(|c| &c.workspace == id));
        if candidates.is_empty() {
            return;
        }
        let env = self.git_account.gh_env(self.environments.base());
        let dir = self.otter_dir();
        for c in candidates {
            if later.get(&c.workspace).is_some_and(|t| *t > Instant::now()) {
                continue;
            }
            // No `gh` here: nothing goes, as before D-189.
            let Some(mut gh) = GitHub::find(&dir, &env) else {
                return;
            };
            gh.repo = Some(c.repo.clone());
            let pr = match tokio::time::timeout(ASK_LIMIT, gh.pr_state(c.pr)).await {
                Ok(Ok(pr)) => pr,
                Ok(Err(e)) => {
                    let e = otter_core::redact::redact(&format!("{e:#}"));
                    tracing::info!(workspace = %c.name, pr = c.pr, "reading the pull request: {e}");
                    continue;
                }
                Err(_) => continue,
            };
            match verdict(&pr, Utc::now()) {
                Verdict::Merged => self.delete_merged(&c, &pr).await,
                Verdict::Closed => {
                    later.insert(c.workspace, Instant::now() + CLOSED_AGAIN);
                }
                Verdict::Soon | Verdict::Open => {}
            }
        }
    }

    /// `c`'s pull request is merged: delete its workspace, or keep it and
    /// say why.
    async fn delete_merged(&self, c: &Candidate, pr: &PrState) {
        // As it is now: unarchived or kept meanwhile, it stays.
        let Ok(ws) = self.workspace_get(c.workspace.as_str()).await else {
            return;
        };
        if ws.state != WorkspaceState::Archived || ws.kept.is_some() {
            return;
        }
        // Committed here after the pull request's last push: work GitHub
        // doesn't have under this pull request. D-108's guard below is the
        // one that keeps what is on no remote; this keeps, too, what was
        // pushed after the pull request was merged.
        let git = Git {
            root: Path::new(&c.root),
            env: self.environments.base(),
        };
        let after = match git.head().await {
            Ok(head) if head == pr.head => false,
            Ok(_) => git.within(&pr.head).await == Some(false),
            Err(_) => false,
        };
        let why = match self.delete_loses(&ws).await {
            Some(loses) => Some(kept_because(&loses)),
            None if after => Some("its branch has commits that aren't in the pull request".into()),
            None => None,
        };
        if let Some(why) = why {
            self.keep_merged(c, why).await;
            return;
        }
        // The task as it is now, too: asked for more meanwhile, it isn't
        // over any more.
        match self.task_get(c.task.as_str()).await {
            Ok(f) if candidate(&f, &ws).is_some() => {}
            _ => return,
        }
        // Still put away once the delete has begun: an unarchive meanwhile
        // (the developer's, or the task's own when more is asked of it) is
        // never undone.
        let deleted = self
            .delete_workspace(
                &otter_protocol::WorkspaceDelete {
                    workspace: c.workspace.to_string(),
                    force: false,
                },
                |w| w.state == WorkspaceState::Archived && w.kept.is_none(),
                Some((c.pr, pr.base.clone())),
            )
            .await;
        if let Err(e) = deleted {
            // Unarchived or being deleted meanwhile, or something failed:
            // looked at again on the next pass.
            tracing::warn!(workspace = %c.name, "deleting a merged task's workspace: {}", e.message);
            return;
        }
        tracing::info!(workspace = %c.name, pr = c.pr, "deleted a task's workspace: its pull request is merged");
        let base = match pr.base.as_str() {
            "" => "its base branch".to_owned(),
            b => b.to_owned(),
        };
        self.say_on_task(
            &c.task,
            format!(
                "Its workspace {} is deleted: pull request #{} was merged, and everything in it is on {base}. Asked for more, the task starts again from {base}.",
                c.name, c.pr
            ),
        )
        .await;
    }

    /// Keep `c`'s workspace, marked, and say why once.
    async fn keep_merged(&self, c: &Candidate, why: String) {
        {
            let mut store = self.store.lock().await;
            let Some(ws) = store.workspace_mut(c.workspace.as_str()) else {
                return;
            };
            ws.kept = Some(why.clone());
            if let Err(e) = crate::daemon::save(&store) {
                tracing::error!("{e}");
                return;
            }
        }
        tracing::info!(workspace = %c.name, "keeping a merged task's workspace: {why}");
        self.say_on_task(
            &c.task,
            format!(
                "Its workspace {} stays although pull request #{} is merged: {why}. Delete it when you no longer need what is there.",
                c.name, c.pr
            ),
        )
        .await;
    }

    /// A line from Otter in the task's conversation.
    async fn say_on_task(&self, task: &TaskId, text: String) {
        let applied = self.tasks.lock().await.apply(task, None, |f, _| {
            let (m, c) = message(MessageRole::Controller, text, None);
            f.messages.push(m);
            Ok(vec![c])
        });
        if let Ok(a) = applied {
            self.task_changed(&a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::task::Delivery;
    use otter_core::{GitSource, WorkspaceSource};

    fn task_and_workspace() -> (Task, Workspace) {
        let now = Utc::now();
        let mut f = Task::new("CSV export".into(), "Export it".into(), now);
        f.status = TaskStatus::Done;
        let ws = Workspace {
            id: WorkspaceId::from("ws_1"),
            name: "app-csv-export".into(),
            root: "/tmp/ws_1".into(),
            brief: Default::default(),
            source: WorkspaceSource::Git(GitSource {
                repository: "git@github.com:o/r.git".into(),
                repo_id: "r".into(),
                branch: "otter/csv-export-ab12".into(),
                base: Some("origin/main".into()),
                base_commit: None,
                requested_branch: None,
            }),
            environment: Default::default(),
            state: WorkspaceState::Archived,
            state_message: None,
            created_at: now,
            updated_at: now,
            sessions: vec![],
            attention: vec![],
            task_id: Some(f.id.clone()),
            kept: None,
        };
        f.workspace_id = Some(ws.id.clone());
        f.delivery = Some(Delivery {
            branch: "otter/csv-export-ab12".into(),
            base: Some("main".into()),
            head: None,
            pushed_at: None,
            pr_url: Some("https://github.com/o/r/pull/7".into()),
            pr_number: Some(7),
            ci: vec![],
            reruns: 0,
            declined: false,
            mergeable: None,
            ci_read_at: None,
        });
        (f, ws)
    }

    #[test]
    fn only_a_finished_tasks_own_archived_workspace_with_a_pull_request() {
        let (f, ws) = task_and_workspace();
        let c = candidate(&f, &ws).expect("looked at");
        assert_eq!((c.pr, c.repo.as_str()), (7, "o/r"));

        let mut g = f.clone();
        g.status = TaskStatus::Cancelled;
        assert!(candidate(&g, &ws).is_some(), "cancelled is over too");

        for (what, f, ws) in [
            (
                "in review",
                {
                    let mut f = f.clone();
                    f.status = TaskStatus::Review;
                    f
                },
                ws.clone(),
            ),
            ("unarchived", f.clone(), {
                let mut w = ws.clone();
                w.state = WorkspaceState::Ready;
                w
            }),
            ("kept", f.clone(), {
                let mut w = ws.clone();
                w.kept = Some("x".into());
                w
            }),
            ("chosen, not made for it", f.clone(), {
                let mut w = ws.clone();
                w.task_id = None;
                w
            }),
            (
                "not its workspace now",
                {
                    let mut f = f.clone();
                    f.workspace_id = Some(WorkspaceId::from("ws_2"));
                    f
                },
                ws.clone(),
            ),
            ("attached", f.clone(), {
                let mut w = ws.clone();
                w.source = WorkspaceSource::Directory { path: "/x".into() };
                w
            }),
            (
                "no pull request",
                {
                    let mut f = f.clone();
                    f.delivery.as_mut().unwrap().pr_number = None;
                    f
                },
                ws.clone(),
            ),
            (
                "declined",
                {
                    let mut f = f.clone();
                    f.delivery.as_mut().unwrap().declined = true;
                    f
                },
                ws.clone(),
            ),
            (
                "not GitHub",
                {
                    let mut f = f.clone();
                    f.delivery.as_mut().unwrap().pr_url = Some("https://example.com/x".into());
                    f
                },
                ws.clone(),
            ),
            (
                "no delivery",
                {
                    let mut f = f.clone();
                    f.delivery = None;
                    f
                },
                ws.clone(),
            ),
        ] {
            assert_eq!(candidate(&f, &ws), None, "{what}");
        }
    }

    #[test]
    fn merged_a_day_ago_goes() {
        let now = Utc::now();
        let pr = |state: &str, ago: Option<i64>| PrState {
            state: state.into(),
            mergeable: "UNKNOWN".into(),
            head: "abc".into(),
            base: "main".into(),
            merged_at: ago.map(|h| now - chrono::Duration::hours(h)),
        };
        assert_eq!(verdict(&pr("MERGED", Some(25)), now), Verdict::Merged);
        assert_eq!(verdict(&pr("MERGED", Some(24)), now), Verdict::Merged);
        assert_eq!(verdict(&pr("MERGED", Some(1)), now), Verdict::Soon);
        assert_eq!(verdict(&pr("MERGED", None), now), Verdict::Soon);
        assert_eq!(verdict(&pr("OPEN", None), now), Verdict::Open);
        assert_eq!(verdict(&pr("CLOSED", None), now), Verdict::Closed);
        assert_eq!(verdict(&pr("", None), now), Verdict::Open);
    }

    #[test]
    fn why_it_stays_names_what_is_only_there() {
        assert_eq!(
            kept_because(&Loses::Uncommitted { untracked: true }),
            "it holds files Git doesn't track"
        );
        assert_eq!(
            kept_because(&Loses::Unpushed {
                branch: "b".into(),
                commits: 2
            }),
            "its branch `b` has 2 commits not pushed anywhere"
        );
    }
}
