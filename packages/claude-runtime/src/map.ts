// SDK messages → Otter's normalized events (D-057). Pure: no I/O, so every
// shape is tested with fixtures.
//
// Identity: a streamed text block is (message id, block index) from
// `message_start` / `content_block_delta`; the whole `assistant` message
// that follows completes that same block, so it replaces the stream rather
// than repeating it. Tool calls keep the provider's `tool_use` id from call
// to result. A subagent's traffic (`parent_tool_use_id` set) isn't the main
// turn: subagents are disabled for now, and anything that slips through is
// reported, not shown as the agent's own words.

import type { ErrorCategory, Event, Outcome } from "./protocol.js";

/** A tool's output as kept in the conversation. */
export const OUTPUT_LEN = 2000;

const excerpt = (s: string, n: number) => (s.length <= n ? s : `${s.slice(0, n - 1)}…`);

type Json = Record<string, unknown>;
const obj = (v: unknown): Json => (typeof v === "object" && v !== null ? (v as Json) : {});
const arr = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
const text = (v: unknown): string | undefined => (typeof v === "string" ? v : undefined);

function resultText(content: unknown): string {
  if (typeof content === "string") return content;
  return arr(content)
    .map((p) => text(obj(p).text))
    .filter((t): t is string => t !== undefined)
    .join("\n");
}

/** An assistant message's `error` (or an API status) as a category. */
export function category(error: unknown, status?: unknown): ErrorCategory | null {
  switch (error) {
    case "authentication_failed":
    case "oauth_org_not_allowed":
    case "account_on_hold":
    case "verification_required":
    case "billing_error":
      return "auth_required";
    case "rate_limit":
      return "rate_limited";
    case "overloaded":
    case "server_error":
      return "provider_unavailable";
    case "invalid_request":
    case "model_not_found":
      return "invalid_input";
  }
  if (status === 401 || status === 403) return "auth_required";
  if (status === 429) return "rate_limited";
  if (typeof status === "number" && status >= 500) return "provider_unavailable";
  return null;
}

export class Mapper {
  /** The turn in progress, and whether we asked it to stop. */
  turn: string | null = null;
  interrupting = false;
  /** The uuid of the user message carrying the current turn. */
  private sentUuid: string | null = null;
  private delivered = false;
  private message: string | null = null;
  private open: [string, number] | null = null;
  private blocks = new Map<string, number>();
  private synthetic = 0;
  private lastError: ErrorCategory | null = null;

  /** A turn was sent as the user message `uuid`. */
  sent(turn: string, uuid: string) {
    this.turn = turn;
    this.sentUuid = uuid;
    this.delivered = false;
    this.interrupting = false;
    this.lastError = null;
  }

  private next(prefix: string) {
    this.synthetic += 1;
    return `${prefix}${this.synthetic}`;
  }

  /** Whether `m` shows the provider has the current turn. */
  private answers(m: Json): boolean {
    if (!this.turn || this.delivered) return false;
    const uuids = [text(m.user_message_uuid), ...arr(m.user_message_uuids).map(text)].filter(Boolean);
    // Messages that name the user message they answer; any other output of
    // the turn (some don't carry the uuid) counts too.
    return uuids.length === 0 || uuids.includes(this.sentUuid ?? "");
  }

  map(message: unknown): Event[] {
    const m = obj(message);
    const out: Event[] = [];
    const type = text(m.type);
    // The provider's first answer to the turn (not our own message echoed back).
    if ((type === "assistant" || type === "stream_event" || type === "result") && this.answers(m)) {
      this.delivered = true;
      out.push({ type: "delivered", turn_id: this.turn! });
    }
    const main = m.parent_tool_use_id === null || m.parent_tool_use_id === undefined;
    switch (type) {
      case "system":
        if (m.subtype === "init" && text(m.session_id)) {
          out.push({
            type: "session",
            session_id: m.session_id as string,
            model: text(m.model) ?? "",
            claude_code_version: text(m.claude_code_version) ?? "",
            auth_source: text(m.apiKeySource) ?? "unknown",
          });
        }
        break;
      case "stream_event": {
        if (!main) break;
        const ev = obj(m.event);
        if (ev.type === "message_start") {
          this.message = text(obj(ev.message).id) ?? null;
        } else if (ev.type === "content_block_delta" && obj(ev.delta).type === "text_delta") {
          const piece = text(obj(ev.delta).text);
          if (!piece) break;
          const message = this.message ?? (this.message = this.next("msg-"));
          const block = typeof ev.index === "number" ? ev.index : 0;
          this.open = [message, block];
          out.push({ type: "text_delta", message, block, text: piece });
        }
        break;
      }
      case "assistant": {
        if (!main) {
          out.push({ type: "notice", message: "a subagent's message was left out" });
          break;
        }
        const msg = obj(m.message);
        const id = text(msg.id);
        const err = category(m.error);
        if (err) this.lastError = err;
        for (const b of arr(msg.content).map(obj)) {
          if (b.type === "text") {
            const t = text(b.text);
            if (!t || !t.trim()) continue;
            let key: [string, number];
            if (this.open && (!id || id === this.open[0])) {
              key = this.open;
              this.open = null;
            } else {
              const message = id ?? this.next("msg-");
              const n = this.blocks.get(message) ?? 0;
              this.blocks.set(message, n + 1);
              key = [message, 1000 + n];
            }
            this.message = null;
            out.push({ type: "text", message: key[0], block: key[1], text: t });
          } else if (b.type === "tool_use") {
            out.push({
              type: "tool_started",
              call: text(b.id) ?? this.next("tool-"),
              parent: null,
              tool: text(b.name) ?? "",
              input: b.input ?? {},
            });
          }
        }
        break;
      }
      case "user": {
        if (!main) break;
        for (const b of arr(obj(m.message).content).map(obj)) {
          if (b.type !== "tool_result" || !text(b.tool_use_id)) continue;
          out.push({
            type: "tool_finished",
            call: b.tool_use_id as string,
            ok: b.is_error !== true,
            output: excerpt(resultText(b.content), OUTPUT_LEN),
          });
        }
        break;
      }
      case "result": {
        if (!this.turn) {
          out.push({ type: "notice", message: "a result arrived with no turn running" });
          break;
        }
        const ok = m.subtype === "success" && m.is_error !== true;
        const aborted = String(m.terminal_reason ?? "").startsWith("aborted");
        let outcome: Outcome;
        let error: ErrorCategory | null = null;
        // An interrupt that landed is an interruption, whatever the subtype;
        // a turn that finished first (terminal_reason "completed") completed.
        if (this.interrupting && aborted) outcome = "interrupted";
        else if (ok) outcome = "completed";
        else if (m.subtype === "error_max_turns" || m.subtype === "error_max_budget_usd") outcome = "limit_reached";
        else if (
          m.subtype === "error_during_execution" &&
          (this.interrupting || String(m.terminal_reason ?? "").startsWith("aborted"))
        )
          outcome = "interrupted";
        else {
          outcome = "failed";
          error = this.lastError ?? category(undefined, m.api_error_status);
        }
        const errors = arr(m.errors).map(text).filter(Boolean).join("; ");
        out.push({
          type: "turn_finished",
          turn_id: this.turn,
          outcome,
          summary: text(m.result) ? excerpt(m.result as string, OUTPUT_LEN) : errors ? excerpt(errors, OUTPUT_LEN) : null,
          error,
          session_cost_usd: typeof m.total_cost_usd === "number" ? m.total_cost_usd : null,
        });
        this.turn = null;
        this.sentUuid = null;
        this.open = null;
        this.message = null;
        this.interrupting = false;
        break;
      }
    }
    return out;
  }
}
