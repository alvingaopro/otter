//! The Control Agent's engine (D-045): owns a feature from plan to review.
//!
//! An event-driven loop in the daemon — woken whenever a feature changes,
//! and on a tick that enforces time limits — steps each active feature:
//!
//! - **planning** → the brain writes requirements, acceptance criteria, tasks
//!   and a check command (which policy must allow, or someone approves);
//! - **implementing** → the next task whose dependencies are done goes to a
//!   managed run (an interrupted one resumes its conversation); the brain
//!   decides what policy leaves open, the rest waits for the developer;
//! - **verifying** → the check runs and the work's state (its diff) is read,
//!   evidence is recorded; the check decides check-typed criteria, the brain
//!   judges the rest; all met → review, else a remediation task →
//!   implementing. A check that can't run at all blocks on the developer; a
//!   fix that changed nothing goes to review unverified (D-062).
//!
//! Deterministic code enforces what a model must not decide: the lifecycle,
//! the attempt and time budgets, per-run timeouts, loop detection, and what
//! may run. The feature document is the checkpoint: a restarted daemon picks
//! up where the last one stopped. One feature, one run at a time (MVP). No
//! lock is held across a model call, a run or a check.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use otter_core::feature::{
    Criterion, Decider, DecisionKind, DecisionRequest, DecisionStatus, Evidence, EvidenceKind,
    Feature, FeatureEvent, FeatureStatus, MessageRole, Risk, RunState, Task, TaskStatus,
    VerificationMark,
};
use otter_core::{DecisionId, EvidenceId, FeatureId, TaskId};
use otter_protocol::RpcError;

use crate::brain::{self, Context, Verdict as BrainVerdict};
use crate::daemon::{Daemon, RpcResult, workspace_not_found};
use crate::env::EnvMap;
use crate::features::{Change, message};
use crate::runtime::ToolCall;
use crate::runtime::policy::{self, Verdict};

/// Attempts at one task before the feature fails.
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
    /// Features being stepped right now.
    inflight: HashSet<FeatureId>,
    /// Decisions the brain has already been asked about.
    asked: HashSet<DecisionId>,
    /// When each feature's CI was last read.
    ci_polled: std::collections::HashMap<FeatureId, std::time::Instant>,
}

impl Daemon {
    /// Run the controller until the daemon stops.
    pub(crate) async fn run_controller(self: Arc<Self>) {
        let mut shutdown = self.shutdown_signal();
        let mut clocks = (std::time::Instant::now(), Utc::now());
        loop {
            tokio::select! {
                _ = self.feature_wake.notified() => {}
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
            // Off: features stay where they are until the developer acts.
            if self.brain_choice().controller.as_deref() == Some("off") {
                continue;
            }
            let ids: Vec<FeatureId> = self
                .features
                .lock()
                .await
                .list()
                .into_iter()
                .filter(|f| {
                    !unanswered(f).is_empty()
                        || f.status.is_active()
                        || f.status == FeatureStatus::Review
                        // Blocked: on a decision the brain hasn't seen yet,
                        // or on nothing any more (see `unblock`).
                        || (f.status == FeatureStatus::Blocked && f.workspace_id.is_some())
                })
                .map(|f| f.id)
                .collect();
            for id in ids {
                if !self.controller.lock().unwrap().inflight.insert(id.clone()) {
                    continue;
                }
                let daemon = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = daemon.step(&id).await {
                        tracing::warn!(feature = %id, "controller: {}", e.message);
                        daemon
                            .fail(&id, &format!("Otter stopped: {}", e.message), None)
                            .await;
                    }
                    daemon.controller.lock().unwrap().inflight.remove(&id);
                });
            }
        }
    }

    async fn step(self: &Arc<Self>, id: &FeatureId) -> RpcResult<()> {
        let mut f = self.feature_get(id.as_str()).await?;
        // The developer said something: answer first (it may pause or go on).
        if !unanswered(&f).is_empty() {
            self.converse(&f).await?;
            f = self.feature_get(id.as_str()).await?;
        }
        match f.status {
            FeatureStatus::Planning => self.plan_step(&f).await,
            FeatureStatus::Implementing => self.implement_step(&f).await,
            FeatureStatus::Verifying => self.verify_step(&f).await,
            FeatureStatus::Review => self.review_step(&f).await,
            FeatureStatus::Blocked => {
                if f.pending_decisions().next().is_some() {
                    self.decide_open(&f).await
                } else {
                    self.unblock(&f).await
                }
            }
            _ => Ok(()),
        }
    }

    /// Answer the developer's latest messages (D-049), and act on what they
    /// ask: go on (resume, unblock, start, retry) or pause. Instructions for
    /// the work reach the coding agent with its next turn.
    async fn converse(self: &Arc<Self>, f: &Feature) -> RpcResult<()> {
        use crate::brain::Intent;
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
        let reply = match self.feature_workspace(f).await {
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
                            feature: f,
                            root: &root,
                            env: &env,
                        },
                        &text,
                        &say,
                    )
                    .await
            }
            Err(e) => Err(anyhow::anyhow!(e.message)),
        };
        let reply = reply.unwrap_or_else(|e| {
            tracing::warn!(feature = %f.id, "replying: {e:#}");
            crate::brain::Reply {
                text: crate::brain::status_text(f),
                intent: crate::brain::Intent::None,
            }
        });
        let intent = reply.intent;
        let reply_text = if reply.text.is_empty() {
            crate::brain::status_text(f)
        } else {
            reply.text
        };
        let mut stop = false;
        let applied = self.features.lock().await.apply(&f.id, None, |f, now| {
            let mut changes = Vec::new();
            let (m, c) = message(
                MessageRole::Controller,
                reply_text.clone(),
                Some(last.id.to_string()),
            );
            f.messages.push(m);
            changes.push(c);
            let move_to = |f: &mut Feature, to: FeatureStatus, why: &str| {
                f.transition(to, Some(why.into()), now)
                    .ok()
                    .map(Change::new)
            };
            let step = match (intent, f.status) {
                (Intent::Continue, FeatureStatus::Draft) => {
                    move_to(f, FeatureStatus::Planning, "You said go on")
                }
                (Intent::Continue, FeatureStatus::Paused) => {
                    let next = crate::features::resume_status(f);
                    move_to(f, next, "You said go on")
                }
                (Intent::Continue, FeatureStatus::Blocked)
                    if f.pending_decisions().next().is_none() =>
                {
                    let next = crate::features::resume_status(f);
                    move_to(f, next, "You said go on")
                }
                (Intent::Continue, FeatureStatus::Failed) => {
                    f.budget.iterations_used = 0;
                    for t in &mut f.tasks {
                        if t.status == TaskStatus::Failed {
                            t.status = TaskStatus::Pending;
                            t.attempts = 0;
                        }
                    }
                    let next = crate::features::resume_status(f);
                    move_to(f, next, "You said go on: retrying with a fresh budget")
                }
                (Intent::Pause, s)
                    if s.is_active()
                        || s == FeatureStatus::Blocked
                        || s == FeatureStatus::Review =>
                {
                    stop = true;
                    move_to(f, FeatureStatus::Paused, "Paused: you asked")
                }
                // The goal moved (D-050): plan again, keeping what is done.
                (Intent::Revise, s) if !s.is_terminal() => {
                    stop = true;
                    changes.extend(crate::features::replan(
                        f,
                        "You changed the goal: planning again",
                        now,
                    ));
                    None
                }
                // Satisfied: accept what's in review (its gates still apply),
                // or stop work in progress.
                (Intent::Finish, FeatureStatus::Review) => {
                    let accept = otter_core::feature::FeatureAction::Accept {
                        override_gates: false,
                    };
                    match crate::features::apply_action(f, &accept, last.id.as_str(), now) {
                        Ok(c) => {
                            changes.extend(c);
                            None
                        }
                        Err(e) => {
                            f.rationale.push(format!("Not done yet: {}", e.message));
                            None
                        }
                    }
                }
                (Intent::Finish, s) if s.is_active() || s == FeatureStatus::Blocked => {
                    stop = true;
                    move_to(f, FeatureStatus::Paused, "Stopped: you said that's enough")
                }
                _ => None,
            };
            changes.extend(step);
            Ok(changes)
        })?;
        self.feature_changed(&applied);
        self.stream(
            &f.id,
            &stream_id,
            MessageRole::Controller,
            reply_text.clone(),
            true,
        );
        // What reaches the coding agent (D-059): guidance as turns of their
        // own (each message's id is its command id, so it is queued once);
        // a redirect stops the current turn first; a question or chat
        // doesn't go at all.
        match routing(intent) {
            Routing::Queue => {
                let carried = self.carried(f);
                for m in open.iter().filter(|m| !carried.contains(m.id.as_str())) {
                    self.tell_runs(&f.id, m.id.as_str(), &m.text).await;
                }
            }
            Routing::Redirect => {
                self.redirect_runs(&f.id, last.id.as_str(), &text).await;
            }
            Routing::Nothing => {}
        }
        if stop {
            self.hold_runs(&f.id, false, "Paused").await;
        }
        Ok(())
    }

    /// A feature blocked on nothing that is still open (a decision that
    /// became moot, an interrupted run) picks up where its work stands.
    async fn unblock(&self, f: &Feature) -> RpcResult<()> {
        self.controller_apply(&f.id, FeatureStatus::Blocked, |f, now| {
            if f.workspace_id.is_none() || f.pending_decisions().next().is_some() {
                return Ok(vec![]);
            }
            let next = crate::features::resume_status(f);
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
        }
    }

    /// The workspace root and environment the feature works in.
    pub(crate) async fn feature_workspace(&self, f: &Feature) -> RpcResult<(PathBuf, EnvMap)> {
        let ws_id = f
            .workspace_id
            .clone()
            .ok_or_else(|| RpcError::conflict("the feature has no workspace"))?;
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

    /// Change a feature on behalf of the controller, if it is still `expect`.
    async fn controller_apply(
        &self,
        id: &FeatureId,
        expect: FeatureStatus,
        step: impl FnOnce(&mut Feature, chrono::DateTime<Utc>) -> RpcResult<Vec<Change>>,
    ) -> RpcResult<bool> {
        let mut moved = false;
        let applied = self.features.lock().await.apply(id, None, |f, now| {
            // The developer may have paused or cancelled meanwhile.
            if f.status != expect {
                moved = true;
                return Ok(vec![]);
            }
            step(f, now)
        })?;
        self.feature_changed(&applied);
        Ok(!moved)
    }

    /// Stop the feature with a reason (a limit, or an error), keeping the plan.
    pub(crate) async fn fail(&self, id: &FeatureId, reason: &str, limit: Option<&str>) {
        let reason = reason.to_owned();
        let limit = limit.map(String::from);
        let applied = self.features.lock().await.apply(id, None, |f, now| {
            if !f.status.can_become(FeatureStatus::Failed) {
                return Ok(vec![]);
            }
            let mut changes = Vec::new();
            if let Some(l) = limit {
                changes.push(Change::new(FeatureEvent::LimitReached { limit: l }));
            }
            changes.push(Change::new(
                f.transition(FeatureStatus::Failed, Some(reason.clone()), now)
                    .map_err(RpcError::conflict)?,
            ));
            let (m, c) = message(MessageRole::Controller, reason, None);
            f.messages.push(m);
            changes.push(c);
            Ok(changes)
        });
        match applied {
            Ok(a) => self.feature_changed(&a),
            Err(e) => tracing::warn!(feature = %id, "failing: {e:?}"),
        }
    }

    async fn plan_step(self: &Arc<Self>, f: &Feature) -> RpcResult<()> {
        if f.workspace_id.is_none() {
            self.controller_apply(&f.id, FeatureStatus::Planning, |f, now| {
                let e = f
                    .transition(
                        FeatureStatus::Blocked,
                        Some("Choose a workspace for this feature".into()),
                        now,
                    )
                    .map_err(RpcError::conflict)?;
                Ok(vec![Change::new(e)])
            })
            .await?;
            return Ok(());
        }
        if !f.tasks.is_empty() && !f.replan {
            // Back from a pause before any plan step was lost: carry on.
            self.controller_apply(&f.id, FeatureStatus::Planning, |f, now| {
                let next = crate::features::resume_status(f);
                Ok(vec![Change::new(
                    f.transition(next, None, now).map_err(RpcError::conflict)?,
                )])
            })
            .await?;
            return Ok(());
        }
        let (root, env) = self.feature_workspace(f).await?;
        let brain = brain::select(&env, &self.brain_choice());
        let plan = brain
            .plan(&Context {
                feature: f,
                root: &root,
                env: &env,
            })
            .await
            .map_err(|e| RpcError::internal(format!("planning failed: {e:#}")))?;
        let verify = plan.verify_command.clone().filter(|c| !c.trim().is_empty());
        let check_verdict = verify
            .as_ref()
            .map(|c| policy::classify(&ToolCall::Command { line: c.clone() }, &root));
        let brain_name = brain.name();
        self.controller_apply(&f.id, FeatureStatus::Planning, move |f, now| {
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
            let kept: Vec<Task> = f
                .tasks
                .iter()
                .filter(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Skipped))
                .cloned()
                .collect();
            f.replan = false;
            let ids: Vec<TaskId> = plan.tasks.iter().map(|_| TaskId::generate()).collect();
            let fresh: Vec<Task> = plan
                .tasks
                .iter()
                .enumerate()
                .map(|(i, t)| Task {
                    id: ids[i].clone(),
                    title: t.title.clone(),
                    detail: Some(t.detail.clone()).filter(|d| !d.is_empty()),
                    status: TaskStatus::Pending,
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
                })
                .collect();
            f.tasks = kept.into_iter().chain(fresh).collect();
            f.verify_command = verify.clone();
            f.rationale
                .push(format!("Plan ({brain_name}): {}", plan.rationale));
            let mut changes = vec![Change::new(FeatureEvent::PlanSet {
                tasks: f.tasks.len(),
                criteria: f.acceptance.len(),
            })];
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
                        task_id: None,
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
                    });
                    f.verify_decision = Some(id.clone());
                    changes.push(Change::new(FeatureEvent::DecisionRequested {
                        decision_id: id,
                        kind: DecisionKind::ToolPermission,
                        risk: *risk,
                    }));
                }
                (Some(_), Some(Verdict::Deny { .. })) => f.verify_command = None,
                _ => {}
            }
            let mut text = format!(
                "{}: {} task(s).\n{}\nAcceptance criteria:\n{}",
                if revised { "Revised plan" } else { "Plan" },
                f.tasks.len(),
                f.tasks
                    .iter()
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
                f.transition(FeatureStatus::Implementing, None, now)
                    .map_err(RpcError::conflict)?,
            ));
            Ok(changes)
        })
        .await?;
        Ok(())
    }

    /// Ask the brain about open decisions; escalate what it won't take.
    async fn decide_open(self: &Arc<Self>, f: &Feature) -> RpcResult<()> {
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
        let (root, env) = self.feature_workspace(f).await?;
        let brain = brain::select(&env, &self.brain_choice());
        for d in open {
            let verdict = brain
                .decide(
                    &Context {
                        feature: f,
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
                    .feature_get(f.id.as_str())
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

    async fn escalate(&self, id: &FeatureId, d: &DecisionRequest, why: &str) {
        let line = format!("Needs your decision: {} — {why}", d.summary);
        let applied = self.features.lock().await.apply(id, None, |f, now| {
            f.rationale.push(line.clone());
            let mut changes = Vec::new();
            if f.status.can_become(FeatureStatus::Blocked) {
                changes.push(Change::new(
                    f.transition(FeatureStatus::Blocked, Some(line.clone()), now)
                        .map_err(RpcError::conflict)?,
                ));
            }
            Ok(changes)
        });
        if let Ok(a) = applied {
            self.feature_changed(&a);
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
        let ids: Vec<FeatureId> = self
            .features
            .lock()
            .await
            .list()
            .into_iter()
            .filter(|f| f.live_run().is_some_and(|r| r.working_since.is_some()))
            .map(|f| f.id)
            .collect();
        for id in ids {
            let applied = self.features.lock().await.apply(&id, None, |f, now| {
                for r in f.runs.iter_mut().filter(|r| !r.state.is_over()) {
                    r.skip(gap, now);
                }
                Ok(vec![])
            });
            if let Ok(a) = applied {
                self.feature_changed(&a);
            }
        }
    }

    async fn implement_step(self: &Arc<Self>, f: &Feature) -> RpcResult<()> {
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
            return self.decide_open(f).await;
        }
        if f.pending_decisions().next().is_some() {
            return self.decide_open(f).await;
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
        let done = |id: &TaskId| {
            f.task(id)
                .is_some_and(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Skipped))
        };
        let ready = |t: &&Task| t.depends_on.iter().all(done);
        let next = f
            .tasks
            .iter()
            .filter(ready)
            .find(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::Running))
            .or_else(|| {
                f.tasks
                    .iter()
                    .filter(ready)
                    .find(|t| t.status == TaskStatus::Failed && t.attempts < MAX_TASK_ATTEMPTS)
            })
            .cloned();
        let Some(task) = next else {
            if f.tasks
                .iter()
                .all(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Skipped))
            {
                self.controller_apply(&f.id, FeatureStatus::Implementing, |f, now| {
                    Ok(vec![Change::new(
                        f.transition(FeatureStatus::Verifying, None, now)
                            .map_err(RpcError::conflict)?,
                    )])
                })
                .await?;
            } else {
                let stuck = f
                    .tasks
                    .iter()
                    .find(|t| t.status == TaskStatus::Failed)
                    .map(|t| {
                        format!(
                            "“{}” failed {} times: {}",
                            t.title,
                            t.attempts,
                            t.last_error.clone().unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "no task can start".into());
                self.fail(&f.id, &format!("Stopped: {stuck}"), Some("attempts"))
                    .await;
            }
            return Ok(());
        };
        // An interrupted run (restart, pause, takeover) continues its
        // conversation; a failed one starts over with what went wrong.
        let last = f.runs.iter().rev().find(|r| r.task_id == task.id);
        let resume = last
            .filter(|r| matches!(r.state, RunState::Cancelled | RunState::HandedOff))
            .and_then(|r| r.provider_session_id.clone());
        let counts = resume.is_none();
        if counts && f.budget.iterations_used >= f.budget.max_iterations {
            let reason = format!(
                "Stopped: all {} attempts are used up",
                f.budget.max_iterations
            );
            self.fail(&f.id, &reason, Some("attempts")).await;
            return Ok(());
        }
        let prompt = if resume.is_some() {
            // Messages a turn of the task's conversation already carries
            // (queued, sent or delivered) aren't restated.
            let carried = last
                .and_then(|r| r.conversation_id.as_ref())
                .and_then(|c| self.conversations.get(c.as_str()))
                .map(|c| carried_by(&c))
                .unwrap_or_default();
            continue_prompt(f, &task, last, &carried)
        } else {
            task_prompt(f, &task)
        };
        if let Err(e) = self
            .start_run(&f.id, &task.id, prompt, resume, counts)
            .await
        {
            self.fail(
                &f.id,
                &format!("Couldn't start the coding agent: {}", e.message),
                None,
            )
            .await;
        }
        Ok(())
    }

    async fn verify_step(self: &Arc<Self>, f: &Feature) -> RpcResult<()> {
        if f.pending_decisions().next().is_some() {
            return self.decide_open(f).await;
        }
        let (root, env) = self.feature_workspace(f).await?;
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
            self.controller_apply(&f.id, FeatureStatus::Verifying, |f, _| {
                match decided {
                    Some((DecisionStatus::Answered, Some(a))) if a == WITHOUT_CHECK => {
                        f.verify_command = None;
                        f.rationale
                            .push("Going on without the check, as you said.".into());
                    }
                    Some((DecisionStatus::Approved | DecisionStatus::Answered, _)) => {
                        f.verify_approved = true
                    }
                    _ => {
                        f.verify_command = None;
                        f.rationale
                            .push("The check wasn't allowed; judging without it.".into());
                    }
                }
                Ok(vec![])
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
                .controller_apply(&f.id, FeatureStatus::Verifying, move |f, _| {
                    let mut changes = Vec::new();
                    for e in ev {
                        changes.push(Change::new(FeatureEvent::EvidenceAdded {
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
        let f = self.feature_get(f.id.as_str()).await?;
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
                    feature: &f,
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
        self.controller_apply(&f.id, FeatureStatus::Verifying, move |f, now| {
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
                f.rationale.push("Verified: every criterion is met.".into());
                let (m, c) = message(
                    MessageRole::Controller,
                    format!("Verified.\n{summary}"),
                    None,
                );
                f.messages.push(m);
                changes.push(c);
                changes.push(Change::new(
                    f.transition(
                        FeatureStatus::Review,
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
                f.rationale.push(format!(
                    "Not verified: {unproven}; nothing changed after a fix"
                ));
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
                        FeatureStatus::Review,
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
                let id = TaskId::generate();
                f.tasks.push(Task {
                    id: id.clone(),
                    title: "Fix what verification found".into(),
                    detail: Some(fix.clone()),
                    status: TaskStatus::Pending,
                    depends_on: vec![],
                    workspace_id: f.workspace_id.clone(),
                    session_id: None,
                    attempts: 0,
                    last_error: None,
                });
                f.rationale.push(format!("Not verified: {fix}"));
                let (m, c) = message(
                    MessageRole::Controller,
                    format!("Not verified yet.\n{summary}\nNext: {fix}"),
                    None,
                );
                f.messages.push(m);
                changes.push(c);
                changes.push(Change::new(FeatureEvent::TaskChanged {
                    task_id: id,
                    status: TaskStatus::Pending,
                }));
                changes.push(Change::new(
                    f.transition(
                        FeatureStatus::Implementing,
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
    /// task: the work isn't what's wrong (D-062).
    async fn check_cant_run(
        &self,
        f: &Feature,
        cmd: &str,
        check: Evidence,
        why: &str,
    ) -> RpcResult<()> {
        let (cmd, why) = (cmd.to_owned(), why.to_owned());
        self.controller_apply(&f.id, FeatureStatus::Verifying, move |f, now| {
            let mut changes = vec![Change::new(FeatureEvent::EvidenceAdded {
                evidence_id: check.id.clone(),
                kind: check.kind,
                ok: check.ok,
            })];
            f.evidence.push(check);
            let id = DecisionId::generate();
            f.decisions.push(DecisionRequest {
                id: id.clone(),
                task_id: None,
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
            });
            f.verify_decision = Some(id.clone());
            f.verify_approved = false;
            changes.push(Change::new(FeatureEvent::DecisionRequested {
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
                    FeatureStatus::Blocked,
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

    /// Whether `cmd` may run for this feature: policy, or someone approved
    /// it (asked once, as a decision).
    pub(crate) async fn allowance(
        &self,
        f: &Feature,
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
                let applied = self.features.lock().await.apply(&f.id, None, |f, _| {
                    if !f.allowed_commands.contains(&cmd) {
                        f.allowed_commands.push(cmd.clone());
                    }
                    Ok(vec![])
                })?;
                self.feature_changed(&applied);
                Allowance::Allowed
            }
            Some(DecisionStatus::Pending) => Allowance::Pending,
            Some(_) => Allowance::Denied("not allowed".into()),
            None => {
                let applied = self
                    .features
                    .lock()
                    .await
                    .apply(&f.id, None, move |f, now| {
                        let id = DecisionId::generate();
                        f.decisions.push(DecisionRequest {
                            id: id.clone(),
                            task_id: None,
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
                        });
                        Ok(vec![Change::new(FeatureEvent::DecisionRequested {
                            decision_id: id,
                            kind: DecisionKind::ToolPermission,
                            risk,
                        })])
                    })?;
                self.feature_changed(&applied);
                Allowance::Pending
            }
        };
        Ok(allowance)
    }

    /// Where a feature's checks keep their files (screenshots).
    pub(crate) fn artifacts_dir(&self, id: &FeatureId) -> PathBuf {
        self.paths.features_dir.join(id.as_str()).join("artifacts")
    }

    /// The project's browser checks (D-046), run in a real headless browser.
    /// `None` while a preview command waits to be allowed.
    async fn browser_checks(
        &self,
        f: &Feature,
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
                    .review(
                        &Context {
                            feature: f,
                            root,
                            env,
                        },
                        &check.name,
                        shot,
                    )
                    .await
            {
                evidence.push(Evidence {
                    id: EvidenceId::generate(),
                    task_id: None,
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
    async fn review_step(self: &Arc<Self>, f: &Feature) -> RpcResult<()> {
        use crate::delivery::{self, Git, GitHub};
        if f.pending_decisions().any(|d| !d.user_only) {
            return self.decide_open(f).await;
        }
        let (root, env) = self.feature_workspace(f).await?;
        let git = Git {
            root: &root,
            env: &env,
        };
        let branch = git.publishable_branch().await;
        let gh = GitHub::find(&root, &env);
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
        if let (Some(branch), Some(gh)) = (&branch, &gh)
            && !d.as_ref().is_some_and(|d| d.declined)
        {
            let head = git.head().await.ok();
            let published = d.as_ref().is_some_and(|d| d.pr_url.is_some());
            if !published || d.as_ref().and_then(|d| d.head.clone()) != head {
                // Pushing is an external action: the developer allows it once.
                let cmd = format!("git push origin {branch}");
                match self
                    .allowance(f, &cmd, "and open a pull request", &root)
                    .await?
                {
                    Allowance::Pending => {}
                    Allowance::Denied(_) => {
                        d = Some(otter_core::feature::Delivery {
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
                        d = Some(otter_core::feature::Delivery {
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
                    Err(e) => tracing::warn!(feature = %f.id, "reading CI: {e:#}"),
                }
                let failed: Vec<_> = dl
                    .ci
                    .iter()
                    .filter(|c| matches!(c.state.as_str(), "fail" | "cancel"))
                    .cloned()
                    .collect();
                if !failed.is_empty() {
                    let mut text = String::new();
                    for c in &failed {
                        text.push_str(&format!(
                            "{}: {}\n",
                            c.name,
                            c.description.clone().unwrap_or_default()
                        ));
                        if let Some(log) = gh.failed_log(c).await {
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
                        remediation = Some(format!(
                            "CI failed ({}):\n{}",
                            names.join(", "),
                            text.trim()
                        ));
                        let mut e = evidence_item(
                            EvidenceKind::Ci,
                            &format!("CI failed: {}", names.join(", ")),
                            Some(text),
                        );
                        e.ok = Some(false);
                        e.uri = failed[0].url.clone();
                        evidence.push(e);
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
        {
            return Ok(());
        }
        let mut posted = None;
        self.controller_apply(&f.id, FeatureStatus::Review, |f, now| {
            let mut changes = Vec::new();
            f.delivery = d.clone();
            for e in evidence.clone() {
                changes.push(Change::new(FeatureEvent::EvidenceAdded {
                    evidence_id: e.id.clone(),
                    kind: e.kind,
                    ok: e.ok,
                }));
                f.evidence.push(e);
            }
            for n in &notes {
                f.rationale.push(n.clone());
                let (m, c) = message(MessageRole::Controller, n.clone(), None);
                f.messages.push(m);
                changes.push(c);
            }
            f.gates = gates.clone();
            if let Some(fix) = &remediation {
                let id = TaskId::generate();
                f.tasks.push(Task {
                    id: id.clone(),
                    title: "Fix what CI found".into(),
                    detail: Some(fix.clone()),
                    status: TaskStatus::Pending,
                    depends_on: vec![],
                    workspace_id: f.workspace_id.clone(),
                    session_id: None,
                    attempts: 0,
                    last_error: None,
                });
                f.report = None;
                f.rationale.push("CI failed: back to implementing.".into());
                changes.push(Change::new(FeatureEvent::TaskChanged { task_id: id, status: TaskStatus::Pending }));
                changes.push(Change::new(
                    f.transition(FeatureStatus::Implementing, Some("CI failed".into()), now)
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
    async fn workspace_base(&self, f: &Feature) -> Option<String> {
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
    fn ci_due(&self, id: &FeatureId) -> bool {
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
        task_id: None,
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
fn pr_body(f: &Feature) -> String {
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
/// (the request that created the feature isn't one).
pub(crate) fn unanswered(f: &Feature) -> Vec<otter_core::feature::Message> {
    let answered = f.messages.iter().rposition(|m| {
        m.role == MessageRole::Controller
            && m.correlation_id
                .as_deref()
                .is_some_and(|c| c.starts_with("msg_"))
    });
    f.messages
        .iter()
        .enumerate()
        .skip(answered.map_or(1, |i| i + 1))
        .filter(|(_, m)| m.role == MessageRole::User)
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
/// (`artifact:<name>`, served by `feature.artifact`).
fn browser_evidence(
    title: &str,
    ok: Option<bool>,
    detail: String,
    shot: Option<&std::path::Path>,
) -> Evidence {
    Evidence {
        id: EvidenceId::generate(),
        task_id: None,
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

/// The last runs of one task all failed the same way, since the work last
/// (re)started — a retry starts counting afresh.
fn repeating_failure(f: &Feature) -> Option<String> {
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
        r.state == RunState::Failed && r.task_id == first.task_id && r.summary == first.summary
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
    /// The messages the feature's live runs' conversations already carry.
    fn carried(&self, f: &Feature) -> std::collections::HashSet<String> {
        f.runs
            .iter()
            .filter(|r| !r.state.is_over())
            .filter_map(|r| r.conversation_id.as_ref())
            .filter_map(|c| self.conversations.get(c.as_str()))
            .flat_map(|c| carried_by(&c))
            .collect()
    }
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
    f: &Feature,
    since: Option<chrono::DateTime<Utc>>,
    carried: &std::collections::HashSet<String>,
) -> String {
    let notes: Vec<String> = f
        .messages
        .iter()
        .skip(1) // The request itself.
        .filter(|m| m.role == MessageRole::User && since.is_none_or(|s| m.at > s))
        .filter(|m| !carried.contains(m.id.as_str()))
        .map(|m| format!("- {}", m.text))
        .collect();
    if notes.is_empty() {
        String::new()
    } else {
        format!("\nFrom the developer:\n{}\n", notes.join("\n"))
    }
}

/// What a coding agent is told for a fresh run of `task`.
pub fn task_prompt(f: &Feature, task: &Task) -> String {
    let n = f
        .tasks
        .iter()
        .position(|t| t.id == task.id)
        .map_or(0, |i| i + 1);
    let mut p = format!(
        "Feature: {}\n\nRequest:\n{}\n",
        f.title,
        if f.request.is_empty() {
            &f.title
        } else {
            &f.request
        }
    );
    if !f.requirements.is_empty() {
        p.push_str(&format!(
            "\nRequirements:\n- {}\n",
            f.requirements.join("\n- ")
        ));
    }
    if !f.acceptance.is_empty() {
        p.push_str("\nAcceptance criteria:\n");
        for c in &f.acceptance {
            p.push_str(&format!("- {}\n", c.text));
        }
    }
    p.push_str(&format!(
        "\nYour task ({n} of {}): {}\n",
        f.tasks.len(),
        task.title
    ));
    if let Some(d) = &task.detail {
        p.push_str(&format!("{d}\n"));
    }
    if let Some(e) = &task.last_error {
        p.push_str(&format!("\nThe previous attempt failed: {e}\n"));
    }
    p.push_str(&developer_notes(f, None, &Default::default()));
    if let Some(cmd) = &f.verify_command {
        p.push_str(&format!(
            "\nThe work is checked with `{cmd}`; run it before you finish.\n"
        ));
    }
    p
}

/// What a resumed run is told: carry on, plus anything new from the developer.
fn continue_prompt(
    f: &Feature,
    task: &Task,
    last: Option<&otter_core::feature::Run>,
    carried: &std::collections::HashSet<String>,
) -> String {
    // A turn cut off says so (D-063): by a limit, what to do about it.
    let cut = last
        .filter(|r| r.state == RunState::Cancelled)
        .map(|r| r.summary.as_deref().unwrap_or_default());
    let opening = match cut {
        Some(why) if why.starts_with(OUT_OF_TIME) || why.starts_with(NO_PROGRESS) => format!(
            "You ran out of time on “{}” ({}). Continue where you left off: \
             do what the task still needs first, then end your turn with a short report.",
            task.title,
            why.to_lowercase()
        ),
        Some(_) => "Your last turn was interrupted before it finished. \
                    Continue the task where you left off."
            .into(),
        None => "Continue the task where you left off.".into(),
    };
    format!(
        "{opening}{}",
        developer_notes(f, last.map(|r| r.started_at), carried)
    )
}

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
        task_id: None,
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
        // A question, or a message about the feature itself, is Otter's.
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
    use otter_core::feature::Run;

    fn feature_with_runs(summaries: &[(&str, RunState)]) -> Feature {
        let mut f = Feature::new("t".into(), "r".into(), Utc::now());
        let task = TaskId::generate();
        for (s, state) in summaries {
            f.runs.push(Run {
                id: RunId::generate(),
                task_id: task.clone(),
                runtime: "claude".into(),
                state: *state,
                started_at: Utc::now(),
                ended_at: None,
                summary: Some((*s).into()),
                provider_session_id: None,
                activity: vec![],
                conversation_id: None,
                turn_id: None,
                worked_ms: 0,
                working_since: None,
            });
        }
        f
    }

    #[test]
    fn a_resumed_turn_says_why_the_last_one_was_cut_off() {
        let task = Task {
            id: TaskId::generate(),
            title: "Add CSV export".into(),
            detail: None,
            status: TaskStatus::Pending,
            depends_on: vec![],
            workspace_id: None,
            session_id: None,
            attempts: 1,
            last_error: None,
        };
        let none = HashSet::new();
        let prompt = |summary: &str, state| {
            let f = feature_with_runs(&[(summary, state)]);
            continue_prompt(&f, &task, f.runs.last(), &none)
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
            "Continue the task where you left off."
        );
        assert_eq!(span(Duration::from_millis(1500)), "1 s");
        assert_eq!(span(Duration::from_secs(1800)), "30 min");
    }

    #[test]
    fn the_same_failure_three_times_is_a_loop() {
        use RunState::*;
        assert!(repeating_failure(&feature_with_runs(&[("x", Failed), ("x", Failed)])).is_none());
        assert_eq!(
            repeating_failure(&feature_with_runs(&[
                ("x", Failed),
                ("x", Failed),
                ("x", Failed)
            ])),
            Some("x".into())
        );
        assert!(
            repeating_failure(&feature_with_runs(&[
                ("x", Failed),
                ("y", Failed),
                ("x", Failed)
            ]))
            .is_none()
        );
        assert!(
            repeating_failure(&feature_with_runs(&[
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
    fn a_task_prompt_carries_the_plan_and_what_went_wrong() {
        let mut f = Feature::new("CSV".into(), "Export it".into(), Utc::now());
        f.verify_command = Some("make test".into());
        f.acceptance.push(Criterion {
            id: "ac_1".into(),
            text: "has a header".into(),
            met: None,
            evidence: vec![],
            how: otter_core::feature::Proof::Diff,
        });
        let t = Task {
            id: TaskId::generate(),
            title: "Write it".into(),
            detail: None,
            status: TaskStatus::Failed,
            depends_on: vec![],
            workspace_id: None,
            session_id: None,
            attempts: 1,
            last_error: Some("tests failed".into()),
        };
        f.tasks.push(t.clone());
        let p = task_prompt(&f, &t);
        for want in [
            "Export it",
            "has a header",
            "1 of 1",
            "previous attempt failed: tests failed",
            "`make test`",
        ] {
            assert!(p.contains(want), "{want} in {p}");
        }
    }
}
