//! Runtime conversations (D-055): the coding agent's side of the work, with
//! its own identity, apart from the feature it serves.
//!
//! A [`Conversation`] is one durable coding-agent context in one workspace
//! (Claude's native session, behind an opaque [`NativeBinding`]). It is made
//! of [`Turn`]s — one accepted input each and the work that followed —
//! served by [`RunId`]s: a run is one live process, a restart is a new run
//! (and a new `generation`) of the same conversation. What the agent says is
//! [`Message`]s of [`Block`]s with stable ids, so streamed and finished text
//! are the same thing; what it does is [`ToolRecord`]s, whose result is
//! known or explicitly not; what it asks is [`Interaction`]s.
//!
//! Kept apart, on purpose:
//!
//! - a turn **completed** means the provider finished executing it; whether
//!   the feature's work is **done** is the controller's call, from evidence;
//! - an input **accepted** by Otter (a receipt) is not an input **delivered**
//!   to the provider ([`Delivery`]);
//! - a tool **started** is not a tool that **succeeded**: no result is
//!   [`ToolStatus::ResultUnavailable`] or pending, never success.
//!
//! The rules ([`Conversation::accept`] and the runtime-side transitions) are
//! pure: they validate targets, generations and command ids, and say what
//! changes. Durability (a journal) is the daemon's business.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::feature::{Decider, MessageRole};
use crate::ids::{
    BlockId, ConversationId, DecisionId, FeatureId, MessageId, RunId, TaskId, ToolCallId, TurnId,
    WorkspaceId,
};
use crate::model::Timestamp;

/// Version of a persisted [`Conversation`].
pub const CONVERSATION_SCHEMA: u32 = 1;

/// The provider's own handle on the conversation (e.g. Claude's session id).
/// Private to the runtime adapter: clients see whether it is resumable.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeBinding {
    pub session_id: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// Accepting turns.
    #[default]
    Open,
    /// A scheduling barrier: nothing new is delivered until resumed.
    Paused,
    /// Cancelled: no new turns.
    Closed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Conversation {
    #[serde(default = "schema")]
    pub schema: u32,
    pub id: ConversationId,
    /// The agent, e.g. `claude`.
    pub provider: String,
    /// How it is driven, e.g. `legacy_cli` or `sdk`.
    pub backend: String,
    pub workspace_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<FeatureId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Bumped by every accepted change.
    pub revision: u64,
    #[serde(default)]
    pub lifecycle: Lifecycle,
    /// The run serving it now, if one is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<RunId>,
    /// Bumped by every new run: events and commands of an older generation
    /// can't touch a newer one.
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<NativeBinding>,
    /// The coding model asked for (`None`: the provider's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_fingerprint: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub turns: Vec<Turn>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolRecord>,
    #[serde(default)]
    pub interactions: Vec<Interaction>,
    /// Commands accepted, by client id, for idempotency.
    #[serde(default)]
    pub receipts: Vec<Receipt>,
}

fn schema() -> u32 {
    CONVERSATION_SCHEMA
}

/// Who sent a turn's input.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Initiator {
    /// Otter (the controller), the only sender of coding work for now.
    Controller,
    User,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputBlock {
    Text {
        text: String,
    },
    /// A file on the host, by reference (never inline bytes).
    Attachment {
        artifact: String,
    },
}

/// Whether a turn's input reached the provider. Accepted is not delivered.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Queued,
    Sending,
    Delivered,
    /// Lost between sending and a confirmed receipt: never resent on its own.
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Queued,
    Running,
    /// Waiting on an interaction.
    Waiting,
    /// Interrupt asked for; waiting for the provider to settle.
    Interrupting,
    Finished,
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    /// The provider finished it successfully (not: the work is accepted).
    Completed,
    Interrupted,
    Cancelled,
    Failed,
    LimitReached,
    /// Execution was lost; what happened isn't known.
    OutcomeUnknown,
}

/// Why something failed, for machines (retry advice is metadata, not
/// permission to resubmit).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    AuthRequired,
    ProviderUnavailable,
    RateLimited,
    ResumeUnavailable,
    ProtocolIncompatible,
    WorkerCrashed,
    StorageFailed,
    InvalidInput,
}

/// What a figure covers: providers report some per turn, some for the
/// whole session so far. Never add session totals up.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    Turn,
    SessionCumulative,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub scope: UsageScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub id: TurnId,
    /// The command that asked for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    pub initiator: Initiator,
    pub input: Vec<InputBlock>,
    pub delivery: Delivery,
    pub state: TurnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TurnOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCategory>,
    /// The run (and its generation) it was delivered in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub queued_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl Turn {
    pub fn is_over(&self) -> bool {
        self.state == TurnState::Finished
    }

    /// The input as plain text (attachments by reference).
    pub fn text(&self) -> String {
        self.input
            .iter()
            .map(|b| match b {
                InputBlock::Text { text } => text.clone(),
                InputBlock::Attachment { artifact } => format!("[attachment {artifact}]"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageLifecycle {
    Streaming,
    Completed,
    /// Cut off (interrupted, or the run was lost) before it was finished.
    Interrupted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Block {
    pub id: BlockId,
    #[serde(flatten)]
    pub kind: BlockKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Text { text: String },
}

/// What the agent said: streamed text and the finished message share these
/// ids, so the finished one replaces what was streamed instead of adding it
/// again.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub turn_id: TurnId,
    pub role: MessageRole,
    pub blocks: Vec<Block>,
    pub revision: u64,
    pub lifecycle: MessageLifecycle,
    pub at: Timestamp,
}

impl Message {
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .map(|b| match &b.kind {
                BlockKind::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// What a tool does, generically.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    Read,
    Edit,
    Command,
    Fetch,
    Question,
    Plan,
    /// Starts another agent (a subagent).
    Delegate,
    Other,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Asked for; not started (e.g. waiting for permission).
    Pending,
    Running,
    Succeeded,
    Failed,
    /// Not allowed to run.
    Denied,
    /// It ran (or may have), but its result was never reported.
    ResultUnavailable,
}

impl ToolStatus {
    pub fn is_over(self) -> bool {
        !matches!(self, ToolStatus::Pending | ToolStatus::Running)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    /// Bounded; the rest, if any, is in `artifact`.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolRecord {
    pub id: ToolCallId,
    pub turn_id: TurnId,
    pub run_id: RunId,
    /// The provider's tool name, e.g. `Bash`.
    pub name: String,
    pub effect: ToolEffect,
    /// The input, summarized and bounded.
    pub summary: String,
    /// The tool call that started this one (a subagent's tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ToolCallId>,
    pub status: ToolStatus,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolResult>,
}

/// One question of a (possibly multi-question) form.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Question {
    /// Stable within the interaction (the provider's, or its position).
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The answer to one question: options chosen, and/or free text.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Answer {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionKind {
    /// May this tool call run?
    Permission {
        tool: String,
        effect: ToolEffect,
        summary: String,
    },
    /// Every question of the form, as asked.
    Questions { questions: Vec<Question> },
    /// Approve a plan the agent proposes.
    Plan {
        plan: String,
        #[serde(default)]
        revision: u32,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionStatus {
    Pending,
    /// Decided; not yet passed to the provider.
    ResponseRecorded,
    Delivered,
    Denied,
    /// Its run or turn ended first.
    Expired,
    Cancelled,
    DeliveryUnknown,
}

impl InteractionStatus {
    pub fn is_open(self) -> bool {
        matches!(
            self,
            InteractionStatus::Pending | InteractionStatus::ResponseRecorded
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionResponse {
    Allow,
    Deny {
        message: String,
    },
    /// Per question id.
    Answers {
        answers: BTreeMap<String, Answer>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Interaction {
    pub id: DecisionId,
    pub turn_id: TurnId,
    pub run_id: RunId,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<ToolCallId>,
    /// A hash of the exact input: an approval is for this input only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hash: Option<String>,
    #[serde(flatten)]
    pub kind: InteractionKind,
    pub status: InteractionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<InteractionResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Decider>,
    pub requested_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<Timestamp>,
}

/// A command accepted: the same id again gets this back.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub command_id: String,
    /// Identifies the payload: the same id with another payload conflicts.
    pub fingerprint: String,
    /// The revision it was accepted at.
    pub revision: u64,
    /// What it created or targeted (a turn, an interaction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<DecisionId>,
    /// Kept only in memory so far (no journal yet): a retry after a daemon
    /// restart isn't recognized.
    #[serde(default)]
    pub durable: bool,
}

/// A command on a conversation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// A new turn: delivered now if idle, else queued after the current
    /// one. Never merged with another turn.
    SendTurn {
        initiator: Initiator,
        input: Vec<InputBlock>,
    },
    /// Stop this turn (only this one).
    Interrupt { turn_id: TurnId },
    /// Answer this interaction, asked in this generation.
    Resolve {
        interaction_id: DecisionId,
        generation: u64,
        response: InteractionResponse,
        by: Decider,
    },
}

/// Why a command wasn't accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// That command id was used for something else.
    Conflict(String),
    /// It targets a turn, run or interaction that has moved on.
    Stale(String),
    NotFound(String),
    /// Not in this state.
    Invalid(String),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::Conflict(m)
            | Rejection::Stale(m)
            | Rejection::NotFound(m)
            | Rejection::Invalid(m) => f.write_str(m),
        }
    }
}

/// What a command did.
#[derive(Clone, Debug, PartialEq)]
pub enum Accepted {
    /// Newly accepted.
    New(Receipt),
    /// The same command again: nothing changed.
    Repeat(Receipt),
}

impl Accepted {
    pub fn receipt(&self) -> &Receipt {
        match self {
            Accepted::New(r) | Accepted::Repeat(r) => r,
        }
    }
}

impl Conversation {
    pub fn new(
        provider: &str,
        backend: &str,
        workspace_id: WorkspaceId,
        now: Timestamp,
    ) -> Conversation {
        Conversation {
            schema: CONVERSATION_SCHEMA,
            id: ConversationId::generate(),
            provider: provider.into(),
            backend: backend.into(),
            workspace_id,
            feature_id: None,
            task_id: None,
            revision: 0,
            lifecycle: Lifecycle::Open,
            active_run: None,
            generation: 0,
            binding: None,
            model: None,
            config_fingerprint: None,
            created_at: now,
            updated_at: now,
            turns: vec![],
            messages: vec![],
            tools: vec![],
            interactions: vec![],
            receipts: vec![],
        }
    }

    pub fn turn(&self, id: &TurnId) -> Option<&Turn> {
        self.turns.iter().find(|t| &t.id == id)
    }

    fn turn_mut(&mut self, id: &TurnId) -> Option<&mut Turn> {
        self.turns.iter_mut().find(|t| &t.id == id)
    }

    /// The turn being executed (delivered, not finished).
    pub fn active_turn(&self) -> Option<&Turn> {
        self.turns.iter().find(|t| {
            matches!(
                t.state,
                TurnState::Running | TurnState::Waiting | TurnState::Interrupting
            )
        })
    }

    /// The next turn to deliver, if nothing is running and nothing holds
    /// delivery back.
    pub fn next_queued(&self) -> Option<&Turn> {
        if self.lifecycle != Lifecycle::Open || self.active_turn().is_some() {
            return None;
        }
        self.turns.iter().find(|t| t.state == TurnState::Queued)
    }

    fn touch(&mut self, now: Timestamp) {
        self.revision += 1;
        self.updated_at = now;
    }

    /// Accept a client's command. `fingerprint` identifies its payload
    /// (e.g. canonical JSON): the same command id again with the same
    /// fingerprint returns the original receipt; with another, a conflict.
    pub fn accept(
        &mut self,
        command_id: &str,
        fingerprint: &str,
        command: Command,
        now: Timestamp,
    ) -> Result<Accepted, Rejection> {
        self.accept_as(command_id, fingerprint, command, None, now)
    }

    /// [`Self::accept`], with the new turn's id chosen by the caller (so a
    /// replayed [`Op::Accept`] gives the same id).
    fn accept_as(
        &mut self,
        command_id: &str,
        fingerprint: &str,
        command: Command,
        new_turn: Option<TurnId>,
        now: Timestamp,
    ) -> Result<Accepted, Rejection> {
        if let Some(r) = self.receipts.iter().find(|r| r.command_id == command_id) {
            return if r.fingerprint == fingerprint {
                Ok(Accepted::Repeat(r.clone()))
            } else {
                Err(Rejection::Conflict(format!(
                    "command `{command_id}` was already used for something else"
                )))
            };
        }
        let mut receipt = Receipt {
            command_id: command_id.into(),
            fingerprint: fingerprint.into(),
            revision: 0,
            turn_id: None,
            interaction_id: None,
            durable: false,
        };
        match command {
            Command::SendTurn { initiator, input } => {
                if self.lifecycle == Lifecycle::Closed {
                    return Err(Rejection::Invalid("the conversation is closed".into()));
                }
                if input.is_empty() {
                    return Err(Rejection::Invalid("the turn has no input".into()));
                }
                let id = new_turn.unwrap_or_else(TurnId::generate);
                self.turns.push(Turn {
                    id: id.clone(),
                    command_id: Some(command_id.into()),
                    initiator,
                    input,
                    delivery: Delivery::Queued,
                    state: TurnState::Queued,
                    outcome: None,
                    reason: None,
                    error: None,
                    run_id: None,
                    generation: None,
                    queued_at: now,
                    started_at: None,
                    ended_at: None,
                    usage: None,
                });
                receipt.turn_id = Some(id);
            }
            Command::Interrupt { turn_id } => {
                let turn = self
                    .turn_mut(&turn_id)
                    .ok_or_else(|| Rejection::NotFound(format!("no turn `{turn_id}`")))?;
                match turn.state {
                    TurnState::Finished => {
                        return Err(Rejection::Stale(format!(
                            "turn `{turn_id}` is already over"
                        )));
                    }
                    // Not started: it simply never will be.
                    TurnState::Queued => {
                        turn.state = TurnState::Finished;
                        turn.outcome = Some(TurnOutcome::Cancelled);
                        turn.reason = Some("interrupted before it started".into());
                        turn.ended_at = Some(now);
                    }
                    TurnState::Interrupting => {}
                    TurnState::Running | TurnState::Waiting => {
                        turn.state = TurnState::Interrupting;
                    }
                }
                receipt.turn_id = Some(turn_id);
            }
            Command::Resolve {
                interaction_id,
                generation,
                response,
                by,
            } => {
                let current = self.generation;
                let i = self
                    .interactions
                    .iter_mut()
                    .find(|i| i.id == interaction_id)
                    .ok_or_else(|| {
                        Rejection::NotFound(format!("no interaction `{interaction_id}`"))
                    })?;
                if i.generation != generation || generation != current {
                    return Err(Rejection::Stale(format!(
                        "interaction `{interaction_id}` belongs to another run"
                    )));
                }
                if i.status != InteractionStatus::Pending {
                    return Err(Rejection::Stale(format!(
                        "interaction `{interaction_id}` is no longer waiting"
                    )));
                }
                if let (
                    InteractionKind::Questions { questions },
                    InteractionResponse::Answers { answers },
                ) = (&i.kind, &response)
                    && let Some(q) = questions.iter().find(|q| !answers.contains_key(&q.id))
                {
                    return Err(Rejection::Invalid(format!(
                        "no answer to question `{}`",
                        q.id
                    )));
                }
                i.status = match response {
                    InteractionResponse::Deny { .. } => InteractionStatus::Denied,
                    _ => InteractionStatus::ResponseRecorded,
                };
                i.response = Some(response);
                i.decided_by = Some(by);
                i.decided_at = Some(now);
                receipt.interaction_id = Some(interaction_id);
            }
        }
        self.touch(now);
        receipt.revision = self.revision;
        self.receipts.push(receipt.clone());
        Ok(Accepted::New(receipt))
    }

    /// A new run takes the conversation: a new generation. Refused while
    /// another run holds it — never two drivers.
    pub fn claim(&mut self, run: RunId, now: Timestamp) -> Result<u64, Rejection> {
        if let Some(r) = &self.active_run {
            return Err(Rejection::Invalid(format!(
                "run `{r}` is still serving this conversation"
            )));
        }
        if self.lifecycle == Lifecycle::Closed {
            return Err(Rejection::Invalid("the conversation is closed".into()));
        }
        self.generation += 1;
        self.active_run = Some(run);
        self.touch(now);
        Ok(self.generation)
    }

    fn check_generation(&self, generation: u64) -> Result<(), Rejection> {
        if generation != self.generation || self.active_run.is_none() {
            return Err(Rejection::Stale(format!(
                "generation {generation} is not the live run"
            )));
        }
        Ok(())
    }

    /// A queued turn goes to the provider in this generation.
    pub fn sending(
        &mut self,
        turn: &TurnId,
        generation: u64,
        now: Timestamp,
    ) -> Result<(), Rejection> {
        self.check_generation(generation)?;
        let run = self.active_run.clone();
        let t = self
            .turn_mut(turn)
            .ok_or_else(|| Rejection::NotFound(format!("no turn `{turn}`")))?;
        if t.state != TurnState::Queued {
            return Err(Rejection::Stale(format!("turn `{turn}` isn't queued")));
        }
        t.delivery = Delivery::Sending;
        t.state = TurnState::Running;
        t.run_id = run;
        t.generation = Some(generation);
        t.started_at = Some(now);
        self.touch(now);
        Ok(())
    }

    /// The provider has the turn's input.
    pub fn delivered(
        &mut self,
        turn: &TurnId,
        generation: u64,
        now: Timestamp,
    ) -> Result<(), Rejection> {
        self.check_generation(generation)?;
        let t = self
            .turn_mut(turn)
            .ok_or_else(|| Rejection::NotFound(format!("no turn `{turn}`")))?;
        if t.generation != Some(generation) {
            return Err(Rejection::Stale(format!("turn `{turn}` is another run's")));
        }
        t.delivery = Delivery::Delivered;
        self.touch(now);
        Ok(())
    }

    /// The provider settled a turn. A late settlement of an older
    /// generation changes nothing.
    pub fn finished(
        &mut self,
        turn: &TurnId,
        generation: u64,
        outcome: TurnOutcome,
        reason: Option<String>,
        usage: Option<Usage>,
        now: Timestamp,
    ) -> Result<(), Rejection> {
        self.check_generation(generation)?;
        let t = self
            .turn_mut(turn)
            .ok_or_else(|| Rejection::NotFound(format!("no turn `{turn}`")))?;
        if t.generation != Some(generation) || t.is_over() {
            return Err(Rejection::Stale(format!(
                "turn `{turn}` is over or another run's"
            )));
        }
        t.state = TurnState::Finished;
        t.outcome = Some(outcome);
        t.reason = reason;
        t.usage = usage;
        t.ended_at = Some(now);
        let turn = turn.clone();
        self.expire_interactions(|i| i.turn_id == turn, now);
        for m in self
            .messages
            .iter_mut()
            .filter(|m| m.turn_id == turn && m.lifecycle == MessageLifecycle::Streaming)
        {
            m.lifecycle = if outcome == TurnOutcome::Completed {
                MessageLifecycle::Completed
            } else {
                MessageLifecycle::Interrupted
            };
        }
        self.touch(now);
        Ok(())
    }

    /// The run is gone. What it was doing ends as `outcome` (interrupted,
    /// or unknown when the run was lost); its open interactions expire; a
    /// turn sent but never confirmed is `Delivery::Unknown`, never resent.
    pub fn run_ended(&mut self, generation: u64, outcome: TurnOutcome, now: Timestamp) {
        if generation != self.generation || self.active_run.is_none() {
            return;
        }
        self.active_run = None;
        for t in self
            .turns
            .iter_mut()
            .filter(|t| t.generation == Some(generation) && !t.is_over())
        {
            if t.delivery == Delivery::Sending {
                t.delivery = Delivery::Unknown;
            }
            t.state = TurnState::Finished;
            t.outcome = Some(outcome);
            t.ended_at = Some(now);
        }
        self.expire_interactions(|i| i.generation == generation, now);
        for m in self
            .messages
            .iter_mut()
            .filter(|m| m.lifecycle == MessageLifecycle::Streaming)
        {
            m.lifecycle = MessageLifecycle::Interrupted;
        }
        self.touch(now);
    }

    /// Turns still queued will never be delivered (their run ended): they
    /// end `cancelled`, with why. Returns how many.
    pub fn cancel_queued(&mut self, reason: &str, now: Timestamp) -> usize {
        let mut n = 0;
        for t in self
            .turns
            .iter_mut()
            .filter(|t| t.state == TurnState::Queued)
        {
            t.state = TurnState::Finished;
            t.outcome = Some(TurnOutcome::Cancelled);
            t.reason = Some(reason.into());
            t.ended_at = Some(now);
            n += 1;
        }
        if n > 0 {
            self.touch(now);
        }
        n
    }

    fn expire_interactions(&mut self, which: impl Fn(&Interaction) -> bool, now: Timestamp) {
        for i in self
            .interactions
            .iter_mut()
            .filter(|i| i.status.is_open() && which(i))
        {
            i.status = InteractionStatus::Expired;
            i.decided_at.get_or_insert(now);
        }
    }

    /// Stream text into a message (created on first use): `text` is the
    /// whole block so far, so a late or repeated piece can't duplicate it.
    pub fn write_block(
        &mut self,
        turn: &TurnId,
        message: &MessageId,
        block: &BlockId,
        text: &str,
        done: bool,
        now: Timestamp,
    ) {
        let m = match self.messages.iter().position(|m| &m.id == message) {
            Some(i) => &mut self.messages[i],
            None => {
                self.messages.push(Message {
                    id: message.clone(),
                    turn_id: turn.clone(),
                    role: MessageRole::Agent,
                    blocks: vec![],
                    revision: 0,
                    lifecycle: MessageLifecycle::Streaming,
                    at: now,
                });
                self.messages.last_mut().unwrap()
            }
        };
        // A finished message isn't reopened by a straggling piece.
        if m.lifecycle != MessageLifecycle::Streaming {
            return;
        }
        match m.blocks.iter_mut().find(|b| &b.id == block) {
            Some(b) => b.kind = BlockKind::Text { text: text.into() },
            None => m.blocks.push(Block {
                id: block.clone(),
                kind: BlockKind::Text { text: text.into() },
            }),
        }
        m.revision += 1;
        if done {
            m.lifecycle = MessageLifecycle::Completed;
        }
        self.touch(now);
    }

    /// Record a tool call starting (or asked for).
    pub fn tool_started(&mut self, record: ToolRecord, now: Timestamp) {
        if self.tools.iter().any(|t| t.id == record.id) {
            return;
        }
        self.tools.push(record);
        self.touch(now);
    }

    /// Record a tool call's end. Only a reported result is success or
    /// failure; `None` means it was never reported.
    pub fn tool_finished(
        &mut self,
        id: &ToolCallId,
        result: Option<(bool, ToolResult)>,
        now: Timestamp,
    ) {
        let Some(t) = self.tools.iter_mut().find(|t| &t.id == id) else {
            return;
        };
        if t.status.is_over() {
            return;
        }
        match result {
            Some((ok, r)) => {
                t.status = if ok {
                    ToolStatus::Succeeded
                } else {
                    ToolStatus::Failed
                };
                t.result = Some(r);
            }
            None => t.status = ToolStatus::ResultUnavailable,
        }
        t.finished_at = Some(now);
        self.touch(now);
    }

    /// Tools still open when their turn ended never reported a result.
    pub fn close_tools(&mut self, turn: &TurnId, now: Timestamp) {
        let mut changed = false;
        for t in self
            .tools
            .iter_mut()
            .filter(|t| &t.turn_id == turn && !t.status.is_over())
        {
            t.status = ToolStatus::ResultUnavailable;
            t.finished_at = Some(now);
            changed = true;
        }
        if changed {
            self.touch(now);
        }
    }

    /// The agent asks something in this generation.
    pub fn interaction_requested(
        &mut self,
        i: Interaction,
        now: Timestamp,
    ) -> Result<(), Rejection> {
        self.check_generation(i.generation)?;
        if let Some(t) = self.turn_mut(&i.turn_id)
            && t.state == TurnState::Running
        {
            t.state = TurnState::Waiting;
        }
        self.interactions.push(i);
        self.touch(now);
        Ok(())
    }

    /// A recorded response reached the provider.
    pub fn interaction_delivered(
        &mut self,
        id: &DecisionId,
        generation: u64,
        now: Timestamp,
    ) -> Result<(), Rejection> {
        self.check_generation(generation)?;
        let i = self
            .interactions
            .iter_mut()
            .find(|i| &i.id == id)
            .ok_or_else(|| Rejection::NotFound(format!("no interaction `{id}`")))?;
        if i.generation != generation {
            return Err(Rejection::Stale(format!(
                "interaction `{id}` is another run's"
            )));
        }
        if i.status == InteractionStatus::ResponseRecorded {
            i.status = InteractionStatus::Delivered;
        }
        let turn = i.turn_id.clone();
        let still_waiting = self
            .interactions
            .iter()
            .any(|i| i.turn_id == turn && i.status == InteractionStatus::Pending);
        if !still_waiting
            && let Some(t) = self.turn_mut(&turn)
            && t.state == TurnState::Waiting
        {
            t.state = TurnState::Running;
        }
        self.touch(now);
        Ok(())
    }
}

/// A change to a conversation, with everything it needs (ids, times) inside:
/// applying the same ops to the same start gives the same conversation. The
/// daemon journals these (D-058).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Accept {
        command_id: String,
        fingerprint: String,
        command: Command,
        /// The id a new turn gets.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        at: Timestamp,
    },
    Claim {
        run: RunId,
        at: Timestamp,
    },
    Sending {
        turn: TurnId,
        generation: u64,
        at: Timestamp,
    },
    Delivered {
        turn: TurnId,
        generation: u64,
        at: Timestamp,
    },
    Finished {
        turn: TurnId,
        generation: u64,
        outcome: TurnOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorCategory>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        at: Timestamp,
    },
    /// The run serving `generation` is gone.
    RunEnded {
        generation: u64,
        outcome: TurnOutcome,
        at: Timestamp,
    },
    CancelQueued {
        reason: String,
        at: Timestamp,
    },
    /// What a turn that was lost died of.
    TurnError {
        turn: TurnId,
        error: ErrorCategory,
        reason: String,
        at: Timestamp,
    },
    Bind {
        binding: NativeBinding,
        at: Timestamp,
    },
    WriteBlock {
        turn: TurnId,
        message: MessageId,
        block: BlockId,
        text: String,
        done: bool,
        at: Timestamp,
    },
    ToolStarted {
        record: ToolRecord,
        at: Timestamp,
    },
    ToolFinished {
        id: ToolCallId,
        /// `None`: it never reported a result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<(bool, ToolResult)>,
        at: Timestamp,
    },
    CloseTools {
        turn: TurnId,
        at: Timestamp,
    },
    InteractionRequested {
        interaction: Interaction,
        at: Timestamp,
    },
    InteractionDelivered {
        id: DecisionId,
        generation: u64,
        at: Timestamp,
    },
    /// A response was sent but may not have arrived.
    InteractionUndelivered {
        id: DecisionId,
        at: Timestamp,
    },
}

/// What applying an [`Op`] gave.
#[derive(Clone, Debug, PartialEq)]
pub enum Applied {
    Done,
    Accepted(Accepted),
    /// The generation a claim got.
    Claimed(u64),
    /// How many queued turns were cancelled.
    Cancelled(usize),
}

impl Conversation {
    /// Apply one change. A rejected op changes nothing.
    pub fn apply(&mut self, op: &Op) -> Result<Applied, Rejection> {
        Ok(match op.clone() {
            Op::Accept {
                command_id,
                fingerprint,
                command,
                turn_id,
                at,
            } => Applied::Accepted(self.accept_as(
                &command_id,
                &fingerprint,
                command,
                turn_id,
                at,
            )?),
            Op::Claim { run, at } => Applied::Claimed(self.claim(run, at)?),
            Op::Sending {
                turn,
                generation,
                at,
            } => {
                self.sending(&turn, generation, at)?;
                Applied::Done
            }
            Op::Delivered {
                turn,
                generation,
                at,
            } => {
                self.delivered(&turn, generation, at)?;
                Applied::Done
            }
            Op::Finished {
                turn,
                generation,
                outcome,
                reason,
                error,
                usage,
                at,
            } => {
                self.finished(&turn, generation, outcome, reason, usage, at)?;
                if let Some(t) = self.turn_mut(&turn) {
                    t.error = error;
                }
                Applied::Done
            }
            Op::RunEnded {
                generation,
                outcome,
                at,
            } => {
                self.run_ended(generation, outcome, at);
                Applied::Done
            }
            Op::CancelQueued { reason, at } => Applied::Cancelled(self.cancel_queued(&reason, at)),
            Op::TurnError {
                turn,
                error,
                reason,
                at,
            } => {
                let t = self
                    .turn_mut(&turn)
                    .ok_or_else(|| Rejection::NotFound(format!("no turn `{turn}`")))?;
                t.error = Some(error);
                t.reason = Some(reason);
                self.touch(at);
                Applied::Done
            }
            Op::Bind { binding, at } => {
                if self.binding.as_ref() != Some(&binding) {
                    self.binding = Some(binding);
                    self.touch(at);
                }
                Applied::Done
            }
            Op::WriteBlock {
                turn,
                message,
                block,
                text,
                done,
                at,
            } => {
                self.write_block(&turn, &message, &block, &text, done, at);
                Applied::Done
            }
            Op::ToolStarted { record, at } => {
                self.tool_started(record, at);
                Applied::Done
            }
            Op::ToolFinished { id, result, at } => {
                self.tool_finished(&id, result, at);
                Applied::Done
            }
            Op::CloseTools { turn, at } => {
                self.close_tools(&turn, at);
                Applied::Done
            }
            Op::InteractionRequested { interaction, at } => {
                self.interaction_requested(interaction, at)?;
                Applied::Done
            }
            Op::InteractionDelivered { id, generation, at } => {
                self.interaction_delivered(&id, generation, at)?;
                Applied::Done
            }
            Op::InteractionUndelivered { id, at } => {
                let i = self
                    .interactions
                    .iter_mut()
                    .find(|i| i.id == id)
                    .ok_or_else(|| Rejection::NotFound(format!("no interaction `{id}`")))?;
                i.status = InteractionStatus::DeliveryUnknown;
                self.touch(at);
                Applied::Done
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        chrono::Utc::now()
    }

    fn conv() -> Conversation {
        Conversation::new("claude", "legacy_cli", WorkspaceId::from("ws_1"), now())
    }

    fn text(s: &str) -> Vec<InputBlock> {
        vec![InputBlock::Text { text: s.into() }]
    }

    fn send(c: &mut Conversation, id: &str, s: &str) -> Result<Accepted, Rejection> {
        c.accept(
            id,
            s,
            Command::SendTurn {
                initiator: Initiator::Controller,
                input: text(s),
            },
            now(),
        )
    }

    fn turn_of(a: &Accepted) -> TurnId {
        a.receipt().turn_id.clone().unwrap()
    }

    #[test]
    fn a_command_id_is_applied_once_and_never_reused_for_something_else() {
        let mut c = conv();
        let a = send(&mut c, "cmd_1", "fix it").unwrap();
        assert!(matches!(a, Accepted::New(_)));
        let again = send(&mut c, "cmd_1", "fix it").unwrap();
        assert_eq!(again, Accepted::Repeat(a.receipt().clone()));
        assert_eq!(c.turns.len(), 1);
        assert!(matches!(
            send(&mut c, "cmd_1", "something else"),
            Err(Rejection::Conflict(_))
        ));
        assert!(!a.receipt().durable, "no journal yet");
    }

    #[test]
    fn turns_queue_one_by_one_and_are_never_merged() {
        let mut c = conv();
        let t1 = turn_of(&send(&mut c, "cmd_1", "first").unwrap());
        let t2 = turn_of(&send(&mut c, "cmd_2", "second").unwrap());
        let g = c.claim(RunId::from("run_a"), now()).unwrap();
        assert_eq!(c.next_queued().unwrap().id, t1);
        c.sending(&t1, g, now()).unwrap();
        // While one runs, nothing else is delivered.
        assert!(c.next_queued().is_none());
        c.delivered(&t1, g, now()).unwrap();
        c.finished(&t1, g, TurnOutcome::Completed, None, None, now())
            .unwrap();
        let next = c.next_queued().unwrap();
        assert_eq!(next.id, t2);
        assert_eq!(next.text(), "second", "its own input, not joined");
    }

    #[test]
    fn stale_generations_and_targets_cant_touch_a_newer_run() {
        let mut c = conv();
        let t1 = turn_of(&send(&mut c, "cmd_1", "first").unwrap());
        let g1 = c.claim(RunId::from("run_a"), now()).unwrap();
        // Two drivers: no.
        assert!(c.claim(RunId::from("run_b"), now()).is_err());
        c.sending(&t1, g1, now()).unwrap();
        c.run_ended(g1, TurnOutcome::OutcomeUnknown, now());
        // Sent but never confirmed: unknown, not queued again.
        let t = c.turn(&t1).unwrap();
        assert_eq!(t.delivery, Delivery::Unknown);
        assert_eq!(t.outcome, Some(TurnOutcome::OutcomeUnknown));
        assert!(c.next_queued().is_none());

        // A restart: same conversation, new run, new generation.
        let g2 = c.claim(RunId::from("run_b"), now()).unwrap();
        assert_eq!(g2, g1 + 1);
        let t2 = turn_of(&send(&mut c, "cmd_2", "continue").unwrap());
        c.sending(&t2, g2, now()).unwrap();
        // The old run's late events change nothing.
        assert!(matches!(
            c.finished(&t2, g1, TurnOutcome::Failed, None, None, now()),
            Err(Rejection::Stale(_))
        ));
        assert!(c.delivered(&t2, g1, now()).is_err());
        c.run_ended(g1, TurnOutcome::Failed, now());
        assert_eq!(c.turn(&t2).unwrap().state, TurnState::Running);
        // A late interrupt of a finished turn is stale, not applied to the next.
        assert!(matches!(
            c.accept("cmd_3", "x", Command::Interrupt { turn_id: t1 }, now()),
            Err(Rejection::Stale(_))
        ));
        assert_eq!(c.turn(&t2).unwrap().state, TurnState::Running);
    }

    #[test]
    fn interrupt_awaits_settlement_and_a_queued_turn_just_never_starts() {
        let mut c = conv();
        let t1 = turn_of(&send(&mut c, "cmd_1", "a").unwrap());
        let t2 = turn_of(&send(&mut c, "cmd_2", "b").unwrap());
        let g = c.claim(RunId::from("run_a"), now()).unwrap();
        c.sending(&t1, g, now()).unwrap();
        c.accept(
            "cmd_3",
            "i1",
            Command::Interrupt {
                turn_id: t1.clone(),
            },
            now(),
        )
        .unwrap();
        assert_eq!(c.turn(&t1).unwrap().state, TurnState::Interrupting);
        assert!(c.next_queued().is_none(), "not until it settles");
        c.finished(&t1, g, TurnOutcome::Interrupted, None, None, now())
            .unwrap();
        c.accept(
            "cmd_4",
            "i2",
            Command::Interrupt {
                turn_id: t2.clone(),
            },
            now(),
        )
        .unwrap();
        let t = c.turn(&t2).unwrap();
        assert_eq!(t.outcome, Some(TurnOutcome::Cancelled));
        assert_eq!(t.delivery, Delivery::Queued, "never sent");
    }

    fn questions() -> InteractionKind {
        InteractionKind::Questions {
            questions: vec![
                Question {
                    id: "q1".into(),
                    header: Some("Format".into()),
                    prompt: "Which format?".into(),
                    options: vec![
                        QuestionOption {
                            label: "CSV".into(),
                            description: None,
                        },
                        QuestionOption {
                            label: "JSON".into(),
                            description: Some("one object per line".into()),
                        },
                    ],
                    multi_select: false,
                },
                Question {
                    id: "q2".into(),
                    header: None,
                    prompt: "Which columns?".into(),
                    options: vec![],
                    multi_select: true,
                },
            ],
        }
    }

    fn ask(c: &mut Conversation, turn: &TurnId, g: u64) -> DecisionId {
        let id = DecisionId::generate();
        c.interaction_requested(
            Interaction {
                id: id.clone(),
                turn_id: turn.clone(),
                run_id: c.active_run.clone().unwrap(),
                generation: g,
                tool_call: None,
                input_hash: Some("h".into()),
                kind: questions(),
                status: InteractionStatus::Pending,
                response: None,
                decided_by: None,
                requested_at: now(),
                decided_at: None,
            },
            now(),
        )
        .unwrap();
        id
    }

    #[test]
    fn every_question_needs_its_own_answer_and_only_the_live_run_can_be_answered() {
        let mut c = conv();
        let t = turn_of(&send(&mut c, "cmd_1", "a").unwrap());
        let g = c.claim(RunId::from("run_a"), now()).unwrap();
        c.sending(&t, g, now()).unwrap();
        let id = ask(&mut c, &t, g);
        assert_eq!(c.turn(&t).unwrap().state, TurnState::Waiting);
        let resolve = |answers: BTreeMap<String, Answer>, generation| Command::Resolve {
            interaction_id: id.clone(),
            generation,
            response: InteractionResponse::Answers { answers },
            by: Decider::User,
        };
        let one = BTreeMap::from([(
            "q1".to_owned(),
            Answer {
                selected: vec!["CSV".into()],
                text: None,
            },
        )]);
        assert!(matches!(
            c.accept("cmd_2", "a1", resolve(one.clone(), g), now()),
            Err(Rejection::Invalid(_))
        ));
        let mut both = one;
        both.insert(
            "q2".into(),
            Answer {
                selected: vec!["name".into(), "time".into()],
                text: Some("and the host".into()),
            },
        );
        assert!(matches!(
            c.accept("cmd_3", "a2", resolve(both.clone(), g + 1), now()),
            Err(Rejection::Stale(_))
        ));
        c.accept("cmd_4", "a3", resolve(both.clone(), g), now())
            .unwrap();
        // A second, different answer: too late.
        assert!(matches!(
            c.accept("cmd_5", "a4", resolve(both, g), now()),
            Err(Rejection::Stale(_))
        ));
        c.interaction_delivered(&id, g, now()).unwrap();
        assert_eq!(c.turn(&t).unwrap().state, TurnState::Running);
    }

    #[test]
    fn a_lost_run_expires_its_questions_and_cuts_off_its_text() {
        let mut c = conv();
        let t = turn_of(&send(&mut c, "cmd_1", "a").unwrap());
        let g = c.claim(RunId::from("run_a"), now()).unwrap();
        c.sending(&t, g, now()).unwrap();
        c.delivered(&t, g, now()).unwrap();
        let m = MessageId::generate();
        let b = BlockId::generate();
        c.write_block(&t, &m, &b, "Hel", false, now());
        c.write_block(&t, &m, &b, "Hello", false, now());
        let id = ask(&mut c, &t, g);
        c.run_ended(g, TurnOutcome::Interrupted, now());
        assert_eq!(
            c.interactions.iter().find(|i| i.id == id).unwrap().status,
            InteractionStatus::Expired
        );
        let msg = &c.messages[0];
        assert_eq!(msg.text(), "Hello", "the whole block, once");
        assert_eq!(msg.lifecycle, MessageLifecycle::Interrupted);
        assert_eq!(c.turn(&t).unwrap().delivery, Delivery::Delivered);
    }

    #[test]
    fn a_finished_message_replaces_its_stream_and_isnt_reopened() {
        let mut c = conv();
        let t = TurnId::generate();
        let m = MessageId::generate();
        let b = BlockId::generate();
        c.write_block(&t, &m, &b, "Do", false, now());
        c.write_block(&t, &m, &b, "Done.", true, now());
        c.write_block(&t, &m, &b, "Done. And", false, now());
        assert_eq!(c.messages.len(), 1);
        assert_eq!(c.messages[0].text(), "Done.");
        assert_eq!(c.messages[0].lifecycle, MessageLifecycle::Completed);
    }

    #[test]
    fn a_tool_without_a_result_is_never_a_success() {
        let mut c = conv();
        let t = TurnId::generate();
        let record = |name: &str| ToolRecord {
            id: ToolCallId::generate(),
            turn_id: t.clone(),
            run_id: RunId::from("run_a"),
            name: name.into(),
            effect: ToolEffect::Command,
            summary: "npm test".into(),
            parent: None,
            status: ToolStatus::Running,
            started_at: now(),
            finished_at: None,
            result: None,
        };
        let (a, b, d) = (record("Bash"), record("Bash"), record("Bash"));
        let ids = (a.id.clone(), b.id.clone(), d.id.clone());
        c.tool_started(a, now());
        c.tool_started(b, now());
        c.tool_started(d, now());
        c.tool_finished(
            &ids.0,
            Some((
                false,
                ToolResult {
                    summary: "1 failed".into(),
                    artifact: None,
                    truncated: false,
                },
            )),
            now(),
        );
        c.tool_finished(&ids.1, None, now());
        c.close_tools(&t, now());
        let status = |id: &ToolCallId| c.tools.iter().find(|x| &x.id == id).unwrap().status;
        assert_eq!(status(&ids.0), ToolStatus::Failed);
        assert_eq!(status(&ids.1), ToolStatus::ResultUnavailable);
        assert_eq!(status(&ids.2), ToolStatus::ResultUnavailable);
    }

    /// The same ops on the same start give the same conversation, ids and all.
    #[test]
    fn ops_replay_to_the_same_conversation() {
        let start = conv();
        let t = TurnId::generate();
        let (m, b) = (MessageId::generate(), BlockId::generate());
        let at = now();
        let ops = vec![
            Op::Accept {
                command_id: "cmd_1".into(),
                fingerprint: "a".into(),
                command: Command::SendTurn {
                    initiator: Initiator::Controller,
                    input: text("a"),
                },
                turn_id: Some(t.clone()),
                at,
            },
            Op::Claim {
                run: RunId::from("run_a"),
                at,
            },
            Op::Sending {
                turn: t.clone(),
                generation: 1,
                at,
            },
            Op::Bind {
                binding: NativeBinding {
                    session_id: "s-1".into(),
                },
                at,
            },
            Op::Delivered {
                turn: t.clone(),
                generation: 1,
                at,
            },
            Op::WriteBlock {
                turn: t.clone(),
                message: m,
                block: b,
                text: "Done.".into(),
                done: true,
                at,
            },
            Op::Finished {
                turn: t.clone(),
                generation: 1,
                outcome: TurnOutcome::Completed,
                reason: None,
                error: None,
                usage: None,
                at,
            },
        ];
        let mut one = start.clone();
        let mut two = start;
        for op in &ops {
            one.apply(op).unwrap();
            // Through JSON, as a journal would.
            let back: Op = serde_json::from_str(&serde_json::to_string(op).unwrap()).unwrap();
            two.apply(&back).unwrap();
        }
        assert_eq!(one, two);
        assert_eq!(one.turns[0].id, t);
        assert_eq!(one.turns[0].outcome, Some(TurnOutcome::Completed));
        // A rejected op changes nothing.
        let before = one.clone();
        assert!(
            one.apply(&Op::Sending {
                turn: t,
                generation: 9,
                at,
            })
            .is_err()
        );
        assert_eq!(one, before);
    }

    /// The persisted shapes: every outcome, tool status and interaction kind
    /// survives a round trip, and missing optional fields read as absent.
    #[test]
    fn records_round_trip() {
        for o in [
            TurnOutcome::Completed,
            TurnOutcome::Interrupted,
            TurnOutcome::Cancelled,
            TurnOutcome::Failed,
            TurnOutcome::LimitReached,
            TurnOutcome::OutcomeUnknown,
        ] {
            let s = serde_json::to_string(&o).unwrap();
            assert_eq!(serde_json::from_str::<TurnOutcome>(&s).unwrap(), o);
        }
        assert_eq!(
            serde_json::to_string(&TurnOutcome::OutcomeUnknown).unwrap(),
            "\"outcome_unknown\""
        );
        for s in [
            ToolStatus::Pending,
            ToolStatus::Running,
            ToolStatus::Succeeded,
            ToolStatus::Failed,
            ToolStatus::Denied,
            ToolStatus::ResultUnavailable,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<ToolStatus>(&j).unwrap(), s);
        }

        let mut c = conv();
        let t = turn_of(&send(&mut c, "cmd_1", "a").unwrap());
        let g = c.claim(RunId::from("run_a"), now()).unwrap();
        c.sending(&t, g, now()).unwrap();
        ask(&mut c, &t, g);
        c.write_block(
            &t,
            &MessageId::generate(),
            &BlockId::generate(),
            "hi",
            true,
            now(),
        );
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["interactions"][0]["type"], "questions");
        assert_eq!(
            json["interactions"][0]["questions"][1]["multi_select"],
            true
        );
        assert_eq!(json["messages"][0]["blocks"][0]["type"], "text");
        let back: Conversation = serde_json::from_value(json).unwrap();
        assert_eq!(back, c);

        // A minimal record (an older or sparser writer) reads with defaults.
        let minimal = serde_json::json!({
            "id": "conv_1", "provider": "claude", "backend": "legacy_cli",
            "workspace_id": "ws_1", "revision": 0, "generation": 0,
            "created_at": "2026-10-09T00:00:00Z", "updated_at": "2026-10-09T00:00:00Z"
        });
        let c: Conversation = serde_json::from_value(minimal).unwrap();
        assert_eq!(c.schema, CONVERSATION_SCHEMA);
        assert_eq!(c.lifecycle, Lifecycle::Open);
        assert!(c.binding.is_none() && c.turns.is_empty());
    }
}
