import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AutomationsView } from "./AutomationsView";
import { TasksView } from "./TasksView";
import { mockSource, type TaskSource } from "./taskSource";
import { mockTriggers, type TriggerSource } from "./triggerSource";
import type { PlacedTask, TaskAction } from "./tasks";

let root: Root;
let host: HTMLDivElement;
const onOpenTask = vi.fn();

const q = <T extends Element = HTMLElement>(sel: string) => host.querySelector<T & HTMLElement>(sel);
const all = (sel: string) => [...host.querySelectorAll<HTMLElement>(sel)];
const byText = (sel: string, text: string) =>
  all(sel).find((e) => e.textContent?.includes(text)) ?? [...document.querySelectorAll<HTMLElement>(sel)].find((e) => e.textContent?.includes(text));

/** The sample tasks (with their watches), their actions recorded and applied. */
function recorded(): { source: TaskSource; acts: { key: string; action: TaskAction }[] } {
  const base = mockSource("mac");
  const acts: { key: string; action: TaskAction }[] = [];
  return {
    acts,
    source: {
      ...base,
      act: async (key, action) => {
        acts.push({ key, action });
        await base.act(key, action);
      },
    },
  };
}

async function render(source: TaskSource, triggers: TriggerSource, tasks: PlacedTask[] = source.list()) {
  await act(async () =>
    root.render(
      <AutomationsView
        tasks={tasks}
        source={source}
        triggers={triggers}
        now={Date.now()}
        workspaceName={(_h, id) => (id === "ws_demo" ? "demo" : undefined)}
        onOpenTask={onOpenTask}
      />,
    ),
  );
}

describe("Automations view", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    onOpenTask.mockReset();
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  it("lists what Otter watches, the triggers, and what's over, and opens on the first", async () => {
    const { source } = recorded();
    await render(source, mockTriggers("mac"));
    const groups = all(".sidebar .group-label").map((g) => [g.firstElementChild?.textContent, g.lastElementChild?.textContent]);
    expect(groups).toEqual([
      ["WATCHING", "1"],
      ["SCHEDULED", "2"],
      ["HISTORY", "1"],
    ]);
    const rows = all('[role="listitem"]').map((r) => r.querySelector(".ws-name")?.textContent);
    expect(rows).toEqual(["See how the export is going", "nightly", "deps-check", "CI on #31"]);
    // A paused trigger is dimmed and says so.
    const paused = byText('[role="listitem"]', "deps-check")!;
    expect(paused.className).toContain("stale");
    expect(paused.textContent).toContain("paused");
    // The first is shown: a waiting watch, its task one click away.
    expect(q(".pane-head h1")?.textContent).toBe("See how the export is going");
    expect(q(".pane-head .status-pill")?.textContent).toBe("watching");
    await act(async () => byText(".automation-from button", "Export workspace timeline as CSV")!.click());
    expect(onOpenTask).toHaveBeenCalledWith(expect.stringMatching(/^mac\/ft_/));
  });

  it("shows an ended watch's readings and what it came to", async () => {
    const { source } = recorded();
    await render(source, mockTriggers("mac"));
    await act(async () => byText('[role="listitem"]', "CI on #31")!.click());
    expect(q(".pane-head .status-pill")?.textContent).toBe("passed");
    expect(host.textContent).toContain("CI · example/otter #31");
    expect(host.textContent).toContain("passed: all 3 checks passed (at abc1234)");
    const readings = all('[aria-label="Readings, newest first"] li').map((li) => li.lastElementChild?.textContent);
    expect(readings).toEqual(["all 3 checks passed (at abc1234)", "2 of 3 checks running, 1 passed (at abc1234)", "no checks yet"]);
    // Ended: nothing to stop.
    expect(byText(".pane-head button", "Stop watching")).toBeUndefined();
  });

  it("stops a watch, and deletes one only once you confirm", async () => {
    const { source, acts } = recorded();
    await render(source, mockTriggers("mac"));
    await act(async () => byText(".pane-head button", "Stop watching")!.click());
    expect(acts.map((a) => a.action)).toEqual([{ action: "cancel_watch", watch_id: "wch_mock1" }]);
    // The source has it stopped: shown again from the list it now gives.
    await render(source, mockTriggers("mac"));
    expect(all(".sidebar .group-label").map((g) => g.firstElementChild?.textContent)).toEqual(["SCHEDULED", "HISTORY"]);

    await act(async () => byText('[role="listitem"]', "CI on #31")!.click());
    await act(async () => q('[aria-label="More actions"]')!.click());
    await act(async () => byText('[role="menuitem"]', "Delete…")!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    expect(dialog.textContent).toContain("Delete CI on #31 from mac?");
    // Not yet: only the dialog's Delete deletes.
    expect(acts).toHaveLength(1);
    await act(async () => byText('[role="dialog"] button', "Delete")!.click());
    expect(acts[1].action).toEqual({ action: "delete_watch", watch_id: "wch_mock2" });
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });

  it("pauses, resumes and runs a trigger, and says when a host can't", async () => {
    const { source } = recorded();
    const triggers = mockTriggers("mac");
    const setEnabled = vi.spyOn(triggers, "setEnabled");
    const run = vi.spyOn(triggers, "run");
    await render(source, triggers);
    await act(async () => byText('[role="listitem"]', "nightly")!.click());
    expect(q(".pane-head h1")?.textContent).toBe("nightly");
    expect(host.textContent).toContain("daily at 20:20 UTC");
    expect(host.textContent).toContain("publishes without asking; at most 30 min of work; pauses after waiting 30 min for you");
    expect(host.textContent).toContain("demo");
    expect(host.textContent).toContain("Skipped: “Nightly checks” is still at work");
    await act(async () => byText(".pane-head button", "Pause")!.click());
    expect(setEnabled).toHaveBeenCalledWith("mac/trg_nightly", false);
    expect(q(".pane-head .status-pill")?.textContent).toBe("paused");
    expect(byText(".pane-head button", "Resume")).toBeTruthy();

    // From the row's menu.
    const row = byText('[role="listitem"]', "nightly")!;
    await act(async () => row.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 10, clientY: 10 })));
    await act(async () => byText('[role="menuitem"]', "Run now")!.click());
    expect(run).toHaveBeenCalledWith("mac/trg_nightly");

    // An otterd from before pausing answers with an error: said, in place.
    setEnabled.mockRejectedValueOnce("invalid request: unknown method `trigger.update`");
    await act(async () => byText(".pane-head button", "Resume")!.click());
    expect(q('[role="alert"]')?.textContent).toContain("unknown method `trigger.update`");
  });

  it("says what it's for when there's nothing", async () => {
    const { source } = recorded();
    const none: TriggerSource = { ...mockTriggers("mac"), list: () => [], subscribe: () => () => {} };
    await render(source, none, []);
    expect(host.textContent).toContain("Nothing yet.");
    expect(host.textContent).toContain("Nothing automatic yet");
  });
});

describe("Watches in Chat", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  it("marks a watched task's row and shows what it watches above the message box, with Stop", async () => {
    const { source, acts } = recorded();
    await act(async () =>
      root.render(<TasksView source={source} hosts={["mac"]} workspaces={{ mac: [] }} now={Date.now()} onOpenWorkspace={() => {}} />),
    );
    const watched = byText('[role="listitem"]', "Export workspace timeline as CSV")!;
    expect(watched.querySelector(".watching-mark")?.getAttribute("aria-label")).toBe("Watching: See how the export is going");
    // A task whose watch ended has no mark.
    expect(byText('[role="listitem"]', "Show host uptime")!.querySelector(".watching-mark")).toBeNull();
    await act(async () => watched.click());
    const strip = q('[aria-label="Watching"]')!;
    expect(strip.textContent).toMatch(/^Watching See how the export is going · at \d{2}:\d{2}Stop$/);
    await act(async () => byText('[aria-label="Watching"] button', "Stop")!.click());
    expect(acts[0].action).toEqual({ action: "cancel_watch", watch_id: "wch_mock1" });
    expect(q('[aria-label="Watching"]')).toBeNull();
  });
});
