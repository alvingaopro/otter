// The coding agent's side of a feature (D-055…D-059), as the daemon shows
// it: conversations of turns, messages, tool calls and interactions.
// Mirrors `otter_protocol::conversation::ConversationView`; wording only —
// no state of its own.

export type TurnOutcome = "completed" | "interrupted" | "cancelled" | "failed" | "limit_reached" | "outcome_unknown";
export type ErrorCategory =
  | "auth_required"
  | "provider_unavailable"
  | "rate_limited"
  | "resume_unavailable"
  | "protocol_incompatible"
  | "worker_crashed"
  | "storage_failed"
  | "invalid_input";

export interface CodingTurn {
  id: string;
  command_id?: string;
  initiator: "controller" | "user" | "system";
  input: { type: "text"; text: string }[];
  delivery: "queued" | "sending" | "delivered" | "unknown";
  state: "queued" | "running" | "waiting" | "interrupting" | "finished";
  outcome?: TurnOutcome;
  reason?: string;
  error?: ErrorCategory;
  run_id?: string;
  generation?: number;
  queued_at: string;
  started_at?: string;
  ended_at?: string;
  redirect?: boolean;
  usage?: { scope: "turn" | "session_cumulative" | "unknown"; cost_usd?: number };
}

export interface CodingMessage {
  id: string;
  turn_id: string;
  role: string;
  blocks: { id: string; type: "text"; text: string }[];
  lifecycle: "streaming" | "completed" | "interrupted";
  at: string;
}

export type ToolStatus = "pending" | "running" | "succeeded" | "failed" | "denied" | "result_unavailable";

export interface ToolRecord {
  id: string;
  turn_id: string;
  run_id: string;
  name: string;
  effect: string;
  summary: string;
  parent?: string;
  status: ToolStatus;
  started_at: string;
  finished_at?: string;
  result?: { summary: string; artifact?: string; truncated?: boolean };
}

export interface Question {
  id: string;
  header?: string;
  prompt: string;
  options?: { label: string; description?: string }[];
  multi_select?: boolean;
}

export interface Interaction {
  id: string;
  turn_id: string;
  generation: number;
  type: "permission" | "questions" | "plan";
  tool?: string;
  summary?: string;
  questions?: Question[];
  plan?: string;
  status: "pending" | "response_recorded" | "delivered" | "denied" | "expired" | "cancelled" | "delivery_unknown";
  decided_by?: string;
  requested_at: string;
  decided_at?: string;
}

export interface CodingConversation {
  id: string;
  provider: string;
  backend: string;
  lifecycle: "open" | "paused" | "closed";
  generation: number;
  active_run?: string;
  model?: string;
  turns: CodingTurn[];
  messages: CodingMessage[];
  tools: ToolRecord[];
  interactions: Interaction[];
  /** The provider's session can be continued. */
  resumable: boolean;
  /** Why it accepts no more changes, if it doesn't. */
  read_only?: string;
  updated_at: string;
}

/** Mirrors `otter_protocol::conversation::RuntimeCapabilities`. */
export interface RuntimeCapabilities {
  provider: string;
  backend: string;
  available: boolean;
  notes?: string[];
  tested_version?: string;
  structured_ready: boolean;
  features: {
    send_turn: boolean;
    resume: boolean;
    interrupt_turn: boolean;
    permission_requests: boolean;
    questions: boolean;
    tool_results: boolean;
    streaming: boolean;
    attachments: boolean;
    usage: boolean;
    pause?: boolean;
    redirect?: boolean;
  };
}

export const turnText = (t: CodingTurn) => t.input.map((b) => b.text).join("\n");
export const messageText = (m: CodingMessage) => m.blocks.map((b) => b.text).join("\n\n");

const ERROR: Record<ErrorCategory, string> = {
  auth_required: "Claude needs signing in on this host",
  provider_unavailable: "Claude is unavailable right now",
  rate_limited: "Rate limited: try again in a while",
  resume_unavailable: "Its earlier session couldn't be resumed",
  protocol_incompatible: "This host's Claude worker doesn't match otterd",
  worker_crashed: "The coding agent stopped unexpectedly",
  storage_failed: "Otter couldn't record the work",
  invalid_input: "Claude refused the request",
};

/** A turn in a few words: what it is doing, or how it ended (never just "failed"). */
export function turnLabel(t: CodingTurn): string {
  switch (t.state) {
    case "queued":
      return t.redirect ? "Next: a change of direction" : "Queued after the current work";
    case "running":
      return t.delivery === "sending" ? "Sending" : "Working";
    case "waiting":
      return "Needs you";
    case "interrupting":
      return "Stopping the current work";
    case "finished":
      break;
  }
  if (t.error) return ERROR[t.error];
  switch (t.outcome) {
    case "completed":
      return "Done";
    case "interrupted":
      return "Stopped";
    case "cancelled":
      return t.delivery === "queued" ? "Not sent" : "Cancelled";
    case "limit_reached":
      return "Stopped at a limit";
    case "failed":
      return "Didn't finish";
    case "outcome_unknown":
      return "Outcome needs checking";
    default:
      return "Finished";
  }
}

/** The glyph for a turn (`Glyph` kinds). */
export function turnGlyph(t: CodingTurn): "working" | "needs" | "done" | "failed" | "idle" {
  if (t.state === "waiting") return "needs";
  if (t.state !== "finished") return t.state === "queued" ? "idle" : "working";
  if (t.outcome === "completed") return "done";
  if (t.outcome === "failed" || t.outcome === "outcome_unknown" || t.error) return "failed";
  return "idle";
}

export const TOOL_LABEL: Record<ToolStatus, string> = {
  pending: "waiting",
  running: "running",
  succeeded: "done",
  failed: "failed",
  denied: "not allowed",
  result_unavailable: "result unknown",
};

export const TOOL_GLYPH: Record<ToolStatus, "working" | "done" | "failed" | "idle" | "needs"> = {
  pending: "needs",
  running: "working",
  succeeded: "done",
  failed: "failed",
  denied: "failed",
  result_unavailable: "idle",
};

/** Where the coding agent stands, in a line: what's running, or why not. */
export function conversationStatus(c: CodingConversation): string {
  if (c.read_only) return `Read-only: ${c.read_only}`;
  const active = c.turns.find((t) => ["running", "waiting", "interrupting"].includes(t.state));
  if (active) return turnLabel(active);
  if (c.lifecycle === "paused") return "Paused";
  if (c.lifecycle === "closed") return "Cancelled";
  const queued = c.turns.filter((t) => t.state === "queued").length;
  if (queued > 0) return `${queued} queued for the next run`;
  const last = c.turns[c.turns.length - 1];
  return last ? turnLabel(last) : "Not started";
}

/**
 * A question form's answers, as the daemon takes them (`decide {answers}`):
 * question id → the chosen labels comma-joined, or the free text. `null`
 * while a question is unanswered.
 */
export function formAnswers(
  questions: Question[],
  chosen: Record<string, string[]>,
  other: Record<string, string>,
): Record<string, string> | null {
  const out: Record<string, string> = {};
  for (const q of questions) {
    const text = (other[q.id] ?? "").trim();
    const picked = chosen[q.id] ?? [];
    const answer = text || picked.join(", ");
    if (!answer) return null;
    out[q.id] = answer;
  }
  return out;
}
