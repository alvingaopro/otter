import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HostView } from "./types";

const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
// Each host's install, let go by the test.
const installs = vi.hoisted(() => new Map<string, { ok: () => void; fail: (e: string) => void }>());

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd !== "host_install") return Promise.resolve(undefined);
    return new Promise((resolve, reject) =>
      installs.set(String(args.name), { ok: () => resolve("installed"), fail: (e) => reject(e) }),
    );
  }),
}));

const { Sidebar, older } = await import("./Sidebar");

const host = (name: string, version: string | undefined, status: HostView["status"] = "connected"): HostView =>
  ({ name, status, version, describe: `ssh ${name}` }) as HostView;

let root: Root;
let el: HTMLDivElement;

function render(hosts: HostView[], appVersion = "0.7.20") {
  return act(async () =>
    root.render(
      <Sidebar
        placed={[]}
        hosts={hosts}
        onSelect={() => {}}
        now={0}
        appVersion={appVersion}
        onAddHost={() => {}}
        onHost={() => {}}
        onNew={() => {}}
        onUpdate={() => {}}
        pins={[]}
        pinMenu={() => []}
        onMovePin={() => {}}
      />,
    ),
  );
}

const update = () => el.querySelector<HTMLButtonElement>(".hosts-update");
const state = (name: string) =>
  [...el.querySelectorAll(".host-row")].find((r) => r.querySelector(".host-name")?.textContent === name)!.querySelector(".host-state")!
    .textContent;

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  calls.length = 0;
  installs.clear();
  el = document.createElement("div");
  document.body.appendChild(el);
  root = createRoot(el);
});

afterEach(() => {
  act(() => root.unmount());
  el.remove();
});

describe("the title bar (D-177)", () => {
  it("holds Otter's name, the counts and New workspace in one row", async () => {
    await render([host("code", "0.7.20")]);
    const top = el.querySelector(".sidebar-top")!;
    expect(top.querySelector(".brand-name")?.textContent).toBe("Otter");
    expect([...top.querySelectorAll("button")].map((b) => b.getAttribute("aria-label"))).toEqual([
      "0 need you",
      "0 working",
      "New workspace",
    ]);
    // No second row for the name below it.
    expect(el.querySelectorAll(".brand-name")).toHaveLength(1);
  });
});

describe("updating every host at once (D-131)", () => {
  it("compares releases by number", () => {
    expect(older("0.7.19", "0.7.20")).toBe(true);
    expect(older("0.7.9", "0.7.10")).toBe(true);
    expect(older("0.8.0", "0.7.20")).toBe(false);
    expect(older("0.7.20", "0.7.20")).toBe(false);
  });

  it("updates the hosts behind this app, side by side, and says which didn't", async () => {
    await render([host("code", "0.7.19"), host("mac", "0.7.19"), host("new", "0.8.0"), host("same", "0.7.20"), host("away", "0.7.1", "unreachable")]);
    expect(update()?.textContent).toBe("Update all");
    expect(update()?.title).toContain("code, mac to 0.7.20");

    await act(async () => update()!.click());
    // Only the two behind it, both at once; not a newer one, not one away.
    expect(calls).toEqual([
      { cmd: "host_install", args: { name: "code" } },
      { cmd: "host_install", args: { name: "mac" } },
    ]);
    expect(update()?.textContent).toBe("Updating…");
    expect(update()?.disabled).toBe(true);
    expect(state("code")).toBe("updating…");

    await act(async () => installs.get("code")!.ok());
    await act(async () => installs.get("mac")!.fail("could not download the release"));
    expect(state("mac")).toBe("update failed");
    expect(el.querySelector('.host-row[title*="could not download"]')).not.toBeNull();
    // Updated hosts come back on the new version; the failed one can go again.
    await render([host("code", "0.7.20"), host("mac", "0.7.19")]);
    expect(update()?.textContent).toBe("Update");
    expect(update()?.disabled).toBe(false);
  });

  it("shows nothing when every host is up to date", async () => {
    await render([host("code", "0.7.20"), host("new", "0.8.0")]);
    expect(update()).toBeNull();
  });
});
