// What tasks need of the developer, for the notifications (D-073) and the
// menu bar item. Pure: which needs are news, and in what order they wait.

import { needOf, type Need, type PlacedTask } from "./tasks";
import type { TrayLine } from "./model";

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
