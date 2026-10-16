import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MemoryDialog } from "./MemoryDialog";
import { mockMemory, type MemorySource } from "./memory";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

let root: Root;
let host: HTMLDivElement;
const flush = () => act(async () => new Promise((r) => setTimeout(r, 0)));
const button = (text: string) => [...host.querySelectorAll("button")].find((b) => b.textContent === text)!;
const areas = () => [...host.querySelectorAll<HTMLTextAreaElement>("textarea")];

function type(el: HTMLTextAreaElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

async function render(el: React.ReactElement) {
  await act(async () => root.render(el));
  await flush();
}

describe("Otter's memory", () => {
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

  it("shows a task's memory and the host's, and saves only what changed", async () => {
    const source = mockMemory();
    await source.set("mac", undefined, "host", "- prefers small PRs", "");
    await source.set("mac", "ft_1", "conversation", "- Postgres, not SQLite", "");
    const set = vi.spyOn(source, "set");
    let closed = false;
    await render(<MemoryDialog source={source} host="mac" task="ft_1" conversation="This task" onClose={() => (closed = true)} />);
    expect(host.textContent).toContain("This task");
    expect(host.textContent).toContain("Every conversation on mac");
    const [conversation, hostMemory] = areas();
    expect(conversation.value).toBe("- Postgres, not SQLite\n");
    expect(hostMemory.value).toBe("- prefers small PRs\n");
    expect(button("Save").disabled).toBe(true);
    await act(async () => type(conversation, "- Postgres 16, not SQLite\n"));
    await act(async () => button("Save").click());
    await flush();
    expect(set).toHaveBeenCalledTimes(1);
    expect(set).toHaveBeenCalledWith("mac", "ft_1", "conversation", "- Postgres 16, not SQLite\n", "- Postgres, not SQLite\n");
    expect(closed).toBe(true);
    expect((await source.get("mac", "ft_1")).conversation).toBe("- Postgres 16, not SQLite\n");
  });

  it("says when Otter noted something meanwhile, and looks again", async () => {
    const source = mockMemory();
    await render(<MemoryDialog source={source} host="mac" conversation="This conversation" onClose={() => {}} />);
    await act(async () => type(areas()[1], "- mine"));
    await source.set("mac", undefined, "host", "- Otter's", "");
    await act(async () => button("Save").click());
    await flush();
    expect(host.textContent).toContain("Otter changed this memory");
    await act(async () => button("Look again").click());
    await flush();
    expect(areas()[1].value).toBe("- Otter's\n");
  });

  it("says when the host's otterd is too old", async () => {
    const source: MemorySource = {
      get: async () => {
        throw new Error("invalid_request: unknown variant `memory.get`");
      },
      set: async () => {
        throw new Error("unreachable");
      },
    };
    await render(<MemoryDialog source={source} host="old" conversation="This conversation" onClose={() => {}} />);
    expect(host.textContent).toContain("too old for Otter's memory");
    expect(areas()).toHaveLength(0);
  });
});
