// The Features contract (D-042): mirrors `otter_core::feature` on the wire
// (snake_case, as `otterd` sends it). Features are product work — what should
// be built — owned by a host's daemon and linked to Workspaces and Sessions
// by id, never embedded in them.

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
export type RunState = "starting" | "running" | "waiting" | "completed" | "failed" | "cancelled" | "handed_off";
export type MessageRole = "user" | "controller" | "agent" | "system";
export type DecisionKind = "tool_permission" | "question" | "plan_approval" | "merge" | "deploy";
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
  | { action: "decide"; decision_id: string; approve: boolean; answer?: string };

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

/** The developer has to act: a decision waits, the feature is blocked, or it is ready for review. */
export function needsYou(f: Feature): boolean {
  return f.status === "blocked" || f.status === "review" || pendingDecisions(f).length > 0;
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

export const STATUS_LABEL: Record<FeatureStatus, string> = {
  draft: "Draft",
  planning: "Planning",
  implementing: "Implementing",
  verifying: "Verifying",
  review: "Ready for review",
  done: "Done",
  blocked: "Blocked",
  paused: "Paused",
  failed: "Failed",
  cancelled: "Cancelled",
};

/** The status mark shared with workspaces (shape, not only color). */
export function featureGlyph(f: Feature): "needs" | "working" | "done" | "failed" | "idle" {
  if (needsYou(f)) return "needs";
  if (f.status === "failed") return "failed";
  if (f.status === "done") return "done";
  if (ACTIVE.includes(f.status)) return "working";
  return "idle";
}

/** Lifecycle steps shown as progress, in order. */
export const STAGES: FeatureStatus[] = ["draft", "planning", "implementing", "verifying", "review", "done"];

/** Which stage a feature is at (an off-path status keeps the last stage it reached). */
export function stageIndex(f: Feature): number {
  const i = STAGES.indexOf(f.status);
  if (i >= 0) return i;
  // Off the happy path: infer from what exists.
  if (f.evidence.length > 0) return STAGES.indexOf("verifying");
  if (f.tasks.some((t) => t.status !== "pending")) return STAGES.indexOf("implementing");
  if (f.tasks.length > 0) return STAGES.indexOf("planning");
  return 0;
}

/** Newest activity first; features that need you on top. */
export function sortFeatures(list: PlacedFeature[]): PlacedFeature[] {
  return [...list].sort((a, b) => {
    const n = Number(needsYou(b.feature)) - Number(needsYou(a.feature));
    if (n !== 0) return n;
    return b.feature.updated_at.localeCompare(a.feature.updated_at);
  });
}
