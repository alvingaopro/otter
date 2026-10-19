import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const calls = vi.hoisted(() => [] as { cmd: string; args: Record<string, unknown> }[]);
const kept = vi.hoisted(() => ({ sites: ["*.okta.test"] as string[] }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    calls.push({ cmd, args });
    if (cmd === "sign_in_sites") return { builtIn: ["AWS", "Salesforce"], sites: kept.sites };
    if (cmd === "sign_in_allow") {
      const site = String(args.site);
      if (!site.includes(".")) throw "`localhost` isn't a site: try login.example.com or *.okta.test";
      kept.sites = [...kept.sites, new URL(site.includes("://") ? site : `https://${site}`).host];
      return kept.sites;
    }
    if (cmd === "sign_in_forget") {
      kept.sites = kept.sites.filter((s) => s !== args.site);
      return kept.sites;
    }
    return undefined;
  }),
}));

import { SignInAsks, SignInSites } from "./SignIn";

let root: Root;
let el: HTMLDivElement;

beforeEach(() => {
  calls.length = 0;
  kept.sites = ["*.okta.test"];
  el = document.createElement("div");
  document.body.appendChild(el);
  root = createRoot(el);
});

afterEach(() => {
  act(() => root.unmount());
  el.remove();
});

const flush = () => act(async () => {});
const button = (name: string) =>
  [...el.querySelectorAll("button")].find((b) => b.textContent === name || b.getAttribute("aria-label") === name)!;

function type(input: HTMLInputElement, value: string) {
  const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  set.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

describe("the sites that open without asking (D-121)", () => {
  it("lists them, adds a pasted link as its site, and forgets one", async () => {
    await act(async () => root.render(<SignInSites />));
    await flush();
    expect(el.textContent).toContain("*.okta.test");
    expect(el.textContent).toContain("Built in: AWS, Salesforce");

    const input = el.querySelector<HTMLInputElement>('input[aria-label="Add a sign-in site"]')!;
    await act(async () => type(input, "https://login.corp.test/oauth?x=1"));
    await act(async () => button("Add").click());
    await flush();
    expect(calls.find((c) => c.cmd === "sign_in_allow")?.args.site).toBe("https://login.corp.test/oauth?x=1");
    expect(el.textContent).toContain("login.corp.test");
    expect(input.value).toBe("");

    await act(async () => button("Ask before opening *.okta.test").click());
    await flush();
    expect(el.textContent).not.toContain("*.okta.test");
  });

  it("says what's wrong with what was typed", async () => {
    await act(async () => root.render(<SignInSites />));
    const input = el.querySelector<HTMLInputElement>('input[aria-label="Add a sign-in site"]')!;
    await act(async () => type(input, "localhost"));
    await act(async () => button("Add").click());
    await flush();
    expect(el.textContent).toContain("isn't a site");
  });
});

describe("asking about a sign-in page (D-121)", () => {
  const ask = { host: "dev-01", requestId: "login_1", site: "login.corp.test", workspace: "billing" };

  it("opens it once or always, or not now", async () => {
    const done: string[] = [];
    await act(async () => root.render(<SignInAsks asks={[ask]} onDone={(id) => done.push(id)} />));
    expect(el.textContent).toContain("Sign in at login.corp.test?");
    expect(el.textContent).toContain("in billing on dev-01");

    await act(async () => button("Open").click());
    expect(calls[calls.length - 1]).toEqual({
      cmd: "sign_in_open",
      args: { host: "dev-01", requestId: "login_1", site: "login.corp.test", always: false },
    });
    await act(async () => button("Always open").click());
    expect(calls[calls.length - 1]?.args.always).toBe(true);
    const asked = calls.length;
    await act(async () => button("Not now").click());
    expect(calls.length).toBe(asked);
    expect(done).toEqual(["login_1", "login_1", "login_1"]);
  });

  it("shows nothing when nothing waits", async () => {
    await act(async () => root.render(<SignInAsks asks={[]} onDone={() => {}} />));
    expect(el.innerHTML).toBe("");
  });
});
