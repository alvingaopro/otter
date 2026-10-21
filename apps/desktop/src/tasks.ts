// The Tasks contract (D-042): mirrors `otter_core::task` on the wire
// (snake_case, as `otterd` sends it). Tasks are product work — what should
// be built — owned by a host's daemon and linked to Workspaces and Sessions
// by id, never embedded in them.

import { ago } from "./model";
import type { Activity } from "./types";

export type TaskStatus =
  | "draft"
  | "planning"
  | "implementing"
  | "verifying"
  | "review"
  | "done"
  | "blocked"
  | "paused"
  | "failed"
  | "cancelled";
export type StepStatus = "pending" | "running" | "blocked" | "done" | "failed" | "skipped";
export type RunState =
  | "starting"
  | "running"
  | "waiting"
  | "completed"
  | "failed"
  | "cancelled"
  | "handed_off"
  | "interrupted"
  | "recovery_required";
export type MessageRole = "user" | "controller" | "agent" | "system";
export type DecisionKind = "tool_permission" | "question" | "plan_approval" | "merge" | "deploy" | "recovery";
export type Risk = "low" | "medium" | "high";
export type DecisionStatus = "pending" | "approved" | "denied" | "answered" | "cancelled";
export type Decider = "policy" | "controller" | "user";
export type EvidenceKind = "test" | "browser" | "screenshot" | "ci" | "review" | "diff" | "pull_request" | "log" | "note";

export interface Criterion {
  id: string;
  text: string;
  /** Unset until verification has judged it. */
  met?: boolean;
  /** Evidence ids backing the judgment. */
  evidence: string[];
  /** What proves it (D-062): the check's result, or the diff (judged). */
  how?: "check" | "diff";
}

/** A step of a task's plan. */
export interface Step {
  id: string;
  title: string;
  detail?: string;
  status: StepStatus;
  depends_on: string[];
  /** Where it runs: a workspace on the same host, and the session doing it. */
  workspace_id?: string;
  session_id?: string;
  attempts: number;
  last_error?: string;
  /** The coding agent's planning turn, not a step of the work. */
  planning?: boolean;
}

export interface Run {
  id: string;
  step_id: string;
  /** The agent runtime, e.g. `claude`. */
  runtime: string;
  state: RunState;
  started_at: string;
  ended_at?: string;
  summary?: string;
  /** The agent's own conversation id (opaque): what a takeover opens. */
  provider_session_id?: string;
  /** What the agent did lately (tools it used), newest last. */
  activity?: string[];
  /** When each `activity` line was noted: as many, or none (an older otterd, D-141). */
  activity_at?: string[];
  /** A live run: when its agent was last heard from (D-141). */
  heard_at?: string;
  /** A live run: how many of its tools (or background commands) run now. */
  tools_running?: number;
  /** How long the agent has worked, in ms, up to `working_since` (waits don't count). */
  worked_ms?: number;
  /** When the current stretch of work began; absent while waiting or over. */
  working_since?: string;
}

export interface Message {
  id: string;
  role: MessageRole;
  text: string;
  at: string;
  /** The command or decision this message belongs to. */
  correlation_id?: string;
}

export interface DecisionRequest {
  id: string;
  step_id?: string;
  run_id?: string;
  kind: DecisionKind;
  risk: Risk;
  summary: string;
  detail?: string;
  /** The whole command, when the summary can't show all of it (redacted, bounded; D-157). */
  command?: string;
  /** Choices for a question; empty for approve/deny. */
  options: string[];
  status: DecisionStatus;
  decided_by?: Decider;
  answer?: string;
  rationale?: string;
  created_at: string;
  decided_at?: string;
  /** Only the developer may decide this one (policy). */
  user_only?: boolean;
  /** What approving it allows for the rest of the task (opaque). */
  grant?: string;
}

export interface Evidence {
  id: string;
  step_id?: string;
  criterion_id?: string;
  kind: EvidenceKind;
  title: string;
  /** Passed / failed; unset for informational evidence. */
  ok?: boolean;
  /** Where to see it: a URL or a path on the host. */
  uri?: string;
  detail?: string;
  /** Not a deterministic check (e.g. a model's visual review). */
  uncertain?: boolean;
  at: string;
}

export interface Budget {
  max_iterations: number;
  max_minutes: number;
  iterations_used: number;
}

export interface Task {
  id: string;
  title: string;
  /** What the developer asked for, as written. */
  request: string;
  /** Only an answer is wanted: the coding agent only reads (D-092). */
  answer_only?: boolean;
  status: TaskStatus;
  status_reason?: string;
  requirements: string[];
  acceptance: Criterion[];
  steps: Step[];
  runs: Run[];
  messages: Message[];
  decisions: DecisionRequest[];
  evidence: Evidence[];
  budget: Budget;
  /** The workspace the task's work happens in, once chosen. */
  workspace_id?: string;
  created_at: string;
  updated_at: string;
  /** The last entry of the task's own history (`task.events`). */
  history_seq?: number;
  /** The command that checks the work, from the plan. */
  verify_command?: string;
  /** Why the Control Agent did what it did, newest last. */
  rationale?: string[];
  /** What comes next while Otter has the work (D-141). */
  next?: string;
  /** What the developer allowed once and isn't asked about again (opaque). */
  grants?: string[];
  /** The app's preview for the developer, if running: a port on the host. */
  preview?: { session_id: string; port: number; path: string };
  /** What must hold before done, as last evaluated. */
  gates?: Gate[];
  /** The change as published. */
  delivery?: Delivery;
  /** The final report (Markdown). */
  report?: string;
  /** When it took its status. */
  status_since?: string;
  /** Computed by otter-core where the app gets tasks (D-071). */
  activity?: Activity;
  /** What it waits on you for, computed by otter-core (D-071). */
  needs?: Need;
}

export type NeedKind = "decision" | "question" | "review" | "blocked" | "failed";

export interface Need {
  kind: NeedKind;
  summary: string;
  since: string;
  decision_id?: string;
  decisions: number;
}

export type GateStatus = "passed" | "failed" | "pending" | "not_applicable";

export interface Gate {
  name: string;
  status: GateStatus;
  detail?: string;
}

export interface Delivery {
  branch: string;
  base?: string;
  head?: string;
  pushed_at?: string;
  pr_url?: string;
  pr_number?: number;
  ci: { name: string; state: string; url?: string; description?: string }[];
  reruns: number;
  declined: boolean;
}

/** A workspace a task can work in. */
export interface WorkspaceChoice {
  id: string;
  name: string;
  /** Its sessions, to open the one a task waits for. */
  sessions?: { id: string; name: string }[];
}

/** How a task waiting for an agent the developer started says so (`YOUR_AGENT` in otter-core, D-134). */
export const YOUR_AGENT = ", an agent you started, is working there";

/** The session a task waits for in its workspace (D-079), by the reason it gives. */
export function waitedSession(f: Task, ws: WorkspaceChoice | undefined): { id: string; name: string } | undefined {
  const r = f.status_reason;
  if (!ws || !r?.endsWith(YOUR_AGENT)) return undefined;
  const prefix = `Waiting for ${ws.name}: `;
  if (!r.startsWith(prefix)) return undefined;
  const name = r.slice(prefix.length, -YOUR_AGENT.length);
  return ws.sessions?.find((s) => s.name === name);
}

/** A message being written right now (`TaskStream`, D-051). */
export interface Draft {
  stream_id: string;
  role: MessageRole;
  /** The text so far. */
  text: string;
  /** Written: the message itself is on its way with the next reload. */
  done: boolean;
}

/** One entry of a task's own append-only history (`task.events`). */
export interface TaskEventRecord {
  seq: number;
  ts: string;
  /** Schema version of this record. */
  v: number;
  correlation_id?: string;
  type: string;
  /** What happened, in one line. */
  text: string;
}

/** A task as the app lists it: on a host. */
export interface PlacedTask {
  host: string;
  task: Task;
  /** `host/task-id`. */
  key: string;
}

/** What the developer can tell a task to do (`task.act`). */
export type TaskAction =
  | { action: "start" }
  | { action: "pause" }
  | { action: "resume" }
  | { action: "cancel" }
  | { action: "retry" }
  | { action: "decide"; decision_id: string; approve: boolean; answer?: string; answers?: Record<string, string> }
  | { action: "accept"; override_gates?: boolean }
  | { action: "request_changes"; note?: string }
  | { action: "set_workspace"; workspace: string }
  | { action: "own_workspace" }
  | { action: "preview" }
  | { action: "interrupt" }
  | { action: "redirect"; text: string };

// --- Wording and grouping (no runtime state) ---

/** Where the list shows a task: waiting on you, going on, or over. */
export type TaskGroup = "needs" | "active" | "done";

export const GROUPS: { id: TaskGroup; label: string }[] = [
  { id: "needs", label: "Needs you" },
  { id: "active", label: "Active" },
  { id: "done", label: "Done" },
];

const ACTIVE: TaskStatus[] = ["planning", "implementing", "verifying", "review"];

export function pendingDecisions(f: Task): DecisionRequest[] {
  return f.decisions.filter((d) => d.status === "pending");
}

/**
 * What it waits on you for. otter-core decides (D-071) and the app is
 * served the answer; preview tasks without it get a rough stand-in.
 */
export function needOf(f: Task): Need | undefined {
  if (f.activity !== undefined) return f.needs;
  const d = pendingDecisions(f)[0];
  if (d) return { kind: d.kind === "question" ? "question" : "decision", summary: d.summary, since: d.created_at, decision_id: d.id, decisions: pendingDecisions(f).length };
  const since = f.status_since ?? f.updated_at;
  if (f.status === "review" && f.report) return { kind: "review", summary: f.status_reason ?? "Ready for your review", since, decisions: 0 };
  if (f.status === "blocked") return { kind: "blocked", summary: f.status_reason ?? "Waiting on you", since, decisions: 0 };
  if (f.status === "failed") return { kind: "failed", summary: f.status_reason ?? "Stopped", since, decisions: 0 };
  return undefined;
}

/** Where it stands on the workspaces' scale: served by otter-core (D-071); derived for preview data. */
export function taskActivity(f: Task): Activity {
  if (f.activity !== undefined) return f.activity;
  const n = needOf(f);
  if (n) return n.kind === "failed" ? "failed" : "needs_you";
  if (f.status === "done") return "completed";
  if (f.status === "cancelled") return "archived";
  if (f.status === "draft" || f.status === "paused") return "idle";
  return "working";
}

/** The developer has to act: a decision, review, a block, a stop. */
export function needsYou(f: Task): boolean {
  return needOf(f) !== undefined;
}

export function groupOf(f: Task): TaskGroup {
  if (needsYou(f)) return "needs";
  if (f.status === "done" || f.status === "cancelled" || f.status === "failed") return "done";
  return "active";
}

/** Whether `text` finds a task: part of its title, request or host. */
export function findMatch(p: PlacedTask, text: string): boolean {
  const t = text.trim().toLowerCase();
  return !t || [p.task.title, p.task.request, p.host].some((s) => s.toLowerCase().includes(t));
}

/** The same line said several times in a row, once with a count. */
export function collapseRepeats(lines: string[]): { text: string; times: number }[] {
  const out: { text: string; times: number }[] = [];
  for (const text of lines) {
    const last = out[out.length - 1];
    if (last?.text === text) last.times += 1;
    else out.push({ text, times: 1 });
  }
  return out;
}

/**
 * When a working run's agent was last heard from (D-141): a sign of work,
 * not a verdict on it. A tool that runs isn't silence; a run waiting on a
 * decision says so elsewhere; an older otterd doesn't say.
 */
export function heardLine(f: Task, now: number): string | undefined {
  const run = f.runs.find((r) => r.state === "running");
  if (!run) return undefined;
  if ((run.tools_running ?? 0) > 0) return "The coding agent is running a tool.";
  if (!run.heard_at) return undefined;
  return `Last heard from the coding agent ${ago(run.heard_at, now)} ago.`;
}

/**
 * A task's history with a reason said again right after itself (D-141)
 * shown once, the last time, with how many times: D-114's repeats, in the
 * feed. Other events are never folded.
 */
export function collapseReasons(events: TaskEventRecord[]): { e: TaskEventRecord; times: number }[] {
  const out: { e: TaskEventRecord; times: number }[] = [];
  for (const e of events) {
    const last = out[out.length - 1];
    if (e.type === "Reasoned" && last?.e.type === "Reasoned" && last.e.text === e.text) {
      last.e = e;
      last.times += 1;
    } else out.push({ e, times: 1 });
  }
  return out;
}

/** The budget once it constrains the work: half of the attempts used. */
export function budgetBinds(f: Task): boolean {
  return f.budget.max_iterations > 0 && f.budget.iterations_used * 2 >= f.budget.max_iterations;
}

/**
 * What the developer sees (D-050): planning, implementing and verifying are
 * the Control Agent's own loop, all "Working".
 */
export const STATUS_LABEL: Record<TaskStatus, string> = {
  draft: "Draft",
  planning: "Working",
  implementing: "Working",
  verifying: "Working",
  review: "Ready to check",
  done: "Done",
  blocked: "Needs you",
  paused: "Paused",
  failed: "Stopped",
  cancelled: "Cancelled",
};

/** One line on what is going on right now. */
export function statusDetail(f: Task): string | undefined {
  if (f.status === "planning") return "planning";
  const i = f.steps.findIndex((t) => t.status === "running");
  if ((f.status === "implementing" || f.status === "verifying") && f.status_reason === undefined) {
    if (f.status === "verifying") return "checking the work";
    if (i >= 0) return `step ${i + 1} of ${f.steps.length}: ${f.steps[i].title}`;
  }
  return f.status_reason;
}

/** The status mark shared with workspaces (shape, not only color). */
export function taskGlyph(f: Task): "needs" | "working" | "done" | "failed" | "idle" {
  if (needOf(f)?.kind === "failed") return "failed";
  if (needsYou(f)) return "needs";
  if (f.status === "failed") return "failed";
  if (f.status === "done") return "done";
  if (ACTIVE.includes(f.status)) return "working";
  return "idle";
}

/**
 * The tasks still going in a workspace: their coding agent works there
 * without a session of its own, so the workspace shows them instead.
 */
export function tasksIn(list: PlacedTask[], host: string, workspaceId: string): PlacedTask[] {
  return sortTasks(
    list.filter(
      (p) =>
        p.host === host &&
        p.task.status !== "done" &&
        p.task.status !== "cancelled" &&
        (p.task.workspace_id === workspaceId || p.task.steps.some((t) => t.workspace_id === workspaceId)),
    ),
  );
}

/** Tasks that need you on top, longest-waiting first; then newest activity first. */
export function sortTasks(list: PlacedTask[]): PlacedTask[] {
  return [...list].sort((a, b) => {
    const na = needOf(a.task);
    const nb = needOf(b.task);
    if (na && nb) return na.since.localeCompare(nb.since);
    const n = Number(!!nb) - Number(!!na);
    if (n !== 0) return n;
    return b.task.updated_at.localeCompare(a.task.updated_at);
  });
}
