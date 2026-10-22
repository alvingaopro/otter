import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AutomationsView } from "./AutomationsView";
import { TasksView } from "./TasksView";
import { mockSource, type TaskSource } from "./taskSource";
import { mockTriggers, type TriggerSource } from "./triggerSource";
import type { PlacedTask, TaskAction } from "./tasks";
import type { OtterAction } from "./automations";

let root: Root;
let host: HTMLDivElement;
const onOpenTask = vi.fn();
const onSettings = vi.fn();

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

async function render(source: TaskSource, triggers: TriggerSource, tasks: PlacedTask[] = source.list(), active?: boolean) {
  await act(async () =>
    root.render(
      <AutomationsView
        tasks={tasks}
        source={source}
        triggers={triggers}
        now={Date.now()}
        workspaceName={(_h, id) => (id === "ws_demo" ? "demo" : undefined)}
        onOpenTask={onOpenTask}
        onSettings={onSettings}
        active={active}
      />,
    ),
  );
}

/** What Otter did is read a moment after the tasks change. */
const settle = () => act(async () => new Promise((r) => setTimeout(r, 350)));

/** What Otter did on the sample tasks (D-193): moved one on your word, failed to publish one, made a sub-task of one gone. */
function sampleActions(source: TaskSource): OtterAction[] {
  const of = (title: string) => source.list().find((p) => p.task.title === title)!.task;
  const csv = of("Export workspace timeline as CSV");
  const uptime = of("Show host uptime on the host page");
  const at = (minutesAgo: number) => new Date(Date.now() - minutesAgo * 60_000).toISOString();
  return [
    {
      task_id: csv.id,
      task_title: csv.title,
      seq: 9,
      at: at(2),
      tool: "set_workspace",
      origin: "developer",
      authority: "m1",
      detail: "demo",
      undo: { task_id: csv.id, action: { action: "set_workspace", workspace: "ws_old" } },
    },
    { task_id: uptime.id, task_title: uptime.title, seq: 7, at: at(30), tool: "publish", origin: "auto", error: "not_ready" },
    {
      task_id: "ft_gone",
      task_title: "Document the export",
      seq: 3,
      at: at(90),
      tool: "create_task",
      origin: "event",
      detail: "Write the CSV docs (ft_child)",
      undo: { task_id: "ft_child", action: { action: "cancel" } },
    },
  ];
}

/** A source that says what Otter did on each host (`undefined`: an otterd that can't say), counting the reads. */
function withActions(source: TaskSource, byHost: Record<string, OtterAction[] | undefined>): TaskSource & { reads: string[] } {
  const reads: string[] = [];
  return {
    ...source,
    reads,
    actions: async (h) => {
      reads.push(h);
      return byHost[h];
    },
  };
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
    // What Otter said of it, here, and the task one click away.
    const report = q('[aria-label="Otter\'s report"]')!;
    expect(report.textContent).toContain("CI on #31 passed: all 3 checks. It's yours to merge.");
    await act(async () => byText('[aria-label="Otter\'s report"] button', "Open the task")!.click());
    expect(onOpenTask).toHaveBeenCalledWith(expect.stringMatching(/^mac\/ft_/));
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
    // Its own occurrence, taken at the click.
    expect(run).toHaveBeenCalledWith("mac/trg_nightly", expect.stringMatching(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/));
    // What came of it, from its last firing; a task not (or no longer) here isn't a link.
    expect(q('[role="status"]')?.textContent).toBe("Started ft_mockrun.");
    const runs = all('[aria-label="Runs, newest first"] li');
    expect(runs[0].textContent).toContain("ft_mockrun (task deleted)");
    expect(runs[0].querySelector("button")).toBeNull();

    // An otterd from before pausing answers with an error: said, in place.
    setEnabled.mockRejectedValueOnce("invalid request: unknown method `trigger.update`");
    await act(async () => byText(".pane-head button", "Resume")!.click());
    expect(q('[role="alert"]')?.textContent).toContain("unknown method `trigger.update`");
    // About that trigger only: another one, and back, start clean.
    await act(async () => byText('[role="listitem"]', "deps-check")!.click());
    expect(q('[role="alert"]')).toBeNull();
    await act(async () => byText('[role="listitem"]', "nightly")!.click());
    expect(q('[role="alert"]')).toBeNull();
    expect(q('[role="status"]')).toBeNull();
  });

  it("sends nothing more while an action is on its way", async () => {
    const { source } = recorded();
    const triggers = mockTriggers("mac");
    let finish: () => void = () => {};
    const run = vi.spyOn(triggers, "run").mockImplementation(
      (key) =>
        new Promise((resolve) => {
          finish = () => resolve(triggers.list().find((p) => p.key === key)!.trigger);
        }),
    );
    await render(source, triggers);
    await act(async () => byText('[role="listitem"]', "nightly")!.click());
    const row = byText('[role="listitem"]', "nightly")!;
    await act(async () => row.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 10, clientY: 10 })));
    await act(async () => byText('[role="menuitem"]', "Run now")!.click());
    // In flight: the pane's buttons are off, the menu offers nothing that acts.
    expect((byText(".pane-head button", "Pause") as HTMLButtonElement).disabled).toBe(true);
    await act(async () => row.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 10, clientY: 10 })));
    expect([...document.querySelectorAll('[role="menuitem"]')].map((m) => m.textContent)).toEqual([]);
    expect(run).toHaveBeenCalledTimes(1);
    await act(async () => finish());
    expect((byText(".pane-head button", "Pause") as HTMLButtonElement).disabled).toBe(false);

    // A watch's Stop, clicked twice: sent once.
    let stopped: () => void = () => {};
    const acts: TaskAction[] = [];
    const slow: TaskSource = {
      ...source,
      act: (_k, a) => {
        acts.push(a);
        return new Promise<void>((resolve) => {
          stopped = resolve;
        });
      },
    };
    await render(slow, triggers);
    await act(async () => byText('[role="listitem"]', "See how the export is going")!.click());
    const stop = byText(".pane-head button", "Stop watching") as HTMLButtonElement;
    await act(async () => stop.click());
    await act(async () => stop.click());
    expect(stop.disabled).toBe(true);
    expect(acts).toEqual([{ action: "cancel_watch", watch_id: "wch_mock1" }]);
    await act(async () => stopped());
  });

  it("links a run to its task only while the task is there", async () => {
    const { source } = recorded();
    const tasks = source.list();
    const kept = tasks[0];
    const base = mockTriggers("mac");
    const t = base.list()[0];
    const one: TriggerSource = {
      ...base,
      list: () => [
        {
          ...t,
          trigger: {
            ...t.trigger,
            history: [
              { occurrence: "2026-10-21T20:20:00Z", at: "2026-10-21T20:20:01Z", task_id: kept.task.id },
              { occurrence: "2026-10-22T20:20:00Z", at: "2026-10-22T20:20:01Z", task_id: "ft_gone" },
            ],
          },
        },
      ],
      subscribe: () => () => {},
    };
    await render(source, one, tasks);
    await act(async () => byText('[role="listitem"]', "nightly")!.click());
    const runs = all('[aria-label="Runs, newest first"] li');
    expect(runs[0].textContent).toContain("ft_gone (task deleted)");
    expect(runs[0].querySelector("button")).toBeNull();
    await act(async () => runs[1].querySelector("button")!.click());
    expect(onOpenTask).toHaveBeenCalledWith(kept.key);
  });

  it("says a deadline has passed, in the list and the page, instead of a time ahead", async () => {
    const { source } = recorded();
    const tasks = source.list().map((p) =>
      p.task.watches?.some((w) => w.status === "waiting")
        ? { ...p, task: { ...p.task, watches: p.task.watches.map((w) => ({ ...w, deadline: new Date(Date.now() - 60_000).toISOString() })) } }
        : p,
    );
    await render(source, mockTriggers("mac"), tasks);
    const row = byText('[role="listitem"]', "See how the export is going")!;
    expect(row.textContent).toContain("due · said at the next look");
    expect(q(".automation-facts")?.textContent).toContain("Duedue · said at the next look");
  });

  it("says what it's for when there's nothing", async () => {
    const { source } = recorded();
    const none: TriggerSource = { ...mockTriggers("mac"), list: () => [], subscribe: () => () => {} };
    await render(source, none, []);
    expect(host.textContent).toContain("Nothing yet.");
    expect(host.textContent).toContain("Nothing automatic yet");
  });
});

describe("What Otter did (D-193)", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    onOpenTask.mockReset();
    onSettings.mockReset();
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  const rows = () => all('[aria-label="What Otter did, newest first"] li');

  it("lists what Otter did on a host, newest first, with the task one click away", async () => {
    const { source } = recorded();
    const actions = sampleActions(source);
    await render(withActions(source, { mac: actions }), mockTriggers("mac"));
    await settle();
    // Under the watches and triggers, one entry for the host.
    const groups = all(".sidebar .group-label").map((g) => g.firstElementChild?.textContent);
    expect(groups).toEqual(["WATCHING", "SCHEDULED", "HISTORY", "WHAT OTTER DID"]);
    const entry = byText('[role="listitem"]', "Otter's actions")!;
    expect(entry.querySelector(".ws-summary")?.textContent).toBe("Moved the task · Export workspace timeline as CSV");
    await act(async () => entry.click());
    expect(q(".pane-head h1")?.textContent).toBe("What Otter did");

    const [moved, published, made] = rows();
    expect(rows()).toHaveLength(3);
    // In plain words, what it was done on, and on whose word — your message's, quoted.
    expect(moved.textContent).toContain("Moved the task");
    expect(moved.textContent).toContain("demo");
    expect(moved.textContent).toContain("on your word: “Let me export a workspace's timeline as CSV from the Timeline panel.”");
    // What it couldn't do is marked, and has nothing to undo.
    expect(published.querySelector(".ledger-error")?.textContent).toBe("not ready");
    expect(published.textContent).toContain("Published");
    expect(published.textContent).toContain("on its own");
    expect(published.querySelector(".ledger-undo")).toBeNull();
    // A task no longer here isn't a link.
    expect(made.textContent).toContain("Made a sub-task");
    expect(made.textContent).toContain("Document the export");
    expect([...made.querySelectorAll("button")].map((b) => b.textContent)).toEqual(["Undo"]);

    await act(async () => byText("li button", "Export workspace timeline as CSV")!.click());
    expect(onOpenTask).toHaveBeenCalledWith(source.list().find((p) => p.task.id === actions[0].task_id)!.key);
    await act(async () => byText("button", "Change what Otter may do")!.click());
    expect(onSettings).toHaveBeenCalledWith("mac");
  });

  it("undoes what Otter did, once, on the task that undoes it, and reads it again", async () => {
    const { source, acts } = recorded();
    const actions = sampleActions(source);
    const s = withActions(source, { mac: actions });
    await render(s, mockTriggers("mac"));
    await settle();
    await act(async () => byText('[role="listitem"]', "Otter's actions")!.click());
    const reads = s.reads.length;
    await act(async () => rows()[0].querySelector<HTMLButtonElement>(".ledger-undo")!.click());
    expect(acts).toEqual([{ key: `mac/${actions[0].task_id}`, action: { action: "set_workspace", workspace: "ws_old" } }]);
    // Undone: said instead of offered again, and what Otter did read again.
    expect(rows()[0].querySelector(".ledger-undo")?.textContent).toBe("Undone");
    await settle();
    expect(s.reads.length).toBeGreaterThan(reads);
    // A sub-task Otter made is undone on the sub-task.
    await act(async () => rows()[2].querySelector<HTMLButtonElement>(".ledger-undo")!.click());
    expect(acts[1]).toEqual({ key: "mac/ft_child", action: { action: "cancel" } });

    // What the host says when it can't is said in place, and Undo stays.
    act(() => root.unmount());
    root = createRoot(host);
    const refusing: TaskSource = { ...s, act: async () => Promise.reject("invalid request: the task has worked since") };
    await render(refusing, mockTriggers("mac"));
    await settle();
    await act(async () => byText('[role="listitem"]', "Otter's actions")!.click());
    await act(async () => rows()[0].querySelector<HTMLButtonElement>(".ledger-undo")!.click());
    expect(q('[role="alert"]')?.textContent).toContain("the task has worked since");
    expect(rows()[0].querySelector(".ledger-undo")?.textContent).toBe("Undo");
  });

  it("reads it only while in front, and shows nothing for a host whose otterd can't say", async () => {
    const { source } = recorded();
    const s = withActions(source, { mac: sampleActions(source) });
    await render(s, mockTriggers("mac"), source.list(), false);
    await settle();
    expect(s.reads).toEqual([]);
    expect(byText('[role="listitem"]', "Otter's actions")).toBeUndefined();
    await render(s, mockTriggers("mac"), source.list(), true);
    await settle();
    expect(s.reads).toEqual(["mac"]);

    // An older otterd: no entry, the rest as before.
    act(() => root.unmount());
    root = createRoot(host);
    await render(withActions(source, { mac: undefined }), mockTriggers("mac"));
    await settle();
    expect(all(".sidebar .group-label").map((g) => g.firstElementChild?.textContent)).toEqual(["WATCHING", "SCHEDULED", "HISTORY"]);
    expect(host.textContent).not.toContain("What Otter did");
  });

  it("says when Otter hasn't done anything yet, and is what shows when nothing else is", async () => {
    const { source } = recorded();
    const none: TriggerSource = { ...mockTriggers("mac"), list: () => [], subscribe: () => () => {} };
    const quiet = source.list().map((p) => ({ ...p, task: { ...p.task, watches: [] } }));
    await render(withActions(source, { mac: [] }), none, quiet);
    await settle();
    expect(host.textContent).not.toContain("Nothing automatic yet");
    expect(q(".pane-head h1")?.textContent).toBe("What Otter did");
    expect(byText('[role="listitem"]', "Otter's actions")?.querySelector(".ws-summary")?.textContent).toBe("Nothing yet");
    expect(q(".automation-body")?.textContent).toContain("Nothing yet.");
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
    await act(async () => q('[aria-label="Stop watching See how the export is going"]')!.click());
    expect(acts[0].action).toEqual({ action: "cancel_watch", watch_id: "wch_mock1" });
    expect(q('[aria-label="Watching"]')).toBeNull();
  });

  it("offers Stop for a watch that is telling you what it found, too", async () => {
    const { source, acts } = recorded();
    const firing: TaskSource = {
      ...source,
      list: () =>
        source.list().map((p) =>
          p.task.watches?.some((w) => w.status === "waiting")
            ? {
                ...p,
                task: {
                  ...p.task,
                  watches: p.task.watches.map((w) => ({ ...w, status: "firing" as const, finding: { at: w.created_at, outcome: "due" as const, text: "the time came" } })),
                },
              }
            : p,
        ),
    };
    await act(async () =>
      root.render(<TasksView source={firing} hosts={["mac"]} workspaces={{ mac: [] }} now={Date.now()} onOpenWorkspace={() => {}} />),
    );
    await act(async () => byText('[role="listitem"]', "Export workspace timeline as CSV")!.click());
    expect(q('[aria-label="Watching"]')!.textContent).toContain("See how the export is going: telling you what it found");
    await act(async () => q('[aria-label="Stop watching See how the export is going"]')!.click());
    expect(acts[0].action).toEqual({ action: "cancel_watch", watch_id: "wch_mock1" });
  });
});
