import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
const state = vi.hoisted(() => ({
  set: false,
  controller: undefined as string | undefined,
  modern: false,
  backend: undefined as string | undefined,
  otterTools: undefined as boolean | undefined,
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "settings_models") {
      if (args.controller === "openai") throw new Error("set the OpenAI key (OPENAI_API_KEY) first");
      return [
        { id: "anthropic/claude-opus-5.5", name: "Anthropic: Claude Opus 5.5" },
        { id: "openai/gpt-x", name: "OpenAI: GPT X" },
      ];
    }
    if (cmd === "runtime_capabilities") {
      return state.backend === "sdk"
        ? { backend: "sdk", available: false, notes: ["Node.js wasn't found."], features: {} }
        : { backend: "legacy_cli", available: true, features: {} };
    }
    if (cmd === "settings_set") {
      const u = args.update as { controller?: string; secrets?: Record<string, string | null> };
      if (u.controller !== undefined) state.controller = u.controller || undefined;
      const coding = (args.update as { coding?: { backend?: string } }).coding;
      if (coding?.backend && state.backend) state.backend = coding.backend;
      if (u.secrets && "OPENROUTER_API_KEY" in u.secrets) state.set = u.secrets.OPENROUTER_API_KEY !== null;
      const tools = (args.update as { otter_tools?: boolean }).otter_tools;
      if (tools !== undefined && state.otterTools !== undefined) state.otterTools = tools;
    }
    return {
      controller: state.controller,
      secrets: [
        { name: "OPENROUTER_API_KEY", purpose: "Lets Otter use OpenRouter", set: state.set },
        ...(state.modern ? [{ name: "OPENAI_API_KEY", purpose: "Lets Otter use OpenAI", set: false }] : []),
      ],
      ...(state.backend ? { coding: { backend: state.backend } } : {}),
      ...(state.otterTools !== undefined
        ? {
            otter_tools: state.otterTools,
            otter_answers: state.otterTools ? "with tools (claude)" : "without tools (off in Settings)",
          }
        : {}),
      ...(state.modern
        ? {
            active: "openrouter",
            controllers: [
              { id: "openrouter", label: "OpenRouter", secret: "OPENROUTER_API_KEY", models: true, default_model: "openrouter/auto" },
              { id: "openai", label: "OpenAI", secret: "OPENAI_API_KEY", models: true },
              { id: "claude", label: "Claude Code", models: true },
              { id: "off", label: "Off", models: false },
            ],
          }
        : {}),
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
    state.modern = expect.getState().currentTestName?.includes("models") ?? false;
    state.backend = expect.getState().currentTestName?.includes("coding agent") ? "legacy_cli" : undefined;
    state.otterTools = expect.getState().currentTestName?.includes("Otter's tools") ? false : undefined;
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

  it("sets this Mac's terminal font and size, at once and without a host", async () => {
    const mac = dialog().querySelector('[aria-label="This Mac"]')!;
    type(mac.querySelector<HTMLInputElement>('input[aria-label="Terminal font"]')!, "Menlo");
    expect(localStorage.getItem("otter.terminal-font-family")).toBe("Menlo");
    const size = mac.querySelector<HTMLInputElement>('input[aria-label="Terminal text size"]')!;
    await act(async () => type(size, "15"));
    expect(localStorage.getItem("otter.terminal-font-size")).toBe("15");
    expect(size.value).toBe("15");
    // Nothing went to the host (this Mac's sign-in sites are read here).
    expect(calls.map((c) => c.cmd).filter((c) => !c.startsWith("sign_in_"))).toEqual(["settings_get"]);
    await act(async () => type(mac.querySelector<HTMLInputElement>('input[aria-label="Terminal font"]')!, ""));
    await act(async () => type(size, "13"));
  });

  it("sets what Otter thinks with and a key, which is sent once and never shown", async () => {
    expect(calls.find((c) => c.cmd === "settings_get")).toEqual({ cmd: "settings_get", args: { host: "mac" } });
    expect(dialog().textContent).toContain("not set");
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!;
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

  it("offers the provider's models, and asks for one where there's no default", async () => {
    // Automatic, now OpenRouter: its models are listed for the model field.
    expect(calls).toContainEqual({ cmd: "settings_models", args: { host: "mac", controller: "openrouter" } });
    expect(dialog().textContent).toContain("2 models");
    const input = () => dialog().querySelector<HTMLInputElement>('input[aria-label="Model"]')!;
    const options = () => [...dialog().querySelectorAll<HTMLLIElement>('[role="option"]')];
    expect(input().placeholder).toContain("openrouter/auto");
    // Focus opens the list; typing narrows it, by id or name, every word.
    await act(async () => input().focus());
    expect(options()).toHaveLength(2);
    type(input(), "gpt");
    await flush();
    expect(options().map((o) => o.textContent)).toEqual(["OpenAI: GPT Xopenai/gpt-x"]);
    // Picked with the keyboard.
    await act(async () => input().dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })));
    expect(input().value).toBe("openai/gpt-x");
    expect(options()).toHaveLength(0);
    expect(dialog().textContent).toContain("OpenAI: GPT X");

    // OpenAI: no default, no key yet — the listing says why, Save waits for a model.
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!;
    select.value = "openai";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    await flush();
    expect(input().value).toBe("");
    expect(dialog().textContent).toContain("Couldn't list models: Error: set the OpenAI key");
    expect(dialog().querySelector('input[aria-label="OPENAI_API_KEY"]')).not.toBeNull();
    expect(button("Save").disabled).toBe(true);
    type(input(), "gpt-x");
    await flush();
    expect(button("Save").disabled).toBe(false);
  });

  it("doesn't send an empty key", async () => {
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect((set.args.update as { secrets: object }).secrets).toEqual({});
  });

  it("sets how the coding agent runs, and says whether it can here", async () => {
    expect(calls).toContainEqual({ cmd: "runtime_capabilities", args: { host: "mac" } });
    expect(dialog().querySelector('[role="status"]')?.textContent).toBe("Ready");
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Coding agent runs through"]')!;
    expect(select.value).toBe("legacy_cli");
    select.value = "sdk";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    type(dialog().querySelector<HTMLInputElement>('input[aria-label="Coding model"]')!, "sonnet");
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect((set.args.update as { coding: object }).coding).toEqual({ backend: "sdk", model: "sonnet" });
    // Rechecked after the save: the SDK worker isn't ready, and why.
    expect(dialog().textContent).toContain("Not ready — Node.js wasn't found.");
  });

  it("turns Otter's tools on for a host, and says how Otter answers there", async () => {
    expect(dialog().textContent).toContain("Now: without tools (off in Settings).");
    const box = dialog().querySelector<HTMLInputElement>('input[aria-label="Otter answers with tools"]')!;
    expect(box.checked).toBe(false);
    await act(async () => box.click());
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect((set.args.update as { otter_tools: boolean }).otter_tools).toBe(true);
    expect(dialog().textContent).toContain("Now: with tools (claude).");
  });

  it("shows no tools toggle for an older host", async () => {
    expect(dialog().querySelector('input[aria-label="Otter answers with tools"]')).toBeNull();
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect(set.args.update).not.toHaveProperty("otter_tools");
  });

  it("shows nothing about coding for an older host", async () => {
    expect(dialog().querySelector('select[aria-label="Coding agent runs through"]')).toBeNull();
    expect(calls.some((c) => c.cmd === "runtime_capabilities")).toBe(false);
  });
});
