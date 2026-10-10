// One run of one conversation through the Claude Agent SDK (D-057).
//
// - Turns go in through the SDK's streaming input, one at a time; each is a
//   user message with its own uuid, so the provider's answer names it.
// - Every tool call passes Otter's policy first (a mandatory `PreToolUse`
//   hook asks otterd: allow, deny, or ask). "Ask" reaches `canUseTool`,
//   which waits for otterd's answer for that exact request.
// - Interrupting stops the turn (the session stays); shutdown ends it all.
//   A cancelled wait is a denial, never an allow.
// - Subagents are off (`Task`/`Agent`), until their lineage and policy are
//   handled. Claude Code is the one the SDK bundles, never whichever is on
//   PATH.

import { randomUUID } from "node:crypto";
import {
  query as sdkQuery,
  type CanUseTool,
  type HookCallback,
  type Options,
  type PermissionResult,
  type PreToolUseHookInput,
  type Query,
  type SDKUserMessage,
} from "@anthropic-ai/claude-agent-sdk";

import { Pending } from "./interactions.js";
import { Mapper } from "./map.js";
import type { Event, InitializePayload, PolicyReplyPayload, Resolution } from "./protocol.js";

export type QueryFn = (params: { prompt: AsyncIterable<SDKUserMessage>; options?: Options }) => Query;

/** Turns waiting to go in, as an async iterable the SDK reads. */
export class InputQueue implements AsyncIterable<SDKUserMessage> {
  private items: SDKUserMessage[] = [];
  private wake: (() => void) | null = null;
  private ended = false;

  push(m: SDKUserMessage) {
    if (this.ended) throw new Error("the input has ended");
    this.items.push(m);
    this.wake?.();
  }

  end() {
    this.ended = true;
    this.wake?.();
  }

  async *[Symbol.asyncIterator](): AsyncIterator<SDKUserMessage> {
    for (;;) {
      const next = this.items.shift();
      if (next) {
        yield next;
        continue;
      }
      if (this.ended) return;
      await new Promise<void>((r) => (this.wake = r));
      this.wake = null;
    }
  }
}

/** Tools held back for now (subagents). */
export const DISALLOWED_TOOLS = ["Task", "Agent"];

export class Session {
  readonly mapper = new Mapper();
  private q: Query | null = null;
  private input = new InputQueue();
  private abort = new AbortController();
  private checks = new Pending<PolicyReplyPayload>();
  private permissions = new Pending<Resolution>();
  private n = 0;
  /** Resolves when the SDK's message stream ends. */
  done: Promise<void> = Promise.resolve();

  constructor(
    private emit: (ev: Event) => void,
    private init: InitializePayload,
    private query: QueryFn = sdkQuery as unknown as QueryFn,
  ) {}

  /** The SDK options for this run (exported for tests). */
  options(): Options {
    const i = this.init;
    return {
      cwd: i.cwd,
      env: { ...process.env },
      model: i.model,
      resume: i.resume,
      includePartialMessages: true,
      settingSources: i.setting_sources,
      systemPrompt: { type: "preset", preset: "claude_code", ...(i.instructions ? { append: i.instructions } : {}) },
      maxTurns: i.max_turns,
      maxBudgetUsd: i.max_budget_usd,
      permissionMode: "default",
      disallowedTools: DISALLOWED_TOOLS,
      abortController: this.abort,
      hooks: { PreToolUse: [{ hooks: [this.preToolUse] }] },
      canUseTool: this.canUseTool,
      stderr: (data: string) => {
        // Diagnostics, bounded, on our stderr (never the protocol).
        process.stderr.write(data.length > 4000 ? `${data.slice(0, 4000)}…\n` : data);
      },
    };
  }

  /** Start the SDK and wait until it is initialized (or `timeoutMs`). */
  async start(timeoutMs: number): Promise<void> {
    this.q = this.query({ prompt: this.input, options: this.options() });
    const q = this.q;
    this.done = (async () => {
      try {
        for await (const message of q) {
          // OTTER_WORKER_TRACE=1: how turns end, on stderr (diagnostics only).
          if (process.env.OTTER_WORKER_TRACE && message.type === "result") {
            const { usage: _u, modelUsage: _m, ...rest } = message as Record<string, unknown>;
            process.stderr.write(`trace result: ${JSON.stringify(rest).slice(0, 600)}\n`);
          }
          for (const ev of this.mapper.map(message)) this.emit(ev);
        }
      } catch (e) {
        if (!this.abort.signal.aborted) {
          this.emit({ type: "fatal", message: `the SDK stopped: ${(e as Error).message}`, category: "worker_crashed" });
        }
      }
    })();
    let timer: NodeJS.Timeout | undefined;
    const timeout = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`not initialized within ${timeoutMs} ms`)), timeoutMs);
    });
    try {
      await Promise.race([q.initializationResult(), timeout]);
    } finally {
      clearTimeout(timer);
    }
  }

  private id(prefix: string) {
    this.n += 1;
    return `${prefix}${this.n}`;
  }

  /** Otter's policy for every tool call, before it runs. */
  readonly preToolUse: HookCallback = async (input, _toolUseID, { signal }) => {
    const i = input as PreToolUseHookInput;
    const check_id = this.id("chk-");
    this.emit({ type: "policy_check", check_id, tool: i.tool_name, input: i.tool_input, tool_use_id: i.tool_use_id });
    let reply: PolicyReplyPayload;
    try {
      reply = await this.checks.wait(check_id, signal);
    } catch {
      return {
        hookSpecificOutput: {
          hookEventName: "PreToolUse",
          permissionDecision: "deny",
          permissionDecisionReason: "Otter couldn't check this tool call (the turn ended).",
        },
      };
    }
    return {
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: reply.decision,
        ...(reply.reason ? { permissionDecisionReason: reply.reason } : {}),
      },
    };
  };

  /** A tool call someone has to decide (after the policy said "ask"). */
  readonly canUseTool: CanUseTool = async (tool, input, options): Promise<PermissionResult> => {
    const request_id = options.requestId || this.id("perm-");
    this.emit({ type: "permission_request", request_id, tool, input, tool_use_id: options.toolUseID });
    let r: Resolution;
    try {
      r = await this.permissions.wait(request_id, options.signal);
    } catch {
      return { behavior: "deny", message: "The request was cancelled before anyone answered." };
    }
    switch (r.behavior) {
      case "allow":
        return { behavior: "allow", updatedInput: input };
      case "deny":
        return { behavior: "deny", message: r.message };
      case "answer":
        return { behavior: "allow", updatedInput: { ...input, answers: r.answers } };
    }
  };

  /** Send a turn. One at a time: an error while another runs. */
  sendTurn(turn_id: string, text: string) {
    if (this.mapper.turn) throw new Error(`turn ${this.mapper.turn} is still running`);
    const uuid = randomUUID();
    this.mapper.sent(turn_id, uuid);
    this.input.push({
      type: "user",
      message: { role: "user", content: text },
      parent_tool_use_id: null,
      uuid: uuid as SDKUserMessage["uuid"],
      session_id: "",
    });
  }

  /** Stop the current turn, if it is `turn_id`. */
  async interrupt(turn_id: string) {
    if (this.mapper.turn !== turn_id) throw new Error(`turn ${turn_id} isn't running`);
    this.mapper.interrupting = true;
    await this.q?.interrupt();
  }

  policyReply(p: PolicyReplyPayload): boolean {
    return this.checks.resolve(p.check_id, p);
  }

  resolve(request_id: string, r: Resolution): boolean {
    return this.permissions.resolve(request_id, r);
  }

  /** End the run: pending answers are cancelled (denied), input closes. */
  async shutdown(graceMs: number) {
    this.checks.cancelAll("shutting down");
    this.permissions.cancelAll("shutting down");
    this.input.end();
    const settled = await Promise.race([this.done.then(() => true), new Promise((r) => setTimeout(() => r(false), graceMs))]);
    if (!settled) {
      this.abort.abort();
      this.q?.close();
    }
  }
}
