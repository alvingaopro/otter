import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { OtterView, tasksNamed } from "./OtterView";
import type { OtterChat, OtterDraft, OtterSource } from "./otterChat";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

let root: Root;
let host: HTMLDivElement;
const flush = () => act(async () => new Promise((r) => setTimeout(r, 0)));
const q = <T extends Element = HTMLElement>(sel: string) => host.querySelector<T>(sel);

function type(el: HTMLTextAreaElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

/** A source with a conversation per host, and a hook to stream a draft. */
function fakeSource(opts: { tooOld?: string[] } = {}) {
  const chats: Record<string, OtterChat> = {};
  const drafts: Record<string, (d: OtterDraft) => void> = {};
  const changes: Record<string, () => void> = {};
  const sent: { host: string; text: string }[] = [];
  const source: OtterSource = {
    async get(h) {
      if (opts.tooOld?.includes(h)) throw new Error("unknown variant `otter.get`");
      return chats[h] ?? { messages: [], history_seq: 0 };
    },
    async send(h, text) {
      sent.push({ host: h, text });
      const c = chats[h] ?? { messages: [], history_seq: 0 };
      chats[h] = { ...c, messages: [...c.messages, { id: `msg_${c.messages.length}`, role: "user", text, at: new Date().toISOString() }] };
      return chats[h];
    },
    watch(h, onChange, onDraft) {
      drafts[h] = onDraft;
      changes[h] = onChange;
      return () => {};
    },
  };
  const reply = (h: string, text: string) => {
    const c = chats[h];
    const to = c.messages[c.messages.length - 1];
    chats[h] = { ...c, messages: [...c.messages, { id: `msg_r${c.messages.length}`, role: "controller", text, at: new Date().toISOString(), correlation_id: to.id }] };
    changes[h]?.();
  };
  return { source, sent, drafts, reply };
}

async function render(el: React.ReactElement) {
  await act(async () => root.render(el));
  await flush();
}

describe("Otter on a host", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    localStorage.clear();
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  it("finds the tasks a reply names", () => {
    expect(tasksNamed("Made [ft_ab12] and [ft_cd34]; ft_ab12 starts now.")).toEqual(["ft_ab12", "ft_cd34"]);
    expect(tasksNamed("nothing here")).toEqual([]);
  });

  it("talks to Otter on the host shown, streams its reply, and opens a task it names", async () => {
    const f = fakeSource();
    const opened: string[] = [];
    await render(<OtterView hosts={["mac", "box"]} source={f.source} now={Date.now()} onOpenTask={(h, t) => opened.push(`${h}/${t}`)} />);
    expect(q<HTMLSelectElement>('select[aria-label="Host"]')!.value).toBe("mac");
    expect(host.textContent).toContain("Ask Otter about the tasks on mac");
    type(q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter on this host"]')!, "Fix the login bug in api");
    await act(async () => [...host.querySelectorAll("button")].find((b) => b.textContent === "Send")!.click());
    await flush();
    expect(f.sent).toEqual([{ host: "mac", text: "Fix the login bug in api" }]);
    expect(host.textContent).toContain("Otter is replying…");
    // Written as it comes, then the message.
    await act(async () => f.drafts.mac({ stream_id: "reply-1", text: "Making it", done: false }));
    expect(q(".message.controller.streaming")?.textContent).toContain("Making it");
    await act(async () => f.reply("mac", "Made **Fix the login bug** [ft_ab12], started."));
    await flush();
    const chip = [...host.querySelectorAll("button")].find((b) => b.textContent === "Open ft_ab12")!;
    await act(async () => chip.click());
    expect(opened).toEqual(["mac/ft_ab12"]);
  });

  it("says when a host's otterd is too old, and keeps the host chosen", async () => {
    const f = fakeSource({ tooOld: ["old"] });
    await render(<OtterView hosts={["mac", "old"]} source={f.source} now={Date.now()} onOpenTask={() => {}} />);
    const select = q<HTMLSelectElement>('select[aria-label="Host"]')!;
    select.value = "old";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    await flush();
    expect(host.textContent).toContain("too old for Otter's conversation");
    expect(q<HTMLTextAreaElement>("textarea")!.disabled).toBe(true);
    expect(localStorage.getItem("otter.chat.host")).toBe("old");
  });

  it("asks for a host when none is connected", async () => {
    await render(<OtterView hosts={[]} source={fakeSource().source} now={Date.now()} onOpenTask={() => {}} />);
    expect(host.textContent).toContain("Connect a host first");
  });
});
