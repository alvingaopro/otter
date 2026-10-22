// The worker's wire protocol with otterd (D-057): newline-delimited JSON on
// stdin (commands) and stdout (events). Stdout carries protocol only;
// diagnostics go to stderr. Every frame is bounded and validated before
// anything reaches the SDK.

export const PROTOCOL_VERSION = 1;
/**
 * The largest frame either side accepts, in bytes: a turn with its pictures
 * (up to MAX_IMAGES_TOTAL, base64) has to fit in one.
 */
export const MAX_FRAME = 4 * 1024 * 1024;

/** What a turn may carry in pictures (the same limits as a task message's). */
export const MAX_IMAGES = 4;
export const MAX_IMAGE_BYTES = 700 * 1024;
export const MAX_IMAGES_TOTAL = 2 * 1024 * 1024;
export const IMAGE_TYPES = ["image/png", "image/jpeg", "image/webp"] as const;
export type ImageType = (typeof IMAGE_TYPES)[number];

/** A picture in a turn: its type, and its bytes in base64. */
export interface TurnImage {
  media_type: ImageType;
  data: string;
}

/** A command from otterd. */
export interface Envelope<T extends string = string, P = unknown> {
  protocol_version: number;
  request_id: string;
  run_id: string;
  generation: number;
  type: T;
  payload: P;
}

/** A tool Otter offers its own turn (D-089): otterd answers each call. */
export interface OtterTool {
  name: string;
  description: string;
  /** Its arguments, as a JSON schema (an object's properties). */
  input_schema: Record<string, unknown>;
}

/**
 * What a run is for: `coding` (Claude Code's tools, Otter's policy on each
 * call) or `otter` (Otter's own turn: none of Claude Code's tools, only
 * `otter_tools`). A worker says which it has (`ready`, `--check`).
 */
export const PROFILES = ["coding", "otter"] as const;

/** Claude Code's own tools an `otter` run may be given (D-191). */
export const OTTER_BUILTINS = ["WebSearch", "WebFetch", "Bash"] as const;
export type Profile = (typeof PROFILES)[number];

export interface InitializePayload {
  /** Absent: `coding`. */
  profile?: Profile;
  /** The tools of an `otter` run (required there, refused elsewhere). */
  otter_tools?: OtterTool[];
  /**
   * Claude Code's own tools an `otter` run may use ([`OTTER_BUILTINS`]),
   * each call checked by otterd first (D-191). Absent: none.
   */
  otter_builtins?: string[];
  conversation_id: string;
  cwd: string;
  /** The coding model; absent: the SDK's default. */
  model?: string;
  /** The provider's session to resume (an explicit id, never "the latest"). */
  resume?: string;
  /** Appended to Claude Code's system prompt. */
  instructions?: string;
  max_turns?: number;
  max_budget_usd?: number;
  /** Which settings files Claude Code reads (`project` loads CLAUDE.md). */
  setting_sources: ("user" | "project" | "local")[];
}

export interface SendTurnPayload {
  turn_id: string;
  text: string;
  /** Pictures that go with the text. */
  images?: TurnImage[];
}

export interface InterruptPayload {
  turn_id: string;
}

/** The answer to a `permission_request`. */
export type Resolution =
  | { behavior: "allow" }
  | { behavior: "deny"; message: string }
  /** A question form: question text → answer (multi-select comma-joined). */
  | { behavior: "answer"; answers: Record<string, string> };

export interface ResolvePayload {
  request_id: string;
  resolution: Resolution;
}

/** What otterd answered to one of Otter's tool calls (an `otter` run). */
export interface ToolResultPayload {
  call_id: string;
  output: string;
}

/** Otter's policy for one tool call, checked before it runs. */
export interface PolicyReplyPayload {
  check_id: string;
  decision: "allow" | "deny" | "ask";
  reason?: string;
}

export type Command =
  | Envelope<"initialize", InitializePayload>
  | Envelope<"send_turn", SendTurnPayload>
  | Envelope<"interrupt", InterruptPayload>
  | Envelope<"resolve_interaction", ResolvePayload>
  | Envelope<"policy_reply", PolicyReplyPayload>
  | Envelope<"tool_result", ToolResultPayload>
  | Envelope<"shutdown", Record<string, never>>;

export type Outcome = "completed" | "interrupted" | "cancelled" | "failed" | "limit_reached" | "outcome_unknown";

export type ErrorCategory =
  | "auth_required"
  | "provider_unavailable"
  | "rate_limited"
  | "resume_unavailable"
  | "protocol_incompatible"
  | "worker_crashed"
  | "storage_failed"
  | "invalid_input";

/** What the worker tells otterd. */
export type Event =
  | {
      type: "ready";
      protocol_version: number;
      worker_version: string;
      sdk_version: string;
      node_version: string;
      /** The profiles it runs ([`PROFILES`]); absent from older workers. */
      profiles: string[];
    }
  | { type: "ack"; request_id: string }
  | { type: "nack"; request_id: string; error: string; category: ErrorCategory }
  | {
      type: "session";
      session_id: string;
      model: string;
      claude_code_version: string;
      /** Where its credentials come from (never the credentials). */
      auth_source: string;
    }
  | { type: "delivered"; turn_id: string }
  /** How much background work the agent started still runs (the whole count, each change). */
  | { type: "background"; running: number }
  | { type: "text_delta"; message: string; block: number; text: string }
  | { type: "text"; message: string; block: number; text: string }
  | { type: "tool_started"; call: string; parent: string | null; tool: string; input: unknown }
  | { type: "tool_finished"; call: string; ok: boolean; output: string }
  | { type: "policy_check"; check_id: string; tool: string; input: unknown; tool_use_id: string }
  /** One of Otter's tools was called (an `otter` run): answer with `tool_result`. */
  | { type: "tool_call"; call_id: string; tool: string; input: unknown }
  | { type: "permission_request"; request_id: string; tool: string; input: unknown; tool_use_id: string }
  | {
      type: "turn_finished";
      turn_id: string;
      outcome: Outcome;
      summary: string | null;
      error: ErrorCategory | null;
      /** The session's cost so far, as Claude Code reports it. */
      session_cost_usd: number | null;
    }
  /** Something not fatal worth knowing (diagnostics). */
  | { type: "notice"; message: string }
  | { type: "fatal"; message: string; category: ErrorCategory };

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const str = (v: unknown): v is string => typeof v === "string";
const optStr = (v: unknown) => v === undefined || typeof v === "string";
const optNum = (v: unknown) => v === undefined || (typeof v === "number" && Number.isFinite(v) && v > 0);

/** Read one frame: a command, or why it isn't one. */
export function parseCommand(line: string): { ok: true; command: Command } | { ok: false; error: string } {
  if (Buffer.byteLength(line, "utf8") > MAX_FRAME) return { ok: false, error: "frame too large" };
  let v: unknown;
  try {
    v = JSON.parse(line);
  } catch {
    return { ok: false, error: "not JSON" };
  }
  if (!isObject(v)) return { ok: false, error: "not an object" };
  if (v.protocol_version !== PROTOCOL_VERSION) return { ok: false, error: `protocol ${String(v.protocol_version)} unsupported` };
  if (!str(v.request_id) || !str(v.run_id) || typeof v.generation !== "number" || !str(v.type)) {
    return { ok: false, error: "missing envelope fields" };
  }
  const p = v.payload;
  if (!isObject(p)) return { ok: false, error: "missing payload" };
  const bad = (what: string) => ({ ok: false as const, error: `${v.type}: ${what}` });
  switch (v.type) {
    case "initialize": {
      if (!str(p.conversation_id) || !str(p.cwd)) return bad("conversation_id and cwd are required");
      if (!optStr(p.model) || !optStr(p.resume) || !optStr(p.instructions)) return bad("bad string field");
      if (!optNum(p.max_turns) || !optNum(p.max_budget_usd)) return bad("limits must be positive numbers");
      const sources = p.setting_sources;
      if (!Array.isArray(sources) || !sources.every((s) => s === "user" || s === "project" || s === "local")) {
        return bad("setting_sources must list user/project/local");
      }
      if (p.profile !== undefined && !PROFILES.includes(p.profile as Profile)) return bad("unknown profile");
      if (p.profile === "otter") {
        const tools = p.otter_tools;
        if (!Array.isArray(tools) || tools.length === 0) return bad("an otter run needs otter_tools");
        const ok = (t: unknown) =>
          isObject(t) &&
          str(t.name) &&
          /^[a-z][a-z_]{0,63}$/.test(t.name) &&
          str(t.description) &&
          isObject(t.input_schema);
        if (!tools.every(ok)) return bad("each otter tool needs a name, a description and an input_schema");
        if (sources.length > 0) return bad("an otter run reads no settings");
        const builtins = p.otter_builtins;
        if (
          builtins !== undefined &&
          (!Array.isArray(builtins) ||
            !builtins.every((b) => typeof b === "string" && (OTTER_BUILTINS as readonly string[]).includes(b)))
        ) {
          return bad(`otter_builtins are some of ${OTTER_BUILTINS.join(", ")}`);
        }
      } else if (p.otter_tools !== undefined || p.otter_builtins !== undefined) {
        return bad("otter_tools are for an otter run");
      }
      break;
    }
    case "send_turn": {
      if (!str(p.turn_id) || !str(p.text) || p.text.length === 0) return bad("turn_id and text are required");
      if (p.images !== undefined) {
        const why = badImages(p.images);
        if (why) return bad(why);
      }
      break;
    }
    case "interrupt":
      if (!str(p.turn_id)) return bad("turn_id is required");
      break;
    case "resolve_interaction": {
      const r = p.resolution;
      if (!str(p.request_id) || !isObject(r)) return bad("request_id and resolution are required");
      if (r.behavior === "deny" && !str(r.message)) return bad("a denial needs a message");
      if (r.behavior === "answer" && !(isObject(r.answers) && Object.values(r.answers).every(str))) {
        return bad("answers must map question text to text");
      }
      if (!["allow", "deny", "answer"].includes(String(r.behavior))) return bad("unknown behavior");
      break;
    }
    case "policy_reply":
      if (!str(p.check_id) || !["allow", "deny", "ask"].includes(String(p.decision))) return bad("check_id and decision are required");
      break;
    case "tool_result":
      if (!str(p.call_id) || !str(p.output)) return bad("call_id and output are required");
      break;
    case "shutdown":
      break;
    default:
      return { ok: false, error: `unknown command ${v.type}` };
  }
  return { ok: true, command: v as unknown as Command };
}

/** Why a turn's pictures are refused, if they are. */
export function badImages(images: unknown): string | undefined {
  if (!Array.isArray(images)) return "images must be a list";
  if (images.length > MAX_IMAGES) return `at most ${MAX_IMAGES} images`;
  let total = 0;
  for (const i of images) {
    if (!isObject(i) || !IMAGE_TYPES.includes(i.media_type as ImageType) || !str(i.data)) {
      return "each image needs a media_type (png, jpeg or webp) and base64 data";
    }
    if (i.data.length === 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(i.data) || i.data.length % 4 !== 0) {
      return "an image isn't valid base64";
    }
    const bytes = (i.data.length / 4) * 3 - (i.data.endsWith("==") ? 2 : i.data.endsWith("=") ? 1 : 0);
    if (bytes > MAX_IMAGE_BYTES) return `an image is over ${MAX_IMAGE_BYTES} bytes`;
    total += bytes;
  }
  if (total > MAX_IMAGES_TOTAL) return `the images are over ${MAX_IMAGES_TOTAL} bytes together`;
  return undefined;
}

/** Write one event as a frame (cut to fit, never dropped silently). */
export function frame(ev: Event): string {
  let line = JSON.stringify(ev);
  if (Buffer.byteLength(line, "utf8") > MAX_FRAME) {
    line = JSON.stringify({ type: "notice", message: `an event (${ev.type}) was too large to send` } satisfies Event);
  }
  return line + "\n";
}
