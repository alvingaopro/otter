import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { TasksView } from "./TasksView";
import { mockSource, type TaskSource } from "./taskSource";

// Slash commands in the chat boxes (D-190).

let root: Root;
let host: HTMLDivElement;

const q = <T extends Element = HTMLElement>(sel: string) => host.querySelector<T & HTMLElement>(sel);
const all = (sel: string) => [...host.querySelectorAll<HTMLElement>(sel)];
const byText = (sel: string, text: string) => all(sel).find((e) => e.textContent?.includes(text));

function type(el: HTMLTextAreaElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

/** A source that says what was asked of it, and otherwise is the preview's. */
function recording(over: Partial<TaskSource> = {}) {
  const base = mockSource("mac");
  const acts: unknown[] = [];
  const sent: unknown[][] = [];
  const created: unknown[][] = [];
  const s: TaskSource = {
    ...base,
    act: async (key, action) => {
      acts.push(action);
      return base.act(key, action);
    },
    send: async (key, text, images, command) => {
      sent.push([text, command]);
      return base.send(key, text, images, command);
    },
    create: async (...args) => {
      created.push(args);
      return base.create(...args);
    },
    ...over,
  };
  return { s, acts, sent, created };
}

async function render(s: TaskSource) {
  await act(async () =>
    root.render(<TasksView source={s} hosts={["mac"]} workspaces={{ mac: [{ id: "ws_demo", name: "demo" }] }} now={Date.now()} onOpenWorkspace={async () => {}} />),
  );
}

const open = async (title: string) => act(async () => byText('[role="listitem"]', title)!.click());
const box = () => q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
const write = async (text: string, el = box()) => act(async () => type(el, text));
async function press(key: string, init: KeyboardEventInit & { keyCode?: number } = {}, el = box()) {
  const e = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init });
  if (init.keyCode !== undefined) Object.defineProperty(e, "keyCode", { get: () => init.keyCode });
  await act(async () => el.dispatchEvent(e));
}
const rows = () => all(".slash-row").map((r) => r.querySelector(".slash-cmd")!.textContent);
const highlighted = () => q(".slash-row.on .slash-cmd")?.textContent;
const hint = () => q(".slash-hint")?.textContent;

describe("slash commands (D-190)", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
    localStorage.removeItem("otter.task-details");
  });

  it("lists every command on a /, the ones that can't be used now dimmed with why", async () => {
    await render(recording().s);
    await open("Export workspace timeline as CSV");
    expect(box().placeholder).toContain("/ for commands");
    await write("/");
    expect(all(".slash-group").map((g) => g.textContent)).toEqual(["Task", "Tell", "Show"]);
    expect(rows()).toContain("/pause");
    expect(rows()).toContain("/delete");
    const resume = byText(".slash-row", "/resume")!;
    expect(resume.classList.contains("off")).toBe(true);
    expect(resume.getAttribute("aria-disabled")).toBe("true");
    expect(resume.querySelector(".slash-why")?.textContent).toBe("Not paused");
    // The first that can be used is the one Enter runs; ↓ skips the dimmed.
    expect(highlighted()).toBe("/pause");
    await press("ArrowDown");
    expect(highlighted()).toBe("/stop what next");
    // Narrowed as it is typed; Tab completes, ready for what follows.
    await write("/st");
    expect(rows()).toEqual(["/stop what next", "/start"]);
    await press("Tab");
    expect(box().value).toBe("/stop ");
    expect(q(".slash-menu")).toBeNull();
    expect(hint()).toContain("Enter stops the coding agent's turn and has it do this next");
    // Esc closes the menu; the words stay.
    await write("/");
    await press("Escape");
    expect(q(".slash-menu")).toBeNull();
    expect(box().value).toBe("/");
  });

  it("runs a command on Enter, never on the Enter that confirms an input method's candidate", async () => {
    const { s, acts, sent } = recording();
    await render(s);
    await open("Export workspace timeline as CSV");
    await write("/pause");
    await press("Enter", { isComposing: true });
    await press("Enter", { keyCode: 229 });
    expect(acts).toEqual([]);
    await press("Enter");
    expect(acts).toEqual([{ action: "pause" }]);
    expect(sent).toEqual([]);
    expect(box().value).toBe("");
    expect(q(".status-pill")?.textContent).toBe("Paused");
  });

  it("takes Pinyin's 、 for a / at the start", async () => {
    await render(recording().s);
    await open("Export workspace timeline as CSV");
    await write("、");
    expect(box().value).toBe("/");
    expect(q(".slash-menu")).not.toBeNull();
    // Inside a sentence it is punctuation.
    await write("好的、谢谢");
    expect(box().value).toBe("好的、谢谢");
    expect(q(".slash-menu")).toBeNull();
  });

  it("says what Enter will do and names the button for it; a command that can't be used now isn't sent", async () => {
    const { s, acts, sent } = recording();
    await render(s);
    await open("Export workspace timeline as CSV");
    await write("/stop use the existing icon");
    expect(hint()).toBe("/stopEnter stops the coding agent's turn and has it do this next");
    expect(q(".composer-cmd")?.textContent).toBe("Stop & send");
    expect(q('button[aria-label="Send"]')).toBeNull();
    // The buttons beside the box step aside for it.
    expect(byText(".composer-row button", "Pause")).toBeUndefined();
    await act(async () => q<HTMLButtonElement>(".composer-cmd")!.click());
    expect(acts).toEqual([{ action: "redirect", text: "use the existing icon" }]);

    await write("/resume");
    await press("Escape");
    expect(q(".slash-hint.warn")?.textContent).toBe("/resumeNot paused");
    expect(q<HTMLButtonElement>(".composer-cmd")!.disabled).toBe(true);
    await press("Enter");
    expect(acts).toHaveLength(1);
    expect(sent).toEqual([]);
    expect(box().value).toBe("/resume");
  });

  it("asks for Enter twice to cancel, and Esc keeps the task", async () => {
    const { s, acts } = recording();
    await render(s);
    await open("Export workspace timeline as CSV");
    await write("/cancel");
    await press("Enter");
    expect(acts).toEqual([]);
    expect(q(".slash-hint.warn")?.textContent).toContain("Enter again to cancel the task");
    await press("Escape");
    expect(box().value).toBe("");
    await write("/cancel");
    await press("Enter");
    await press("Enter");
    expect(acts).toEqual([{ action: "cancel" }]);
  });

  it("sends a message command with its words, and shows it as one", async () => {
    const { s, sent } = recording();
    await render(s);
    await open("Export workspace timeline as CSV");
    await write("/tell use tabs, not commas");
    await press("Enter");
    expect(sent).toEqual([["use tabs, not commas", { name: "tell" }]]);
    const mine = all(".message.user").slice(-1)[0];
    expect(mine.querySelector(".message-command")?.textContent).toBe("/tell");
    expect(mine.textContent).toContain("use tabs, not commas");
    // A choice that completes it: Enter on it sends.
    await write("/remind ");
    expect(rows()).toEqual(["15m", "30m", "1h", "2h", "4h"]);
    await press("ArrowDown");
    await press("Enter");
    expect(sent[sent.length - 1]).toEqual(["", { name: "remind", in_minutes: 30 }]);
  });

  it("sends what isn't a command as a message", async () => {
    const { s, sent } = recording();
    await render(s);
    await open("Export workspace timeline as CSV");
    await write("/xyz");
    expect(q(".slash-none")?.textContent).toContain("Enter sends it as a message");
    await press("Enter");
    await write("/usr is full on the host");
    expect(q(".slash-menu")).toBeNull();
    await press("Enter");
    expect(sent).toEqual([
      ["/xyz", undefined],
      ["/usr is full on the host", undefined],
    ]);
  });

  it("sets a workspace from the host's, and shows what it is asked to", async () => {
    const { s, acts } = recording();
    await render(s);
    await open("Dark-mode charts legend");
    await write("/workspace ");
    expect(rows()).toEqual(["demo"]);
    await press("Enter");
    expect(acts).toEqual([{ action: "set_workspace", workspace: "ws_demo" }]);
    await write("/details");
    await press("Enter");
    expect(q("#task-details")?.hidden).toBe(false);
    await write("/activity");
    await press("Enter");
    expect(q('[role="tab"][aria-selected="true"]')?.textContent).toBe("Activity");
  });

  it("puts the empty page's commands on the new task as conditions", async () => {
    const { s, acts, created } = recording({ list: () => [] });
    await render(s);
    const hello = q<HTMLTextAreaElement>('textarea[aria-label="Tell Otter what you want"]')!;
    await write("/", hello);
    expect(rows()).toEqual(["/in workspace", "/repo repository", "/scratch", "/answer-only", "/draft", "/host name"]);
    expect(byText(".slash-row", "/host")!.querySelector(".slash-why")?.textContent).toBe("Only one host");
    await write("/in ", hello);
    await press("Enter", {}, hello);
    await write("/answer-only", hello);
    await press("Enter", {}, hello);
    expect(all(".hello-option").map((o) => o.firstChild?.textContent)).toEqual(["in demo", "only an answer"]);
    // Where it works is one thing: the latest says.
    await write("/scratch why is the build slow", hello);
    await press("Escape", {}, hello);
    await press("Enter", {}, hello);
    expect(all(".hello-option").map((o) => o.firstChild?.textContent)).toEqual(["only an answer", "empty directory"]);
    expect(hello.value).toBe("why is the build slow");
    await press("Enter", {}, hello);
    expect(created).toEqual([["mac", "why is the build slow", "why is the build slow", { scratch: {} }, true]]);
    expect(acts).toEqual([{ action: "start" }]);
  });

  it("keeps a new task a draft when asked", async () => {
    const { s, acts, created } = recording({ list: () => [] });
    await render(s);
    const hello = q<HTMLTextAreaElement>('textarea[aria-label="Tell Otter what you want"]')!;
    await write("/draft", hello);
    await press("Enter", {}, hello);
    await write("Look into the flaky test", hello);
    expect(byText(".hello-row button", "Create draft")).toBeTruthy();
    await press("Enter", {}, hello);
    expect(created).toHaveLength(1);
    expect(acts).toEqual([]);
  });
});
