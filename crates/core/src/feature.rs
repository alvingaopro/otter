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
use crate::model::Timestamp;

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
    /// The decision asking to allow `verify_command`, if one was needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_decision: Option<DecisionId>,
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

    /// The run currently working, if any.
    pub fn live_run(&self) -> Option<&Run> {
        self.runs.iter().rev().find(|r| !r.state.is_over())
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
        self.updated_at = now;
        if to.is_active() && !from.is_active() {
            self.active_since = Some(now);
        }
        Ok(FeatureEvent::StatusChanged { from, to, reason })
    }
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
