import { describe, expect, it } from "vitest";
import { sampleTasks } from "./taskSource";
import { inboxGroups, inboxItems, needCount, newNeeds, trayTasks } from "./inboxModel";
import { workspace } from "./inboxFixtures";

const NOW = Date.parse("2026-10-09T12:00:00Z");

describe("the inbox", () => {
  const [active, blocked, failed, done, draft] = sampleTasks(NOW);
  const tasks = [active, blocked, failed, done, draft].map((task) => ({ host: "mac", task, key: `mac/${task.id}` }));
  const workspaces = [workspace("api", "needs_you", 200), workspace("web", "working", 30), workspace("old", "idle", 900)];

  it("puts what needs you first, longest-waiting first, across tasks and workspaces", () => {
    const groups = inboxGroups(inboxItems(tasks, workspaces));
    expect(groups.map((g) => g.label)).toEqual(["NEEDS YOU", "WORKING", "DONE"]);
    const needs = groups[0].items;
    // The workspace has waited 200 minutes; the stopped task 90; then the decision.
    expect(needs.map((i) => i.title)).toEqual(["api", failed.title, blocked.title]);
    expect(needs[0].summary).toContain("make deploy");
    expect(needs[1].need?.kind).toBe("failed");
    expect(needs[2].need?.kind).toBe("decision");
    expect(needs[2].summary).toBe(blocked.decisions[0].summary);
    // Working: newest first.
    expect(groups[1].items.map((i) => i.title)).toEqual([active.title, "web"]);
    expect(groups[2].items.map((i) => i.title)).toEqual([done.title]);
    // Idle work (a draft, an idle workspace) isn't in the inbox.
    expect(groups.flatMap((g) => g.items).some((i) => i.title === draft.title || i.title === "old")).toBe(false);
    expect(needCount(inboxItems(tasks, workspaces))).toBe(3);
  });

  it("is empty when nothing is going on", () => {
    expect(inboxGroups(inboxItems([], [workspace("old", "idle", 1)]))).toEqual([]);
  });

  it("tells about a need once, and not about what waited before it looked", () => {
    const seen = new Map<string, string | null>();
    const placed = (f: typeof active) => ({ host: "mac", task: f, key: `mac/${f.id}` });
    // The first look: the baseline.
    expect(newNeeds(seen, [placed(active), placed(blocked)])).toEqual([]);
    // The same again: nothing new.
    expect(newNeeds(seen, [placed(active), placed(blocked)])).toEqual([]);
    // The working one stops: news, once.
    const stopped = { ...active, status: "failed" as const, status_reason: "Stopped: out of attempts", status_since: "2026-10-09T12:01:00Z" };
    const told = newNeeds(seen, [placed(stopped), placed(blocked)]);
    expect(told.map((t) => [t.title, t.need.kind])).toEqual([[active.title, "failed"]]);
    expect(newNeeds(seen, [placed(stopped), placed(blocked)])).toEqual([]);
  });

  it("lists what waits in the menu bar, longest first", () => {
    const items = trayTasks(tasks, (t, what, h) => `${t} · ${what} (${h})`);
    expect(items.map((i) => i.key)).toEqual([`task:mac/${failed.id}`, `task:mac/${blocked.id}`]);
    expect(items[0].label).toContain("Stopped after 6 of 6 attempts");
  });
});
