import { describe, expect, it } from "vitest";
import { sampleTasks } from "./taskSource";
import { tasksIn } from "./tasks";

const NOW = Date.parse("2026-10-09T12:00:00Z");

describe("tasks in a workspace", () => {
  const [active, blocked, , done] = sampleTasks(NOW);
  const placed = (host: string, task: (typeof active)) => ({ host, task, key: `${host}/${task.id}` });

  it("shows the tasks still going there, on that host only", () => {
    const here = { ...active, workspace_id: "ws_a" };
    const viaStep = { ...blocked, workspace_id: undefined, steps: [{ ...blocked.steps[0], workspace_id: "ws_a" }] };
    const ended = { ...done, workspace_id: "ws_a" };
    const list = [placed("mac", here), placed("mac", viaStep), placed("mac", ended), placed("box", here)];
    const found = tasksIn(list, "mac", "ws_a").map((p) => p.key);
    expect(found).toHaveLength(2);
    expect(found).toEqual(expect.arrayContaining([`mac/${here.id}`, `mac/${viaStep.id}`]));
    expect(tasksIn(list, "mac", "ws_b")).toEqual([]);
  });
});
