import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mockSource, type TaskSource } from "./taskSource";
import { Inbox } from "./Inbox";
import { workspace } from "./inboxFixtures";

let root: Root;
let host: HTMLDivElement;
let source: TaskSource;
const onOpenTask = vi.fn();
const onOpenWorkspace = vi.fn();

const section = (label: string) => host.querySelector<HTMLElement>(`section[aria-label="${label}"]`);
const item = (title: string) => host.querySelector<HTMLElement>(`li[aria-label="${title}"]`);
const button = (el: HTMLElement | null, text: string) =>
  [...(el?.querySelectorAll<HTMLButtonElement>("button") ?? [])].find((b) => b.textContent?.trim() === text);

async function render() {
  source = mockSource("mac", Date.parse("2026-10-09T12:00:00Z"));
  await act(async () =>
    root.render(
      <Inbox
        source={source}
        workspaces={[workspace("api", "needs_you", 5), workspace("web", "working", 1)]}
        now={Date.parse("2026-10-09T12:00:00Z")}
        onOpenTask={onOpenTask}
        onOpenWorkspace={onOpenWorkspace}
      />,
    ),
  );
}

describe("Inbox", () => {
  beforeEach(() => {
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    onOpenTask.mockClear();
    onOpenWorkspace.mockClear();
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  it("answers a decision right there, and the task goes back to work", async () => {
    await render();
    const row = item("Rotate the staging database password");
    expect(section("NEEDS YOU")?.contains(row)).toBe(true);
    await act(async () => button(row, "Approve")!.click());
    const after = item("Rotate the staging database password");
    expect(section("WORKING")?.contains(after)).toBe(true);
    const f = source.list().find((p) => p.task.title.startsWith("Rotate"))!;
    expect(f.task.decisions[0]).toMatchObject({ status: "approved", decided_by: "user" });
  });

  it("retries a stopped task, and opens tasks and workspaces", async () => {
    await render();
    const stopped = item("Upgrade xterm to v7");
    expect(stopped?.textContent).toContain("Stopped after 6 of 6 attempts");
    await act(async () => button(stopped, "Retry")!.click());
    expect(section("WORKING")?.contains(item("Upgrade xterm to v7"))).toBe(true);

    await act(async () => button(item("Rotate the staging database password"), "Open")!.click());
    expect(onOpenTask).toHaveBeenCalledWith(source.list().find((p) => p.task.title.startsWith("Rotate"))!.key);
    await act(async () => button(item("api"), "Open")!.click());
    expect(onOpenWorkspace).toHaveBeenCalledWith("mac/ws_api");
  });

  it("pauses every task at work at once", async () => {
    await render();
    await act(async () => button(section("WORKING"), "Pause all tasks")!.click());
    const working = source.list().filter((p) => ["planning", "implementing", "verifying"].includes(p.task.status));
    expect(working).toEqual([]);
    expect(source.list().some((p) => p.task.status === "paused")).toBe(true);
  });
});
