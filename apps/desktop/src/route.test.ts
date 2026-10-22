import { describe, expect, it } from "vitest";
import { routeHost } from "./route";

const hosts = {
  dev: [
    { name: "billing", repository: "git@github.com:acme/billing.git" },
    { name: "web", repository: "https://github.com/acme/web-app" },
  ],
  gpu: [{ name: "trainer", repository: "git@github.com:acme/trainer.git" }],
};

describe("routeHost (D-179, as route.rs)", () => {
  it("sends a message that names a workspace to the host that has it", () => {
    expect(routeHost("Add retries in billing, please.", hosts)).toEqual({
      kind: "one",
      host: "dev",
      why: "workspace billing",
    });
  });

  it("sends one that names a repository to its host, by address or by name", () => {
    expect(routeHost("train faster: acme/trainer", hosts)).toMatchObject({ kind: "one", host: "gpu" });
    expect(routeHost("fix https://github.com/acme/trainer.git", hosts)).toMatchObject({ host: "gpu" });
    expect(routeHost("the web-app login is slow", hosts)).toEqual({
      kind: "one",
      host: "dev",
      why: "repository web-app",
    });
  });

  it("does not find a short name inside a longer one", () => {
    expect(routeHost("rebuild the billing-engine", hosts)).toEqual({ kind: "nowhere" });
    expect(routeHost("subbilling", hosts)).toEqual({ kind: "nowhere" });
  });

  it("says when nothing is named, and when several hosts have it equally", () => {
    expect(routeHost("write a csv converter", hosts)).toEqual({ kind: "nowhere" });
    expect(routeHost("billing", { a: [{ name: "billing" }], b: [{ name: "billing" }] })).toEqual({
      kind: "several",
      hosts: ["a", "b"],
    });
  });

  it("prefers a workspace's name to a repository's", () => {
    const h = { a: [{ name: "misc", repository: "git@h:other/trainer.git" }], b: [{ name: "trainer" }] };
    expect(routeHost("trainer", h)).toMatchObject({ kind: "one", host: "b" });
  });

  it("does not take a credential in an address for a name", () => {
    const h = { a: [{ name: "x1", repository: "https://secret-token@github.com/acme/billing.git" }] };
    expect(routeHost("secret-token", h)).toEqual({ kind: "nowhere" });
    expect(routeHost("acme/billing", h)).toMatchObject({ host: "a" });
  });
});
