import { describe, expect, it } from "vitest";
import { glance, trayLabel } from "./model";

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

  it("fits a menu", () => {
    const g = glance(`Approve: Bash ${"x".repeat(200)}`);
    expect(g.length).toBeLessThanOrEqual(52);
    expect(g.endsWith("…")).toBe(true);
  });

  it("names the host only when there are several", () => {
    expect(trayLabel("feature", "agent finished", "code", false)).toBe("feature · agent finished");
    expect(trayLabel("feature", "agent finished", "code", true)).toBe("feature · agent finished  (code)");
  });
});
