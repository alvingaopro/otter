//! Managed runs (D-044, D-055): one coding-agent run working on one feature
//! task, bridged to the feature — progress becomes history, permission
//! requests become [`DecisionRequest`]s decided by policy, the Control Agent
//! or the developer.
//!
//! A run serves a runtime conversation (`runtime::conversations`): the
//! [`Actor`] owns the agent and the conversation's state; the bridge here
//! listens to it and is the only thing that touches the feature. The task's
//! conversation continues across runs (a resumed run is a new run, a new
//! generation, of the same conversation); a fresh attempt starts a new one.
//!
//! A run is a child of `otterd` (its stdin/stdout are the protocol), so
//! unlike a tmux session it ends when the daemon does. Its conversation
//! doesn't: it keeps the agent's session id, and a new daemon marks the run
//! interrupted ([`Daemon::recover_runs`]) so the controller resumes it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use otter_core::conversation::{
    Applied, Command, Conversation, Initiator, InputBlock, NativeBinding, Op, TurnOutcome,
};
use otter_core::feature::{
    Decider, DecisionKind, DecisionRequest, DecisionStatus, Feature, FeatureEvent, FeatureStatus,
    MessageRole, Risk, Run, RunState, TaskStatus,
};
use otter_core::{ConversationId, DecisionId, FeatureId, RunId, TaskId};
use otter_protocol::RpcError;
use tokio::sync::{mpsc, oneshot};

use crate::daemon::{Daemon, RpcResult, workspace_not_found};
use crate::features::{Change, message};
use crate::runtime::actor::{Actor, ActorCmd, ActorEvent};
use crate::runtime::policy::{self, Proposal, Verdict};
use crate::runtime::{CheckDecision, DecisionAsk, DecisionReply, RunLimits, RunSpec, runtime};

/// The runtime features use (the only one with a managed mode so far).
pub const DEFAULT_RUNTIME: &str = "claude";

/// A run in progress, as the rest of the daemon reaches it.
pub(crate) struct LiveRun {
    pub feature: FeatureId,
    pub conversation: ConversationId,
    /// The run's actor.
    tx: mpsc::Sender<ActorCmd>,
    /// How the run is recorded when it is stopped, and why.
    stopping: Arc<std::sync::Mutex<Option<(RunState, String)>>>,
}

/// Live runs by id.
pub(crate) type Runs = std::sync::Mutex<HashMap<RunId, LiveRun>>;

impl Daemon {
    /// Start a run of `task`: fresh (a new conversation), or continuing
    /// `resume` (the agent's own conversation id) in the task's
    /// conversation. `counts` charges an attempt to the budget.
    pub(crate) async fn start_run(
        self: &Arc<Self>,
        feature_id: &FeatureId,
        task_id: &TaskId,
        prompt: String,
        resume: Option<String>,
        counts: bool,
    ) -> RpcResult<RunId> {
        let feature = self.feature_get(feature_id.as_str()).await?;
        if !feature.status.is_active() {
            return Err(RpcError::conflict(format!(
                "the feature is {}",
                feature.status.as_str()
            )));
        }
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
        let (backend, coding_model) = {
            let s = self.settings.read().unwrap();
            (s.coding_backend(), s.coding_model())
        };

        // The conversation: the task's own when continuing it, else a new one.
        // A run from before conversations brings its session id (a trusted
        // link: it is the run's own record), never a guess.
        let now = Utc::now();
        let continued = resume.as_ref().and_then(|sid| {
            feature
                .runs
                .iter()
                .rev()
                .find(|r| &r.task_id == task_id && r.provider_session_id.as_ref() == Some(sid))
                .and_then(|r| r.conversation_id.clone())
                .and_then(|id| self.conversations.get(id.as_str()))
        });
        // A conversation keeps the backend it began with (D-057).
        let (conversation_id, backend) = match continued {
            Some(c) => (c.id, c.backend),
            None => {
                let mut c = Conversation::new(DEFAULT_RUNTIME, &backend, ws.id.clone(), now);
                c.feature_id = Some(feature_id.clone());
                c.task_id = Some(task_id.clone());
                c.binding = resume
                    .clone()
                    .map(|session_id| NativeBinding { session_id });
                c.model = coding_model;
                let id = c.id.clone();
                self.conversations.insert(c).map_err(|e| {
                    RpcError::internal(format!("recording the conversation: {e:#}"))
                })?;
                (id, backend)
            }
        };
        let rt = runtime(DEFAULT_RUNTIME, &backend).ok_or_else(|| {
            RpcError::unsupported(format!("no managed agent runtime for `{backend}`"))
        })?;
        // The turn, then the run takes the conversation (a new generation).
        let run_id = RunId::generate();
        let missing = || RpcError::internal("the conversation went missing");
        let current = self
            .conversations
            .get(conversation_id.as_str())
            .ok_or_else(missing)?;
        if let Some(other) = &current.active_run {
            return Err(RpcError::conflict(format!(
                "run `{other}` is still serving this conversation"
            )));
        }
        let turn_id = otter_core::TurnId::generate();
        self.conversations
            .apply(
                &conversation_id,
                Op::Accept {
                    command_id: format!("{run_id}:start"),
                    fingerprint: prompt.clone(),
                    command: Command::SendTurn {
                        initiator: Initiator::Controller,
                        input: vec![InputBlock::Text {
                            text: prompt.clone(),
                        }],
                    },
                    turn_id: Some(turn_id.clone()),
                    at: now,
                },
            )
            .ok_or_else(missing)?
            .map_err(|e| RpcError::conflict(e.to_string()))?;
        let generation = match self.conversations.apply(
            &conversation_id,
            Op::Claim {
                run: run_id.clone(),
                at: now,
            },
        ) {
            Some(Ok(Applied::Claimed(g))) => g,
            Some(Err(e)) => return Err(RpcError::conflict(e.to_string())),
            _ => return Err(missing()),
        };
        let (native, model) = (
            current.binding.as_ref().map(|b| b.session_id.clone()),
            current.model.clone(),
        );
        tracing::info!(runtime = rt.id(), feature = %feature_id, conversation = %conversation_id, generation, resume = native.is_some(), "starting a managed run");
        let root = PathBuf::from(&ws.root);
        let handle = match rt
            .start(RunSpec {
                conversation_id: conversation_id.clone(),
                run_id: run_id.clone(),
                generation,
                cwd: root.clone(),
                env,
                resume: native.clone(),
                instructions: Some(INSTRUCTIONS.into()),
                model,
                limits: RunLimits::default(),
            })
            .await
        {
            Ok(h) => h,
            Err(e) => {
                self.end_conversation_run(
                    &conversation_id,
                    generation,
                    TurnOutcome::Failed,
                    "not delivered: the agent didn't start",
                );
                return Err(RpcError::internal(format!("starting the agent: {e:#}")));
            }
        };

        let rid = run_id.clone();
        let tid = task_id.clone();
        let (cid, turn) = (conversation_id.clone(), turn_id.clone());
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
                    provider_session_id: native,
                    activity: vec![],
                    conversation_id: Some(cid),
                    turn_id: Some(turn),
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
            });
        let applied = match applied {
            Ok(a) => a,
            Err(e) => {
                // The feature changed under us: don't leave the agent running.
                let mut handle = handle;
                let _ = handle.stop().await;
                self.end_conversation_run(
                    &conversation_id,
                    generation,
                    TurnOutcome::Cancelled,
                    "not delivered: the run didn't start",
                );
                return Err(e);
            }
        };
        self.feature_changed(&applied);

        let (tx, mailbox) = mpsc::channel(32);
        let (out, events) = mpsc::channel(256);
        let stopping = Arc::new(std::sync::Mutex::new(None));
        self.runs.lock().unwrap().insert(
            run_id.clone(),
            LiveRun {
                feature: feature_id.clone(),
                conversation: conversation_id.clone(),
                tx: tx.clone(),
                stopping: stopping.clone(),
            },
        );
        let actor = Actor::new(
            conversation_id,
            run_id.clone(),
            generation,
            self.conversations.clone(),
            handle,
            out,
        );
        tokio::spawn(actor.run(mailbox));
        let daemon = self.clone();
        let (fid, rid) = (feature_id.clone(), run_id.clone());
        tokio::spawn(async move {
            daemon
                .bridge(fid, rid.clone(), root, events, tx, stopping)
                .await;
            daemon.runs.lock().unwrap().remove(&rid);
            daemon.feature_wake.notify_one();
        });
        Ok(run_id)
    }

    /// A run that never got going: its generation ends, its turns too.
    fn end_conversation_run(
        &self,
        conversation: &ConversationId,
        generation: u64,
        outcome: TurnOutcome,
        why: &str,
    ) {
        let at = Utc::now();
        self.conversations.apply(
            conversation,
            Op::RunEnded {
                generation,
                outcome,
                at,
            },
        );
        self.conversations.apply(
            conversation,
            Op::CancelQueued {
                reason: why.into(),
                at,
            },
        );
    }

    /// Follow one run's actor until the run ends, keeping the feature in step.
    async fn bridge(
        self: &Arc<Self>,
        feature: FeatureId,
        run: RunId,
        root: PathBuf,
        mut events: mpsc::Receiver<ActorEvent>,
        actor: mpsc::Sender<ActorCmd>,
        stopping: Arc<std::sync::Mutex<Option<(RunState, String)>>>,
    ) {
        // What the agent said this turn (already in the conversation).
        let mut said: Vec<String> = Vec::new();
        // What it is saying right now, streamed (D-051), by block.
        let mut draft: Option<Draft> = None;
        let mut last_text: Option<String> = None;
        // How the run ends once its last turn is over.
        let mut ending: Option<(RunState, Option<String>, bool)> = None;
        while let Some(ev) = events.recv().await {
            match ev {
                ActorEvent::Bound { session_id } => {
                    self.update_run(&feature, &run, |r| r.provider_session_id = Some(session_id))
                        .await;
                }
                ActorEvent::TurnStarted { .. } => {}
                // Streamed: what the agent says shows up as it says it.
                ActorEvent::TextDelta { block, piece, .. } => {
                    let d = match &mut draft {
                        Some(d) if d.id == block.as_str() => d,
                        _ => draft.insert(Draft::new(block.to_string())),
                    };
                    if let Some(so_far) = d.push(&piece) {
                        self.stream(&feature, &d.id, MessageRole::Agent, so_far, false);
                    }
                }
                ActorEvent::Text { block, text, .. } => {
                    self.agent_said(&feature, &run, text.clone()).await;
                    // Kept now: the streamed draft is done.
                    if let Some(d) = draft.take_if(|d| d.id == block.as_str())
                        && d.started()
                    {
                        self.stream(&feature, &d.id, MessageRole::Agent, text.clone(), true);
                    }
                    said.push(text.trim().to_owned());
                    last_text = Some(text);
                }
                ActorEvent::ToolStarted { record, call } => {
                    self.run_activity(&feature, &run, activity_line(&record.name, &call))
                        .await;
                }
                ActorEvent::Ask { interaction, ask } => {
                    let verdict = policy::classify(&ask.call, &root);
                    let waiting = self
                        .record_ask(&feature, &run, &interaction, &ask, &verdict)
                        .await;
                    if !waiting {
                        let reply = match verdict {
                            Verdict::Deny { why } => DecisionReply::Deny { message: why },
                            _ => DecisionReply::Allow,
                        };
                        let _ = actor
                            .send(ActorCmd::Decide {
                                interaction,
                                reply,
                                by: Decider::Policy,
                            })
                            .await;
                    }
                }
                // Every tool call, before it runs (runtimes with a mandatory
                // hook): routine work goes on; a denial is recorded; the rest
                // comes back as a decision.
                ActorEvent::Check {
                    check_id,
                    tool,
                    call,
                    ..
                } => {
                    let verdict = policy::classify(&call, &root);
                    let decision = match &verdict {
                        Verdict::Allow => CheckDecision::Allow,
                        Verdict::Deny { why } => {
                            let ask = DecisionAsk {
                                request_id: check_id.clone(),
                                summary: crate::agents::claude_stream::summarize(&call, &tool),
                                tool,
                                call,
                                tool_use_id: None,
                                input_hash: String::new(),
                                questions: vec![],
                            };
                            self.record_ask(
                                &feature,
                                &run,
                                &DecisionId::generate(),
                                &ask,
                                &verdict,
                            )
                            .await;
                            CheckDecision::Deny {
                                reason: why.clone(),
                            }
                        }
                        Verdict::Ask { .. } => CheckDecision::Ask,
                    };
                    let _ = actor.send(ActorCmd::Check { check_id, decision }).await;
                }
                ActorEvent::TurnFinished {
                    outcome,
                    summary,
                    next,
                    ..
                } => {
                    tracing::debug!(?outcome, "run finished its turn");
                    let summary = summary.or(last_text.take());
                    // The turn's result repeats what was streamed: say it once.
                    let announce = summary
                        .as_deref()
                        .is_some_and(|s| !said.iter().any(|x| x == s.trim()));
                    said.clear();
                    if next.is_some() {
                        // The developer wrote meanwhile: the conversation goes on.
                        if announce && let Some(s) = summary.filter(|s| !s.trim().is_empty()) {
                            self.agent_said(&feature, &run, s).await;
                        }
                    } else {
                        let state = if outcome == TurnOutcome::Completed {
                            RunState::Completed
                        } else {
                            RunState::Failed
                        };
                        ending = Some((state, summary, announce));
                    }
                }
                ActorEvent::Ended { error, stopped, .. } => {
                    // otterd itself is going: the run ends with it, which
                    // isn't the run failing. Left as it is, the next otterd
                    // marks it interrupted and the controller resumes it.
                    if stopped.is_none() && self.shutting_down() {
                        tracing::info!(run = %run, "the run ended with otterd; it resumes after the restart");
                        return;
                    }
                    if let Some(done) = stopped {
                        let (state, reason) = stopping
                            .lock()
                            .unwrap()
                            .take()
                            .unwrap_or((RunState::Cancelled, "Stopped".into()));
                        self.finish_run(&feature, &run, state, Some(reason), true)
                            .await;
                        let _ = done.send(());
                    } else if let Some((state, summary, announce)) = ending.take() {
                        self.finish_run(&feature, &run, state, summary, announce)
                            .await;
                    } else {
                        let why =
                            error.unwrap_or_else(|| "the agent exited before finishing".into());
                        self.finish_run(&feature, &run, RunState::Failed, Some(why), true)
                            .await;
                    }
                    return;
                }
            }
        }
        // The actor went away without a word — with otterd, when it stops
        // (see above), or else it is a failure.
        if self.shutting_down() {
            return;
        }
        self.finish_run(
            &feature,
            &run,
            RunState::Failed,
            Some("the run ended unexpectedly".into()),
            true,
        )
        .await;
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

    /// Record a permission request under the interaction's id. True when
    /// someone has to decide; false when policy decided (the request is
    /// recorded as decided, for the audit trail).
    async fn record_ask(
        &self,
        feature: &FeatureId,
        run: &RunId,
        interaction: &DecisionId,
        ask: &crate::runtime::DecisionAsk,
        verdict: &Verdict,
    ) -> bool {
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
        let did = interaction.clone();
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
                status == DecisionStatus::Pending
            }
            Err(e) => {
                tracing::warn!("recording a decision: {e:?}");
                false
            }
        }
    }

    async fn finish_run(
        &self,
        feature: &FeatureId,
        run: &RunId,
        state: RunState,
        summary: Option<String>,
        announce: bool,
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
            if let Some(text) = summary.clone().filter(|s| announce && !s.trim().is_empty()) {
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
        self.forward_decision(feature_id, decision_id, allow, answer, None, by)
            .await;
        Ok(applied.feature)
    }

    /// What the coding agent said at the end of a turn that isn't the last.
    async fn agent_said(&self, feature: &FeatureId, run: &RunId, text: String) {
        let applied = self.features.lock().await.apply(feature, None, |f, _| {
            let (m, c) = message(MessageRole::Agent, text, Some(run.to_string()));
            f.messages.push(m);
            Ok(vec![c])
        });
        if let Ok(a) = applied {
            self.feature_changed(&a);
        }
    }

    /// Text being written right now, to the app (transient, D-051).
    pub(crate) fn stream(
        &self,
        feature: &FeatureId,
        stream_id: &str,
        role: MessageRole,
        text: String,
        done: bool,
    ) {
        self.events
            .emit_transient(otter_protocol::Event::FeatureStream {
                feature_id: feature.clone(),
                stream_id: stream_id.to_owned(),
                role,
                text,
                done,
            });
    }

    /// Note what the agent just did on its run (kept short: the last lines).
    async fn run_activity(&self, feature: &FeatureId, run: &RunId, line: String) {
        const KEEP: usize = 30;
        let applied = self.features.lock().await.apply(feature, None, |f, _| {
            if let Some(r) = f.run_mut(run) {
                r.activity.push(line);
                let extra = r.activity.len().saturating_sub(KEEP);
                r.activity.drain(..extra);
            }
            Ok(vec![])
        });
        if let Ok(a) = applied {
            self.feature_changed(&a);
        }
    }

    /// Hand the developer's message to a run in progress: a turn of its
    /// own, delivered after the current one (never merged with another).
    /// `command_id` is the message's own, so a repeat is queued once.
    pub(crate) async fn tell_runs(&self, feature_id: &FeatureId, command_id: &str, text: &str) {
        let live: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.feature == feature_id)
            .map(|r| (r.conversation.clone(), r.tx.clone()))
            .collect();
        let input = format!("From the developer:\n{text}");
        for (conversation, tx) in live {
            let queued = self.conversations.apply(
                &conversation,
                Op::Accept {
                    command_id: command_id.into(),
                    fingerprint: input.clone(),
                    command: Command::SendTurn {
                        initiator: Initiator::User,
                        input: vec![InputBlock::Text {
                            text: input.clone(),
                        }],
                    },
                    turn_id: Some(otter_core::TurnId::generate()),
                    at: Utc::now(),
                },
            );
            match queued {
                Some(Ok(_)) => {
                    let _ = tx.send(ActorCmd::Wake).await;
                }
                Some(Err(e)) => tracing::debug!(%conversation, "queueing the message: {e}"),
                None => {}
            }
        }
    }

    /// Pass a decision made in the feature on to the run waiting for it.
    pub(crate) async fn forward_decision(
        &self,
        feature_id: &FeatureId,
        decision_id: &DecisionId,
        allow: bool,
        answer: Option<String>,
        answers: Option<std::collections::BTreeMap<String, String>>,
        by: Decider,
    ) {
        let reply = match (answers, answer, allow) {
            (Some(answers), _, _) if !answers.is_empty() => DecisionReply::Answers { answers },
            (_, answer, allow) => match (answer, allow) {
                (Some(text), _) => DecisionReply::Answer { text },
                (None, true) => DecisionReply::Allow,
                (None, false) => DecisionReply::Deny {
                    message: "The developer declined this.".into(),
                },
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
                .send(ActorCmd::Decide {
                    interaction: decision_id.clone(),
                    reply: reply.clone(),
                    by,
                })
                .await;
        }
    }

    /// Stop every run of a feature and wait until each has ended. What it
    /// was doing ends interrupted (or, for a run out of time, at its limit).
    pub(crate) async fn stop_runs(&self, feature_id: &FeatureId, state: RunState, reason: &str) {
        let senders: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.feature == feature_id)
            .map(|r| (r.tx.clone(), r.stopping.clone()))
            .collect();
        let outcome = if state == RunState::Failed {
            TurnOutcome::LimitReached
        } else {
            TurnOutcome::Interrupted
        };
        for (tx, stopping) in senders {
            *stopping.lock().unwrap() = Some((state, reason.to_owned()));
            let (done, wait) = oneshot::channel();
            if tx.send(ActorCmd::Stop { outcome, done }).await.is_ok() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(15), wait).await;
            }
        }
    }

    /// After a restart: runs the old daemon left behind ended with it.
    pub(crate) async fn recover_runs(&self) {
        // A run's processes end with it; make sure none outlived the old
        // daemon (D-058).
        for c in self.conversations.list(None) {
            if let Some(p) = &c.process
                && c.active_run.is_none()
                && p.generation == c.generation
            {
                crate::runtime::end_leftover(p).await;
            }
        }
        let ids: Vec<FeatureId> = self
            .features
            .lock()
            .await
            .list()
            .into_iter()
            .filter(|f| f.live_run().is_some())
            .map(|f| f.id)
            .collect();
        let conversations = self.conversations.list(None);
        for id in ids {
            let applied = self.features.lock().await.apply(&id, None, |f, now| {
                let mut changes = Vec::new();
                let mut tasks = Vec::new();
                let mut done = Vec::new();
                for r in f.runs.iter_mut().filter(|r| !r.state.is_over()) {
                    // The journal may show the run had finished its work
                    // before the old daemon could tell the feature.
                    let finished = r
                        .conversation_id
                        .as_ref()
                        .and_then(|c| conversations.iter().find(|x| &x.id == c))
                        .and_then(|c| finished_run(c, &r.id));
                    match finished {
                        Some(summary) => {
                            r.state = RunState::Completed;
                            r.summary = summary;
                            done.push(r.task_id.clone());
                        }
                        None => {
                            r.state = RunState::Cancelled;
                            r.summary = Some("Interrupted: otterd restarted; resumable".into());
                            tasks.push(r.task_id.clone());
                        }
                    }
                    r.ended_at = Some(now);
                    changes.push(Change::new(FeatureEvent::RunChanged {
                        run_id: r.id.clone(),
                        state: r.state,
                    }));
                }
                for t in done {
                    if let Some(t) = f.task_mut(&t) {
                        t.status = TaskStatus::Done;
                        changes.push(Change::new(FeatureEvent::TaskChanged {
                            task_id: t.id.clone(),
                            status: TaskStatus::Done,
                        }));
                    }
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

/// Whether `run` finished its work, as `c`'s journal shows: its last turn
/// completed (and none was lost). The summary is what the agent said last.
fn finished_run(c: &otter_core::conversation::Conversation, run: &RunId) -> Option<Option<String>> {
    let turns: Vec<_> = c
        .turns
        .iter()
        .filter(|t| t.run_id.as_ref() == Some(run))
        .collect();
    let last = turns.last()?;
    if last.outcome != Some(TurnOutcome::Completed) {
        return None;
    }
    let said = c
        .messages
        .iter()
        .rev()
        .find(|m| m.turn_id == last.id)
        .map(|m| m.text());
    Some(said)
}

/// Appended to the coding agent's system prompt for managed runs.
const INSTRUCTIONS: &str = "You are working on one task of a feature, managed by Otter. \
Stay inside the working directory. Run the project's tests before you finish. \
End with a short summary of what you changed and how you checked it.";

/// One line for a tool the agent used.
fn activity_line(tool: &str, call: &crate::runtime::ToolCall) -> String {
    use crate::runtime::ToolCall;
    let line = match call {
        ToolCall::Command { line } => format!("$ {line}"),
        ToolCall::Edit { path } => format!("✎ {path}"),
        ToolCall::Fetch { url } => format!("↗ {url}"),
        ToolCall::Question { question, .. } => format!("? {question}"),
        ToolCall::PlanApproval { .. } => "plan proposed".into(),
        ToolCall::Delegate { description } => format!("subagent: {description}"),
        ToolCall::Read | ToolCall::Other { .. } => tool.to_owned(),
    };
    crate::agents::excerpt(&line, 200)
}

/// A message being written, sent at most every [`Draft::EVERY`].
pub(crate) struct Draft {
    pub id: String,
    text: String,
    sent: Option<std::time::Instant>,
}

impl Draft {
    const EVERY: std::time::Duration = std::time::Duration::from_millis(100);

    pub fn new(id: String) -> Draft {
        Draft {
            id,
            text: String::new(),
            sent: None,
        }
    }

    /// Add a piece; the text so far when it's time to send it.
    pub fn push(&mut self, piece: &str) -> Option<String> {
        self.text.push_str(piece);
        let due = self.sent.is_none_or(|t| t.elapsed() >= Self::EVERY);
        due.then(|| {
            self.sent = Some(std::time::Instant::now());
            self.text.clone()
        })
    }

    /// Something was sent.
    pub fn started(&self) -> bool {
        self.sent.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_finished_by_the_journal_is_one_whose_last_turn_completed() {
        use otter_core::conversation::{Command, Op};
        use otter_core::{BlockId, MessageId, TurnId};
        let mut c = Conversation::new("claude", "sdk", "ws_1".into(), Utc::now());
        let run = RunId::from("run_1");
        let (a, b) = (TurnId::generate(), TurnId::generate());
        let at = Utc::now();
        let send = |id: &str, turn: &TurnId| Op::Accept {
            command_id: id.into(),
            fingerprint: id.into(),
            command: Command::SendTurn {
                initiator: Initiator::Controller,
                input: vec![InputBlock::Text { text: id.into() }],
            },
            turn_id: Some(turn.clone()),
            at,
        };
        for op in [
            send("one", &a),
            Op::Claim {
                run: run.clone(),
                at,
            },
            Op::Sending {
                turn: a.clone(),
                generation: 1,
                at,
            },
        ] {
            c.apply(&op).unwrap();
        }
        assert_eq!(finished_run(&c, &run), None, "still working");
        for op in [
            Op::WriteBlock {
                turn: a.clone(),
                message: MessageId::generate(),
                block: BlockId::generate(),
                text: "Done: added the export.".into(),
                done: true,
                at,
            },
            Op::Finished {
                turn: a.clone(),
                generation: 1,
                outcome: TurnOutcome::Completed,
                reason: None,
                error: None,
                usage: None,
                at,
            },
        ] {
            c.apply(&op).unwrap();
        }
        assert_eq!(
            finished_run(&c, &run),
            Some(Some("Done: added the export.".into()))
        );
        // A later turn of the run that was lost: not finished.
        for op in [
            send("two", &b),
            Op::Sending {
                turn: b.clone(),
                generation: 1,
                at,
            },
            Op::RunEnded {
                generation: 1,
                outcome: TurnOutcome::OutcomeUnknown,
                at,
            },
        ] {
            c.apply(&op).unwrap();
        }
        assert_eq!(finished_run(&c, &run), None);
        assert_eq!(finished_run(&c, &RunId::from("run_other")), None);
    }

    #[test]
    fn a_draft_sends_the_text_so_far_at_most_every_100ms() {
        let mut d = Draft::new("r-0".into());
        assert!(!d.started());
        assert_eq!(
            d.push("Hel").as_deref(),
            Some("Hel"),
            "the first piece goes out"
        );
        assert_eq!(d.push("lo"), None, "then not before 100 ms");
        std::thread::sleep(Draft::EVERY);
        assert_eq!(
            d.push("!").as_deref(),
            Some("Hello!"),
            "the whole text so far"
        );
        assert!(d.started());
    }
}
