// Presentation rules: grouping, ordering and wording. Classification itself
// (activity, session status) comes computed from otter-core via the backend.

import type { Activity, AttentionView, Glyph, HostsPayload, Placed, SessionView, WorkspaceView } from "./types";

export interface Group {
  id: "needs" | "working" | "done" | "idle" | "archived";
  label: string;
  items: Placed[];
}

const GROUP_OF: Record<Activity, Group["id"]> = {
  needs_you: "needs",
  failed: "needs",
  working: "working",
  preparing: "working",
  completed: "done",
  idle: "idle",
  archived: "archived",
};

const ORDER: Activity[] = ["needs_you", "failed", "working", "preparing", "completed", "idle", "archived"];

export function placeAll(payload: HostsPayload): Placed[] {
  return payload.hosts.flatMap((host) =>
    host.workspaces.map((ws) => ({ host, ws, key: `${host.name}/${ws.id}` })),
  );
}

export function groups(placed: Placed[]): Group[] {
  const out: Group[] = [
    { id: "needs", label: "NEEDS YOU", items: [] },
    { id: "working", label: "WORKING", items: [] },
    { id: "done", label: "COMPLETED", items: [] },
    { id: "idle", label: "IDLE", items: [] },
    { id: "archived", label: "ARCHIVED", items: [] },
  ];
  const sorted = [...placed].sort((a, b) => {
    const d = ORDER.indexOf(a.ws.activity) - ORDER.indexOf(b.ws.activity);
    if (d !== 0) return d;
    // Idle and archived: the latest active first (D-106).
    if (a.ws.activity === "idle" || a.ws.activity === "archived") {
      return lastActive(b.ws).localeCompare(lastActive(a.ws)) || a.ws.name.localeCompare(b.ws.name);
    }
    // Needs you: what blocks an agent before a finished turn (D-109).
    if (a.ws.activity === "needs_you") {
      const u = urgency(a.ws) - urgency(b.ws);
      if (u !== 0) return u;
    }
    // Longest-waiting first, as in `otter ls`.
    return (oldestAttention(a.ws) ?? "").localeCompare(oldestAttention(b.ws) ?? "") || a.ws.name.localeCompare(b.ws.name);
  });
  for (const p of sorted) out.find((g) => g.id === GROUP_OF[p.ws.activity])!.items.push(p);
  return out.filter((g) => g.items.length > 0);
}

/**
 * How much a workspace's attention blocks work (D-109): 0 an approval or a
 * question an agent reported, 1 one it seems to wait on (a guess), 2 only a
 * finished turn or less.
 */
export function urgency(ws: WorkspaceView): number {
  const blocking = ws.attention.filter((a) => a.kind === "approval" || a.kind === "question");
  if (blocking.some((a) => !a.inferred)) return 0;
  return blocking.length > 0 ? 1 : 2;
}

/**
 * A notification's title (D-109): an agent's prompt needs you; one it only
 * seems to wait on may; a finished turn is news, not a call.
 */
export function noticeTitle(name: string, a: AttentionView): string {
  if (a.kind === "failure") return `${name}: something failed`;
  if (a.kind === "review" || a.kind === "completion") return `${name}: turn finished`;
  return a.inferred ? `${name} may be waiting for you` : `${name} needs you`;
}

/** When a workspace was last active: its last change, an agent's last move, its newest attention. */
export function lastActive(ws: WorkspaceView): string {
  return [ws.updatedAt, ...ws.sessions.map((s) => s.agent?.since ?? ""), ...ws.attention.map((a) => a.createdAt)].reduce(
    (a, b) => (b > a ? b : a),
    "",
  );
}

/**
 * How well a workspace matches what was typed (⌘K, the sidebar's filter):
 * 0 not at all, higher better. Every word must be in its name, host, branch
 * or folder, repository, brief title or a session's name.
 */
export function matchScore(p: Placed, query: string): number {
  const q = query.trim().toLowerCase();
  if (!q) return 1;
  const name = p.ws.name.toLowerCase();
  const rest = [p.host.name, p.ws.source, p.ws.repository ?? "", p.ws.brief.title ?? "", ...p.ws.sessions.map((s) => s.name)]
    .join(" ")
    .toLowerCase();
  const words = q.split(/\s+/);
  if (!words.every((w) => name.includes(w) || rest.includes(w))) return 0;
  if (name.startsWith(q)) return 4;
  if (name.includes(q)) return 3;
  return words.every((w) => name.includes(w)) ? 2 : 1;
}

function oldestAttention(ws: WorkspaceView): string | undefined {
  return ws.attention.map((a) => a.createdAt).sort()[0];
}

export function workspaceGlyph(ws: WorkspaceView): Glyph {
  switch (ws.activity) {
    case "needs_you":
      return "needs";
    case "failed":
      return "failed";
    case "working":
    case "preparing":
      return "working";
    case "completed":
      return "done";
    default:
      return "idle";
  }
}

export function sessionGlyph(s: SessionView): Glyph {
  if (s.attention?.needsYou) return "needs";
  if (s.attention?.kind === "failure" || s.status === "failed" || s.status === "lost") return "failed";
  if (s.status === "running") {
    if (s.agent) return s.agent.state === "working" || s.agent.state === "starting" ? "working" : "idle";
    if (s.kind === "service" || s.kind === "command") return "working";
    return "idle";
  }
  if (s.status === "completed" || s.attention?.kind === "completion") return "done";
  return "idle";
}

/** One line about a workspace for the sidebar, and since when. */
export function headline(ws: WorkspaceView): { text: string; since?: string } {
  switch (ws.activity) {
    case "needs_you":
    case "failed":
    case "completed": {
      const latest = [...ws.attention].sort((a, b) => b.createdAt.localeCompare(a.createdAt))[0];
      if (latest) {
        const more = ws.attention.length - 1;
        return { text: latest.summary + (more > 0 ? ` (+${more})` : ""), since: latest.createdAt };
      }
      return { text: ws.stateMessage ?? ws.state };
    }
    case "preparing":
      return { text: ws.stateMessage ?? "preparing" };
    case "archived":
      return { text: ws.sourceKind === "git" ? ws.source : ws.root, since: ws.updatedAt };
    case "working": {
      const busy = ws.sessions.filter((s) => s.status === "running" && sessionGlyph(s) === "working");
      const first = busy.find((s) => s.agent);
      return {
        text: busy.map((s) => `${s.name} ${s.agent ? "working" : "running"}`).join(" · "),
        since: first?.agent?.since,
      };
    }
    default:
      return {
        text: ws.sessions.length ? ws.sessions.map((s) => s.name).join(" · ") : "no sessions",
        since: ws.updatedAt,
      };
  }
}

/** The session to show when a workspace is opened: whatever needs you first. */
export function defaultSession(ws: WorkspaceView): string | undefined {
  const pick =
    ws.sessions.find((s) => s.attention?.needsYou) ??
    ws.sessions.find((s) => sessionGlyph(s) === "failed") ??
    ws.sessions.find((s) => s.kind === "agent") ??
    ws.sessions[0];
  return pick?.id;
}

/** `45s`, `12m`, `3h`, `2d`. */
export function ago(iso: string | undefined, now: number): string {
  if (!iso) return "";
  const secs = Math.max(0, Math.floor((now - Date.parse(iso)) / 1000));
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}

/** A hint next to the session name: the agent, or the kind of process. */
export function kindLabel(s: SessionView): string {
  if (s.agent) {
    return ({ codex: "Codex", claude: "Claude Code" } as Record<string, string>)[s.agent.provider] ?? s.agent.provider;
  }
  return s.name === s.kind ? "" : s.kind;
}

// Pins (D-040): `host/workspace-id` keys, top first, kept on this Mac. A pin
// whose workspace isn't loaded (its host still connecting) keeps its place.

/** The pinned workspaces that are loaded, in pin order. */
export function pinnedOf(placed: Placed[], pins: string[]): Placed[] {
  return pins.flatMap((k) => placed.find((p) => p.key === k) ?? []);
}

export function togglePin(pins: string[], key: string): string[] {
  return pins.includes(key) ? pins.filter((k) => k !== key) : [...pins, key];
}

/** `ids` with `id` put at `to` among the others (past the end: last). */
export function moveTo(ids: string[], id: string, to: number): string[] {
  const order = ids.filter((k) => k !== id);
  order.splice(Math.max(0, Math.min(to, order.length)), 0, id);
  return order;
}

/** Move a pinned workspace to `to` among the loaded ones (`shown`, in order). */
export function movePin(pins: string[], shown: string[], key: string, to: number): string[] {
  const next = moveTo(shown, key, to)[Symbol.iterator]();
  const visible = new Set(shown);
  return pins.map((k) => (visible.has(k) ? next.next().value! : k));
}

/** Pins of workspaces gone from a connected host (deleted elsewhere). */
export function stalePins(pins: string[], hosts: HostsPayload["hosts"]): string[] {
  return pins.filter((k) => {
    const slash = k.indexOf("/");
    const host = hosts.find((h) => h.name === k.slice(0, slash));
    return host?.status === "connected" && !host.workspaces.some((w) => w.id === k.slice(slash + 1));
  });
}

/** Longest line in the menu bar: one long line widens the whole menu; the app has the rest. */
const TRAY_LINE = 44;
/** Longest name on a menu bar line (task titles are sentences). */
const TRAY_NAME = 24;

function cut(t: string, max: number): string {
  return t.length > max ? `${t.slice(0, max - 1).trimEnd()}…` : t;
}

/**
 * What needs you, at a glance: "needs your approval" becomes "Approve", paths
 * become their last part, a finished turn says only that it finished, and
 * it's cut to `max`, keeping a trailing "(+N)".
 *   "claude needs your approval: Read: /home/a/b/createOffer.html"
 *   → "Approve: Read createOffer.html"
 *   "claude-2 finished: Yes, it would exceed the request… (+1)"
 *   → "claude-2 finished (+1)"
 */
export function glance(summary: string, max = TRAY_LINE): string {
  const more = / \(\+\d+\)$/.exec(summary)?.[0] ?? "";
  const t = summary
    .slice(0, summary.length - more.length)
    .replace(/^.*?\bneeds your approval:\s*/i, "Approve: ")
    .replace(/^(Approve: \w+):\s+/, "$1 ")
    // A finished turn: the reply is the app's to show.
    .replace(/^(\S+ finished):[\s\S]*$/, "$1")
    // Paths: only their last part.
    .replace(/(?:~|\.{1,2})?\/?(?:[\w.@+-]+\/)+([\w.@+-]+)/g, "$1")
    .replace(/\s+/g, " ")
    .trim();
  return cut(t, max - more.length) + more;
}

/** Something waiting, for the menu bar: a workspace or a task. */
export interface TrayLine {
  key: string;
  name: string;
  what: string;
  host: string;
}

/**
 * The menu bar's lines, each cut to fit: the name, its host only when a line
 * on another host has the same name, and what waits.
 *   "marketplace · claude-2 finished", "api (box) · Approve: Bash make"
 */
export function trayLabels(lines: TrayLine[]): { key: string; label: string }[] {
  const hosts = new Map<string, Set<string>>();
  for (const l of lines) hosts.set(l.name, (hosts.get(l.name) ?? new Set()).add(l.host));
  return lines.map((l) => {
    let name = cut(l.name, TRAY_NAME);
    if ((hosts.get(l.name)?.size ?? 0) > 1) name += ` (${l.host})`;
    return { key: l.key, label: `${name} · ${glance(l.what, Math.max(TRAY_LINE - name.length - 3, 16))}` };
  });
}

/** Where a box was laid out (a `DOMRect`'s). */
export interface Box {
  left: number;
  top: number;
  width: number;
  bottom: number;
}

/**
 * The place a tab dragged to (x, y) goes among the others (D-150): how many
 * of them, as laid out when the drag started, come before the pointer in
 * reading order, on a row above it or on its row with their middle to its
 * left. Tabs wrap onto more rows; above the first or below the last counts
 * as on it.
 */
export function dropPlace(others: Box[], x: number, y: number): number {
  if (others.length === 0) return 0;
  const top = Math.min(...others.map((b) => b.top));
  const bottom = Math.max(...others.map((b) => b.bottom));
  const at = Math.min(Math.max(y, top), bottom - 1);
  return others.filter((b) => b.bottom <= at || (b.top <= at && b.left + b.width / 2 < x)).length;
}

/** A session's name on its tab: a long one (an agent's title, D-196) cut at
 * 40 characters, the whole of it in the tab's tooltip. */
export function tabName(name: string): string {
  const chars = [...name];
  return chars.length > TAB_NAME ? `${chars.slice(0, TAB_NAME - 1).join("").trimEnd()}…` : name;
}
const TAB_NAME = 40;
