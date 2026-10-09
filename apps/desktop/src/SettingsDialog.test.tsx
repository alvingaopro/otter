import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
const state = vi.hoisted(() => ({ set: false, controller: undefined as string | undefined }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "settings_set") {
      const u = args.update as { controller?: string; secrets?: Record<string, string | null> };
      if (u.controller !== undefined) state.controller = u.controller || undefined;
      if (u.secrets && "OPENROUTER_API_KEY" in u.secrets) state.set = u.secrets.OPENROUTER_API_KEY !== null;
    }
    return {
      controller: state.controller,
      secrets: [{ name: "OPENROUTER_API_KEY", purpose: "OpenRouter, for the Control Agent", set: state.set }],
    };
  }),
}));

const { SettingsDialog } = await import("./SettingsDialog");

let root: Root;
let host: HTMLDivElement;
const flush = () => act(async () => new Promise((r) => setTimeout(r, 0)));

function type(el: HTMLInputElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

describe("Settings", () => {
  beforeEach(async () => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    calls.length = 0;
    state.set = false;
    state.controller = undefined;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    await act(async () => root.render(<SettingsDialog hosts={["mac"]} onClose={() => {}} />));
    await flush();
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  const dialog = () => document.querySelector('[role="dialog"]')!;
  const button = (text: string) => [...dialog().querySelectorAll("button")].find((b) => b.textContent === text)!;

  it("sets the Control Agent and a key, which is sent once and never shown", async () => {
    expect(calls[0]).toEqual({ cmd: "settings_get", args: { host: "mac" } });
    expect(dialog().textContent).toContain("not set");
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Control Agent"]')!;
    select.value = "openrouter";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    const key = dialog().querySelector<HTMLInputElement>('input[aria-label="OPENROUTER_API_KEY"]')!;
    expect(key.type).toBe("password");
    type(key, "  sk-or-123  ");
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect(set.args).toEqual({ host: "mac", update: { controller: "openrouter", model: "", secrets: { OPENROUTER_API_KEY: "sk-or-123" } } });
    // Afterwards it only says the key is set; the field is empty again.
    expect(dialog().querySelector(".key-state.set")).not.toBeNull();
    expect(dialog().querySelector<HTMLInputElement>('input[aria-label="OPENROUTER_API_KEY"]')!.value).toBe("");
    expect(dialog().innerHTML).not.toContain("sk-or-123");

    await act(async () => button("Clear").click());
    await flush();
    expect(calls[calls.length - 1]).toEqual({
      cmd: "settings_set",
      args: { host: "mac", update: { secrets: { OPENROUTER_API_KEY: null } } },
    });
    expect(dialog().textContent).toContain("not set");
  });

  it("doesn't send an empty key", async () => {
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect((set.args.update as { secrets: object }).secrets).toEqual({});
  });
});
