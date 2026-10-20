import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
const state = vi.hoisted(() => ({
  set: false,
  controller: undefined as string | undefined,
  modern: false,
  backend: undefined as string | undefined,
  codingModel: undefined as string | undefined,
  otterTools: undefined as boolean | undefined,
  oldTest: false,
  workerMissing: false,
  installFails: false,
  // Found by a test of the SDK, saved or not (D-129).
  testWorkerMissing: false,
  noRelease: false,
}));
// Holds back the host's answers to a save or a test until let go.
const gate = vi.hoisted(() => ({ wait: null as Promise<void> | null }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (gate.wait && (cmd === "settings_set" || cmd === "runtime_test")) await gate.wait;
    if (cmd === "settings_models") {
      if (args.controller === "openai") throw new Error("set the OpenAI key (OPENAI_API_KEY) first");
      return [
        { id: "anthropic/claude-opus-5.5", name: "Anthropic: Claude Opus 5.5" },
        { id: "openai/gpt-x", name: "OpenAI: GPT X" },
      ];
    }
    if (cmd === "host_install_worker") {
      if (state.installFails) throw "could not download https://github.com/alvingaopro/otter/releases/download/v0.7.19/otter-0.7.19-linux-x86_64.tar.gz";
      // A release with no worker for the platform: a note, and success.
      if (state.noRelease) return "note: release 0.7.19 has no Claude worker for linux-riscv64; Claude's SDK mode won't be available";
      state.workerMissing = false;
      state.testWorkerMissing = false;
      return "installed the Claude worker 0.7.19";
    }
    if (cmd === "runtime_capabilities") {
      if (state.workerMissing) {
        return {
          backend: "sdk",
          available: false,
          worker_missing: true,
          notes: ["The Claude worker isn't installed next to otterd: `otter host install` installs it."],
          features: {},
        };
      }
      return state.backend === "sdk"
        ? { backend: "sdk", available: false, notes: ["Node.js wasn't found."], features: {} }
        : { backend: "legacy_cli", available: true, features: {} };
    }
    if (cmd === "runtime_test") {
      if (state.oldTest) throw "invalid_request: unknown variant `runtime.test`";
      if (args.backend === "sdk" && state.testWorkerMissing) {
        return {
          backend: "sdk",
          ready: false,
          worker_missing: true,
          steps: [
            { name: "Installed", state: "failed", detail: "The Claude worker isn't installed next to otterd: `otter host install` installs it." },
            { name: "Signed in and answering", state: "skipped", detail: "Not tried." },
          ],
        };
      }
      return args.backend === "sdk"
        ? {
            backend: "sdk",
            ready: false,
            steps: [
              { name: "Installed", state: "failed", detail: "Node.js isn't on this host's PATH." },
              { name: "Signed in and answering", state: "skipped", detail: "Not tried." },
            ],
          }
        : {
            backend: "legacy_cli",
            ready: true,
            steps: [
              { name: "Installed", state: "passed", detail: "Found on this host." },
              { name: "Signed in and answering", state: "passed", detail: "Answered in 2.1 s, with haiku." },
            ],
          };
    }
    if (cmd === "settings_set") {
      const u = args.update as { controller?: string; secrets?: Record<string, string | null> };
      if (u.controller !== undefined) state.controller = u.controller || undefined;
      const coding = (args.update as { coding?: { backend?: string; model?: string } }).coding;
      if (coding?.backend && state.backend) state.backend = coding.backend;
      if (coding?.model !== undefined) state.codingModel = coding.model || undefined;
      if (u.secrets && "OPENROUTER_API_KEY" in u.secrets) state.set = u.secrets.OPENROUTER_API_KEY !== null;
      const tools = (args.update as { otter_tools?: boolean }).otter_tools;
      if (tools !== undefined && state.otterTools !== undefined) state.otterTools = tools;
    }
    return {
      controller: state.controller,
      secrets: [
        { name: "OPENROUTER_API_KEY", purpose: "OpenRouter", set: state.set },
        ...(state.modern ? [{ name: "OPENAI_API_KEY", purpose: "OpenAI", set: false }] : []),
      ],
      ...(state.backend ? { coding: { backend: state.backend, model: state.codingModel } } : {}),
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
const themes: string[] = [];
let closed = 0;
function hold(): () => void {
  let go = () => {};
  gate.wait = new Promise<void>((r) => (go = r));
  return () => {
    gate.wait = null;
    go();
  };
}
const flush = () => act(async () => new Promise((r) => setTimeout(r, 0)));

function type(el: HTMLInputElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

describe("Settings", () => {
  beforeEach(async () => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    calls.length = 0;
    themes.length = 0;
    closed = 0;
    gate.wait = null;
    state.set = false;
    state.controller = undefined;
    state.codingModel = undefined;
    const name = expect.getState().currentTestName ?? "";
    state.modern = name.includes("models");
    state.backend = name.includes("before saving")
      ? "legacy_cli"
      : name.includes("not installed") || name.includes("worker")
        ? "sdk"
        : name.includes("coding agent")
          ? "legacy_cli"
          : undefined;
    state.workerMissing = name.includes("worker") && !name.includes("before saving");
    state.testWorkerMissing = name.includes("before saving");
    state.installFails = name.includes("can't be installed");
    state.noRelease = name.includes("no release");
    state.otterTools = name.includes("Otter's tools") ? false : undefined;
    state.oldTest = name.includes("older otterd");
    localStorage.removeItem("otter.settings-tab");
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    await act(async () =>
      root.render(
        <SettingsDialog hosts={["mac", "pc"]} defaultHost="mac" theme="system" onTheme={(t) => themes.push(t)} onClose={() => closed++} />,
      ),
    );
    await flush();
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  const dialog = () => document.querySelector('[role="dialog"]')!;
  const button = (text: string) => [...dialog().querySelectorAll("button")].find((b) => b.textContent === text)!;
  const tabs = () => [...dialog().querySelectorAll<HTMLButtonElement>(".settings-nav-item")];
  const tab = async (name: string) => {
    await act(async () => tabs().find((b) => b.textContent?.startsWith(name))!.click());
    await flush();
  };
  const radio = (group: string, text: string) =>
    [...dialog().querySelectorAll<HTMLLabelElement>(`[aria-label="${group}"] label`)].find((l) => l.textContent === text)!.querySelector("input")!;

  it("opens on this Mac's look, then remembers the tab", async () => {
    expect(dialog().querySelector("h2")?.textContent).toBe("Appearance");
    // This host's otterd has no coding settings: no tab for them.
    expect(tabs().map((b) => b.textContent)).toEqual(["Appearance", "Sign-in pages", "Developer", "Git & GitHub", "Otter", "API keys0 set"]);
    await tab("Otter");
    expect(localStorage.getItem("otter.settings-tab")).toBe("otter");
    expect(dialog().querySelector('[aria-current="page"]')?.textContent).toBe("Otter");
  });

  it("turns the performance overlay on and off under Developer, at once and without a host (D-128)", async () => {
    await tab("Developer");
    const toggle = dialog().querySelector<HTMLInputElement>('[aria-label="Show the performance overlay"]')!;
    expect(toggle.checked).toBe(false);
    await act(async () => toggle.click());
    expect(toggle.checked).toBe(true);
    expect(localStorage.getItem("otter.perf-overlay")).toBe("on");
    await act(async () => toggle.click());
    expect(localStorage.getItem("otter.perf-overlay")).toBeNull();
    expect(calls.map((c) => c.cmd)).toEqual(["settings_get"]);
    expect(dialog().textContent).not.toContain("Unsaved changes");
  });

  it("sets this Mac's theme, terminal font and size, at once and without a host", async () => {
    await act(async () => radio("Theme", "Dark").click());
    expect(themes).toEqual(["dark"]);
    type(dialog().querySelector<HTMLInputElement>('input[aria-label="Terminal font"]')!, "Menlo");
    expect(localStorage.getItem("otter.terminal-font-family")).toBe("Menlo");
    expect(dialog().querySelector<HTMLElement>('[aria-label="Preview"]')!.style.fontFamily).toContain("Menlo");
    const size = dialog().querySelector<HTMLInputElement>('input[aria-label="Terminal text size"]')!;
    await act(async () => type(size, "15"));
    expect(localStorage.getItem("otter.terminal-font-size")).toBe("15");
    await act(async () => dialog().querySelector<HTMLButtonElement>('button[aria-label="Larger"]')!.click());
    expect(size.value).toBe("16");
    // Nothing went to the host, and nothing waits to be saved.
    expect(calls.map((c) => c.cmd)).toEqual(["settings_get"]);
    expect(dialog().textContent).not.toContain("Unsaved changes");
    await act(async () => type(dialog().querySelector<HTMLInputElement>('input[aria-label="Terminal font"]')!, ""));
    await act(async () => type(size, "13"));
  });

  it("sets what Otter thinks with and a key, which is sent once and never shown", async () => {
    await tab("Otter");
    expect(calls.find((c) => c.cmd === "settings_get")).toEqual({ cmd: "settings_get", args: { host: "mac" } });
    expect(dialog().textContent).not.toContain("Unsaved changes");
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!;
    select.value = "openrouter";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    const key = dialog().querySelector<HTMLInputElement>('input[aria-label="OPENROUTER_API_KEY"]')!;
    expect(key.type).toBe("password");
    type(key, "  sk-or-123  ");
    await flush();
    expect(dialog().textContent).toContain("Unsaved changes on mac");
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect(set.args).toEqual({ host: "mac", update: { controller: "openrouter", model: "", secrets: { OPENROUTER_API_KEY: "sk-or-123" } } });
    // Afterwards it only says the key is set.
    expect(dialog().textContent).toContain("Saved on mac.");
    expect(dialog().querySelector(".key-state.set")).not.toBeNull();
    expect(dialog().querySelector('input[aria-label="OPENROUTER_API_KEY"]')).toBeNull();
    expect(dialog().innerHTML).not.toContain("sk-or-123");
    // Replace… opens an empty field again.
    await act(async () => button("Replace…").click());
    expect(dialog().querySelector<HTMLInputElement>('input[aria-label="OPENROUTER_API_KEY"]')!.value).toBe("");

    // Every key is on its own tab, where one is cleared.
    await tab("API keys");
    expect(dialog().textContent).toContain("In use");
    await act(async () => button("Clear").click());
    await flush();
    expect(calls[calls.length - 1]).toEqual({
      cmd: "settings_set",
      args: { host: "mac", update: { secrets: { OPENROUTER_API_KEY: null } } },
    });
    expect(dialog().textContent).toContain("Not set");
  });

  it("offers the provider's models, and asks for one where there's no default", async () => {
    await tab("Otter");
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

  it("doesn't send an empty key, and discards what wasn't saved", async () => {
    await tab("Otter");
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!;
    select.value = "openrouter";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    // Unsaved changes stay while another tab is shown.
    await tab("Appearance");
    expect(dialog().textContent).toContain("Unsaved changes on mac");
    await act(async () => button("Discard").click());
    expect(dialog().textContent).not.toContain("Unsaved changes");
    await tab("Otter");
    expect(dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!.value).toBe("");

    const again = dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!;
    again.value = "openrouter";
    await act(async () => again.dispatchEvent(new Event("change", { bubbles: true })));
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect((set.args.update as { secrets: object }).secrets).toEqual({});
  });

  it("sets how the coding agent runs, tests it as shown, and says whether it can run here", async () => {
    await tab("Coding agent");
    expect(calls).toContainEqual({ cmd: "runtime_capabilities", args: { host: "mac" } });
    // Installed, as the host says; whether it's signed in only a test knows.
    expect(dialog().querySelector('[role="status"]')?.textContent).toBe("Installed");
    expect(radio("Coding agent runs through", "Claude Code CLI").checked).toBe(true);
    // The SDK first: the default.
    expect([...dialog().querySelectorAll('[aria-label="Coding agent runs through"] label')].map((l) => l.textContent)).toEqual([
      "Agent SDK",
      "Claude Code CLI",
    ]);

    // A test of what's on screen, before saving.
    await act(async () => button("Run a test").click());
    await flush();
    expect(calls[calls.length - 1]).toEqual({ cmd: "runtime_test", args: { host: "mac", backend: "legacy_cli", model: "" } });
    expect(dialog().querySelector('[aria-label="Test run"]')?.textContent).toContain("Answered in 2.1 s, with haiku.");
    expect(dialog().querySelector('[role="status"]')?.textContent).toBe("Ready");

    await act(async () => radio("Coding agent runs through", "Agent SDK").click());
    type(dialog().querySelector<HTMLInputElement>('input[aria-label="Coding model"]')!, "sonnet");
    await flush();
    // What it showed was of another setup: gone.
    expect(dialog().querySelector('[aria-label="Test run"]')).toBeNull();
    await act(async () => button("Run a test").click());
    await flush();
    expect(calls[calls.length - 1]).toEqual({ cmd: "runtime_test", args: { host: "mac", backend: "sdk", model: "sonnet" } });
    const steps = dialog().querySelector('[aria-label="Test run"]')!;
    expect(steps.querySelector(".test-step.failed")?.textContent).toContain("Node.js isn't on this host's PATH.");
    expect(steps.querySelector(".test-step.skipped")?.textContent).toContain("Not tried.");
    expect(dialog().querySelector('[role="status"]')?.textContent).toBe("Not ready");

    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect((set.args.update as { coding: object }).coding).toEqual({ backend: "sdk", model: "sonnet" });
    // The host is asked again after the save; the test still stands.
    expect(calls.filter((c) => c.cmd === "runtime_capabilities")).toHaveLength(2);
    expect(dialog().querySelector('[role="status"]')?.textContent).toBe("Not ready");
    expect(dialog().querySelector('[aria-label="Test run"]')).not.toBeNull();

    // Only what changed goes: the backend isn't sent again.
    type(dialog().querySelector<HTMLInputElement>('input[aria-label="Coding model"]')!, "opus");
    await flush();
    await act(async () => button("Save").click());
    await flush();
    const last = calls.filter((c) => c.cmd === "settings_set").pop()!;
    expect((last.args.update as { coding: object }).coding).toEqual({ model: "opus" });
  });

  it("says why the coding agent can't run where it's not installed", async () => {
    await tab("Coding agent");
    expect(dialog().querySelector('[role="status"]')?.textContent).toBe("Not ready");
    expect(dialog().textContent).toContain("Node.js wasn't found.");
  });

  it("installs a missing Claude worker, and asks the host again", async () => {
    await tab("Coding agent");
    expect(dialog().textContent).toContain("The Claude worker isn't installed on mac. Install puts it there; otterd keeps running.");
    await act(async () => button("Install").click());
    await flush();
    expect(calls).toContainEqual({ cmd: "host_install_worker", args: { name: "mac" } });
    expect(calls.filter((c) => c.cmd === "runtime_capabilities")).toHaveLength(2);
    // Found now, though Node.js is still missing in this host's story.
    expect(dialog().textContent).toContain("Installed. Still not ready: Node.js wasn't found.");
    expect(button("Install")).toBeUndefined();
  });

  it("offers a worker the SDK needs when it's tried before saving", async () => {
    await tab("Coding agent");
    // Saved: the CLI; the host says nothing of the SDK's worker.
    expect(button("Install")).toBeUndefined();
    await act(async () => radio("Coding agent runs through", "Agent SDK").click());
    await act(async () => button("Run a test").click());
    await flush();
    await act(async () => button("Install").click());
    await flush();
    expect(calls).toContainEqual({ cmd: "host_install_worker", args: { name: "mac" } });
    expect(dialog().textContent).toContain("installed the Claude worker 0.7.19");
    expect(dialog().textContent).toContain("Run a test to check it.");
  });

  it("shows what the install said when a worker has no release for the host", async () => {
    await tab("Coding agent");
    await act(async () => button("Install").click());
    await flush();
    expect(dialog().querySelector("pre")?.textContent).toContain("has no Claude worker for linux-riscv64");
    // Still missing: the button stays.
    expect(button("Install")).toBeDefined();
  });

  it("says why a Claude worker can't be installed", async () => {
    await tab("Coding agent");
    await act(async () => button("Install").click());
    await flush();
    expect(dialog().textContent).toContain("could not download");
    // Still missing: the button stays, to try again.
    expect(button("Install")).toBeDefined();
  });

  it("says an older otterd can't test the coding agent", async () => {
    await tab("Coding agent");
    await act(async () => button("Run a test").click());
    await flush();
    expect(dialog().textContent).toContain("mac's otterd is too old for this: update it from the host's page.");
  });

  it("turns Otter's tools on for a host, and says how Otter answers there", async () => {
    await tab("Otter");
    expect(dialog().textContent).toContain("Now: without tools (off in Settings).");
    const box = dialog().querySelector<HTMLInputElement>('input[role="switch"][aria-label="Otter answers with tools"]')!;
    expect(box.checked).toBe(false);
    await act(async () => box.click());
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect((set.args.update as { otter_tools: boolean }).otter_tools).toBe(true);
    expect(dialog().textContent).toContain("Now: with tools (claude).");
  });

  it("shows no tools toggle for an older host", async () => {
    await tab("Otter");
    expect(dialog().querySelector('input[aria-label="Otter answers with tools"]')).toBeNull();
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!;
    select.value = "openrouter";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    await act(async () => button("Save").click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect(set.args.update).not.toHaveProperty("otter_tools");
    expect(set.args.update).not.toHaveProperty("coding");
  });

  it("shows nothing about coding for an older host", async () => {
    expect(tabs().map((b) => b.textContent)).not.toContain("Coding agent");
    expect(calls.some((c) => c.cmd === "runtime_capabilities")).toBe(false);
  });

  it("asks before closing over unsaved changes", async () => {
    await tab("Otter");
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!;
    select.value = "openrouter";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    const esc = () => act(async () => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" })));
    await esc();
    expect(closed).toBe(0);
    expect(dialog().textContent).toContain("Close without saving the changes on mac?");
    await act(async () => button("Keep editing").click());
    expect(dialog().textContent).toContain("Unsaved changes on mac");
    await esc();
    await esc();
    expect(closed).toBe(1);
  });

  it("keeps another host's late answer out of the form", async () => {
    await tab("Otter");
    const select = dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!;
    select.value = "openrouter";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    const go = hold();
    await act(async () => button("Save").click());
    // Another host, while mac's save is under way.
    const hostPick = dialog().querySelector<HTMLSelectElement>('select[aria-label="Host"]')!;
    hostPick.value = "pc";
    await act(async () => hostPick.dispatchEvent(new Event("change", { bubbles: true })));
    await flush();
    state.controller = "openrouter";
    await act(async () => go());
    await flush();
    expect(dialog().textContent).not.toContain("Saved on");
    expect(dialog().querySelector<HTMLSelectElement>('select[aria-label="Model provider"]')!.value).toBe("");
    expect(dialog().textContent).not.toContain("Unsaved changes");
  });

  it("clears one key and keeps the others typed", async () => {
    state.set = true;
    state.modern = true;
    act(() => root.unmount());
    root = createRoot(host);
    await act(async () => root.render(<SettingsDialog hosts={["mac"]} theme="system" onTheme={() => {}} onClose={() => {}} />));
    await flush();
    await tab("API keys");
    await act(async () => button("Add…").click());
    type(dialog().querySelector<HTMLInputElement>('input[aria-label="OPENAI_API_KEY"]')!, "sk-openai");
    await act(async () => button("Clear").click());
    await flush();
    expect(calls.filter((c) => c.cmd === "settings_set").pop()!.args).toEqual({ host: "mac", update: { secrets: { OPENROUTER_API_KEY: null } } });
    expect(dialog().querySelector<HTMLInputElement>('input[aria-label="OPENAI_API_KEY"]')!.value).toBe("sk-openai");
  });

  it("shows a coding agent test only for what's still on screen", async () => {
    await tab("Coding agent");
    const go = hold();
    await act(async () => button("Run a test").click());
    expect(dialog().textContent).toContain("Testing…");
    type(dialog().querySelector<HTMLInputElement>('input[aria-label="Coding model"]')!, "opus");
    await flush();
    await act(async () => go());
    await flush();
    expect(dialog().querySelector('[aria-label="Test run"]')).toBeNull();
    expect(dialog().querySelector('[role="status"]')?.textContent).toBe("Installed");
  });

  it("asks for a host before the host's settings", async () => {
    const added: string[] = [];
    act(() => root.unmount());
    root = createRoot(host);
    await act(async () =>
      root.render(<SettingsDialog hosts={[]} theme="system" onTheme={() => {}} onAddHost={() => added.push("add")} onClose={() => {}} />),
    );
    await tab("Otter");
    expect(dialog().textContent).toContain("No host connected");
    await act(async () => button("Add a host…").click());
    expect(added).toEqual(["add"]);
  });
});
