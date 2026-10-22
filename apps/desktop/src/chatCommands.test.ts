import { describe, expect, it } from "vitest";
import {
  NEW_TASK_COMMANDS,
  TASK_COMMANDS,
  find,
  matches,
  newTask,
  parse,
  parseMinutes,
  slashed,
  withOption,
  type NewTaskCtx,
  type TaskCtx,
} from "./chatCommands";
import { sampleTasks } from "./taskSource";
import { HELD, commandPrefix, type Task } from "./tasks";
import type { RuntimeCapabilities } from "./conversation";

const [active, blocked, failed, done, draft] = sampleTasks(Date.parse("2026-10-22T12:00:00Z"));
const caps = (over: Partial<RuntimeCapabilities> = {}): RuntimeCapabilities => ({
  provider: "claude",
  backend: "sdk",
  available: true,
  structured_ready: false,
  features: {
    send_turn: true,
    resume: true,
    interrupt_turn: true,
    permission_requests: true,
    questions: true,
    tool_results: true,
    streaming: true,
    attachments: true,
    usage: true,
    pause: true,
    redirect: true,
  },
  message_commands: true,
  ...over,
});
const why = (name: string, ctx: TaskCtx) => find(TASK_COMMANDS, name)!.available(ctx);
const effect = (name: string, rest: string, ctx: TaskCtx) => find(TASK_COMMANDS, name)!.effect(rest, ctx);

describe("slash commands (D-190)", () => {
  it("reads the box as a command only from a / at its start, Pinyin's 、 and ／ too", () => {
    expect(parse("/pau")).toEqual({ name: "pau", rest: "", named: false });
    expect(parse("/stop use tabs\nnot commas")).toEqual({ name: "stop", rest: "use tabs\nnot commas", named: true });
    expect(parse("/")).toEqual({ name: "", rest: "", named: false });
    expect(parse("、pause")?.name).toBe("pause");
    expect(parse("／remind 30m")).toEqual({ name: "remind", rest: "30m", named: true });
    expect(slashed("、ask")).toBe("/ask");
    expect(slashed("好的、谢谢")).toBe("好的、谢谢");
    // A path or a word with a slash in it is a message.
    for (const text of ["src/main.rs is wrong", " /pause", "use A/B", "/usr/bin/env"]) expect(parse(text)).toBeNull();
  });

  it("says why each task command can't be used now, and the buttons ask the same", () => {
    const ctx = (task: Task, more: Partial<TaskCtx> = {}): TaskCtx => ({ task, caps: caps(), memory: true, ...more });
    const usable = (c: TaskCtx) => TASK_COMMANDS.filter((x) => x.available(c) === true).map((x) => x.name);
    expect(usable(ctx(active, { live: true }))).toEqual([
      "pause",
      "stop",
      "unwatch",
      "cancel",
      "ask",
      "tell",
      "revise",
      "remember",
      "remind",
      "answer-only",
      "details",
      "activity",
      "changes",
      "open",
      "memory",
    ]);
    expect(why("resume", ctx(active))).toBe("Not paused");
    expect(why("workspace", ctx(active, { live: true }))).toBe("Pause first: the agent is working");
    expect(why("delete", ctx(active))).toBe("Not while it works");
    expect(why("watch", ctx(active))).toBe("No pull request yet");
    expect(why("start", ctx(draft))).toBe(true);
    expect(why("revise", ctx(draft))).toBe("Start it first");
    expect(why("retry", ctx(failed))).toBe(true);
    expect(why("accept", ctx({ ...done, status: "review" }))).toBe(true);
    expect(why("cancel", ctx(done))).toBe("Already over");
    expect(why("pause", ctx(blocked))).toBe(true);
    // Asking needs the agent at work, on the SDK runtime.
    expect(why("ask", ctx(active))).toBe("The coding agent isn't working: ask while it works");
    expect(why("ask", ctx(active, { live: true, caps: caps({ backend: "legacy_cli" }) }))).toBe("Needs the coding agent on the SDK runtime (Settings)");
    // An otterd before D-190 can't take a message command.
    expect(why("remember", ctx(active, { caps: caps({ message_commands: undefined }) }))).toBe("Update otterd on this host to use this");
    expect(why("stop", ctx(active, { live: true, caps: caps({ features: { ...caps().features, redirect: false } }) }))).toBe(
      "This host's coding agent can't be stopped mid-turn",
    );
    // Its own workspace only while it waits for one someone else holds.
    const waiting: Task = { ...draft, status: "blocked", status_reason: `Waiting for demo: “Fix the export”${HELD}` };
    expect(why("own-workspace", ctx(waiting))).toBe(true);
    expect(why("own-workspace", ctx(blocked))).toBe("Not waiting for a workspace");
  });

  it("turns what follows a command into what it does, or says what's missing", () => {
    const ctx: TaskCtx = { task: { ...draft, workspace_id: "ws_a" }, workspaces: [{ id: "ws_a", name: "api" }, { id: "ws_b", name: "Web" }] };
    expect(effect("workspace", "web", ctx)).toEqual({ kind: "act", action: { action: "set_workspace", workspace: "ws_b" } });
    expect(effect("workspace", "api", ctx)).toEqual({ problem: "It works there already" });
    expect(effect("workspace", "ios", ctx)).toEqual({ problem: "No workspace “ios” on this host" });
    expect(effect("stop", "  ", ctx)).toEqual({ problem: "Say what it should do instead" });
    expect(effect("remind", "1h30m check the deploy", ctx)).toEqual({
      kind: "send",
      command: { name: "remind", in_minutes: 90 },
      words: "check the deploy",
    });
    expect(effect("remind", "soon", ctx)).toEqual({ problem: "Say when: 15m, 30m, 1h…" });
    expect(effect("remind", "8d", ctx)).toEqual({ problem: "Say when: 15m, 30m, 1h…" });
    expect(effect("remind", `${8 * 24 * 60}`, ctx)).toEqual({ problem: "At most a week from now" });
    expect(effect("watch", "ci when it's green", ctx)).toEqual({ kind: "send", command: { name: "watch_ci" }, words: "when it's green" });
    expect(effect("answer-only", "on", ctx)).toEqual({ kind: "send", command: { name: "answer_only", on: true }, words: "" });
    expect(effect("answer-only", "off", ctx)).toEqual({ problem: "It isn't answer-only" });
    expect(effect("accept", "", { task: { ...done, status: "review", gates: [{ name: "CI", status: "failed" }] } })).toEqual({
      kind: "act",
      action: { action: "accept", override_gates: true },
    });
    expect(find(TASK_COMMANDS, "accept")!.confirm!({ task: { ...done, status: "review", gates: [{ name: "CI", status: "passed" }] } })).toBeUndefined();
  });

  it("writes a command as otterd does", () => {
    expect(commandPrefix({ name: "remind", in_minutes: 90 })).toBe("/remind 1h30m");
    expect(commandPrefix({ name: "remind", in_minutes: 120 })).toBe("/remind 2h");
    expect(commandPrefix({ name: "watch_ci" })).toBe("/watch ci");
    expect(commandPrefix({ name: "answer_only", on: false })).toBe("/answer-only off");
    expect(commandPrefix({ name: "tell" })).toBe("/tell");
    expect([parseMinutes("15m"), parseMinutes("1h"), parseMinutes("2h15m"), parseMinutes("45"), parseMinutes("1 h")]).toEqual([15, 60, 135, 45, undefined]);
  });

  it("lists every command by heading, the usable first; a query, the best match first", () => {
    const ctx: TaskCtx = { task: active, live: true, caps: caps(), memory: true };
    const all = matches(TASK_COMMANDS, "", ctx);
    expect(all).toHaveLength(TASK_COMMANDS.length);
    expect([...new Set(all.map((r) => r.command.group))]).toEqual(["Task", "Tell", "Show"]);
    const task = all.filter((r) => r.command.group === "Task");
    const usable = task.filter((r) => !r.why).length;
    expect(task.slice(usable).every((r) => r.why)).toBe(true);
    expect(matches(TASK_COMMANDS, "re", ctx).map((r) => [r.command.name, r.why ?? "ok"])).toEqual([
      ["revise", "ok"],
      ["remember", "ok"],
      ["remind", "ok"],
      ["resume", "Not paused"],
      ["retry", "Hasn't failed"],
    ]);
    // A word of what it does finds it too.
    expect(matches(TASK_COMMANDS, "memory", ctx).map((r) => r.command.name)).toEqual(["memory", "remember"]);
  });

  it("makes a new task's conditions from the empty page's commands", () => {
    const ctx: NewTaskCtx = {
      hosts: ["mac", "gpu"],
      workspaces: { mac: [{ id: "ws_a", name: "api", repository: "git@h:o/api.git" }], gpu: [{ id: "ws_b", name: "api" }, { id: "ws_c", name: "trainer" }] },
    };
    const run = (name: string, rest: string) => find(NEW_TASK_COMMANDS, name)!.effect(rest, ctx);
    expect(run("in", "trainer")).toEqual({ kind: "in", host: "gpu", workspace: "ws_c", label: "trainer" });
    expect(run("in", "api")).toEqual({ problem: "“api” is on several hosts: say api@mac or api@gpu" });
    expect(run("in", "api@gpu")).toEqual({ kind: "in", host: "gpu", workspace: "ws_b", label: "api@gpu" });
    expect(find(NEW_TASK_COMMANDS, "repo")!.choices!(ctx)).toEqual([{ value: "git@h:o/api.git", label: "git@h:o/api.git" }]);
    expect(find(NEW_TASK_COMMANDS, "host")!.available({ ...ctx, hosts: ["mac"] })).toBe("Only one host");
    // Where it works is one thing; the latest says.
    let o = withOption([], { kind: "scratch" });
    o = withOption(o, { kind: "answer_only" });
    o = withOption(o, { kind: "repo", repository: "git@h:o/api.git" });
    expect(o.map((x) => x.kind)).toEqual(["answer_only", "repo"]);
    expect(newTask(o)).toEqual({ host: undefined, workspace: { new: { repository: "git@h:o/api.git" } }, answerOnly: true, start: true });
    // A workspace brings its host; another host drops it.
    o = withOption([{ kind: "host", host: "mac" }], { kind: "in", host: "gpu", workspace: "ws_c", label: "trainer" });
    expect(newTask(o)).toMatchObject({ host: "gpu", workspace: "ws_c" });
    o = withOption(o, { kind: "host", host: "mac" });
    expect(newTask(withOption(o, { kind: "draft" }))).toEqual({ host: "mac", workspace: undefined, answerOnly: false, start: false });
  });
});
