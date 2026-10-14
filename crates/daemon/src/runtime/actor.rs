//! The run actor (D-055): one live run of one conversation, driven in order.
//!
//! The actor owns the [`RunHandle`]. It takes commands from a bounded
//! mailbox (deliver what's queued, answer an interaction, interrupt a turn,
//! stop) and events from the runtime, applies both to the conversation's
//! state ([`Conversations`]) with their generation checked, and reports
//! what happened, with Otter ids, on a channel. It never touches features:
//! whoever started it (the feature bridge in `runs.rs`) listens and does
//! that, so a slow feature store never holds up the runtime.
//!
//! One turn at a time: the next queued turn is delivered only after the
//! current one settled successfully. A turn that didn't (interrupted,
//! failed, limits) ends the run, and turns still queued are cancelled —
//! nothing is merged, nothing resent.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use otter_core::conversation::{
    Answer, Applied, Command, ErrorCategory, Interaction, InteractionKind, InteractionResponse,
    InteractionStatus, NativeBinding, Op, Rejection, ToolRecord, ToolResult, ToolStatus,
    TurnOutcome,
};
use otter_core::feature::Decider;
use otter_core::{BlockId, ConversationId, DecisionId, MessageId, RunId, ToolCallId, TurnId};
use tokio::sync::{mpsc, oneshot};

use super::conversations::Conversations;
use super::{
    CheckDecision, DecisionAsk, DecisionReply, RunHandle, RuntimeEvent, ToolCall, TurnSpec,
};

/// What the actor is asked to do.
pub enum ActorCmd {
    /// A turn was accepted into the conversation: deliver it when it's due.
    Wake,
    /// Answer an interaction this run asked.
    Decide {
        interaction: DecisionId,
        reply: DecisionReply,
        by: Decider,
    },
    /// Otter's policy for a tool call the runtime checks before running it.
    Check {
        check_id: String,
        decision: CheckDecision,
    },
    /// Stop this turn (only this one); the run doesn't go on by itself.
    Interrupt { turn: TurnId },
    /// Guidance that can't wait: the current turn is interrupted and this
    /// goes next, in the same run (D-059).
    Redirect {
        command_id: String,
        initiator: otter_core::conversation::Initiator,
        text: String,
    },
    /// Pause (a barrier: nothing more is delivered) or cancel (nothing more
    /// at all): what runs is interrupted, then the run ends.
    Hold {
        command_id: String,
        cancel: bool,
        done: oneshot::Sender<()>,
    },
    /// End the run. What it was doing ends as `outcome`.
    Stop {
        outcome: TurnOutcome,
        done: oneshot::Sender<()>,
    },
}

/// How long an interrupt may take to settle before the run is stopped
/// (`OTTER_INTERRUPT_DEADLINE_MS`, default 10 s).
fn interrupt_deadline() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("OTTER_INTERRUPT_DEADLINE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000),
    )
}

/// How long a finished turn may wait for the agent's background work
/// (`OTTER_BACKGROUND_WAIT_MS`, default 20 min; D-065).
fn background_wait() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("OTTER_BACKGROUND_WAIT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20 * 60 * 1000),
    )
}

/// Once the background work is done, how long the agent has to take it up
/// again before its turn finishes as it was (`OTTER_BACKGROUND_SETTLE_MS`,
/// default 5 s).
fn background_settle() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("OTTER_BACKGROUND_SETTLE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_000),
    )
}

/// A turn the agent finished while its background work still ran (D-065).
struct Held {
    summary: Option<String>,
    error: Option<ErrorCategory>,
    usage: Option<otter_core::conversation::Usage>,
    /// It finishes by then whatever runs.
    until: tokio::time::Instant,
}

/// What happened, with Otter's ids. Listeners use what they need (the
/// feature bridge needs fewer ids than a conversation view would).
#[derive(Debug)]
#[allow(dead_code)]
pub enum ActorEvent {
    /// The provider's session id is known (resumable).
    Bound {
        session_id: String,
    },
    /// A turn went to the provider.
    TurnStarted {
        turn: TurnId,
    },
    /// A piece of text being written.
    TextDelta {
        turn: TurnId,
        message: MessageId,
        block: BlockId,
        piece: String,
    },
    /// A whole text block.
    Text {
        turn: TurnId,
        message: MessageId,
        block: BlockId,
        text: String,
    },
    ToolStarted {
        record: ToolRecord,
        call: ToolCall,
    },
    /// The agent asks; answer with [`ActorCmd::Decide`].
    Ask {
        interaction: DecisionId,
        ask: DecisionAsk,
    },
    /// A tool call is about to run: answer with [`ActorCmd::Check`].
    Check {
        check_id: String,
        tool: String,
        call: ToolCall,
        tool_call: Option<ToolCallId>,
    },
    TurnFinished {
        turn: TurnId,
        outcome: TurnOutcome,
        summary: Option<String>,
        /// The turn delivered next, if the run goes on.
        next: Option<TurnId>,
    },
    /// The run is over.
    Ended {
        outcome: TurnOutcome,
        error: Option<String>,
        /// Set when it ended because it was asked to stop.
        stopped: Option<oneshot::Sender<()>>,
    },
}

/// When the run's agent was last heard from, and how many of its tool calls
/// are running: what the controller's no-progress watchdog reads (D-063).
#[derive(Debug)]
pub struct Pulse {
    pub at: std::time::Instant,
    pub tools: usize,
}

pub type SharedPulse = Arc<std::sync::Mutex<Pulse>>;

pub fn pulse() -> SharedPulse {
    Arc::new(std::sync::Mutex::new(Pulse {
        at: std::time::Instant::now(),
        tools: 0,
    }))
}

pub struct Actor {
    pub conversation: ConversationId,
    pub run: RunId,
    pub generation: u64,
    convs: Arc<Conversations>,
    handle: Box<dyn RunHandle>,
    out: mpsc::Sender<ActorEvent>,
    /// The provider's keys → Otter's ids, for this run.
    messages: HashMap<String, MessageId>,
    blocks: HashMap<(String, u32), BlockId>,
    tools: HashMap<String, ToolCallId>,
    /// Open interactions → the runtime's request id.
    asks: HashMap<DecisionId, String>,
    current: Option<TurnId>,
    /// Text being written, by block, and when it was last checkpointed.
    drafts: HashMap<BlockId, Checkpoint>,
    /// The journal couldn't be written: the run must stop (D-058).
    storage: Option<String>,
    /// An interrupt was asked for a redirect: when it settles, go on.
    redirecting: bool,
    /// Asked to pause or cancel: when the turn settles, end as this.
    holding: Option<(TurnOutcome, oneshot::Sender<()>)>,
    /// Stopped for good (a changed goal, a timeout): queued turns go too.
    stopping: bool,
    /// An interrupt that must settle by then, and how long it may take.
    interrupt_by: Option<tokio::time::Instant>,
    interrupt_after: std::time::Duration,
    /// Tool calls started and not yet finished, by the provider's key.
    running: std::collections::HashSet<String>,
    /// Background work the agent started that still runs, as the runtime
    /// counts it.
    background: usize,
    /// A finished turn waiting for that work, and when to stop waiting for
    /// the agent to take it up once the work is done.
    held: Option<Held>,
    settle_by: Option<tokio::time::Instant>,
    background_wait: std::time::Duration,
    background_settle: std::time::Duration,
    pulse: SharedPulse,
}

/// Text being written, journaled now and then so a crash keeps most of it.
struct Checkpoint {
    text: String,
    at: std::time::Instant,
    len: usize,
}

impl Checkpoint {
    /// Every second, or 8 KiB of new text, whichever first.
    const EVERY: std::time::Duration = std::time::Duration::from_secs(1);
    const BYTES: usize = 8 * 1024;

    fn due(&self) -> bool {
        self.at.elapsed() >= Self::EVERY || self.text.len() - self.len >= Self::BYTES
    }
}

enum Flow {
    Go,
    End,
}

impl Actor {
    pub fn new(
        conversation: ConversationId,
        run: RunId,
        generation: u64,
        convs: Arc<Conversations>,
        handle: Box<dyn RunHandle>,
        out: mpsc::Sender<ActorEvent>,
    ) -> Actor {
        Actor {
            conversation,
            run,
            generation,
            convs,
            handle,
            out,
            messages: HashMap::new(),
            blocks: HashMap::new(),
            tools: HashMap::new(),
            asks: HashMap::new(),
            current: None,
            drafts: HashMap::new(),
            storage: None,
            redirecting: false,
            holding: None,
            stopping: false,
            interrupt_by: None,
            interrupt_after: interrupt_deadline(),
            running: std::collections::HashSet::new(),
            background: 0,
            held: None,
            settle_by: None,
            background_wait: background_wait(),
            background_settle: background_settle(),
            pulse: pulse(),
        }
    }

    /// How long a turn waits for background work, and for the agent once
    /// it's done (tests).
    #[cfg(test)]
    pub fn background_timing(
        mut self,
        wait: std::time::Duration,
        settle: std::time::Duration,
    ) -> Actor {
        self.background_wait = wait;
        self.background_settle = settle;
        self
    }

    /// Report signs of work to `pulse`.
    pub fn with_pulse(mut self, pulse: SharedPulse) -> Actor {
        self.pulse = pulse;
        self
    }

    /// The agent did something (or was answered): note it.
    fn beat(&self) {
        let mut p = self.pulse.lock().unwrap();
        p.at = std::time::Instant::now();
        // Background work is work under way, like a tool that runs.
        p.tools = self.running.len() + self.background;
    }

    /// How long an interrupt may take to settle (tests).
    #[cfg(test)]
    pub fn interrupt_after(mut self, d: std::time::Duration) -> Actor {
        self.interrupt_after = d;
        self
    }

    /// Drive the run until it ends.
    pub async fn run(mut self, mut mailbox: mpsc::Receiver<ActorCmd>) {
        // Which process serves this generation, so a later daemon can make
        // sure it didn't outlive the run.
        if let Some(pid) = self.handle.inspect().pid
            && let Some(started) = super::process_started(pid)
        {
            self.apply(Op::Process {
                process: otter_core::conversation::ProcessRef {
                    generation: self.generation,
                    pid,
                    started,
                },
                at: Utc::now(),
            });
        }
        if let Flow::End = self.deliver_next().await {
            return;
        }
        loop {
            let deadline = self.interrupt_by;
            let wake = self
                .held
                .as_ref()
                .map(|h| self.settle_by.map_or(h.until, |s| s.min(h.until)));
            let flow = tokio::select! {
                ev = self.handle.next_event() => match ev {
                    Some(ev) => self.on_event(ev).await,
                    None => self.lost("the agent's output ended").await,
                },
                cmd = mailbox.recv() => match cmd {
                    Some(cmd) => self.on_cmd(cmd).await,
                    // Nobody can reach the run any more: end it.
                    None => self.stop(TurnOutcome::Interrupted, None).await,
                },
                // A held turn's wait is over.
                _ = tokio::time::sleep_until(wake.unwrap_or_else(tokio::time::Instant::now)), if wake.is_some() => {
                    self.release_held().await
                }
                // An interrupt that didn't settle in time: stop the run; what
                // its turn did is unknown.
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                    let why = format!(
                        "it didn't stop within {} ms of being interrupted",
                        self.interrupt_after.as_millis()
                    );
                    self.lost(&why).await
                }
            };
            if let Flow::End = flow {
                return;
            }
            // Nothing more can be recorded: stop rather than go on unrecorded.
            if let Some(why) = self.storage.take() {
                let _ = self.handle.stop().await;
                self.emit(ActorEvent::Ended {
                    outcome: TurnOutcome::OutcomeUnknown,
                    error: Some(format!("Otter couldn't record the conversation: {why}")),
                    stopped: None,
                })
                .await;
                return;
            }
        }
    }

    /// Apply a change to the conversation. `None`: it's gone.
    fn apply(&mut self, op: Op) -> Option<Result<Applied, Rejection>> {
        let r = self.convs.apply(&self.conversation, op);
        match &r {
            Some(Err(Rejection::Storage(why))) => self.storage = Some(why.clone()),
            Some(Err(e)) => tracing::debug!(conversation = %self.conversation, "not applied: {e}"),
            _ => {}
        }
        r
    }

    async fn emit(&self, ev: ActorEvent) {
        let _ = self.out.send(ev).await;
    }

    /// Ask the provider to stop the current turn, by a deadline.
    async fn interrupt_current(&mut self) -> Flow {
        // Waiting on background work: nothing runs to stop, so the turn
        // ends interrupted now (the work itself is spared, D-065).
        if let Some(h) = self.held.take() {
            return self
                .finish_turn(TurnOutcome::Interrupted, h.summary, h.error, h.usage)
                .await;
        }
        if self.current.is_some() {
            let _ = self.handle.interrupt().await;
            let after = self.interrupt_after;
            self.interrupt_by
                .get_or_insert_with(|| tokio::time::Instant::now() + after);
        }
        Flow::Go
    }

    /// Send the next queued turn, if one is due.
    async fn deliver_next(&mut self) -> Flow {
        if self.current.is_some() {
            return Flow::Go;
        }
        let g = self.generation;
        let Some(next) = self
            .convs
            .get(self.conversation.as_str())
            .and_then(|c| c.next_queued().cloned())
        else {
            return Flow::Go;
        };
        let sending = self.apply(Op::Sending {
            turn: next.id.clone(),
            generation: g,
            at: Utc::now(),
        });
        if !matches!(sending, Some(Ok(_))) {
            return Flow::Go;
        }
        let turn = TurnSpec {
            turn_id: next.id.clone(),
            text: next.text(),
        };
        self.current = Some(turn.turn_id.clone());
        if let Err(e) = self.handle.send_turn(&turn).await {
            return self.lost(&format!("couldn't send the turn: {e:#}")).await;
        }
        self.emit(ActorEvent::TurnStarted { turn: turn.turn_id })
            .await;
        Flow::Go
    }

    fn message_id(&mut self, key: &str) -> MessageId {
        self.messages
            .entry(key.to_owned())
            .or_insert_with(MessageId::generate)
            .clone()
    }

    fn block_id(&mut self, key: &str, block: u32) -> BlockId {
        self.blocks
            .entry((key.to_owned(), block))
            .or_insert_with(BlockId::generate)
            .clone()
    }

    async fn on_event(&mut self, ev: RuntimeEvent) -> Flow {
        let g = self.generation;
        let now = Utc::now();
        // What the agent and its tools printed is journaled and shown, never
        // a credential in it (D-075). Tool calls stay as they are: policy
        // reads them, and they're kept in the journal only by summary.
        let ev = scrub(ev);
        match &ev {
            RuntimeEvent::ToolStarted { call, .. } => {
                self.running.insert(call.clone());
            }
            RuntimeEvent::ToolFinished { call, .. } => {
                self.running.remove(call);
            }
            RuntimeEvent::TurnFinished { .. } => self.running.clear(),
            RuntimeEvent::Background { running } => {
                self.background = *running as usize;
                self.beat();
                // The work is done: the agent may take it up (a turn the
                // runtime starts itself), or the held turn finishes.
                if self.held.is_some() && self.background == 0 {
                    self.settle_by = Some(tokio::time::Instant::now() + self.background_settle);
                }
                return Flow::Go;
            }
            // The agent is at it again.
            _ => self.settle_by = None,
        }
        self.beat();
        match ev {
            RuntimeEvent::Session { id } => {
                self.apply(Op::Bind {
                    binding: NativeBinding {
                        session_id: id.clone(),
                    },
                    at: now,
                });
                self.emit(ActorEvent::Bound { session_id: id }).await;
            }
            RuntimeEvent::TurnDelivered => {
                if let Some(turn) = self.current.clone() {
                    self.apply(Op::Delivered {
                        turn,
                        generation: g,
                        at: now,
                    });
                }
            }
            RuntimeEvent::TextDelta {
                message,
                block,
                text,
            } => {
                let Some(turn) = self.current.clone() else {
                    return Flow::Go;
                };
                let (m, b) = (self.message_id(&message), self.block_id(&message, block));
                let draft = self.drafts.entry(b.clone()).or_insert_with(|| Checkpoint {
                    text: String::new(),
                    at: std::time::Instant::now(),
                    len: 0,
                });
                draft.text.push_str(&text);
                if draft.due() {
                    draft.at = std::time::Instant::now();
                    draft.len = draft.text.len();
                    let so_far = otter_core::redact::redact(&draft.text);
                    self.apply(Op::WriteBlock {
                        turn: turn.clone(),
                        message: m.clone(),
                        block: b.clone(),
                        text: so_far,
                        done: false,
                        at: now,
                    });
                }
                self.emit(ActorEvent::TextDelta {
                    turn,
                    message: m,
                    block: b,
                    piece: text,
                })
                .await;
            }
            RuntimeEvent::Text {
                message,
                block,
                text,
            } => {
                let Some(turn) = self.current.clone() else {
                    return Flow::Go;
                };
                let (m, b) = (self.message_id(&message), self.block_id(&message, block));
                self.drafts.remove(&b);
                self.apply(Op::WriteBlock {
                    turn: turn.clone(),
                    message: m.clone(),
                    block: b.clone(),
                    text: text.clone(),
                    done: true,
                    at: now,
                });
                self.emit(ActorEvent::Text {
                    turn,
                    message: m,
                    block: b,
                    text,
                })
                .await;
            }
            RuntimeEvent::ToolStarted {
                call,
                parent,
                tool,
                input,
            } => {
                let Some(turn) = self.current.clone() else {
                    return Flow::Go;
                };
                let id = ToolCallId::generate();
                self.tools.insert(call, id.clone());
                let record = ToolRecord {
                    id,
                    turn_id: turn,
                    run_id: self.run.clone(),
                    name: tool.clone(),
                    effect: input.effect(),
                    summary: crate::agents::excerpt(
                        &otter_core::redact::redact(&tool_summary(&tool, &input)),
                        200,
                    ),
                    parent: parent.and_then(|p| self.tools.get(&p).cloned()),
                    status: ToolStatus::Running,
                    started_at: now,
                    finished_at: None,
                    result: None,
                };
                self.apply(Op::ToolStarted {
                    record: record.clone(),
                    at: now,
                });
                self.emit(ActorEvent::ToolStarted {
                    record,
                    call: input,
                })
                .await;
            }
            RuntimeEvent::ToolFinished { call, ok, output } => {
                if let Some(id) = self.tools.get(&call).cloned() {
                    let result = ToolResult {
                        truncated: output.ends_with('…'),
                        summary: output,
                        artifact: None,
                    };
                    self.apply(Op::ToolFinished {
                        id,
                        result: Some((ok, result)),
                        at: now,
                    });
                }
            }
            RuntimeEvent::DecisionNeeded(ask) => {
                let Some(turn) = self.current.clone() else {
                    // Nothing to ask about: say no rather than leave it hanging.
                    let _ = self
                        .handle
                        .decide(
                            &ask.request_id,
                            DecisionReply::Deny {
                                message: "No turn is running.".into(),
                            },
                        )
                        .await;
                    return Flow::Go;
                };
                let id = DecisionId::generate();
                let kind = if !ask.questions.is_empty() {
                    InteractionKind::Questions {
                        questions: ask.questions.clone(),
                    }
                } else if let ToolCall::PlanApproval { plan } = &ask.call {
                    InteractionKind::Plan {
                        plan: plan.clone(),
                        revision: 0,
                    }
                } else {
                    InteractionKind::Permission {
                        tool: ask.tool.clone(),
                        effect: ask.call.effect(),
                        summary: ask.summary.clone(),
                    }
                };
                let interaction = Interaction {
                    id: id.clone(),
                    turn_id: turn,
                    run_id: self.run.clone(),
                    generation: g,
                    tool_call: ask
                        .tool_use_id
                        .as_ref()
                        .and_then(|t| self.tools.get(t).cloned()),
                    input_hash: Some(ask.input_hash.clone()),
                    kind,
                    status: InteractionStatus::Pending,
                    response: None,
                    decided_by: None,
                    requested_at: now,
                    decided_at: None,
                };
                self.apply(Op::InteractionRequested {
                    interaction,
                    at: now,
                });
                self.asks.insert(id.clone(), ask.request_id.clone());
                self.emit(ActorEvent::Ask {
                    interaction: id,
                    ask,
                })
                .await;
            }
            RuntimeEvent::ToolCheck {
                check_id,
                tool,
                call,
                tool_use_id,
            } => {
                let tool_call = tool_use_id.and_then(|t| self.tools.get(&t).cloned());
                self.emit(ActorEvent::Check {
                    check_id,
                    tool,
                    call,
                    tool_call,
                })
                .await;
            }
            RuntimeEvent::TurnFinished {
                outcome,
                summary,
                error,
                usage,
            } => {
                if self.current.is_none() {
                    return Flow::Go;
                }
                // Done for now, but its background work still runs: the turn
                // stays open, and what the agent does when that work ends
                // (the runtime starts a turn by itself) belongs to it (D-065).
                if outcome == TurnOutcome::Completed
                    && self.background > 0
                    && self.holding.is_none()
                    && !self.redirecting
                    && !self.stopping
                {
                    let until = self.held.as_ref().map_or_else(
                        || tokio::time::Instant::now() + self.background_wait,
                        |h| h.until,
                    );
                    tracing::debug!(conversation = %self.conversation, background = self.background, "the turn waits for background work");
                    self.held = Some(Held {
                        summary,
                        error,
                        usage,
                        until,
                    });
                    self.settle_by = None;
                    return Flow::Go;
                }
                return self.finish_turn(outcome, summary, error, usage).await;
            }
            RuntimeEvent::Background { .. } => {}
            RuntimeEvent::Exited { error, .. } => {
                let why = error.unwrap_or_else(|| "the agent exited before finishing".into());
                return self.lost(&why).await;
            }
        }
        Flow::Go
    }

    /// The turn is over: record it, then go on with the next or end the run.
    async fn finish_turn(
        &mut self,
        outcome: TurnOutcome,
        summary: Option<String>,
        error: Option<ErrorCategory>,
        usage: Option<otter_core::conversation::Usage>,
    ) -> Flow {
        let g = self.generation;
        let now = Utc::now();
        self.held = None;
        self.settle_by = None;
        let Some(turn) = self.current.take() else {
            return Flow::Go;
        };
        tracing::debug!(conversation = %self.conversation, %turn, ?outcome, turns = self.handle.inspect().turns, "turn settled");
        let reason = (outcome != TurnOutcome::Completed)
            .then(|| summary.clone())
            .flatten();
        self.apply(Op::Finished {
            turn: turn.clone(),
            generation: g,
            outcome,
            reason,
            error,
            usage,
            at: now,
        });
        self.apply(Op::CloseTools {
            turn: turn.clone(),
            at: now,
        });
        self.asks.clear();
        self.interrupt_by = None;
        let go_on = match outcome {
            TurnOutcome::Completed => self.holding.is_none(),
            TurnOutcome::Interrupted => self.redirecting && self.holding.is_none(),
            _ => false,
        };
        self.redirecting = false;
        let next = if go_on {
            if let Flow::End = self.deliver_next().await {
                return Flow::End;
            }
            self.current.clone()
        } else {
            None
        };
        self.emit(ActorEvent::TurnFinished {
            turn,
            outcome,
            summary,
            next: next.clone(),
        })
        .await;
        if next.is_none() {
            // Idle, held, or stopped short: the run is done. Its
            // context stays resumable.
            let _ = self.handle.stop().await;
            match self.holding.take() {
                Some((held, done)) => self.end(held, None, Some(done)).await,
                None => self.end(outcome, None, None).await,
            }
            return Flow::End;
        }
        Flow::Go
    }

    /// A held turn's wait is over (its background work ended and the agent
    /// had nothing more to say, or it took too long): it finishes as it was.
    async fn release_held(&mut self) -> Flow {
        match self.held.take() {
            Some(h) => {
                self.finish_turn(TurnOutcome::Completed, h.summary, h.error, h.usage)
                    .await
            }
            None => {
                self.settle_by = None;
                Flow::Go
            }
        }
    }

    async fn on_cmd(&mut self, cmd: ActorCmd) -> Flow {
        let g = self.generation;
        let now = Utc::now();
        // An answer sets the agent going again: the wait wasn't idleness.
        if matches!(cmd, ActorCmd::Decide { .. }) {
            self.beat();
        }
        match cmd {
            ActorCmd::Wake => return self.deliver_next().await,
            ActorCmd::Check { check_id, decision } => {
                if let Err(e) = self.handle.check(&check_id, decision).await {
                    tracing::warn!(%check_id, "answering a policy check: {e:#}");
                }
            }
            ActorCmd::Decide {
                interaction,
                reply,
                by,
            } => {
                let Some(request) = self.asks.remove(&interaction) else {
                    return Flow::Go;
                };
                let response = self.convs.get(self.conversation.as_str()).and_then(|c| {
                    let i = c.interactions.iter().find(|i| i.id == interaction)?;
                    Some(response_of(&i.kind, &reply))
                });
                if let Some(response) = response {
                    let fingerprint = format!("{response:?}");
                    self.apply(Op::Accept {
                        command_id: format!("resolve:{interaction}"),
                        fingerprint,
                        command: Command::Resolve {
                            interaction_id: interaction.clone(),
                            generation: g,
                            response,
                            by,
                        },
                        turn_id: None,
                        at: now,
                    });
                }
                let delivered = self.handle.decide(&request, reply).await;
                let op = match delivered {
                    Ok(()) => Op::InteractionDelivered {
                        id: interaction,
                        generation: g,
                        at: Utc::now(),
                    },
                    Err(_) => Op::InteractionUndelivered {
                        id: interaction,
                        at: Utc::now(),
                    },
                };
                self.apply(op);
            }
            ActorCmd::Interrupt { turn } => {
                let accepted = self.apply(Op::Accept {
                    command_id: format!("interrupt:{turn}:{g}"),
                    fingerprint: "interrupt".into(),
                    command: Command::Interrupt {
                        turn_id: turn.clone(),
                    },
                    turn_id: None,
                    at: now,
                });
                if matches!(accepted, Some(Ok(_))) && self.current.as_ref() == Some(&turn) {
                    return self.interrupt_current().await;
                }
            }
            ActorCmd::Redirect {
                command_id,
                initiator,
                text,
            } => {
                let accepted = self.apply(Op::Accept {
                    command_id,
                    fingerprint: text.clone(),
                    command: Command::Redirect {
                        initiator,
                        input: vec![otter_core::conversation::InputBlock::Text { text }],
                    },
                    turn_id: Some(TurnId::generate()),
                    at: now,
                });
                // A repeat changes nothing.
                if matches!(
                    accepted,
                    Some(Ok(Applied::Accepted(
                        otter_core::conversation::Accepted::New(_)
                    )))
                ) {
                    if self.current.is_some() {
                        self.redirecting = true;
                        return self.interrupt_current().await;
                    } else {
                        return self.deliver_next().await;
                    }
                }
            }
            ActorCmd::Hold {
                command_id,
                cancel,
                done,
            } => {
                let (command, outcome) = if cancel {
                    (Command::Cancel, TurnOutcome::Cancelled)
                } else {
                    (Command::Pause, TurnOutcome::Interrupted)
                };
                self.apply(Op::Accept {
                    command_id,
                    fingerprint: format!("{command:?}"),
                    command,
                    turn_id: None,
                    at: now,
                });
                if self.current.is_some() {
                    // Ends when the turn settles (or by the deadline).
                    self.holding = Some((outcome, done));
                    self.redirecting = false;
                    return self.interrupt_current().await;
                } else {
                    return self.stop(outcome, Some(done)).await;
                }
            }
            ActorCmd::Stop { outcome, done } => {
                self.stopping = true;
                return self.stop(outcome, Some(done)).await;
            }
        }
        Flow::Go
    }

    async fn stop(&mut self, outcome: TurnOutcome, done: Option<oneshot::Sender<()>>) -> Flow {
        let _ = self.handle.stop().await;
        self.end(outcome, None, done).await;
        Flow::End
    }

    /// The run is gone without settling what it was doing.
    async fn lost(&mut self, why: &str) -> Flow {
        if let Some(turn) = self.current.clone() {
            self.apply(Op::TurnError {
                turn,
                error: ErrorCategory::WorkerCrashed,
                reason: why.into(),
                at: Utc::now(),
            });
        }
        let _ = self.handle.stop().await;
        let done = self.holding.take().map(|(_, d)| d);
        self.end(TurnOutcome::OutcomeUnknown, Some(why.into()), done)
            .await;
        Flow::End
    }

    async fn end(
        &mut self,
        outcome: TurnOutcome,
        error: Option<String>,
        stopped: Option<oneshot::Sender<()>>,
    ) {
        let (g, now) = (self.generation, Utc::now());
        if let Some(turn) = self.current.take() {
            self.apply(Op::CloseTools { turn, at: now });
        }
        self.apply(Op::RunEnded {
            generation: g,
            outcome,
            at: now,
        });
        // Queued turns wait for the next run — unless this one failed or
        // was stopped for good (D-059).
        if matches!(
            outcome,
            TurnOutcome::Failed | TurnOutcome::LimitReached | TurnOutcome::Cancelled
        ) || self.stopping
        {
            self.apply(Op::CancelQueued {
                reason: "not delivered: the run ended first".into(),
                at: now,
            });
        }
        self.emit(ActorEvent::Ended {
            outcome,
            error,
            stopped,
        })
        .await;
    }
}

/// A reply in the conversation's terms. An answer goes to every question
/// (the legacy backend takes one answer per form, D-055).
fn response_of(kind: &InteractionKind, reply: &DecisionReply) -> InteractionResponse {
    match (kind, reply) {
        (_, DecisionReply::Deny { message }) => InteractionResponse::Deny {
            message: message.clone(),
        },
        (InteractionKind::Questions { questions }, DecisionReply::Answer { text }) => {
            InteractionResponse::Answers {
                answers: questions
                    .iter()
                    .map(|q| {
                        (
                            q.id.clone(),
                            Answer {
                                selected: q
                                    .options
                                    .iter()
                                    .filter(|o| &o.label == text)
                                    .map(|o| o.label.clone())
                                    .collect(),
                                text: Some(text.clone()),
                            },
                        )
                    })
                    .collect(),
            }
        }
        (_, DecisionReply::Answers { answers }) => InteractionResponse::Answers {
            answers: answers
                .iter()
                .map(|(q, a)| {
                    (
                        q.clone(),
                        Answer {
                            selected: vec![],
                            text: Some(a.clone()),
                        },
                    )
                })
                .collect(),
        },
        _ => InteractionResponse::Allow,
    }
}

/// A runtime event with credentials taken out of what it reports (D-075).
fn scrub(ev: RuntimeEvent) -> RuntimeEvent {
    use otter_core::redact::redact;
    match ev {
        RuntimeEvent::Text {
            message,
            block,
            text,
        } => RuntimeEvent::Text {
            message,
            block,
            text: redact(&text),
        },
        RuntimeEvent::ToolFinished { call, ok, output } => RuntimeEvent::ToolFinished {
            call,
            ok,
            output: redact(&output),
        },
        RuntimeEvent::DecisionNeeded(mut ask) => {
            ask.summary = redact(&ask.summary);
            RuntimeEvent::DecisionNeeded(ask)
        }
        RuntimeEvent::TurnFinished {
            outcome,
            summary,
            error,
            usage,
        } => RuntimeEvent::TurnFinished {
            outcome,
            summary: summary.map(|s| redact(&s)),
            error,
            usage,
        },
        RuntimeEvent::Exited { code, error } => RuntimeEvent::Exited {
            code,
            error: error.map(|e| redact(&e)),
        },
        other => other,
    }
}

fn tool_summary(tool: &str, call: &ToolCall) -> String {
    match call {
        ToolCall::Command { line } => line.clone(),
        ToolCall::Edit { path } => path.clone(),
        ToolCall::Fetch { url } => url.clone(),
        ToolCall::Question { question, .. } => question.clone(),
        ToolCall::PlanApproval { .. } => "plan".into(),
        ToolCall::Delegate { description } => description.clone(),
        ToolCall::Read | ToolCall::Outside { .. } | ToolCall::Other { .. } => tool.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RunInfo;
    use async_trait::async_trait;
    use otter_core::conversation::{
        Conversation, Delivery, Initiator, InputBlock, MessageLifecycle, TurnState,
    };
    use std::sync::Mutex;

    type Sent = Arc<Mutex<Vec<String>>>;
    type Decided = Arc<Mutex<Vec<(String, DecisionReply)>>>;

    /// A scripted runtime: each turn's text picks what it does.
    struct Fake {
        events: mpsc::Receiver<RuntimeEvent>,
        tx: mpsc::Sender<RuntimeEvent>,
        sent: Arc<Mutex<Vec<String>>>,
        decided: Arc<Mutex<Vec<(String, DecisionReply)>>>,
        /// The turn being worked on (`STUCK` ignores interrupts; `RACE`
        /// finishes as the interrupt arrives).
        last: String,
    }

    impl Fake {
        fn new() -> (Fake, Sent, Decided) {
            let (tx, events) = mpsc::channel(64);
            let sent = Arc::new(Mutex::new(vec![]));
            let decided = Arc::new(Mutex::new(vec![]));
            (
                Fake {
                    events,
                    tx,
                    sent: sent.clone(),
                    decided: decided.clone(),
                    last: String::new(),
                },
                sent,
                decided,
            )
        }
        fn say(&self, ev: RuntimeEvent) {
            self.tx.try_send(ev).unwrap();
        }
    }

    fn done() -> RuntimeEvent {
        RuntimeEvent::TurnFinished {
            outcome: TurnOutcome::Completed,
            summary: Some("ok".into()),
            error: None,
            usage: None,
        }
    }

    #[async_trait]
    impl RunHandle for Fake {
        async fn send_turn(&mut self, turn: &TurnSpec) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(turn.text.clone());
            self.last = turn.text.clone();
            self.say(RuntimeEvent::TurnDelivered);
            let text = |t: &str| RuntimeEvent::Text {
                message: format!("m-{}", turn.text),
                block: 0,
                text: t.into(),
            };
            match turn.text.as_str() {
                t if t.starts_with("ASK") => self.say(RuntimeEvent::DecisionNeeded(DecisionAsk {
                    request_id: "r1".into(),
                    tool: "AskUserQuestion".into(),
                    call: ToolCall::Question {
                        question: "Which?".into(),
                        options: vec!["a".into()],
                    },
                    summary: "Which?".into(),
                    tool_use_id: None,
                    input_hash: "h".into(),
                    questions: vec![otter_core::conversation::Question {
                        id: "Which?".into(),
                        header: None,
                        prompt: "Which?".into(),
                        options: vec![],
                        multi_select: false,
                    }],
                })),
                // Starts something in the background and waits for it.
                t if t.starts_with("BG") => {
                    self.say(RuntimeEvent::Background { running: 1 });
                    self.say(text("I'll wait for it."));
                    self.say(RuntimeEvent::TurnFinished {
                        outcome: TurnOutcome::Completed,
                        summary: Some("waiting".into()),
                        error: None,
                        usage: None,
                    });
                }
                t if t.starts_with("HANG") || t.starts_with("STUCK") || t.starts_with("RACE") => {
                    self.say(RuntimeEvent::TextDelta {
                        message: "m-h".into(),
                        block: 0,
                        text: "work".into(),
                    });
                    self.say(RuntimeEvent::ToolStarted {
                        call: "c1".into(),
                        parent: None,
                        tool: "Bash".into(),
                        input: ToolCall::Command {
                            line: "sleep".into(),
                        },
                    });
                }
                _ => {
                    self.say(RuntimeEvent::TextDelta {
                        message: format!("m-{}", turn.text),
                        block: 0,
                        text: "piece".into(),
                    });
                    self.say(text(&format!("did {}", turn.text)));
                    self.say(done());
                }
            }
            Ok(())
        }
        async fn next_event(&mut self) -> Option<RuntimeEvent> {
            self.events.recv().await
        }
        async fn decide(&mut self, request_id: &str, reply: DecisionReply) -> anyhow::Result<()> {
            self.decided
                .lock()
                .unwrap()
                .push((request_id.into(), reply));
            self.say(done());
            Ok(())
        }
        async fn interrupt(&mut self) -> anyhow::Result<()> {
            if self.last.starts_with("STUCK") {
                return Ok(());
            }
            let outcome = if self.last.starts_with("RACE") {
                TurnOutcome::Completed
            } else {
                TurnOutcome::Interrupted
            };
            self.say(RuntimeEvent::TurnFinished {
                outcome,
                summary: None,
                error: None,
                usage: None,
            });
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn inspect(&self) -> RunInfo {
            RunInfo::default()
        }
    }

    struct Setup {
        _dir: tempfile::TempDir,
        convs: Arc<Conversations>,
        id: ConversationId,
    }

    fn setup(turns: &[&str]) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let convs = Arc::new(Conversations::load(dir.path()).unwrap());
        let c = Conversation::new("claude", "fake", "ws_1".into(), Utc::now());
        let id = c.id.clone();
        convs.insert(c).unwrap();
        for (i, t) in turns.iter().enumerate() {
            send(&convs, &id, &format!("cmd_{i}"), t);
        }
        Setup {
            _dir: dir,
            convs,
            id,
        }
    }

    fn send(convs: &Conversations, id: &ConversationId, command: &str, text: &str) {
        convs
            .apply(
                id,
                Op::Accept {
                    command_id: command.into(),
                    fingerprint: text.into(),
                    command: Command::SendTurn {
                        initiator: Initiator::Controller,
                        input: vec![InputBlock::Text { text: text.into() }],
                    },
                    turn_id: Some(otter_core::TurnId::generate()),
                    at: Utc::now(),
                },
            )
            .unwrap()
            .unwrap();
    }

    fn spawn(
        s: &Setup,
        run: &str,
        fake: Fake,
    ) -> (
        mpsc::Sender<ActorCmd>,
        mpsc::Receiver<ActorEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        let g = s
            .convs
            .apply(
                &s.id,
                Op::Claim {
                    run: RunId::from(run),
                    runtime: None,
                    at: Utc::now(),
                },
            )
            .unwrap()
            .map(|a| match a {
                Applied::Claimed(g) => g,
                _ => unreachable!(),
            })
            .unwrap();
        let (tx, mailbox) = mpsc::channel(8);
        let (out, events) = mpsc::channel(64);
        let actor = Actor::new(
            s.id.clone(),
            RunId::from(run),
            g,
            s.convs.clone(),
            Box::new(fake),
            out,
        )
        .background_timing(
            std::time::Duration::from_millis(1500),
            std::time::Duration::from_millis(200),
        );
        (tx, events, tokio::spawn(actor.run(mailbox)))
    }

    fn finished(seen: &[ActorEvent]) -> Vec<(TurnOutcome, Option<String>)> {
        seen.iter()
            .filter_map(|e| match e {
                ActorEvent::TurnFinished {
                    outcome, summary, ..
                } => Some((*outcome, summary.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_turn_waits_for_its_background_work_and_ends_with_the_follow_up() {
        let s = setup(&["BG start it"]);
        let (fake, _, _) = Fake::new();
        let inject = fake.tx.clone();
        let (_tx, mut rx, _h) = spawn(&s, "run_bg", fake);
        // The agent's turn is done for now; the run isn't.
        let mut seen = vec![];
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
        {
            seen.push(ev);
        }
        assert!(finished(&seen).is_empty(), "{seen:?}");
        // The work ends; Claude Code takes it up by itself, in the same turn.
        inject
            .try_send(RuntimeEvent::Background { running: 0 })
            .unwrap();
        inject
            .try_send(RuntimeEvent::Text {
                message: "m-z".into(),
                block: 0,
                text: "It printed done.".into(),
            })
            .unwrap();
        inject
            .try_send(RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::Completed,
                summary: Some("done after waiting".into()),
                error: None,
                usage: None,
            })
            .unwrap();
        seen.extend(until_ended(&mut rx).await);
        assert_eq!(
            finished(&seen),
            [(TurnOutcome::Completed, Some("done after waiting".into()))]
        );
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(c.turns.len(), 1);
        assert!(
            c.messages
                .iter()
                .any(|m| m.turn_id == c.turns[0].id && m.text().contains("It printed done."))
        );
    }

    #[tokio::test]
    async fn a_held_turn_ends_when_its_work_ends_quietly_or_takes_too_long() {
        // The work ends and the agent says nothing more: done as it was.
        let s = setup(&["BG quiet"]);
        let (fake, _, _) = Fake::new();
        let inject = fake.tx.clone();
        let (_tx, mut rx, _h) = spawn(&s, "run_q", fake);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        inject
            .try_send(RuntimeEvent::Background { running: 0 })
            .unwrap();
        let seen = until_ended(&mut rx).await;
        assert_eq!(
            finished(&seen),
            [(TurnOutcome::Completed, Some("waiting".into()))]
        );
        // The work never ends: the turn doesn't wait for ever.
        let s = setup(&["BG forever"]);
        let (fake, _, _) = Fake::new();
        let started = std::time::Instant::now();
        let (_tx, mut rx, _h) = spawn(&s, "run_f", fake);
        let seen = until_ended(&mut rx).await;
        assert_eq!(
            finished(&seen),
            [(TurnOutcome::Completed, Some("waiting".into()))]
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(1400));
    }

    #[tokio::test]
    async fn interrupting_a_turn_that_waits_on_background_work_ends_it_at_once() {
        let s = setup(&["BG then stop"]);
        let (fake, _, _) = Fake::new();
        let pulse = pulse();
        let (tx, mut rx, _h) = {
            // As `spawn`, with a pulse to read.
            let g = s
                .convs
                .apply(
                    &s.id,
                    Op::Claim {
                        run: RunId::from("run_i"),
                        runtime: None,
                        at: Utc::now(),
                    },
                )
                .unwrap()
                .map(|a| match a {
                    Applied::Claimed(g) => g,
                    _ => unreachable!(),
                })
                .unwrap();
            let (tx, mailbox) = mpsc::channel(8);
            let (out, events) = mpsc::channel(64);
            let actor = Actor::new(
                s.id.clone(),
                RunId::from("run_i"),
                g,
                s.convs.clone(),
                Box::new(fake),
                out,
            )
            .with_pulse(pulse.clone());
            (tx, events, tokio::spawn(actor.run(mailbox)))
        };
        let turn = started(&mut rx).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // Background work counts as work under way (the watchdog).
        assert_eq!(pulse.lock().unwrap().tools, 1);
        let t0 = std::time::Instant::now();
        tx.send(ActorCmd::Interrupt { turn }).await.unwrap();
        let seen = until_ended(&mut rx).await;
        assert_eq!(
            finished(&seen),
            [(TurnOutcome::Interrupted, Some("waiting".into()))]
        );
        assert!(t0.elapsed() < std::time::Duration::from_secs(1));
    }

    async fn until_ended(rx: &mut mpsc::Receiver<ActorEvent>) -> Vec<ActorEvent> {
        let mut seen = vec![];
        while let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
        {
            let end = matches!(ev, ActorEvent::Ended { .. });
            seen.push(ev);
            if end {
                break;
            }
        }
        seen
    }

    #[tokio::test]
    async fn two_turns_one_after_the_other_in_one_conversation() {
        let s = setup(&["one", "two"]);
        let (fake, sent, _) = Fake::new();
        let (_tx, mut rx, task) = spawn(&s, "run_a", fake);
        let seen = until_ended(&mut rx).await;
        task.await.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            vec!["one", "two"],
            "each its own turn"
        );
        let finished: Vec<_> = seen
            .iter()
            .filter_map(|e| match e {
                ActorEvent::TurnFinished { next, .. } => Some(next.is_some()),
                _ => None,
            })
            .collect();
        assert_eq!(finished, vec![true, false]);
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert!(c.turns.iter().all(
            |t| t.outcome == Some(TurnOutcome::Completed) && t.delivery == Delivery::Delivered
        ));
        assert!(c.active_run.is_none(), "idle: the run ended");
        // Streamed and whole text: one message each, whole.
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.messages[0].text(), "did one");
        assert_eq!(c.messages[0].lifecycle, MessageLifecycle::Completed);

        // A restart of the conversation: a new run, a new generation.
        let g1 = c.generation;
        s.convs.get(s.id.as_str()).unwrap();
        send(&s.convs, &s.id, "cmd_9", "three");
        let (fake, sent, _) = Fake::new();
        let (_tx, mut rx, _task) = spawn(&s, "run_b", fake);
        until_ended(&mut rx).await;
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(c.generation, g1 + 1);
        assert_eq!(*sent.lock().unwrap(), vec!["three"]);
        assert_eq!(c.turns.last().unwrap().run_id, Some(RunId::from("run_b")));
    }

    #[tokio::test]
    async fn questions_are_asked_and_answered_by_interaction_id() {
        let s = setup(&["ASK"]);
        let (fake, _, decided) = Fake::new();
        let (tx, mut rx, _task) = spawn(&s, "run_a", fake);
        let interaction = loop {
            if let Some(ActorEvent::Ask { interaction, .. }) = rx.recv().await {
                break interaction;
            }
        };
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(c.turns[0].state, TurnState::Waiting);
        assert!(
            matches!(&c.interactions[0].kind, InteractionKind::Questions { questions } if questions.len() == 1)
        );
        tx.send(ActorCmd::Decide {
            interaction: interaction.clone(),
            reply: DecisionReply::Answer { text: "a".into() },
            by: Decider::User,
        })
        .await
        .unwrap();
        until_ended(&mut rx).await;
        assert_eq!(decided.lock().unwrap()[0].0, "r1");
        let c = s.convs.get(s.id.as_str()).unwrap();
        let i = &c.interactions[0];
        assert_eq!(i.status, InteractionStatus::Delivered);
        assert_eq!(i.decided_by, Some(Decider::User));
        assert!(
            matches!(&i.response, Some(InteractionResponse::Answers { answers }) if answers["Which?"].text.as_deref() == Some("a"))
        );
    }

    #[tokio::test]
    async fn an_interrupted_turn_ends_the_run_and_unsent_turns_wait_for_the_next() {
        let s = setup(&["HANG", "later"]);
        let (fake, sent, _) = Fake::new();
        let (tx, mut rx, _task) = spawn(&s, "run_a", fake);
        let turn = loop {
            if let Some(ActorEvent::TurnStarted { turn }) = rx.recv().await {
                break turn;
            }
        };
        tx.send(ActorCmd::Interrupt { turn: turn.clone() })
            .await
            .unwrap();
        let seen = until_ended(&mut rx).await;
        assert!(seen.iter().any(|e| matches!(
            e,
            ActorEvent::TurnFinished {
                outcome: TurnOutcome::Interrupted,
                next: None,
                ..
            }
        )));
        assert_eq!(
            *sent.lock().unwrap(),
            vec!["HANG"],
            "the queued turn never went"
        );
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(
            c.turn(&turn).unwrap().outcome,
            Some(TurnOutcome::Interrupted)
        );
        // Queued turns wait for the next run (D-059).
        let later = &c.turns[1];
        assert_eq!(later.state, TurnState::Queued);
        assert_eq!(later.delivery, Delivery::Queued);
        assert!(c.next_queued().is_some());
        // The tool it started never reported: not a success.
        assert_eq!(c.tools[0].status, ToolStatus::ResultUnavailable);
    }

    async fn started(rx: &mut mpsc::Receiver<ActorEvent>) -> TurnId {
        loop {
            if let Some(ActorEvent::TurnStarted { turn }) = rx.recv().await {
                return turn;
            }
        }
    }

    #[tokio::test]
    async fn a_redirect_stops_the_turn_and_goes_next_in_the_same_run() {
        let s = setup(&["HANG", "later"]);
        let (fake, sent, _) = Fake::new();
        let (tx, mut rx, _task) = spawn(&s, "run_a", fake);
        started(&mut rx).await;
        let redirect = || ActorCmd::Redirect {
            command_id: "redirect-1".into(),
            initiator: otter_core::conversation::Initiator::User,
            text: "go this way".into(),
        };
        tx.send(redirect()).await.unwrap();
        // Sent twice (a retry): applied once.
        tx.send(redirect()).await.unwrap();
        let seen = until_ended(&mut rx).await;
        assert!(seen.iter().any(|e| matches!(
            e,
            ActorEvent::TurnFinished {
                outcome: TurnOutcome::Interrupted,
                next: Some(_),
                ..
            }
        )));
        assert_eq!(
            *sent.lock().unwrap(),
            vec!["HANG", "go this way", "later"],
            "the redirect before what was queued"
        );
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(c.turns.len(), 3, "one redirect turn");
        assert!(
            c.turns
                .iter()
                .all(|t| t.run_id == Some(RunId::from("run_a")))
        );
        assert!(c.turns[2].redirect);
    }

    #[tokio::test]
    async fn pausing_while_a_question_waits_expires_it_and_holds_the_rest() {
        let s = setup(&["ASK", "later"]);
        let (fake, _, decided) = Fake::new();
        let (tx, mut rx, _task) = spawn(&s, "run_a", fake);
        while !matches!(rx.recv().await, Some(ActorEvent::Ask { .. })) {}
        let (done, wait) = oneshot::channel();
        tx.send(ActorCmd::Hold {
            command_id: "pause-1".into(),
            cancel: false,
            done,
        })
        .await
        .unwrap();
        let seen = until_ended(&mut rx).await;
        assert!(matches!(
            seen.last(),
            Some(ActorEvent::Ended {
                outcome: TurnOutcome::Interrupted,
                stopped: Some(_),
                ..
            })
        ));
        drop(seen);
        let _ = wait.await;
        assert!(decided.lock().unwrap().is_empty(), "never answered");
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(c.lifecycle, otter_core::conversation::Lifecycle::Paused);
        assert_eq!(c.interactions[0].status, InteractionStatus::Expired);
        assert_eq!(c.turns[0].outcome, Some(TurnOutcome::Interrupted));
    }

    #[tokio::test]
    async fn a_turn_that_finishes_as_it_is_interrupted_stays_completed() {
        let s = setup(&["RACE"]);
        let (fake, _, _) = Fake::new();
        let (tx, mut rx, _task) = spawn(&s, "run_a", fake);
        let turn = started(&mut rx).await;
        tx.send(ActorCmd::Interrupt { turn: turn.clone() })
            .await
            .unwrap();
        until_ended(&mut rx).await;
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(c.turn(&turn).unwrap().outcome, Some(TurnOutcome::Completed));
        // A late interrupt of it: stale, nothing changes.
        let late = s.convs.apply(
            &s.id,
            Op::Accept {
                command_id: "late".into(),
                fingerprint: "i".into(),
                command: Command::Interrupt { turn_id: turn },
                turn_id: None,
                at: Utc::now(),
            },
        );
        assert!(matches!(late, Some(Err(Rejection::Stale(_)))));
    }

    #[tokio::test]
    async fn an_interrupt_that_never_settles_ends_the_run_by_the_deadline() {
        let s = setup(&["STUCK"]);
        let (fake, _, _) = Fake::new();
        let g = s
            .convs
            .apply(
                &s.id,
                Op::Claim {
                    run: RunId::from("run_a"),
                    runtime: None,
                    at: Utc::now(),
                },
            )
            .unwrap()
            .map(|a| match a {
                Applied::Claimed(g) => g,
                _ => unreachable!(),
            })
            .unwrap();
        let (tx, mailbox) = mpsc::channel(8);
        let (out, mut rx) = mpsc::channel(64);
        let actor = Actor::new(
            s.id.clone(),
            RunId::from("run_a"),
            g,
            s.convs.clone(),
            Box::new(fake),
            out,
        )
        .interrupt_after(std::time::Duration::from_millis(300));
        tokio::spawn(actor.run(mailbox));
        let turn = started(&mut rx).await;
        let asked = std::time::Instant::now();
        tx.send(ActorCmd::Interrupt { turn: turn.clone() })
            .await
            .unwrap();
        let seen = until_ended(&mut rx).await;
        assert!(asked.elapsed() >= std::time::Duration::from_millis(300));
        assert!(seen.iter().any(|e| matches!(e,
            ActorEvent::Ended { outcome: TurnOutcome::OutcomeUnknown, error: Some(why), .. } if why.contains("didn't stop"))));
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(
            c.turn(&turn).unwrap().outcome,
            Some(TurnOutcome::OutcomeUnknown)
        );
    }

    #[tokio::test]
    async fn cancelling_closes_the_conversation() {
        let s = setup(&["HANG", "later"]);
        let (fake, _, _) = Fake::new();
        let (tx, mut rx, _task) = spawn(&s, "run_a", fake);
        started(&mut rx).await;
        let (done, _wait) = oneshot::channel();
        tx.send(ActorCmd::Hold {
            command_id: "cancel-1".into(),
            cancel: true,
            done,
        })
        .await
        .unwrap();
        let seen = until_ended(&mut rx).await;
        assert!(matches!(
            seen.last(),
            Some(ActorEvent::Ended {
                outcome: TurnOutcome::Cancelled,
                ..
            })
        ));
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(c.lifecycle, otter_core::conversation::Lifecycle::Closed);
        assert_eq!(c.turns[1].outcome, Some(TurnOutcome::Cancelled));
    }

    #[tokio::test]
    async fn long_text_is_checkpointed_while_it_is_written() {
        let s = setup(&["HANG"]);
        let (fake, _, _) = Fake::new();
        let say = fake.tx.clone();
        let (_tx, mut rx, _task) = spawn(&s, "run_a", fake);
        while !matches!(rx.recv().await, Some(ActorEvent::TurnStarted { .. })) {}
        // 9 KiB of new text: over the checkpoint size.
        say.send(RuntimeEvent::TextDelta {
            message: "m-h".into(),
            block: 0,
            text: "x".repeat(9 * 1024),
        })
        .await
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let c = s.convs.get(s.id.as_str()).unwrap();
            if let Some(m) = c.messages.first() {
                assert_eq!(m.lifecycle, MessageLifecycle::Streaming);
                assert!(m.text().len() > 9 * 1024, "the text so far");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no checkpoint");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_journal_that_cant_be_written_stops_the_run() {
        let s = setup(&["HANG"]);
        let (fake, _, _) = Fake::new();
        let say = fake.tx.clone();
        let (_tx, mut rx, _task) = spawn(&s, "run_a", fake);
        while !matches!(rx.recv().await, Some(ActorEvent::TurnStarted { .. })) {}
        s.convs
            .inject(Some(crate::runtime::conversations::Fault::BeforeAppend));
        say.send(RuntimeEvent::Text {
            message: "m-h".into(),
            block: 0,
            text: "done".into(),
        })
        .await
        .unwrap();
        let seen = until_ended(&mut rx).await;
        assert!(seen.iter().any(|e| matches!(e, ActorEvent::Ended { error: Some(e), .. } if e.contains("couldn't record"))));
        assert!(s.convs.read_only(s.id.as_str()).is_some());
    }

    #[tokio::test]
    async fn a_lost_run_leaves_its_turn_unknown() {
        let s = setup(&["HANG"]);
        let (fake, _, _) = Fake::new();
        let killer = fake.tx.clone();
        let (_tx, mut rx, _task) = spawn(&s, "run_a", fake);
        while !matches!(rx.recv().await, Some(ActorEvent::TurnStarted { .. })) {}
        killer
            .send(RuntimeEvent::Exited {
                code: Some(137),
                error: Some("killed".into()),
            })
            .await
            .unwrap();
        let seen = until_ended(&mut rx).await;
        assert!(seen.iter().any(|e| matches!(
            e,
            ActorEvent::Ended {
                outcome: TurnOutcome::OutcomeUnknown,
                ..
            }
        )));
        let c = s.convs.get(s.id.as_str()).unwrap();
        assert_eq!(c.turns[0].outcome, Some(TurnOutcome::OutcomeUnknown));
        assert_eq!(c.turns[0].error, Some(ErrorCategory::WorkerCrashed));
    }
}
