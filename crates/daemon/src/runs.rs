//! Managed runs (D-044): one coding-agent run working on one feature task,
//! bridged to the feature — progress becomes history, permission requests
//! become [`DecisionRequest`]s decided by policy, the Control Agent or the
//! developer.
//!
//! A run is a child of `otterd` (its stdin/stdout are the protocol), so
//! unlike a tmux session it ends when the daemon does. Its conversation
//! doesn't: the run keeps the agent's session id, and a new daemon marks the
//! run interrupted ([`Daemon::recover_runs`]) so the controller resumes it
//! with that id.
//!
//! Handing over: [`Daemon::take_over`] stops the managed run and opens the
//! same conversation in an interactive agent session in the workspace; the
//! developer hands it back by quitting that session and choosing Hand back.
//! At no time do two processes drive one conversation.
#![allow(dead_code)] // TEMP: used by the controller in the next commit (D-045).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use otter_core::feature::{
    Decider, DecisionKind, DecisionRequest, DecisionStatus, Feature, FeatureEvent, FeatureStatus,
    MessageRole, Risk, Run, RunState, TaskStatus,
};
use otter_core::{DecisionId, FeatureId, RunId, SessionKind, SessionSpec, TaskId};
use otter_protocol::{RpcError, SessionCreate};
use tokio::sync::{mpsc, oneshot};

use crate::daemon::{Daemon, RpcResult, workspace_not_found};
use crate::features::{Change, message};
use crate::runtime::policy::{self, Proposal, Verdict};
use crate::runtime::{DecisionReply, RunHandle, RunSpec, RuntimeEvent, runtime};

/// The runtime features use (the only one with a managed mode so far).
pub const DEFAULT_RUNTIME: &str = "claude";

/// A run in progress, as the rest of the daemon reaches it.
pub(crate) struct LiveRun {
    pub feature: FeatureId,
    tx: mpsc::Sender<RunCmd>,
}

enum RunCmd {
    Decide {
        decision: DecisionId,
        reply: DecisionReply,
    },
    /// End the run; `state` is how it is recorded.
    Stop {
        state: RunState,
        reason: String,
        done: oneshot::Sender<()>,
    },
}

/// Live runs by id.
pub(crate) type Runs = std::sync::Mutex<HashMap<RunId, LiveRun>>;

impl Daemon {
    /// Start a run of `task`: fresh, or continuing `resume` (the agent's own
    /// conversation id). `counts` charges an attempt to the budget.
    pub(crate) async fn start_run(
        self: &Arc<Self>,
        feature_id: &FeatureId,
        task_id: &TaskId,
        prompt: String,
        resume: Option<String>,
        counts: bool,
    ) -> RpcResult<RunId> {
        let feature = self.feature_get(feature_id.as_str()).await?;
        if feature.live_run().is_some() {
            return Err(RpcError::conflict(
                "a run is already working on this feature",
            ));
        }
        let ws_id = feature
            .workspace_id
            .clone()
            .ok_or_else(|| RpcError::conflict("the feature has no workspace yet"))?;
        let ws = self
            .store
            .lock()
            .await
            .workspace(ws_id.as_str())
            .cloned()
            .ok_or_else(|| workspace_not_found(ws_id.as_str()))?;
        // Never two drivers for one conversation: not while a developer has it.
        if let Some(sid) = feature.task(task_id).and_then(|t| t.session_id.clone())
            && ws
                .session(sid.as_str())
                .is_some_and(|s| s.current_execution().is_some_and(|e| e.is_running()))
        {
            return Err(RpcError::conflict(
                "the task's interactive session is still running; quit it first",
            ));
        }
        let mut env = self.env_for_launch(&ws).await?;
        env.insert("OTTER_WORKSPACE_ID".into(), ws.id.to_string());
        env.insert("OTTER_FEATURE_ID".into(), feature_id.to_string());
        let rt = runtime(DEFAULT_RUNTIME)
            .ok_or_else(|| RpcError::unsupported("no managed agent runtime"))?;
        let root = PathBuf::from(&ws.root);
        let handle = rt
            .start(RunSpec {
                cwd: root.clone(),
                env,
                prompt,
                resume: resume.clone(),
                instructions: Some(INSTRUCTIONS.into()),
            })
            .await
            .map_err(|e| RpcError::internal(format!("starting the agent: {e:#}")))?;

        let run_id = RunId::generate();
        let rid = run_id.clone();
        let tid = task_id.clone();
        let applied = self
            .features
            .lock()
            .await
            .apply(feature_id, None, move |f, now| {
                let task = f
                    .task_mut(&tid)
                    .ok_or_else(|| RpcError::not_found(format!("no task `{tid}`")))?;
                task.status = TaskStatus::Running;
                if counts {
                    task.attempts += 1;
                }
                let mut changes = vec![Change::new(FeatureEvent::TaskChanged {
                    task_id: tid.clone(),
                    status: TaskStatus::Running,
                })];
                if counts {
                    f.budget.iterations_used += 1;
                }
                f.runs.push(Run {
                    id: rid.clone(),
                    task_id: tid.clone(),
                    runtime: DEFAULT_RUNTIME.into(),
                    state: RunState::Running,
                    started_at: now,
                    ended_at: None,
                    summary: None,
                    provider_session_id: resume,
                });
                changes.push(
                    Change::new(FeatureEvent::RunStarted {
                        run_id: rid.clone(),
                        task_id: tid.clone(),
                        runtime: DEFAULT_RUNTIME.into(),
                    })
                    .because(rid.as_str()),
                );
                Ok(changes)
            })?;
        self.feature_changed(&applied);

        let (tx, rx) = mpsc::channel(32);
        self.runs.lock().unwrap().insert(
            run_id.clone(),
            LiveRun {
                feature: feature_id.clone(),
                tx,
            },
        );
        let daemon = self.clone();
        let (fid, rid) = (feature_id.clone(), run_id.clone());
        tokio::spawn(async move {
            daemon.pump(fid, rid.clone(), root, handle, rx).await;
            daemon.runs.lock().unwrap().remove(&rid);
            daemon.feature_wake.notify_one();
        });
        Ok(run_id)
    }

    /// Follow one run until it ends.
    async fn pump(
        self: &Arc<Self>,
        feature: FeatureId,
        run: RunId,
        root: PathBuf,
        mut handle: Box<dyn RunHandle>,
        mut rx: mpsc::Receiver<RunCmd>,
    ) {
        // Our decision ids → the runtime's request ids.
        let mut asks: HashMap<DecisionId, String> = HashMap::new();
        let mut last_text: Option<String> = None;
        loop {
            tokio::select! {
                ev = handle.next_event() => {
                    let Some(ev) = ev else { break };
                    match ev {
                        RuntimeEvent::Session { id } => {
                            self.update_run(&feature, &run, |r| r.provider_session_id = Some(id)).await;
                        }
                        RuntimeEvent::Text { text } => last_text = Some(text),
                        RuntimeEvent::Tool { .. } => {}
                        RuntimeEvent::DecisionNeeded(ask) => {
                            let verdict = policy::classify(&ask.call, &root);
                            match self.record_ask(&feature, &run, &ask, &verdict).await {
                                Some(id) => {
                                    asks.insert(id, ask.request_id);
                                }
                                None => {
                                    let reply = match verdict {
                                        Verdict::Deny { why } => DecisionReply::Deny { message: why },
                                        _ => DecisionReply::Allow,
                                    };
                                    let _ = handle.decide(&ask.request_id, reply).await;
                                }
                            }
                        }
                        RuntimeEvent::TurnEnded { ok, summary, .. } => {
                            let summary = summary.or(last_text.take());
                            let _ = handle.cancel().await;
                            self.finish_run(&feature, &run, if ok { RunState::Completed } else { RunState::Failed }, summary).await;
                            return;
                        }
                        RuntimeEvent::Exited { error, .. } => {
                            let why = error.unwrap_or_else(|| "the agent exited before finishing".into());
                            self.finish_run(&feature, &run, RunState::Failed, Some(why)).await;
                            return;
                        }
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(RunCmd::Decide { decision, reply }) => {
                            if let Some(req) = asks.remove(&decision) {
                                let _ = handle.decide(&req, reply).await;
                                self.update_run(&feature, &run, |r| r.state = RunState::Running).await;
                            }
                        }
                        Some(RunCmd::Stop { state, reason, done }) => {
                            let _ = handle.cancel().await;
                            self.finish_run(&feature, &run, state, Some(reason)).await;
                            let _ = done.send(());
                            return;
                        }
                        None => {
                            let _ = handle.cancel().await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn update_run(&self, feature: &FeatureId, run: &RunId, f: impl FnOnce(&mut Run)) {
        let mut f = Some(f);
        let applied = self.features.lock().await.apply(feature, None, |feat, _| {
            if let Some(r) = feat.run_mut(run) {
                let before = r.state;
                (f.take().unwrap())(r);
                if r.state != before {
                    return Ok(vec![Change::new(FeatureEvent::RunChanged {
                        run_id: run.clone(),
                        state: r.state,
                    })]);
                }
            }
            Ok(vec![])
        });
        if let Ok(a) = applied {
            self.feature_changed(&a);
        }
    }

    /// Record a permission request. Returns the decision id when someone
    /// has to decide; `None` when policy decided (the request is recorded
    /// as decided, for the audit trail).
    async fn record_ask(
        &self,
        feature: &FeatureId,
        run: &RunId,
        ask: &crate::runtime::DecisionAsk,
        verdict: &Verdict,
    ) -> Option<DecisionId> {
        let now = Utc::now();
        let (kind, risk, user_only, why, status) = match verdict {
            Verdict::Allow => (
                DecisionKind::ToolPermission,
                Risk::Low,
                false,
                "routine inside the workspace".to_owned(),
                DecisionStatus::Approved,
            ),
            Verdict::Deny { why } => (
                DecisionKind::ToolPermission,
                Risk::High,
                true,
                why.clone(),
                DecisionStatus::Denied,
            ),
            Verdict::Ask {
                risk,
                kind,
                user_only,
                why,
            } => (
                *kind,
                *risk,
                *user_only,
                why.clone(),
                DecisionStatus::Pending,
            ),
        };
        let id = DecisionId::generate();
        let did = id.clone();
        let options = match &ask.call {
            crate::runtime::ToolCall::Question { options, .. } => options.clone(),
            _ => vec![],
        };
        let applied = self.features.lock().await.apply(feature, None, |f, _| {
            let task_id = f
                .runs
                .iter()
                .find(|r| &r.id == run)
                .map(|r| r.task_id.clone());
            f.decisions.push(DecisionRequest {
                id: did.clone(),
                task_id,
                run_id: Some(run.clone()),
                kind,
                risk,
                summary: ask.summary.clone(),
                detail: Some(why.clone()),
                options,
                status,
                decided_by: (status != DecisionStatus::Pending).then_some(Decider::Policy),
                answer: None,
                rationale: (status != DecisionStatus::Pending).then(|| why.clone()),
                created_at: now,
                decided_at: (status != DecisionStatus::Pending).then_some(now),
                user_only,
            });
            let mut changes = vec![
                Change::new(FeatureEvent::DecisionRequested {
                    decision_id: did.clone(),
                    kind,
                    risk,
                })
                .because(run.as_str()),
            ];
            if status != DecisionStatus::Pending {
                changes.push(
                    Change::new(FeatureEvent::DecisionResolved {
                        decision_id: did.clone(),
                        status,
                        by: Decider::Policy,
                    })
                    .because(run.as_str()),
                );
                return Ok(changes);
            }
            if let Some(r) = f.run_mut(run) {
                r.state = RunState::Waiting;
            }
            changes.push(Change::new(FeatureEvent::RunChanged {
                run_id: run.clone(),
                state: RunState::Waiting,
            }));
            // Only the developer can answer this one: tell them.
            if user_only && f.status.can_become(FeatureStatus::Blocked) {
                let reason = format!("Needs your approval: {}", ask.summary);
                if let Ok(e) = f.transition(FeatureStatus::Blocked, Some(reason), now) {
                    changes.push(Change::new(e));
                }
            }
            Ok(changes)
        });
        match applied {
            Ok(a) => {
                self.feature_changed(&a);
                (status == DecisionStatus::Pending).then_some(id)
            }
            Err(e) => {
                tracing::warn!("recording a decision: {e:?}");
                None
            }
        }
    }

    async fn finish_run(
        &self,
        feature: &FeatureId,
        run: &RunId,
        state: RunState,
        summary: Option<String>,
    ) {
        let applied = self.features.lock().await.apply(feature, None, |f, now| {
            let mut changes = Vec::new();
            let Some(r) = f.run_mut(run) else {
                return Ok(changes);
            };
            if r.state.is_over() {
                return Ok(changes);
            }
            r.state = state;
            r.ended_at = Some(now);
            r.summary = summary.clone();
            let task_id = r.task_id.clone();
            changes.push(Change::new(FeatureEvent::RunChanged {
                run_id: run.clone(),
                state,
            }));
            // Whatever it was asking for is moot now.
            for d in f.decisions.iter_mut() {
                if d.run_id.as_ref() == Some(run) && d.status == DecisionStatus::Pending {
                    d.status = DecisionStatus::Cancelled;
                    d.decided_at = Some(now);
                }
            }
            if let Some(text) = summary.clone().filter(|s| !s.trim().is_empty()) {
                let (m, c) = message(MessageRole::Agent, text, Some(run.to_string()));
                f.messages.push(m);
                changes.push(c);
            }
            if let Some(t) = f.task_mut(&task_id) {
                t.status = match state {
                    RunState::Completed => TaskStatus::Done,
                    RunState::Failed => TaskStatus::Failed,
                    // Stopped, not finished: it runs again later.
                    _ => TaskStatus::Pending,
                };
                if state == RunState::Failed {
                    t.last_error = summary.clone();
                }
                changes.push(Change::new(FeatureEvent::TaskChanged {
                    task_id,
                    status: t.status,
                }));
            }
            Ok(changes)
        });
        if let Ok(a) = applied {
            self.feature_changed(&a);
        }
    }

    /// Answer a pending decision as `by`. Policy has the last word: a model
    /// can't allow what is the developer's to allow, and nobody can turn a
    /// denial around.
    pub(crate) async fn decide(
        &self,
        feature_id: &FeatureId,
        decision_id: &DecisionId,
        allow: bool,
        answer: Option<String>,
        by: Decider,
        rationale: Option<String>,
    ) -> RpcResult<Feature> {
        let feature = self.feature_get(feature_id.as_str()).await?;
        let d = feature
            .decisions
            .iter()
            .find(|d| &d.id == decision_id)
            .ok_or_else(|| RpcError::not_found(format!("no decision `{decision_id}`")))?;
        let verdict = Verdict::Ask {
            risk: d.risk,
            kind: d.kind,
            user_only: d.user_only,
            why: d.detail.clone().unwrap_or_default(),
        };
        let previously_denied = d.status == DecisionStatus::Denied;
        policy::resolve(&verdict, previously_denied, &Proposal { by, allow })
            .map_err(RpcError::conflict)?;
        if d.status != DecisionStatus::Pending {
            return Err(RpcError::conflict("that decision has already been made"));
        }
        let did = decision_id.clone();
        let ans = answer.clone();
        let applied = self
            .features
            .lock()
            .await
            .apply(feature_id, None, move |f, now| {
                let d = f.decision_mut(&did).expect("checked");
                d.status = match (&ans, allow) {
                    (Some(_), _) => DecisionStatus::Answered,
                    (None, true) => DecisionStatus::Approved,
                    (None, false) => DecisionStatus::Denied,
                };
                d.answer = ans.clone();
                d.decided_by = Some(by);
                d.rationale = rationale;
                d.decided_at = Some(now);
                let status = d.status;
                let mut changes = vec![Change::new(FeatureEvent::DecisionResolved {
                    decision_id: did.clone(),
                    status,
                    by,
                })];
                if f.status == FeatureStatus::Blocked && f.pending_decisions().next().is_none() {
                    let next = crate::features::resume_status(f);
                    if let Ok(e) = f.transition(next, None, now) {
                        changes.push(Change::new(e));
                    }
                }
                Ok(changes)
            })?;
        self.feature_changed(&applied);
        self.forward_decision(feature_id, decision_id, allow, answer)
            .await;
        Ok(applied.feature)
    }

    /// Pass a decision made in the feature on to the run waiting for it.
    pub(crate) async fn forward_decision(
        &self,
        feature_id: &FeatureId,
        decision_id: &DecisionId,
        allow: bool,
        answer: Option<String>,
    ) {
        let reply = match (answer, allow) {
            (Some(text), _) => DecisionReply::Answer { text },
            (None, true) => DecisionReply::Allow,
            (None, false) => DecisionReply::Deny {
                message: "The developer declined this.".into(),
            },
        };
        let senders: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.feature == feature_id)
            .map(|r| r.tx.clone())
            .collect();
        for tx in senders {
            let _ = tx
                .send(RunCmd::Decide {
                    decision: decision_id.clone(),
                    reply: reply.clone(),
                })
                .await;
        }
    }

    /// Stop every run of a feature and wait until each has ended.
    pub(crate) async fn stop_runs(&self, feature_id: &FeatureId, state: RunState, reason: &str) {
        let senders: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.feature == feature_id)
            .map(|r| r.tx.clone())
            .collect();
        for tx in senders {
            let (done, wait) = oneshot::channel();
            if tx
                .send(RunCmd::Stop {
                    state,
                    reason: reason.to_owned(),
                    done,
                })
                .await
                .is_ok()
            {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(15), wait).await;
            }
        }
    }

    /// The developer takes the conversation: stop the managed run and open
    /// it in an interactive agent session. Returns the session's id.
    pub(crate) async fn take_over(self: &Arc<Self>, feature_id: &FeatureId) -> RpcResult<String> {
        self.stop_runs(feature_id, RunState::HandedOff, "You took over")
            .await;
        let feature = self.feature_get(feature_id.as_str()).await?;
        let ws = feature
            .workspace_id
            .clone()
            .ok_or_else(|| RpcError::conflict("the feature has no workspace yet"))?;
        let run = feature
            .runs
            .iter()
            .rev()
            .find(|r| r.provider_session_id.is_some())
            .cloned()
            .ok_or_else(|| RpcError::conflict("no agent conversation to take over yet"))?;
        let session = self
            .session_create(SessionCreate {
                workspace: ws.to_string(),
                spec: SessionSpec {
                    name: Some(format!("takeover-{}", &run.id.as_str()[4..])),
                    kind: SessionKind::Agent,
                    command: None,
                    provider: Some(run.runtime.clone()),
                    prompt: None,
                    resume: run.provider_session_id.clone(),
                },
            })
            .await?;
        let (tid, sid) = (run.task_id.clone(), session.id.clone());
        let applied = self
            .features
            .lock()
            .await
            .apply(feature_id, None, move |f, _| {
                if let Some(t) = f.task_mut(&tid) {
                    t.session_id = Some(sid);
                }
                Ok(vec![])
            })?;
        self.feature_changed(&applied);
        Ok(session.id.to_string())
    }

    /// After a restart: runs the old daemon left behind ended with it.
    pub(crate) async fn recover_runs(&self) {
        let ids: Vec<FeatureId> = self
            .features
            .lock()
            .await
            .list()
            .into_iter()
            .filter(|f| f.live_run().is_some())
            .map(|f| f.id)
            .collect();
        for id in ids {
            let applied = self.features.lock().await.apply(&id, None, |f, now| {
                let mut changes = Vec::new();
                let mut tasks = Vec::new();
                for r in f.runs.iter_mut().filter(|r| !r.state.is_over()) {
                    r.state = RunState::Cancelled;
                    r.ended_at = Some(now);
                    r.summary = Some("Interrupted: otterd restarted; resumable".into());
                    tasks.push(r.task_id.clone());
                    changes.push(Change::new(FeatureEvent::RunChanged {
                        run_id: r.id.clone(),
                        state: RunState::Cancelled,
                    }));
                }
                for d in f.decisions.iter_mut() {
                    if d.status == DecisionStatus::Pending && d.run_id.is_some() {
                        d.status = DecisionStatus::Cancelled;
                        d.decided_at = Some(now);
                    }
                }
                for t in tasks {
                    if let Some(t) = f.task_mut(&t) {
                        t.status = TaskStatus::Pending;
                    }
                }
                // A block that was about the interrupted run is moot.
                if f.status == FeatureStatus::Blocked && f.pending_decisions().next().is_none() {
                    let next = crate::features::resume_status(f);
                    if let Ok(e) = f.transition(next, None, now) {
                        changes.push(Change::new(e));
                    }
                }
                Ok(changes)
            });
            match applied {
                Ok(a) => self.feature_changed(&a),
                Err(e) => tracing::warn!(feature = %id, "recovering runs: {e:?}"),
            }
        }
    }
}

/// Appended to the coding agent's system prompt for managed runs.
const INSTRUCTIONS: &str = "You are working on one task of a feature, managed by Otter. \
Stay inside the working directory. Run the project's tests before you finish. \
End with a short summary of what you changed and how you checked it.";

/// The conversation id to resume a task with, if an earlier run has one.
pub fn resumable(f: &Feature, task: &TaskId) -> Option<String> {
    f.runs
        .iter()
        .rev()
        .filter(|r| &r.task_id == task)
        .find_map(|r| r.provider_session_id.clone())
}
