import { describe, expect, it } from "vitest";
import {
  findItem,
  groupAutomations,
  localTime,
  openWatches,
  policyText,
  scheduleText,
  triggerLine,
  watchGlyph,
  watchLine,
  watchState,
  watchTarget,
  type PlacedTrigger,
  type Trigger,
} from "./automations";
import type { PlacedTask, Task, Watch } from "./tasks";

const NOW = Date.parse("2026-10-22T12:00:00Z");
const at = (minutes: number) => new Date(NOW + minutes * 60_000).toISOString();

const watch = (over: Partial<Watch>): Watch => ({
  id: "wch_1",
  kind: "ci",
  target: { pr: 171, repo: "alvingaopro/otter" },
  label: "CI on #171",
  what: "tell me when CI is done",
  created_at: at(-10),
  deadline: at(110),
  status: "waiting",
  findings: 0,
  ...over,
});

const task = (id: string, watches: Watch[]): PlacedTask => ({
  host: "mac",
  key: `mac/${id}`,
  task: {
    id,
    title: `Task ${id}`,
    request: "",
    status: "done",
    requirements: [],
    acceptance: [],
    steps: [],
    runs: [],
    messages: [],
    decisions: [],
    evidence: [],
    budget: { max_iterations: 6, max_minutes: 120, iterations_used: 0 },
    created_at: at(-60),
    updated_at: at(-1),
    watches,
  } as Task,
});

const trigger = (over: Partial<Trigger>): PlacedTrigger => {
  const t: Trigger = {
    id: "trg_1",
    name: "nightly",
    workspace_id: "ws_1",
    title: "Nightly",
    request: "Run the suite",
    schedule: { kind: "daily", at: "20:20" },
    policy: {},
    enabled: true,
    created_at: at(-1000),
    next_at: at(300),
    history: [],
    ...over,
  };
  return { host: "mac", key: `mac/${t.id}`, trigger: t };
};

describe("automations", () => {
  it("groups watches waiting by the soonest deadline, ended ones latest first, triggers on before paused", () => {
    const tasks = [
      task("ft_a", [
        watch({ id: "w_late", deadline: at(100) }),
        watch({ id: "w_old", status: "reported", ended_at: at(-50), finding: { at: at(-50), outcome: "passed", text: "all 3 checks passed" } }),
      ]),
      task("ft_b", [
        watch({ id: "w_soon", kind: "time", deadline: at(5), status: "firing", finding: { at: at(0), outcome: "due", text: "the time came" } }),
        watch({ id: "w_new", status: "cancelled", ended_at: at(-5) }),
      ]),
    ];
    const g = groupAutomations(tasks, [
      trigger({ id: "trg_off", name: "deps", enabled: false, next_at: undefined }),
      trigger({ id: "trg_later", name: "b", next_at: at(600) }),
      trigger({ id: "trg_sooner", name: "a", next_at: at(60) }),
    ]);
    expect(g.watching.map((w) => w.watch.id)).toEqual(["w_soon", "w_late"]);
    expect(g.history.map((w) => w.watch.id)).toEqual(["w_new", "w_old"]);
    expect(g.scheduled.map((t) => t.trigger.id)).toEqual(["trg_sooner", "trg_later", "trg_off"]);
    // Selected by key: a watch's is its task's and its own, a trigger's is its own.
    expect(findItem(g, "mac/ft_a/w_old")?.kind).toBe("watch");
    expect(findItem(g, "mac/trg_off")?.kind).toBe("trigger");
    expect(findItem(g, "mac/nothing")).toBeUndefined();
    expect(openWatches(tasks[1].task).map((w) => w.id)).toEqual(["w_soon"]);
  });

  it("says where a watch stands and how it ended", () => {
    expect(watchState(watch({}))).toBe("watching");
    expect(watchState(watch({ status: "cancelled" }))).toBe("stopped");
    expect(watchState(watch({ status: "firing", finding: { at: at(0), outcome: "failed", text: "x" } }))).toBe("failed — telling you");
    expect(watchState(watch({ status: "reported", finding: { at: at(0), outcome: "stuck", text: "x" } }))).toBe("can't go on");
    expect(watchGlyph(watch({}))).toBe("working");
    expect(watchGlyph(watch({ status: "reported", finding: { at: at(0), outcome: "passed", text: "" } }))).toBe("done");
    expect(watchGlyph(watch({ status: "reported", finding: { at: at(0), outcome: "failed", text: "" } }))).toBe("failed");
    expect(watchGlyph(watch({ status: "reported", finding: { at: at(0), outcome: "expired", text: "" } }))).toBe("needs");
    expect(watchGlyph(watch({ status: "cancelled" }))).toBe("idle");
    expect(watchTarget(watch({}))).toBe("alvingaopro/otter #171");
    expect(watchTarget(watch({ kind: "time", target: { in_minutes: 30 } }))).toBeUndefined();
    const p = task("ft_a", [watch({})]);
    expect(watchLine({ key: "k", host: "mac", task: p, watch: watch({}) }, NOW)).toBe(`Task ft_a · until ${localTime(at(110), NOW)}`);
    expect(watchLine({ key: "k", host: "mac", task: p, watch: watch({ status: "cancelled" }) }, NOW)).toBe("Task ft_a · stopped");
  });

  it("shows times in this Mac's zone, with the day when it isn't today", () => {
    const today = localTime(at(30), NOW);
    expect(today).toMatch(/^\d{2}:\d{2}$/);
    const another = localTime(at(3 * 24 * 60), NOW);
    expect(another).toMatch(/\d{2}:\d{2}$/);
    expect(another.length).toBeGreaterThan(today.length);
    expect(localTime(undefined, NOW)).toBe("");
    expect(localTime("nonsense", NOW)).toBe("");
  });

  it("says a trigger's schedule and policy plainly", () => {
    expect(scheduleText({ kind: "every", minutes: 30 })).toBe("every 30 min");
    expect(scheduleText({ kind: "every", minutes: 720 })).toBe("every 12 h");
    expect(scheduleText({ kind: "daily", at: "20:20" })).toBe("daily at 20:20 UTC");
    expect(scheduleText({ kind: "manual" })).toBe("by hand");
    expect(triggerLine(trigger({ enabled: false }).trigger, NOW)).toBe("daily at 20:20 UTC · paused");
    expect(triggerLine(trigger({ schedule: { kind: "manual" }, next_at: undefined }).trigger, NOW)).toBe("by hand");
    expect(policyText({})).toEqual(["asks before publishing"]);
    expect(policyText({ publish: true, max_iterations: 2, max_minutes: 30, wait_limit_secs: 1800 })).toEqual([
      "publishes without asking",
      "at most 2 attempts",
      "at most 30 min of work",
      "pauses after waiting 30 min for you",
    ]);
  });
});
