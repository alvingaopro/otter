import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HostSettings } from "./SettingsDialog";

const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
// Each host's settings, as its otterd would say; `"unreachable"`: not reached.
const hosts = vi.hoisted(() => ({ settings: {} as Record<string, HostSettings | "unreachable"> }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    const host = String(args?.host ?? "");
    const s = hosts.settings[host];
    if (s === "unreachable") throw new Error("ssh: connect to host gpu port 22: Connection refused");
    if (cmd === "settings_get") return s;
    if (cmd === "settings_set") {
      const u = args.update as {
        hub?: { enabled: boolean; name: string; users: string[] };
        secrets?: Record<string, string>;
      };
      for (const [name, v] of Object.entries(u.secrets ?? {})) {
        const k = s.secrets.find((k) => k.name === name);
        if (k) k.set = !!v;
      }
      if (u.hub) {
        // What otterd says once the hub it runs has said so.
        s.hub = u.hub.enabled
          ? {
              ...u.hub,
              hosts: [u.hub.name],
              state: "running",
              slack: "connected",
              served: [{ host: u.hub.name, line: "takes Slack's events from the hub" }],
              boot: "Starts with this machine: systemd user unit otterd-workd.service.",
            }
          : { ...u.hub, hosts: [u.hub.name], state: "off" };
      }
      return s;
    }
    return undefined;
  }),
}));

const { HubSettings } = await import("./HubSettings");

/** A host's settings as a new otterd says them: the hub off, these tokens set. */
function host(tokens: boolean, users: string[] = []): HostSettings {
  return {
    secrets: [
      { name: "SLACK_BOT_TOKEN", purpose: "", set: tokens },
      { name: "SLACK_APP_TOKEN", purpose: "", set: tokens },
    ],
    slack: { users, state: tokens ? "connected" : "off" },
    hub: { enabled: false, name: "", users: [], hosts: [], state: "off" },
  };
}

let root: Root;
let el: HTMLDivElement;
const flush = () => act(async () => {});
const button = (text: string) => [...el.querySelectorAll("button")].find((b) => b.textContent === text);
const input = (label: string) => el.querySelector<HTMLInputElement>(`input[aria-label="${label}"]`)!;
const line = (h: string) => el.querySelector(`.settings-row[aria-label="${h}"] .settings-hint`)?.textContent;
function type(i: HTMLInputElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(i, value);
  i.dispatchEvent(new Event("input", { bubbles: true }));
}
async function render(names: string[]) {
  await act(async () => root.render(<HubSettings hosts={names} />));
  await flush();
}

describe("Settings › All hosts › Slack (D-208)", () => {
  beforeEach(() => {
    calls.length = 0;
    el = document.createElement("div");
    document.body.appendChild(el);
    root = createRoot(el);
  });
  afterEach(() => {
    act(() => root.unmount());
    el.remove();
  });

  it("runs the hub on the host chosen, with the tokens and member it has, and says how it is", async () => {
    hosts.settings = { "code-server": host(true, ["U06MPBPRX7C"]), gpu: host(false) };
    await render(["code-server", "gpu"]);
    expect(el.querySelector<HTMLSelectElement>('select[aria-label="Hub host"]')!.value).toBe("code-server");
    expect(el.textContent).toContain("Set on code-server: leave it empty to keep it.");
    expect(input("Hub member ID").placeholder).toBe("U06MPBPRX7C");
    await act(async () => button("Set up on code-server")!.click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect(set.args.host).toBe("code-server");
    expect(set.args.update).toEqual({
      hub: { enabled: true, name: "code-server", users: ["U06MPBPRX7C"], hosts: [] },
      secrets: {},
    });
    // How it is, from its otterd.
    expect(el.textContent).toContain("On code-server");
    expect(el.textContent).toContain("Running");
    expect(el.textContent).toContain("connected");
    expect(line("code-server")).toBe("takes Slack's events from the hub");
    expect(line("gpu")).toBe("Not served yet: the hub on code-server reaches only its own machine for now.");
    expect(button("Set up on code-server")).toBeUndefined();

    // Stopped: off there, and the form again.
    await act(async () => button("Stop")!.click());
    await flush();
    const sets = calls.filter((c) => c.cmd === "settings_set");
    const stop = sets[sets.length - 1];
    expect(stop.args.update).toEqual({ hub: { enabled: false, name: "code-server", users: ["U06MPBPRX7C"], hosts: [] } });
    expect(button("Set up on code-server")).toBeTruthy();
  });

  it("asks for the tokens a host doesn't have, sends them once and never shows them", async () => {
    hosts.settings = { mac: host(false) };
    await render(["mac"]);
    const setUp = () => button("Set up on mac")!;
    expect(setUp().disabled).toBe(true);
    type(input("Bot token"), " xoxb-1-abc ");
    type(input("App token"), "xapp-1-def");
    await flush();
    expect(setUp().disabled).toBe(true);
    type(input("Hub member ID"), "U06MPBPRX7C");
    await flush();
    expect(input("Bot token").type).toBe("password");
    await act(async () => setUp().click());
    await flush();
    const set = calls.find((c) => c.cmd === "settings_set")!;
    expect(set.args.update).toEqual({
      hub: { enabled: true, name: "mac", users: ["U06MPBPRX7C"], hosts: [] },
      secrets: { SLACK_BOT_TOKEN: "xoxb-1-abc", SLACK_APP_TOKEN: "xapp-1-def" },
    });
    expect(el.innerHTML).not.toContain("xoxb-1-abc");
    expect(el.innerHTML).not.toContain("xapp-1-def");
  });

  it("offers only the hosts whose otterd can run it, and says why the others aren't", async () => {
    const old = host(true);
    delete old.hub;
    hosts.settings = { old, gpu: "unreachable", "code-server": host(true, ["U1A"]) };
    await render(["old", "gpu", "code-server"]);
    const options = [...el.querySelectorAll<HTMLOptionElement>('select[aria-label="Hub host"] option')].map((o) => o.textContent);
    expect(options).toEqual(["code-server"]);
    expect(line("old")).toBe("Its otterd is too old for the hub: update it.");
    expect(line("gpu")).toContain("Not reached: ");
    expect(line("gpu")).toContain("Connection refused");
  });

  it("says a hub that runs, when one does, and doesn't offer to set up another", async () => {
    const running = host(true, ["U1A"]);
    running.hub = {
      enabled: true,
      name: "code-server",
      users: ["U1A"],
      hosts: ["code-server"],
      state: "failed",
      detail: "the hub stopped (exit status: 1); otterd starts it again.",
      boot: "Starts with this machine: systemd user unit otterd.service.",
    };
    hosts.settings = { "code-server": running, gpu: host(false) };
    await render(["gpu", "code-server"]);
    expect(el.textContent).toContain("On code-server");
    expect(el.textContent).toContain("Not running");
    expect(el.textContent).toContain("the hub stopped (exit status: 1); otterd starts it again.");
    expect(el.textContent).toContain("Starts with this machine");
    expect(button("Set up on gpu")).toBeUndefined();
    expect(button("Set up on code-server")).toBeUndefined();
  });
});
