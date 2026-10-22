import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TasksView } from "./TasksView";
import { mockSource, sampleTasks, type TaskSource } from "./taskSource";

// A browser shrinks the picture with a canvas; here it is a stand-in.
vi.mock("./pastedImage", async (orig) => ({
  ...(await orig<typeof import("./pastedImage")>()),
  prepareImage: vi.fn(async () => ({ media_type: "image/png", data: "AAAA" })),
}));
import {
  STATUS_LABEL,
  budgetBinds,
  collapseReasons,
  collapseRepeats,
  findMatch,
  groupOf,
  HELD,
  heardLine,
  needsYou,
  sortTasks,
  statusDetail,
  type Task,
  type TaskEventRecord,
  type PlacedTask,
} from "./tasks";

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

  it("folds a reason said again right after itself in the history, nothing else", () => {
    const ev = (seq: number, type: string, text: string): TaskEventRecord => ({ seq, ts: `2026-10-20T12:00:0${seq}Z`, v: 1, type, text });
    const out = collapseReasons([
      ev(1, "Reasoned", "Why: a"),
      ev(2, "Reasoned", "Why: a"),
      ev(3, "StatusChanged", "Review"),
      ev(4, "StatusChanged", "Review"),
      ev(5, "Reasoned", "Why: a"),
    ]);
    expect(out.map((o) => [o.e.seq, o.times])).toEqual([
      [2, 2],
      [3, 1],
      [4, 1],
      [5, 1],
    ]);
  });

  it("says when a working run's agent was last heard from, never silence while a tool runs", () => {
    const now = Date.parse("2026-10-20T12:05:00Z");
    const run = { ...active.runs[0], state: "running" as const, heard_at: "2026-10-20T12:02:00Z" };
    expect(heardLine({ ...active, runs: [run] }, now)).toBe("Last heard from the coding agent 3m ago.");
    expect(heardLine({ ...active, runs: [{ ...run, tools_running: 1 }] }, now)).toBe("The coding agent is running a tool.");
    // An otterd before D-141 doesn't say; a run waiting on a decision says so elsewhere.
    expect(heardLine({ ...active, runs: [{ ...run, heard_at: undefined }] }, now)).toBeUndefined();
    expect(heardLine({ ...active, runs: [{ ...run, state: "waiting" }] }, now)).toBeUndefined();
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
    localStorage.removeItem("otter.task-detail-width");
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
    expect(host.textContent).toContain("What shall we work on?");
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

  it("pauses every task at work at once, from the ACTIVE group (D-081, D-178)", async () => {
    await render(mockSource("mac", Date.parse("2026-10-09T12:00:00Z")));
    expect(q(".sidebar")?.getAttribute("aria-label")).toBe("Chat");
    expect(q(".sidebar-top .brand-name")?.textContent).toBe("Chat");
    await act(async () => byText('section[aria-label="Active"] button', "Pause all tasks")!.click());
    const working = source.list().filter((p) => ["planning", "implementing", "verifying"].includes(p.task.status));
    expect(working).toEqual([]);
    expect(source.list().some((p) => p.task.status === "paused")).toBe(true);
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
    await act(async () => q<HTMLButtonElement>('button[aria-label="Send"]')!.click());
    const messages = [...host.querySelectorAll(".message")].map((m) => m.textContent);
    expect(messages.some((m) => m?.includes("Also include the session name"))).toBe(true);
    expect(host.querySelector(".message.system")?.textContent).toContain("nothing is connected");

    await act(async () => byText("button", "Start")!.click());
    expect(q(".status-pill")?.textContent).toBe("Working");
    expect(byText("button", "Take over")).toBeUndefined();
  });

  it("tells you and Otter apart by the bubble, the coding agent by name too (D-175)", async () => {
    await render(mockSource("mac"));
    await open("Export workspace timeline as CSV");
    const bubble = (role: string) => q(`.message.${role}:not(.working)`)!;
    // No line above your words or Otter's: the time is beside the bubble.
    expect(bubble("user").querySelector(".message-head")).toBeNull();
    expect(bubble("controller").querySelector(".message-head")).toBeNull();
    expect(bubble("controller").getAttribute("aria-label")).toBe("Otter");
    expect(bubble("controller").querySelector(".message-at.aside")?.textContent).toMatch(/^\d+[smhd]$/);
    expect(bubble("agent").querySelector(".message-role")?.textContent).toBe("Coding agent");
    // Named once: by its head, not again for a screen reader.
    expect(bubble("agent").getAttribute("aria-label")).toBeNull();
  });

  it("makes the details beside the conversation as wide as you drag them (D-175)", async () => {
    localStorage.removeItem("otter.task-detail-width");
    localStorage.removeItem("otter.task-details");
    await render(mockSource("mac"));
    const body = q(".task-body")!;
    expect(body.style.getPropertyValue("--detail-width")).toBe("380px");
    // Open (D-177), its handle is on its left edge: ← widens it, and it is remembered.
    await act(async () => q<HTMLButtonElement>(".otter-chip")!.click());
    const grip = q('[role="separator"][aria-label="Details width"]')!;
    await act(async () => grip.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })));
    expect(body.style.getPropertyValue("--detail-width")).toBe("396px");
    expect(localStorage.getItem("otter.task-detail-width")).toBe("396");
    localStorage.removeItem("otter.task-details");
  });

  it("sends on Enter, but not the Enter that confirms an input method's candidate", async () => {
    const base = mockSource("mac");
    const sent: string[] = [];
    const s: TaskSource = {
      ...base,
      send: async (key, text) => {
        sent.push(text);
        return base.send(key, text);
      },
    };
    await render(s);
    await act(async () => q('[aria-label="New task"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "CSV export");
    await act(async () => byText("button", "Create draft")!.click());
    const composer = q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
    const press = async (init: KeyboardEventInit & { keyCode?: number }) => {
      const e = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true, ...init });
      if (init.keyCode !== undefined) Object.defineProperty(e, "keyCode", { get: () => init.keyCode });
      await act(async () => composer.dispatchEvent(e));
    };
    const compose = async (type: "compositionstart" | "compositionend") =>
      act(async () => composer.dispatchEvent(new CompositionEvent(type, { bubbles: true })));
    const typeIn = async (t: string) => act(async () => type(composer, t));
    const wait = () => act(async () => new Promise((r) => setTimeout(r, 80)));

    await typeIn("还没写完");
    await press({ isComposing: true });
    await press({ keyCode: 229 });
    // WebKit: the confirming Enter comes after compositionend, flags cleared.
    await compose("compositionstart");
    await compose("compositionend");
    await press({});
    expect(sent).toEqual([]);
    expect(composer.value).toBe("还没写完");

    // Shift+Enter is a newline, not a send.
    await wait();
    await press({ shiftKey: true });
    expect(sent).toEqual([]);

    // A later, plain Enter sends.
    await press({});
    expect(sent).toEqual(["还没写完"]);
  });

  it("keeps the details closed until Otter at the top is clicked, and remembers it (D-177)", async () => {
    localStorage.removeItem("otter.task-details");
    await render(mockSource("mac"));
    await open("Export workspace timeline as CSV");
    const otter = q<HTMLButtonElement>(".otter-chip")!;
    const details = q("#task-details")!;
    expect(otter.getAttribute("aria-expanded")).toBe("false");
    expect(details.hidden).toBe(true);
    // Being worked on: Otter says so (and moves).
    expect(otter.classList.contains("working")).toBe(true);
    expect(q(".otter-chip-state")?.textContent).toBe("working");

    await act(async () => otter.click());
    expect(otter.getAttribute("aria-expanded")).toBe("true");
    expect(details.hidden).toBe(false);
    expect(localStorage.getItem("otter.task-details")).toBe("open");
    // Otter again closes them; so does ×.
    await act(async () => otter.click());
    expect(details.hidden).toBe(true);
    await act(async () => otter.click());
    expect(details.hidden).toBe(false);
    await act(async () => q<HTMLButtonElement>('[aria-label="Hide details"]')!.click());
    expect(details.hidden).toBe(true);
    expect(localStorage.getItem("otter.task-details")).toBe("closed");
    localStorage.removeItem("otter.task-details");
  });

  it("adds pictures from the + as from a paste (D-177)", async () => {
    const base = mockSource("mac");
    const sent: unknown[] = [];
    const s: TaskSource = {
      ...base,
      send: async (key, text, images) => {
        sent.push(images);
        return base.send(key, text, images);
      },
      artifact: async () => "data:image/png;base64,AAAA",
    };
    await render(s);
    await act(async () => q('[aria-label="New task"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "From a file");
    await act(async () => byText("button", "Create draft")!.click());
    const picker = q<HTMLInputElement>('.composer input[type="file"]')!;
    const opened = vi.spyOn(picker, "click").mockImplementation(() => {});
    await act(async () => q<HTMLButtonElement>('[aria-label="Add pictures"]')!.click());
    expect(opened).toHaveBeenCalled();
    Object.defineProperty(picker, "files", { configurable: true, value: [new File(["x"], "s.png", { type: "image/png" })] });
    await act(async () => picker.dispatchEvent(new Event("change", { bubbles: true })));
    expect(host.querySelectorAll('[aria-label="Pictures to send"] img')).toHaveLength(1);
    await act(async () => q<HTMLButtonElement>('button[aria-label="Send"]')!.click());
    expect(sent).toEqual([[{ media_type: "image/png", data: "AAAA" }]]);
  });

  it("takes a pasted picture into the message, shows it, and sends it with or without words", async () => {
    const base = mockSource("mac");
    const sent: { text: string; images: unknown }[] = [];
    const s: TaskSource = {
      ...base,
      send: async (key, text, images) => {
        sent.push({ text, images });
        return base.send(key, text, images);
      },
      artifact: async () => "data:image/png;base64,AAAA",
    };
    await render(s);
    await act(async () => q('[aria-label="New task"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "Match the mock");
    await act(async () => byText("button", "Create draft")!.click());
    const composer = q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
    const paste = async (items: { kind: string; type: string; getAsFile?: () => File | null }[]) => {
      const e = new Event("paste", { bubbles: true, cancelable: true });
      Object.defineProperty(e, "clipboardData", { value: { items } });
      await act(async () => composer.dispatchEvent(e));
      return e;
    };
    const picture = () => ({ kind: "file", type: "image/png", getAsFile: () => new File(["x"], "s.png", { type: "image/png" }) });
    const thumbs = () => host.querySelectorAll('[aria-label="Pictures to send"] img');
    const send = () => q<HTMLButtonElement>('button[aria-label="Send"]')!;

    // Text on the clipboard (alone, or with a picture, as a copied table has) pastes as text.
    expect((await paste([{ kind: "string", type: "text/plain" }])).defaultPrevented).toBe(false);
    expect((await paste([{ kind: "string", type: "text/plain" }, picture()])).defaultPrevented).toBe(false);
    expect(thumbs()).toHaveLength(0);
    expect(send().disabled).toBe(true);

    // A picture alone is taken, shown, and is enough to send.
    expect((await paste([picture()])).defaultPrevented).toBe(true);
    expect(thumbs()).toHaveLength(1);
    expect(send().disabled).toBe(false);
    // And it can be taken back out.
    await act(async () => q<HTMLButtonElement>('[aria-label="Remove picture 1"]')!.click());
    expect(thumbs()).toHaveLength(0);
    expect(send().disabled).toBe(true);

    // Sent with words: both go, the box is empty after, and the conversation shows the picture.
    await paste([picture()]);
    await act(async () => type(composer, "make it look like this"));
    await act(async () => send().click());
    expect(sent).toEqual([{ text: "make it look like this", images: [{ media_type: "image/png", data: "AAAA" }] }]);
    expect(thumbs()).toHaveLength(0);
    expect(composer.value).toBe("");
    await vi.waitFor(() => expect(host.querySelector(".message-picture")).toBeTruthy());

    // Sent with no words at all.
    await paste([picture()]);
    await act(async () => send().click());
    expect(sent[1]).toEqual({ text: "", images: [{ media_type: "image/png", data: "AAAA" }] });
  });

  it("waits for a picture still being made to fit, and doesn't show the placeholder over pictures", async () => {
    const { prepareImage } = await import("./pastedImage");
    let ready: (v: { media_type: string; data: string }) => void = () => {};
    vi.mocked(prepareImage).mockImplementationOnce(() => new Promise((r) => (ready = r)));
    const base = mockSource("mac");
    const sent: string[] = [];
    const s: TaskSource = {
      ...base,
      send: async (key, text, images) => {
        sent.push(`${text}|${images?.length ?? 0}`);
        return base.send(key, text, images);
      },
      artifact: async () => "data:image/png;base64,AAAA",
    };
    await render(s);
    await act(async () => q('[aria-label="New task"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "Slow picture");
    await act(async () => byText("button", "Create draft")!.click());
    const composer = q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
    const e = new Event("paste", { bubbles: true, cancelable: true });
    const item = { kind: "file", type: "image/png", getAsFile: () => new File(["x"], "s.png", { type: "image/png" }) };
    Object.defineProperty(e, "clipboardData", { value: { items: [item] } });
    await act(async () => composer.dispatchEvent(e));
    await act(async () => type(composer, "see this"));
    // Still being made: neither the button nor Enter sends the words alone.
    const send = q<HTMLButtonElement>('button[aria-label="Send"]')!;
    expect(send.disabled).toBe(true);
    await act(async () => composer.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true })));
    expect(sent).toEqual([]);
    await act(async () => ready({ media_type: "image/png", data: "AAAA" }));
    expect(send.disabled).toBe(false);
    await act(async () => send.click());
    expect(sent).toEqual(["see this|1"]);

    // Pictures alone: the placeholder the host stores isn't printed over them.
    await act(async () => composer.dispatchEvent(e));
    await act(async () => send.click());
    await vi.waitFor(() => expect(host.querySelectorAll(".message-picture").length).toBe(2));
    expect(host.textContent).not.toContain("(pictures, no words)");
  });

  it("takes at most four pictures in a message", async () => {
    await render(mockSource("mac"));
    await act(async () => q('[aria-label="New task"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "Many pictures");
    await act(async () => byText("button", "Create draft")!.click());
    const composer = q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
    const e = new Event("paste", { bubbles: true, cancelable: true });
    const item = { kind: "file", type: "image/png", getAsFile: () => new File(["x"], "s.png", { type: "image/png" }) };
    Object.defineProperty(e, "clipboardData", { value: { items: Array(6).fill(item) } });
    await act(async () => composer.dispatchEvent(e));
    expect(host.querySelectorAll('[aria-label="Pictures to send"] img')).toHaveLength(4);
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

  it("shows the coding agent's work in Activity and offers pause and stop & send while it runs", async () => {
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
    // Stopping is beside the message box, not in the header (D-180).
    const header = q("header")!;
    for (const label of ["Pause", "Interrupt", "Preview", "Open preview"]) {
      expect(Array.from(header.querySelectorAll("button")).some((b) => b.textContent === label)).toBe(false);
    }
    await act(async () => byText(".composer-row button", "Pause")!.click());
    expect(acts).toContainEqual({ action: "pause" });
    const composer = q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
    type(composer, "Use the existing icon");
    await act(async () => byText(".composer-row button", "Stop & send")!.click());
    expect(acts).toContainEqual({ action: "redirect", text: "Use the existing icon" });
    // Cancelling is in the menu now.
    await menu();
    await act(async () => byText('[role="menuitem"]', "Cancel task")!.click());
    expect(acts).toContainEqual({ action: "cancel" });
  });

  it("offers neither where nothing runs", async () => {
    await render(mockSource("mac"));
    await open("Show host uptime");
    expect(byText(".composer-row button", "Pause")).toBeUndefined();
    expect(byText(".composer-row button", "Stop & send")).toBeUndefined();
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
    // Its one line says it all: no block for the whole command.
    expect(q(".pending-decisions .decision-command")).toBeNull();
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

  it("shows the whole command an approval is about when its one line cuts it (D-157)", async () => {
    const [blocked] = sampleTasks().filter((f) => f.status === "blocked");
    const command = "python3 - <<'E'\nimport re\nprint('rewrote')\nE\nGITHUB_TOKEN=*** rm -rf /opt/shared";
    const d = {
      ...blocked.decisions[0],
      summary: "Run `python3 - <<'E' import re print('rewrote') E …`",
      detail: "deletes or destroys something (`rm`)",
      command,
    };
    const acts = await single({ ...blocked, decisions: [d] });
    const pending = q(".pending-decisions")!;
    expect(pending.querySelector(".decision-summary")?.textContent).toBe(d.summary);
    // As written, its lines kept, under the one line and the reason.
    const block = pending.querySelector('[aria-label="The whole command"]');
    expect(block?.textContent).toBe(command);
    expect(block?.previousElementSibling?.textContent).toBe(d.detail);
    await act(async () => byText(".pending-decisions button", "Deny")!.click());
    expect(acts).toContainEqual({ action: "decide", decision_id: d.id, approve: false });
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

  it("says in Now when the agent was last heard from and what comes next", async () => {
    const [active] = sampleTasks().filter((f) => f.status === "implementing");
    const heard = new Date(Date.now() - 125_000).toISOString();
    const run = {
      ...active.runs[0],
      heard_at: heard,
      activity: ["Read src/csv.rs", "Bash cargo test"],
      activity_at: [new Date(Date.now() - 300_000).toISOString(), heard],
    };
    await single({ ...active, runs: [run], next: "Then: the work is checked against its 2 acceptance criteria." });
    const now = byText(".ov-section", "Last heard")!;
    expect(now.querySelector(".ov-heard")?.textContent).toBe("Last heard from the coding agent 2m ago.");
    expect(now.querySelector(".ov-next")?.textContent).toBe("Then: the work is checked against its 2 acceptance criteria.");
    // Each line of what it did with its time.
    expect([...host.querySelectorAll(".activity li")].map((l) => l.textContent)).toEqual(["5m Read src/csv.rs", "2m Bash cargo test"]);
  });

  it("puts Otter's reasons in the history at their time, a repeat once", async () => {
    const [done] = sampleTasks().filter((f) => f.status === "done");
    const base = mockSource("mac");
    const task = { ...done, rationale: ["Planned it.", "Answered.", "Answered."] };
    const ev = (seq: number, type: string, text: string): TaskEventRecord => ({ seq, ts: new Date(Date.now() - (10 - seq) * 1000).toISOString(), v: 1, type, text });
    const s: TaskSource = {
      ...base,
      list: () => [{ host: "mac", task, key: "mac/" + task.id }],
      subscribe: () => () => {},
      history: async () => [ev(1, "Reasoned", "Why: Planned it."), ev(2, "Reasoned", "Why: Answered."), ev(3, "Reasoned", "Why: Answered."), ev(4, "StatusChanged", "Done")],
    };
    await render(s);
    await act(async () => byText('[role="tab"]', "Activity")!.click());
    const lines = [...host.querySelectorAll(".feed-event.reason")].map((l) => l.textContent);
    expect(lines).toHaveLength(2);
    expect(lines[0]).toContain("Why: Answered. ×2");
    // Already in the feed: no fold of them as well.
    expect(q(".reasons")).toBeNull();
  });

  it("links a sub-task to the task it's part of, and a task to its sub-tasks (D-194)", async () => {
    const base = mockSource("mac");
    const [active, , , done, draft] = sampleTasks();
    const tasks: Task[] = [
      active,
      { ...done, id: "ft_child1", title: "Write the CSV docs", parent: active.id, created_at: "2026-10-22T10:00:00Z" },
      { ...draft, id: "ft_child2", title: "Add an export test", parent: active.id, created_at: "2026-10-22T11:00:00Z" },
      { ...draft, id: "ft_orphan", title: "Left over", parent: "ft_gone" },
    ];
    await render({ ...base, list: () => tasks.map((task) => ({ host: "mac", task, key: `mac/${task.id}` })), subscribe: () => () => {} });
    await open(active.title);
    // Its parts, each where it stands; it's part of nothing.
    const parts = q('[aria-label="Sub-tasks"]')!;
    expect(parts.querySelector(".count")?.textContent).toBe("1 of 2 done");
    expect([...parts.querySelectorAll("li")].map((li) => li.textContent)).toEqual(["Write the CSV docsDone", "Add an export testDraft"]);
    expect(q(".task-parent")).toBeNull();
    await act(async () => byText('[aria-label="Sub-tasks"] button', "Add an export test")!.click());
    expect(q(".pane-head h1")?.textContent).toBe("Add an export test");
    expect(q(".task-parent")?.textContent).toBe(`Part of ${active.title}`);
    expect(q('[aria-label="Sub-tasks"]')).toBeNull();
    await act(async () => q<HTMLButtonElement>(".task-parent button")!.click());
    expect(q(".pane-head h1")?.textContent).toBe(active.title);
    // One whose task isn't here says so, with nothing to click.
    await open("Left over");
    expect(q(".task-parent")?.textContent).toBe("Part of another task");
    expect(q(".task-parent button")).toBeNull();
  });

  it("shows what Otter did and what undoes it in Activity (D-193)", async () => {
    const [done] = sampleTasks().filter((f) => f.status === "done");
    const base = mockSource("mac");
    const ev = (seq: number, type: string, text: string, extra: object = {}): TaskEventRecord =>
      Object.assign({ seq, ts: new Date(Date.now() - (10 - seq) * 1000).toISOString(), v: 1, type, text }, extra);
    const s: TaskSource = {
      ...base,
      list: () => [{ host: "mac", task: done, key: "mac/" + done.id }],
      subscribe: () => () => {},
      history: async () => [
        ev(1, "ToolCalled", "Otter: set_workspace: demo", { tool: "set_workspace", origin: "developer", detail: "demo" }),
        ev(2, "ToolUndo", "Otter: set_workspace — can be undone", {
          tool: "set_workspace",
          task_id: done.id,
          action: { action: "set_workspace", workspace: "ws_old" },
        }),
        ev(3, "ToolDone", "Otter: set_workspace — done", { tool: "set_workspace" }),
      ],
    };
    await render(s);
    await act(async () => byText('[role="tab"]', "Activity")!.click());
    const lines = [...host.querySelectorAll(".feed-event")].map((l) => l.lastElementChild?.textContent);
    expect(lines).toEqual(["Otter: set_workspace — done", "Otter: set_workspace — can be undone", "Otter: set_workspace: demo"]);
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

  it("shows what the task changed in its workspace, and a file's diff", async () => {
    await render(mockSource("mac"));
    await open("Export workspace timeline");
    await act(async () => new Promise((r) => setTimeout(r, 350)));
    expect(sections()).toEqual(["Now", "Progress", "Changes", "Checks", "Request"]);
    const changes = q('.ov-section[aria-label="Changes"]')!;
    expect(changes.querySelector(".count")?.textContent).toBe("2 files · +65 −3 · ↑2");
    await act(async () => byText(".files-row", "timelineCsv.ts")!.click());
    expect(changes.querySelector(".files-where")?.textContent).toBe("apps/desktop/src/timelineCsv.ts");
    expect(changes.querySelector(".diff-view")).not.toBeNull();
    await act(async () => q('[aria-label="Back to the changes"]')!.click());
    expect(changes.querySelectorAll(".files-row")).toHaveLength(2);
  });

  it("leaves Changes out where the host can't say", async () => {
    await render({ ...mockSource("mac"), changes: async () => undefined });
    await open("Export workspace timeline");
    await act(async () => new Promise((r) => setTimeout(r, 350)));
    expect(sections()).not.toContain("Changes");
  });

  it("says a task waiting for an agent you started needs you, and opens that session", async () => {
    const [active] = sampleTasks(Date.now());
    const task: Task = {
      ...active,
      status: "planning",
      steps: [],
      runs: [],
      workspace_id: "ws_demo",
      status_reason: "Waiting for demo: claude, an agent you started, is working there",
      activity: "needs_you",
      needs: { kind: "blocked", summary: "Waiting on you", since: active.updated_at, decisions: 0 },
    };
    const s: TaskSource = { ...mockSource("mac"), list: () => [{ host: "mac", task, key: "mac/" + task.id }], subscribe: () => () => {} };
    await act(async () =>
      root.render(
        <TasksView
          source={s}
          hosts={["mac"]}
          workspaces={{ mac: [{ id: "ws_demo", name: "demo", sessions: [{ id: "ses_c", name: "claude" }] }] }}
          now={Date.now()}
          onOpenWorkspace={onOpenWorkspace}
        />,
      ),
    );
    expect(q(".status-pill")?.textContent).toBe("Needs you");
    expect(q(".banner.needs")?.textContent).toContain("an agent you started");
    await act(async () => byText(".banner button", "Open claude")!.click());
    expect(onOpenWorkspace).toHaveBeenCalledWith("mac", "ws_demo", "ses_c");
    expect(byText(".banner button", "Work in its own workspace")).toBeDefined();
  });

  it("says a task waiting behind another task's unfinished work needs you, and offers its own workspace (D-156)", async () => {
    const [active] = sampleTasks(Date.now());
    const task: Task = {
      ...active,
      status: "planning",
      steps: [],
      runs: [],
      workspace_id: "ws_demo",
      status_reason: `Waiting for demo: “Fix the export”${HELD}`,
      activity: "needs_you",
      needs: { kind: "blocked", summary: "Waiting on you", since: active.updated_at, decisions: 0 },
    };
    const s: TaskSource = { ...mockSource("mac"), list: () => [{ host: "mac", task, key: "mac/" + task.id }], subscribe: () => () => {} };
    await act(async () =>
      root.render(
        <TasksView
          source={s}
          hosts={["mac"]}
          workspaces={{ mac: [{ id: "ws_demo", name: "demo", sessions: [] }] }}
          now={Date.now()}
          onOpenWorkspace={async () => {}}
        />,
      ),
    );
    expect(q(".status-pill")?.textContent).toBe("Needs you");
    expect(q(".banner.needs")?.textContent).toContain("has unfinished work there");
    expect(byText(".banner button", "Work in its own workspace")).toBeDefined();
  });

  /** A new task on a source that notes where it was made (D-179). */
  async function newTaskOn(hosts: string[], workspaces: Record<string, { id: string; name: string; repository?: string }[]>) {
    const created: unknown[][] = [];
    const base = mockSource(hosts[0]);
    const s: TaskSource = {
      ...base,
      create: async (...args) => {
        created.push(args);
        return base.create(...args);
      },
    };
    await act(async () =>
      root.render(
        <TasksView source={s} hosts={hosts} workspaces={workspaces} now={Date.now()} onOpenWorkspace={onOpenWorkspace} />,
      ),
    );
    await act(async () => q('[aria-label="New task"]')!.click());
    return { created, dialog: document.querySelector('[role="dialog"]')! };
  }
  const choose = (el: Element, value: string) => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value")!.set!.call(el, value);
    el.dispatchEvent(new Event("change", { bubbles: true }));
  };

  it("sends a new task to the host that has the workspace it names, and says so (D-179)", async () => {
    localStorage.removeItem("otter.new-task-host");
    const { created, dialog } = await newTaskOn(["mac", "gpu"], {
      mac: [{ id: "ws_a", name: "billing", repository: "git@github.com:acme/billing.git" }],
      gpu: [{ id: "ws_b", name: "trainer", repository: "git@github.com:acme/trainer.git" }],
    });
    expect(dialog.querySelector("#new-task-host-hint")?.textContent).toContain("Nothing written names a host; using mac");
    type(dialog.querySelector("input")!, "Speed up trainer");
    await act(async () => {});
    expect(dialog.querySelector("#new-task-host-hint")?.textContent).toBe("Goes to gpu: it has workspace trainer.");
    await act(async () => byText("button", "Create draft")!.click());
    expect(created).toEqual([["gpu", "Speed up trainer", "", undefined, false]]);
    // The next task starts from where this one went.
    await act(async () => q('[aria-label="New task"]')!.click());
    expect(document.querySelector('[role="dialog"] #new-task-host-hint')?.textContent).toContain("using gpu");
    localStorage.removeItem("otter.new-task-host");
  });

  /** newTaskOn, with the actions it was asked for recorded. */
  async function startingOn(failStart = false) {
    const acted: unknown[][] = [];
    const created: unknown[][] = [];
    const base = mockSource("mac");
    const s: TaskSource = {
      ...base,
      create: async (...args) => {
        created.push(args);
        return base.create(...args);
      },
      act: async (key, action) => {
        acted.push([key, action]);
        if (failStart) throw new Error("no");
        return base.act(key, action);
      },
    };
    await act(async () =>
      root.render(<TasksView source={s} hosts={["mac"]} workspaces={{ mac: [] }} now={Date.now()} onOpenWorkspace={async () => {}} />),
    );
    await act(async () => q('[aria-label="New task"]')!.click());
    return { acted, created, dialog: document.querySelector('[role="dialog"]')! };
  }

  it("creates and starts a task from the request alone", async () => {
    const { acted, created, dialog } = await startingOn();
    expect(byText("button", "Create and start")!.hasAttribute("disabled")).toBe(true);
    type(dialog.querySelector("textarea")!, "Fix the retry bug in billing\nIt drops the second attempt.");
    await act(async () => {});
    await act(async () => byText("button", "Create and start")!.click());
    // No title written: the request's first line, and no workspace chosen.
    expect(created).toEqual([["mac", "Fix the retry bug in billing", "Fix the retry bug in billing\nIt drops the second attempt.", undefined, false]]);
    expect(acted).toEqual([[expect.any(String), { action: "start" }]]);
    expect(q(".status-pill")?.textContent).toBe("Working");
  });

  it("starts a task from the chat box of the empty page, beside a cute Otter", async () => {
    const acted: unknown[][] = [];
    const created: unknown[][] = [];
    const base = mockSource("mac");
    const s: TaskSource = {
      ...base,
      list: () => [],
      create: async (...args) => {
        created.push(args);
        return base.create(...args);
      },
      act: async (key, action) => {
        acted.push([key, action]);
      },
    };
    await act(async () =>
      root.render(<TasksView source={s} hosts={["mac"]} workspaces={{ mac: [] }} now={Date.now()} onOpenWorkspace={async () => {}} />),
    );
    expect(q(".otter-hello svg")).toBeTruthy();
    const box = q<HTMLTextAreaElement>('textarea[aria-label="Tell Otter what you want"]')!;
    expect(byText("button", "Start")!.hasAttribute("disabled")).toBe(true);
    type(box, "Add retries to the billing client");
    await act(async () => byText("button", "Start")!.click());
    expect(created).toEqual([["mac", "Add retries to the billing client", "Add retries to the billing client", undefined, false]]);
    expect(acted).toEqual([[expect.any(String), { action: "start" }]]);
  });

  it("creates a draft that is not started when asked for one", async () => {
    const { acted, dialog } = await startingOn();
    type(dialog.querySelector("textarea")!, "Look into the flaky test");
    await act(async () => {});
    await act(async () => byText("button", "Create draft")!.click());
    expect(acted).toEqual([]);
    expect(q(".status-pill")?.textContent).toBe("Draft");
  });

  it("keeps the task as a draft when starting it fails", async () => {
    const { acted, dialog } = await startingOn(true);
    type(dialog.querySelector("textarea")!, "Look into the flaky test");
    await act(async () => {});
    await act(async () => byText("button", "Create and start")!.click());
    expect(acted).toHaveLength(1);
    expect(q(".status-pill")?.textContent).toBe("Draft");
  });

  it("does not carry a workspace to another host when the text moves the task there", async () => {
    localStorage.removeItem("otter.new-task-host");
    const { created, dialog } = await newTaskOn(["mac", "gpu"], {
      mac: [{ id: "ws_a", name: "billing" }],
      gpu: [{ id: "ws_b", name: "trainer" }],
    });
    choose(dialog.querySelectorAll("select")[1], "ws_a");
    await act(async () => {});
    type(dialog.querySelector("input")!, "Speed up trainer");
    await act(async () => {});
    await act(async () => byText("button", "Create draft")!.click());
    expect(created).toEqual([["gpu", "Speed up trainer", "", undefined, false]]);
    localStorage.removeItem("otter.new-task-host");
  });

  it("lets a host chosen by hand win over what is written", async () => {
    const { created, dialog } = await newTaskOn(["mac", "gpu"], { mac: [], gpu: [{ id: "ws_b", name: "trainer" }] });
    type(dialog.querySelector("input")!, "Speed up trainer");
    choose(dialog.querySelectorAll("select")[0], "mac");
    await act(async () => {});
    expect(dialog.querySelector("#new-task-host-hint")).toBeNull();
    await act(async () => byText("button", "Create draft")!.click());
    expect(created[0][0]).toBe("mac");
    localStorage.removeItem("otter.new-task-host");
  });

  it("makes the task a workspace from a repository, or an empty directory (D-179)", async () => {
    const { created, dialog } = await newTaskOn(["mac"], { mac: [{ id: "ws_a", name: "billing" }] });
    type(dialog.querySelector("input")!, "Retries");
    const select = dialog.querySelector("select")!;
    choose(select, "@repository");
    await act(async () => {});
    expect(byText("button", "Create draft")!.hasAttribute("disabled")).toBe(true);
    type(dialog.querySelector('input[aria-label="Repository"]') as HTMLInputElement, "git@github.com:acme/billing.git");
    await act(async () => {});
    await act(async () => byText("button", "Create draft")!.click());
    expect(created[0]).toEqual(["mac", "Retries", "", { new: { repository: "git@github.com:acme/billing.git" } }, false]);

    await act(async () => q('[aria-label="New task"]')!.click());
    const again = document.querySelector('[role="dialog"]')!;
    type(again.querySelector("input")!, "CSV");
    choose(again.querySelector("select")!, "@empty");
    await act(async () => {});
    await act(async () => byText("button", "Create draft")!.click());
    expect(created[1]).toEqual(["mac", "CSV", "", { scratch: {} }, false]);
  });

  it("links a task to its workspace and session", async () => {
    await render(mockSource("mac"));
    await open("Export workspace timeline");
    await act(async () => byText(".ov-section button", "Open session")!.click());
    expect(onOpenWorkspace).toHaveBeenCalledWith("mac", "ws_demo", "ses_demo");
  });
});
