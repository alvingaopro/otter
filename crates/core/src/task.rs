//! Tasks: product work — what should be built — as opposed to a
//! [`crate::Workspace`], which is where work runs (D-042, D-043).
//!
//! A task is owned by one host's daemon. It links to workspaces and
//! sessions on that host by id and never embeds them, and no workspace knows
//! about tasks. Three things are kept apart:
//!
//! - **messages** — the conversation between the developer, the Control
//!   Agent and the coding agent (what was *said*);
//! - **commands** — what a client *asks* the daemon to do
//!   ([`TaskAction`], carrying a client-chosen id so a repeated delivery
//!   is applied once);
//! - **events** — what *happened*, appended to the task's own ordered
//!   history ([`TaskEventRecord`]).

use serde::{Deserialize, Serialize};

use crate::ids::{
    ConversationId, DecisionId, EvidenceId, MessageId, RunId, SessionId, StepId, TaskId, TurnId,
    WorkspaceId,
};
use crate::model::{Activity, Timestamp};

/// What a task waiting for its workspace says first (D-079).
pub const WAITING: &str = "Waiting for ";
/// How that wait ends when only the developer can end it: their own agent
/// works there, or the workspace is put away (D-134).
pub const YOUR_AGENT: &str = ", an agent you started, is working there";
pub const ARCHIVED: &str = ": it's archived; unarchive it to go on";
/// …or another task's work is there, at rest, until it's done or
/// cancelled (D-156). The app matches it too (`HELD` in `tasks.ts`).
pub const HELD: &str =
    " has unfinished work there: finish or cancel it, or give this task a workspace of its own";

/// Version of the persisted [`Task`] document. Bump with a migration in
/// the daemon's task store.
pub const TASK_SCHEMA: u32 = 1;
/// Version of a [`TaskEventRecord`] line.
pub const TASK_EVENT_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Written down, not started.
    Draft,
    /// The Control Agent is writing requirements, acceptance criteria and steps.
    Planning,
    /// A coding agent works on the steps.
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

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Draft => "draft",
            TaskStatus::Planning => "planning",
            TaskStatus::Implementing => "implementing",
            TaskStatus::Verifying => "verifying",
            TaskStatus::Review => "review",
            TaskStatus::Done => "done",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Paused => "paused",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    /// Nothing more will happen. (Done isn't final: the developer can
    /// always change the goal and the work picks up again, D-050.)
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Cancelled)
    }

    /// The controller is (or should be) driving it.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            TaskStatus::Planning | TaskStatus::Implementing | TaskStatus::Verifying
        )
    }

    /// The lifecycle (D-050) is a loop, not a line: plan → implement →
    /// verify, as many times as it takes, then review; the developer can
    /// change the goal at any point (even after done), which plans again
    /// from where the work stands. Blocked, paused and failed are side exits;
    /// only cancelled is final.
    pub fn can_become(self, to: TaskStatus) -> bool {
        use TaskStatus::*;
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
            // Accepted in review; or planning found a question, not a
            // change, and the coding agent answered it (D-084).
            Done => matches!(self, Review | Planning),
            // From paused: resumed while the developer still has to decide.
            Blocked => self.is_active() || self == Review || self == Paused,
            Paused => self.is_active() || self == Blocked || self == Review,
            Failed => self.is_active() || self == Blocked || self == Review,
            Cancelled => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Blocked,
    Done,
    Failed,
    Skipped,
}

/// Where a managed run stands (D-061). The legal moves are
/// [`RunState::can_become`]; `docs/runtime-states.md` has the table and what
/// each crash leaves behind. Input waiting for a run is a queued turn of the
/// run's conversation, not a run state.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    Running,
    /// Waiting on a decision.
    Waiting,
    Completed,
    Failed,
    /// Stopped on purpose (pause, interrupt, a changed goal, cancel).
    Cancelled,
    /// A human took over the session (D-044).
    HandedOff,
    /// Ended with its process (otterd restarted, or the agent's process was
    /// lost) while what it was doing was unsettled. Its conversation
    /// resumes; nothing claims the cut-off step succeeded.
    Interrupted,
    /// Ended in a way Otter can't safely continue on its own: there is no
    /// conversation to resume, or its record can't be trusted, and starting
    /// over could repeat what the agent already did. The developer decides
    /// (a [`DecisionKind::Recovery`]).
    RecoveryRequired,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Starting => "starting",
            RunState::Running => "running",
            RunState::Waiting => "waiting",
            RunState::Completed => "completed",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
            RunState::HandedOff => "handed_off",
            RunState::Interrupted => "interrupted",
            RunState::RecoveryRequired => "recovery_required",
        }
    }

    pub fn is_over(self) -> bool {
        !matches!(
            self,
            RunState::Starting | RunState::Running | RunState::Waiting
        )
    }

    /// A run goes starting → running ⇄ waiting, and ends once; an ended run
    /// never changes again (a later run continues its work).
    pub fn can_become(self, to: RunState) -> bool {
        !self.is_over() && self != to && to != RunState::Starting
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
    /// A run ended [`RunState::RecoveryRequired`]: start its step over, or
    /// not yet. The developer's only (D-061).
    Recovery,
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
/// criteria it left unmet. The same work after a fix, with no fewer unmet,
/// means another round won't change it (D-165).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VerificationMark {
    pub fingerprint: String,
    /// The unmet criteria, by text.
    pub unmet: Vec<String>,
}

/// Where a pause found a task (D-168).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PausedFrom {
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where a task's own work starts (D-167): the revision its workspace was
/// at when the task first worked there. What the task changed is measured
/// from it, not from wherever the workspace's base happens to be.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StartPoint {
    pub workspace_id: WorkspaceId,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Step {
    pub id: StepId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub status: StepStatus,
    #[serde(default)]
    pub depends_on: Vec<StepId>,
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
    #[serde(alias = "task_id")]
    pub step_id: StepId,
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
    /// When each line of `activity` was noted: as many, or none (runs
    /// from before D-141).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_at: Vec<Timestamp>,
    /// When the agent was last heard from — any sign of work, a word or a
    /// tool (D-141). Only on a live run as clients see it; never kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heard_at: Option<Timestamp>,
    /// How many of its tools (and background commands) run now: a run
    /// that says nothing while one runs isn't silent (D-063). Live only.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub tools_running: u32,
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

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_zero_usize(n: &usize) -> bool {
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
    /// Pictures that came with it, by name in the task's artifacts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
}

/// What a message of pictures alone says, for whoever reads it as words
/// (Otter's turn, the planner, the agent): never blank.
pub const PICTURES_ONLY: &str = "(pictures, no words)";

/// Otter's conversation on a host (D-093): the developer and Otter about
/// any task there, apart from each task's own conversation. Its messages
/// are [`Message`]s, as a task's are.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct OtterChat {
    #[serde(default)]
    pub messages: Vec<Message>,
    /// The last entry of its history (Otter's tool calls).
    #[serde(default)]
    pub history_seq: u64,
    /// The tasks Otter follows here (D-094): those it made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub follows: Vec<Follow>,
}

/// A task Otter follows in its conversation on the host (D-094), and the
/// last change it said something about.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Follow {
    pub task_id: TaskId,
    /// The last change followed up ([`Task::follow_key`]); empty: none.
    #[serde(default)]
    pub seen: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DecisionRequest {
    pub id: DecisionId,
    #[serde(default, alias = "task_id", skip_serializing_if = "Option::is_none")]
    pub step_id: Option<StepId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub kind: DecisionKind,
    pub risk: Risk,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The whole command it is about, when the one-line summary can't show
    /// all of it: redacted, and bounded with a line saying so (D-157).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
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
    /// task, when policy says it may (D-066): opaque to all but policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub id: EvidenceId,
    #[serde(default, alias = "task_id", skip_serializing_if = "Option::is_none")]
    pub step_id: Option<StepId>,
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
pub struct Task {
    #[serde(default = "schema_one")]
    pub schema: u32,
    pub id: TaskId,
    pub title: String,
    /// What the developer asked for, as written.
    #[serde(default)]
    pub request: String,
    /// Only an answer is wanted (D-092): every turn of the coding agent's
    /// only reads, and a plan to change things goes to the developer first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub answer_only: bool,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// When it took its status (absent in documents from before D-071).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_since: Option<Timestamp>,
    /// While paused: the status the pause found and its reason, where a
    /// resume goes back to (D-168).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_from: Option<PausedFrom>,
    /// The trigger that started it, unattended (D-083).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<crate::trigger::Started>,
    #[serde(default)]
    pub requirements: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<Criterion>,
    #[serde(default)]
    #[serde(alias = "tasks")]
    pub steps: Vec<Step>,
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
    /// How many runs the task had at its last verdict, met or not: the next
    /// verdict's judge sees the commands of the runs after them (D-165).
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub judged_runs: usize,
    /// Where its own work starts (D-167): set at its first run in a
    /// workspace with revisions. Absent in older documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_from: Option<StartPoint>,
    /// What the developer allowed once and isn't asked about again in this
    /// task (D-066): `DecisionRequest::grant`s they approved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<String>,
    /// Why the Control Agent did what it did, newest last (short lines).
    #[serde(default)]
    pub rationale: Vec<String>,
    /// Commands someone approved for this task's checks (previews).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_commands: Vec<String>,
    /// The developer's preview of the app, if running (D-046).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<PreviewLink>,
    /// What must hold before the task is done (D-047), as last evaluated.
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
    /// The last entry of this task's history.
    #[serde(default)]
    pub history_seq: u64,
    /// What comes next ([`Task::what_next`]), as clients see the task;
    /// never kept (D-141).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

fn schema_one() -> u32 {
    1
}

impl Task {
    pub fn new(title: String, request: String, now: Timestamp) -> Task {
        Task {
            schema: TASK_SCHEMA,
            id: TaskId::generate(),
            title,
            request,
            answer_only: false,
            status: TaskStatus::Draft,
            status_since: Some(now),
            paused_from: None,
            started_by: None,
            status_reason: None,
            requirements: Vec::new(),
            acceptance: Vec::new(),
            steps: Vec::new(),
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
            judged_runs: 0,
            started_from: None,
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
            next: None,
        }
    }

    pub fn step(&self, id: &StepId) -> Option<&Step> {
        self.steps.iter().find(|t| &t.id == id)
    }

    /// The revision its own work starts from in its current workspace
    /// (D-167), if it was recorded there.
    pub fn own_base(&self) -> Option<&str> {
        self.started_from
            .as_ref()
            .filter(|s| self.workspace_id.as_ref() == Some(&s.workspace_id))
            .map(|s| s.revision.as_str())
    }

    pub fn step_mut(&mut self, id: &StepId) -> Option<&mut Step> {
        self.steps.iter_mut().find(|t| &t.id == id)
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

    /// What the task waits on the developer for, if anything (D-071).
    /// A decision comes first (the oldest); then review once it's ready to
    /// accept (or a gate failed), blocked, failed. Review still publishing
    /// or waiting for CI isn't waiting on them.
    /// The change worth a follow-up (D-094), if it stands at one: done,
    /// failed, in review, or waiting on the developer — with when it got
    /// there, so each change is its own.
    pub fn follow_key(&self) -> Option<String> {
        let notable = matches!(
            self.status,
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Review
        ) || (self.status == TaskStatus::Blocked && self.needs().is_some());
        notable.then(|| {
            format!(
                "{}:{}",
                self.status.as_str(),
                self.status_since
                    .unwrap_or(self.updated_at)
                    .timestamp_millis()
            )
        })
    }

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
            TaskStatus::Review
                if self.report.is_some()
                    || self.gates.iter().any(|g| g.status == GateStatus::Failed) =>
            {
                need(NeedKind::Review, reason("Ready for your review"))
            }
            TaskStatus::Blocked => need(NeedKind::Blocked, reason("Waiting on you")),
            // Unattended work that waited too long for the developer (D-083).
            TaskStatus::Paused
                if self
                    .status_reason
                    .as_deref()
                    .is_some_and(|r| r.starts_with(crate::trigger::WAITED)) =>
            {
                need(NeedKind::Blocked, reason("Waiting on you"))
            }
            TaskStatus::Failed => need(NeedKind::Failed, reason("Stopped")),
            // Waiting for a workspace only the developer can free (D-134,
            // D-156).
            s if s.is_active()
                && self.status_reason.as_deref().is_some_and(|r| {
                    r.starts_with(WAITING)
                        && (r.ends_with(YOUR_AGENT) || r.ends_with(ARCHIVED) || r.ends_with(HELD))
                }) =>
            {
                need(NeedKind::Blocked, reason("Waiting on you"))
            }
            _ => None,
        }
    }

    /// Where it stands, on the workspaces' scale (D-071).
    pub fn activity(&self) -> Activity {
        match (self.needs(), self.status) {
            (Some(n), _) if n.kind == NeedKind::Failed => Activity::Failed,
            (Some(_), _) => Activity::NeedsYou,
            (None, TaskStatus::Done) => Activity::Completed,
            (None, TaskStatus::Cancelled) => Activity::Archived,
            (None, TaskStatus::Draft | TaskStatus::Paused) => Activity::Idle,
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
    pub fn resume_waiting_runs(&mut self, now: Timestamp) -> Vec<TaskEvent> {
        let pending: Vec<RunId> = self
            .pending_decisions()
            .filter_map(|d| d.run_id.clone())
            .collect();
        let mut events = Vec::new();
        for r in &mut self.runs {
            if r.state == RunState::Waiting && !pending.contains(&r.id) {
                r.state = RunState::Running;
                r.start_clock(now);
                events.push(TaskEvent::RunChanged {
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
        to: TaskStatus,
        reason: Option<String>,
        now: Timestamp,
    ) -> Result<TaskEvent, String> {
        let from = self.status;
        if !from.can_become(to) {
            return Err(format!(
                "a {} task can't become {}",
                from.as_str(),
                to.as_str()
            ));
        }
        if to == TaskStatus::Paused {
            self.paused_from = Some(PausedFrom {
                status: from,
                reason: self.status_reason.clone(),
            });
        } else if from == TaskStatus::Paused {
            self.paused_from = None;
        }
        self.status = to;
        self.status_reason = reason.clone();
        self.status_since = Some(now);
        self.updated_at = now;
        if to.is_active() && !from.is_active() {
            self.active_since = Some(now);
        }
        Ok(TaskEvent::StatusChanged { from, to, reason })
    }

    /// Note why the Control Agent did what it did: kept with the task, and
    /// the event that puts it in the history, at its time (D-141).
    pub fn reason(&mut self, why: impl Into<String>) -> TaskEvent {
        let why = crate::redact::redact(&why.into());
        self.rationale.push(why.clone());
        TaskEvent::Reasoned { why }
    }

    /// What comes next while Otter has the work (D-141): what the engine
    /// does after what it does now, from the task's own record and the
    /// transitions in `docs/task-states.md`. Where it depends on what the
    /// engine hasn't seen yet (the check, CI), both ways are said. `None`
    /// when the task waits on the developer (that is [`Task::needs`]), or
    /// isn't being worked.
    pub fn what_next(&self) -> Option<String> {
        if self.needs().is_some() {
            return None;
        }
        let work: Vec<&Step> = self.steps.iter().filter(|t| !t.planning).collect();
        let check = || match self.acceptance.len() {
            0 => "the work is checked".to_owned(),
            1 => "the work is checked against its acceptance criterion".to_owned(),
            n => format!("the work is checked against its {n} acceptance criteria"),
        };
        match self.status {
            TaskStatus::Planning => Some(if self.answer_only {
                "Then: the coding agent's answer, or its plan for you to approve.".into()
            } else {
                "Then: Otter reviews the coding agent's plan and starts on it, or posts its \
                 answer."
                    .into()
            }),
            TaskStatus::Implementing => {
                let open: Vec<(usize, &&Step)> = work
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| matches!(t.status, StepStatus::Pending | StepStatus::Running))
                    .collect();
                // The one after the step under way (or the first, between steps).
                let after = match open.first() {
                    Some((_, t)) if t.status == StepStatus::Running => open.get(1),
                    first => first,
                };
                Some(match after {
                    Some((i, t)) => {
                        format!("Then: step {} of {} — {}.", i + 1, work.len(), t.title)
                    }
                    None => format!("Then: {}.", check()),
                })
            }
            TaskStatus::Verifying => Some(
                "Then: review if every criterion is met; a fix step if one isn't and the \
                 work changed."
                    .into(),
            ),
            TaskStatus::Review => {
                let waiting: Vec<&str> = self
                    .gates
                    .iter()
                    .filter(|g| g.status == GateStatus::Pending && g.name != "Your acceptance")
                    .map(|g| g.name.as_str())
                    .collect();
                Some(if waiting.is_empty() {
                    "Then: the report, for your acceptance.".into()
                } else {
                    format!(
                        "Then: the report, once {} {} settled; a fix step if CI fails on the \
                         code.",
                        waiting.join(", "),
                        if waiting.len() == 1 { "is" } else { "are" }
                    )
                })
            }
            TaskStatus::Paused => Some("Then: Resume carries on where it stopped.".into()),
            _ => None,
        }
    }
}

/// What a task waits on the developer for (D-071): one rule, here, for
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

/// What a client tells a task to do (`task.act`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TaskAction {
    /// Draft → planning: hand it to the Control Agent.
    Start,
    Pause,
    Resume,
    Cancel,
    /// A failed task: try again with a fresh budget of attempts.
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
    /// The developer signs off a task in review. Refused while a gate
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
    /// Work in a workspace of its own (D-080): a worktree of its Git
    /// workspace's repository, on a branch for the task — when another
    /// task holds that workspace.
    OwnWorkspace,
    /// Start (or restart) the app's preview for the developer to look at.
    Preview,
    /// Stop what the coding agent is doing now (its current turn only) and
    /// wait: the task pauses (D-059).
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
    /// Doesn't apply to this task (no browser checks, no CI, …).
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

/// A preview of the app running in the task's workspace (D-046): a
/// service session listening on `port` on the host. A client reaches it by
/// forwarding that one port.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PreviewLink {
    pub session_id: SessionId,
    pub port: u16,
    /// The path to open (e.g. `/`).
    pub path: String,
}

impl TaskAction {
    pub fn name(&self) -> &'static str {
        match self {
            TaskAction::Start => "start",
            TaskAction::Pause => "pause",
            TaskAction::Resume => "resume",
            TaskAction::Cancel => "cancel",
            TaskAction::Retry => "retry",
            TaskAction::Decide { .. } => "decide",
            TaskAction::Accept { .. } => "accept",
            TaskAction::RequestChanges { .. } => "request_changes",
            TaskAction::SetWorkspace { .. } => "set_workspace",
            TaskAction::OwnWorkspace => "own_workspace",
            TaskAction::Preview => "preview",
            TaskAction::Interrupt => "interrupt",
            TaskAction::Redirect { .. } => "redirect",
        }
    }
}

/// Something that happened to a task. Carries ids and short labels; the
/// content (messages, evidence details) is in the task itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum TaskEvent {
    #[serde(alias = "FeatureCreated")]
    TaskCreated {
        title: String,
    },
    StatusChanged {
        from: TaskStatus,
        to: TaskStatus,
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
        #[serde(alias = "tasks")]
        steps: usize,
        criteria: usize,
    },
    #[serde(alias = "TaskChanged")]
    StepChanged {
        #[serde(alias = "task_id")]
        step_id: StepId,
        status: StepStatus,
    },
    RunStarted {
        run_id: RunId,
        #[serde(alias = "task_id")]
        step_id: StepId,
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
    /// Otter is about to change something with one of its tools (D-089):
    /// written before the change, so one that never finished is known.
    ToolCalled {
        tool: String,
        /// What started Otter's turn: `developer` (their message) or `event`.
        origin: String,
        /// The developer's message the change acts on, when it does.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authority: Option<String>,
    },
    /// What came of one of Otter's tools: done, or why not.
    ToolDone {
        tool: String,
        /// The error's code; absent: done.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Why the Control Agent did what it did (D-141): the line it adds to
    /// `Task::rationale`, at its time.
    Reasoned {
        why: String,
    },
    /// A kind this build doesn't know.
    #[serde(other)]
    Unknown,
}

impl TaskEvent {
    /// One line for a timeline.
    pub fn text(&self) -> String {
        match self {
            TaskEvent::TaskCreated { title } => format!("Created “{title}”"),
            TaskEvent::StatusChanged { to, reason, .. } => match reason {
                Some(r) => format!("{} — {r}", capital(to.as_str())),
                None => capital(to.as_str()),
            },
            TaskEvent::MessageAdded { role, .. } => match role {
                MessageRole::User => "You sent a message".into(),
                MessageRole::Controller => "Otter replied".into(),
                MessageRole::Agent => "The coding agent reported".into(),
                MessageRole::System => "Otter noted something".into(),
            },
            TaskEvent::CommandAccepted { action, .. } => format!("Command: {action}"),
            TaskEvent::PlanSet { steps, criteria } => {
                format!("Plan: {steps} step(s), {criteria} acceptance criteria")
            }
            TaskEvent::StepChanged { step_id, status } => {
                format!("Step {step_id}: {status:?}").to_lowercase()
            }
            TaskEvent::RunStarted { runtime, .. } => format!("A {runtime} run started"),
            TaskEvent::RunChanged { state, .. } => {
                format!("Run {}", state.as_str().replace('_', " "))
            }
            TaskEvent::DecisionRequested { kind, risk, .. } => {
                format!("Decision needed ({kind:?}, {risk:?} risk)").to_lowercase()
            }
            TaskEvent::DecisionResolved { status, by, .. } => {
                format!("Decision {status:?} by {by:?}").to_lowercase()
            }
            TaskEvent::EvidenceAdded { kind, ok, .. } => {
                let verdict = match ok {
                    Some(true) => " (passed)",
                    Some(false) => " (failed)",
                    None => "",
                };
                format!("Evidence: {kind:?}{verdict}").to_lowercase()
            }
            TaskEvent::LimitReached { limit } => format!("Stopped: {limit}"),
            TaskEvent::ToolCalled { tool, .. } => format!("Otter: {tool}"),
            TaskEvent::ToolDone { tool, error } => match error {
                Some(code) => format!("Otter: {tool} — {}", code.replace('_', " ")),
                None => format!("Otter: {tool} — done"),
            },
            // A timeline's line: the reason's first; the whole is in `why`.
            TaskEvent::Reasoned { why } => {
                format!("Why: {}", why.lines().next().unwrap_or_default().trim_end())
            }
            TaskEvent::Unknown => "Something happened".into(),
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

/// One line of a task's append-only history.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskEventRecord {
    /// Per-task, strictly increasing from 1.
    pub seq: u64,
    pub ts: Timestamp,
    /// [`TASK_EVENT_SCHEMA`] when written.
    pub v: u32,
    /// The command (or decision, or run) this happened because of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// One line for a timeline (rendered when written, so old lines read the
    /// same after the wording changes).
    pub text: String,
    #[serde(flatten)]
    pub event: TaskEvent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskStatus::*;

    fn step(id: &str, title: &str, status: StepStatus) -> Step {
        Step {
            id: StepId::from(id),
            title: title.into(),
            detail: None,
            status,
            depends_on: vec![],
            workspace_id: None,
            session_id: None,
            attempts: 0,
            last_error: None,
            planning: false,
        }
    }

    #[test]
    fn what_comes_next_follows_the_transitions() {
        let now = chrono::Utc::now();
        let mut f = Task::new("t".into(), "r".into(), now);
        assert_eq!(f.what_next(), None, "a draft: nothing until Start");
        f.status = Planning;
        assert!(
            f.what_next()
                .unwrap()
                .contains("reviews the coding agent's plan")
        );
        f.status = Implementing;
        f.steps = vec![
            step("s1", "Plan", StepStatus::Done),
            step("s2", "Write the export", StepStatus::Running),
            step("s3", "Wire the button", StepStatus::Pending),
        ];
        f.steps[0].planning = true;
        // The planning turn isn't a step of the work.
        assert_eq!(
            f.what_next().as_deref(),
            Some("Then: step 2 of 2 — Wire the button.")
        );
        f.steps[2].status = StepStatus::Done;
        f.acceptance = vec![Criterion {
            id: "c1".into(),
            text: "x".into(),
            met: None,
            evidence: vec![],
            how: Proof::default(),
        }];
        assert_eq!(
            f.what_next().as_deref(),
            Some("Then: the work is checked against its acceptance criterion.")
        );
        f.status = Verifying;
        assert!(f.what_next().unwrap().contains("a fix step"));
        f.status = Review;
        f.gates = vec![
            Gate {
                name: "CI".into(),
                status: GateStatus::Pending,
                detail: None,
            },
            Gate {
                name: "Your acceptance".into(),
                status: GateStatus::Pending,
                detail: None,
            },
        ];
        assert_eq!(
            f.what_next().as_deref(),
            Some("Then: the report, once CI is settled; a fix step if CI fails on the code.")
        );
        // Ready for review: that's what it needs, not what comes next.
        f.report = Some("## Report".into());
        assert_eq!(f.what_next(), None);
        for s in [Blocked, Failed, Done, Cancelled] {
            f.status = s;
            f.report = None;
            assert_eq!(f.what_next(), None, "{s:?}");
        }
    }

    #[test]
    fn a_reason_is_kept_and_goes_in_the_history() {
        let mut f = Task::new("t".into(), "r".into(), chrono::Utc::now());
        let e = f.reason("Sent the plan back:\n- no tests\n- token=abc123secret");
        assert_eq!(f.rationale.len(), 1);
        assert!(!f.rationale[0].contains("abc123secret"), "redacted");
        assert_eq!(e.text(), "Why: Sent the plan back:");
        let TaskEvent::Reasoned { why } = &e else {
            panic!("{e:?}")
        };
        assert_eq!(why, &f.rationale[0]);
        // An older build reads it as something that happened.
        #[derive(Deserialize)]
        #[serde(tag = "type")]
        enum Old {
            #[serde(other)]
            Unknown,
        }
        let json = serde_json::to_string(&e).unwrap();
        assert!(matches!(
            serde_json::from_str::<Old>(&json),
            Ok(Old::Unknown)
        ));
    }

    #[test]
    fn a_run_from_before_activity_times_reads() {
        let r: Run = serde_json::from_str(
            r#"{"id":"run_1","step_id":"s1","runtime":"claude","state":"completed",
                "started_at":"2026-10-20T12:00:00Z","activity":["$ make test"]}"#,
        )
        .unwrap();
        assert_eq!(r.activity.len(), 1);
        assert!(r.activity_at.is_empty());
        assert_eq!((r.heard_at, r.tools_running), (None, 0));
        // Nothing live is written back.
        let out = serde_json::to_value(&r).unwrap();
        assert!(out.get("heard_at").is_none() && out.get("tools_running").is_none());
    }

    #[test]
    fn a_run_ending_reads_plainly_and_round_trips() {
        for (state, text) in [
            (RunState::Interrupted, "Run interrupted"),
            (RunState::RecoveryRequired, "Run recovery required"),
            (RunState::HandedOff, "Run handed off"),
        ] {
            let e = TaskEvent::RunChanged {
                run_id: RunId::from("run_1"),
                state,
            };
            assert_eq!(e.text(), text);
        }
        let k: DecisionKind = serde_json::from_str("\"recovery\"").unwrap();
        assert_eq!(k, DecisionKind::Recovery);
        let s: RunState = serde_json::from_str("\"recovery_required\"").unwrap();
        assert_eq!(s, RunState::RecoveryRequired);
    }

    #[test]
    fn a_run_ends_once() {
        use RunState as R;
        let ends = [
            R::Completed,
            R::Failed,
            R::Cancelled,
            R::HandedOff,
            R::Interrupted,
            R::RecoveryRequired,
        ];
        for live in [R::Starting, R::Running, R::Waiting] {
            assert!(!live.is_over());
            for end in ends {
                assert!(live.can_become(end), "{live:?} -> {end:?}");
            }
            assert!(!live.can_become(R::Starting));
        }
        assert!(R::Starting.can_become(R::Running));
        assert!(R::Running.can_become(R::Waiting));
        assert!(R::Waiting.can_become(R::Running));
        for end in ends {
            assert!(end.is_over());
            for to in ends.iter().chain(&[R::Running, R::Waiting]) {
                assert!(!end.can_become(*to), "{end:?} -> {to:?}");
            }
            // The wire name is the serde name.
            assert_eq!(
                serde_json::to_string(&end).unwrap(),
                format!("\"{}\"", end.as_str())
            );
        }
    }

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
        // A question is answered while planning: done, nothing built.
        assert!(Planning.can_become(Done));
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
        let mut f = Task::new("t".into(), "r".into(), now);
        assert!(f.transition(Done, None, now).is_err());
        let ev = f.transition(Planning, Some("go".into()), now).unwrap();
        assert_eq!(
            ev,
            TaskEvent::StatusChanged {
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
            step_id: StepId::generate(),
            runtime: "claude".into(),
            state: RunState::Running,
            started_at: now,
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
        let mut f = Task::new("t".into(), "r".into(), now);
        let mut r = run_at(now);
        r.state = RunState::Waiting;
        r.working_since = None;
        let run = r.id.clone();
        f.runs.push(r);
        f.decisions.push(DecisionRequest {
            id: DecisionId::generate(),
            step_id: None,
            run_id: Some(run.clone()),
            kind: DecisionKind::ToolPermission,
            risk: Risk::High,
            summary: "Run `rm -rf build`".into(),
            detail: None,
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
        assert!(f.resume_waiting_runs(now).is_empty(), "still waiting");
        f.decisions[0].status = DecisionStatus::Approved;
        let later = now + chrono::Duration::seconds(5);
        assert_eq!(
            f.resume_waiting_runs(later),
            [TaskEvent::RunChanged {
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
            attachments: vec![],
        };
        let mut f = Task::new("t".into(), String::new(), now);
        f.messages
            .push(msg(MessageRole::Controller, "What should this task do?"));
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
            step_id: None,
            run_id: None,
            kind,
            risk: Risk::High,
            summary: summary.into(),
            detail: None,
            command: None,
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
    fn what_a_task_needs_is_one_rule() {
        let t0 = chrono::Utc::now();
        let later = t0 + chrono::Duration::minutes(5);
        let mut f = Task::new("t".into(), "r".into(), t0);
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
    fn a_workspace_only_the_developer_can_free_waits_on_them() {
        let t0 = chrono::Utc::now();
        let mut f = Task::new("t".into(), "r".into(), t0);
        f.transition(Planning, None, t0).unwrap();
        let waiting = |what: &str| format!("{WAITING}proj{what}");
        for (why, theirs) in [
            (waiting(&format!(": claude{YOUR_AGENT}")), true),
            (waiting(ARCHIVED), true),
            (waiting(": it's being prepared"), false),
            (waiting(": “Other task” is working there"), false),
            (waiting(&format!(": “Other task”{HELD}")), true),
        ] {
            f.status_reason = Some(why.clone());
            let n = f.needs();
            assert_eq!(n.is_some(), theirs, "{why}");
            if let Some(n) = n {
                assert_eq!((n.kind, n.summary), (NeedKind::Blocked, why));
                assert_eq!(f.activity(), Activity::NeedsYou);
            }
        }
        // Paused, it waits on nothing.
        f.status_reason = Some(waiting(&format!(": claude{YOUR_AGENT}")));
        f.status = Paused;
        assert_eq!(f.needs(), None);
    }

    #[test]
    fn records_are_flat_and_unknown_kinds_parse() {
        let rec = TaskEventRecord {
            seq: 2,
            ts: chrono::Utc::now(),
            v: TASK_EVENT_SCHEMA,
            correlation_id: Some("cmd_1".into()),
            text: "x".into(),
            event: TaskEvent::CommandAccepted {
                command_id: "cmd_1".into(),
                action: "start".into(),
            },
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains(r#""type":"CommandAccepted""#), "{json}");
        assert_eq!(serde_json::from_str::<TaskEventRecord>(&json).unwrap(), rec);
        let future: TaskEventRecord = serde_json::from_str(
            r#"{"seq":3,"ts":"2026-10-09T00:00:00Z","v":9,"text":"?","type":"Teleported"}"#,
        )
        .unwrap();
        assert_eq!(future.event, TaskEvent::Unknown);
    }

    /// Otter's tool calls (D-089) are history like the rest; a build from
    /// before them reads them as `Unknown`, with their text (the test above).
    #[test]
    fn otters_tool_records_read_back_and_say_what_happened() {
        let called = TaskEvent::ToolCalled {
            tool: "pause_task".into(),
            origin: "developer".into(),
            authority: Some("msg_1".into()),
        };
        let json = serde_json::to_string(&called).unwrap();
        assert!(json.contains(r#""type":"ToolCalled""#), "{json}");
        assert_eq!(serde_json::from_str::<TaskEvent>(&json).unwrap(), called);
        assert_eq!(called.text(), "Otter: pause_task");
        let refused = TaskEvent::ToolDone {
            tool: "accept_task".into(),
            error: Some("not_allowed_in_this_turn".into()),
        };
        assert_eq!(
            refused.text(),
            "Otter: accept_task — not allowed in this turn"
        );
        let done = TaskEvent::ToolDone {
            tool: "get_task".into(),
            error: None,
        };
        assert!(!serde_json::to_string(&done).unwrap().contains("error"));
        assert_eq!(done.text(), "Otter: get_task — done");
    }

    #[test]
    fn a_minimal_document_loads_with_defaults() {
        // What an early writer (or a hand edit) might leave: no schema, no lists.
        let f: Task = serde_json::from_str(
            r#"{"id":"ft_x","title":"t","status":"draft","created_at":"2026-10-09T00:00:00Z","updated_at":"2026-10-09T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(f.schema, 1);
        assert!(f.steps.is_empty());
        assert_eq!(f.budget, Budget::default());
    }

    #[test]
    fn actions_are_tagged() {
        let a: TaskAction =
            serde_json::from_str(r#"{"action":"decide","decision_id":"dec_1","approve":false}"#)
                .unwrap();
        assert_eq!(a.name(), "decide");
        assert_eq!(
            serde_json::to_value(TaskAction::Pause).unwrap(),
            serde_json::json!({"action":"pause"})
        );
    }
}
