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
    Answer, Command, ErrorCategory, Interaction, InteractionKind, InteractionResponse,
    InteractionStatus, ToolRecord, ToolResult, ToolStatus, TurnOutcome,
};
use otter_core::feature::Decider;
use otter_core::{BlockId, ConversationId, DecisionId, MessageId, RunId, ToolCallId, TurnId};
use tokio::sync::{mpsc, oneshot};

use super::conversations::Conversations;
use super::{DecisionAsk, DecisionReply, RunHandle, RuntimeEvent, ToolCall, TurnSpec};

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
    /// Stop this turn (only this one). Sent by the run controls (interrupt,
    /// redirect) that come with milestone 4; handled and tested here.
    #[allow(dead_code)]
    Interrupt { turn: TurnId },
    /// End the run. What it was doing ends as `outcome`.
    Stop {
        outcome: TurnOutcome,
        done: oneshot::Sender<()>,
    },
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
        }
    }

    /// Drive the run until it ends.
    pub async fn run(mut self, mut mailbox: mpsc::Receiver<ActorCmd>) {
        if let Flow::End = self.deliver_next().await {
            return;
        }
        loop {
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
            };
            if let Flow::End = flow {
                return;
            }
        }
    }

    fn update<R>(
        &self,
        f: impl FnOnce(&mut otter_core::conversation::Conversation) -> R,
    ) -> Option<R> {
        self.convs.update(&self.conversation, f)
    }

    async fn emit(&self, ev: ActorEvent) {
        let _ = self.out.send(ev).await;
    }

    /// Send the next queued turn, if one is due.
    async fn deliver_next(&mut self) -> Flow {
        if self.current.is_some() {
            return Flow::Go;
        }
        let g = self.generation;
        let next = self
            .update(|c| {
                let t = c.next_queued()?.clone();
                c.sending(&t.id, g, Utc::now()).ok()?;
                Some(TurnSpec {
                    turn_id: t.id.clone(),
                    text: t.text(),
                })
            })
            .flatten();
        let Some(turn) = next else {
            return Flow::Go;
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
        match ev {
            RuntimeEvent::Session { id } => {
                let binding = otter_core::conversation::NativeBinding {
                    session_id: id.clone(),
                };
                self.update(|c| {
                    if c.binding.as_ref() != Some(&binding) {
                        c.binding = Some(binding);
                        c.revision += 1;
                        c.updated_at = now;
                    }
                });
                self.emit(ActorEvent::Bound { session_id: id }).await;
            }
            RuntimeEvent::TurnDelivered => {
                if let Some(t) = self.current.clone() {
                    self.update(|c| c.delivered(&t, g, now));
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
                self.update(|c| c.write_block(&turn, &m, &b, &text, true, now));
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
                    summary: crate::agents::excerpt(&tool_summary(&tool, &input), 200),
                    parent: parent.and_then(|p| self.tools.get(&p).cloned()),
                    status: ToolStatus::Running,
                    started_at: now,
                    finished_at: None,
                    result: None,
                };
                self.update(|c| c.tool_started(record.clone(), now));
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
                    self.update(|c| c.tool_finished(&id, Some((ok, result)), now));
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
                self.update(|c| c.interaction_requested(interaction, now));
                self.asks.insert(id.clone(), ask.request_id.clone());
                self.emit(ActorEvent::Ask {
                    interaction: id,
                    ask,
                })
                .await;
            }
            RuntimeEvent::TurnFinished {
                outcome,
                summary,
                error,
                usage,
            } => {
                let Some(turn) = self.current.take() else {
                    return Flow::Go;
                };
                tracing::debug!(conversation = %self.conversation, %turn, ?outcome, turns = self.handle.inspect().turns, "turn settled");
                let reason = (outcome != TurnOutcome::Completed)
                    .then(|| summary.clone())
                    .flatten();
                self.update(|c| {
                    let _ = c.finished(&turn, g, outcome, reason, usage, now);
                    c.close_tools(&turn, now);
                    if let Some(t) = c.turns.iter_mut().find(|t| t.id == turn) {
                        t.error = error;
                    }
                });
                self.asks.clear();
                let next = if outcome == TurnOutcome::Completed {
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
                    // Idle (or stopped short): the run is done. Its context
                    // stays resumable.
                    let _ = self.handle.stop().await;
                    self.end(outcome, None, None).await;
                    return Flow::End;
                }
            }
            RuntimeEvent::Exited { error, .. } => {
                let why = error.unwrap_or_else(|| "the agent exited before finishing".into());
                return self.lost(&why).await;
            }
        }
        Flow::Go
    }

    async fn on_cmd(&mut self, cmd: ActorCmd) -> Flow {
        let g = self.generation;
        let now = Utc::now();
        match cmd {
            ActorCmd::Wake => return self.deliver_next().await,
            ActorCmd::Decide {
                interaction,
                reply,
                by,
            } => {
                let Some(request) = self.asks.remove(&interaction) else {
                    return Flow::Go;
                };
                let response = self
                    .update(|c| {
                        let i = c.interactions.iter().find(|i| i.id == interaction)?;
                        Some(response_of(&i.kind, &reply))
                    })
                    .flatten();
                if let Some(response) = response {
                    let fingerprint = format!("{response:?}");
                    let accepted = self.update(|c| {
                        c.accept(
                            &format!("resolve:{interaction}"),
                            &fingerprint,
                            Command::Resolve {
                                interaction_id: interaction.clone(),
                                generation: g,
                                response,
                                by,
                            },
                            now,
                        )
                    });
                    if let Some(Err(e)) = accepted {
                        tracing::debug!(%interaction, "resolving: {e}");
                    }
                }
                let delivered = self.handle.decide(&request, reply).await;
                self.update(|c| match delivered {
                    Ok(()) => {
                        let _ = c.interaction_delivered(&interaction, g, now);
                    }
                    Err(_) => {
                        if let Some(i) = c.interactions.iter_mut().find(|i| i.id == interaction) {
                            i.status = InteractionStatus::DeliveryUnknown;
                        }
                    }
                });
            }
            ActorCmd::Interrupt { turn } => {
                let accepted = self.update(|c| {
                    c.accept(
                        &format!("interrupt:{turn}:{g}"),
                        "interrupt",
                        Command::Interrupt {
                            turn_id: turn.clone(),
                        },
                        now,
                    )
                });
                if matches!(accepted, Some(Ok(_))) && self.current.as_ref() == Some(&turn) {
                    let _ = self.handle.interrupt().await;
                }
            }
            ActorCmd::Stop { outcome, done } => {
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
        if let Some(current) = self.current.clone() {
            self.update(|c| {
                if let Some(t) = c.turns.iter_mut().find(|t| t.id == current) {
                    t.error = Some(ErrorCategory::WorkerCrashed);
                    t.reason = Some(why.into());
                }
            });
        }
        let _ = self.handle.stop().await;
        self.end(TurnOutcome::OutcomeUnknown, Some(why.into()), None)
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
        let current = self.current.take();
        self.update(|c| {
            if let Some(t) = &current {
                c.close_tools(t, now);
            }
            c.run_ended(g, outcome, now);
            c.cancel_queued("not delivered: the run ended first", now);
        });
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
        _ => InteractionResponse::Allow,
    }
}

fn tool_summary(tool: &str, call: &ToolCall) -> String {
    match call {
        ToolCall::Command { line } => line.clone(),
        ToolCall::Edit { path } => path.clone(),
        ToolCall::Fetch { url } => url.clone(),
        ToolCall::Question { question, .. } => question.clone(),
        ToolCall::PlanApproval { .. } => "plan".into(),
        ToolCall::Read | ToolCall::Other { .. } => tool.to_owned(),
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
                t if t.starts_with("HANG") => {
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
            self.say(RuntimeEvent::TurnFinished {
                outcome: TurnOutcome::Interrupted,
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
        convs.insert(c);
        for (i, t) in turns.iter().enumerate() {
            convs
                .update(&id, |c| {
                    c.accept(
                        &format!("cmd_{i}"),
                        t,
                        Command::SendTurn {
                            initiator: Initiator::Controller,
                            input: vec![InputBlock::Text { text: (*t).into() }],
                        },
                        Utc::now(),
                    )
                    .unwrap()
                })
                .unwrap();
        }
        Setup {
            _dir: dir,
            convs,
            id,
        }
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
            .update(&s.id, |c| c.claim(RunId::from(run), Utc::now()).unwrap())
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
        );
        (tx, events, tokio::spawn(actor.run(mailbox)))
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
        s.convs
            .update(&s.id, |c| {
                c.accept(
                    "cmd_9",
                    "three",
                    Command::SendTurn {
                        initiator: Initiator::Controller,
                        input: vec![InputBlock::Text {
                            text: "three".into(),
                        }],
                    },
                    Utc::now(),
                )
                .unwrap()
            })
            .unwrap();
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
    async fn an_interrupted_turn_ends_the_run_and_unsent_turns_are_cancelled() {
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
        let later = &c.turns[1];
        assert_eq!(later.outcome, Some(TurnOutcome::Cancelled));
        assert_eq!(later.delivery, Delivery::Queued);
        // The tool it started never reported: not a success.
        assert_eq!(c.tools[0].status, ToolStatus::ResultUnavailable);
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
