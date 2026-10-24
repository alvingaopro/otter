//! Managed runs (D-044, D-055): one coding-agent run working on one task
//! step, bridged to the task — progress becomes history, permission
//! requests become [`DecisionRequest`]s decided by policy, the Control Agent
//! or the developer.
//!
//! A run serves a runtime conversation (`runtime::conversations`): the
//! [`Actor`] owns the agent and the conversation's state; the bridge here
//! listens to it and is the only thing that touches the task. The
//! task's conversation continues across runs and steps (a resumed run is a
//! new run, a new generation, of the same conversation, D-064); a new one
//! starts only when there is none yet, or its session is gone.
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
    Applied, Command, Conversation, Initiator, NativeBinding, Op, TurnOutcome,
};
use otter_core::task::{
    Decider, DecisionKind, DecisionRequest, DecisionStatus, MessageRole, Risk, Run, RunState,
    StepStatus, Task, TaskEvent, TaskStatus,
};
use otter_core::{ConversationId, DecisionId, RunId, StepId, TaskId, Timestamp};
use otter_protocol::RpcError;
use tokio::sync::{mpsc, oneshot};

use crate::daemon::{Daemon, RpcResult, workspace_not_found};
use crate::runtime::actor::{Actor, ActorCmd, ActorEvent, SharedPulse, input_blocks};
use crate::runtime::policy::{self, Proposal, Verdict};
use crate::runtime::{CheckDecision, DecisionAsk, DecisionReply, RunLimits, RunSpec, runtime};
use crate::tasks::{Change, message};

/// The runtime tasks use (the only one with a managed mode so far).
pub const DEFAULT_RUNTIME: &str = "claude";

/// A run in progress, as the rest of the daemon reaches it.
pub(crate) struct LiveRun {
    pub task: TaskId,
    /// The workspace it works in: one driver at a time (D-079).
    pub workspace: otter_core::WorkspaceId,
    /// It only reads (an answer-only task's, on the SDK runtime, D-192):
    /// it neither holds the workspace nor waits for it, and stays reading
    /// until it ends.
    pub shared: bool,
    pub conversation: ConversationId,
    /// The run's actor.
    tx: mpsc::Sender<ActorCmd>,
    /// How the run is recorded when it is stopped, and why.
    stopping: Arc<std::sync::Mutex<Option<(RunState, String)>>>,
    /// When the agent was last heard from (the no-progress watchdog).
    pulse: SharedPulse,
    /// What it did lately and when, newest last: kept here while it runs,
    /// not rewritten into the task for every tool (D-069, D-141).
    activity: Arc<std::sync::Mutex<Vec<(Timestamp, String)>>>,
}

/// Lines of activity a run keeps.
const ACTIVITY_LINES: usize = 30;

/// Live runs by id.
pub(crate) type Runs = std::sync::Mutex<HashMap<RunId, LiveRun>>;

/// How the coding agent is told what Otter itself says (D-193).
pub(crate) const OTTER_SAYS: &str = "From Otter, your tech lead (not the developer)";

/// What a run that can't start because its workspace is taken says first:
/// the task waits, it doesn't fail (D-079).
pub(crate) const BUSY: &str = "busy: ";

/// A workspace claimed by a run that is starting, until it's live (or not).
struct Claim<'a> {
    daemon: &'a Daemon,
    workspace: otter_core::WorkspaceId,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.daemon.starting.lock().unwrap().remove(&self.workspace);
    }
}

impl Daemon {
    /// Start a run of `step`: fresh (a new conversation), or continuing
    /// `resume` (the agent's own conversation id) in the task's
    /// conversation. `counts` charges an attempt to the budget.
    pub(crate) async fn start_run(
        self: &Arc<Self>,
        task_id: &TaskId,
        step_id: &StepId,
        prompt: String,
        pictures: Vec<(String, PathBuf)>,
        resume: Option<String>,
        counts: bool,
    ) -> RpcResult<RunId> {
        let task = self.task_get(task_id.as_str()).await?;
        if !task.status.is_active() {
            return Err(RpcError::conflict(format!(
                "the task is {}",
                task.status.as_str()
            )));
        }
        if task.live_run().is_some() {
            return Err(RpcError::conflict("a run is already working on this task"));
        }
        let ws_id = task
            .workspace_id
            .clone()
            .ok_or_else(|| RpcError::conflict("the task has no workspace yet"))?;
        let ws = self
            .store
            .lock()
            .await
            .workspace(ws_id.as_str())
            .cloned()
            .ok_or_else(|| workspace_not_found(ws_id.as_str()))?;
        // One driver per workspace (D-079): claimed now, held while the run
        // lives. A run that only reads takes no claim and waits for none
        // (D-192): it changes nothing there.
        // The backend that will run it: the conversation's own when it
        // continues one (D-057), else the host's — only the SDK's hook keeps
        // an agent to reading.
        let continues_on = resume.as_ref().and_then(|sid| {
            task.runs
                .iter()
                .rev()
                .find(|r| r.provider_session_id.as_ref() == Some(sid))
                .and_then(|r| r.conversation_id.clone())
                .and_then(|id| self.conversations.get(id.as_str()))
                .map(|c| c.backend)
        });
        let shared = task.answer_only
            && continues_on.unwrap_or_else(|| self.settings.read().unwrap().coding_backend())
                == "sdk";
        let _claim = if shared {
            None
        } else {
            let runs = self.runs.lock().unwrap();
            let mut starting = self.starting.lock().unwrap();
            let holder = runs
                .values()
                .find(|r| r.workspace == ws.id && &r.task != task_id && !r.shared)
                .map(|r| r.task.clone())
                .or_else(|| starting.get(&ws.id).filter(|f| *f != task_id).cloned());
            if let Some(other) = holder {
                return Err(RpcError::conflict(format!("{BUSY}{other}")));
            }
            starting.insert(ws.id.clone(), task_id.clone());
            Some(Claim {
                daemon: self.as_ref(),
                workspace: ws.id.clone(),
            })
        };
        // Never two drivers for one conversation: not while a developer has
        // it. The task's steps share one (D-064).
        if task
            .steps
            .iter()
            .filter_map(|t| t.session_id.as_ref())
            .any(|sid| {
                ws.session(sid.as_str())
                    .is_some_and(|s| s.current_execution().is_some_and(|e| e.is_running()))
            })
        {
            return Err(RpcError::conflict(
                "the task's interactive session is still running; quit it first",
            ));
        }
        let mut env = self.env_for_launch(&ws).await?;
        env.insert("OTTER_WORKSPACE_ID".into(), ws.id.to_string());
        env.insert("OTTER_TASK_ID".into(), task_id.to_string());
        self.agent_secrets(DEFAULT_RUNTIME, &mut env);
        let (backend, coding_model) = {
            let s = self.settings.read().unwrap();
            (s.coding_backend(), s.coding_model())
        };

        // The conversation: the task's own when continuing it (D-064),
        // else a new one. A run from before conversations brings its session
        // id (a trusted link: it is the run's own record), never a guess.
        let now = Utc::now();
        let continued = resume.as_ref().and_then(|sid| {
            task.runs
                .iter()
                .rev()
                .find(|r| r.provider_session_id.as_ref() == Some(sid))
                .and_then(|r| r.conversation_id.clone())
                .and_then(|id| self.conversations.get(id.as_str()))
        });
        // A conversation keeps the backend it began with (D-057).
        let (conversation_id, backend) = match continued {
            Some(c) => (c.id, c.backend),
            None => {
                let mut c = Conversation::new(DEFAULT_RUNTIME, &backend, ws.id.clone(), now);
                c.task_id = Some(task_id.clone());
                c.step_id = Some(step_id.clone());
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
        // The messages' pictures, if the agent can see them — as many as a
        // turn carries, the rest said (D-181); else each message they came
        // with is told so, once.
        // The developer hears of the ones left out once the run has started.
        let mut prompt = prompt;
        let mut left_out_of = None;
        let pictures: Vec<PathBuf> = if sees_pictures(&backend) {
            let (fit, left) = fitting(pictures);
            if left > 0 {
                prompt.push_str(&format!("\n\n{}", left_out(left)));
                left_out_of = Some((fit.len(), left));
            }
            fit.into_iter().map(|(_, path)| path).collect()
        } else {
            let mut told = std::collections::HashSet::new();
            for (message, _) in &pictures {
                if told.insert(message.clone()) {
                    self.pictures_unseen(task_id, message).await;
                }
            }
            vec![]
        };
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
        if current.lifecycle == otter_core::conversation::Lifecycle::Paused {
            self.conversations
                .apply(
                    &conversation_id,
                    Op::Accept {
                        command_id: format!("{run_id}:resume"),
                        fingerprint: "resume".into(),
                        command: Command::Resume,
                        turn_id: None,
                        at: now,
                    },
                )
                .ok_or_else(missing)?
                .map_err(|e| RpcError::conflict(e.to_string()))?;
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
                        input: input_blocks(prompt.clone(), host_paths(&pictures)),
                        read_only: false,
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
                runtime: rt.version(&env),
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
        tracing::info!(runtime = rt.id(), task = %task_id, conversation = %conversation_id, generation, resume = native.is_some(), "starting a managed run");
        let root = PathBuf::from(&ws.root);
        // Where the task's own work starts (D-167): the workspace's revision
        // as its first run there begins, before the agent can move it. Not
        // for a task that ran before this was kept: its work may be
        // committed already.
        let first_here = match &task.started_from {
            Some(s) => s.workspace_id != ws.id,
            None => task.runs.is_empty(),
        };
        let start_point = if first_here {
            let git = crate::delivery::Git {
                root: &root,
                env: &env,
            };
            git.head()
                .await
                .ok()
                .map(|revision| otter_core::task::StartPoint {
                    workspace_id: ws.id.clone(),
                    revision,
                })
        } else {
            None
        };
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
        let tid = step_id.clone();
        let (cid, turn) = (conversation_id.clone(), turn_id.clone());
        let applied = self.tasks.lock().await.apply(task_id, None, move |f, now| {
            if let Some(s) = start_point {
                f.started_from = Some(s);
            }
            let step = f
                .step_mut(&tid)
                .ok_or_else(|| RpcError::not_found(format!("no step `{tid}`")))?;
            step.status = StepStatus::Running;
            if counts {
                step.attempts += 1;
            }
            let mut changes = vec![Change::new(TaskEvent::StepChanged {
                step_id: tid.clone(),
                status: StepStatus::Running,
            })];
            if counts {
                f.budget.iterations_used += 1;
            }
            f.runs.push(Run {
                id: rid.clone(),
                step_id: tid.clone(),
                runtime: DEFAULT_RUNTIME.into(),
                state: RunState::Running,
                started_at: now,
                ended_at: None,
                summary: None,
                provider_session_id: native,
                activity: vec![],
                activity_at: vec![],
                heard_at: None,
                tools_running: 0,
                conversation_id: Some(cid),
                turn_id: Some(turn),
                worked_ms: 0,
                working_since: Some(now),
                read_only: shared,
            });
            changes.push(
                Change::new(TaskEvent::RunStarted {
                    run_id: rid.clone(),
                    step_id: tid.clone(),
                    runtime: DEFAULT_RUNTIME.into(),
                })
                .because(rid.as_str()),
            );
            Ok(changes)
        });
        let applied = match applied {
            Ok(a) => a,
            Err(e) => {
                // The task changed under us: don't leave the agent running.
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
        self.task_changed(&applied);
        if let Some((given, left)) = left_out_of {
            self.tell_once(
                task_id,
                format!("pictures-left:{run_id}"),
                format!(
                    "The coding agent was given the newest {given} pictures you sent, not the {left} before them: one turn carries at most {}, {} MB in all. Send again any it still needs.",
                    otter_protocol::task::MAX_IMAGES,
                    otter_protocol::task::MAX_IMAGES_TOTAL >> 20
                ),
            )
            .await;
        }

        let (tx, mailbox) = mpsc::channel(32);
        let (out, events) = mpsc::channel(256);
        let stopping = Arc::new(std::sync::Mutex::new(None));
        let pulse = crate::runtime::actor::pulse();
        self.runs.lock().unwrap().insert(
            run_id.clone(),
            LiveRun {
                task: task_id.clone(),
                workspace: ws.id.clone(),
                shared,
                conversation: conversation_id.clone(),
                tx: tx.clone(),
                stopping: stopping.clone(),
                pulse: pulse.clone(),
                activity: Arc::default(),
            },
        );
        let actor = Actor::new(
            conversation_id.clone(),
            run_id.clone(),
            generation,
            self.conversations.clone(),
            handle,
            out,
        )
        .with_pulse(pulse);
        tokio::spawn(actor.run(mailbox));
        let daemon = self.clone();
        let (fid, rid) = (task_id.clone(), run_id.clone());
        tokio::spawn(async move {
            // A task of its own, so that a panic in it still lets the run
            // go (D-134).
            let bridged = {
                let (daemon, fid, rid) = (daemon.clone(), fid.clone(), rid.clone());
                tokio::spawn(
                    async move { daemon.bridge(fid, rid, root, events, tx, stopping).await },
                )
                .await
            };
            // Before its record goes, so what it did stays with the run.
            if bridged.is_err_and(|e| e.is_panic()) {
                daemon.lost_run(&fid, &rid, Some(&conversation_id)).await;
            }
            // Its mailbox closes with it: the actor stops the agent.
            daemon.runs.lock().unwrap().remove(&rid);
            daemon.task_wake.notify_one();
        });
        Ok(run_id)
    }

    /// How long a live run's agent has gone without a sign of work while
    /// none of its tools runs (a tool may take long without a word). `None`:
    /// a tool runs, or the run isn't live here.
    pub(crate) fn run_idle(&self, run: &RunId) -> Option<std::time::Duration> {
        let runs = self.runs.lock().unwrap();
        let p = runs.get(run)?.pulse.lock().unwrap();
        (p.tools == 0).then(|| p.at.elapsed())
    }

    /// The task whose run works in `workspace` now, or is starting there
    /// (D-079).
    pub(crate) fn workspace_holder(&self, workspace: &otter_core::WorkspaceId) -> Option<TaskId> {
        let runs = self.runs.lock().unwrap();
        runs.values()
            .find(|r| &r.workspace == workspace && !r.shared)
            .map(|r| r.task.clone())
            .or_else(|| self.starting.lock().unwrap().get(workspace).cloned())
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

    /// Follow one run's actor until the run ends, keeping the task in step.
    async fn bridge(
        self: &Arc<Self>,
        task: TaskId,
        run: RunId,
        root: PathBuf,
        mut events: mpsc::Receiver<ActorEvent>,
        actor: mpsc::Sender<ActorCmd>,
        stopping: Arc<std::sync::Mutex<Option<(RunState, String)>>>,
    ) {
        // A planning turn's words are a proposal for Otter, not for the
        // task's conversation (D-064); its journal keeps them.
        let quiet = self.task_get(task.as_str()).await.is_ok_and(|f| {
            f.runs
                .iter()
                .find(|r| r.id == run)
                .and_then(|r| f.step(&r.step_id))
                .is_some_and(|t| t.planning)
        });
        // What it is saying right now, streamed (D-051), by block.
        let mut draft: Option<Draft> = None;
        let mut last_text: Option<String> = None;
        // How the run ends once its last turn is over.
        let mut ending: Option<(RunState, Option<String>, bool)> = None;
        // Its conversation: a question's turn there only reads (D-089 M2.5),
        // and how the work's last turn ended stays the run's result.
        let (conversation, shared) = self
            .runs
            .lock()
            .unwrap()
            .get(&run)
            .map(|r| (Some(r.conversation.clone()), r.shared))
            .unwrap_or((None, false));
        let mut work_ending: Option<(RunState, Option<String>, bool)> = None;
        while let Some(ev) = events.recv().await {
            match ev {
                ActorEvent::Bound { session_id } => {
                    self.update_run(&task, &run, |r| r.provider_session_id = Some(session_id))
                        .await;
                }
                ActorEvent::TurnStarted { .. } => {}
                // Streamed: what the agent says shows up as it says it.
                ActorEvent::TextDelta { .. } | ActorEvent::Text { .. } if quiet => {}
                ActorEvent::TextDelta { block, piece, .. } => {
                    let d = match &mut draft {
                        Some(d) if d.id == block.as_str() => d,
                        _ => draft.insert(Draft::new(block.to_string())),
                    };
                    if let Some(so_far) = d.push(&piece) {
                        self.stream(&task, &d.id, MessageRole::Agent, so_far, false);
                    }
                }
                // What it says along the way streams (transient) and stays
                // in the conversation's journal; the task keeps one
                // message per turn, its result (D-069).
                ActorEvent::Text { block, text, .. } => {
                    if let Some(d) = draft.take_if(|d| d.id == block.as_str())
                        && d.started()
                    {
                        self.stream(&task, &d.id, MessageRole::Agent, text.clone(), true);
                    }
                    last_text = Some(text);
                }
                ActorEvent::ToolStarted { record, call } => {
                    self.run_activity(&task, &run, activity_line(&record.name, &call))
                        .await;
                }
                ActorEvent::Ask { interaction, ask } => {
                    let only_reads = shared
                        || self.reads_only(conversation.as_ref(), None)
                        || self.answer_only(&task).await;
                    let (verdict, scope) =
                        self.verdict_of(&task, &ask.call, &root, only_reads).await;
                    let grant = policy::grant_in(&ask.call, &scope);
                    // Routine work goes on, recorded as the tool it is (in
                    // the conversation), not as a decision (D-069).
                    let waiting = verdict != Verdict::Allow
                        && self
                            .record_ask(&task, &run, &interaction, &ask, &verdict, grant)
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
                    let only_reads = shared
                        || self.reads_only(conversation.as_ref(), None)
                        || self.answer_only(&task).await;
                    let (verdict, scope) = self.verdict_of(&task, &call, &root, only_reads).await;
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
                                &task,
                                &run,
                                &DecisionId::generate(),
                                &ask,
                                &verdict,
                                None,
                            )
                            .await;
                            CheckDecision::Deny {
                                reason: why.clone(),
                            }
                        }
                        // Allowed already in this task (D-066): goes on.
                        Verdict::Ask { .. }
                            if self.granted(&task, policy::grant_in(&call, &scope)).await =>
                        {
                            CheckDecision::Allow
                        }
                        Verdict::Ask { .. } => CheckDecision::Ask,
                    };
                    let _ = actor.send(ActorCmd::Check { check_id, decision }).await;
                }
                ActorEvent::TurnFinished {
                    turn,
                    outcome,
                    summary,
                    next,
                } => {
                    tracing::debug!(?outcome, "run finished its turn");
                    let summary = summary
                        .filter(|s| !s.trim().is_empty())
                        .or(last_text.take());
                    last_text = None;
                    let announce = summary.is_some();
                    // Interrupted or cancelled isn't failed: the step can pick
                    // up again (D-059).
                    let state = match outcome {
                        TurnOutcome::Completed => RunState::Completed,
                        TurnOutcome::Interrupted | TurnOutcome::Cancelled => RunState::Cancelled,
                        _ => RunState::Failed,
                    };
                    if self.reads_only(conversation.as_ref(), Some(&turn)) {
                        // A question's answer is the agent's message, never
                        // the run's result: that stays the work's (D-089 M2.5).
                        if let Some(s) = summary {
                            self.agent_said(&task, &run, s).await;
                        }
                        if next.is_none() {
                            ending = Some(work_ending.take().unwrap_or((
                                RunState::Completed,
                                None,
                                false,
                            )));
                        }
                    } else if next.is_some() {
                        // The developer wrote meanwhile: the conversation goes on.
                        if announce
                            && let Some(s) = summary.clone().filter(|s| !s.trim().is_empty())
                        {
                            self.agent_said(&task, &run, s).await;
                        }
                        // Said already: if a question's turn ends the run,
                        // this is still its result.
                        work_ending = Some((state, summary, false));
                    } else {
                        ending = Some((state, summary, announce));
                    }
                }
                ActorEvent::Ended {
                    outcome,
                    error,
                    stopped,
                } => {
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
                        self.finish_run(&task, &run, state, Some(reason), true)
                            .await;
                        let _ = done.send(());
                    } else if let Some((state, summary, announce)) = ending.take() {
                        self.finish_run(&task, &run, state, summary, announce).await;
                    } else {
                        let why =
                            error.unwrap_or_else(|| "the agent exited before finishing".into());
                        // Lost mid-turn: what it was doing is unknown, so it
                        // isn't a failure to start over from (D-061).
                        let state = if outcome == TurnOutcome::OutcomeUnknown {
                            self.unsettled_state(&task, &run, &why).await
                        } else {
                            (RunState::Failed, why)
                        };
                        self.finish_run(&task, &run, state.0, Some(state.1), true)
                            .await;
                    }
                    return;
                }
            }
        }
        // The actor went away without a word — with otterd, when it stops
        // (see above), or else it is a failure: otterd's, not the agent's.
        if self.shutting_down() {
            return;
        }
        self.lost_run(&task, &run, conversation.as_ref()).await;
    }

    /// A run otterd lost — its actor or its bridge went without ending it.
    /// Its conversation is free again, or nothing could run in it until a
    /// restart, and the run ends as one cut off mid-turn: resumed when
    /// that's safe, else the developer's to decide (D-061, D-134).
    async fn lost_run(&self, task: &TaskId, run: &RunId, conversation: Option<&ConversationId>) {
        tracing::warn!(task = %task, run = %run, "otterd lost the run");
        let c = conversation.and_then(|c| self.conversations.get(c.as_str()));
        // Its work may have finished before it was lost, as a restart
        // would find in the journal.
        let finished = c.as_ref().and_then(|c| finished_run(c, run));
        // Free either way: queued turns stay queued, as when otterd
        // restarts.
        if let Some(c) = c.filter(|c| c.active_run.as_ref() == Some(run)) {
            self.conversations.apply(
                &c.id,
                Op::RunEnded {
                    generation: c.generation,
                    outcome: TurnOutcome::OutcomeUnknown,
                    at: Utc::now(),
                },
            );
        }
        if let Some(summary) = finished {
            self.finish_run(task, run, RunState::Completed, summary, true)
                .await;
            return;
        }
        let (state, why) = self
            .unsettled_state(task, run, "otterd lost it: an internal error, see its log")
            .await;
        self.finish_run(task, run, state, Some(why), true).await;
    }

    async fn update_run(&self, task: &TaskId, run: &RunId, f: impl FnOnce(&mut Run)) {
        let mut f = Some(f);
        let applied = self.tasks.lock().await.apply(task, None, |feat, _| {
            if let Some(r) = feat.run_mut(run) {
                let before = r.state;
                (f.take().unwrap())(r);
                if r.state != before && !before.can_become(r.state) {
                    r.state = before;
                } else if r.state != before {
                    return Ok(vec![Change::new(TaskEvent::RunChanged {
                        run_id: run.clone(),
                        state: r.state,
                    })]);
                }
            }
            Ok(vec![])
        });
        if let Ok(a) = applied {
            self.task_changed(&a);
        }
    }

    /// Record a permission request under the interaction's id. True when
    /// someone has to decide; false when policy decided (the request is
    /// recorded as decided, for the audit trail).
    /// What policy says about a call, and the [`policy::Scope`] it said it in:
    /// the task's own space (D-215) — its workspace's directory, where the
    /// worktrees it makes beside its root go, and the worktrees the
    /// developer allowed it to make elsewhere. Not, as before, any worktree
    /// of the same repository: one the task didn't make is another's.
    async fn verdict_of(
        &self,
        task: &TaskId,
        call: &crate::runtime::ToolCall,
        root: &std::path::Path,
        only_reads: bool,
    ) -> (Verdict, policy::Scope) {
        let grants = self
            .tasks
            .lock()
            .await
            .get(task.as_str())
            .map(|f| f.grants.clone())
            .unwrap_or_default();
        let scope =
            policy::Scope::owned(root, &self.paths.workspaces_dir, &self.paths.home, &grants);
        if only_reads && let Some(v) = policy::read_only(call) {
            return (v, scope);
        }
        (policy::classify_in(call, &scope), scope)
    }

    /// Whether the developer allowed `grant` earlier in this task.
    async fn granted(&self, task: &TaskId, grant: Option<String>) -> bool {
        let Some(g) = grant else {
            return false;
        };
        self.task_get(task.as_str())
            .await
            .is_ok_and(|f| f.grants.contains(&g))
    }

    /// Whether Otter's model already denied this same call in this task.
    async fn declined_before(&self, task: &TaskId, ask: &crate::runtime::DecisionAsk) -> bool {
        let same = |a: &str, b: &str| a.split_whitespace().eq(b.split_whitespace());
        let summary = otter_core::redact::redact(&ask.summary);
        let command = whole_command(&ask.call);
        self.task_get(task.as_str()).await.is_ok_and(|f| {
            f.decisions.iter().any(|d| {
                d.status == DecisionStatus::Denied
                    && d.decided_by == Some(Decider::Controller)
                    && same(&d.summary, &summary)
                    && same(
                        d.command.as_deref().unwrap_or_default(),
                        command.as_deref().unwrap_or_default(),
                    )
            })
        })
    }

    async fn record_ask(
        &self,
        task: &TaskId,
        run: &RunId,
        interaction: &DecisionId,
        ask: &crate::runtime::DecisionAsk,
        verdict: &Verdict,
        grant: Option<String>,
    ) -> bool {
        let now = Utc::now();
        let (kind, risk, mut user_only, why, status) = match verdict {
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
        // The developer allowed it earlier in this task (D-066).
        let (status, mut why) =
            if status == DecisionStatus::Pending && self.granted(task, grant.clone()).await {
                (
                    DecisionStatus::Approved,
                    "you allowed this earlier in this task".to_owned(),
                )
            } else {
                (status, why)
            };
        // Otter's model said no to this call once: asking it again would be
        // another sample, so the developer decides (D-174).
        if status == DecisionStatus::Pending
            && !user_only
            && risk == Risk::High
            && self.declined_before(task, ask).await
        {
            user_only = true;
            why = format!("Otter declined this once; it is yours to decide. {why}");
        }
        let grant = grant.filter(|_| status == DecisionStatus::Pending);
        let options = match &ask.call {
            crate::runtime::ToolCall::Question { options, .. } => options.clone(),
            _ => vec![],
        };
        let applied = self.tasks.lock().await.apply(task, None, |f, _| {
            let step_id = f
                .runs
                .iter()
                .find(|r| &r.id == run)
                .map(|r| r.step_id.clone());
            f.decisions.push(DecisionRequest {
                id: did.clone(),
                step_id,
                run_id: Some(run.clone()),
                kind,
                risk,
                summary: otter_core::redact::redact(&ask.summary),
                detail: Some(why.clone()),
                command: whole_command(&ask.call),
                options,
                status,
                decided_by: (status != DecisionStatus::Pending).then_some(Decider::Policy),
                answer: None,
                rationale: (status != DecisionStatus::Pending).then(|| why.clone()),
                created_at: now,
                decided_at: (status != DecisionStatus::Pending).then_some(now),
                user_only,
                grant: grant.clone(),
            });
            let mut changes = vec![
                Change::new(TaskEvent::DecisionRequested {
                    decision_id: did.clone(),
                    kind,
                    risk,
                })
                .because(run.as_str()),
            ];
            if status != DecisionStatus::Pending {
                changes.push(
                    Change::new(TaskEvent::DecisionResolved {
                        decision_id: did.clone(),
                        status,
                        by: Decider::Policy,
                    })
                    .because(run.as_str()),
                );
                return Ok(changes);
            }
            // Waiting isn't working: the run's clock stops (D-063).
            if let Some(r) = f.run_mut(run)
                && r.state.can_become(RunState::Waiting)
            {
                r.state = RunState::Waiting;
                r.stop_clock(now);
                changes.push(Change::new(TaskEvent::RunChanged {
                    run_id: run.clone(),
                    state: RunState::Waiting,
                }));
            }
            // Only the developer can answer this one: tell them.
            if user_only && f.status.can_become(TaskStatus::Blocked) {
                let reason = format!("Needs your approval: {}", ask.summary);
                if let Ok(e) = f.transition(TaskStatus::Blocked, Some(reason), now) {
                    changes.push(Change::new(e));
                }
            }
            Ok(changes)
        });
        match applied {
            Ok(a) => {
                self.task_changed(&a);
                status == DecisionStatus::Pending
            }
            Err(e) => {
                tracing::warn!("recording a decision: {e:?}");
                false
            }
        }
    }

    /// How a run lost mid-turn while otterd runs is recorded ([`unsettled`]).
    async fn unsettled_state(&self, task: &TaskId, run: &RunId, why: &str) -> (RunState, String) {
        let Ok(f) = self.task_get(task.as_str()).await else {
            return (RunState::Failed, why.to_owned());
        };
        let Some(r) = f.runs.iter().find(|r| &r.id == run) else {
            return (RunState::Failed, why.to_owned());
        };
        let id = r.conversation_id.as_ref().map(|c| c.as_str());
        let c = id.and_then(|c| self.conversations.get(c));
        let broken = id.and_then(|c| self.conversations.read_only(c));
        unsettled(
            r,
            c.as_ref(),
            broken.as_deref(),
            losses_before(&f, r),
            &format!("the agent's process was lost ({why})"),
        )
    }

    async fn finish_run(
        &self,
        task: &TaskId,
        run: &RunId,
        state: RunState,
        summary: Option<String>,
        announce: bool,
    ) {
        let activity = self.live_activity(run);
        // Its last words and errors are kept: never a credential in them (D-075).
        let summary = summary.map(|s| otter_core::redact::redact(&s));
        let applied = self.tasks.lock().await.apply(task, None, |f, now| {
            let mut changes = Vec::new();
            let Some(r) = f.run_mut(run) else {
                return Ok(changes);
            };
            if r.state.is_over() {
                return Ok(changes);
            }
            r.state = state;
            r.ended_at = Some(now);
            r.stop_clock(now);
            r.summary = summary.clone();
            if let Some((a, at)) = activity {
                r.activity = a;
                r.activity_at = at;
            }
            let step_id = r.step_id.clone();
            changes.push(Change::new(TaskEvent::RunChanged {
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
            // A planning turn's report is a plan for Otter to review, not
            // something to show as is (D-064).
            let planning = f.step(&step_id).is_some_and(|t| t.planning);
            // Said already (the turn before ended the same way): once (D-069).
            let said = |text: &str| {
                f.messages
                    .iter()
                    .rev()
                    .find(|m| m.role == MessageRole::Agent)
                    .is_some_and(|m| same_words(&m.text, text))
            };
            if let Some(text) = summary
                .clone()
                .filter(|s| announce && !planning && !s.trim().is_empty() && !said(s))
            {
                let (m, c) = message(MessageRole::Agent, text, Some(run.to_string()));
                f.messages.push(m);
                changes.push(c);
            }
            if state == RunState::RecoveryRequired {
                changes.extend(require_recovery(
                    f,
                    &step_id,
                    summary.as_deref().unwrap_or_default(),
                    now,
                ));
            } else if let Some(t) = f.step_mut(&step_id) {
                t.status = match state {
                    RunState::Completed => StepStatus::Done,
                    RunState::Failed => StepStatus::Failed,
                    // Stopped, not finished: it runs again later.
                    _ => StepStatus::Pending,
                };
                // One line: it is quoted in reasons and prompts.
                if state == RunState::Failed {
                    t.last_error = summary.as_deref().map(|s| crate::agents::excerpt(s, 2000));
                }
                changes.push(Change::new(TaskEvent::StepChanged {
                    step_id,
                    status: t.status,
                }));
            }
            Ok(changes)
        });
        if let Ok(a) = applied {
            self.task_changed(&a);
        }
    }

    /// Answer a pending decision as `by`. Policy has the last word: a model
    /// can't allow what is the developer's to allow, and nobody can turn a
    /// denial around.
    pub(crate) async fn decide(
        &self,
        task_id: &TaskId,
        decision_id: &DecisionId,
        allow: bool,
        answer: Option<String>,
        by: Decider,
        rationale: Option<String>,
    ) -> RpcResult<Task> {
        let task = self.task_get(task_id.as_str()).await?;
        let d = task
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
        // Final when the developer or policy denied it. Otter's model's
        // denial isn't (D-174), though a decision once made can't be made
        // again below: it is a repeat of the call that goes to the developer.
        let previously_denied =
            d.status == DecisionStatus::Denied && d.decided_by != Some(Decider::Controller);
        policy::resolve(&verdict, previously_denied, &Proposal { by, allow })
            .map_err(RpcError::conflict)?;
        if d.status != DecisionStatus::Pending {
            return Err(RpcError::conflict("that decision has already been made"));
        }
        let did = decision_id.clone();
        let ans = answer.clone();
        let why = rationale.clone();
        let applied = self
            .tasks
            .lock()
            .await
            .apply(task_id, None, move |f, now| {
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
                let mut changes = vec![Change::new(TaskEvent::DecisionResolved {
                    decision_id: did.clone(),
                    status,
                    by,
                })];
                changes.extend(f.resume_waiting_runs(now).into_iter().map(Change::new));
                if f.status == TaskStatus::Blocked && f.pending_decisions().next().is_none() {
                    let next = crate::tasks::resume_status(f);
                    if let Ok(e) = f.transition(next, None, now) {
                        changes.push(Change::new(e));
                    }
                }
                Ok(changes)
            })?;
        self.task_changed(&applied);
        let reply = decision_reply(allow, answer, None, by, why);
        self.forward_decision(task_id, decision_id, reply, by).await;
        Ok(applied.task)
    }

    /// What the coding agent said at the end of a turn that isn't the last.
    async fn agent_said(&self, task: &TaskId, run: &RunId, text: String) {
        let text = otter_core::redact::redact(&text);
        let applied = self.tasks.lock().await.apply(task, None, |f, _| {
            let (m, c) = message(MessageRole::Agent, text, Some(run.to_string()));
            f.messages.push(m);
            Ok(vec![c])
        });
        if let Ok(a) = applied {
            self.task_changed(&a);
        }
    }

    /// Otter has begun a message in `task`'s conversation (D-196): said to
    /// the app at once, before its first word, so the app shows Otter at it.
    /// Ended without [`Writing::done`] (nothing came of it, an error, a
    /// panic), it says so, and the app takes the sign away.
    pub(crate) fn writing(&self, task: &TaskId, stream_id: &str) -> Writing<'_> {
        self.stream(
            task,
            stream_id,
            MessageRole::Controller,
            String::new(),
            false,
        );
        Writing {
            daemon: self,
            task: task.clone(),
            stream_id: stream_id.to_owned(),
            done: false,
        }
    }

    /// Text being written right now, to the app (transient, D-051).
    pub(crate) fn stream(
        &self,
        task: &TaskId,
        stream_id: &str,
        role: MessageRole,
        text: String,
        done: bool,
    ) {
        self.events
            .emit_transient(otter_protocol::Event::TaskStream {
                task_id: task.clone(),
                stream_id: stream_id.to_owned(),
                role,
                text: otter_core::redact::redact(&text),
                done,
            });
    }

    /// Note what the agent just did on its run (kept short: the last
    /// lines): in memory and to the app, as it happens; into the task
    /// when the run ends (D-069). Each line with its time (D-141).
    async fn run_activity(&self, task: &TaskId, run: &RunId, line: String) {
        let line = otter_core::redact::redact(&line);
        let at = Utc::now();
        if let Some(live) = self.runs.lock().unwrap().get(run) {
            let mut a = live.activity.lock().unwrap();
            a.push((at, line.clone()));
            let extra = a.len().saturating_sub(ACTIVITY_LINES);
            a.drain(..extra);
        }
        self.events
            .emit_transient(otter_protocol::Event::TaskActivity {
                task_id: task.clone(),
                run_id: run.clone(),
                line,
                at: Some(at),
            });
    }

    /// What a live run did lately, and when (its activity isn't written
    /// into the task until it ends).
    pub(crate) fn live_activity(&self, run: &RunId) -> Option<(Vec<String>, Vec<Timestamp>)> {
        let runs = self.runs.lock().unwrap();
        let a = runs.get(run)?.activity.lock().unwrap();
        Some(a.iter().map(|(at, l)| (l.clone(), *at)).unzip())
    }

    /// A task as clients see it (D-141): its live runs with their latest
    /// activity, when their agent was last heard from and how many of its
    /// tools run; and what comes next.
    pub(crate) fn for_clients(&self, mut f: Task) -> Task {
        for r in f.runs.iter_mut().filter(|r| !r.state.is_over()) {
            if let Some((a, at)) = self.live_activity(&r.id) {
                r.activity = a;
                r.activity_at = at;
            }
            let runs = self.runs.lock().unwrap();
            if let Some(live) = runs.get(&r.id) {
                let p = live.pulse.lock().unwrap();
                r.heard_at = Some(p.heard);
                r.tools_running = p.tools as u32;
            }
        }
        f.next = f.what_next();
        f
    }

    /// Hand the developer's message to a run in progress: a turn of its
    /// own, delivered after the current one (never merged with another).
    /// `command_id` is the message's own, so a repeat is queued once.
    pub(crate) async fn tell_runs(
        &self,
        task_id: &TaskId,
        command_id: &str,
        text: &str,
        pictures: Vec<PathBuf>,
    ) {
        let input = format!("From the developer:\n{text}");
        self.queue_turn(task_id, command_id, input, pictures, false)
            .await;
    }

    /// Ask a run in progress the developer's question (D-089 M2.5): a turn
    /// of its own after the current one, in which the agent may only read
    /// ([`policy::read_only`]); its answer is its message, not the run's
    /// result. Only runs on the SDK runtime, where Otter's policy sees every
    /// tool call; how many it went to.
    pub(crate) async fn ask_runs(
        &self,
        task_id: &TaskId,
        command_id: &str,
        text: &str,
        pictures: Vec<PathBuf>,
    ) -> usize {
        let input = format!(
            "From the developer, a question. This turn can only read: Read, Grep and Glob work; writing files and running commands are refused. Answer what you can, pointing to the code (path:line); if part of it asks for a change, say that needs a turn of its own:\n{text}"
        );
        self.queue_turn(task_id, command_id, input, pictures, true)
            .await
    }

    /// The runtime backend of the task's run in progress, if one is.
    pub(crate) fn live_backend(&self, task_id: &TaskId) -> Option<String> {
        let conversation = self
            .runs
            .lock()
            .unwrap()
            .values()
            .find(|r| &r.task == task_id)
            .map(|r| r.conversation.clone())?;
        self.conversations
            .get(conversation.as_str())
            .map(|c| c.backend)
    }

    /// Queue a turn in each run of the task (read-only: SDK runs only);
    /// how many it was queued in.
    async fn queue_turn(
        &self,
        task_id: &TaskId,
        command_id: &str,
        input: String,
        pictures: Vec<PathBuf>,
        read_only: bool,
    ) -> usize {
        self.queue_turn_from(
            task_id,
            command_id,
            input,
            pictures,
            read_only,
            Initiator::User,
        )
        .await
    }

    /// [`Self::queue_turn`], said by `initiator` (Otter's own instructions
    /// are the controller's, D-193).
    async fn queue_turn_from(
        &self,
        task_id: &TaskId,
        command_id: &str,
        input: String,
        pictures: Vec<PathBuf>,
        read_only: bool,
        initiator: Initiator,
    ) -> usize {
        let live: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.task == task_id)
            .map(|r| (r.conversation.clone(), r.tx.clone()))
            .filter(|(c, _)| {
                !read_only
                    || self
                        .conversations
                        .get(c.as_str())
                        .is_some_and(|c| c.backend == "sdk")
            })
            .collect();
        let mut queued_in = 0;
        for (conversation, tx) in live {
            // Only a runtime that can see pictures is given them; for the
            // others the developer is told, once.
            let takes = self
                .conversations
                .get(conversation.as_str())
                .is_some_and(|c| sees_pictures(&c.backend));
            let pictures = if takes || pictures.is_empty() {
                host_paths(&pictures)
            } else {
                self.pictures_unseen(task_id, command_id).await;
                vec![]
            };
            let queued = self.conversations.apply(
                &conversation,
                Op::Accept {
                    command_id: command_id.into(),
                    fingerprint: format!("{input}{}", pictures.join("\n")),
                    command: Command::SendTurn {
                        initiator,
                        input: input_blocks(input.clone(), pictures),
                        read_only,
                    },
                    turn_id: Some(otter_core::TurnId::generate()),
                    at: Utc::now(),
                },
            );
            match queued {
                Some(Ok(_)) => {
                    queued_in += 1;
                    let _ = tx.send(ActorCmd::Wake).await;
                }
                Some(Err(e)) => tracing::debug!(%conversation, "queueing the message: {e}"),
                None => {}
            }
        }
        queued_in
    }

    /// Whether `task`'s run would only read, alongside whatever else works
    /// in its workspace (D-192): an answer-only task, where the SDK
    /// runtime's hook keeps the agent to reading.
    pub(crate) fn reads_alongside(&self, task: &Task) -> bool {
        task.answer_only && self.can_answer_only().is_ok()
    }

    /// Whether the task only wants an answer (D-092): all its turns read.
    async fn answer_only(&self, task: &TaskId) -> bool {
        self.tasks
            .lock()
            .await
            .get(task.as_str())
            .is_some_and(|f| f.answer_only)
    }

    /// Whether the run's turn in progress (or `turn`) is a question's.
    fn reads_only(
        &self,
        conversation: Option<&ConversationId>,
        turn: Option<&otter_core::TurnId>,
    ) -> bool {
        let Some(c) = conversation.and_then(|c| self.conversations.get(c.as_str())) else {
            return false;
        };
        match turn {
            Some(id) => c.turns.iter().any(|t| &t.id == id && t.read_only),
            None => c.active_turn().is_some_and(|t| t.read_only),
        }
    }

    /// Pass a decision made in the task on to the run waiting for it.
    pub(crate) async fn forward_decision(
        &self,
        task_id: &TaskId,
        decision_id: &DecisionId,
        reply: DecisionReply,
        by: Decider,
    ) {
        let senders: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.task == task_id)
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

    /// Pause (or cancel) every run of a task: its turn is interrupted,
    /// then the run ends, recorded as cancelled with `reason`; its
    /// conversation is held (or closed). Waits until each has ended.
    pub(crate) async fn hold_runs(&self, task_id: &TaskId, cancel: bool, reason: &str) {
        let live: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.task == task_id)
            .map(|r| (r.tx.clone(), r.stopping.clone()))
            .collect();
        for (tx, stopping) in live {
            *stopping.lock().unwrap() = Some((RunState::Cancelled, reason.to_owned()));
            let (done, wait) = oneshot::channel();
            let command_id = format!("hold-{}", RunId::generate());
            if tx
                .send(ActorCmd::Hold {
                    command_id,
                    cancel,
                    done,
                })
                .await
                .is_ok()
            {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(20), wait).await;
            }
        }
    }

    /// Stop what the coding agent is doing now (its current turn only).
    pub(crate) async fn interrupt_runs(&self, task_id: &TaskId) {
        let live: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.task == task_id)
            .map(|r| (r.tx.clone(), r.conversation.clone()))
            .collect();
        for (tx, conversation) in live {
            let turn = self
                .conversations
                .get(conversation.as_str())
                .and_then(|c| c.active_turn().map(|t| t.id.clone()));
            if let Some(turn) = turn {
                let _ = tx.send(ActorCmd::Interrupt { turn }).await;
            }
        }
    }

    /// Guidance that can't wait: the current turn stops and this goes next.
    pub(crate) async fn redirect_runs(
        &self,
        task_id: &TaskId,
        command_id: &str,
        text: &str,
        pictures: Vec<PathBuf>,
    ) {
        let live: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.task == task_id)
            .map(|r| r.tx.clone())
            .collect();
        let pictures = if pictures.is_empty()
            || self
                .live_backend(task_id)
                .is_some_and(|b| sees_pictures(&b))
        {
            host_paths(&pictures)
        } else {
            self.pictures_unseen(task_id, command_id).await;
            vec![]
        };
        for tx in live {
            let _ = tx
                .send(ActorCmd::Redirect {
                    command_id: command_id.to_owned(),
                    initiator: Initiator::User,
                    text: format!(
                        "From the developer — a change of direction, before anything else:\n{text}"
                    ),
                    pictures: pictures.clone(),
                })
                .await;
        }
    }

    /// Otter's own instructions for the run in progress (D-193), said as
    /// Otter's — never as the developer's (D-089): after its current turn,
    /// or (`now`) stopping it first. How many runs they reached.
    pub(crate) async fn otter_tells_runs(
        &self,
        task_id: &TaskId,
        command_id: &str,
        text: &str,
        now: bool,
    ) -> usize {
        if !now {
            return self
                .queue_turn_from(
                    task_id,
                    command_id,
                    format!("{OTTER_SAYS}:\n{text}"),
                    vec![],
                    false,
                    Initiator::Controller,
                )
                .await;
        }
        let live: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.task == task_id)
            .map(|r| r.tx.clone())
            .collect();
        let n = live.len();
        for tx in live {
            let _ = tx
                .send(ActorCmd::Redirect {
                    command_id: command_id.to_owned(),
                    initiator: Initiator::Controller,
                    text: format!(
                        "{OTTER_SAYS} — a change of direction, before anything else:\n{text}"
                    ),
                    pictures: vec![],
                })
                .await;
        }
        n
    }

    /// The developer's pictures went with a message, and the coding agent
    /// here can't see them: said once on the task, so it isn't a surprise.
    async fn pictures_unseen(&self, task_id: &TaskId, key: &str) {
        self.tell_once(
            task_id,
            format!("pictures:{key}"),
            "The coding agent on this host can't see pictures, so it wasn't given the ones you sent. Describe what they show, or use the SDK runtime in Settings.".into(),
        )
        .await;
    }

    /// A system message on the task, once for `correlation`.
    async fn tell_once(&self, task_id: &TaskId, correlation: String, text: String) {
        let applied = self
            .tasks
            .lock()
            .await
            .apply(task_id, Some(&correlation), |f, _| {
                let (m, c) = message(MessageRole::System, text, Some(correlation.clone()));
                f.messages.push(m);
                Ok(vec![c])
            });
        if let Ok(applied) = applied {
            self.task_changed(&applied);
        }
    }

    /// Stop every run of a task and wait until each has ended. What it
    /// was doing ends interrupted (or, for a run out of time, at its limit).
    pub(crate) async fn stop_runs(&self, task_id: &TaskId, state: RunState, reason: &str) {
        let senders: Vec<_> = self
            .runs
            .lock()
            .unwrap()
            .values()
            .filter(|r| &r.task == task_id)
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
        let ids: Vec<TaskId> = self
            .tasks
            .lock()
            .await
            .list()
            .into_iter()
            .filter(|f| f.live_run().is_some())
            .map(|f| f.id)
            .collect();
        let conversations = self.conversations.list(None);
        for id in ids {
            let applied = self.tasks.lock().await.apply(&id, None, |f, now| {
                let mut changes = Vec::new();
                let mut steps = Vec::new();
                let mut done = Vec::new();
                let mut stuck = Vec::new();
                let losses: Vec<(RunId, usize)> = f
                    .runs
                    .iter()
                    .filter(|r| !r.state.is_over())
                    .map(|r| (r.id.clone(), losses_before(f, r)))
                    .collect();
                for r in f.runs.iter_mut().filter(|r| !r.state.is_over()) {
                    // The journal may show the run had finished its work
                    // before the old daemon could tell the task.
                    let conversation = r
                        .conversation_id
                        .as_ref()
                        .and_then(|c| conversations.iter().find(|x| &x.id == c));
                    match conversation.and_then(|c| finished_run(c, &r.id)) {
                        Some(summary) => {
                            r.state = RunState::Completed;
                            r.summary = summary;
                            done.push(r.step_id.clone());
                        }
                        None => {
                            let broken = r
                                .conversation_id
                                .as_ref()
                                .and_then(|c| self.conversations.read_only(c.as_str()));
                            let lost = losses
                                .iter()
                                .find(|(id, _)| id == &r.id)
                                .map_or(0, |(_, n)| *n);
                            let (state, why) = unsettled(
                                r,
                                conversation,
                                broken.as_deref(),
                                lost,
                                "otterd restarted",
                            );
                            r.state = state;
                            r.summary = Some(why.clone());
                            if state == RunState::RecoveryRequired {
                                stuck.push((r.step_id.clone(), why));
                            } else {
                                steps.push(r.step_id.clone());
                            }
                        }
                    }
                    r.ended_at = Some(now);
                    changes.push(Change::new(TaskEvent::RunChanged {
                        run_id: r.id.clone(),
                        state: r.state,
                    }));
                }
                for t in done {
                    if let Some(t) = f.step_mut(&t) {
                        t.status = StepStatus::Done;
                        changes.push(Change::new(TaskEvent::StepChanged {
                            step_id: t.id.clone(),
                            status: StepStatus::Done,
                        }));
                    }
                }
                for d in f.decisions.iter_mut() {
                    if d.status == DecisionStatus::Pending && d.run_id.is_some() {
                        d.status = DecisionStatus::Cancelled;
                        d.decided_at = Some(now);
                    }
                }
                for t in steps {
                    if let Some(t) = f.step_mut(&t) {
                        t.status = StepStatus::Pending;
                    }
                }
                // After the old run's decisions are moot: this one isn't.
                for (t, why) in stuck {
                    changes.extend(require_recovery(f, &t, &why, now));
                }
                // A block that was about the interrupted run is moot.
                if f.status == TaskStatus::Blocked && f.pending_decisions().next().is_none() {
                    let next = crate::tasks::resume_status(f);
                    if let Ok(e) = f.transition(next, None, now) {
                        changes.push(Change::new(e));
                    }
                }
                Ok(changes)
            });
            match applied {
                Ok(a) => self.task_changed(&a),
                Err(e) => tracing::warn!(task = %id, "recovering runs: {e:?}"),
            }
        }
    }
}

/// Runs of a step cut off in a row before Otter stops resuming it on its own.
const MAX_LOSSES: usize = 3;

/// How a run that ended without settling its turn — otterd restarted, or
/// the agent's process was lost — is recorded (D-061). Interrupted when its
/// conversation can carry on (or nothing had reached the agent yet, so
/// starting over repeats nothing); recovery required when its record can't
/// be trusted, when it keeps getting cut off, or when the agent had the step
/// but there is no conversation to resume — starting over then could repeat
/// what it did, so the developer decides.
fn unsettled(
    run: &Run,
    c: Option<&Conversation>,
    read_only: Option<&str>,
    lost_before: usize,
    why: &str,
) -> (RunState, String) {
    let recover = |detail: String| (RunState::RecoveryRequired, detail);
    if let Some(broken) = read_only {
        return recover(format!(
            "{why}, and its conversation's record can't be trusted ({broken})"
        ));
    }
    if lost_before + 1 >= MAX_LOSSES {
        return recover(format!(
            "{why}: {} runs in a row were cut off",
            lost_before + 1
        ));
    }
    if run.provider_session_id.is_some() {
        return (
            RunState::Interrupted,
            format!("Interrupted: {why}; resumable"),
        );
    }
    let reached = c.is_none_or(|c| c.turns.iter().any(|t| t.run_id.as_ref() == Some(&run.id)));
    if reached {
        recover(format!(
            "{why} before the agent's conversation could be kept; starting over could repeat what it did"
        ))
    } else {
        (
            RunState::Interrupted,
            format!("Interrupted: {why} before the agent had the step; it starts again"),
        )
    }
}

/// How many runs of `r`'s step, just before it, were cut off in a row.
fn losses_before(f: &Task, r: &Run) -> usize {
    let earlier: Vec<&Run> = f
        .runs
        .iter()
        .take_while(|x| x.id != r.id)
        .filter(|x| x.step_id == r.step_id)
        .collect();
    earlier
        .iter()
        .rev()
        .take_while(|x| x.state == RunState::Interrupted)
        .count()
}

/// A run ended [`RunState::RecoveryRequired`]: its step waits for the
/// developer to say whether it starts over (a [`DecisionKind::Recovery`],
/// theirs only), and the task is blocked until then.
fn require_recovery(
    f: &mut Task,
    step: &StepId,
    why: &str,
    now: chrono::DateTime<Utc>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    let Some(t) = f.step_mut(step) else {
        return changes;
    };
    t.status = StepStatus::Blocked;
    t.last_error = Some(why.to_owned());
    let title = t.title.clone();
    changes.push(Change::new(TaskEvent::StepChanged {
        step_id: step.clone(),
        status: StepStatus::Blocked,
    }));
    let open = f.decisions.iter().any(|d| {
        d.kind == DecisionKind::Recovery
            && d.status == DecisionStatus::Pending
            && d.step_id.as_ref() == Some(step)
    });
    if !open {
        let id = DecisionId::generate();
        f.decisions.push(DecisionRequest {
            id: id.clone(),
            step_id: Some(step.clone()),
            run_id: None,
            kind: DecisionKind::Recovery,
            risk: Risk::High,
            summary: format!("Start “{title}” over?"),
            detail: Some(format!(
                "{why}. Look at the workspace first: what the cut-off run did may already be there."
            )),
            command: None,
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
        changes.push(Change::new(TaskEvent::DecisionRequested {
            decision_id: id,
            kind: DecisionKind::Recovery,
            risk: Risk::High,
        }));
    }
    if f.status.can_become(TaskStatus::Blocked)
        && let Ok(e) = f.transition(
            TaskStatus::Blocked,
            Some(format!("Needs you: a run can't safely continue — {why}")),
            now,
        )
    {
        changes.push(Change::new(e));
    }
    changes
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
const INSTRUCTIONS: &str = "You are working on a task, one step per turn, managed by Otter. \
Stay inside the working directory. Run the project's tests before you finish a step. \
End each turn with a short summary of what you changed and how you checked it.";

/// The same text, give or take whitespace.
fn same_words(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// The most of a command a decision keeps (D-157).
const COMMAND_MAX: usize = 16 * 1024;

/// The whole command line a decision is about, to read before deciding
/// (D-157): only when its one-line summary doesn't show it as it is — cut,
/// or on several lines. Redacted before it is bounded, so a cut can't leave
/// half a credential that no longer looks like one.
fn whole_command(call: &crate::runtime::ToolCall) -> Option<String> {
    let crate::runtime::ToolCall::Command { line } = call else {
        return None;
    };
    let line = otter_core::redact::redact(line);
    if crate::agents::claude_stream::summarize(call, "") == format!("Run `{line}`") {
        return None;
    }
    if line.len() <= COMMAND_MAX {
        return Some(line);
    }
    let cut = line.floor_char_boundary(COMMAND_MAX);
    Some(format!(
        "{}\n… {} more bytes, not kept",
        &line[..cut],
        line.len() - cut
    ))
}

/// What a decision tells the agent: allowed, the answer, or who said no
/// and why, when they said (D-066).
pub(crate) fn decision_reply(
    allow: bool,
    answer: Option<String>,
    answers: Option<std::collections::BTreeMap<String, String>>,
    by: Decider,
    why: Option<String>,
) -> DecisionReply {
    let because = why
        .filter(|w| !w.trim().is_empty())
        .map(|w| format!(": {}", w.trim()))
        .unwrap_or_else(|| ".".into());
    match (answers, answer, allow) {
        (Some(answers), _, _) if !answers.is_empty() => DecisionReply::Answers { answers },
        (_, Some(text), _) => DecisionReply::Answer { text },
        (_, None, true) => DecisionReply::Allow,
        (_, None, false) => DecisionReply::Deny {
            message: match by {
                Decider::User => format!("The developer declined this{because}"),
                Decider::Controller => format!("Otter declined this{because}"),
                Decider::Policy => format!("Otter's policy doesn't allow this{because}"),
            },
        },
    }
}

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
        ToolCall::Read | ToolCall::Outside { .. } | ToolCall::Other { .. } => tool.to_owned(),
    };
    // Redacted before the cut, as a decision's summary is (D-157).
    crate::agents::excerpt(&otter_core::redact::redact(&line), 200)
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

/// Whether a runtime backend gives the agent the pictures of a message:
/// the SDK worker does (`features.attachments`); the others don't.
fn sees_pictures(backend: &str) -> bool {
    backend == "sdk"
}

/// Host paths of files, as a turn's attachments name them.
pub(crate) fn host_paths(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// The pictures one turn carries (D-181): the newest that fit what a turn
/// takes — [`MAX_IMAGES`](otter_protocol::task::MAX_IMAGES), and
/// [`MAX_IMAGES_TOTAL`](otter_protocol::task::MAX_IMAGES_TOTAL) in all —
/// in the order they were sent, and how many older ones were left out. The
/// worker refuses a turn over those whole, and a turn may gather several
/// messages' pictures.
pub(crate) fn fitting<T>(pictures: Vec<(T, PathBuf)>) -> (Vec<(T, PathBuf)>, usize) {
    use otter_protocol::task::{MAX_IMAGES, MAX_IMAGES_TOTAL};
    let all = pictures.len();
    let mut total = 0u64;
    let mut kept: Vec<(T, PathBuf)> = pictures
        .into_iter()
        .rev()
        .take(MAX_IMAGES)
        .take_while(|(_, path)| {
            total += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            total <= MAX_IMAGES_TOTAL as u64
        })
        .collect();
    kept.reverse();
    let left = all - kept.len();
    (kept, left)
}

/// What a turn says of the pictures [`fitting`] left out.
pub(crate) fn left_out(n: usize) -> String {
    use otter_protocol::task::{MAX_IMAGES, MAX_IMAGES_TOTAL};
    format!(
        "({n} older {} the developer sent {} left out: a turn carries at most {MAX_IMAGES}, {} MB in all.)",
        if n == 1 { "picture" } else { "pictures" },
        if n == 1 { "is" } else { "are" },
        MAX_IMAGES_TOTAL >> 20
    )
}

/// Otter writing a message (D-196), from [`Daemon::writing`].
pub(crate) struct Writing<'a> {
    daemon: &'a Daemon,
    task: TaskId,
    stream_id: String,
    done: bool,
}

impl Writing<'_> {
    /// The message is in the conversation, and its stream said `done`.
    pub(crate) fn done(mut self) {
        self.done = true;
    }
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.daemon.stream(
                &self.task,
                &self.stream_id,
                MessageRole::Controller,
                String::new(),
                true,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::conversation::InputBlock;

    fn run_of(step: &StepId, state: RunState) -> Run {
        Run {
            read_only: false,
            id: RunId::generate(),
            step_id: step.clone(),
            runtime: DEFAULT_RUNTIME.into(),
            state,
            started_at: Utc::now(),
            ended_at: None,
            summary: None,
            provider_session_id: None,
            activity: vec![],
            activity_at: vec![],
            heard_at: None,
            tools_running: 0,
            conversation_id: None,
            turn_id: None,
            worked_ms: 0,
            working_since: None,
        }
    }

    fn implementing(step: &StepId) -> Task {
        use otter_core::task::Step;
        let now = Utc::now();
        let mut f = Task::new("CSV".into(), "Export it".into(), now);
        f.transition(TaskStatus::Planning, None, now).unwrap();
        f.steps.push(Step {
            id: step.clone(),
            title: "Write it".into(),
            detail: None,
            status: StepStatus::Running,
            depends_on: vec![],
            workspace_id: None,
            session_id: None,
            attempts: 1,
            last_error: None,
            planning: false,
        });
        f.transition(TaskStatus::Implementing, None, now).unwrap();
        f
    }

    #[test]
    fn only_the_latest_cut_offs_of_the_same_step_count() {
        use RunState::*;
        let (a, b) = (StepId::from("a"), StepId::from("b"));
        let mut f = implementing(&a);
        for (t, s) in [
            (&a, Interrupted),
            (&a, Failed),
            (&a, Interrupted),
            // Another step's run doesn't break the streak, nor count in it.
            (&b, Interrupted),
            (&a, Interrupted),
            (&a, Running),
            // Runs after this one don't count.
            (&a, Interrupted),
        ] {
            f.runs.push(run_of(t, s));
        }
        assert_eq!(losses_before(&f, &f.runs[0]), 0);
        assert_eq!(
            losses_before(&f, &f.runs[2]),
            0,
            "a failure ends the streak"
        );
        assert_eq!(losses_before(&f, &f.runs[5]), 2);
        assert_eq!(losses_before(&f, &f.runs[3]), 0, "the other step has none");
    }

    #[test]
    fn a_run_needing_recovery_blocks_its_step_on_one_developer_decision() {
        let step = StepId::from("t1");
        let mut f = implementing(&step);
        let now = Utc::now();
        let changes = require_recovery(&mut f, &step, "otterd restarted", now);
        assert!(!changes.is_empty());
        assert_eq!(f.status, TaskStatus::Blocked);
        assert_eq!(f.steps[0].status, StepStatus::Blocked);
        assert_eq!(f.steps[0].last_error.as_deref(), Some("otterd restarted"));
        let open: Vec<_> = f.pending_decisions().collect();
        assert_eq!(open.len(), 1);
        let d = open[0];
        assert_eq!(d.kind, DecisionKind::Recovery);
        assert_eq!(d.step_id.as_ref(), Some(&step));
        assert!(d.user_only && d.run_id.is_none());
        assert_eq!(d.risk, Risk::High);
        assert!(d.summary.contains("Write it"), "{}", d.summary);
        assert!(
            d.detail
                .as_deref()
                .unwrap()
                .starts_with("otterd restarted."),
            "{:?}",
            d.detail
        );
        // Asked again while the first is open: still one decision.
        require_recovery(&mut f, &step, "lost again", now);
        assert_eq!(f.pending_decisions().count(), 1);
        assert_eq!(f.steps[0].last_error.as_deref(), Some("lost again"));
        // An unknown step changes nothing.
        assert!(require_recovery(&mut f, &StepId::from("nope"), "x", now).is_empty());
    }

    #[test]
    fn a_run_cut_off_too_often_needs_recovery_even_with_a_session() {
        let step = StepId::from("t1");
        let mut run = run_of(&step, RunState::Running);
        run.provider_session_id = Some("native-1".into());
        let (s, why) = unsettled(&run, None, None, MAX_LOSSES - 1, "lost");
        assert_eq!(s, RunState::RecoveryRequired);
        assert!(
            why.contains(&format!("{MAX_LOSSES} runs in a row")),
            "{why}"
        );
        let (s, why) = unsettled(&run, None, None, 0, "lost");
        assert_eq!(s, RunState::Interrupted);
        assert_eq!(why, "Interrupted: lost; resumable");
    }

    #[test]
    fn a_denial_says_who_said_no() {
        let deny = |by, why: Option<&str>| match decision_reply(
            false,
            None,
            None,
            by,
            why.map(String::from),
        ) {
            DecisionReply::Deny { message } => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(deny(Decider::User, None), "The developer declined this.");
        assert_eq!(
            deny(Decider::Controller, Some("not needed for the task")),
            "Otter declined this: not needed for the task"
        );
        assert_eq!(
            deny(Decider::Policy, Some(" ")),
            "Otter's policy doesn't allow this."
        );
        assert_eq!(
            decision_reply(true, Some("blue".into()), None, Decider::User, None),
            DecisionReply::Answer {
                text: "blue".into()
            }
        );
    }

    #[test]
    fn a_decision_keeps_the_whole_command_its_summary_cuts() {
        use crate::runtime::ToolCall;
        let cmd = |line: &str| ToolCall::Command { line: line.into() };
        // The summary shows it all already: nothing more.
        assert_eq!(whole_command(&cmd("rm -rf /opt/shared")), None);
        let path = "x".repeat(300);
        assert_eq!(whole_command(&ToolCall::Edit { path }), None);
        // Past the summary's cut, on several lines, a credential in it (B06).
        let line = format!(
            "python3 - <<'E'\n{}E\nGITHUB_TOKEN=ghp_WHOLEWHOLEWHOLEWHOLEWHOLE1234 rm -rf /opt/shared",
            "print('padding the line out')\n".repeat(10)
        );
        let summary = crate::agents::claude_stream::summarize(&cmd(&line), "Bash");
        assert!(!summary.contains("rm -rf"), "{summary}");
        let whole = whole_command(&cmd(&line)).expect("cut in the summary");
        assert!(whole.starts_with("python3 - <<'E'\nprint("), "{whole}");
        assert!(whole.ends_with(" rm -rf /opt/shared"), "{whole}");
        assert!(!whole.contains("WHOLEWHOLE"), "{whole}");
        // A short one on several lines is shown as it is, too.
        assert_eq!(
            whole_command(&cmd("cd a &&\nrm b")).as_deref(),
            Some("cd a &&\nrm b")
        );
        // Bounded, at a character, saying so.
        let long = format!("echo {}", "é".repeat(COMMAND_MAX));
        let kept = whole_command(&cmd(&long)).unwrap();
        let (head, tail) = kept.rsplit_once('\n').unwrap();
        assert!(head.len() <= COMMAND_MAX && long.starts_with(head));
        assert_eq!(
            tail,
            format!("… {} more bytes, not kept", long.len() - head.len())
        );
    }

    #[test]
    fn an_activity_line_is_redacted_before_it_is_cut() {
        use crate::runtime::ToolCall;
        for pad in 170..200 {
            let line = format!(
                "curl {} https://bob:hunter2secret@example.com",
                "x".repeat(pad)
            );
            let shown = activity_line("Bash", &ToolCall::Command { line });
            assert!(!shown.contains("hunt"), "{shown}");
        }
    }

    #[test]
    fn a_run_cut_off_is_resumed_only_when_that_is_safe() {
        use otter_core::conversation::{Command, Op};
        use otter_core::{StepId, TurnId};
        let at = Utc::now();
        let mut run = Run {
            read_only: false,
            id: RunId::from("run_1"),
            step_id: StepId::from("t1"),
            runtime: DEFAULT_RUNTIME.into(),
            state: RunState::Running,
            started_at: at,
            ended_at: None,
            summary: None,
            provider_session_id: None,
            activity: vec![],
            activity_at: vec![],
            heard_at: None,
            tools_running: 0,
            conversation_id: None,
            turn_id: None,
            worked_ms: 0,
            working_since: None,
        };
        let mut c = Conversation::new("claude", "sdk", "ws_1".into(), at);
        let turn = TurnId::generate();
        c.apply(&Op::Accept {
            command_id: "one".into(),
            fingerprint: "one".into(),
            command: Command::SendTurn {
                initiator: Initiator::Controller,
                input: vec![InputBlock::Text { text: "do".into() }],
                read_only: false,
            },
            turn_id: Some(turn.clone()),
            at,
        })
        .unwrap();
        c.apply(&Op::Claim {
            run: run.id.clone(),
            runtime: None,
            at,
        })
        .unwrap();
        let state = |run: &Run, c: &Conversation, broken: Option<&str>, lost: usize| {
            unsettled(run, Some(c), broken, lost, "lost").0
        };
        // Nothing reached the agent: starting again repeats nothing.
        assert_eq!(state(&run, &c, None, 0), RunState::Interrupted);
        c.apply(&Op::Sending {
            turn,
            generation: 1,
            at,
        })
        .unwrap();
        // It had the step, with no conversation to resume: the developer's call.
        assert_eq!(state(&run, &c, None, 0), RunState::RecoveryRequired);
        // A record from before conversations: the same.
        assert_eq!(
            unsettled(&run, None, None, 0, "lost").0,
            RunState::RecoveryRequired
        );
        // With one, it resumes — unless it keeps being cut off, or its
        // record can't be trusted.
        run.provider_session_id = Some("native-1".into());
        assert_eq!(state(&run, &c, None, 0), RunState::Interrupted);
        assert_eq!(state(&run, &c, None, 1), RunState::Interrupted);
        assert_eq!(state(&run, &c, None, 2), RunState::RecoveryRequired);
        let (s, why) = unsettled(&run, Some(&c), Some("records jump"), 0, "lost");
        assert_eq!(s, RunState::RecoveryRequired);
        assert!(why.contains("can't be trusted (records jump)"), "{why}");
    }

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
                read_only: false,
            },
            turn_id: Some(turn.clone()),
            at,
        };
        for op in [
            send("one", &a),
            Op::Claim {
                run: run.clone(),
                runtime: None,
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

    /// A turn carries the newest pictures that fit — four, 2 MB in all —
    /// in the order they were sent, and says how many it left out (D-181).
    #[test]
    fn a_turn_carries_the_newest_pictures_that_fit() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, kb: usize| {
            let path = dir.path().join(name);
            std::fs::write(&path, vec![0u8; kb * 1024]).unwrap();
            path
        };
        let six: Vec<(String, PathBuf)> = (1..=6)
            .map(|i| (format!("m{}", i.min(3)), file(&format!("{i}.png"), 10)))
            .collect();
        let (kept, left) = fitting(six.clone());
        assert_eq!(kept, six[2..].to_vec());
        assert_eq!(left, 2);
        // By size: three of 700 KB are over 2 MB together.
        let big: Vec<((), PathBuf)> = (1..=3)
            .map(|i| ((), file(&format!("big{i}.png"), 700)))
            .collect();
        let (kept, left) = fitting(big.clone());
        assert_eq!(kept, big[1..].to_vec());
        assert_eq!(left, 1);
        assert_eq!(fitting(six[..2].to_vec()), (six[..2].to_vec(), 0));
        assert_eq!(
            left_out(1),
            "(1 older picture the developer sent is left out: a turn carries at most 4, 2 MB in all.)"
        );
        assert!(left_out(2).starts_with("(2 older pictures the developer sent are left out"));
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
