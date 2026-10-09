// Presentation rules: grouping, ordering and wording. Classification itself
// (activity, session status) comes computed from otter-core via the backend.

import type { Activity, Glyph, HostsPayload, Placed, SessionView, WorkspaceView } from "./types";

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
    // Longest-waiting first, as in `otter ls`.
    return (oldestAttention(a.ws) ?? "").localeCompare(oldestAttention(b.ws) ?? "") || a.ws.name.localeCompare(b.ws.name);
  });
  for (const p of sorted) out.find((g) => g.id === GROUP_OF[p.ws.activity])!.items.push(p);
  return out.filter((g) => g.items.length > 0);
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
    if (s.kind === "service" || s.kind === "task") return "working";
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

/** Move a pinned workspace to `to` among the loaded ones (`shown`, in order). */
export function movePin(pins: string[], shown: string[], key: string, to: number): string[] {
  const order = shown.filter((k) => k !== key);
  order.splice(Math.max(0, Math.min(to, order.length)), 0, key);
  const next = order[Symbol.iterator]();
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

/** Longest glance in the menu bar; the app has the rest. */
const GLANCE = 52;

/**
 * What needs you, at a glance: "needs your approval" becomes "Approve", paths
 * become their last part, and it's cut to fit a menu.
 *   "claude needs your approval: Read: /home/a/b/createOffer.html"
 *   → "Approve: Read createOffer.html"
 */
export function glance(summary: string): string {
  let t = summary
    .replace(/^.*?\bneeds your approval:\s*/i, "Approve: ")
    .replace(/^(Approve: \w+):\s+/, "$1 ")
    // Paths: only their last part.
    .replace(/(?:~|\.{1,2})?\/?(?:[\w.@+-]+\/)+([\w.@+-]+)/g, "$1")
    .replace(/\s+/g, " ")
    .trim();
  if (t.length > GLANCE) t = `${t.slice(0, GLANCE - 1).trimEnd()}…`;
  return t;
}

/** A menu bar line for a workspace that needs you. */
export function trayLabel(name: string, summary: string, host: string, showHost: boolean): string {
  return `${name} · ${glance(summary)}${showHost ? `  (${host})` : ""}`;
}
