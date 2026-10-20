//! The Control Agent's engine (D-045): owns a task from plan to review.
//!
//! An event-driven loop in the daemon — woken whenever a task changes,
//! and on a tick that enforces time limits — steps each active task:
//!
//! - **planning** → the coding agent reads the code and answers a question
//!   (the task is done) or proposes requirements, acceptance criteria,
//!   steps and a check command (which policy must allow, or someone
//!   approves); the brain reviews the plan and doesn't rewrite it (D-084);
//! - **implementing** → the next step whose dependencies are done goes to a
//!   managed run (an interrupted one resumes its conversation); the brain
//!   decides what policy leaves open, the rest waits for the developer;
//! - **verifying** → the check runs and the work's state (its diff) is read,
//!   evidence is recorded; the check decides check-typed criteria, the brain
//!   judges the rest; all met → review, else a remediation step →
//!   implementing. A check that can't run at all blocks on the developer; a
//!   fix that changed nothing goes to review unverified (D-062).
//!
//! Deterministic code enforces what a model must not decide: the lifecycle,
//! the attempt and time budgets, per-run timeouts, loop detection, and what
//! may run. The task document is the checkpoint: a restarted daemon picks
//! up where the last one stopped. One task, one run at a time (MVP). No
//! lock is held across a model call, a run or a check.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use otter_core::task::{
    ARCHIVED, Criterion, Decider, DecisionKind, DecisionRequest, DecisionStatus, Evidence,
    EvidenceKind, MessageRole, Risk, RunState, Step, StepStatus, Task, TaskEvent, TaskStatus,
    VerificationMark, WAITING, YOUR_AGENT,
};
use otter_core::{DecisionId, EvidenceId, StepId, TaskId};
use otter_protocol::RpcError;

use crate::brain::{self, Context, Verdict as BrainVerdict};
use crate::daemon::{Daemon, RpcResult, workspace_not_found};
use crate::env::EnvMap;
use crate::runtime::ToolCall;
use crate::runtime::policy::{self, Verdict};
use crate::tasks::{Change, message};

/// Attempts at one step before the task fails.
const MAX_TASK_ATTEMPTS: u32 = 3;
/// The same failure this many runs in a row is a loop.
const LOOP_REPEATS: usize = 3;
const CHECK_OUTPUT: usize = 3000;
/// The title of delivery's diffstat evidence, before the branch.
const CHANGES_ON: &str = "Changes on ";
/// The answers to "the check can't run here" (D-062).
const RUN_AGAIN: &str = "Run it again";
const WITHOUT_CHECK: &str = "Go on without it";

fn env_ms(var: &str, default: u64) -> Duration {
    Duration::from_millis(
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

/// How often the controller looks even without a change (time limits).
fn tick() -> Duration {
    env_ms("OTTER_CONTROLLER_TICK_MS", 5_000)
}

/// How long the agent may work in one run (time waiting on a decision
/// doesn't count, D-063); then its turn is cut off and resumed.
fn run_timeout() -> chrono::Duration {
    chrono::Duration::from_std(env_ms("OTTER_RUN_TIMEOUT_MS", 30 * 60 * 1000)).unwrap()
}

/// How long a run may go without a sign of work, while none of its tools
/// runs, before its turn is cut off and resumed (D-063).
fn idle_timeout() -> Duration {
    env_ms("OTTER_RUN_IDLE_MS", 10 * 60 * 1000)
}

/// Wall time that passes beyond the monotonic clock by more than this, the
/// host was asleep: not work (D-063).
const SLEEP_GAP: Duration = Duration::from_secs(30);

/// How a run cut off by a limit is summarized (and recognized on resume).
const OUT_OF_TIME: &str = "Out of time";
const NO_PROGRESS: &str = "No progress";

/// Whether planning starts with the coding agent's look at the code
/// (`OTTER_PLAN_TURN`, on unless `0`; D-064).
pub(crate) fn plan_turn() -> bool {
    std::env::var("OTTER_PLAN_TURN").map_or(true, |v| v != "0")
}

/// The planning step's title.
const PLANNING: &str = "Look at the code and plan the work";
/// The step CI's failure becomes, and what is said when a fix didn't change it.
const CI_FIX: &str = "Fix what CI found";
const SAME_CI: &str = "CI failed the same way after the fix";
/// What Otter asks when a task starts with nothing to do (D-067).
const GOAL_QUESTION: &str = "What should this task do?";
/// Why a task is done when its request was a question (D-084).
const ANSWERED: &str = "Answered: no change was asked for. Say so if you want one.";
/// What Otter asks when a revised plan still has problems (D-084).
const PLAN_QUESTION: &str = "Go ahead with the coding agent's plan?";

/// Why planning stopped when the look at the code failed — the agent's
/// doing or otterd's, as `why` says (D-134).
fn looked_and_failed(why: &str) -> String {
    format!("The look at the code failed ({why}). Retry, or say go on, to try again.")
}

/// Why a step stopped when it panicked.
const PANICKED: &str = "an internal error (otterd's log says where)";

/// `work` as a task of its own, so that a panic in it is an error like any
/// other, not the end of what waits on it (D-134). `None`: otterd is going.
async fn contained(
    work: impl std::future::Future<Output = RpcResult<()>> + Send + 'static,
) -> Option<RpcResult<()>> {
    match tokio::spawn(work).await {
        Ok(r) => Some(r),
        Err(e) if e.is_panic() => Some(Err(RpcError::internal(PANICKED))),
        Err(_) => None,
    }
}

/// The developer's word on the plan the planning turn last came back with:
/// asked after that turn began (D-084).
fn plan_decision<'a>(f: &'a Task, step: &StepId) -> Option<&'a DecisionRequest> {
    let since = f
        .runs
        .iter()
        .rev()
        .find(|r| &r.step_id == step && r.state == RunState::Completed)?
        .started_at;
    f.decisions.iter().rev().find(|d| {
        d.kind == DecisionKind::PlanApproval
            && d.run_id.is_none()
            && d.step_id.as_ref() == Some(step)
            && d.created_at > since
    })
}

/// A duration for people: seconds while short, then minutes.
fn span(d: Duration) -> String {
    match d.as_secs() {
        s if s < 120 => format!("{s} s"),
        s => format!("{} min", s / 60),
    }
}

/// How long the check command may take.
fn check_timeout() -> Duration {
    env_ms("OTTER_CHECK_TIMEOUT_MS", 10 * 60 * 1000)
}

/// What the controller remembers between steps (in memory: losing it on a
/// restart only means asking the brain again).
#[derive(Default)]
pub(crate) struct ControllerState {
    /// Tasks being stepped right now.
    inflight: HashSet<TaskId>,
    /// Decisions the brain has already been asked about.
    asked: HashSet<DecisionId>,
    /// When each task's CI was last read.
    ci_polled: std::collections::HashMap<TaskId, std::time::Instant>,
    /// A turn in Otter's conversation on the host is running (D-093).
    host_turn: bool,
}

impl Daemon {
    /// Run the controller until the daemon stops.
    pub(crate) async fn run_controller(self: Arc<Self>) {
        let mut shutdown = self.shutdown_signal();
        let mut clocks = (std::time::Instant::now(), Utc::now());
        loop {
            tokio::select! {
                _ = self.task_wake.notified() => {}
                _ = tokio::time::sleep(tick()) => {}
                _ = shutdown.changed() => return,
            }
            // The monotonic clock stands still while the host sleeps; the
            // wall clock doesn't.
            let now = (std::time::Instant::now(), Utc::now());
            let wall = (now.1 - clocks.1).to_std().unwrap_or_default();
            let gap = wall.saturating_sub(now.0 - clocks.0);
            clocks = now;
            if gap > SLEEP_GAP {
                self.skip_sleep(gap).await;
            }
            // Tasks that just ended put their own workspaces away (D-105),
            // whatever drives the rest.
            let finished = std::mem::take(&mut *self.finished.lock().unwrap());
            for id in finished {
                let daemon = self.clone();
                tokio::spawn(async move { daemon.put_away_workspace(&id).await });
            }
            // Off: tasks stay where they are until the developer acts.
            if self.brain_choice().controller.as_deref() == Some("off") {
                continue;
            }
            let ids: Vec<TaskId> = self
                .tasks
                .lock()
                .await
                .list()
                .into_iter()
                .filter(|f| {
                    !unanswered(f).is_empty()
                        || f.status.is_active()
                        || f.status == TaskStatus::Review
                        // Blocked: on a decision the brain hasn't seen yet,
                        // or on nothing any more (see `unblock`).
                        || (f.status == TaskStatus::Blocked && f.workspace_id.is_some())
                })
                .map(|f| f.id)
                .collect();
            for id in ids {
                if !self.controller.lock().unwrap().inflight.insert(id.clone()) {
                    continue;
                }
                let daemon = self.clone();
                tokio::spawn(async move {
                    let stepped = {
                        let (daemon, id) = (daemon.clone(), id.clone());
                        contained(async move { daemon.step(&id).await }).await
                    };
                    if let Some(Err(e)) = stepped {
                        tracing::warn!(task = %id, "controller: {}", e.message);
                        if e.message == PANICKED {
                            daemon.could_not_answer(&id).await;
                        }
                        daemon
                            .fail(&id, &format!("Otter stopped: {}", e.message), None)
                            .await;
                    }
                    daemon.controller.lock().unwrap().inflight.remove(&id);
                });
            }
            // Otter's conversation on the host (D-093): one turn at a time —
            // the developer's messages first, then follow-ups (D-094).
            let waiting = !crate::chat::unanswered(self.chat.lock().await.get()).is_empty();
            let busy = self.controller.lock().unwrap().host_turn;
            if !waiting
                && !busy
                && let Some((task, key)) = self.due_follow_up().await
            {
                self.controller.lock().unwrap().host_turn = true;
                let daemon = self.clone();
                tokio::spawn(async move {
                    let done = {
                        let (daemon, task, key) = (daemon.clone(), task.clone(), key.clone());
                        contained(async move { daemon.follow_up(task, key).await }).await
                    };
                    // Said plainly instead, or it would come up again on
                    // every tick (D-134).
                    if matches!(&done, Some(Err(e)) if e.message == PANICKED) {
                        let text = format!("Otter couldn't follow up on [{task}]: {PANICKED}.");
                        let _ = daemon.record_follow_up(&task, &key, text).await;
                    }
                    daemon.controller.lock().unwrap().host_turn = false;
                    match done {
                        Some(Ok(())) => daemon.task_wake.notify_one(),
                        Some(Err(e)) => tracing::warn!("Otter's follow-up: {}", e.message),
                        None => {}
                    }
                });
            }
            if waiting && !std::mem::replace(&mut self.controller.lock().unwrap().host_turn, true) {
                let daemon = self.clone();
                tokio::spawn(async move {
                    let answered = {
                        let daemon = daemon.clone();
                        contained(async move { daemon.host_turn().await }).await
                    };
                    // Answered with that, or it would come up again on every
                    // tick (D-134).
                    if matches!(&answered, Some(Err(e)) if e.message == PANICKED) {
                        let chat = daemon.chat.lock().await.get().clone();
                        if let Some(last) = crate::chat::unanswered(&chat).pop() {
                            let text = format!("I couldn't answer that: {PANICKED}.");
                            let _ = daemon.chat_reply(&last, text).await;
                        }
                    }
                    daemon.controller.lock().unwrap().host_turn = false;
                    match answered {
                        // What came in meanwhile is answered next.
                        Some(Ok(())) => daemon.task_wake.notify_one(),
                        // Tried again on the next tick, not at once.
                        Some(Err(e)) => tracing::warn!("Otter's turn on the host: {}", e.message),
                        None => {}
                    }
                });
            }
        }
    }

    async fn step(self: &Arc<Self>, id: &TaskId) -> RpcResult<()> {
        let mut f = self.task_get(id.as_str()).await?;
        // The developer said something: answer first (it may pause or go on)
        // — with Otter's tools where it has them (D-089).
        if !unanswered(&f).is_empty() {
            match self.turn_backend() {
                Some(backend) => self.otter_turn(&f, backend).await?,
                None => self.converse(&f).await?,
            }
            f = self.task_get(id.as_str()).await?;
        }
        // Unattended work doesn't wait for ever on the developer (D-083).
        if self.waited_too_long(&f).await? {
            return Ok(());
        }
        match f.status {
            TaskStatus::Planning => self.plan_step(&f).await,
            TaskStatus::Implementing => self.implement_step(&f).await,
            TaskStatus::Verifying => self.verify_step(&f).await,
            TaskStatus::Review => self.review_step(&f).await,
            TaskStatus::Blocked => {
                if f.pending_decisions().next().is_some() {
                    self.decide_open(&f).await
                } else {
                    self.unblock(&f).await
                }
            }
            _ => Ok(()),
        }
    }

    /// A triggered task whose developer-only decision has waited past its
    /// trigger's limit is paused (D-083): its agent stops, it says why, and
    /// resuming asks again. Nothing is decided in the developer's name.
    async fn waited_too_long(self: &Arc<Self>, f: &Task) -> RpcResult<bool> {
        let Some(limit) = f.started_by.as_ref().and_then(|s| s.policy.wait_limit_secs) else {
            return Ok(false);
        };
        let now = Utc::now();
        let Some(d) = f
            .pending_decisions()
            .filter(|d| d.user_only)
            .min_by_key(|d| d.created_at)
            .filter(|d| now - d.created_at > chrono::Duration::seconds(limit as i64))
        else {
            return Ok(false);
        };
        if !f.status.can_become(TaskStatus::Paused) {
            return Ok(false);
        }
        let mins = (now - d.created_at).num_minutes().max(1);
        let why = format!(
            "{}{mins} min for you on “{}”: paused, nothing decided. Resume to be asked again.",
            otter_core::trigger::WAITED,
            d.summary
        );
        self.hold_runs(&f.id, false, "Paused: waited too long for the developer")
            .await;
        let did = d.id.clone();
        let applied = self.tasks.lock().await.apply(&f.id, None, |f, now| {
            let mut changes = Vec::new();
            // What it waited on goes with the run (a decision of its own, like
            // publishing, is cancelled here): asked again on resume.
            if let Some(d) = f.decision_mut(&did)
                && d.status == DecisionStatus::Pending
            {
                d.status = DecisionStatus::Cancelled;
                d.decided_at = Some(now);
            }
            changes.push(Change::new(f.reason(why.clone())));
            if f.status.can_become(TaskStatus::Paused) {
                changes.push(Change::new(
                    f.transition(TaskStatus::Paused, Some(why.clone()), now)
                        .map_err(RpcError::conflict)?,
                ));
            }
            Ok(changes)
        })?;
        self.task_changed(&applied);
        Ok(true)
    }

    /// Answer the developer's latest messages (D-049), and act on what they
    /// ask: go on (resume, unblock, start, retry) or pause. Instructions for
    /// the work reach the coding agent with its next turn.
    async fn converse(self: &Arc<Self>, f: &Task) -> RpcResult<()> {
        let open = unanswered(f);
        let Some(last) = open.last().cloned() else {
            return Ok(());
        };
        let text = open
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let stream_id = format!("reply-{}", last.id);
        let reply = match self.task_workspace(f).await {
            Ok((root, env)) => {
                let brain = brain::select(&env, &self.brain_choice());
                // Streamed to the app as it's written (D-051).
                let draft = std::sync::Mutex::new(crate::runs::Draft::new(stream_id.clone()));
                let say = |piece: &str| {
                    let mut d = draft.lock().unwrap();
                    if let Some(so_far) = d.push(piece) {
                        self.stream(&f.id, &d.id, MessageRole::Controller, so_far, false);
                    }
                };
                brain
                    .reply(
                        &Context {
                            task: f,
                            root: &root,
                            env: &env,
                        },
                        &text,
                        &self.memories(Some(&f.id)),
                        &say,
                    )
                    .await
            }
            Err(e) => Err(anyhow::anyhow!(e.message)),
        };
        let reply = reply.unwrap_or_else(|e| {
            tracing::warn!(task = %f.id, "replying: {e:#}");
            crate::brain::Reply {
                text: crate::brain::status_text(f),
                intent: crate::brain::Intent::None,
            }
        });
        let plan_asked = plan_question(f);
        let intent = effective_intent(reply.intent, f.status, plan_asked.is_some());
        let reply_text = if reply.text.is_empty() {
            crate::brain::status_text(f)
        } else {
            reply.text
        };
        let mut stop = false;
        let applied = self.tasks.lock().await.apply(&f.id, None, |f, now| {
            let (m, c) = message(
                MessageRole::Controller,
                reply_text.clone(),
                Some(last.id.to_string()),
            );
            f.messages.push(m);
            let mut changes = vec![c];
            let (step, stops) = intent_step(f, intent, last.id.as_str(), now);
            stop = stops;
            changes.extend(step);
            Ok(changes)
        })?;
        self.task_changed(&applied);
        self.stream(
            &f.id,
            &stream_id,
            MessageRole::Controller,
            reply_text.clone(),
            true,
        );
        self.after_intent(f, intent, &open, &text, stop, plan_asked)
            .await
    }

    /// What follows an intent once the task has changed (`converse`, and
    /// Otter's tools, D-089). What reaches the coding agent (D-059):
    /// guidance as turns of their own (each message's id is its command id,
    /// so it is queued once); a redirect stops the current turn first, with
    /// the last message's id and `redirect` as its text; a question or chat
    /// doesn't go at all. Work that stopped is held; going on while the plan
    /// question waits approves the plan, as the developer said.
    pub(crate) async fn after_intent(
        self: &Arc<Self>,
        f: &Task,
        intent: crate::brain::Intent,
        messages: &[otter_core::task::Message],
        redirect: &str,
        stop: bool,
        plan_asked: Option<DecisionId>,
    ) -> RpcResult<()> {
        use crate::brain::Intent;
        match routing(intent) {
            Routing::Queue => {
                let carried = self.carried(f);
                for m in messages.iter().filter(|m| !carried.contains(m.id.as_str())) {
                    self.tell_runs(&f.id, m.id.as_str(), &m.text).await;
                }
            }
            Routing::Redirect => {
                if let Some(last) = messages.last() {
                    self.redirect_runs(&f.id, last.id.as_str(), redirect).await;
                }
            }
            Routing::Nothing => {}
        }
        if stop {
            self.hold_runs(&f.id, false, "Paused").await;
        }
        if let (Intent::Continue, Some(did)) = (intent, plan_asked) {
            self.decide(
                &f.id,
                &did,
                true,
                None,
                Decider::User,
                Some("You said go on".into()),
            )
            .await?;
        }
        Ok(())
    }

    /// A task blocked on nothing that is still open (a decision that
    /// became moot, an interrupted run) picks up where its work stands.
    async fn unblock(&self, f: &Task) -> RpcResult<()> {
        self.controller_apply(&f.id, TaskStatus::Blocked, |f, now| {
            if f.workspace_id.is_none() || f.pending_decisions().next().is_some() {
                return Ok(vec![]);
            }
            let next = crate::tasks::resume_status(f);
            Ok(vec![Change::new(
                f.transition(next, Some("Nothing is waiting on you any more".into()), now)
                    .map_err(RpcError::conflict)?,
            )])
        })
        .await?;
        Ok(())
    }

    /// How the Control Agent thinks on this host (settings + environment).
    pub(crate) fn brain_choice(&self) -> brain::Choice {
        let s = self.settings.read().unwrap();
        brain::Choice {
            controller: s.controller(),
            model: s.model(),
            keys: crate::providers::PROVIDERS
                .iter()
                .filter_map(|p| Some((p.key.to_owned(), s.secret(p.key)?)))
                .collect(),
            claude_token: s.secret(crate::agents::claude::OAUTH_TOKEN),
            v2: s.otter_tools(),
        }
    }

    /// The workspace root and environment the task works in.
    pub(crate) async fn task_workspace(&self, f: &Task) -> RpcResult<(PathBuf, EnvMap)> {
        let ws_id = f
            .workspace_id
            .clone()
            .ok_or_else(|| RpcError::conflict("the task has no workspace"))?;
        let ws = self
            .store
            .lock()
            .await
            .workspace(ws_id.as_str())
            .cloned()
            .ok_or_else(|| workspace_not_found(ws_id.as_str()))?;
        let env = self.env_for_launch(&ws).await?;
        Ok((PathBuf::from(&ws.root), env))
    }

    /// Change a task on behalf of the controller, if it is still `expect`.
    async fn controller_apply(
        &self,
        id: &TaskId,
        expect: TaskStatus,
        step: impl FnOnce(&mut Task, chrono::DateTime<Utc>) -> RpcResult<Vec<Change>>,
    ) -> RpcResult<bool> {
        let mut moved = false;
        let applied = self.tasks.lock().await.apply(id, None, |f, now| {
            // The developer may have paused or cancelled meanwhile.
            if f.status != expect {
                moved = true;
                return Ok(vec![]);
            }
            step(f, now)
        })?;
        self.task_changed(&applied);
        Ok(!moved)
    }

    /// Stop the task with a reason (a limit, or an error), keeping the plan.
    pub(crate) async fn fail(&self, id: &TaskId, reason: &str, limit: Option<&str>) {
        let reason = reason.to_owned();
        let limit = limit.map(String::from);
        let applied = self.tasks.lock().await.apply(id, None, |f, now| {
            if !f.status.can_become(TaskStatus::Failed) {
                return Ok(vec![]);
            }
            let mut changes = Vec::new();
            if let Some(l) = limit {
                changes.push(Change::new(TaskEvent::LimitReached { limit: l }));
            }
            changes.push(Change::new(
                f.transition(TaskStatus::Failed, Some(reason.clone()), now)
                    .map_err(RpcError::conflict)?,
            ));
            let (m, c) = message(MessageRole::Controller, reason, None);
            f.messages.push(m);
            changes.push(c);
            Ok(changes)
        });
        match applied {
            // Why, in the log too: the task's history goes when it's
            // deleted (D-134).
            Ok(a) if a.from != Some(a.task.status) => {
                let why = a.task.status_reason.as_deref().unwrap_or_default();
                tracing::warn!(task = %id, "task failed: {}", otter_core::redact::redact(why));
                self.task_changed(&a);
            }
            // Nothing changed: nothing to tell, nor to step again.
            Ok(_) => {}
            Err(e) => tracing::warn!(task = %id, "failing: {e:?}"),
        }
    }

    /// What the developer said, answered if only by saying Otter couldn't:
    /// a step that panicked on it would take it up again at once, and
    /// again (D-134).
    async fn could_not_answer(&self, id: &TaskId) {
        if self
            .task_get(id.as_str())
            .await
            .is_ok_and(|f| unanswered(&f).is_empty())
        {
            return;
        }
        let applied = self.tasks.lock().await.apply(id, None, |f, _| {
            let Some(last) = unanswered(f).pop() else {
                return Ok(vec![]);
            };
            let (m, c) = message(
                MessageRole::Controller,
                format!("Otter couldn't answer that: {PANICKED}."),
                Some(last.id.to_string()),
            );
            f.messages.push(m);
            Ok(vec![c])
        });
        if let Ok(a) = applied {
            self.task_changed(&a);
        }
    }

    async fn plan_step(self: &Arc<Self>, f: &Task) -> RpcResult<()> {
        if f.workspace_id.is_none() {
            self.controller_apply(&f.id, TaskStatus::Planning, |f, now| {
                let e = f
                    .transition(
                        TaskStatus::Blocked,
                        Some("Choose a workspace for this task".into()),
                        now,
                    )
                    .map_err(RpcError::conflict)?;
                Ok(vec![Change::new(e)])
            })
            .await?;
            return Ok(());
        }
        if f.steps.iter().any(|t| !t.planning) && !f.replan {
            // Back from a pause before any plan step was lost: carry on.
            self.controller_apply(&f.id, TaskStatus::Planning, |f, now| {
                let next = crate::tasks::resume_status(f);
                Ok(vec![Change::new(
                    f.transition(next, None, now).map_err(RpcError::conflict)?,
                )])
            })
            .await?;
            return Ok(());
        }
        // Nothing to do yet (D-067): the developer's first message is the
        // request, or the answer to Otter asking for one.
        if f.request.trim().is_empty() {
            return self.find_the_goal(f).await;
        }
        // The coding agent looks at the code first (D-064) and either
        // answers or proposes a plan, which the brain reviews (D-084). So
        // does planning again: the agent plans what is left.
        if plan_turn() {
            if self.supervise(f).await? {
                return Ok(());
            }
            match f.steps.iter().find(|t| t.planning) {
                None => {
                    self.controller_apply(&f.id, TaskStatus::Planning, |f, _| {
                        let step_id = StepId::generate();
                        f.steps.push(Step {
                            id: step_id.clone(),
                            title: PLANNING.into(),
                            detail: None,
                            status: StepStatus::Pending,
                            depends_on: vec![],
                            workspace_id: f.workspace_id.clone(),
                            session_id: None,
                            attempts: 0,
                            last_error: None,
                            planning: true,
                        });
                        Ok(vec![Change::new(TaskEvent::StepChanged {
                            step_id,
                            status: StepStatus::Pending,
                        })])
                    })
                    .await?;
                    return Ok(()); // The change wakes the next step.
                }
                Some(t) if matches!(t.status, StepStatus::Pending | StepStatus::Running) => {
                    return self.start_turn(f, t).await;
                }
                // It failed: say so, and don't plan blind (D-084). Going on
                // runs it again.
                Some(t) if t.status == StepStatus::Failed => {
                    let why = t.last_error.clone().unwrap_or_else(|| "it failed".into());
                    self.fail(&f.id, &looked_and_failed(&why), None).await;
                    return Ok(());
                }
                Some(t) if t.status == StepStatus::Done => {
                    let t = t.clone();
                    return self.after_the_look(f, &t).await;
                }
                Some(_) => {}
            }
        }
        let (root, env) = self.task_workspace(f).await?;
        let brain = brain::select(&env, &self.brain_choice());
        let plan = brain
            .plan(&Context {
                task: f,
                root: &root,
                env: &env,
            })
            .await
            .map_err(|e| RpcError::internal(format!("planning failed: {e:#}")))?;
        self.set_plan(f, plan, brain.name(), &root).await
    }

    /// The coding agent's look at the code is over (D-084). A plan in its
    /// report is reviewed — the brain's points go back to the agent once,
    /// and to the developer after that — and set. Anything else is its
    /// answer: the request asked no change, and the task is done.
    async fn after_the_look(self: &Arc<Self>, f: &Task, step: &Step) -> RpcResult<()> {
        let report = brain::proposal(f).unwrap_or_default().trim().to_owned();
        let Some(proposed) = brain::parse_proposal(&report) else {
            return self.answered(f, step, report).await;
        };
        let (root, env) = self.task_workspace(f).await?;
        let plan = brain::complete_proposal(proposed, f, &root);
        // Only an answer was wanted (D-092): a plan to change things is the
        // developer's to allow.
        if f.answer_only {
            return self.plan_for_an_answer(f, step, plan, &root).await;
        }
        // What the developer said about this plan, if they were asked.
        match plan_decision(f, &step.id) {
            Some(d) if d.status == DecisionStatus::Pending => return Ok(()),
            Some(d)
                if matches!(
                    d.status,
                    DecisionStatus::Approved | DecisionStatus::Answered
                ) =>
            {
                return self.set_plan(f, plan, "approved by you", &root).await;
            }
            Some(d) if d.status == DecisionStatus::Denied => {
                let issues = d.detail.clone().unwrap_or_default();
                return self
                    .send_plan_back(
                        f,
                        step,
                        format!("{issues}\n- The developer turned this plan down."),
                    )
                    .await;
            }
            _ => {}
        }
        let brain = brain::select(&env, &self.brain_choice());
        let cx = Context {
            task: f,
            root: &root,
            env: &env,
        };
        let issues = match brain.review_plan(&cx, &plan).await {
            Ok(issues) => issues,
            // The agent's plan stands: it is the one that saw the code.
            Err(e) => {
                tracing::warn!(task = %f.id, "reviewing the plan: {e:#}");
                vec![]
            }
        };
        let by = match brain.name() {
            "rules" => "no model to review it".to_owned(),
            name => format!("reviewed by {name}"),
        };
        if issues.is_empty() {
            return self.set_plan(f, plan, &by, &root).await;
        }
        let issues: String = issues.iter().map(|i| format!("- {i}\n")).collect();
        // Sent back once (its detail says what for); then it's the developer's.
        if step.detail.is_none() {
            self.send_plan_back(f, step, issues).await
        } else {
            self.ask_about_the_plan(f, step, &plan, issues).await
        }
    }

    /// The agent proposes changes to a task that only wanted an answer
    /// (D-092). Asked first; allowed, the task may change code and the plan
    /// is set; turned down, the plan is the answer and the task is done.
    async fn plan_for_an_answer(
        &self,
        f: &Task,
        step: &Step,
        plan: brain::Plan,
        root: &std::path::Path,
    ) -> RpcResult<()> {
        match plan_decision(f, &step.id) {
            Some(d) if d.status == DecisionStatus::Pending => Ok(()),
            Some(d)
                if matches!(
                    d.status,
                    DecisionStatus::Approved | DecisionStatus::Answered
                ) =>
            {
                self.controller_apply(&f.id, TaskStatus::Planning, |f, _| {
                    f.answer_only = false;
                    Ok(vec![Change::new(f.reason(
                        "You let it change the code: the task is no longer answer-only.",
                    ))])
                })
                .await?;
                self.set_plan(f, plan, "approved by you", root).await
            }
            Some(d) if d.status == DecisionStatus::Denied => {
                let answer = format!(
                    "{}\n\nNot carried out: this task only answers.",
                    plan_text(&plan)
                );
                self.answered(f, step, answer).await
            }
            _ => {
                let step_id = step.id.clone();
                let detail =
                    "This task only answers, and the coding agent proposes changing the code.";
                let text = format!(
                    "This task only answers, but the coding agent proposes changes:\n{}\nApprove to let it change the code; turn it down and this plan stands as the answer.",
                    plan_text(&plan)
                );
                self.controller_apply(&f.id, TaskStatus::Planning, move |f, now| {
                    let id = DecisionId::generate();
                    f.decisions.push(DecisionRequest {
                        id: id.clone(),
                        step_id: Some(step_id),
                        run_id: None,
                        kind: DecisionKind::PlanApproval,
                        risk: Risk::Medium,
                        summary: PLAN_QUESTION.into(),
                        detail: Some(detail.into()),
                        options: vec![],
                        status: DecisionStatus::Pending,
                        decided_by: None,
                        answer: None,
                        rationale: None,
                        created_at: now,
                        decided_at: None,
                        user_only: true,
                        grant: None,
                    });
                    let (m, c) = message(MessageRole::Controller, text, Some(id.to_string()));
                    f.messages.push(m);
                    Ok(vec![
                        Change::new(TaskEvent::DecisionRequested {
                            decision_id: id,
                            kind: DecisionKind::PlanApproval,
                            risk: Risk::Medium,
                        }),
                        c,
                        Change::new(
                            f.transition(TaskStatus::Blocked, Some(PLAN_QUESTION.into()), now)
                                .map_err(RpcError::conflict)?,
                        ),
                    ])
                })
                .await?;
                Ok(())
            }
        }
    }

    /// The planning turn's report was no plan: it is the answer (D-084).
    async fn answered(&self, f: &Task, step: &Step, report: String) -> RpcResult<()> {
        let id = step.id.clone();
        if report.is_empty() {
            let why = "it ended without a plan or an answer";
            self.controller_apply(&f.id, TaskStatus::Planning, |f, _| {
                let t = f.step_mut(&id).expect("found");
                t.status = StepStatus::Failed;
                t.last_error = Some(why.into());
                Ok(vec![Change::new(TaskEvent::StepChanged {
                    step_id: id.clone(),
                    status: StepStatus::Failed,
                })])
            })
            .await?;
            self.fail(&f.id, &looked_and_failed(why), None).await;
            return Ok(());
        }
        let run = f
            .runs
            .iter()
            .rev()
            .find(|r| r.step_id == id && r.state == RunState::Completed)
            .map(|r| r.id.to_string());
        self.controller_apply(&f.id, TaskStatus::Planning, move |f, now| {
            let (m, c) = message(MessageRole::Agent, report, run);
            f.messages.push(m);
            let why = f.reason(
                "The coding agent answered: the request asks no change, so nothing was planned or built.",
            );
            Ok(vec![
                c,
                Change::new(why),
                Change::new(
                    f.transition(TaskStatus::Done, Some(ANSWERED.into()), now)
                        .map_err(RpcError::conflict)?,
                ),
            ])
        })
        .await?;
        Ok(())
    }

    /// The planning turn again, with what is wrong with its plan (D-084).
    async fn send_plan_back(&self, f: &Task, step: &Step, issues: String) -> RpcResult<()> {
        let id = step.id.clone();
        self.controller_apply(&f.id, TaskStatus::Planning, move |f, _| {
            let why = f.reason(format!(
                "Sent the plan back to the coding agent:\n{}",
                issues.trim_end()
            ));
            let t = f.step_mut(&id).expect("found");
            t.status = StepStatus::Pending;
            t.detail = Some(issues);
            Ok(vec![
                Change::new(why),
                Change::new(TaskEvent::StepChanged {
                    step_id: id.clone(),
                    status: StepStatus::Pending,
                }),
            ])
        })
        .await?;
        Ok(())
    }

    /// A revised plan the brain still finds fault with: the developer says
    /// whether it goes ahead (D-084). Nothing is set until they do.
    async fn ask_about_the_plan(
        &self,
        f: &Task,
        step: &Step,
        plan: &brain::Plan,
        issues: String,
    ) -> RpcResult<()> {
        let step_id = step.id.clone();
        let mut text = format!(
            "I sent the coding agent's plan back once, and the revised plan still has problems I can see:\n{issues}\nThe plan:\n{}\nAcceptance criteria:\n{}",
            plan.steps
                .iter()
                .enumerate()
                .map(|(i, t)| format!("{}. {}", i + 1, t.title))
                .collect::<Vec<_>>()
                .join("\n"),
            plan.criteria
                .iter()
                .map(|c| format!("- {}", c.text))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        if let Some(cmd) = &plan.verify_command {
            text.push_str(&format!("\nThe check: `{cmd}`."));
        }
        text.push_str(
            "\nApprove it to go ahead as it is, turn it down to have it planned again, or tell me what to change.",
        );
        self.controller_apply(&f.id, TaskStatus::Planning, move |f, now| {
            let id = DecisionId::generate();
            f.decisions.push(DecisionRequest {
                id: id.clone(),
                step_id: Some(step_id),
                run_id: None,
                kind: DecisionKind::PlanApproval,
                risk: Risk::Low,
                summary: PLAN_QUESTION.into(),
                detail: Some(issues),
                options: vec![],
                status: DecisionStatus::Pending,
                decided_by: None,
                answer: None,
                rationale: None,
                created_at: now,
                decided_at: None,
                user_only: true,
                grant: None,
            });
            let (m, c) = message(MessageRole::Controller, text, Some(id.to_string()));
            f.messages.push(m);
            Ok(vec![
                Change::new(TaskEvent::DecisionRequested {
                    decision_id: id,
                    kind: DecisionKind::PlanApproval,
                    risk: Risk::Low,
                }),
                c,
                Change::new(
                    f.transition(TaskStatus::Blocked, Some(PLAN_QUESTION.into()), now)
                        .map_err(RpcError::conflict)?,
                ),
            ])
        })
        .await?;
        Ok(())
    }

    /// Set `plan` as the task's plan, `by` whom it says.
    async fn set_plan(
        &self,
        f: &Task,
        plan: brain::Plan,
        by: &str,
        root: &std::path::Path,
    ) -> RpcResult<()> {
        let verify = plan.verify_command.clone().filter(|c| !c.trim().is_empty());
        let check_verdict = verify
            .as_ref()
            .map(|c| policy::classify(&ToolCall::Command { line: c.clone() }, root));
        let by = by.to_owned();
        self.controller_apply(&f.id, TaskStatus::Planning, move |f, now| {
            f.requirements = plan.requirements.clone();
            f.acceptance = plan
                .criteria
                .iter()
                .enumerate()
                .map(|(i, c)| Criterion {
                    id: format!("ac_{}", i + 1),
                    text: c.text.clone(),
                    met: None,
                    evidence: vec![],
                    how: c.how,
                })
                .collect();
            // Planning again (D-050): what is done stays; the rest is replaced.
            let revised = f.replan;
            let kept: Vec<Step> = f
                .steps
                .iter()
                .filter(|t| matches!(t.status, StepStatus::Done | StepStatus::Skipped))
                .cloned()
                .collect();
            f.replan = false;
            let ids: Vec<StepId> = plan.steps.iter().map(|_| StepId::generate()).collect();
            let fresh: Vec<Step> = plan
                .steps
                .iter()
                .enumerate()
                .map(|(i, t)| Step {
                    id: ids[i].clone(),
                    title: t.title.clone(),
                    detail: Some(t.detail.clone()).filter(|d| !d.is_empty()),
                    status: StepStatus::Pending,
                    depends_on: t
                        .depends_on
                        .iter()
                        .filter(|&&d| d < i)
                        .map(|&d| ids[d].clone())
                        .collect(),
                    workspace_id: f.workspace_id.clone(),
                    session_id: None,
                    attempts: 0,
                    last_error: None,
                    planning: false,
                })
                .collect();
            f.steps = kept.into_iter().chain(fresh).collect();
            f.verify_command = verify.clone();
            let why = f.reason(format!("Plan ({by}): {}", plan.rationale));
            let mut changes = vec![
                Change::new(TaskEvent::PlanSet {
                    steps: f.steps.len(),
                    criteria: f.acceptance.len(),
                }),
                Change::new(why),
            ];
            // The check runs without asking only if policy says so.
            match (&verify, &check_verdict) {
                (Some(_), Some(Verdict::Allow)) => f.verify_approved = true,
                (
                    Some(cmd),
                    Some(Verdict::Ask {
                        risk,
                        user_only,
                        why,
                        ..
                    }),
                ) => {
                    let id = DecisionId::generate();
                    f.decisions.push(DecisionRequest {
                        id: id.clone(),
                        step_id: None,
                        run_id: None,
                        kind: DecisionKind::ToolPermission,
                        risk: *risk,
                        summary: format!("Run `{cmd}` to check the work"),
                        detail: Some(why.clone()),
                        options: vec![],
                        status: DecisionStatus::Pending,
                        decided_by: None,
                        answer: None,
                        rationale: None,
                        created_at: now,
                        decided_at: None,
                        user_only: *user_only,
                        grant: None,
                    });
                    f.verify_decision = Some(id.clone());
                    changes.push(Change::new(TaskEvent::DecisionRequested {
                        decision_id: id,
                        kind: DecisionKind::ToolPermission,
                        risk: *risk,
                    }));
                }
                (Some(_), Some(Verdict::Deny { .. })) => f.verify_command = None,
                _ => {}
            }
            // The work, not the look at the code that planned it.
            let work: Vec<&Step> = f.steps.iter().filter(|t| !t.planning).collect();
            let mut text = format!(
                "{}: {} step(s).\n{}\nAcceptance criteria:\n{}",
                if revised { "Revised plan" } else { "Plan" },
                work.len(),
                work.iter()
                    .enumerate()
                    .map(|(i, t)| format!("{}. {}", i + 1, t.title))
                    .collect::<Vec<_>>()
                    .join("\n"),
                f.acceptance
                    .iter()
                    .map(|c| format!("- {}", c.text))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            if let Some(cmd) = &f.verify_command {
                text.push_str(&format!("\nI'll check the work with `{cmd}`."));
            }
            let (m, c) = message(MessageRole::Controller, text, None);
            f.messages.push(m);
            changes.push(c);
            changes.push(Change::new(
                f.transition(TaskStatus::Implementing, None, now)
                    .map_err(RpcError::conflict)?,
            ));
            Ok(changes)
        })
        .await?;
        Ok(())
    }

    /// Ask the brain about open decisions; escalate what it won't take.
    /// A task started with no request: take the developer's first
    /// message, or their answer to Otter's question; else ask, once, and
    /// wait (D-067). Planning doesn't start without a goal.
    async fn find_the_goal(self: &Arc<Self>, f: &Task) -> RpcResult<()> {
        let said = f
            .messages
            .iter()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.text.clone());
        let answered = f
            .decisions
            .iter()
            .rev()
            .find(|d| d.summary == GOAL_QUESTION && d.status == DecisionStatus::Answered)
            .and_then(|d| d.answer.clone());
        if let Some(goal) = said.or(answered).filter(|g| !g.trim().is_empty()) {
            self.controller_apply(&f.id, TaskStatus::Planning, move |f, _| {
                f.request = goal.trim().to_owned();
                Ok(vec![Change::new(f.reason(
                    "The request is what you said: planning from it.",
                ))])
            })
            .await?;
            return Ok(()); // The change wakes the next step.
        }
        if f.pending_decisions().any(|d| d.summary == GOAL_QUESTION) {
            return Ok(());
        }
        self.controller_apply(&f.id, TaskStatus::Planning, |f, now| {
            let id = DecisionId::generate();
            f.decisions.push(DecisionRequest {
                id: id.clone(),
                step_id: None,
                run_id: None,
                kind: DecisionKind::Question,
                risk: Risk::Low,
                summary: GOAL_QUESTION.into(),
                detail: Some("The task was started without a request.".into()),
                options: vec![],
                status: DecisionStatus::Pending,
                decided_by: None,
                answer: None,
                rationale: None,
                created_at: now,
                decided_at: None,
                user_only: true,
                grant: None,
            });
            let (m, c) = message(
                MessageRole::Controller,
                format!(
                    "{GOAL_QUESTION} Answer here, or just tell me in a message, and I'll plan it."
                ),
                Some(id.to_string()),
            );
            f.messages.push(m);
            Ok(vec![
                Change::new(TaskEvent::DecisionRequested {
                    decision_id: id,
                    kind: DecisionKind::Question,
                    risk: Risk::Low,
                }),
                c,
                Change::new(
                    f.transition(
                        TaskStatus::Blocked,
                        Some("Waiting for what this task should do".into()),
                        now,
                    )
                    .map_err(RpcError::conflict)?,
                ),
            ])
        })
        .await?;
        Ok(())
    }

    async fn decide_open(self: &Arc<Self>, f: &Task) -> RpcResult<()> {
        // Asked what the task should do: a message is an answer too.
        if let Some(d) = f.pending_decisions().find(|d| d.summary == GOAL_QUESTION)
            && let Some(m) = f
                .messages
                .iter()
                .find(|m| m.role == MessageRole::User && m.at > d.created_at)
        {
            let (did, text) = (d.id.clone(), m.text.clone());
            self.decide(&f.id, &did, true, Some(text), Decider::User, None)
                .await?;
            return Ok(());
        }
        let open: Vec<DecisionRequest> = {
            let mut st = self.controller.lock().unwrap();
            f.pending_decisions()
                .filter(|d| st.asked.insert(d.id.clone()))
                .cloned()
                .collect()
        };
        if open.is_empty() {
            return Ok(());
        }
        // Developer-only: no model sees it, approves it or denies it.
        let (open, theirs): (Vec<_>, Vec<_>) = open.into_iter().partition(|d| !d.user_only);
        for d in theirs {
            self.escalate(&f.id, &d, "only the developer may approve or answer this")
                .await;
        }
        if open.is_empty() {
            return Ok(());
        }
        let (root, env) = self.task_workspace(f).await?;
        let brain = brain::select(&env, &self.brain_choice());
        for d in open {
            // Whether to start cut-off work over is the developer's (D-061).
            if d.kind == DecisionKind::Recovery {
                let why = "only you can say whether cut-off work starts over";
                self.escalate(&f.id, &d, why).await;
                continue;
            }
            let verdict = brain
                .decide(
                    &Context {
                        task: f,
                        root: &root,
                        env: &env,
                    },
                    &d,
                )
                .await
                .unwrap_or_else(|e| BrainVerdict::Escalate {
                    why: format!("couldn't decide: {e:#}"),
                });
            let (allow, answer, rationale) = match verdict {
                BrainVerdict::Allow { rationale } => (true, None, rationale),
                BrainVerdict::Deny { rationale } => (false, None, rationale),
                BrainVerdict::Answer { text, rationale } => (true, Some(text), rationale),
                BrainVerdict::Escalate { why } => {
                    self.escalate(&f.id, &d, &why).await;
                    continue;
                }
            };
            if let Err(e) = self
                .decide(
                    &f.id,
                    &d.id,
                    allow,
                    answer,
                    Decider::Controller,
                    Some(rationale.clone()),
                )
                .await
            {
                // Already decided or moot (the run ended meanwhile): nothing
                // to escalate. Still open: policy kept it from the model, so
                // it's the developer's.
                let still_open = self
                    .task_get(f.id.as_str())
                    .await
                    .is_ok_and(|now| now.pending_decisions().any(|x| x.id == d.id));
                if still_open {
                    self.escalate(
                        &f.id,
                        &d,
                        &format!("Otter wanted to allow it ({rationale}), but {}", e.message),
                    )
                    .await;
                }
            }
        }
        Ok(())
    }

    async fn escalate(&self, id: &TaskId, d: &DecisionRequest, why: &str) {
        let line = format!("Needs your decision: {} — {why}", d.summary);
        let did = d.id.clone();
        let applied = self.tasks.lock().await.apply(id, None, |f, now| {
            // Decided meanwhile (the run ended, the developer answered):
            // nothing to block on, which would only unblock again (D-066).
            if !f.pending_decisions().any(|x| x.id == did) {
                return Ok(vec![]);
            }
            let mut changes = vec![Change::new(f.reason(line.clone()))];
            if f.status.can_become(TaskStatus::Blocked) {
                changes.push(Change::new(
                    f.transition(TaskStatus::Blocked, Some(line.clone()), now)
                        .map_err(RpcError::conflict)?,
                ));
            }
            Ok(changes)
        });
        if let Ok(a) = applied {
            self.task_changed(&a);
        }
    }

    /// The host slept for `gap`: the runs working then didn't work.
    async fn skip_sleep(&self, gap: Duration) {
        let Ok(gap) = chrono::Duration::from_std(gap) else {
            return;
        };
        tracing::info!(
            secs = gap.num_seconds(),
            "the host slept; not counted as work"
        );
        let ids: Vec<TaskId> = self
            .tasks
            .lock()
            .await
            .list()
            .into_iter()
            .filter(|f| f.live_run().is_some_and(|r| r.working_since.is_some()))
            .map(|f| f.id)
            .collect();
        for id in ids {
            let applied = self.tasks.lock().await.apply(&id, None, |f, now| {
                for r in f.runs.iter_mut().filter(|r| !r.state.is_over()) {
                    r.skip(gap, now);
                }
                Ok(vec![])
            });
            if let Ok(a) = applied {
                self.task_changed(&a);
            }
        }
    }

    /// A run at work, or decisions open: keep the run within its limits and
    /// get the decisions made. True when that's all there is to do now.
    async fn supervise(self: &Arc<Self>, f: &Task) -> RpcResult<bool> {
        let now = Utc::now();
        if let Some(run) = f.live_run() {
            // Out of time, or silent too long: the turn is cut off, and the
            // run resumes its conversation with a word on why (D-063). Time
            // waiting on a decision isn't work, and a running tool isn't
            // silence.
            if run.state == RunState::Running {
                let limit = run_timeout();
                let idle = self.run_idle(&run.id).filter(|d| *d > idle_timeout());
                let why = if run.worked(now) > limit {
                    Some(format!(
                        "{OUT_OF_TIME} after {} of work",
                        span(limit.to_std().unwrap_or_default())
                    ))
                } else {
                    idle.map(|d| format!("{NO_PROGRESS} for {}", span(d)))
                };
                if let Some(why) = why {
                    self.stop_runs(&f.id, RunState::Cancelled, &why).await;
                }
            }
            self.decide_open(f).await?;
            return Ok(true);
        }
        if f.pending_decisions().next().is_some() {
            self.decide_open(f).await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn implement_step(self: &Arc<Self>, f: &Task) -> RpcResult<()> {
        let now = Utc::now();
        if self.supervise(f).await? {
            return Ok(());
        }
        // Budgets and loops: deterministic, whatever the brain thinks. Time
        // is the agent's work since the work last (re)started (D-063).
        if let Some(since) = f.active_since {
            let worked: chrono::Duration = f
                .runs
                .iter()
                .filter(|r| r.ended_at.is_none_or(|e| e >= since))
                .map(|r| r.worked(now))
                .sum();
            if worked > chrono::Duration::minutes(i64::from(f.budget.max_minutes)) {
                let reason = format!(
                    "Stopped: the time budget ({} min of work) is used up",
                    f.budget.max_minutes
                );
                self.fail(&f.id, &reason, Some("time")).await;
                return Ok(());
            }
        }
        if let Some(summary) = repeating_failure(f) {
            let reason =
                format!("Stopped: the same failure {LOOP_REPEATS} times in a row — {summary}");
            self.fail(&f.id, &reason, Some("loop")).await;
            return Ok(());
        }
        let done = |id: &StepId| {
            f.step(id)
                .is_some_and(|t| matches!(t.status, StepStatus::Done | StepStatus::Skipped))
        };
        let ready = |t: &&Step| t.depends_on.iter().all(done);
        let work = || f.steps.iter().filter(|t| !t.planning).filter(ready);
        let next = work()
            .find(|t| matches!(t.status, StepStatus::Pending | StepStatus::Running))
            .or_else(|| {
                work().find(|t| t.status == StepStatus::Failed && t.attempts < MAX_TASK_ATTEMPTS)
            })
            .cloned();
        let Some(step) = next else {
            if f.steps
                .iter()
                .all(|t| matches!(t.status, StepStatus::Done | StepStatus::Skipped))
            {
                self.controller_apply(&f.id, TaskStatus::Implementing, |f, now| {
                    Ok(vec![Change::new(
                        f.transition(TaskStatus::Verifying, None, now)
                            .map_err(RpcError::conflict)?,
                    )])
                })
                .await?;
            } else {
                let stuck = f
                    .steps
                    .iter()
                    .find(|t| t.status == StepStatus::Failed)
                    .map(|t| {
                        format!(
                            "“{}” failed {} times: {}",
                            t.title,
                            t.attempts,
                            t.last_error.clone().unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "no step can start".into());
                self.fail(&f.id, &format!("Stopped: {stuck}"), Some("attempts"))
                    .await;
            }
            return Ok(());
        };
        self.start_turn(f, &step).await
    }

    /// Why the task's workspace can't be worked in now, if it can't
    /// (D-079): another task's run is there, or an agent the developer
    /// started is at work there.
    async fn workspace_taken(self: &Arc<Self>, f: &Task) -> Option<String> {
        let ws_id = f.workspace_id.as_ref()?;
        let (name, busy_session) = {
            let store = self.store.lock().await;
            let ws = store.workspace(ws_id.as_str())?;
            // Put away when it ended (D-105): its own comes back; another
            // is the developer's to bring back.
            if ws.state == otter_core::WorkspaceState::Archived {
                if ws.task_id.as_ref() != Some(&f.id) {
                    return Some(format!("{WAITING}{}{ARCHIVED}", ws.name));
                }
                let name = ws.name.clone();
                drop(store);
                if let Err(e) = self.workspace_unarchive(ws_id.as_str()).await {
                    return Some(format!("{WAITING}{name}: {}", e.message));
                }
                return Some(format!("{WAITING}{name}: it's being prepared"));
            }
            // A workspace still being made (one of its own, D-080).
            if ws.state == otter_core::WorkspaceState::Preparing {
                return Some(format!("{WAITING}{}: it's being prepared", ws.name));
            }
            // Its preparation failed: the developer's to fix (D-135).
            if ws.state == otter_core::WorkspaceState::Failed {
                let why = ws.state_message.as_deref().unwrap_or("no reason given");
                return Some(format!(
                    "{WAITING}{}: preparing it failed ({}); prepare it again to go on",
                    ws.name,
                    why.lines().next().unwrap_or(why)
                ));
            }
            let busy = ws
                .sessions
                .iter()
                .filter(|s| s.current_execution().is_some_and(|e| e.is_running()))
                .find(|s| {
                    s.agent.as_ref().is_some_and(|a| {
                        matches!(
                            a.state,
                            otter_core::AgentState::Working
                                | otter_core::AgentState::Starting
                                | otter_core::AgentState::Blocked
                        )
                    })
                })
                .map(|s| s.name.clone());
            (ws.name.clone(), busy)
        };
        if let Some(other) = self.workspace_holder(ws_id).filter(|o| o != &f.id) {
            let title = self
                .task_get(other.as_str())
                .await
                .map(|o| o.title)
                .unwrap_or_else(|_| other.to_string());
            return Some(format!("{WAITING}{name}: “{title}” is working there"));
        }
        busy_session.map(|s| format!("{WAITING}{name}: {s}{YOUR_AGENT}"))
    }

    /// The task waits for its workspace (D-079): said once, on the
    /// task; it starts by itself when the workspace is free.
    async fn wait_for_workspace(self: &Arc<Self>, f: &Task, why: String) -> RpcResult<()> {
        if f.status_reason.as_deref() == Some(why.as_str()) {
            return Ok(());
        }
        self.controller_apply(&f.id, f.status, move |f, _| {
            let event = f.reason(why.clone());
            f.status_reason = Some(why);
            Ok(vec![Change::new(event)])
        })
        .await?;
        Ok(())
    }

    /// Give `step` to the coding agent: a turn in the task's conversation.
    async fn start_turn(self: &Arc<Self>, f: &Task, step: &Step) -> RpcResult<()> {
        let step = step.clone();
        if let Some(why) = self.workspace_taken(f).await {
            return self.wait_for_workspace(f, why).await;
        }
        // One conversation per task (D-064): the next step, a fix and a
        // retry are turns in the session the work began in. A turn of this
        // step that was cut off (restart, a lost process, pause, a limit,
        // takeover) carries on; anything else is new work, which is what
        // costs an attempt (planning doesn't), and so is a step the developer
        // said to start over (D-061).
        let last = f.runs.iter().rev().find(|r| r.step_id == step.id);
        let continuing = last
            .filter(|r| {
                matches!(
                    r.state,
                    RunState::Cancelled | RunState::HandedOff | RunState::Interrupted
                )
            })
            .is_some_and(|r| r.provider_session_id.is_some());
        let session = self.task_session(f);
        let counts = !continuing && !step.planning;
        if counts && f.budget.iterations_used >= f.budget.max_iterations {
            let reason = format!(
                "Stopped: all {} attempts are used up",
                f.budget.max_iterations
            );
            self.fail(&f.id, &reason, Some("attempts")).await;
            return Ok(());
        }
        // Messages a turn of the conversation already carries (queued, sent
        // or delivered) aren't restated, nor what an earlier turn's prompt
        // said (anything before it started).
        let carried = session
            .as_ref()
            .map(|(_, c)| carried_by(c))
            .unwrap_or_default();
        let prompt = match (&session, continuing) {
            (Some(_), true) => continue_prompt(f, &step, last, &carried),
            (_, false) if step.planning => planning_prompt(
                f,
                &step,
                developer_notes(f, session.as_ref().map(|(r, _)| r.started_at), &carried),
                session.is_some(),
            ),
            (Some((run, _)), false) => step_prompt(
                f,
                &step,
                developer_notes(f, Some(run.started_at), &carried),
                true,
            ),
            (None, _) => step_prompt(f, &step, developer_notes(f, None, &carried), false),
        };
        let resume = session.and_then(|(run, _)| run.provider_session_id.clone());
        match self
            .start_run(&f.id, &step.id, prompt, resume, counts)
            .await
        {
            // Taken in the meantime: wait, as above.
            Err(e) if e.message.starts_with(crate::runs::BUSY) => {
                if let Some(why) = self.workspace_taken(f).await {
                    self.wait_for_workspace(f, why).await?;
                }
            }
            Err(e) => {
                self.fail(
                    &f.id,
                    &format!("Couldn't start the coding agent: {}", e.message),
                    None,
                )
                .await;
            }
            // No longer waiting.
            Ok(_)
                if f.status_reason
                    .as_deref()
                    .is_some_and(|r| r.starts_with(WAITING)) =>
            {
                let _ = self
                    .controller_apply(&f.id, f.status, |f, _| {
                        f.status_reason = None;
                        Ok(vec![])
                    })
                    .await;
            }
            Ok(_) => {}
        }
        Ok(())
    }

    async fn verify_step(self: &Arc<Self>, f: &Task) -> RpcResult<()> {
        if f.pending_decisions().next().is_some() {
            return self.decide_open(f).await;
        }
        let (root, env) = self.task_workspace(f).await?;
        // The check command: run if allowed (or asked to run again once it
        // can); a denial, or going on without it, drops it.
        if let (Some(_), false, Some(did)) =
            (&f.verify_command, f.verify_approved, &f.verify_decision)
        {
            let decided = f
                .decisions
                .iter()
                .find(|d| &d.id == did)
                .map(|d| (d.status, d.answer.clone()));
            self.controller_apply(&f.id, TaskStatus::Verifying, |f, _| {
                let why = match decided {
                    Some((DecisionStatus::Answered, Some(a))) if a == WITHOUT_CHECK => {
                        f.verify_command = None;
                        Some(f.reason("Going on without the check, as you said."))
                    }
                    Some((DecisionStatus::Approved | DecisionStatus::Answered, _)) => {
                        f.verify_approved = true;
                        None
                    }
                    _ => {
                        f.verify_command = None;
                        Some(f.reason("The check wasn't allowed; judging without it."))
                    }
                };
                Ok(why.into_iter().map(Change::new).collect())
            })
            .await?;
            return Ok(()); // The change wakes the next step.
        }
        let mut evidence = Vec::new();
        if let (Some(cmd), true) = (&f.verify_command, f.verify_approved) {
            let (check, cant_run) = run_check(cmd, &root, &env).await;
            // Not the work's fault: the developer fixes the environment.
            if let Some(why) = cant_run {
                return self.check_cant_run(f, cmd, check, &why).await;
            }
            evidence.push(check);
        }
        match self.browser_checks(f, &root, &env).await? {
            Some(more) => evidence.extend(more),
            // A preview command waits for someone to allow it.
            None => return Ok(()),
        }
        // The work itself, read deterministically: what the judge looks at,
        // and whether a fix changed anything.
        let base = self.workspace_base(f).await;
        let tree = crate::work_state::snapshot(&root, &env, base.as_deref()).await;
        let outcomes: String = evidence
            .iter()
            .filter(|e| !e.uncertain)
            .filter_map(|e| e.ok)
            .map(|ok| if ok { '✓' } else { '✗' })
            .collect();
        let fingerprint = format!("{}:{outcomes}", tree.fingerprint);
        if let Some(report) = tree.report {
            evidence.push(evidence_item(
                EvidenceKind::Diff,
                &format!("{}: {}", crate::work_state::TITLE, tree.summary),
                Some(report),
            ));
        }
        if !evidence.is_empty() {
            let ev = evidence.clone();
            if !self
                .controller_apply(&f.id, TaskStatus::Verifying, move |f, _| {
                    let mut changes = Vec::new();
                    for e in ev {
                        changes.push(Change::new(TaskEvent::EvidenceAdded {
                            evidence_id: e.id.clone(),
                            kind: e.kind,
                            ok: e.ok,
                        }));
                        f.evidence.push(e);
                    }
                    Ok(changes)
                })
                .await?
            {
                return Ok(());
            }
        }
        let f = self.task_get(f.id.as_str()).await?;
        // The check decides check-typed criteria in code; the brain judges
        // the rest (and isn't asked when there is nothing left to judge).
        let check = evidence.iter().find(|e| e.kind == EvidenceKind::Test);
        let check_ok = check.and_then(|e| e.ok) == Some(true);
        let by_check: HashSet<String> = f
            .acceptance
            .iter()
            .filter(|c| brain::decided_by_check(&f, c))
            .map(|c| c.id.clone())
            .collect();
        let judgement = if by_check.len() < f.acceptance.len() {
            let brain = brain::select(&env, &self.brain_choice());
            brain
                .judge(&Context {
                    task: &f,
                    root: &root,
                    env: &env,
                })
                .await
                .map_err(|e| RpcError::internal(format!("judging failed: {e:#}")))?
        } else {
            brain::Judgement {
                criteria: vec![],
                remediation: String::new(),
            }
        };
        let failed_check = check.filter(|e| e.ok == Some(false)).map(|e| {
            let tail: String = {
                let d = e.detail.clone().unwrap_or_default();
                let chars: Vec<char> = d.chars().collect();
                chars[chars.len().saturating_sub(1500)..].iter().collect()
            };
            format!("{} failed:\n{tail}", e.title)
        });
        let fresh: Vec<EvidenceId> = evidence.iter().map(|e| e.id.clone()).collect();
        let any_failed = evidence.iter().any(|e| e.ok == Some(false) && !e.uncertain);
        self.controller_apply(&f.id, TaskStatus::Verifying, move |f, now| {
            for c in &mut f.acceptance {
                c.evidence = fresh.clone();
                c.met = if by_check.contains(&c.id) {
                    Some(check_ok)
                } else {
                    // Not judged this time: not met.
                    judgement
                        .criteria
                        .iter()
                        .find(|j| j.id == c.id)
                        .map(|j| j.met)
                };
            }
            let unmet: Vec<String> = f
                .acceptance
                .iter()
                .filter(|c| c.met != Some(true))
                .map(|c| c.text.clone())
                .collect();
            let all_met = !any_failed && unmet.is_empty();
            let mut changes = Vec::new();
            let summary = f
                .acceptance
                .iter()
                .map(|c| format!("{} {}", if c.met == Some(true) { "✓" } else { "✗" }, c.text))
                .collect::<Vec<_>>()
                .join("\n");
            let mark = VerificationMark { fingerprint, unmet };
            if all_met {
                f.last_verification = None;
                changes.push(Change::new(f.reason("Verified: every criterion is met.")));
                let (m, c) = message(
                    MessageRole::Controller,
                    format!("Verified.\n{summary}"),
                    None,
                );
                f.messages.push(m);
                changes.push(c);
                changes.push(Change::new(
                    f.transition(
                        TaskStatus::Review,
                        Some("Ready for your review".into()),
                        now,
                    )
                    .map_err(RpcError::conflict)?,
                ));
            } else if f.last_verification.as_ref() == Some(&mark) {
                // The same work and the same verdict after a fix: another
                // round won't change it. The developer decides (D-062).
                let unproven = if mark.unmet.is_empty() {
                    "a check failed".to_owned()
                } else {
                    mark.unmet.join("; ")
                };
                changes.push(Change::new(f.reason(format!(
                    "Not verified: {unproven}; nothing changed after a fix"
                ))));
                let (m, c) = message(
                    MessageRole::Controller,
                    format!(
                        "Not verified, and nothing changed after the last fix, so another attempt won't help.\n{summary}\nAccept it as it is, or tell me what to change."
                    ),
                    None,
                );
                f.messages.push(m);
                changes.push(c);
                changes.push(Change::new(
                    f.transition(
                        TaskStatus::Review,
                        Some("Not verified: nothing changed after a fix".into()),
                        now,
                    )
                    .map_err(RpcError::conflict)?,
                ));
            } else {
                f.last_verification = Some(mark);
                let fix = if !judgement.remediation.trim().is_empty() {
                    judgement.remediation.clone()
                } else if let Some(failed) = failed_check {
                    failed
                } else {
                    "Make the failing acceptance criteria pass.".to_owned()
                };
                let id = StepId::generate();
                f.steps.push(Step {
                    id: id.clone(),
                    title: "Fix what verification found".into(),
                    detail: Some(fix.clone()),
                    status: StepStatus::Pending,
                    depends_on: vec![],
                    workspace_id: f.workspace_id.clone(),
                    session_id: None,
                    attempts: 0,
                    last_error: None,
                    planning: false,
                });
                changes.push(Change::new(f.reason(format!("Not verified: {fix}"))));
                let (m, c) = message(
                    MessageRole::Controller,
                    format!("Not verified yet.\n{summary}\nNext: {fix}"),
                    None,
                );
                f.messages.push(m);
                changes.push(c);
                changes.push(Change::new(TaskEvent::StepChanged {
                    step_id: id,
                    status: StepStatus::Pending,
                }));
                changes.push(Change::new(
                    f.transition(
                        TaskStatus::Implementing,
                        Some("Fixing what verification found".into()),
                        now,
                    )
                    .map_err(RpcError::conflict)?,
                ));
            }
            Ok(changes)
        })
        .await?;
        Ok(())
    }

    /// The check couldn't run at all (a missing command or script): record
    /// it, ask the developer — it's theirs alone — and wait, blocked. No fix
    /// step: the work isn't what's wrong (D-062).
    async fn check_cant_run(
        &self,
        f: &Task,
        cmd: &str,
        check: Evidence,
        why: &str,
    ) -> RpcResult<()> {
        let (cmd, why) = (cmd.to_owned(), why.to_owned());
        self.controller_apply(&f.id, TaskStatus::Verifying, move |f, now| {
            let mut changes = vec![Change::new(TaskEvent::EvidenceAdded {
                evidence_id: check.id.clone(),
                kind: check.kind,
                ok: check.ok,
            })];
            f.evidence.push(check);
            let id = DecisionId::generate();
            f.decisions.push(DecisionRequest {
                id: id.clone(),
                step_id: None,
                run_id: None,
                kind: DecisionKind::Question,
                risk: Risk::Low,
                summary: format!("`{cmd}` can't run here"),
                detail: Some(format!(
                    "{why}\nFix the environment (install what's missing, or the PATH this workspace gets), \
                     then run it again — or go on without the check."
                )),
                options: vec![RUN_AGAIN.into(), WITHOUT_CHECK.into()],
                status: DecisionStatus::Pending,
                decided_by: None,
                answer: None,
                rationale: None,
                created_at: now,
                decided_at: None,
                user_only: true,
                grant: None,
            });
            f.verify_decision = Some(id.clone());
            f.verify_approved = false;
            changes.push(Change::new(TaskEvent::DecisionRequested {
                decision_id: id,
                kind: DecisionKind::Question,
                risk: Risk::Low,
            }));
            let (m, c) = message(
                MessageRole::Controller,
                format!(
                    "`{cmd}` can't run here: {why}\nThat's the environment, not the work, so I haven't asked the coding agent to fix it. Fix it and choose “{RUN_AGAIN}”, or “{WITHOUT_CHECK}”."
                ),
                None,
            );
            f.messages.push(m);
            changes.push(c);
            changes.push(Change::new(
                f.transition(
                    TaskStatus::Blocked,
                    Some(format!("`{cmd}` can't run here: {why}")),
                    now,
                )
                .map_err(RpcError::conflict)?,
            ));
            Ok(changes)
        })
        .await?;
        Ok(())
    }

    /// Whether `cmd` may run for this task: policy, or someone approved
    /// it (asked once, as a decision).
    pub(crate) async fn allowance(
        &self,
        f: &Task,
        cmd: &str,
        purpose: &str,
        root: &std::path::Path,
    ) -> RpcResult<Allowance> {
        let (risk, user_only, why) =
            match policy::classify(&ToolCall::Command { line: cmd.into() }, root) {
                Verdict::Allow => return Ok(Allowance::Allowed),
                Verdict::Deny { why } => return Ok(Allowance::Denied(why)),
                Verdict::Ask {
                    risk,
                    user_only,
                    why,
                    ..
                } => (risk, user_only, why),
            };
        if f.allowed_commands.iter().any(|c| c == cmd) {
            return Ok(Allowance::Allowed);
        }
        let summary = format!("Run `{cmd}` {purpose}");
        let decided = f
            .decisions
            .iter()
            .rev()
            .find(|d| d.summary == summary)
            .map(|d| d.status);
        let (cmd, sum) = (cmd.to_owned(), summary.clone());
        let allowance = match decided {
            Some(DecisionStatus::Approved | DecisionStatus::Answered) => {
                let applied = self.tasks.lock().await.apply(&f.id, None, |f, _| {
                    if !f.allowed_commands.contains(&cmd) {
                        f.allowed_commands.push(cmd.clone());
                    }
                    Ok(vec![])
                })?;
                self.task_changed(&applied);
                Allowance::Allowed
            }
            Some(DecisionStatus::Pending) => Allowance::Pending,
            // Never answered (the run it belonged to stopped, or it waited
            // too long, D-083): asked again.
            Some(DecisionStatus::Denied) => Allowance::Denied("not allowed".into()),
            Some(_) | None => {
                let applied = self.tasks.lock().await.apply(&f.id, None, move |f, now| {
                    let id = DecisionId::generate();
                    f.decisions.push(DecisionRequest {
                        id: id.clone(),
                        step_id: None,
                        run_id: None,
                        kind: DecisionKind::ToolPermission,
                        risk,
                        summary: sum,
                        detail: Some(why),
                        options: vec![],
                        status: DecisionStatus::Pending,
                        decided_by: None,
                        answer: None,
                        rationale: None,
                        created_at: now,
                        decided_at: None,
                        user_only,
                        grant: None,
                    });
                    Ok(vec![Change::new(TaskEvent::DecisionRequested {
                        decision_id: id,
                        kind: DecisionKind::ToolPermission,
                        risk,
                    })])
                })?;
                self.task_changed(&applied);
                Allowance::Pending
            }
        };
        Ok(allowance)
    }

    /// Where a task's checks keep their files (screenshots).
    pub(crate) fn artifacts_dir(&self, id: &TaskId) -> PathBuf {
        self.paths.tasks_dir.join(id.as_str()).join("artifacts")
    }

    /// The project's browser checks (D-046), run in a real headless browser.
    /// `None` while a preview command waits to be allowed.
    async fn browser_checks(
        &self,
        f: &Task,
        root: &std::path::Path,
        env: &EnvMap,
    ) -> RpcResult<Option<Vec<Evidence>>> {
        let checks = match crate::browser::load_checks(root) {
            Ok(c) => c,
            Err(e) => {
                return Ok(Some(vec![browser_evidence(
                    "Browser checks",
                    Some(false),
                    format!("{e:#}"),
                    None,
                )]));
            }
        };
        if checks.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let mut runnable = Vec::new();
        let mut evidence = Vec::new();
        for c in checks {
            match self
                .allowance(f, &c.preview.command, "to preview the app", root)
                .await?
            {
                Allowance::Allowed => runnable.push(c),
                Allowance::Pending => return Ok(None),
                Allowance::Denied(why) => evidence.push(browser_evidence(
                    &format!("Browser: {}", c.name),
                    Some(false),
                    format!("The preview `{}` wasn't allowed: {why}", c.preview.command),
                    None,
                )),
            }
        }
        let Some(program) = crate::browser::find_browser(env) else {
            evidence.push(browser_evidence(
                "Browser checks",
                Some(false),
                "No Chrome or Chromium on this host (install one, or set OTTER_BROWSER).".into(),
                None,
            ));
            return Ok(Some(evidence));
        };
        let artifacts = self.artifacts_dir(&f.id);
        let brain = brain::select(env, &self.brain_choice());
        for check in runnable {
            let (p, c, r, e, a) = (
                program.clone(),
                check.clone(),
                root.to_path_buf(),
                env.clone(),
                artifacts.clone(),
            );
            let out =
                tokio::task::spawn_blocking(move || crate::browser::run_check(&p, &c, &r, &e, &a))
                    .await
                    .map_err(|e| RpcError::internal(e.to_string()))?;
            let last = out.screenshots.last().cloned();
            evidence.push(browser_evidence(
                &format!(
                    "Browser: {} ({})",
                    check.name,
                    if out.ok { "passed" } else { "failed" }
                ),
                Some(out.ok),
                out.report(),
                last.as_deref(),
            ));
            for shot in &out.screenshots {
                let mut e = browser_evidence(
                    &format!("Screenshot: {}", file_name(shot)),
                    None,
                    String::new(),
                    Some(shot),
                );
                e.kind = EvidenceKind::Screenshot;
                evidence.push(e);
            }
            // A model's look at the result: supplementary, and labelled so.
            if let Some(shot) = &last
                && let Ok(Some((looks_right, notes))) = brain
                    .review(&Context { task: f, root, env }, &check.name, shot)
                    .await
            {
                evidence.push(Evidence {
                    id: EvidenceId::generate(),
                    step_id: None,
                    criterion_id: None,
                    kind: EvidenceKind::Review,
                    title: format!("Visual review (model): {}", check.name),
                    ok: Some(looks_right),
                    uri: Some(format!("artifact:{}", file_name(shot))),
                    detail: Some(notes),
                    uncertain: true,
                    at: Utc::now(),
                });
            }
        }
        Ok(Some(evidence))
    }

    /// Review (D-047): publish the change (once the developer allows it),
    /// follow CI, keep the gates current, write the report. The developer
    /// accepts; Otter never merges or deploys.
    async fn review_step(self: &Arc<Self>, f: &Task) -> RpcResult<()> {
        use crate::delivery::{self, Git, GitHub};
        if f.pending_decisions().any(|d| !d.user_only) {
            return self.decide_open(f).await;
        }
        let (root, env) = self.task_workspace(f).await?;
        let git = Git {
            root: &root,
            env: &env,
        };
        let branch = git.publishable_branch().await;
        // Otter's own GitHub sign-in when it has one (D-133), else the host's.
        let gh_env = self.git_account.gh_env(&env);
        let gh = GitHub::find(&root, &gh_env);
        let mut cx = delivery::GateContext {
            publishable: branch.is_some() && gh.is_some(),
            has_ci: root.join(".github/workflows").is_dir(),
            browser_checks: crate::browser::load_checks(&root).is_ok_and(|c| !c.is_empty()),
            ci_settling: false,
        };
        let mut d = f.delivery.clone();
        let mut evidence = Vec::new();
        let mut remediation: Option<String> = None;
        let mut notes: Vec<String> = Vec::new();
        // Lines for the record only.
        let mut rationale: Vec<String> = Vec::new();
        if let (Some(branch), Some(gh)) = (&branch, &gh)
            && !d.as_ref().is_some_and(|d| d.declined)
        {
            let mut head = git.head().await.ok();
            let published = d.as_ref().is_some_and(|d| d.pr_url.is_some());
            // Work not yet committed is work not yet published (D-068).
            let unpublished = git.committable().await;
            if !published || d.as_ref().and_then(|d| d.head.clone()) != head || unpublished {
                // Pushing is an external action: the developer allows it once.
                let cmd = format!("git push origin {branch}");
                match self
                    .allowance(f, &cmd, "and open a pull request", &root)
                    .await?
                {
                    Allowance::Pending => {}
                    Allowance::Denied(_) => {
                        d = Some(otter_core::task::Delivery {
                            branch: branch.clone(),
                            base: None,
                            head,
                            pushed_at: None,
                            pr_url: None,
                            pr_number: None,
                            ci: vec![],
                            reruns: 0,
                            declined: true,
                        });
                        notes.push("You chose not to publish: delivery stays local.".into());
                    }
                    Allowance::Allowed => {
                        let base = self.workspace_base(f).await;
                        // What the agent left uncommitted is committed first
                        // (files that may hold secrets left out, D-068): a
                        // retry finds a clean tree and only pushes.
                        let committed = git.commit_work(&f.title).await.map_err(|e| {
                            RpcError::internal(format!("committing the work: {e:#}"))
                        })?;
                        if let Some((sha, left_out)) = &committed {
                            head = Some(sha.clone());
                            let short: String = sha.chars().take(10).collect();
                            evidence.push(evidence_item(
                                EvidenceKind::Diff,
                                &format!("Committed {short}"),
                                Some(sha.clone()),
                            ));
                            notes.push(format!(
                                "Committed the work as {short}{}.",
                                if left_out.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                        ", leaving out what may hold secrets: {}",
                                        left_out.join(", ")
                                    )
                                }
                            ));
                        }
                        git.push(branch)
                            .await
                            .map_err(|e| RpcError::internal(format!("pushing {branch}: {e:#}")))?;
                        let body = pr_body(f);
                        let (number, url) = gh
                            .publish(branch, base.as_deref(), &f.title, &body)
                            .await
                            .map_err(|e| {
                                RpcError::internal(format!("opening the pull request: {e:#}"))
                            })?;
                        let first = !published;
                        d = Some(otter_core::task::Delivery {
                            branch: branch.clone(),
                            base: base.clone(),
                            head,
                            pushed_at: Some(Utc::now()),
                            pr_url: Some(url.clone()),
                            pr_number: Some(number),
                            ci: vec![],
                            reruns: 0,
                            declined: false,
                        });
                        if first {
                            let mut e = evidence_item(
                                EvidenceKind::PullRequest,
                                &format!("Pull request #{number}"),
                                None,
                            );
                            e.uri = Some(url);
                            evidence.push(e);
                        }
                        if let Some(stat) = git
                            .diffstat(base.as_deref().map(|b| format!("origin/{b}")).as_deref())
                            .await
                        {
                            evidence.push(evidence_item(
                                EvidenceKind::Diff,
                                &format!("{CHANGES_ON}{branch}"),
                                Some(stat),
                            ));
                        }
                        notes.push(format!(
                            "Pushed {branch}{}.",
                            if first {
                                " and opened the pull request"
                            } else {
                                " again"
                            }
                        ));
                    }
                }
            }
            // CI, at most every so often, and not right after a push (it
            // would still be the previous commit's).
            let settle = chrono::Duration::from_std(env_ms("OTTER_CI_SETTLE_MS", 60_000)).unwrap();
            cx.ci_settling = d
                .as_ref()
                .and_then(|d| d.pushed_at)
                .is_some_and(|t| Utc::now() < t + settle);
            if let Some(dl) = d.as_mut()
                && let Some(n) = dl.pr_number
                && !cx.ci_settling
                && self.ci_due(&f.id)
            {
                match gh.checks(n).await {
                    Ok(ci) => dl.ci = ci,
                    Err(e) => tracing::warn!(task = %f.id, "reading CI: {e:#}"),
                }
                let mut failed: Vec<_> = dl
                    .ci
                    .iter()
                    .filter(|c| matches!(c.state.as_str(), "fail" | "cancel"))
                    .cloned()
                    .collect();
                // In one order, whatever gh's, so failures compare (D-139).
                failed.sort_by(|a, b| a.name.cmp(&b.name));
                if !failed.is_empty() {
                    let mut text = String::new();
                    let mut runs = std::collections::HashSet::new();
                    for c in &failed {
                        text.push_str(&format!(
                            "{}: {}\n",
                            c.name,
                            c.description.clone().unwrap_or_default()
                        ));
                        // Checks of one run share its log: once.
                        if c.url
                            .as_deref()
                            .and_then(GitHub::run_id)
                            .is_some_and(|r| runs.insert(r))
                            && let Some(log) = gh.failed_log(c).await
                        {
                            text.push_str(&log);
                            text.push('\n');
                        }
                    }
                    if delivery::looks_flaky(&text) && dl.reruns < delivery::MAX_RERUNS {
                        for c in &failed {
                            if let Err(e) = gh.rerun(c).await {
                                tracing::warn!("rerunning {}: {e:#}", c.name);
                            }
                        }
                        dl.reruns += 1;
                        for c in dl
                            .ci
                            .iter_mut()
                            .filter(|c| c.state == "fail" || c.state == "cancel")
                        {
                            c.state = "pending".into();
                        }
                        notes.push(format!(
                            "CI failed in a way that looks like the infrastructure; rerunning ({} of {}).",
                            dl.reruns,
                            delivery::MAX_RERUNS
                        ));
                    } else {
                        let names: Vec<&str> = failed.iter().map(|c| c.name.as_str()).collect();
                        let title = format!("CI failed: {}", names.join(", "));
                        // The same failure as last time, after a fix: another
                        // fix won't help — the developer's call (D-068).
                        let last = f
                            .evidence
                            .iter()
                            .rev()
                            .find(|e| e.kind == EvidenceKind::Ci && e.ok == Some(false));
                        let same = last.is_some_and(|e| {
                            e.title == title
                                && e.detail
                                    .as_deref()
                                    .is_some_and(|d| delivery::same_ci_failure(d, &text))
                        });
                        let fixed = f
                            .steps
                            .iter()
                            .any(|t| t.title == CI_FIX && t.status == StepStatus::Done);
                        if same {
                            let marker = format!(
                                "{SAME_CI} ({})",
                                last.map(|e| e.id.as_str()).unwrap_or_default()
                            );
                            if fixed && !f.rationale.iter().any(|r| r == &marker) {
                                notes.push(format!(
                                    "{SAME_CI}, so another attempt won't help: over to you. Look at the CI log, then accept it as it is or tell me what to change."
                                ));
                                rationale.push(marker);
                            }
                        } else {
                            remediation = Some(format!(
                                "CI failed ({}):\n{}",
                                names.join(", "),
                                text.trim()
                            ));
                            let mut e = evidence_item(EvidenceKind::Ci, &title, Some(text));
                            e.ok = Some(false);
                            e.uri = failed[0].url.clone();
                            evidence.push(e);
                        }
                    }
                } else if !dl.ci.is_empty()
                    && dl
                        .ci
                        .iter()
                        .all(|c| matches!(c.state.as_str(), "pass" | "skipping"))
                    && !f.evidence.iter().any(|e| {
                        e.kind == EvidenceKind::Ci
                            && e.ok == Some(true)
                            && e.detail.as_deref() == dl.head.as_deref()
                    })
                {
                    let mut e = evidence_item(
                        EvidenceKind::Ci,
                        &format!("CI passed ({} checks)", dl.ci.len()),
                        dl.head.clone(),
                    );
                    e.ok = Some(true);
                    e.uri = dl.pr_url.clone();
                    evidence.push(e);
                }
            }
        }
        // Gates, the report, or back to work: written only when something changed.
        let mut next = f.clone();
        next.delivery = d.clone();
        next.evidence.extend(evidence.iter().cloned());
        let gates = delivery::gates(&next, &cx);
        let ready = remediation.is_none() && delivery::ready_for_acceptance(&gates);
        // Delivery's stat of the branch (verification's working-tree diffs
        // are evidence for the judge, not the report).
        let is_stat =
            |e: &&Evidence| e.kind == EvidenceKind::Diff && e.title.starts_with(CHANGES_ON);
        let diffstat = evidence
            .iter()
            .find(is_stat)
            .and_then(|e| e.detail.clone())
            .or_else(|| {
                f.evidence
                    .iter()
                    .rev()
                    .find(is_stat)
                    .and_then(|e| e.detail.clone())
            });
        let write_report = ready && f.report.is_none();
        if d == f.delivery
            && evidence.is_empty()
            && gates == f.gates
            && remediation.is_none()
            && !write_report
            && notes.is_empty()
            && rationale.is_empty()
        {
            return Ok(());
        }
        let mut posted = None;
        self.controller_apply(&f.id, TaskStatus::Review, |f, now| {
            let mut changes = Vec::new();
            f.delivery = d.clone();
            for e in evidence.clone() {
                changes.push(Change::new(TaskEvent::EvidenceAdded {
                    evidence_id: e.id.clone(),
                    kind: e.kind,
                    ok: e.ok,
                }));
                f.evidence.push(e);
            }
            for n in &notes {
                changes.push(Change::new(f.reason(n.clone())));
                let (m, c) = message(MessageRole::Controller, n.clone(), None);
                f.messages.push(m);
                changes.push(c);
            }
            for r in &rationale {
                changes.push(Change::new(f.reason(r.clone())));
            }
            f.gates = gates.clone();
            if let Some(fix) = &remediation {
                let id = StepId::generate();
                f.steps.push(Step {
                    id: id.clone(),
                    title: CI_FIX.into(),
                    detail: Some(fix.clone()),
                    status: StepStatus::Pending,
                    depends_on: vec![],
                    workspace_id: f.workspace_id.clone(),
                    session_id: None,
                    attempts: 0,
                    last_error: None,
                    planning: false,
                });
                f.report = None;
                changes.push(Change::new(f.reason("CI failed: back to implementing.")));
                changes.push(Change::new(TaskEvent::StepChanged { step_id: id, status: StepStatus::Pending }));
                changes.push(Change::new(
                    f.transition(TaskStatus::Implementing, Some("CI failed".into()), now)
                        .map_err(RpcError::conflict)?,
                ));
                return Ok(changes);
            }
            if write_report {
                let r = delivery::report(f, diffstat.as_deref());
                posted = Some(r.clone());
                f.report = Some(r);
                let (m, c) = message(
                    MessageRole::Controller,
                    "Every gate has passed; the report is ready. Accept when you're happy with it — I won't merge or deploy.".into(),
                    None,
                );
                f.messages.push(m);
                changes.push(c);
            }
            Ok(changes)
        })
        .await?;
        // The report goes on the pull request too (it's already public).
        if let (Some(r), Some(gh), Some(n)) = (posted, gh, d.as_ref().and_then(|d| d.pr_number))
            && let Err(e) = gh.comment(n, &r).await
        {
            tracing::warn!("posting the report: {e:#}");
        }
        Ok(())
    }

    /// The branch a Git workspace was made from, as the PR's base (`main`).
    async fn workspace_base(&self, f: &Task) -> Option<String> {
        let ws = f.workspace_id.as_ref()?;
        let store = self.store.lock().await;
        let w = store.workspace(ws.as_str())?;
        match &w.source {
            otter_core::WorkspaceSource::Git(g) => g
                .base
                .as_deref()
                .map(|b| b.trim_start_matches("origin/").to_owned()),
            _ => None,
        }
    }

    /// Whether it's time to look at CI again (`OTTER_CI_POLL_MS`, 15 s).
    fn ci_due(&self, id: &TaskId) -> bool {
        let every = env_ms("OTTER_CI_POLL_MS", 15_000);
        let mut st = self.controller.lock().unwrap();
        let now = std::time::Instant::now();
        match st.ci_polled.get(id) {
            Some(t) if now.duration_since(*t) < every => false,
            _ => {
                st.ci_polled.insert(id.clone(), now);
                true
            }
        }
    }
}

fn evidence_item(kind: EvidenceKind, title: &str, detail: Option<String>) -> Evidence {
    Evidence {
        id: EvidenceId::generate(),
        step_id: None,
        criterion_id: None,
        kind,
        title: title.to_owned(),
        ok: None,
        uri: None,
        detail,
        uncertain: false,
        at: Utc::now(),
    }
}

/// The pull request's description.
fn pr_body(f: &Task) -> String {
    let mut b = format!("{}\n\n### Acceptance criteria\n", f.request);
    for c in &f.acceptance {
        b.push_str(&format!(
            "- [{}] {}\n",
            if c.met == Some(true) { "x" } else { " " },
            c.text
        ));
    }
    let checks: Vec<String> = f
        .evidence
        .iter()
        .filter(|e| e.ok.is_some())
        .map(|e| {
            format!(
                "- {} {}",
                if e.ok == Some(true) { "✓" } else { "✗" },
                e.title
            )
        })
        .collect();
    if !checks.is_empty() {
        b.push_str(&format!("\n### Checked\n{}\n", checks.join("\n")));
    }
    b.push_str(
        "\n_Opened by Otter; publishing was approved by the developer. Otter doesn't merge._\n",
    );
    b
}

/// The developer's messages since the Control Agent last answered one
/// (the request that created the task isn't one).
pub(crate) fn unanswered(f: &Task) -> Vec<otter_core::task::Message> {
    let answered = f.messages.iter().rposition(|m| {
        m.role == MessageRole::Controller
            && m.correlation_id
                .as_deref()
                .is_some_and(|c| c.starts_with("msg_"))
    });
    // The request isn't a message to answer (D-067).
    let request = f.request_message();
    f.messages
        .iter()
        .enumerate()
        .skip(answered.map_or(0, |i| i + 1))
        .filter(|(_, m)| m.role == MessageRole::User && Some(&m.id) != request)
        .map(|(_, m)| m.clone())
        .collect()
}

/// Whether a command may run (see [`Daemon::allowance`]).
pub(crate) enum Allowance {
    Allowed,
    Pending,
    Denied(String),
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Evidence from a browser check; screenshots are referred to by name
/// (`artifact:<name>`, served by `task.artifact`).
fn browser_evidence(
    title: &str,
    ok: Option<bool>,
    detail: String,
    shot: Option<&std::path::Path>,
) -> Evidence {
    Evidence {
        id: EvidenceId::generate(),
        step_id: None,
        criterion_id: None,
        kind: EvidenceKind::Browser,
        title: title.to_owned(),
        ok,
        uri: shot.map(|s| format!("artifact:{}", file_name(s))),
        detail: Some(detail).filter(|d| !d.is_empty()),
        uncertain: false,
        at: Utc::now(),
    }
}

/// The last runs of one step all failed the same way, since the work last
/// (re)started — a retry starts counting afresh.
fn repeating_failure(f: &Task) -> Option<String> {
    let last: Vec<_> = f
        .runs
        .iter()
        .rev()
        .filter(|r| f.active_since.is_none_or(|s| r.started_at >= s))
        .take(LOOP_REPEATS)
        .collect();
    if last.len() < LOOP_REPEATS {
        return None;
    }
    let first = last[0];
    let same = last.iter().all(|r| {
        r.state == RunState::Failed && r.step_id == first.step_id && r.summary == first.summary
    });
    same.then(|| first.summary.clone().unwrap_or_default())
}

/// The messages (by id) a conversation's turns carry: queued, sent or
/// delivered — they reach the agent that way, once.
fn carried_by(c: &otter_core::conversation::Conversation) -> std::collections::HashSet<String> {
    c.turns
        .iter()
        .filter(|t| {
            t.state == otter_core::conversation::TurnState::Queued
                || t.delivery != otter_core::conversation::Delivery::Queued
        })
        .filter_map(|t| t.command_id.clone())
        .collect()
}

impl Daemon {
    /// The task's conversation, to go on in (D-064): the latest run with
    /// an agent session, and its conversation — unless that session is gone
    /// (the agent said so) or the conversation was closed, when the next
    /// turn starts a new one. Nor after a run that needed recovery (D-061):
    /// the developer said to start over, so it starts in a new session.
    fn task_session(
        &self,
        f: &Task,
    ) -> Option<(
        otter_core::task::Run,
        otter_core::conversation::Conversation,
    )> {
        use otter_core::conversation::{ErrorCategory, Lifecycle};
        let run = f
            .runs
            .iter()
            .rev()
            .find(|r| r.provider_session_id.is_some() || r.state == RunState::RecoveryRequired)
            .filter(|r| r.state != RunState::RecoveryRequired)?;
        let c = self
            .conversations
            .get(run.conversation_id.as_ref()?.as_str())?;
        let gone = c.lifecycle == Lifecycle::Closed
            || c.turns
                .last()
                .is_some_and(|t| t.error == Some(ErrorCategory::ResumeUnavailable));
        (!gone).then(|| (run.clone(), c))
    }

    /// The messages the task's live runs' conversations already carry.
    pub(crate) fn carried(&self, f: &Task) -> std::collections::HashSet<String> {
        f.runs
            .iter()
            .filter(|r| !r.state.is_over())
            .filter_map(|r| r.conversation_id.as_ref())
            .filter_map(|c| self.conversations.get(c.as_str()))
            .flat_map(|c| carried_by(&c))
            .collect()
    }
}

/// A plan, for the developer to read: its steps, criteria and check.
fn plan_text(plan: &brain::Plan) -> String {
    let mut text = format!(
        "{}\nAcceptance criteria:\n{}",
        plan.steps
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}. {}", i + 1, t.title))
            .collect::<Vec<_>>()
            .join("\n"),
        plan.criteria
            .iter()
            .map(|c| format!("- {}", c.text))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    if let Some(cmd) = &plan.verify_command {
        text.push_str(&format!("\nThe check: `{cmd}`."));
    }
    text
}

/// The plan question waiting on the developer, if one is (D-084).
pub(crate) fn plan_question(f: &Task) -> Option<DecisionId> {
    f.pending_decisions()
        .find(|d| d.summary == PLAN_QUESTION)
        .map(|d| d.id.clone())
}

/// What a message's intent does, given where the task stands. Done (a
/// question answered, work accepted), there is no current work to guide or
/// go on with: asking for more moves the goal (D-050). Asked whether a plan
/// goes ahead, instructions are what to change (D-084), and going on says
/// yes ([`Daemon::after_intent`]).
pub(crate) fn effective_intent(
    intent: crate::brain::Intent,
    status: TaskStatus,
    plan_asked: bool,
) -> crate::brain::Intent {
    use crate::brain::Intent;
    match (intent, status) {
        (Intent::Guide | Intent::Redirect | Intent::Continue, TaskStatus::Done) => Intent::Revise,
        (Intent::Guide | Intent::Redirect, _) if plan_asked => Intent::Revise,
        (intent, _) => intent,
    }
}

/// The change an intent makes to the task (`converse`, and Otter's tools,
/// D-089), and whether the work in progress stops. `authority` is the
/// developer's message that asked for it: accepting is recorded under it.
pub(crate) fn intent_step(
    f: &mut Task,
    intent: crate::brain::Intent,
    authority: &str,
    now: otter_core::Timestamp,
) -> (Vec<Change>, bool) {
    use crate::brain::Intent;
    let move_to = |f: &mut Task, to: TaskStatus, why: &str| {
        f.transition(to, Some(why.into()), now)
            .ok()
            .map(Change::new)
    };
    let mut changes = Vec::new();
    let mut stop = false;
    let step = match (intent, f.status) {
        (Intent::Continue, TaskStatus::Draft) => move_to(f, TaskStatus::Planning, "You said go on"),
        (Intent::Continue, TaskStatus::Paused) => {
            let next = crate::tasks::resume_status(f);
            move_to(f, next, "You said go on")
        }
        (Intent::Continue, TaskStatus::Blocked) if f.pending_decisions().next().is_none() => {
            let next = crate::tasks::resume_status(f);
            move_to(f, next, "You said go on")
        }
        (Intent::Continue, TaskStatus::Failed) => {
            f.budget.iterations_used = 0;
            for t in &mut f.steps {
                if t.status == StepStatus::Failed {
                    t.status = StepStatus::Pending;
                    t.attempts = 0;
                }
            }
            let next = crate::tasks::resume_status(f);
            move_to(f, next, "You said go on: retrying with a fresh budget")
        }
        (Intent::Pause, s)
            if s.is_active() || s == TaskStatus::Blocked || s == TaskStatus::Review =>
        {
            stop = true;
            move_to(f, TaskStatus::Paused, "Paused: you asked")
        }
        // The goal moved (D-050): plan again, keeping what is done.
        (Intent::Revise, s) if !s.is_terminal() => {
            stop = true;
            changes.extend(crate::tasks::replan(
                f,
                "You changed the goal: planning again",
                now,
            ));
            None
        }
        // Satisfied: accept what's in review (its gates still apply), or
        // stop work in progress.
        (Intent::Finish, TaskStatus::Review) => {
            let accept = otter_core::task::TaskAction::Accept {
                override_gates: false,
            };
            match crate::tasks::apply_action(f, &accept, authority, now) {
                Ok(c) => changes.extend(c),
                Err(e) => changes.push(Change::new(
                    f.reason(format!("Not done yet: {}", e.message)),
                )),
            }
            None
        }
        (Intent::Finish, s) if s.is_active() || s == TaskStatus::Blocked => {
            stop = true;
            move_to(f, TaskStatus::Paused, "Stopped: you said that's enough")
        }
        _ => None,
    };
    changes.extend(step);
    (changes, stop)
}

/// What a developer's message turns into for the coding agent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Routing {
    Nothing,
    Queue,
    Redirect,
}

pub(crate) fn routing(intent: crate::brain::Intent) -> Routing {
    use crate::brain::Intent;
    match intent {
        Intent::Guide => Routing::Queue,
        Intent::Redirect => Routing::Redirect,
        _ => Routing::Nothing,
    }
}

/// The developer's messages since `since`, leaving out those a turn of the
/// conversation already carries (`carried`: queued, sent or delivered) —
/// they reach the agent that way, once (D-059).
fn developer_notes(
    f: &Task,
    since: Option<chrono::DateTime<Utc>>,
    carried: &std::collections::HashSet<String>,
) -> Vec<String> {
    let request = f.request_message();
    f.messages
        .iter()
        .filter(|m| Some(&m.id) != request)
        .filter(|m| m.role == MessageRole::User && since.is_none_or(|s| m.at > s))
        .filter(|m| !carried.contains(m.id.as_str()))
        .map(|m| m.text.clone())
        .collect()
}

/// The developer's notes, as a prompt says them.
fn notes_text(notes: &[String]) -> String {
    if notes.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = notes.iter().map(|n| format!("- {n}")).collect();
        format!("\nFrom the developer:\n{}\n", lines.join("\n"))
    }
}

/// What a coding agent is told to start `step`, with the developer's
/// `notes` it hasn't had yet. In the task's conversation (`warm`) the
/// request and requirements are already said (D-064).
pub fn step_prompt(f: &Task, step: &Step, notes: Vec<String>, warm: bool) -> String {
    let work: Vec<&Step> = f.steps.iter().filter(|t| !t.planning).collect();
    let n = work
        .iter()
        .position(|t| t.id == step.id)
        .map_or(0, |i| i + 1);
    let mut p = String::new();
    if !warm {
        p.push_str(&format!(
            "Task: {}\n\nRequest:\n{}\n",
            f.title,
            if f.request.is_empty() {
                &f.title
            } else {
                &f.request
            }
        ));
        if !f.requirements.is_empty() {
            p.push_str(&format!(
                "\nRequirements:\n- {}\n",
                f.requirements.join("\n- ")
            ));
        }
    }
    if !f.acceptance.is_empty() {
        p.push_str("\nAcceptance criteria:\n");
        for c in &f.acceptance {
            p.push_str(&format!("- {}\n", c.text));
        }
    }
    p.push_str(&format!(
        "\nYour step ({n} of {}): {}\n",
        work.len(),
        step.title
    ));
    if let Some(d) = &step.detail {
        p.push_str(&format!("{d}\n"));
    }
    if let Some(e) = &step.last_error {
        p.push_str(&format!("\nThe previous attempt failed: {e}\n"));
    }
    // Notes the step already says aren't said twice.
    let detail = step.detail.as_deref().unwrap_or_default();
    let notes: Vec<String> = notes
        .into_iter()
        .filter(|n| !detail.contains(n.trim()))
        .collect();
    p.push_str(&notes_text(&notes));
    if let Some(cmd) = &f.verify_command {
        p.push_str(&format!(
            "\nThe work is checked with `{cmd}`; run it before you finish.\n"
        ));
    }
    p
}

/// What a plan proposal looks like, as the planning turn is told (D-064).
const PLAN_FORMAT: &str = "{\"summary\": \"one sentence: the approach\", \
     \"steps\": [{\"title\": \"...\", \"detail\": \"what to do, where\"}], \
     \"criteria\": [{\"text\": \"...\", \"how\": \"check\" or \"diff\"}], \
     \"check\": \"the command that runs the project's tests here\" or null}\n\
     - steps: in order, 1 to 5; one is right for a small change. They are yours to \
     do on your own: none asks, checks with or waits for the developer.\n\
     - criteria: facts about the code or the check's result, never about the conversation \
     or the process. how: \"check\" when the check passing proves it, \"diff\" when it is \
     about what the change contains.\n\
     - check: one shell command that runs here (look at how the project runs its tests).\n";

/// The planning turn (D-064, D-084): read the code, then answer a question,
/// or propose the plan for a change as JSON, changing nothing yet. In the
/// task's conversation (`warm`: planning again) the request is already
/// said; a plan sent back by Otter's review gets its points instead.
fn planning_prompt(f: &Task, step: &Step, notes: Vec<String>, warm: bool) -> String {
    if let Some(issues) = &step.detail {
        return format!(
            "Otter reviewed your plan and sees these problems:\n{issues}{notes}\n\
             Look again where you need to, and plan the work again. Where a point is wrong given \
             the code, keep your plan and say why in its summary. End this turn with only the \
             JSON object, as before:\n{PLAN_FORMAT}",
            notes = notes_text(&notes),
        );
    }
    let done: Vec<String> = f
        .steps
        .iter()
        .filter(|t| !t.planning && t.status == StepStatus::Done)
        .map(|t| format!("- {}", t.title))
        .collect();
    let opening = if warm {
        format!(
            "{}{}",
            if !notes.is_empty() {
                notes_text(&notes)
            } else if f.replan {
                "The developer asked to plan again.\n".to_owned()
            } else {
                "Look at the request again.\n".to_owned()
            },
            if done.is_empty() {
                String::new()
            } else {
                format!(
                    "\nAlready done (don't redo it, don't list it again):\n{}\n",
                    done.join("\n")
                )
            }
        )
    } else {
        format!(
            "Task: {}\n\nRequest:\n{}\n{}",
            f.title,
            if f.request.trim().is_empty() {
                &f.title
            } else {
                &f.request
            },
            notes_text(&notes),
        )
    };
    format!(
        "{opening}\n\
         Read the code this touches first. Then do one of these:\n\
         - If it asks a question (how or why something works, where something is) or for an \
         explanation or an investigation, and not for a change: answer it. That is the whole \
         turn: say what you found and point to the code (path:line). Change nothing, and add \
         no JSON.\n\
         - If it asks for a change: don't change anything yet; plan the work{left}. Otter \
         reviews the plan, then gives you its steps one turn at a time. End this turn with \
         only a JSON object, no summary:\n{PLAN_FORMAT}",
        left = if done.is_empty() { "" } else { " that is left" },
    )
}

/// What a resumed run is told: carry on, plus anything new from the developer.
fn continue_prompt(
    f: &Task,
    step: &Step,
    last: Option<&otter_core::task::Run>,
    carried: &std::collections::HashSet<String>,
) -> String {
    // A turn cut off says so (D-063): by a limit, what to do about it.
    let cut = last
        .filter(|r| r.state == RunState::Cancelled)
        .map(|r| r.summary.as_deref().unwrap_or_default());
    let opening = match cut {
        Some(why) if why.starts_with(OUT_OF_TIME) || why.starts_with(NO_PROGRESS) => format!(
            "You ran out of time on “{}” ({}). Continue where you left off: \
             do what the step still needs first, then end your turn with a short report.",
            step.title,
            why.to_lowercase()
        ),
        Some(_) => "Your last turn was interrupted before it finished. \
                    Continue the step where you left off."
            .into(),
        // Lost mid-turn (D-061): what it was doing may or may not have
        // happened, and must not be repeated blindly.
        None if last.is_some_and(|r| r.state == RunState::Interrupted) => {
            format!("Continue the step where you left off.{CUT_OFF}")
        }
        None => "Continue the step where you left off.".into(),
    };
    format!(
        "{opening}{}",
        notes_text(&developer_notes(f, last.map(|r| r.started_at), carried))
    )
}

/// After a run was cut off mid-turn (D-061).
const CUT_OFF: &str = " Your last run was cut off before it finished, so the result of \
what you were doing then is unknown: check whether it took effect (files, \
processes, anything outside) before you do it again.";

/// Run the check command in the workspace and record what happened; and,
/// if it couldn't run at all, the line that says why (see [`cant_run`]).
async fn run_check(cmd: &str, root: &std::path::Path, env: &EnvMap) -> (Evidence, Option<String>) {
    let started = Utc::now();
    let result = tokio::time::timeout(
        check_timeout(),
        tokio::process::Command::new("sh")
            .args(["-c", cmd])
            .current_dir(root)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let (ok, title, detail, why) = match result {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let tail: String = {
                let chars: Vec<char> = text.chars().collect();
                chars[chars.len().saturating_sub(CHECK_OUTPUT)..]
                    .iter()
                    .collect()
            };
            let code = out.status.code();
            match cant_run(code, &tail) {
                Some(why) => (None, format!("`{cmd}` can't run here"), tail, Some(why)),
                None => {
                    let code = code.map_or("signal".into(), |c| c.to_string());
                    (
                        Some(out.status.success()),
                        format!("`{cmd}` (exit {code})"),
                        tail,
                        None,
                    )
                }
            }
        }
        Ok(Err(e)) => (
            None,
            format!("`{cmd}` can't run here"),
            e.to_string(),
            Some(format!("it couldn't start: {e}")),
        ),
        Err(_) => (
            Some(false),
            format!("`{cmd}` timed out"),
            format!("after {}s", check_timeout().as_secs()),
            None,
        ),
    };
    let evidence = Evidence {
        id: EvidenceId::generate(),
        step_id: None,
        criterion_id: None,
        kind: EvidenceKind::Test,
        title,
        ok,
        uri: None,
        detail: Some(detail).filter(|d| !d.trim().is_empty()),
        uncertain: false,
        at: started,
    };
    (evidence, why)
}

/// Why a check couldn't run at all — a command or script that isn't there,
/// or can't be executed — from its exit status and output; `None` when it
/// ran (passing or failing). The output counts too: `make` reports a
/// missing command as its own exit 2. Only the shell's, `make`'s, `env`'s
/// and npm's own complaints count, not a test's output that happens to
/// mention a missing file.
fn cant_run(code: Option<i32>, output: &str) -> Option<String> {
    if code == Some(0) {
        return None;
    }
    let line = output.lines().map(str::trim).find(|l| says_cant_run(l));
    match (code, line) {
        (_, Some(l)) => Some(l.to_owned()),
        (Some(c @ (126 | 127)), None) => Some(format!("exit {c}")),
        _ => None,
    }
}

fn says_cant_run(line: &str) -> bool {
    if line.contains("command not found") || line.contains("Missing script:") {
        return true;
    }
    let program = line.split(':').next().unwrap_or("");
    let program = program.rsplit('/').next().unwrap_or(program);
    let program = program.split('[').next().unwrap_or(program);
    if program == "make" && (line.ends_with("Error 127") || line.ends_with("Error 126")) {
        return true;
    }
    matches!(program, "sh" | "bash" | "zsh" | "dash" | "env" | "make")
        && (line.ends_with(": not found")
            || line.ends_with("No such file or directory")
            || line.ends_with("Permission denied"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_guidance_reaches_the_coding_agent() {
        use crate::brain::Intent;
        assert_eq!(routing(Intent::Guide), Routing::Queue);
        assert_eq!(routing(Intent::Redirect), Routing::Redirect);
        // A question, or a message about the task itself, is Otter's.
        for i in [
            Intent::None,
            Intent::Continue,
            Intent::Pause,
            Intent::Revise,
            Intent::Finish,
        ] {
            assert_eq!(routing(i), Routing::Nothing, "{i:?}");
        }
    }
    use otter_core::RunId;
    use otter_core::task::Run;

    #[tokio::test]
    async fn a_panic_in_a_step_is_an_error_like_any_other() {
        let panicked = contained(async { panic!("byte index 15 is not a char boundary") }).await;
        let e = panicked.expect("not cancelled").unwrap_err();
        assert!(
            e.message.contains("otterd's log says where"),
            "{}",
            e.message
        );
        assert!(matches!(contained(async { Ok(()) }).await, Some(Ok(()))));
    }

    fn task_with_runs(summaries: &[(&str, RunState)]) -> Task {
        let mut f = Task::new("t".into(), "r".into(), Utc::now());
        let step = StepId::generate();
        for (s, state) in summaries {
            f.runs.push(Run {
                id: RunId::generate(),
                step_id: step.clone(),
                runtime: "claude".into(),
                state: *state,
                started_at: Utc::now(),
                ended_at: None,
                summary: Some((*s).into()),
                provider_session_id: None,
                activity: vec![],
                activity_at: vec![],
                heard_at: None,
                tools_running: 0,
                conversation_id: None,
                turn_id: None,
                worked_ms: 0,
                working_since: None,
            });
        }
        f
    }

    fn step(title: &str, status: StepStatus, planning: bool) -> Step {
        Step {
            id: StepId::generate(),
            title: title.into(),
            detail: None,
            status,
            depends_on: vec![],
            workspace_id: None,
            session_id: None,
            attempts: 0,
            last_error: None,
            planning,
        }
    }

    /// The planning turn (D-084): a question is answered, a change planned;
    /// planning again in the conversation doesn't restate the request but
    /// says what is done; a plan sent back gets the review's points.
    #[test]
    fn the_planning_turn_answers_or_plans() {
        let mut f = Task::new(
            "Backport".into(),
            "How does the backport tag work?".into(),
            Utc::now(),
        );
        let look = step(PLANNING, StepStatus::Pending, true);
        let cold = planning_prompt(&f, &look, vec![], false);
        assert!(
            cold.contains("Request:\nHow does the backport tag work?"),
            "{cold}"
        );
        assert!(cold.contains("answer it"), "{cold}");
        assert!(cold.contains("plan the work."), "{cold}");
        f.steps = vec![
            look.clone(),
            step("Tag the backport", StepStatus::Done, false),
            step("Document it", StepStatus::Pending, false),
        ];
        let warm = planning_prompt(&f, &look, vec!["Also cover 2.x".into()], true);
        assert!(!warm.contains("Request:"), "{warm}");
        assert!(warm.contains("- Also cover 2.x"), "{warm}");
        assert!(warm.contains("Already done"), "{warm}");
        assert!(warm.contains("- Tag the backport"), "{warm}");
        assert!(!warm.contains("- Document it"), "{warm}");
        assert!(warm.contains("plan the work that is left"), "{warm}");
        let mut back = look.clone();
        back.detail = Some("- The criteria are too vague.\n".into());
        let again = planning_prompt(&f, &back, vec![], true);
        assert!(again.starts_with("Otter reviewed your plan"), "{again}");
        assert!(again.contains("too vague"), "{again}");
        assert!(!again.contains("answer it"), "{again}");
    }

    #[test]
    fn a_resumed_turn_says_why_the_last_one_was_cut_off() {
        let step = Step {
            id: StepId::generate(),
            title: "Add CSV export".into(),
            detail: None,
            status: StepStatus::Pending,
            depends_on: vec![],
            workspace_id: None,
            session_id: None,
            attempts: 1,
            last_error: None,
            planning: false,
        };
        let none = HashSet::new();
        let prompt = |summary: &str, state| {
            let f = task_with_runs(&[(summary, state)]);
            continue_prompt(&f, &step, f.runs.last(), &none)
        };
        let p = prompt("Out of time after 30 min of work", RunState::Cancelled);
        assert!(
            p.starts_with(
                "You ran out of time on “Add CSV export” (out of time after 30 min of work)."
            ),
            "{p}"
        );
        assert!(
            prompt("No progress for 10 min", RunState::Cancelled)
                .starts_with("You ran out of time")
        );
        assert!(
            prompt(
                "Interrupted: otterd restarted; resumable",
                RunState::Cancelled
            )
            .starts_with("Your last turn was interrupted")
        );
        assert_eq!(
            prompt("handed back", RunState::HandedOff),
            "Continue the step where you left off."
        );
        // Lost mid-turn (D-061): the last step's result is unknown.
        let lost = prompt(
            "Interrupted: the agent's process was lost",
            RunState::Interrupted,
        );
        assert!(lost.starts_with("Continue the step where you left off."));
        assert!(lost.contains(CUT_OFF), "{lost}");
        assert!(!prompt("handed back", RunState::HandedOff).contains("cut off"));
        assert_eq!(span(Duration::from_millis(1500)), "1 s");
        assert_eq!(span(Duration::from_secs(1800)), "30 min");
    }

    #[test]
    fn the_same_failure_three_times_is_a_loop() {
        use RunState::*;
        assert!(repeating_failure(&task_with_runs(&[("x", Failed), ("x", Failed)])).is_none());
        assert_eq!(
            repeating_failure(&task_with_runs(&[
                ("x", Failed),
                ("x", Failed),
                ("x", Failed)
            ])),
            Some("x".into())
        );
        assert!(
            repeating_failure(&task_with_runs(&[
                ("x", Failed),
                ("y", Failed),
                ("x", Failed)
            ]))
            .is_none()
        );
        assert!(
            repeating_failure(&task_with_runs(&[
                ("x", Failed),
                ("x", Completed),
                ("x", Failed)
            ]))
            .is_none()
        );
    }

    #[test]
    fn a_check_that_cant_run_is_told_from_one_that_fails() {
        // A missing command, through the shell or straight from make.
        assert_eq!(
            cant_run(Some(127), "sh: 1: cargo: not found\n").as_deref(),
            Some("sh: 1: cargo: not found")
        );
        assert_eq!(
            cant_run(
                Some(2),
                "make: not-on-this-path: No such file or directory\nmake: *** [Makefile:2: test] Error 127\n"
            )
            .as_deref(),
            Some("make: not-on-this-path: No such file or directory")
        );
        assert!(
            cant_run(
                Some(2),
                "/bin/sh: 1: pytest: not found\nmake: *** [Makefile:2: test] Error 127"
            )
            .is_some()
        );
        assert!(cant_run(Some(2), "make[1]: *** [Makefile:5: unit] Error 127").is_some());
        assert!(cant_run(Some(1), "bash: line 1: go: command not found").is_some());
        assert!(cant_run(Some(1), "npm error Missing script: \"test\"").is_some());
        assert_eq!(cant_run(Some(126), "").as_deref(), Some("exit 126"));
        // Failing tests are the work's: even ones mentioning a missing file.
        assert!(cant_run(Some(1), "test export ... FAILED\n1 failed").is_none());
        assert!(
            cant_run(
                Some(101),
                "thread 'x' panicked: No such file or directory (os error 2)"
            )
            .is_none()
        );
        assert!(cant_run(Some(2), "make: *** [Makefile:2: test] Error 1").is_none());
        assert!(cant_run(Some(0), "sh: 1: x: not found").is_none());
        assert!(cant_run(None, "").is_none());
    }

    #[test]
    fn a_step_prompt_carries_the_plan_and_what_went_wrong() {
        let mut f = Task::new("CSV".into(), "Export it".into(), Utc::now());
        f.verify_command = Some("make test".into());
        f.acceptance.push(Criterion {
            id: "ac_1".into(),
            text: "has a header".into(),
            met: None,
            evidence: vec![],
            how: otter_core::task::Proof::Diff,
        });
        let t = Step {
            id: StepId::generate(),
            title: "Write it".into(),
            detail: None,
            status: StepStatus::Failed,
            depends_on: vec![],
            workspace_id: None,
            session_id: None,
            attempts: 1,
            last_error: Some("tests failed".into()),
            planning: false,
        };
        f.steps.push(t.clone());
        let p = step_prompt(&f, &t, developer_notes(&f, None, &HashSet::new()), false);
        for want in [
            "Export it",
            "has a header",
            "1 of 1",
            "previous attempt failed: tests failed",
            "`make test`",
        ] {
            assert!(p.contains(want), "{want} in {p}");
        }
        // In the task's conversation, the request was said already.
        let warm = step_prompt(&f, &t, vec![], true);
        assert!(!warm.contains("Export it"), "{warm}");
        assert!(
            warm.contains("has a header") && warm.contains("1 of 1"),
            "{warm}"
        );
    }
}
