// The Features contract (D-042): mirrors `otter_core::feature` on the wire
// (snake_case, as `otterd` sends it). Features are product work — what should
// be built — owned by a host's daemon and linked to Workspaces and Sessions
// by id, never embedded in them.

import type { Activity } from "./types";

export type FeatureStatus =
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
export type TaskStatus = "pending" | "running" | "blocked" | "done" | "failed" | "skipped";
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

export interface Task {
  id: string;
  title: string;
  detail?: string;
  status: TaskStatus;
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
  task_id: string;
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
  task_id?: string;
  run_id?: string;
  kind: DecisionKind;
  risk: Risk;
  summary: string;
  detail?: string;
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
  /** What approving it allows for the rest of the feature (opaque). */
  grant?: string;
}

export interface Evidence {
  id: string;
  task_id?: string;
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

export interface Feature {
  id: string;
  title: string;
  /** What the developer asked for, as written. */
  request: string;
  status: FeatureStatus;
  status_reason?: string;
  requirements: string[];
  acceptance: Criterion[];
  tasks: Task[];
  runs: Run[];
  messages: Message[];
  decisions: DecisionRequest[];
  evidence: Evidence[];
  budget: Budget;
  /** The workspace the feature's work happens in, once chosen. */
  workspace_id?: string;
  created_at: string;
  updated_at: string;
  /** The last entry of the feature's own history (`feature.events`). */
  history_seq?: number;
  /** The command that checks the work, from the plan. */
  verify_command?: string;
  /** Why the Control Agent did what it did, newest last. */
  rationale?: string[];
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
  /** Computed by otter-core where the app gets features (D-071). */
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

/** A workspace a feature can work in. */
export interface WorkspaceChoice {
  id: string;
  name: string;
}

/** A message being written right now (`FeatureStream`, D-051). */
export interface Draft {
  stream_id: string;
  role: MessageRole;
  /** The text so far. */
  text: string;
  /** Written: the message itself is on its way with the next reload. */
  done: boolean;
}

/** One entry of a feature's own append-only history (`feature.events`). */
export interface FeatureEventRecord {
  seq: number;
  ts: string;
  /** Schema version of this record. */
  v: number;
  correlation_id?: string;
  type: string;
  /** What happened, in one line. */
  text: string;
}

/** A feature as the app lists it: on a host. */
export interface PlacedFeature {
  host: string;
  feature: Feature;
  /** `host/feature-id`. */
  key: string;
}

/** What the developer can tell a feature to do (`feature.act`). */
export type FeatureAction =
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

export type FeatureFilter = "all" | "active" | "needs" | "done" | "failed";

export const FILTERS: { id: FeatureFilter; label: string }[] = [
  { id: "all", label: "All" },
  { id: "needs", label: "Needs you" },
  { id: "active", label: "Active" },
  { id: "done", label: "Done" },
  { id: "failed", label: "Failed" },
];

const ACTIVE: FeatureStatus[] = ["planning", "implementing", "verifying", "review"];

export function pendingDecisions(f: Feature): DecisionRequest[] {
  return f.decisions.filter((d) => d.status === "pending");
}

/**
 * What it waits on you for. otter-core decides (D-071) and the app is
 * served the answer; preview features without it get a rough stand-in.
 */
export function needOf(f: Feature): Need | undefined {
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
export function featureActivity(f: Feature): Activity {
  if (f.activity !== undefined) return f.activity;
  const n = needOf(f);
  if (n) return n.kind === "failed" ? "failed" : "needs_you";
  if (f.status === "done") return "completed";
  if (f.status === "cancelled") return "archived";
  if (f.status === "draft" || f.status === "paused") return "idle";
  return "working";
}

/** The developer has to act: a decision, review, a block, a stop. */
export function needsYou(f: Feature): boolean {
  return needOf(f) !== undefined;
}

export function matches(f: Feature, filter: FeatureFilter): boolean {
  switch (filter) {
    case "all":
      return true;
    case "needs":
      return needsYou(f);
    case "active":
      return ACTIVE.includes(f.status) || f.status === "paused" || f.status === "draft";
    case "done":
      return f.status === "done";
    case "failed":
      return f.status === "failed" || f.status === "cancelled";
  }
}

/**
 * What the developer sees (D-050): planning, implementing and verifying are
 * the Control Agent's own loop, all "Working".
 */
export const STATUS_LABEL: Record<FeatureStatus, string> = {
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
export function statusDetail(f: Feature): string | undefined {
  if (f.status === "planning") return "planning";
  const i = f.tasks.findIndex((t) => t.status === "running");
  if ((f.status === "implementing" || f.status === "verifying") && f.status_reason === undefined) {
    if (f.status === "verifying") return "checking the work";
    if (i >= 0) return `step ${i + 1} of ${f.tasks.length}: ${f.tasks[i].title}`;
  }
  return f.status_reason;
}

/** The status mark shared with workspaces (shape, not only color). */
export function featureGlyph(f: Feature): "needs" | "working" | "done" | "failed" | "idle" {
  if (needOf(f)?.kind === "failed") return "failed";
  if (needsYou(f)) return "needs";
  if (f.status === "failed") return "failed";
  if (f.status === "done") return "done";
  if (ACTIVE.includes(f.status)) return "working";
  return "idle";
}

/** Features that need you on top, longest-waiting first; then newest activity first. */
export function sortFeatures(list: PlacedFeature[]): PlacedFeature[] {
  return [...list].sort((a, b) => {
    const na = needOf(a.feature);
    const nb = needOf(b.feature);
    if (na && nb) return na.since.localeCompare(nb.since);
    const n = Number(!!nb) - Number(!!na);
    if (n !== 0) return n;
    return b.feature.updated_at.localeCompare(a.feature.updated_at);
  });
}
