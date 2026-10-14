import { describe, expect, it } from "vitest";
import { sampleFeatures } from "./featureSource";
import { inboxGroups, inboxItems, needCount } from "./inbox";
import { workspace } from "./inboxFixtures";

const NOW = Date.parse("2026-10-09T12:00:00Z");

describe("the inbox", () => {
  const [active, blocked, failed, done, draft] = sampleFeatures(NOW);
  const features = [active, blocked, failed, done, draft].map((feature) => ({ host: "mac", feature, key: `mac/${feature.id}` }));
  const workspaces = [workspace("api", "needs_you", 200), workspace("web", "working", 30), workspace("old", "idle", 900)];

  it("puts what needs you first, longest-waiting first, across features and workspaces", () => {
    const groups = inboxGroups(inboxItems(features, workspaces));
    expect(groups.map((g) => g.label)).toEqual(["NEEDS YOU", "WORKING", "DONE"]);
    const needs = groups[0].items;
    // The workspace has waited 200 minutes; the stopped feature 90; then the decision.
    expect(needs.map((i) => i.title)).toEqual(["api", failed.title, blocked.title]);
    expect(needs[0].summary).toContain("make deploy");
    expect(needs[1].need?.kind).toBe("failed");
    expect(needs[2].need?.kind).toBe("decision");
    expect(needs[2].summary).toBe(blocked.decisions[0].summary);
    // Working: newest first.
    expect(groups[1].items.map((i) => i.title)).toEqual([active.title, "web"]);
    expect(groups[2].items.map((i) => i.title)).toEqual([done.title]);
    // Idle work (a draft, an idle workspace) isn't in the inbox.
    expect(groups.flatMap((g) => g.items).some((i) => i.title === draft.title || i.title === "old")).toBe(false);
    expect(needCount(inboxItems(features, workspaces))).toBe(3);
  });

  it("is empty when nothing is going on", () => {
    expect(inboxGroups(inboxItems([], [workspace("old", "idle", 1)]))).toEqual([]);
  });
});
