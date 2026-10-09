import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FeaturesView } from "./FeaturesView";
import { mockSource, sampleFeatures, type FeatureSource } from "./featureSource";
import { STATUS_LABEL, matches, needsYou, sortFeatures, statusDetail, type Feature, type PlacedFeature } from "./features";

describe("feature contract helpers", () => {
  const [active, blocked, failed, done, draft] = sampleFeatures(Date.parse("2026-10-09T12:00:00Z"));

  it("knows which features need the developer", () => {
    expect(needsYou(blocked)).toBe(true);
    expect(needsYou(active)).toBe(false);
    expect(needsYou({ ...done, status: "review" })).toBe(true);
  });

  it("filters by status", () => {
    expect(matches(active, "active")).toBe(true);
    expect(matches(draft, "active")).toBe(true);
    expect(matches(failed, "failed")).toBe(true);
    expect(matches(done, "done")).toBe(true);
    expect(matches(done, "needs")).toBe(false);
  });

  it("shows Otter's loop as one state, with what it's doing", () => {
    // Planning, implementing and verifying are all just "Working" (D-050).
    expect([STATUS_LABEL.planning, STATUS_LABEL.implementing, STATUS_LABEL.verifying]).toEqual(["Working", "Working", "Working"]);
    expect(STATUS_LABEL.review).toBe("Ready to check");
    expect(statusDetail(active)).toBe("task 2 of 2: Wire Export button into Timeline panel");
    expect(statusDetail(blocked)).toContain("Waiting for your approval");
    expect(statusDetail(draft)).toBeUndefined();
    expect(failed.status).toBe("failed");
  });

  it("sorts what needs you first, then newest", () => {
    const placed: PlacedFeature[] = [active, blocked, done].map((f) => ({ host: "h", feature: f, key: f.id }));
    expect(sortFeatures(placed).map((p) => p.feature.title)).toEqual([blocked.title, active.title, done.title]);
  });
});

let root: Root;
let host: HTMLDivElement;
let source: FeatureSource;
const onOpenWorkspace = vi.fn();

const q = <T extends Element = HTMLElement>(sel: string) => host.querySelector<T & HTMLElement>(sel);
const byText = (sel: string, text: string) =>
  [...host.querySelectorAll<HTMLElement>(sel)].find((e) => e.textContent?.includes(text)) ??
  [...document.querySelectorAll<HTMLElement>(sel)].find((e) => e.textContent?.includes(text));

function type(el: HTMLInputElement | HTMLTextAreaElement, value: string) {
  const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, "value")!.set!.call(el, value);
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

async function render(s: FeatureSource) {
  source = s;
  await act(async () =>
    root.render(
      <FeaturesView
        source={s}
        hosts={["mac"]}
        workspaces={{ mac: [{ id: "ws_demo", name: "demo" }] }}
        now={Date.now()}
        onOpenWorkspace={onOpenWorkspace}
        onOpenPreview={async () => {}}
      />,
    ),
  );
}

describe("Features view", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    onOpenWorkspace.mockReset();
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  it("shows an empty state with a create action", async () => {
    const empty = mockSource("mac");
    // Start from nothing.
    const s: FeatureSource = { ...empty, list: () => [], subscribe: () => () => {} };
    await render(s);
    expect(host.textContent).toContain("Describe a feature");
    expect(byText("button", "New feature")).toBeTruthy();
  });

  it("lists features with what needs you first and says it is a preview", async () => {
    await render(mockSource("mac"));
    const rows = [...host.querySelectorAll('[role="listitem"]')].map((r) => r.querySelector(".ws-name")?.textContent);
    expect(rows[0]).toBe("Rotate the staging database password");
    expect(rows).toHaveLength(5);
    expect(q('[role="note"]')?.textContent).toContain("Preview");
    // The selected (blocked) feature opens on its approvals.
    expect(q('[role="tab"][aria-selected="true"]')?.textContent).toContain("Approvals");
    expect(q(".banner.needs")).not.toBeNull();
  });

  it("creates a draft, converses and starts it", async () => {
    await render(mockSource("mac"));
    await act(async () => q('[aria-label="New feature"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "CSV export");
    type(dialog.querySelector("textarea")!, "Export timelines as CSV");
    await act(async () => byText("button", "Create draft")!.click());

    expect(q(".feature-title")?.textContent).toBe("CSV export");
    expect(q(".status-pill")?.textContent).toBe("Draft");
    expect(host.querySelector(".message.user")?.textContent).toContain("Export timelines as CSV");

    const composer = q<HTMLTextAreaElement>('textarea[aria-label="Message to Otter"]')!;
    type(composer, "Also include the session name");
    await act(async () => byText("button", "Send")!.click());
    const messages = [...host.querySelectorAll(".message")].map((m) => m.textContent);
    expect(messages.some((m) => m?.includes("Also include the session name"))).toBe(true);
    expect(host.querySelector(".message.system")?.textContent).toContain("nothing is connected");

    await act(async () => byText("button", "Start")!.click());
    expect(q(".status-pill")?.textContent).toBe("Working");
    // No progress bar, no takeover.
    expect(q(".stages")).toBeNull();
    expect(byText("button", "Take over")).toBeUndefined();
  });

  it("creates a feature in a chosen workspace", async () => {
    await render(mockSource("mac"));
    await act(async () => q('[aria-label="New feature"]')!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    type(dialog.querySelector("input")!, "In a workspace");
    const selects = dialog.querySelectorAll("select");
    const select = selects[selects.length - 1];
    select.value = "ws_demo";
    await act(async () => select.dispatchEvent(new Event("change", { bubbles: true })));
    await act(async () => byText("button", "Create draft")!.click());
    const created = source.list().find((p) => p.feature.title === "In a workspace")!;
    expect(created.feature.workspace_id).toBe("ws_demo");
    // The header links to it; no "choose a workspace" prompt.
    expect(byText("button.chip", "demo")).toBeTruthy();
    expect(q('[aria-label="Workspace"][role="group"]')).toBeNull();
  });

  it("answers an approval and the feature moves on", async () => {
    await render(mockSource("mac"));
    await act(async () => byText(".decision button", "Approve")!.click());
    const blocked = source.list().find((p) => p.feature.title.startsWith("Rotate"))!;
    expect(blocked.feature.decisions[0].status).toBe("approved");
    expect(blocked.feature.decisions[0].decided_by).toBe("user");
    expect(blocked.feature.status).toBe("implementing");
    expect(q(".banner.needs")).toBeNull();
  });

  it("filters, and handles failed and completed features", async () => {
    await render(mockSource("mac"));
    await act(async () => byText(".filter-chip", "Failed")!.click());
    expect(host.querySelectorAll('[role="listitem"]')).toHaveLength(1);
    expect(q(".banner.failure")?.textContent).toContain("6 of 6 attempts");
    expect(byText("button", "Retry")).toBeTruthy();

    await act(async () => byText(".filter-chip", "Done")!.click());
    expect(q(".status-pill")?.textContent).toBe("Done");
    expect(byText("button", "Cancel")).toBeUndefined();
    await act(async () => byText('[role="tab"]', "Evidence")!.click());
    expect(host.querySelector(".evidence-item a")?.getAttribute("href")).toContain("/pull/31");
  });

  it("shows the gates and only offers a plain Accept when they pass", async () => {
    const base = mockSource("mac");
    const [done] = sampleFeatures().filter((f) => f.status === "done");
    const review: Feature = {
      ...done,
      status: "review",
      gates: [
        { name: "Requirements met", status: "passed" },
        { name: "CI", status: "pending", detail: "running" },
        { name: "Your acceptance", status: "pending" },
      ],
      delivery: { branch: "feature/x", pr_url: "https://github.com/o/r/pull/7", pr_number: 7, ci: [{ name: "test", state: "pending" }], reruns: 0, declined: false },
      report: undefined,
    };
    let list: PlacedFeature[] = [{ host: "mac", feature: review, key: "mac/" + review.id }];
    const acts: unknown[] = [];
    const s: FeatureSource = { ...base, list: () => list, subscribe: () => () => {}, act: async (_k, a) => void acts.push(a) };
    await render(s);
    expect(byText("button", "Accept anyway")).toBeTruthy();
    await act(async () => byText('[role="tab"]', "Delivery")!.click());
    expect(host.textContent).toContain("https://github.com/o/r/pull/7");
    expect(host.textContent).toContain("running");
    await act(async () => byText("button", "Accept anyway")!.click());
    expect(acts).toEqual([{ action: "accept", override_gates: true }]);

    list = [{ ...list[0], feature: { ...review, gates: review.gates!.map((g) => ({ ...g, status: g.name === "Your acceptance" ? g.status : "passed" })) } }];
    act(() => root.unmount());
    root = createRoot(host);
    await render(s);
    expect(byText("button", "Accept anyway")).toBeUndefined();
    expect([...host.querySelectorAll("button")].some((b) => b.textContent === "Accept")).toBe(true);
  });

  it("links a task to its workspace and session", async () => {
    await render(mockSource("mac"));
    await act(async () => byText(".filter-chip", "Active")!.click());
    await act(async () => byText('[role="listitem"]', "Export workspace timeline")!.click());
    await act(async () => byText('[role="tab"]', "Tasks")!.click());
    await act(async () => byText("button", "Open session")!.click());
    expect(onOpenWorkspace).toHaveBeenCalledWith("mac", "ws_demo", "ses_demo");
  });
});
