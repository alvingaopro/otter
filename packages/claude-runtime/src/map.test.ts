import assert from "node:assert/strict";
import { test } from "node:test";

import { Mapper, OUTPUT_LEN, category } from "./map.js";

const sent = () => {
  const m = new Mapper();
  m.sent("turn_1", "u-1");
  return m;
};

test("streamed and whole text are one block, said once", () => {
  const m = sent();
  assert.deepEqual(m.map({ type: "stream_event", parent_tool_use_id: null, user_message_uuid: "u-1", event: { type: "message_start", message: { id: "msg_A" } } }), [
    { type: "delivered", turn_id: "turn_1" },
  ]);
  const delta = (t: string) => ({ type: "stream_event", parent_tool_use_id: null, event: { type: "content_block_delta", index: 1, delta: { type: "text_delta", text: t } } });
  assert.deepEqual(m.map(delta("Hel")), [{ type: "text_delta", message: "msg_A", block: 1, text: "Hel" }]);
  m.map(delta("lo"));
  assert.deepEqual(
    m.map({ type: "assistant", parent_tool_use_id: null, message: { id: "msg_A", content: [{ type: "text", text: "Hello" }] } }),
    [{ type: "text", message: "msg_A", block: 1, text: "Hello" }],
  );
  // A second whole block of the same message gets a key of its own.
  const [again] = m.map({ type: "assistant", parent_tool_use_id: null, message: { id: "msg_A", content: [{ type: "text", text: "More" }] } });
  assert.equal(again?.type, "text");
  assert.notEqual((again as { block: number }).block, 1);
});

test("delivery is the provider answering this turn, once", () => {
  const m = sent();
  // Another turn's echo doesn't count.
  assert.deepEqual(m.map({ type: "assistant", parent_tool_use_id: null, user_message_uuids: ["u-0"], message: { content: [] } }), []);
  const evs = m.map({ type: "assistant", parent_tool_use_id: null, user_message_uuids: ["u-1"], message: { content: [] } });
  assert.deepEqual(evs, [{ type: "delivered", turn_id: "turn_1" }]);
  assert.deepEqual(m.map({ type: "assistant", parent_tool_use_id: null, message: { content: [] } }), []);
});

test("tool calls keep their ids from call to result; a failed result is a failure", () => {
  const m = sent();
  const call = m.map({ type: "assistant", parent_tool_use_id: null, message: { id: "msg_B", content: [{ type: "tool_use", id: "toolu_7", name: "Bash", input: { command: "make test" } }] } });
  assert.deepEqual(call.at(-1), { type: "tool_started", call: "toolu_7", parent: null, tool: "Bash", input: { command: "make test" } });
  const two = m.map({ type: "assistant", parent_tool_use_id: null, message: { id: "msg_C", content: [{ type: "tool_use", id: "toolu_8", name: "Read", input: { file_path: "a" } }] } });
  assert.equal((two[0] as { call: string }).call, "toolu_8");
  assert.deepEqual(
    m.map({ type: "user", parent_tool_use_id: null, message: { content: [{ type: "tool_result", tool_use_id: "toolu_7", is_error: true, content: [{ type: "text", text: "1 failed" }] }] } }),
    [{ type: "tool_finished", call: "toolu_7", ok: false, output: "1 failed" }],
  );
  const long = m.map({ type: "user", parent_tool_use_id: null, message: { content: [{ type: "tool_result", tool_use_id: "toolu_8", content: "x".repeat(5000) }] } });
  assert.equal((long[0] as { output: string }).output.length, OUTPUT_LEN);
});

test("a subagent's words aren't the agent's own", () => {
  const m = sent();
  const evs = m.map({ type: "assistant", parent_tool_use_id: "toolu_9", message: { content: [{ type: "text", text: "sub" }] } });
  assert.ok(evs.every((e) => e.type !== "text"));
  assert.ok(evs.some((e) => e.type === "notice"));
});

test("results end the turn with the right outcome; EOF never does", () => {
  const done = (r: Record<string, unknown>, interrupting = false) => {
    const m = sent();
    m.interrupting = interrupting;
    m.map({ type: "assistant", parent_tool_use_id: null, message: { content: [] } });
    return m.map({ type: "result", parent_tool_use_id: null, ...r }).find((e) => e.type === "turn_finished") as Record<string, unknown>;
  };
  assert.equal(done({ subtype: "success", is_error: false, result: "ok", total_cost_usd: 0.01 }).outcome, "completed");
  assert.equal(done({ subtype: "success", is_error: false, result: "ok", total_cost_usd: 0.01 }).session_cost_usd, 0.01);
  assert.equal(done({ subtype: "error_max_turns", is_error: true }).outcome, "limit_reached");
  assert.equal(done({ subtype: "error_max_budget_usd", is_error: true }).outcome, "limit_reached");
  assert.equal(done({ subtype: "error_during_execution", is_error: true }, true).outcome, "interrupted");
  // An interrupt during a tool call may end as a "success" that was aborted.
  assert.equal(done({ subtype: "success", is_error: false, result: "", terminal_reason: "aborted_tools" }, true).outcome, "interrupted");
  // A turn that finished before the interrupt landed did complete.
  assert.equal(done({ subtype: "success", is_error: false, result: "ok", terminal_reason: "completed" }, true).outcome, "completed");
  // As observed on SDK 0.3.285: an interrupt ends the turn this way.
  assert.equal(done({ subtype: "error_during_execution", is_error: true, terminal_reason: "aborted_streaming" }).outcome, "interrupted");
  const failed = done({ subtype: "error_during_execution", is_error: true, api_error_status: 429, errors: ["slow down"] });
  assert.equal(failed.outcome, "failed");
  assert.equal(failed.error, "rate_limited");
  assert.equal(failed.summary, "slow down");
  // No turn running: nothing finishes.
  const idle = new Mapper();
  assert.ok(idle.map({ type: "result", subtype: "success" }).every((e) => e.type !== "turn_finished"));
});

test("an authentication failure is told apart", () => {
  const m = sent();
  m.map({ type: "assistant", parent_tool_use_id: null, error: "authentication_failed", message: { content: [{ type: "text", text: "Not logged in" }] } });
  const end = m.map({ type: "result", subtype: "success", is_error: true, result: "Not logged in" }).find((e) => e.type === "turn_finished");
  assert.equal((end as { error: string }).error, "auth_required");
  assert.equal(category("overloaded"), "provider_unavailable");
});

test("the session says which Claude Code and whose credentials, not the credentials", () => {
  const m = new Mapper();
  assert.deepEqual(
    m.map({ type: "system", subtype: "init", session_id: "s-1", model: "claude-opus-5-5", claude_code_version: "2.1.285", apiKeySource: "none" }),
    [{ type: "session", session_id: "s-1", model: "claude-opus-5-5", claude_code_version: "2.1.285", auth_source: "none" }],
  );
});
