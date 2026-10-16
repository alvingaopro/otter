import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TasksView } from "./TasksView";
import { mockSource, sampleTasks, type TaskSource } from "./taskSource";
import { STATUS_LABEL, budgetBinds, collapseRepeats, findMatch, groupOf, needsYou, sortTasks, statusDetail, type Task, type PlacedTask } from "./tasks";

describe("task contract helpers", () => {
  const [active, blocked, failed, done, draft] = sampleTasks(Date.parse("2026-10-09T12:00:00Z"));

  it("knows which tasks need the developer", () => {
    expect(needsYou(blocked)).toBe(true);
    expect(needsYou(active)).toBe(false);
    // Review needs you once it's ready to accept, not while CI runs (D-071).
    expect(needsYou({ ...done, status: "review", report: "## Report" })).toBe(true);
    expect(needsYou({ ...done, status: "review", report: undefined })).toBe(false);
    // A stopped task waits on your retry.
    expect(needsYou(failed)).toBe(true);
    // What otter-core served wins.
    expect(needsYou({ ...blocked, activity: "working", needs: undefined })).toBe(false);
  });

  it("groups by what needs you, what goes on and what is over", () => {
    expect(groupOf(blocked)).toBe("needs");
    expect(groupOf(failed)).toBe("needs");
    expect(groupOf(active)).toBe("active");
    expect(groupOf(draft)).toBe("active");
    expect(groupOf(done)).toBe("done");
    expect(groupOf({ ...done, status: "cancelled" })).toBe("done");
  });

  it("finds a task by its title, request or host", () => {
    const p: PlacedTask = { host: "mac", task: active, key: "k" };
    expect(findMatch(p, "")).toBe(true);
    expect(findMatch(p, "csv")).toBe(true);
    expect(findMatch(p, "MAC")).toBe(true);
    expect(findMatch(p, "uptime")).toBe(false);
  });

  it("says a repeated reason once, with how many times", () => {
    expect(collapseRepeats(["a", "b", "b", "b", "a"])).toEqual([
      { text: "a", times: 1 },
      { text: "b", times: 3 },
      { text: "a", times: 1 },
    ]);
  });

  it("shows the budget once half of it is used", () => {
    expect(budgetBinds(active)).toBe(false);
    expect(budgetBinds({ ...active, budget: { ...active.budget, iterations_used: 3 } })).toBe(true);
  });

  it("shows Otter's loop as one state, with what it's doing", () => {
    // Planning, implementing and verifying are all just "Working" (D-050).
    expect([STATUS_LABEL.planning, STATUS_LABEL.implementing, STATUS_LABEL.verifying]).toEqual(["Working", "Working", "Working"]);
    expect(STATUS_LABEL.review).toBe("Ready to check");
    expect(statusDetail(active)).toBe("step 2 of 2: Wire Export button into Timeline panel");
    expect(statusDetail(blocked)).toContain("Waiting for your approval");
    expect(statusDetail(draft)).toBeUndefined();
    expect(failed.status).toBe("failed");
  });

  it("sorts what needs you first, longest-waiting first, then newest", () => {
    const placed: PlacedTask[] = [active, blocked, done, failed].map((f) => ({ host: "h", task: f, key: f.id }));
    expect(sortTasks(placed).map((p) => p.task.title)).toEqual([failed.title, blocked.title, active.title, done.title]);
  });
});

let root: Root;
let host: HTMLDivElement;
let source: TaskSource;
const onOpenWorkspace = vi.fn();

const q = <T extends Element = HTMLElement>(sel: string) => host.querySelector<T & HTMLElement>(sel);
const byText = (sel: string, text: string) =>
  [...host.querySelectorAll<HTMLElement>(sel)].find((e) => e.textContent?.includes(text)) ??
  [...document.querySelectorAll<HTMLElement>(sel)].find((e) => e.textContent?.includes(text));

function type(el: HTMLInputElement | HTMLTextAreaElement, value: string) {
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, "value")!.set!.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

async function render(s: TaskSource) {
  source = s;
  await act(async () =>
    root.render(
      <TasksView
        source={s}
        hosts={["mac"]}
        workspaces={{ mac: [{ id: "ws_demo", name: "demo" }] }}
        now={Date.now()}
        onOpenWorkspace={onOpenWorkspace}
        onOpenPreview={async () => {}}
      />,
    ),
  );
}

describe("Tasks view", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    onOpenWorkspace.mockReset();
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  const open = async (title: string) => act(async () => byText('[role="listitem"]', title)!.click());
  const menu = async () => {
    await act(async () => q('[aria-label="More actions"]')!.click());
    return [...document.querySelectorAll('[role="menuitem"]')].map((m) => m.textContent);
  };
  const sections = () => [...host.querySelectorAll(".ov-section")].map((s) => s.getAttribute("aria-label"));
  const single = async (task: Task) => {
    const base = mockSource("mac");
    const acts: unknown[] = [];
    const s: TaskSource = {
      ...base,
      list: () => [{ host: "mac", task, key: "mac/" + task.id }],
      subscribe: () => () => {},
      act: async (_k, a) => void acts.push(a),
    };
    await render(s);
    return acts;
  };

  it("shows an empty state with a create action", async () => {
    const empty = mockSource("mac");
    // Start from nothing.
    const s: TaskSource = { ...empty, list: () => [], subscribe: () => () => {} };
    await render(s);
    expect(host.textContent).toContain("Describe a task");
    expect(host.textContent).toContain("No tasks yet.");
    expect(byText("button", "New task")).toBeTruthy();
  });

  it("lists tasks grouped, what needs you first, and says it is a preview", async () => {
    await render(mockSource("mac"));
    const groups = [...host.querySelectorAll(".sidebar .group-label")].map((g) => g.firstElementChild?.textContent);
    expect(groups).toEqual(["NEEDS YOU", "ACTIVE", "DONE"]);
    const rows = [...host.querySelectorAll('[role="listitem"]')].map((r) => r.querySelector(".ws-name")?.textContent);
    // Waiting on you longest: the stopped one, then the blocked one.
    expect(rows.slice(0, 2)).toEqual(["Upgrade xterm to v7", "Rotate the staging database password"]);
    expect(rows).toHaveLength(5);
    expect(q('[role="note"]')?.textContent).toContain("Preview");
    // A blocked task's decision waits above the message box, no tab to find it in.
    await open("Rotate the staging");
    expect(q('[role="tab"][aria-selected="true"]')?.textContent).toBe("Overview");
    expect(q(".pending-decisions .decision.pending")?.textContent).toContain("vault kv put");
    expect(q(".banner.needs")).toBeNull();
    // Two tabs, not seven.
    expect([...host.querySelectorAll('[role="tab"]')].map((t) => t.textContent)).toEqual(["Overview", "Activity"]);
  });

  it("finds a task by part of its name", async () => {
    await render(mockSource("mac"));
    const find = q<HTMLInputElement>('input[aria-label="Find a task"]')!;
    await act(async () => type(find, "uptime"));
    expect([...host.querySelectorAll('[role="listitem"]')].map((r) => r.textContent)).toEqual([expect.stringContaining("Show host uptime")]);
    expect(q(".task-title")?.textContent).toBe("Show host uptime on the host page");
    await act(async () => type(find, "nothing like it"));
    expect(host.textContent).toContain("No task matches.");
  });

  it("deletes a task that isn't running, after asking, from the ⋯ menu", async () => {
    await render(mockSource("mac"));
    // Being worked on: no Delete.
    await open("Export workspace timeline as CSV");
    expect(await menu()).not.toContain("Delete task…");

    await open("Show host uptime on the host page");
    expect(await menu()).toContain("Delete task…");
    await act(async () => byText('[role="menuitem"]', "Delete task…")!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    expect(dialog.textContent).toContain("Show host uptime on the host page");
    await act(async () => byText('[role="dialog"] button', "Delete")!.click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(byText(".ws-name", "Show host uptime")).toBeUndefined();
    expect(host.querySelectorAll('[role="listitem"]')).toHaveLength(4);
  });

  it("creates a draft, converses and starts it", async () => {
    await render(mockSource("mac"));
    await act(async () => q('[aria-label="New task"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "CSV export");
    type(dialog.querySelector("textarea")!, "Export timelines as CSV");
    await act(async () => byText("button", "Create draft")!.click());

    expect(q(".task-title")?.textContent).toBe("CSV export");
    expect(q(".status-pill")?.textContent).toBe("Draft");
    expect(host.querySelector(".message.user")?.textContent).toContain("Export timelines as CSV");

    const composer = q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
    type(composer, "Also include the session name");
    await act(async () => byText("button", "Send")!.click());
    const messages = [...host.querySelectorAll(".message")].map((m) => m.textContent);
    expect(messages.some((m) => m?.includes("Also include the session name"))).toBe(true);
    expect(host.querySelector(".message.system")?.textContent).toContain("nothing is connected");

    await act(async () => byText("button", "Start")!.click());
    expect(q(".status-pill")?.textContent).toBe("Working");
    expect(byText("button", "Take over")).toBeUndefined();
  });

  it("creates an answer-only task, and says so on it", async () => {
    const base = mockSource("mac");
    const made: unknown[] = [];
    const s: TaskSource = {
      ...base,
      create: async (h, title, request, workspace, answerOnly) => {
        made.push({ title, answerOnly });
        return base.create(h, title, request, workspace, answerOnly);
      },
    };
    await render(s);
    await act(async () => q('[aria-label="New task"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "How does export work?");
    await act(async () => dialog.querySelector<HTMLInputElement>('input[aria-label="Only an answer"]')!.click());
    await act(async () => byText("button", "Create draft")!.click());
    expect(made).toEqual([{ title: "How does export work?", answerOnly: true }]);
    expect(byText(".chip", "Answer only")).toBeTruthy();
  });

  it("shows the coding agent's work in Activity and offers interrupt and redirect while it runs", async () => {
    const base = mockSource("mac");
    const acts: unknown[] = [];
    const s: TaskSource = { ...base, act: async (_key, action) => void acts.push(action) };
    await render(s);
    await open("Export workspace timeline as CSV");
    await act(async () => byText('[role="tab"]', "Activity")!.click());
    const feed = q(".feed")!;
    expect(feed.textContent).toContain("Working");
    expect(feed.textContent).toContain("cargo test timeline::csv");
    expect(feed.textContent).toContain("Queued after the current work");
    // Only the task's own history: the turns go.
    await act(async () => byText(".seg-btn", "Task")!.click());
    expect(q(".feed")!.textContent).not.toContain("cargo test timeline::csv");
    await act(async () => byText(".seg-btn", "Coding agent")!.click());
    expect(q(".feed")!.textContent).toContain("cargo test timeline::csv");
    // Its runtime can interrupt and redirect: the buttons are there.
    await act(async () => byText("button", "Interrupt")!.click());
    expect(acts).toContainEqual({ action: "interrupt" });
    const composer = q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
    type(composer, "Use the existing icon");
    await act(async () => byText("button", "Redirect now")!.click());
    expect(acts).toContainEqual({ action: "redirect", text: "Use the existing icon" });
    // Cancelling is in the menu now.
    await menu();
    await act(async () => byText('[role="menuitem"]', "Cancel task")!.click());
    expect(acts).toContainEqual({ action: "cancel" });
  });

  it("offers neither where nothing runs", async () => {
    await render(mockSource("mac"));
    await open("Show host uptime");
    expect(byText("button", "Interrupt")).toBeUndefined();
    expect(byText("button", "Redirect now")).toBeUndefined();
  });

  it("creates a task in a chosen workspace", async () => {
    await render(mockSource("mac"));
    await act(async () => q('[aria-label="New task"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "In a workspace");
    const selects = dialog.querySelectorAll("select");
    const select = selects[selects.length - 1];
    select.value = "ws_demo";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    await act(async () => byText("button", "Create draft")!.click());
    const created = source.list().find((p) => p.task.title === "In a workspace")!;
    expect(created.task.workspace_id).toBe("ws_demo");
    // The header links to it; no "choose a workspace" prompt.
    expect(q(".task-where")?.textContent).toBe("mac / demo");
    await act(async () => byText(".task-where button", "demo")!.click());
    expect(onOpenWorkspace).toHaveBeenCalledWith("mac", "ws_demo");
    expect(q('[aria-label="Workspace"][role="group"]')).toBeNull();
  });

  it("answers an approval and shows it where it was asked", async () => {
    await render(mockSource("mac"));
    await open("Rotate the staging");
    await act(async () => byText(".pending-decisions button", "Approve")!.click());
    const blocked = source.list().find((p) => p.task.title.startsWith("Rotate"))!;
    expect(blocked.task.decisions[0].status).toBe("approved");
    expect(blocked.task.decisions[0].decided_by).toBe("user");
    expect(blocked.task.status).toBe("implementing");
    expect(q(".pending-decisions")).toBeNull();
    // Taken: in the conversation, after Otter asked for it, and under Decisions.
    const log = [...host.querySelectorAll('[role="log"] > *')].map((e) => e.className);
    expect(log.slice(0, 3)).toEqual(["message user", "message controller", "decided"]);
    expect(q(".decided")?.textContent).toContain("Approved by you");
    expect(sections()).toContain("Decisions");
  });

  it("handles failed and completed tasks", async () => {
    await render(mockSource("mac"));
    await open("Upgrade xterm");
    expect(q(".banner.failure")?.textContent).toContain("6 of 6 attempts");
    expect(byText("button", "Retry")).toBeTruthy();
    // Every attempt used: the budget shows, once, with the stop in the banner.
    expect(byText(".ov-section", "attempts used")?.textContent).toContain("6 of 6 attempts used");

    await open("Show host uptime");
    expect(q(".status-pill")?.textContent).toBe("Done");
    expect(await menu()).not.toContain("Cancel task");
    // Evidence under the criterion it backs.
    expect(byText(".check", "Host page shows uptime")?.querySelector(".evidence-item a")?.getAttribute("href")).toContain("/pull/31");
  });

  it("shows only the sections a task has something for", async () => {
    await render(mockSource("mac"));
    await open("Dark-mode charts legend");
    expect(sections()).toEqual(["Now", "Request"]);
    await open("Export workspace timeline");
    // 2 of 6 attempts: not yet worth showing.
    expect(sections()).toEqual(["Now", "Progress", "Checks", "Request"]);
    expect(byText(".ov-section", "attempts used")).toBeUndefined();
    await open("Show host uptime");
    expect(sections()[0]).toBe("Outcome");
    expect(sections()).not.toContain("Now");
  });

  it("folds a long report from the coding agent", async () => {
    const [done] = sampleTasks().filter((f) => f.status === "done");
    const long = "A finding.\n\n".repeat(150);
    await single({ ...done, messages: [{ id: "m1", role: "agent", text: long, at: done.updated_at }] });
    expect(q(".fold:not(.open)")).not.toBeNull();
    await act(async () => byText(".fold-toggle", "Show all")!.click());
    expect(q(".fold.open")).not.toBeNull();
    expect(byText(".fold-toggle", "Show less")).toBeTruthy();
  });

  it("lists Otter's reasons in Activity, a repeated one once", async () => {
    const [done] = sampleTasks().filter((f) => f.status === "done");
    await single({ ...done, rationale: ["Planned it.", "Answered.", "Answered.", "Answered."] });
    expect(host.textContent).not.toContain("Planned it.");
    await act(async () => byText('[role="tab"]', "Activity")!.click());
    const reasons = q(".reasons")!;
    expect(reasons.textContent).toContain("Otter's reasons (2)");
    expect(reasons.textContent).toContain("Answered. ×3");
  });

  it("shows the gates and only offers a plain Accept when they pass", async () => {
    const base = mockSource("mac");
    const [done] = sampleTasks().filter((f) => f.status === "done");
    const review: Task = {
      ...done,
      status: "review",
      gates: [
        { name: "Requirements met", status: "passed" },
        { name: "CI", status: "pending", detail: "running" },
        { name: "Your acceptance", status: "pending" },
      ],
      delivery: { branch: "feature/x", pr_url: "https://github.com/o/r/pull/7", pr_number: 7, ci: [{ name: "test", state: "pending" }], reruns: 0, declined: false },
      report: undefined,
    };
    let list: PlacedTask[] = [{ host: "mac", task: review, key: "mac/" + review.id }];
    const acts: unknown[] = [];
    const s: TaskSource = { ...base, list: () => list, subscribe: () => () => {}, act: async (_k, a) => void acts.push(a) };
    await render(s);
    expect(byText("button", "Accept anyway")).toBeTruthy();
    // Under review the checks and the delivery come first.
    expect(sections().slice(0, 2)).toEqual(["Checks", "Delivery"]);
    expect(host.textContent).toContain("https://github.com/o/r/pull/7");
    expect(host.textContent).toContain("running");
    await act(async () => byText("button", "Accept anyway")!.click());
    expect(acts).toEqual([{ action: "accept", override_gates: true }]);

    list = [{ ...list[0], task: { ...review, gates: review.gates!.map((g) => ({ ...g, status: g.name === "Your acceptance" ? g.status : "passed" })) } }];
    act(() => root.unmount());
    root = createRoot(host);
    await render(s);
    expect(byText("button", "Accept anyway")).toBeUndefined();
    expect([...host.querySelectorAll("button")].some((b) => b.textContent === "Accept")).toBe(true);
  });

  it("links a task to its workspace and session", async () => {
    await render(mockSource("mac"));
    await open("Export workspace timeline");
    await act(async () => byText(".ov-section button", "Open session")!.click());
    expect(onOpenWorkspace).toHaveBeenCalledWith("mac", "ws_demo", "ses_demo");
  });
});
