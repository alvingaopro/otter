import assert from "node:assert/strict";
import { test } from "node:test";

import type { Options, Query, SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";

import { Pending } from "./interactions.js";
import { Frames, bundledClaude, check, worker } from "./main.js";
import { MAX_FRAME, MAX_IMAGE_BYTES, PROFILES, PROTOCOL_VERSION, parseCommand } from "./protocol.js";
import { DISALLOWED_TOOLS, SEARCH_TOOLS, Session, otterToolName, shapeOf } from "./sdk.js";

const env = (type: string, payload: unknown, request_id = `r-${type}`) =>
  JSON.stringify({ protocol_version: PROTOCOL_VERSION, request_id, run_id: "run_1", generation: 1, type, payload });

const init = { conversation_id: "conv_1", cwd: "/w", setting_sources: ["project"] };

test("commands are validated before anything reaches the SDK", () => {
  assert.equal(parseCommand(env("initialize", init)).ok, true);
  assert.equal(parseCommand("not json").ok, false);
  assert.equal(parseCommand(env("send_turn", { turn_id: "t" })).ok, false);
  assert.equal(parseCommand(env("initialize", { ...init, setting_sources: ["everything"] })).ok, false);
  assert.equal(parseCommand(env("resolve_interaction", { request_id: "p", resolution: { behavior: "deny" } })).ok, false);
  assert.equal(parseCommand(env("resolve_interaction", { request_id: "p", resolution: { behavior: "answer", answers: { "Which?": "a, b" } } })).ok, true);
  const wrong = parseCommand(JSON.stringify({ protocol_version: 99, request_id: "x", run_id: "r", generation: 1, type: "shutdown", payload: {} }));
  assert.equal(wrong.ok, false);
  assert.match((wrong as { error: string }).error, /protocol/);
  assert.equal(parseCommand(env("teleport", {})).ok, false);
});

test("a turn's pictures are checked: type, base64, count and size", () => {
  const png = Buffer.from("png bytes").toString("base64");
  const turn = (images: unknown) => parseCommand(env("send_turn", { turn_id: "t", text: "see", images })).ok;
  assert.equal(turn(undefined), true);
  assert.equal(turn([{ media_type: "image/png", data: png }]), true);
  assert.equal(turn("nope"), false);
  assert.equal(turn([{ media_type: "image/gif", data: png }]), false, "a type we don't take");
  assert.equal(turn([{ media_type: "image/png", data: "not base64!" }]), false);
  assert.equal(turn([{ media_type: "image/png", data: "" }]), false);
  assert.equal(turn(Array(5).fill({ media_type: "image/png", data: png })), false, "five");
  const big = Buffer.alloc(MAX_IMAGE_BYTES + 1).toString("base64");
  assert.equal(turn([{ media_type: "image/png", data: big }]), false, "one over the limit");
  const large = Buffer.alloc(600 * 1024).toString("base64");
  assert.equal(turn(Array(4).fill({ media_type: "image/png", data: large })), false, "over the limit together");
});

test("a turn with pictures reaches Claude as image blocks before the text", async () => {
  const png = Buffer.from("png bytes").toString("base64");
  let seen: unknown;
  const { out } = await drive(
    async function* (_text, _uuid, _opts, content) {
      seen = content;
      yield { type: "result", subtype: "success", is_error: false, result: "ok", total_cost_usd: 0 };
    },
    async (send, out) => {
      send(env("initialize", init));
      await until(out, (e) => e.type === "ready");
      send(env("send_turn", { turn_id: "turn_1", text: "match this", images: [{ media_type: "image/png", data: png }] }));
      await until(out, (e) => e.type === "turn_finished");
      send(env("shutdown", {}));
    },
  );
  assert.ok(out.length > 0);
  assert.deepEqual(seen, [
    { type: "image", source: { type: "base64", media_type: "image/png", data: png } },
    { type: "text", text: "match this" },
  ]);
});

test("a turn with a bad picture is refused as invalid_input and never reaches the SDK", async () => {
  let reached = false;
  const { out } = await drive(
    async function* () {
      reached = true;
      yield { type: "result", subtype: "success", is_error: false, result: "ok", total_cost_usd: 0 };
    },
    async (send, out) => {
      send(env("initialize", init));
      await until(out, (e) => e.type === "ready");
      send(env("send_turn", { turn_id: "turn_1", text: "see", images: [{ media_type: "image/gif", data: "AAAA" }] }, "r-bad"));
      await until(out, (e) => e.type === "nack" && e.request_id === "r-bad");
      send(env("shutdown", {}));
    },
  );
  const nack = out.find((e) => (e as Record<string, unknown>).type === "nack") as Record<string, unknown>;
  assert.equal(nack.category, "invalid_input");
  assert.match(String(nack.error), /image/);
  assert.equal(reached, false);
});

test("frames over the limit are dropped and reported, the next one still read", () => {
  const lines: string[] = [];
  let tooLarge = 0;
  const f = new Frames((l) => lines.push(l), () => tooLarge++);
  f.push("a\nb");
  f.push("c\n");
  f.push("x".repeat(MAX_FRAME + 10));
  f.push("tail of the big one\nok\n");
  assert.deepEqual(lines, ["a", "bc", "ok"]);
  assert.equal(tooLarge, 1);
});

test("a cancelled wait is rejected, never resolved", async () => {
  const p = new Pending<string>();
  const a = new AbortController();
  const waiting = p.wait("x", a.signal);
  a.abort();
  await assert.rejects(waiting);
  assert.equal(p.resolve("x", "late"), false);
  const w2 = p.wait("y");
  p.cancelAll("bye");
  await assert.rejects(w2);
});

test("the SDK is asked for isolation-safe options", () => {
  const s = new Session(() => {}, { ...init, model: "sonnet", resume: "s-1", instructions: "be brief", max_turns: 30, setting_sources: ["project"] });
  const o = s.options();
  assert.equal(o.model, "sonnet");
  assert.equal(o.resume, "s-1");
  assert.equal(o.includePartialMessages, true);
  assert.deepEqual(o.settingSources, ["project"]);
  assert.deepEqual(o.disallowedTools, DISALLOWED_TOOLS);
  assert.deepEqual(o.allowedTools, SEARCH_TOOLS, "the search tools a native build leaves out unless asked");
  assert.equal(o.permissionMode, "default");
  assert.equal(o.perTaskStopAffordance, true, "an interrupt spares background work");
  assert.equal(o.pathToClaudeCodeExecutable, undefined, "the bundled Claude Code, not PATH's");
  assert.ok(o.hooks?.PreToolUse?.[0]?.hooks.length === 1);
  assert.deepEqual(o.systemPrompt, { type: "preset", preset: "claude_code", append: "be brief" });
});

/** A stand-in for the SDK's `query`: replays scripted messages per turn. */
function fakeQuery(script: (text: string, uuid: string, opts: Options, content: unknown) => AsyncGenerator<unknown>) {
  return ({ prompt, options }: { prompt: AsyncIterable<SDKUserMessage>; options?: Options }) => {
    async function* run() {
      yield { type: "system", subtype: "init", session_id: "s-9", model: "m", claude_code_version: "2.1.285", apiKeySource: "none" };
      for await (const m of prompt) {
        yield* script(String((m.message as { content: string }).content), String(m.uuid), options!, m.message.content);
      }
    }
    const gen = run() as unknown as Query;
    Object.assign(gen, {
      initializationResult: async () => ({}),
      interrupt: async () => undefined,
      close: () => {},
    });
    return gen;
  };
}

async function drive(script: Parameters<typeof fakeQuery>[0], commands: (send: (s: string) => void, out: unknown[]) => Promise<void>) {
  const out: unknown[] = [];
  let exited = -1;
  const w = worker(
    (s) => out.push(JSON.parse(s)),
    (code) => (exited = code),
    fakeQuery(script),
  );
  const send = (s: string) => w.input(`${s}\n`);
  await commands(send, out);
  return { out, exited: () => exited };
}

const until = async (out: unknown[], pred: (e: Record<string, unknown>) => boolean) => {
  for (let i = 0; i < 200; i++) {
    const hit = out.find((e) => pred(e as Record<string, unknown>));
    if (hit) return hit as Record<string, unknown>;
    await new Promise((r) => setTimeout(r, 5));
  }
  throw new Error(`never saw it; got ${JSON.stringify(out)}`);
};

test("initialize, a turn, a policy check, a two-question form and a clean shutdown", async () => {
  const { out, exited } = await drive(
    async function* (text, uuid, opts) {
      yield { type: "assistant", parent_tool_use_id: null, user_message_uuids: [uuid], message: { id: "m1", content: [{ type: "tool_use", id: "tu1", name: "AskUserQuestion", input: {} }] } };
      // The SDK runs the mandatory hook, then asks for the form.
      const hook = opts.hooks!.PreToolUse![0]!.hooks[0]!;
      const verdict = await hook({ hook_event_name: "PreToolUse", tool_name: "AskUserQuestion", tool_input: {}, tool_use_id: "tu1", session_id: "s-9", transcript_path: "", cwd: "/w" } as never, "tu1", { signal: new AbortController().signal });
      const decision = (verdict as { hookSpecificOutput: { permissionDecision: string } }).hookSpecificOutput.permissionDecision;
      const input = { questions: [{ question: "Which format?" }, { question: "Which columns?" }] };
      const answer = await opts.canUseTool!("AskUserQuestion", input, { signal: new AbortController().signal, toolUseID: "tu1", requestId: "perm-x" });
      yield { type: "assistant", parent_tool_use_id: null, message: { id: "m2", content: [{ type: "text", text: `${decision}: ${JSON.stringify((answer as unknown as { updatedInput: { answers: unknown } }).updatedInput.answers)}` }] } };
      yield { type: "result", subtype: "success", is_error: false, result: `done ${text}`, total_cost_usd: 0.02 };
    },
    async (send, out) => {
      send(env("send_turn", { turn_id: "t0", text: "too early" }, "r-early"));
      await until(out, (e) => e.type === "nack" && e.request_id === "r-early");
      send(env("initialize", init));
      await until(out, (e) => e.type === "ready");
      send(env("send_turn", { turn_id: "turn_1", text: "export" }));
      const check = await until(out, (e) => e.type === "policy_check");
      assert.equal(check.tool, "AskUserQuestion");
      send(env("policy_reply", { check_id: check.check_id, decision: "ask" }));
      const req = await until(out, (e) => e.type === "permission_request");
      assert.equal(req.request_id, "perm-x");
      assert.equal(req.tool_use_id, "tu1");
      // Each question its own answer.
      send(env("resolve_interaction", { request_id: "perm-x", resolution: { behavior: "answer", answers: { "Which format?": "CSV", "Which columns?": "name, time" } } }));
      // A second answer to the same request is refused.
      send(env("resolve_interaction", { request_id: "perm-x", resolution: { behavior: "allow" } }, "r-again"));
      await until(out, (e) => e.type === "turn_finished");
      send(env("shutdown", {}));
    },
  );
  const types = out.map((e) => (e as { type: string }).type);
  assert.ok(types.indexOf("ready") < types.indexOf("delivered"), "nothing goes to Claude before ready");
  const said = out.find((e) => (e as { type: string; text?: string }).type === "text") as { text: string };
  assert.equal(said.text, 'ask: {"Which format?":"CSV","Which columns?":"name, time"}');
  const fin = out.find((e) => (e as { type: string }).type === "turn_finished") as Record<string, unknown>;
  assert.deepEqual([fin.turn_id, fin.outcome, fin.session_cost_usd], ["turn_1", "completed", 0.02]);
  assert.ok(out.some((e) => (e as Record<string, unknown>).type === "nack" && (e as Record<string, unknown>).request_id === "r-again"));
  for (let i = 0; i < 100 && exited() < 0; i++) await new Promise((r) => setTimeout(r, 5));
  assert.equal(exited(), 0);
});

test("one turn at a time, and a shutdown denies what is still waiting", async () => {
  let denied: unknown;
  await drive(
    async function* (_text, uuid, opts) {
      yield { type: "assistant", parent_tool_use_id: null, user_message_uuids: [uuid], message: { content: [] } };
      denied = await opts.canUseTool!("Bash", { command: "rm -rf build" }, { signal: new AbortController().signal, toolUseID: "tu2", requestId: "perm-y" });
    },
    async (send, out) => {
      send(env("initialize", init));
      await until(out, (e) => e.type === "ready");
      send(env("send_turn", { turn_id: "turn_1", text: "a" }));
      send(env("send_turn", { turn_id: "turn_2", text: "b" }, "r-second"));
      const n = await until(out, (e) => e.type === "nack" && e.request_id === "r-second");
      assert.match(String(n.error), /still running/);
      await until(out, (e) => e.type === "permission_request");
      send(env("shutdown", {}));
      for (let i = 0; i < 100 && !denied; i++) await new Promise((r) => setTimeout(r, 5));
    },
  );
  assert.equal((denied as { behavior: string }).behavior, "deny");
});

test("--check says what this worker is without starting anything", () => {
  const c = check();
  assert.equal(c.protocol_version, 1);
  assert.match(c.sdk_version, /^\d+\.\d+\.\d+$/);
  assert.equal(c.node_supported, Number(process.versions.node.split(".")[0]) >= 20);
  // Claude Code is an optional per-platform package (CI leaves it out);
  // there's never one for a platform the SDK doesn't ship.
  assert.equal(typeof c.claude_code, "boolean");
  assert.equal(bundledClaude("aix", "ppc64"), null);
});

const getTask = { name: "get_task", description: "Where the task stands.", input_schema: { type: "object", properties: {} } };
const otterInit = { conversation_id: "otter_1", cwd: "/w", setting_sources: [], profile: "otter", otter_tools: [getTask], instructions: "You are Otter." };

test("an otter run is asked for with its tools, and nothing else", () => {
  assert.equal(parseCommand(env("initialize", otterInit)).ok, true);
  assert.equal(parseCommand(env("initialize", { ...otterInit, otter_tools: [] })).ok, false);
  assert.equal(parseCommand(env("initialize", { ...otterInit, otter_tools: [{ ...getTask, name: "Bash" }] })).ok, false);
  assert.equal(parseCommand(env("initialize", { ...otterInit, setting_sources: ["project"] })).ok, false, "no settings");
  assert.equal(parseCommand(env("initialize", { ...init, otter_tools: [getTask] })).ok, false, "tools only for an otter run");
  assert.equal(parseCommand(env("initialize", { ...init, profile: "root" })).ok, false);
  assert.equal(parseCommand(env("tool_result", { call_id: "call-1", output: "{}" })).ok, true);
  assert.equal(parseCommand(env("tool_result", { call_id: "call-1" })).ok, false);
});

test("Otter's turn has none of Claude Code's tools, only its own", () => {
  const o = new Session(() => {}, otterInit as never).options();
  assert.deepEqual(o.tools, [], "no built-in tools");
  assert.deepEqual(Object.keys(o.mcpServers ?? {}), ["otter"]);
  assert.equal(o.strictMcpConfig, true);
  assert.deepEqual(o.allowedTools, [otterToolName("get_task")]);
  assert.deepEqual(o.settingSources, []);
  assert.equal(o.systemPrompt, "You are Otter.");
  assert.equal(o.hooks, undefined, "no coding policy hook: nothing to check but Otter's tools");
  assert.deepEqual(o.disallowedTools, DISALLOWED_TOOLS);
});

test("a JSON schema becomes the shape the model is told", () => {
  const shape = shapeOf({
    type: "object",
    properties: { limit: { type: "integer" }, message_ids: { type: "array", items: { type: "string" } } },
    required: ["message_ids"],
  });
  assert.equal(shape.limit!.safeParse(undefined).success, true, "optional");
  assert.equal(shape.limit!.safeParse(2.5).success, false, "an integer");
  assert.equal(shape.message_ids!.safeParse(undefined).success, false, "required");
  assert.equal(shape.message_ids!.safeParse(["msg_1"]).success, true);
});

test("each of Otter's tool calls goes to otterd, and its answer back", async () => {
  const out: Record<string, unknown>[] = [];
  const s = new Session((e) => out.push(e as never), otterInit as never);
  const answered = s.otterCall("get_task", {});
  const call = await until(out, (e) => e.type === "tool_call");
  assert.deepEqual([call.tool, call.input], ["get_task", {}]);
  assert.equal(s.toolResult({ call_id: "nope", output: "x" }), false);
  assert.equal(s.toolResult({ call_id: String(call.call_id), output: '{"ok":true}' }), true);
  assert.deepEqual(await answered, { content: [{ type: "text", text: '{"ok":true}' }] });
  // A turn that ends first answers with an error, never hangs.
  const late = s.otterCall("get_task", {});
  await s.shutdown(10);
  const text = (await late).content[0]!.text;
  assert.match(text, /"ok":false/);
});

test("a worker says which runs it can do", async () => {
  const { out } = await drive(
    async function* () {},
    async (send, out) => {
      send(env("initialize", otterInit));
      const ready = await until(out, (e) => e.type === "ready");
      assert.deepEqual(ready.profiles, [...PROFILES]);
      send(env("tool_result", { call_id: "call-9", output: "{}" }, "r-stray"));
      await until(out, (e) => e.type === "nack" && e.request_id === "r-stray");
      send(env("shutdown", {}));
    },
  );
  assert.ok(out.length > 0);
  assert.deepEqual(check().profiles, ["coding", "otter"]);
});
