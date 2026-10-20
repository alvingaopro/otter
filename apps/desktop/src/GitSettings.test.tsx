import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { GitView } from "./GitSettings";

const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
// Each host's git, as its otterd would say; `null`: too old for it.
const hosts = vi.hoisted(() => ({ views: {} as Record<string, GitView | null>, identity: {} as { name?: string; email?: string } }));
const copied = vi.hoisted(() => [] as string[]);
const opened = vi.hoisted(() => [] as string[]);

vi.mock("@tauri-apps/plugin-clipboard-manager", () => ({ writeText: vi.fn(async (t: string) => void copied.push(t)) }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn(async (u: string) => void opened.push(u)) }));
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    const host = String(args?.host ?? "");
    const v = hosts.views[host];
    switch (cmd) {
      case "git_identity":
        return hosts.identity;
      case "git_identity_set":
        return (args.hosts as string[]).map((h) =>
          h === "old" ? { host: h, error: "old's otterd is too old for this: update it" } : { host: h },
        );
      case "git_view":
        return v;
      case "git_key_create":
        hosts.views[host] = { ...v, ssh_key: { public: `ssh-ed25519 AAAA otter@${host}`, fingerprint: "SHA256:x" } };
        return hosts.views[host];
      case "github_login":
        hosts.views[host] = args.token
          ? { ...v, github: "alvin" }
          : { ...v, login: { state: "pending", code: `CODE-${host.toUpperCase()}`, url: "https://github.com/login/device" } };
        return hosts.views[host];
      case "git_key_add":
        hosts.views[host] = { ...v, key_account: "alvin" };
        return {
          ssh: { ok: true, detail: "The key signs in to GitHub as alvin.", account: "alvin" },
          github: { ok: true, detail: "Signed in as alvin.", account: "alvin" },
          git: hosts.views[host],
        };
      case "github_logout":
        hosts.views[host] = { ...v, github: undefined, login: undefined };
        return hosts.views[host];
    }
    return undefined;
  }),
}));

const { GitSettings } = await import("./GitSettings");

let root: Root;
let el: HTMLDivElement;
const flush = () => act(async () => {});
const button = (text: string, within: Element = el) => [...within.querySelectorAll("button")].find((b) => b.textContent === text);
const row = (host: string) => el.querySelector(`.git-host[aria-label="${host}"], .settings-row[aria-label="${host}"]`)!;
function type(input: HTMLInputElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}
// A browser sign-in ends: the host's otterd now says who it is.
function approve(host: string) {
  hosts.views[host] = { ...hosts.views[host], login: undefined, github: "alvin" };
}

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  vi.useFakeTimers();
  calls.length = 0;
  copied.length = 0;
  opened.length = 0;
  hosts.identity = { name: "Alvin Gao", email: "old@example.com" };
  hosts.views = { code: {}, mac: {} };
  el = document.createElement("div");
  document.body.appendChild(el);
  root = createRoot(el);
});

afterEach(() => {
  act(() => root.unmount());
  el.remove();
  vi.useRealTimers();
});

async function render(names = ["code", "mac"]) {
  await act(async () => root.render(<GitSettings hosts={names} />));
  await flush();
}

describe("Git & GitHub settings (D-132)", () => {
  it("keeps one identity and sends it to every host", async () => {
    await render();
    const email = el.querySelector<HTMLInputElement>('input[aria-label="Git email"]')!;
    expect(email.value).toBe("old@example.com");
    expect(button("Save on all hosts")).toBeUndefined();
    await act(async () => type(email, "alvin@example.com"));
    await act(async () => button("Save on all hosts")!.click());
    await flush();
    expect(calls).toContainEqual({ cmd: "git_identity_set", args: { hosts: ["code", "mac"], name: "Alvin Gao", email: "alvin@example.com" } });
    expect(el.textContent).toContain("Saved on all 2 hosts.");
    expect(button("Save on all hosts")).toBeUndefined();
  });

  it("takes a host through its key, its sign-in and the key on GitHub, one step at a time", async () => {
    await render(["code"]);
    await act(async () => button("Make a key", row("code"))!.click());
    await flush();
    expect(calls).toContainEqual({ cmd: "git_key_create", args: { host: "code", replace: false } });
    expect(row("code").textContent).toContain("Key not on GitHub yet");

    await act(async () => button("Sign in to GitHub", row("code"))!.click());
    await flush();
    // The code, on the clipboard, and GitHub's page opened, once.
    expect(row("code").querySelector(".git-code-value")?.textContent).toBe("CODE-CODE");
    expect(copied).toEqual(["CODE-CODE"]);
    expect(opened).toEqual(["https://github.com/login/device"]);

    approve("code");
    await act(async () => vi.advanceTimersByTime(2000));
    await flush();
    expect(row("code").textContent).toContain("Signed in as alvin");
    expect(row("code").querySelector(".git-code-value")).toBeNull();

    await act(async () => button("Add key to GitHub", row("code"))!.click());
    await flush();
    expect(row("code").textContent).toContain("Ready");
    expect(row("code").querySelector('[aria-label="Check of code"]')?.textContent).toContain("The key signs in to GitHub as alvin.");
    expect(button("Check", row("code"))).toBeDefined();
  });

  it("sets up every host at once, adding each key as its sign-in ends", async () => {
    await render();
    await act(async () => button("Set up all hosts")!.click());
    await flush();
    for (const h of ["code", "mac"]) {
      expect(calls).toContainEqual({ cmd: "git_key_create", args: { host: h, replace: false } });
      expect(calls).toContainEqual({ cmd: "github_login", args: { host: h, token: null } });
    }
    expect(copied).toEqual(["CODE-CODE", "CODE-MAC"]);
    approve("code");
    approve("mac");
    await act(async () => vi.advanceTimersByTime(2000));
    await flush();
    await flush();
    expect(calls.filter((c) => c.cmd === "git_key_add").map((c) => c.args.host)).toEqual(["code", "mac"]);
    expect(button("Set up all hosts")).toBeUndefined();
  });

  it("signs every host in with a token instead", async () => {
    await render();
    const token = el.querySelector<HTMLInputElement>('input[aria-label="GitHub token"]')!;
    await act(async () => type(token, "github_pat_x"));
    await act(async () => button("Use on all hosts")!.click());
    await flush();
    expect(calls.filter((c) => c.cmd === "github_login").map((c) => c.args)).toEqual([
      { host: "code", token: "github_pat_x" },
      { host: "mac", token: "github_pat_x" },
    ]);
    // Keys made first, then added to the account once signed in.
    expect(calls.filter((c) => c.cmd === "git_key_create").map((c) => c.args.host)).toEqual(["code", "mac"]);
    await flush();
    expect(calls.filter((c) => c.cmd === "git_key_add").map((c) => c.args.host)).toEqual(["code", "mac"]);
    expect(token.value).toBe("");
    expect(el.innerHTML).not.toContain("github_pat_x");
  });

  it("leaves a sign-in started elsewhere to its buttons, and cancels one", async () => {
    // Begun from another Mac, say: no clipboard, no browser by itself.
    hosts.views = {
      code: { ssh_key: { public: "ssh-ed25519 AAAA otter@code" }, login: { state: "pending", code: "THEIR-CODE", url: "https://github.com/login/device" } },
    };
    await render(["code"]);
    expect(row("code").querySelector(".git-code-value")?.textContent).toBe("THEIR-CODE");
    expect(copied).toEqual([]);
    expect(opened).toEqual([]);
    await act(async () => button("Cancel", row("code"))!.click());
    await flush();
    expect(calls).toContainEqual({ cmd: "github_logout", args: { host: "code" } });
    expect(row("code").querySelector(".git-code-value")).toBeNull();
    expect(button("Sign in to GitHub", row("code"))).toBeDefined();
  });

  it("says which host didn't take the identity", async () => {
    hosts.views = { code: {}, old: null };
    await render(["code", "old"]);
    const name = el.querySelector<HTMLInputElement>('input[aria-label="Git name"]')!;
    await act(async () => type(name, "Ada"));
    await act(async () => button("Save on all hosts")!.click());
    await flush();
    expect(el.textContent).toContain("old: old's otterd is too old for this: update it");
  });

  it("says an older otterd can't do this", async () => {
    hosts.views = { old: null };
    await render(["old"]);
    expect(el.textContent).toContain("Update otterd on old to set this up.");
    expect(button("Set up old")).toBeUndefined();
  });
});
