//! Features: product work — what should be built — as opposed to a
//! [`crate::Workspace`], which is where work runs (D-042, D-043).
//!
//! A feature is owned by one host's daemon. It links to workspaces and
//! sessions on that host by id and never embeds them, and no workspace knows
//! about features. Three things are kept apart:
//!
//! - **messages** — the conversation between the developer, the Control
//!   Agent and the coding agent (what was *said*);
//! - **commands** — what a client *asks* the daemon to do
//!   ([`FeatureAction`], carrying a client-chosen id so a repeated delivery
//!   is applied once);
//! - **events** — what *happened*, appended to the feature's own ordered
//!   history ([`FeatureEventRecord`]).

use serde::{Deserialize, Serialize};

use crate::ids::{
    ConversationId, DecisionId, EvidenceId, FeatureId, MessageId, RunId, SessionId, TaskId, TurnId,
    WorkspaceId,
};
use crate::model::{Activity, Timestamp};

/// Version of the persisted [`Feature`] document. Bump with a migration in
/// the daemon's feature store.
pub const FEATURE_SCHEMA: u32 = 1;
/// Version of a [`FeatureEventRecord`] line.
pub const FEATURE_EVENT_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FeatureStatus {
    /// Written down, not started.
    Draft,
    /// The Control Agent is writing requirements, acceptance criteria and tasks.
    Planning,
    /// A coding agent works on the tasks.
    Implementing,
    /// Checking the result against the acceptance criteria.
    Verifying,
    /// Verified; waiting for the developer (and the delivery gates).
    Review,
    Done,
    /// Waiting on the developer: a decision, or something only they can fix.
    Blocked,
    /// Stopped by the developer; resumable.
    Paused,
    /// Gave up (limits reached, or an unrecoverable error); retryable.
    Failed,
    Cancelled,
}

impl FeatureStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FeatureStatus::Draft => "draft",
            FeatureStatus::Planning => "planning",
            FeatureStatus::Implementing => "implementing",
            FeatureStatus::Verifying => "verifying",
            FeatureStatus::Review => "review",
            FeatureStatus::Done => "done",
            FeatureStatus::Blocked => "blocked",
            FeatureStatus::Paused => "paused",
            FeatureStatus::Failed => "failed",
            FeatureStatus::Cancelled => "cancelled",
        }
    }

    /// Nothing more will happen. (Done isn't final: the developer can
    /// always change the goal and the work picks up again, D-050.)
    pub fn is_terminal(self) -> bool {
        matches!(self, FeatureStatus::Cancelled)
    }

    /// The controller is (or should be) driving it.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            FeatureStatus::Planning | FeatureStatus::Implementing | FeatureStatus::Verifying
        )
    }

    /// The lifecycle (D-050) is a loop, not a line: plan → implement →
    /// verify, as many times as it takes, then review; the developer can
    /// change the goal at any point (even after done), which plans again
    /// from where the work stands. Blocked, paused and failed are side exits;
    /// only cancelled is final.
    pub fn can_become(self, to: FeatureStatus) -> bool {
        use FeatureStatus::*;
        if self == to || self.is_terminal() {
            return false;
        }
        match to {
            Draft => false,
            // Planning again is always possible: the goal moved.
            Planning => true,
            Implementing => matches!(
                self,
                Planning | Verifying | Review | Paused | Failed | Blocked
            ),
            Verifying => matches!(self, Implementing | Paused | Blocked | Failed),
            Review => matches!(self, Verifying | Paused | Blocked),
            Done => matches!(self, Review),
            Blocked => self.is_active() || self == Review,
            Paused => self.is_active() || self == Blocked || self == Review,
            Failed => self.is_active() || self == Blocked || self == Review,
            Cancelled => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Blocked,
    Done,
    Failed,
    Skipped,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    Running,
    /// Waiting on a decision.
    Waiting,
    Completed,
    Failed,
    Cancelled,
    /// A human took over the session (D-044).
    HandedOff,
}

impl RunState {
    pub fn is_over(self) -> bool {
        matches!(
            self,
            RunState::Completed | RunState::Failed | RunState::Cancelled | RunState::HandedOff
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Controller,
    Agent,
    System,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// A tool the coding agent wants to run.
    ToolPermission,
    /// A question the coding agent asks.
    Question,
    /// A plan waiting to be accepted.
    PlanApproval,
    /// Merging a change (an irreversible external action).
    Merge,
    /// Deploying (an irreversible external action).
    Deploy,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Pending,
    Approved,
    Denied,
    Answered,
    /// The run it belonged to ended first.
    Cancelled,
}

/// Who made a decision.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decider {
    /// A deterministic rule (D-044), not a model.
    Policy,
    /// The Control Agent (a model), within what policy allows it.
    Controller,
    User,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Test,
    Browser,
    Screenshot,
    Ci,
    Review,
    Diff,
    PullRequest,
    Log,
    Note,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Criterion {
    pub id: String,
    pub text: String,
    /// Unset until verification has judged it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub met: Option<bool>,
    #[serde(default)]
    pub evidence: Vec<EvidenceId>,
    /// What proves it (D-062).
    #[serde(default)]
    pub how: Proof,
}

/// What proves an acceptance criterion (D-062).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Proof {
    /// The check command passing: decided in code, from its result.
    Check,
    /// What the change contains: judged from the working tree's diff.
    #[default]
    Diff,
}

/// What the last verification saw (D-062): the state of the work and the
/// criteria it left unmet. The same again after a fix means the fix
/// changed nothing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VerificationMark {
    pub fingerprint: String,
    /// The unmet criteria, by text.
    pub unmet: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub status: TaskStatus,
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The coding agent's planning turn (D-064), not a step of the work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub planning: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub id: RunId,
    pub task_id: TaskId,
    /// The agent runtime, e.g. `claude`.
    pub runtime: String,
    pub state: RunState,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The runtime's own conversation id, to resume. Opaque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    /// What the agent did lately (tools it used), newest last, a line each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity: Vec<String>,
    /// The runtime conversation this run served (D-055), and the turn it
    /// started with. Absent on runs from before conversations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ConversationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// How long the agent has worked on this run, in ms, up to
    /// `working_since`: waits on a decision don't count (D-063).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub worked_ms: u64,
    /// When the current stretch of work began; absent while the run waits on
    /// a decision, and once it is over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_since: Option<Timestamp>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Run {
    /// How long the agent has worked on this run as of `now` (D-063).
    pub fn worked(&self, now: Timestamp) -> chrono::Duration {
        let banked = chrono::Duration::milliseconds(self.worked_ms as i64);
        match self.working_since {
            Some(since) if now > since => banked + (now - since),
            _ => banked,
        }
    }

    /// The agent works (again): its clock runs.
    pub fn start_clock(&mut self, now: Timestamp) {
        if self.working_since.is_none() {
            self.working_since = Some(now);
        }
    }

    /// The agent waits, or is done: what it worked so far is banked.
    pub fn stop_clock(&mut self, now: Timestamp) {
        if let Some(since) = self.working_since.take()
            && now > since
        {
            self.worked_ms += (now - since).num_milliseconds() as u64;
        }
    }

    /// Time that wasn't work (the host asleep) doesn't count: the current
    /// stretch starts `gap` later, never after `now`.
    pub fn skip(&mut self, gap: chrono::Duration, now: Timestamp) {
        if let Some(since) = &mut self.working_since {
            *since = (*since + gap).min(now);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub role: MessageRole,
    pub text: String,
    pub at: Timestamp,
    /// The command or decision this message belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DecisionRequest {
    pub id: DecisionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub kind: DecisionKind,
    pub risk: Risk,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Choices for a question; empty for approve/deny.
    #[serde(default)]
    pub options: Vec<String>,
    pub status: DecisionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<Decider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<Timestamp>,
    /// Only the developer may decide this one (policy said so; D-044).
    #[serde(default)]
    pub user_only: bool,
    /// What the developer's approval lets through for the rest of the
    /// feature, when policy says it may (D-066): opaque to all but policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub id: EvidenceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criterion_id: Option<String>,
    pub kind: EvidenceKind,
    pub title: String,
    /// Passed / failed; unset for informational evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// Where to see it: a URL, or a path on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A judgment (e.g. a model's visual review), not a deterministic check.
    #[serde(default)]
    pub uncertain: bool,
    pub at: Timestamp,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Budget {
    /// Implementation attempts (coding-agent runs) before giving up.
    pub max_iterations: u32,
    /// Wall-clock minutes of active work before giving up.
    pub max_minutes: u32,
    #[serde(default)]
    pub iterations_used: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_iterations: 6,
            max_minutes: 120,
            iterations_used: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Feature {
    #[serde(default = "schema_one")]
    pub schema: u32,
    pub id: FeatureId,
    pub title: String,
    /// What the developer asked for, as written.
    #[serde(default)]
    pub request: String,
    pub status: FeatureStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// When it took its status (absent in documents from before D-071).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_since: Option<Timestamp>,
    #[serde(default)]
    pub requirements: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<Criterion>,
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub runs: Vec<Run>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub decisions: Vec<DecisionRequest>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub budget: Budget,
    /// The workspace the work happens in, once chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// When work last (re)started: the time budget counts from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_since: Option<Timestamp>,
    /// The command that checks the work (e.g. `cargo test`), from the plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_command: Option<String>,
    /// The goal changed since the plan was written: plan again, keeping
    /// what is done (D-050).
    #[serde(default)]
    pub replan: bool,
    /// Policy (or the developer) allowed `verify_command` to run.
    #[serde(default)]
    pub verify_approved: bool,
    /// The decision asking to allow `verify_command`, or what to do when it
    /// can't run here, if one was needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_decision: Option<DecisionId>,
    /// The last verification that found something unmet (D-062).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verification: Option<VerificationMark>,
    /// What the developer allowed once and isn't asked about again in this
    /// feature (D-066): `DecisionRequest::grant`s they approved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<String>,
    /// Why the Control Agent did what it did, newest last (short lines).
    #[serde(default)]
    pub rationale: Vec<String>,
    /// Commands someone approved for this feature's checks (previews).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_commands: Vec<String>,
    /// The developer's preview of the app, if running (D-046).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<PreviewLink>,
    /// What must hold before the feature is done (D-047), as last evaluated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<Gate>,
    /// The change as published: branch, pull request, CI (D-047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
    /// The final report: what changed, decisions, evidence, risks, the PR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The last entry of this feature's history.
    #[serde(default)]
    pub history_seq: u64,
}

fn schema_one() -> u32 {
    1
}

impl Feature {
    pub fn new(title: String, request: String, now: Timestamp) -> Feature {
        Feature {
            schema: FEATURE_SCHEMA,
            id: FeatureId::generate(),
            title,
            request,
            status: FeatureStatus::Draft,
            status_since: Some(now),
            status_reason: None,
            requirements: Vec::new(),
            acceptance: Vec::new(),
            tasks: Vec::new(),
            runs: Vec::new(),
            messages: Vec::new(),
            decisions: Vec::new(),
            evidence: Vec::new(),
            budget: Budget::default(),
            workspace_id: None,
            active_since: None,
            verify_command: None,
            replan: false,
            verify_approved: false,
            verify_decision: None,
            last_verification: None,
            grants: vec![],
            rationale: Vec::new(),
            allowed_commands: Vec::new(),
            preview: None,
            gates: Vec::new(),
            delivery: None,
            report: None,
            created_at: now,
            updated_at: now,
            history_seq: 0,
        }
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| &t.id == id)
    }

    pub fn task_mut(&mut self, id: &TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| &t.id == id)
    }

    pub fn run_mut(&mut self, id: &RunId) -> Option<&mut Run> {
        self.runs.iter_mut().find(|r| &r.id == id)
    }

    pub fn decision_mut(&mut self, id: &DecisionId) -> Option<&mut DecisionRequest> {
        self.decisions.iter_mut().find(|d| &d.id == id)
    }

    pub fn pending_decisions(&self) -> impl Iterator<Item = &DecisionRequest> {
        self.decisions
            .iter()
            .filter(|d| d.status == DecisionStatus::Pending)
    }

    /// What the feature waits on the developer for, if anything (D-071).
    /// A decision comes first (the oldest); then review once it's ready to
    /// accept (or a gate failed), blocked, failed. Review still publishing
    /// or waiting for CI isn't waiting on them.
    pub fn needs(&self) -> Option<Need> {
        let since = self.status_since.unwrap_or(self.updated_at);
        let reason = |fallback: &str| {
            self.status_reason
                .clone()
                .unwrap_or_else(|| fallback.to_owned())
        };
        let mut pending: Vec<&DecisionRequest> = self.pending_decisions().collect();
        pending.sort_by_key(|d| d.created_at);
        if let Some(d) = pending.first() {
            return Some(Need {
                kind: if d.kind == DecisionKind::Question {
                    NeedKind::Question
                } else {
                    NeedKind::Decision
                },
                summary: d.summary.clone(),
                since: d.created_at,
                decision_id: Some(d.id.clone()),
                decisions: pending.len(),
            });
        }
        let need = |kind, summary| {
            Some(Need {
                kind,
                summary,
                since,
                decision_id: None,
                decisions: 0,
            })
        };
        match self.status {
            FeatureStatus::Review
                if self.report.is_some()
                    || self.gates.iter().any(|g| g.status == GateStatus::Failed) =>
            {
                need(NeedKind::Review, reason("Ready for your review"))
            }
            FeatureStatus::Blocked => need(NeedKind::Blocked, reason("Waiting on you")),
            FeatureStatus::Failed => need(NeedKind::Failed, reason("Stopped")),
            _ => None,
        }
    }

    /// Where it stands, on the workspaces' scale (D-071).
    pub fn activity(&self) -> Activity {
        match (self.needs(), self.status) {
            (Some(n), _) if n.kind == NeedKind::Failed => Activity::Failed,
            (Some(_), _) => Activity::NeedsYou,
            (None, FeatureStatus::Done) => Activity::Completed,
            (None, FeatureStatus::Cancelled) => Activity::Archived,
            (None, FeatureStatus::Draft | FeatureStatus::Paused) => Activity::Idle,
            (None, _) => Activity::Working,
        }
    }

    /// The message the request came in (D-067): the developer's first, when
    /// its text is the request. What else they said is a note, an answer.
    pub fn request_message(&self) -> Option<&MessageId> {
        let first = self.messages.iter().find(|m| m.role == MessageRole::User)?;
        (!self.request.trim().is_empty() && first.text.trim() == self.request.trim())
            .then_some(&first.id)
    }

    /// The run currently working, if any.
    pub fn live_run(&self) -> Option<&Run> {
        self.runs.iter().rev().find(|r| !r.state.is_over())
    }

    /// Runs that waited on decisions which are all made now work again: their
    /// state and clock run (D-063). The events to record.
    pub fn resume_waiting_runs(&mut self, now: Timestamp) -> Vec<FeatureEvent> {
        let pending: Vec<RunId> = self
            .pending_decisions()
            .filter_map(|d| d.run_id.clone())
            .collect();
        let mut events = Vec::new();
        for r in &mut self.runs {
            if r.state == RunState::Waiting && !pending.contains(&r.id) {
                r.state = RunState::Running;
                r.start_clock(now);
                events.push(FeatureEvent::RunChanged {
                    run_id: r.id.clone(),
                    state: RunState::Running,
                });
            }
        }
        events
    }

    /// Change status if the lifecycle allows it; the event to record.
    pub fn transition(
        &mut self,
        to: FeatureStatus,
        reason: Option<String>,
        now: Timestamp,
    ) -> Result<FeatureEvent, String> {
        let from = self.status;
        if !from.can_become(to) {
            return Err(format!(
                "a {} feature can't become {}",
                from.as_str(),
                to.as_str()
            ));
        }
        self.status = to;
        self.status_reason = reason.clone();
        self.status_since = Some(now);
        self.updated_at = now;
        if to.is_active() && !from.is_active() {
            self.active_since = Some(now);
        }
        Ok(FeatureEvent::StatusChanged { from, to, reason })
    }
}

/// What a feature waits on the developer for (D-071): one rule, here, for
/// the app, its notifications and `otter ls`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Need {
    pub kind: NeedKind,
    /// What it is, in a line: the decision's summary, the status reason.
    pub summary: String,
    /// Waiting since.
    pub since: Timestamp,
    /// The decision to answer, when it's one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<DecisionId>,
    /// Decisions waiting, this one included.
    pub decisions: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NeedKind {
    /// To allow or deny.
    Decision,
    /// To answer.
    Question,
    /// Ready for the developer to accept, or not.
    Review,
    /// Waiting on something only the developer can do (no decision).
    Blocked,
    /// Stopped: retry, change the goal, or cancel.
    Failed,
}

/// What a client tells a feature to do (`feature.act`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FeatureAction {
    /// Draft → planning: hand it to the Control Agent.
    Start,
    Pause,
    Resume,
    Cancel,
    /// A failed feature: try again with a fresh budget of attempts.
    Retry,
    /// Answer a decision: approve/deny, or answer a question.
    Decide {
        decision_id: DecisionId,
        approve: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        /// For a question form: an answer per question, by question id
        /// (multi-select comma-joined). Takes the place of `answer`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answers: Option<std::collections::BTreeMap<String, String>>,
    },
    /// The developer signs off a feature in review. Refused while a gate
    /// fails or waits, unless `override_gates` (recorded).
    Accept {
        #[serde(default)]
        override_gates: bool,
    },
    /// The developer changes or adds to the goal (at any point, even after
    /// done): the Control Agent plans again from where the work stands.
    RequestChanges {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Choose the workspace (id or name, on this host) the work happens in.
    SetWorkspace {
        workspace: String,
    },
    /// Start (or restart) the app's preview for the developer to look at.
    Preview,
    /// Stop what the coding agent is doing now (its current turn only) and
    /// wait: the feature pauses (D-059).
    Interrupt,
    /// Guidance that can't wait: the coding agent's current turn stops and
    /// this goes next, in the same run (D-059).
    Redirect {
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Passed,
    Failed,
    /// Not decided yet (CI running, waiting for approval, …).
    Pending,
    /// Doesn't apply to this feature (no browser checks, no CI, …).
    NotApplicable,
}

impl GateStatus {
    /// Doesn't stand in the way of done.
    pub fn clear(self) -> bool {
        matches!(self, GateStatus::Passed | GateStatus::NotApplicable)
    }
}

/// One condition for completion (D-047).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Gate {
    pub name: String,
    pub status: GateStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One CI check on the pull request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CiCheck {
    pub name: String,
    /// `pass`, `fail`, `pending`, `skipping` or `cancel`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where the change was published.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Delivery {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The commit last pushed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// When it was pushed: CI results right after are the previous
    /// commit's, so they're read only once things settle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default)]
    pub ci: Vec<CiCheck>,
    /// Reruns of failed CI judged flaky, for the current head.
    #[serde(default)]
    pub reruns: u32,
    /// The developer declined publishing: delivery stays local.
    #[serde(default)]
    pub declined: bool,
}

/// A preview of the app running in the feature's workspace (D-046): a
/// service session listening on `port` on the host. A client reaches it by
/// forwarding that one port.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PreviewLink {
    pub session_id: SessionId,
    pub port: u16,
    /// The path to open (e.g. `/`).
    pub path: String,
}

impl FeatureAction {
    pub fn name(&self) -> &'static str {
        match self {
            FeatureAction::Start => "start",
            FeatureAction::Pause => "pause",
            FeatureAction::Resume => "resume",
            FeatureAction::Cancel => "cancel",
            FeatureAction::Retry => "retry",
            FeatureAction::Decide { .. } => "decide",
            FeatureAction::Accept { .. } => "accept",
            FeatureAction::RequestChanges { .. } => "request_changes",
            FeatureAction::SetWorkspace { .. } => "set_workspace",
            FeatureAction::Preview => "preview",
            FeatureAction::Interrupt => "interrupt",
            FeatureAction::Redirect { .. } => "redirect",
        }
    }
}

/// Something that happened to a feature. Carries ids and short labels; the
/// content (messages, evidence details) is in the feature itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum FeatureEvent {
    FeatureCreated {
        title: String,
    },
    StatusChanged {
        from: FeatureStatus,
        to: FeatureStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    MessageAdded {
        message_id: MessageId,
        role: MessageRole,
    },
    /// A client's command was accepted (and applied once).
    CommandAccepted {
        command_id: String,
        action: String,
    },
    PlanSet {
        tasks: usize,
        criteria: usize,
    },
    TaskChanged {
        task_id: TaskId,
        status: TaskStatus,
    },
    RunStarted {
        run_id: RunId,
        task_id: TaskId,
        runtime: String,
    },
    RunChanged {
        run_id: RunId,
        state: RunState,
    },
    DecisionRequested {
        decision_id: DecisionId,
        kind: DecisionKind,
        risk: Risk,
    },
    DecisionResolved {
        decision_id: DecisionId,
        status: DecisionStatus,
        by: Decider,
    },
    EvidenceAdded {
        evidence_id: EvidenceId,
        kind: EvidenceKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<bool>,
    },
    /// A deterministic limit stopped the work (attempts, time, a loop).
    LimitReached {
        limit: String,
    },
    /// A kind this build doesn't know.
    #[serde(other)]
    Unknown,
}

impl FeatureEvent {
    /// One line for a timeline.
    pub fn text(&self) -> String {
        match self {
            FeatureEvent::FeatureCreated { title } => format!("Created “{title}”"),
            FeatureEvent::StatusChanged { to, reason, .. } => match reason {
                Some(r) => format!("{} — {r}", capital(to.as_str())),
                None => capital(to.as_str()),
            },
            FeatureEvent::MessageAdded { role, .. } => match role {
                MessageRole::User => "You sent a message".into(),
                MessageRole::Controller => "Otter replied".into(),
                MessageRole::Agent => "The coding agent reported".into(),
                MessageRole::System => "Otter noted something".into(),
            },
            FeatureEvent::CommandAccepted { action, .. } => format!("Command: {action}"),
            FeatureEvent::PlanSet { tasks, criteria } => {
                format!("Plan: {tasks} task(s), {criteria} acceptance criteria")
            }
            FeatureEvent::TaskChanged { task_id, status } => {
                format!("Task {task_id}: {status:?}").to_lowercase()
            }
            FeatureEvent::RunStarted { runtime, .. } => format!("A {runtime} run started"),
            FeatureEvent::RunChanged { state, .. } => {
                format!("Run {}", format!("{state:?}").to_lowercase())
            }
            FeatureEvent::DecisionRequested { kind, risk, .. } => {
                format!("Decision needed ({kind:?}, {risk:?} risk)").to_lowercase()
            }
            FeatureEvent::DecisionResolved { status, by, .. } => {
                format!("Decision {status:?} by {by:?}").to_lowercase()
            }
            FeatureEvent::EvidenceAdded { kind, ok, .. } => {
                let verdict = match ok {
                    Some(true) => " (passed)",
                    Some(false) => " (failed)",
                    None => "",
                };
                format!("Evidence: {kind:?}{verdict}").to_lowercase()
            }
            FeatureEvent::LimitReached { limit } => format!("Stopped: {limit}"),
            FeatureEvent::Unknown => "Something happened".into(),
        }
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// One line of a feature's append-only history.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FeatureEventRecord {
    /// Per-feature, strictly increasing from 1.
    pub seq: u64,
    pub ts: Timestamp,
    /// [`FEATURE_EVENT_SCHEMA`] when written.
    pub v: u32,
    /// The command (or decision, or run) this happened because of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// One line for a timeline (rendered when written, so old lines read the
    /// same after the wording changes).
    pub text: String,
    #[serde(flatten)]
    pub event: FeatureEvent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use FeatureStatus::*;

    #[test]
    fn lifecycle_follows_the_happy_path_and_its_side_exits() {
        let path = [Draft, Planning, Implementing, Verifying, Review, Done];
        for w in path.windows(2) {
            assert!(w[0].can_become(w[1]), "{:?} -> {:?}", w[0], w[1]);
        }
        // No skipping ahead, no going back to draft, nothing after the end.
        assert!(!Draft.can_become(Implementing));
        assert!(!Implementing.can_become(Done));
        assert!(!Verifying.can_become(Draft));
        assert!(!Done.can_become(Implementing));
        assert!(!Cancelled.can_become(Planning));
        // Verification and review can send work back.
        assert!(Verifying.can_become(Implementing));
        assert!(Review.can_become(Implementing));
        // Side exits from active work, and back.
        for s in [Planning, Implementing, Verifying] {
            assert!(s.can_become(Blocked) && s.can_become(Paused) && s.can_become(Failed));
        }
        assert!(Paused.can_become(Implementing));
        assert!(Failed.can_become(Implementing));
        assert!(Draft.can_become(Cancelled) && Review.can_become(Cancelled));
        assert!(!Draft.can_become(Paused));
        // The goal can move at any point, even after done: plan again.
        for s in [
            Implementing,
            Verifying,
            Review,
            Done,
            Blocked,
            Paused,
            Failed,
        ] {
            assert!(s.can_become(Planning), "{s:?}");
        }
        assert!(!Done.is_terminal() && Cancelled.is_terminal());
    }

    #[test]
    fn transition_records_the_change_and_starts_the_clock() {
        let now = chrono::Utc::now();
        let mut f = Feature::new("t".into(), "r".into(), now);
        assert!(f.transition(Done, None, now).is_err());
        let ev = f.transition(Planning, Some("go".into()), now).unwrap();
        assert_eq!(
            ev,
            FeatureEvent::StatusChanged {
                from: Draft,
                to: Planning,
                reason: Some("go".into())
            }
        );
        assert_eq!(f.active_since, Some(now));
    }

    fn run_at(now: Timestamp) -> Run {
        Run {
            id: RunId::generate(),
            task_id: TaskId::generate(),
            runtime: "claude".into(),
            state: RunState::Running,
            started_at: now,
            ended_at: None,
            summary: None,
            provider_session_id: None,
            activity: vec![],
            conversation_id: None,
            turn_id: None,
            worked_ms: 0,
            working_since: Some(now),
        }
    }

    #[test]
    fn a_runs_clock_counts_work_not_waits_or_sleep() {
        let t0 = chrono::Utc::now();
        let s = chrono::Duration::seconds;
        let mut r = run_at(t0);
        assert_eq!(r.worked(t0 + s(10)), s(10));
        // Waiting on a decision for an hour: not work.
        r.stop_clock(t0 + s(10));
        assert_eq!(r.worked(t0 + s(3610)), s(10));
        r.start_clock(t0 + s(3610));
        assert_eq!(r.worked(t0 + s(3615)), s(15));
        // The host slept for 20 min of the stretch: 5 s of it were work.
        r.skip(s(1200), t0 + s(4815));
        assert_eq!(r.worked(t0 + s(4815)), s(15));
        // Never ahead of now.
        r.skip(s(99_999), t0 + s(4815));
        assert_eq!(r.worked(t0 + s(4815)), s(10));
        r.stop_clock(t0 + s(4820));
        assert_eq!(r.worked_ms, 15_000);
        assert_eq!(r.worked(t0 + s(9999)), s(15));
        // Old documents: no clock at all.
        let old: Run = serde_json::from_value(serde_json::json!({
            "id": "run_1", "task_id": "task_1", "runtime": "claude",
            "state": "running", "started_at": t0,
        }))
        .unwrap();
        assert_eq!(old.worked(t0 + s(60)), s(0));
    }

    #[test]
    fn a_run_works_again_once_its_decisions_are_made() {
        let now = chrono::Utc::now();
        let mut f = Feature::new("t".into(), "r".into(), now);
        let mut r = run_at(now);
        r.state = RunState::Waiting;
        r.working_since = None;
        let run = r.id.clone();
        f.runs.push(r);
        f.decisions.push(DecisionRequest {
            id: DecisionId::generate(),
            task_id: None,
            run_id: Some(run.clone()),
            kind: DecisionKind::ToolPermission,
            risk: Risk::High,
            summary: "Run `rm -rf build`".into(),
            detail: None,
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
        assert!(f.resume_waiting_runs(now).is_empty(), "still waiting");
        f.decisions[0].status = DecisionStatus::Approved;
        let later = now + chrono::Duration::seconds(5);
        assert_eq!(
            f.resume_waiting_runs(later),
            [FeatureEvent::RunChanged {
                run_id: run,
                state: RunState::Running
            }]
        );
        assert_eq!(f.runs[0].working_since, Some(later));
    }

    #[test]
    fn the_request_message_is_the_first_one_only_when_it_is_the_request() {
        let now = chrono::Utc::now();
        let msg = |role, text: &str| Message {
            id: MessageId::generate(),
            role,
            text: text.into(),
            at: now,
            correlation_id: None,
        };
        let mut f = Feature::new("t".into(), String::new(), now);
        f.messages
            .push(msg(MessageRole::Controller, "What should this feature do?"));
        f.messages.push(msg(MessageRole::User, "Add CSV export"));
        assert_eq!(
            f.request_message(),
            None,
            "no request yet: a message to read"
        );
        f.request = "Add CSV export".into();
        assert_eq!(f.request_message(), Some(&f.messages[1].id));
        f.request = "Something else".into();
        assert_eq!(f.request_message(), None);
    }

    fn decision(kind: DecisionKind, summary: &str, at: Timestamp) -> DecisionRequest {
        DecisionRequest {
            id: DecisionId::generate(),
            task_id: None,
            run_id: None,
            kind,
            risk: Risk::High,
            summary: summary.into(),
            detail: None,
            options: vec![],
            status: DecisionStatus::Pending,
            decided_by: None,
            answer: None,
            rationale: None,
            created_at: at,
            decided_at: None,
            user_only: true,
            grant: None,
        }
    }

    #[test]
    fn what_a_feature_needs_is_one_rule() {
        let t0 = chrono::Utc::now();
        let later = t0 + chrono::Duration::minutes(5);
        let mut f = Feature::new("t".into(), "r".into(), t0);
        assert_eq!((f.needs(), f.activity()), (None, Activity::Idle));
        f.transition(Planning, None, t0).unwrap();
        f.transition(Implementing, None, t0).unwrap();
        assert_eq!((f.needs(), f.activity()), (None, Activity::Working));
        // Decisions: the oldest first, counted.
        f.decisions.push(decision(
            DecisionKind::ToolPermission,
            "Run `git push`",
            later,
        ));
        f.decisions
            .push(decision(DecisionKind::Question, "Which format?", t0));
        let n = f.needs().unwrap();
        assert_eq!(
            (n.kind, n.summary.as_str(), n.since, n.decisions),
            (NeedKind::Question, "Which format?", t0, 2)
        );
        assert_eq!(n.decision_id.as_ref(), Some(&f.decisions[1].id));
        assert_eq!(f.activity(), Activity::NeedsYou);
        f.decisions
            .iter_mut()
            .for_each(|d| d.status = DecisionStatus::Approved);
        // Review waits on the developer once it's ready to accept, not
        // while it publishes or CI runs.
        f.transition(Verifying, None, t0).unwrap();
        f.transition(Review, Some("Ready for your review".into()), later)
            .unwrap();
        assert_eq!((f.needs(), f.activity()), (None, Activity::Working));
        f.report = Some("## Report".into());
        let n = f.needs().unwrap();
        assert_eq!((n.kind, n.since), (NeedKind::Review, later));
        f.report = None;
        f.gates.push(Gate {
            name: "CI".into(),
            status: GateStatus::Failed,
            detail: None,
        });
        assert_eq!(f.needs().unwrap().kind, NeedKind::Review);
        // Failed waits on a retry; blocked on the developer.
        f.transition(
            Failed,
            Some("Stopped: the time budget is used up".into()),
            later,
        )
        .unwrap();
        assert_eq!(f.needs().unwrap().kind, NeedKind::Failed);
        assert_eq!(f.activity(), Activity::Failed);
        f.status = Blocked;
        f.status_reason = Some("Choose a workspace".into());
        let n = f.needs().unwrap();
        assert_eq!(
            (n.kind, n.summary.as_str()),
            (NeedKind::Blocked, "Choose a workspace")
        );
        f.status = Done;
        assert_eq!((f.needs(), f.activity()), (None, Activity::Completed));
        f.status = Paused;
        assert_eq!(f.activity(), Activity::Idle);
        f.status = Cancelled;
        assert_eq!(f.activity(), Activity::Archived);
        // Documents from before `status_since`: since the last change.
        let mut old = f.clone();
        old.status = Failed;
        old.status_since = None;
        assert_eq!(old.needs().unwrap().since, old.updated_at);
    }

    #[test]
    fn records_are_flat_and_unknown_kinds_parse() {
        let rec = FeatureEventRecord {
            seq: 2,
            ts: chrono::Utc::now(),
            v: FEATURE_EVENT_SCHEMA,
            correlation_id: Some("cmd_1".into()),
            text: "x".into(),
            event: FeatureEvent::CommandAccepted {
                command_id: "cmd_1".into(),
                action: "start".into(),
            },
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains(r#""type":"CommandAccepted""#), "{json}");
        assert_eq!(
            serde_json::from_str::<FeatureEventRecord>(&json).unwrap(),
            rec
        );
        let future: FeatureEventRecord = serde_json::from_str(
            r#"{"seq":3,"ts":"2026-10-09T00:00:00Z","v":9,"text":"?","type":"Teleported"}"#,
        )
        .unwrap();
        assert_eq!(future.event, FeatureEvent::Unknown);
    }

    #[test]
    fn a_minimal_document_loads_with_defaults() {
        // What an early writer (or a hand edit) might leave: no schema, no lists.
        let f: Feature = serde_json::from_str(
            r#"{"id":"ft_x","title":"t","status":"draft","created_at":"2026-10-09T00:00:00Z","updated_at":"2026-10-09T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(f.schema, 1);
        assert!(f.tasks.is_empty());
        assert_eq!(f.budget, Budget::default());
    }

    #[test]
    fn actions_are_tagged() {
        let a: FeatureAction =
            serde_json::from_str(r#"{"action":"decide","decision_id":"dec_1","approve":false}"#)
                .unwrap();
        assert_eq!(a.name(), "decide");
        assert_eq!(
            serde_json::to_value(FeatureAction::Pause).unwrap(),
            serde_json::json!({"action":"pause"})
        );
    }
}
