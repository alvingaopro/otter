import { describe, expect, it } from "vitest";
import { glance, trayLabels } from "./model";

const line = (name: string, what: string, host = "code", key = name) => ({ key, name, what, host });

describe("menu bar glance", () => {
  it("says what to do, with only a file's name", () => {
    expect(
      glance(
        "claude needs your approval: Read: /home/alvin/Work/marketplace/cs-96847/apps/salesforce/force-app/main/default/lwc/createOffer/createOffer.html",
      ),
    ).toBe("Approve: Read createOffer.html");
    expect(glance("codex needs your approval: Bash: rm -rf ./build/out")).toBe("Approve: Bash rm -rf out");
    expect(glance("waiting for input (+2)")).toBe("waiting for input (+2)");
  });

  it("says a turn finished, not what it said", () => {
    expect(glance("claude-2 finished: Yes, it would exceed the request limit.\nSo I split it.")).toBe("claude-2 finished");
    expect(glance("claude-2 finished: Yes, it would exceed the request limit (+1)")).toBe("claude-2 finished (+1)");
    expect(glance("claude finished its turn")).toBe("claude finished its turn");
  });

  it("fits a menu", () => {
    const [{ label }] = trayLabels([line("n".repeat(100), `Approve: Bash ${"x".repeat(200)} (+3)`)]);
    expect(label.length).toBeLessThanOrEqual(44);
    expect(label.endsWith("… (+3)")).toBe(true);
    expect(trayLabels([line("marketplace", "claude-2 finished: Yes, it would exceed the request limit")])[0].label).toBe(
      "marketplace · claude-2 finished",
    );
  });

  it("names the host only when another host has the same name", () => {
    const labels = trayLabels([
      line("api", "agent finished", "mac", "a"),
      line("api", "build failed (status 1)", "box", "b"),
      line("web", "agent finished", "box", "c"),
    ]).map((l) => l.label);
    expect(labels).toEqual(["api (mac) · agent finished", "api (box) · build failed (status 1)", "web · agent finished"]);
  });
});
