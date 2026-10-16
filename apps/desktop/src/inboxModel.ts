// The Inbox (design §26, D-072): everything going on, on every connected
// host — tasks and workspaces — in one list. What needs the developer
// comes first, longest-waiting first; then what is working; then what is
// done. Pure: grouping, order and wording only.

import { taskActivity, needOf, STATUS_LABEL, statusDetail, type Need, type PlacedTask } from "./tasks";
import { headline, type TrayLine } from "./model";
import type { Activity, Placed } from "./types";

export type InboxGroupId = "needs" | "working" | "done";

export interface InboxItem {
  /** `task:host/id` or `workspace:host/id`. */
  id: string;
  host: string;
  title: string;
  summary: string;
  /** Since when it waits (needs), or its latest activity. */
  since?: string;
  activity: Activity;
  task?: PlacedTask;
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

export function inboxItems(tasks: PlacedTask[], workspaces: Placed[]): InboxItem[] {
  const fromTasks = tasks.map((p): InboxItem => {
    const need = needOf(p.task);
    return {
      id: `task:${p.key}`,
      host: p.host,
      title: p.task.title,
      summary: need?.summary ?? statusDetail(p.task) ?? STATUS_LABEL[p.task.status],
      since: need?.since ?? p.task.updated_at,
      activity: taskActivity(p.task),
      task: p,
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
  return [...fromTasks, ...fromWorkspaces];
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
 * What tasks newly need the developer, since `seen` (task key → the
 * need it had, `null` for none). A task seen for the first time is the
 * baseline — what waited before the app looked isn't news.
 */
export function newNeeds(seen: Map<string, string | null>, list: PlacedTask[]): NewNeed[] {
  const out: NewNeed[] = [];
  for (const p of list) {
    const need = needOf(p.task);
    const id = need ? needId(need) : null;
    if (seen.has(p.key) && need && id !== seen.get(p.key)) {
      out.push({ key: p.key, host: p.host, title: p.task.title, need });
    }
    seen.set(p.key, id);
  }
  return out;
}

/** Tasks in the menu bar item: what waits, longest first (keys `task:host/id`). */
export function trayTasks(list: PlacedTask[]): TrayLine[] {
  return list
    .map((p) => ({ p, need: needOf(p.task) }))
    .filter((x): x is { p: PlacedTask; need: Need } => x.need !== undefined)
    .sort((a, b) => a.need.since.localeCompare(b.need.since))
    .map(({ p, need }) => ({ key: `task:${p.key}`, name: p.task.title, what: need.summary, host: p.host }));
}
