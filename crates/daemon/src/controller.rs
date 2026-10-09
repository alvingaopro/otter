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
//! - **verifying** → the check runs, evidence is recorded, the brain judges
//!   the criteria; all met → review, else a remediation task → implementing.
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
    Feature, FeatureEvent, FeatureStatus, MessageRole, RunState, Task, TaskStatus,
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

/// How long one run may take.
fn run_timeout() -> chrono::Duration {
    chrono::Duration::from_std(env_ms("OTTER_RUN_TIMEOUT_MS", 30 * 60 * 1000)).unwrap()
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
        loop {
            tokio::select! {
                _ = self.feature_wake.notified() => {}
                _ = tokio::time::sleep(tick()) => {}
                _ = shutdown.changed() => return,
            }
            let ids: Vec<FeatureId> = self
                .features
                .lock()
                .await
                .list()
                .into_iter()
                .filter(|f| {
                    f.status.is_active()
                        || f.status == FeatureStatus::Review
                        // Blocked on a decision the brain hasn't seen yet.
                        || (f.status == FeatureStatus::Blocked && f.pending_decisions().next().is_some())
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
                            .fail(
                                &id,
                                &format!("The Control Agent stopped: {}", e.message),
                                None,
                            )
                            .await;
                    }
                    daemon.controller.lock().unwrap().inflight.remove(&id);
                });
            }
        }
    }

    async fn step(self: &Arc<Self>, id: &FeatureId) -> RpcResult<()> {
        let f = self.feature_get(id.as_str()).await?;
        match f.status {
            FeatureStatus::Planning => self.plan_step(&f).await,
            FeatureStatus::Implementing => self.implement_step(&f).await,
            FeatureStatus::Verifying => self.verify_step(&f).await,
            FeatureStatus::Review => self.review_step(&f).await,
            FeatureStatus::Blocked => self.decide_open(&f).await,
            _ => Ok(()),
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
        if !f.tasks.is_empty() {
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
        let brain = brain::select(&env);
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
                .map(|(i, text)| Criterion {
                    id: format!("ac_{}", i + 1),
                    text: text.clone(),
                    met: None,
                    evidence: vec![],
                })
                .collect();
            let ids: Vec<TaskId> = plan.tasks.iter().map(|_| TaskId::generate()).collect();
            f.tasks = plan
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
                "Plan: {} task(s).\n{}\nAcceptance criteria:\n{}",
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
        let (root, env) = self.feature_workspace(f).await?;
        let brain = brain::select(&env);
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
                // Policy kept it from the model: it's the developer's.
                self.escalate(
                    &f.id,
                    &d,
                    &format!(
                        "the Control Agent wanted to allow it ({rationale}), but {}",
                        e.message
                    ),
                )
                .await;
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

    async fn implement_step(self: &Arc<Self>, f: &Feature) -> RpcResult<()> {
        let now = Utc::now();
        if let Some(run) = f.live_run() {
            if run.started_at + run_timeout() < now {
                let mins = run_timeout().num_minutes().max(1);
                self.stop_runs(
                    &f.id,
                    RunState::Failed,
                    &format!("Timed out after {mins} min"),
                )
                .await;
            }
            return self.decide_open(f).await;
        }
        if f.pending_decisions().next().is_some() {
            return self.decide_open(f).await;
        }
        // Budgets and loops: deterministic, whatever the brain thinks.
        if let Some(since) = f.active_since
            && since + chrono::Duration::minutes(i64::from(f.budget.max_minutes)) < now
        {
            let reason = format!(
                "Stopped: the time budget ({} min) is used up",
                f.budget.max_minutes
            );
            self.fail(&f.id, &reason, Some("time")).await;
            return Ok(());
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
            continue_prompt(f, last.map(|r| r.started_at))
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
        // The check command: run if allowed; a denial drops it.
        if let (Some(_), false, Some(did)) =
            (&f.verify_command, f.verify_approved, &f.verify_decision)
        {
            let status = f.decisions.iter().find(|d| &d.id == did).map(|d| d.status);
            self.controller_apply(&f.id, FeatureStatus::Verifying, |f, _| {
                match status {
                    Some(DecisionStatus::Approved | DecisionStatus::Answered) => {
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
            evidence.push(run_check(cmd, &root, &env).await);
        }
        match self.browser_checks(f, &root, &env).await? {
            Some(more) => evidence.extend(more),
            // A preview command waits for someone to allow it.
            None => return Ok(()),
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
        let brain = brain::select(&env);
        let judgement = brain
            .judge(&Context {
                feature: &f,
                root: &root,
                env: &env,
            })
            .await
            .map_err(|e| RpcError::internal(format!("judging failed: {e:#}")))?;
        let fresh: Vec<EvidenceId> = evidence.iter().map(|e| e.id.clone()).collect();
        let any_failed = evidence.iter().any(|e| e.ok == Some(false) && !e.uncertain);
        self.controller_apply(&f.id, FeatureStatus::Verifying, move |f, now| {
            for j in &judgement.criteria {
                if let Some(c) = f.acceptance.iter_mut().find(|c| c.id == j.id) {
                    c.met = Some(j.met);
                    c.evidence = fresh.clone();
                }
            }
            let all_met = !any_failed && f.acceptance.iter().all(|c| c.met == Some(true));
            let mut changes = Vec::new();
            let summary = f
                .acceptance
                .iter()
                .map(|c| format!("{} {}", if c.met == Some(true) { "✓" } else { "✗" }, c.text))
                .collect::<Vec<_>>()
                .join("\n");
            if all_met {
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
            } else {
                let fix = if judgement.remediation.trim().is_empty() {
                    "Make the failing acceptance criteria pass.".to_owned()
                } else {
                    judgement.remediation.clone()
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
        let brain = brain::select(env);
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
                                &format!("Changes on {branch}"),
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
        let diffstat = evidence
            .iter()
            .find(|e| e.kind == EvidenceKind::Diff)
            .and_then(|e| e.detail.clone())
            .or_else(|| {
                f.evidence
                    .iter()
                    .rev()
                    .find(|e| e.kind == EvidenceKind::Diff)
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
    b.push_str("\n_Opened by Otter's Control Agent; publishing was approved by the developer. Otter doesn't merge._\n");
    b
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

fn developer_notes(f: &Feature, since: Option<chrono::DateTime<Utc>>) -> String {
    let notes: Vec<String> = f
        .messages
        .iter()
        .skip(1) // The request itself.
        .filter(|m| m.role == MessageRole::User && since.is_none_or(|s| m.at > s))
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
    p.push_str(&developer_notes(f, None));
    if let Some(cmd) = &f.verify_command {
        p.push_str(&format!(
            "\nThe work is checked with `{cmd}`; run it before you finish.\n"
        ));
    }
    p
}

/// What a resumed run is told: carry on, plus anything new from the developer.
fn continue_prompt(f: &Feature, since: Option<chrono::DateTime<Utc>>) -> String {
    format!(
        "Continue the task where you left off.{}",
        developer_notes(f, since)
    )
}

/// Run the check command in the workspace and record what happened.
async fn run_check(cmd: &str, root: &std::path::Path, env: &EnvMap) -> Evidence {
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
    let (ok, title, detail) = match result {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let tail: String = {
                let chars: Vec<char> = text.chars().collect();
                chars[chars.len().saturating_sub(CHECK_OUTPUT)..]
                    .iter()
                    .collect()
            };
            let code = out.status.code().map_or("signal".into(), |c| c.to_string());
            (out.status.success(), format!("`{cmd}` (exit {code})"), tail)
        }
        Ok(Err(e)) => (false, format!("`{cmd}` couldn't start"), e.to_string()),
        Err(_) => (
            false,
            format!("`{cmd}` timed out"),
            format!("after {}s", check_timeout().as_secs()),
        ),
    };
    Evidence {
        id: EvidenceId::generate(),
        task_id: None,
        criterion_id: None,
        kind: EvidenceKind::Test,
        title,
        ok: Some(ok),
        uri: None,
        detail: Some(detail).filter(|d| !d.trim().is_empty()),
        uncertain: false,
        at: started,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            });
        }
        f
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
    fn a_task_prompt_carries_the_plan_and_what_went_wrong() {
        let mut f = Feature::new("CSV".into(), "Export it".into(), Utc::now());
        f.verify_command = Some("make test".into());
        f.acceptance.push(Criterion {
            id: "ac_1".into(),
            text: "has a header".into(),
            met: None,
            evidence: vec![],
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
