import { describe, expect, it } from "vitest";
import { sampleTasks } from "./taskSource";
import { newNeeds, trayTasks } from "./taskNeeds";

const NOW = Date.parse("2026-10-09T12:00:00Z");

describe("what tasks need of you", () => {
  const [active, blocked, failed, done, draft] = sampleTasks(NOW);
  const tasks = [active, blocked, failed, done, draft].map((task) => ({ host: "mac", task, key: `mac/${task.id}` }));

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
    const items = trayTasks(tasks);
    expect(items.map((i) => i.key)).toEqual([`task:mac/${failed.id}`, `task:mac/${blocked.id}`]);
    expect(items[0].what).toContain("Stopped after 6 of 6 attempts");
  });
});
