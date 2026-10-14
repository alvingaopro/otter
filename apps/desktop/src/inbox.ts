// The Inbox (design §26, D-072): everything going on, on every connected
// host — features and workspaces — in one list. What needs the developer
// comes first, longest-waiting first; then what is working; then what is
// done. Pure: grouping, order and wording only.

import { featureActivity, needOf, STATUS_LABEL, statusDetail, type Need, type PlacedFeature } from "./features";
import { headline } from "./model";
import type { Activity, Placed } from "./types";

export type InboxGroupId = "needs" | "working" | "done";

export interface InboxItem {
  /** `feature:host/id` or `workspace:host/id`. */
  id: string;
  host: string;
  title: string;
  summary: string;
  /** Since when it waits (needs), or its latest activity. */
  since?: string;
  activity: Activity;
  feature?: PlacedFeature;
  workspace?: Placed;
  need?: Need;
}

export interface InboxGroup {
  id: InboxGroupId;
  label: string;
  items: InboxItem[];
}

/** Done items shown: the most recent. */
export const DONE_SHOWN = 8;

const GROUP: Partial<Record<Activity, InboxGroupId>> = {
  needs_you: "needs",
  failed: "needs",
  working: "working",
  preparing: "working",
  completed: "done",
};

export function inboxItems(features: PlacedFeature[], workspaces: Placed[]): InboxItem[] {
  const fromFeatures = features.map((p): InboxItem => {
    const need = needOf(p.feature);
    return {
      id: `feature:${p.key}`,
      host: p.host,
      title: p.feature.title,
      summary: need?.summary ?? statusDetail(p.feature) ?? STATUS_LABEL[p.feature.status],
      since: need?.since ?? p.feature.updated_at,
      activity: featureActivity(p.feature),
      feature: p,
      need,
    };
  });
  const fromWorkspaces = workspaces.map((p): InboxItem => {
    const h = headline(p.ws);
    return {
      id: `workspace:${p.key}`,
      host: p.host.name,
      title: p.ws.name,
      summary: h.text,
      since: h.since ?? p.ws.updatedAt,
      activity: p.ws.activity,
      workspace: p,
    };
  });
  return [...fromFeatures, ...fromWorkspaces];
}

/** NEEDS YOU (longest-waiting first), WORKING and DONE (newest first); idle and archived work isn't in the inbox. */
export function inboxGroups(items: InboxItem[]): InboxGroup[] {
  const groups: InboxGroup[] = [
    { id: "needs", label: "NEEDS YOU", items: [] },
    { id: "working", label: "WORKING", items: [] },
    { id: "done", label: "DONE", items: [] },
  ];
  for (const item of items) {
    const g = GROUP[item.activity];
    if (g) groups.find((x) => x.id === g)!.items.push(item);
  }
  const oldest = (a: InboxItem, b: InboxItem) => (a.since ?? "").localeCompare(b.since ?? "");
  const newest = (a: InboxItem, b: InboxItem) => (b.since ?? "").localeCompare(a.since ?? "");
  groups[0].items.sort(oldest);
  groups[1].items.sort(newest);
  groups[2].items = groups[2].items.sort(newest).slice(0, DONE_SHOWN);
  return groups.filter((g) => g.items.length > 0);
}

/** How many things wait on the developer (the Inbox's badge). */
export function needCount(items: InboxItem[]): number {
  return items.filter((i) => GROUP[i.activity] === "needs").length;
}

/** A need, as one thing that happened: notified about once (D-073). */
export function needId(need: Need): string {
  return `${need.kind}:${need.decision_id ?? ""}:${need.since}`;
}

export interface NewNeed {
  key: string;
  host: string;
  title: string;
  need: Need;
}

/**
 * What features newly need the developer, since `seen` (feature key → the
 * need it had, `null` for none). A feature seen for the first time is the
 * baseline — what waited before the app looked isn't news.
 */
export function newNeeds(seen: Map<string, string | null>, list: PlacedFeature[]): NewNeed[] {
  const out: NewNeed[] = [];
  for (const p of list) {
    const need = needOf(p.feature);
    const id = need ? needId(need) : null;
    if (seen.has(p.key) && need && id !== seen.get(p.key)) {
      out.push({ key: p.key, host: p.host, title: p.feature.title, need });
    }
    seen.set(p.key, id);
  }
  return out;
}

/** Features in the menu bar item: what waits, longest first (keys `feature:host/id`). */
export function trayFeatures(list: PlacedFeature[], label: (title: string, what: string, host: string) => string): { key: string; label: string }[] {
  return list
    .map((p) => ({ p, need: needOf(p.feature) }))
    .filter((x): x is { p: PlacedFeature; need: Need } => x.need !== undefined)
    .sort((a, b) => a.need.since.localeCompare(b.need.since))
    .map(({ p, need }) => ({ key: `feature:${p.key}`, label: label(p.feature.title, need.summary, p.host) }));
}
