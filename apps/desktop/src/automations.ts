// Automations (D-185): what Otter does without a message from you — the
// watches it keeps for you (D-183) and the host's triggers (D-083) — and
// what it did on its tools (D-192). Pure: grouping and wording, no runtime
// state.

import type { PlacedTask, TaskAction, Watch, WatchOutcome } from "./tasks";

/** When a trigger fires: mirrors `otter_core::trigger::Schedule`. */
export type Schedule = { kind: "every"; minutes: number } | { kind: "daily"; at: string } | { kind: "manual" };

/** What a trigger's work may do without you, decided in advance (D-083). */
export interface TriggerPolicy {
  publish?: boolean;
  max_iterations?: number;
  max_minutes?: number;
  wait_limit_secs?: number;
}

/** One occurrence: the task it started, or why it was skipped. */
export interface Firing {
  occurrence: string;
  at: string;
  task_id?: string;
  skipped?: string;
}

/** Work that starts on a schedule (D-083): mirrors `otter_core::trigger::Trigger`. */
export interface Trigger {
  id: string;
  name: string;
  workspace_id: string;
  title: string;
  request: string;
  schedule: Schedule;
  policy: TriggerPolicy;
  enabled: boolean;
  created_at: string;
  next_at?: string;
  history: Firing[];
}

/** A watch as the view lists it: on its task, on its host. */
export interface PlacedWatch {
  /** `host/task-id/watch-id`. */
  key: string;
  host: string;
  task: PlacedTask;
  watch: Watch;
}

/** A trigger as the view lists it: on its host. */
export interface PlacedTrigger {
  /** `host/trigger-id`. */
  key: string;
  host: string;
  trigger: Trigger;
}

export type Item = { kind: "watch"; item: PlacedWatch } | { kind: "trigger"; item: PlacedTrigger };

export function isOpen(w: Watch): boolean {
  return w.status === "waiting" || w.status === "firing";
}

/** A task's watches still waiting or firing. */
export function openWatches(t: { watches?: Watch[] }): Watch[] {
  return (t.watches ?? []).filter(isOpen);
}

/** Every watch of every task, placed. */
export function placeWatches(tasks: PlacedTask[]): PlacedWatch[] {
  return tasks.flatMap((p) => (p.task.watches ?? []).map((watch) => ({ key: `${p.key}/${watch.id}`, host: p.host, task: p, watch })));
}

export interface AutomationGroups {
  /** Waiting or firing, the soonest deadline first. */
  watching: PlacedWatch[];
  /** The host's triggers: on ones by their next occurrence, then paused ones, by name. */
  scheduled: PlacedTrigger[];
  /** Ended watches, the latest first. */
  history: PlacedWatch[];
}

export function groupAutomations(tasks: PlacedTask[], triggers: PlacedTrigger[]): AutomationGroups {
  const all = placeWatches(tasks);
  const watching = all.filter((w) => isOpen(w.watch)).sort((a, b) => a.watch.deadline.localeCompare(b.watch.deadline));
  const ended = (w: PlacedWatch) => w.watch.ended_at ?? w.watch.finding?.at ?? w.watch.created_at;
  const history = all.filter((w) => !isOpen(w.watch)).sort((a, b) => ended(b).localeCompare(ended(a)));
  const scheduled = [...triggers].sort((a, b) => {
    if (a.trigger.enabled !== b.trigger.enabled) return a.trigger.enabled ? -1 : 1;
    const an = a.trigger.next_at ?? "";
    const bn = b.trigger.next_at ?? "";
    if (an !== bn) return an === "" ? 1 : bn === "" ? -1 : an.localeCompare(bn);
    return a.trigger.name.localeCompare(b.trigger.name);
  });
  return { watching, scheduled, history };
}

/** A time in the Mac's own zone: `14:05`, or `Oct 23 14:05` on another day. */
export function localTime(iso: string | undefined, now: number): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const hm = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
  if (d.toDateString() === new Date(now).toDateString()) return hm;
  const day = d.toLocaleDateString([], { month: "short", day: "numeric" });
  return `${day} ${hm}`;
}

/** How a watch ended, or where it is, in a word or two. */
export const OUTCOME_LABEL: Record<WatchOutcome, string> = {
  passed: "passed",
  failed: "failed",
  stuck: "can't go on",
  gone: "merged or closed",
  due: "time came",
  expired: "ran out of time",
  unreadable: "couldn't be read",
  unknown: "ended",
};

/** An outcome in words; one from a newer otterd as it's named. */
export function outcomeLabel(o: string | undefined): string {
  if (!o) return OUTCOME_LABEL.unknown;
  return (OUTCOME_LABEL as Record<string, string>)[o] ?? o;
}

export function watchState(w: Watch): string {
  if (w.status === "cancelled") return "stopped";
  if (w.status === "waiting") return "watching";
  const o = outcomeLabel(w.finding?.outcome);
  if (w.status === "firing") return `${o} — telling you`;
  if (w.status === "reported") return o;
  // A status this build doesn't know: as it's named.
  return w.status;
}

/** An open watch whose deadline is behind it: otterd was down, or hasn't looked yet. */
export function pastDeadline(w: Watch, now: number): boolean {
  return isOpen(w) && Date.parse(w.deadline) <= now;
}

/** When an open watch ends: its deadline, or that it ends when otterd looks next. */
export function untilText(w: Watch, now: number): string {
  if (pastDeadline(w, now)) return w.kind === "time" ? "due · said at the next look" : "deadline passed · ends at the next look";
  return `${w.kind === "time" ? "at" : "until"} ${localTime(w.deadline, now)}`;
}

/** The status mark of a watch: shape and colour by how it stands. */
export function watchGlyph(w: Watch): "needs" | "working" | "done" | "failed" | "idle" {
  if (isOpen(w) && w.status === "waiting") return "working";
  if (w.status === "cancelled") return "idle";
  switch (w.finding?.outcome) {
    case "passed":
    case "due":
    case "gone":
      return "done";
    case "failed":
      return "failed";
    case "stuck":
    case "expired":
    case "unreadable":
      return "needs";
    default:
      return "idle";
  }
}

/** The second line of a watch's row. */
export function watchLine(p: PlacedWatch, now: number): string {
  const w = p.watch;
  if (isOpen(w)) return `${p.task.task.title} · ${untilText(w, now)}`;
  return `${p.task.task.title} · ${watchState(w)}`;
}

/** What a `ci` watch is on: `o/r #7`; nothing for other kinds. */
export function watchTarget(w: Watch): string | undefined {
  const t = w.target ?? {};
  if (w.kind === "ci" && typeof t.pr === "number") return `${typeof t.repo === "string" ? `${t.repo} ` : ""}#${t.pr}`;
  return undefined;
}

export function kindLabel(kind: string): string {
  return ({ ci: "CI", time: "A look later" } as Record<string, string>)[kind] ?? kind;
}

/** `every 30 min`, `every 2 h`, `daily at 04:20 (20:20 UTC)`, `by hand`; a kind from a newer otterd as it's named. */
export function scheduleText(s: Schedule, now = Date.now()): string {
  switch (s.kind) {
    case "every":
      return s.minutes % 60 === 0 ? `every ${s.minutes / 60} h` : `every ${s.minutes} min`;
    case "daily": {
      // Today at that time UTC, in this Mac's zone.
      const [h, m] = s.at.split(":").map(Number);
      const d = new Date(now);
      d.setUTCHours(h, m, 0, 0);
      const local = Number.isNaN(d.getTime()) ? "" : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
      return local && local !== s.at ? `daily at ${local} (${s.at} UTC)` : `daily at ${s.at} UTC`;
    }
    case "manual":
      return "by hand";
    default:
      return (s as { kind: string }).kind;
  }
}

/** When a trigger runs next: its time, or that it's due and runs when otterd looks next. */
export function nextText(t: Trigger, now: number): string {
  if (!t.enabled) return "paused";
  if (!t.next_at) return "only when run by hand";
  if (Date.parse(t.next_at) <= now) return "due · runs at the next look";
  return localTime(t.next_at, now);
}

/** The second line of a trigger's row. */
export function triggerLine(t: Trigger, now: number): string {
  if (!t.enabled) return `${scheduleText(t.schedule, now)} · paused`;
  if (t.next_at) {
    const next = nextText(t, now);
    // "due · runs at the next look" says when on its own.
    return `${scheduleText(t.schedule, now)} · ${Date.parse(t.next_at) <= now ? next : `next ${next}`}`;
  }
  return scheduleText(t.schedule, now);
}

/** What a trigger's policy allows, one phrase each. */
export function policyText(p: TriggerPolicy): string[] {
  const out: string[] = [];
  out.push(p.publish ? "publishes without asking" : "asks before publishing");
  if (p.max_iterations !== undefined) out.push(`at most ${p.max_iterations} attempts`);
  if (p.max_minutes !== undefined) out.push(`at most ${p.max_minutes} min of work`);
  if (p.wait_limit_secs !== undefined) out.push(`pauses after waiting ${Math.max(1, Math.round(p.wait_limit_secs / 60))} min for you`);
  return out;
}

/** The view's selection, by key: a watch (`host/task/watch`) or a trigger (`host/trigger`). */
export function findItem(groups: AutomationGroups, key: string | undefined): Item | undefined {
  if (!key) return undefined;
  const w = [...groups.watching, ...groups.history].find((x) => x.key === key);
  if (w) return { kind: "watch", item: w };
  const t = groups.scheduled.find((x) => x.key === key);
  return t ? { kind: "trigger", item: t } : undefined;
}

/**
 * One thing Otter did on its tools (D-192), from its task's history:
 * mirrors `otter_protocol::otter::OtterAction`.
 */
export interface OtterAction {
  task_id: string;
  task_title: string;
  /** Its place in the task's history. */
  seq: number;
  at: string;
  tool: string;
  /** What started it: `developer` (your message), `event` (a watch, a sub-task, a change) or `auto` (Otter's own step). */
  origin: string;
  /** Your message it acted on (its id), when it did. */
  authority?: string;
  /** What it was done on, when that says more than the tool: the command, the sub-task. */
  detail?: string;
  /** Why it did nothing, when it didn't: a refusal's code. */
  error?: string;
  /** What undoes it: an action on a task (the sub-task's, for one Otter made). */
  undo?: { task_id: string; action: TaskAction };
}

/** What each of Otter's tools did, in a few words; others by their name. */
const DID: Record<string, string> = {
  set_workspace: "Moved the task",
  set_answer_only: "Changed whether it only answers",
  start_task: "Started the task",
  resume_task: "Resumed the task",
  retry_task: "Retried the task",
  pause_task: "Paused the task",
  revise_task: "Planned it again",
  approve_plan: "Approved the plan",
  accept_task: "Accepted",
  finish_with_answer: "Answered and finished",
  create_task: "Made a sub-task",
  forward_to_agent: "Passed your message on",
  ask_agent: "Asked the coding agent",
  redirect_agent: "Redirected the coding agent",
  tell_agent: "Told the coding agent",
  watch: "Set a watch",
  cancel_watch: "Stopped a watch",
  publish: "Published",
  remember: "Noted in its memory",
  edit_memory: "Corrected its memory",
  Bash: "Ran a command",
  WebSearch: "Searched the web",
  WebFetch: "Read a page",
};

/** What Otter did, in plain words. */
export function actionText(tool: string): string {
  if (DID[tool]) return DID[tool];
  const words = tool.replace(/_/g, " ");
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** Who started it: your message, or Otter on its own (a step of its own, or something that happened). */
export function originText(origin: string): string {
  return origin === "developer" ? "on your word" : "on its own";
}

/** Why it did nothing, from the refusal's code. */
export function refusalText(code: string): string {
  return code.replace(/_/g, " ");
}
