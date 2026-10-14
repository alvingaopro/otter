// Test data shared by the Inbox's tests.
import type { Activity, HostView, Placed, WorkspaceView } from "./types";

const NOW = Date.parse("2026-10-09T12:00:00Z");
const iso = (minutesAgo: number) => new Date(NOW - minutesAgo * 60_000).toISOString();

export function workspace(name: string, activity: Activity, minutesAgo: number, host = "mac"): Placed {
  const ws: WorkspaceView = {
    id: `ws_${name}`,
    name,
    activity,
    state: "ready",
    root: `/w/${name}`,
    brief: {},
    sourceKind: "directory",
    source: `/w/${name}`,
    sessions: [],
    attention:
      activity === "needs_you"
        ? [{ id: `att_${name}`, kind: "approval", needsYou: true, summary: "codex needs your approval: Bash: make deploy", createdAt: iso(minutesAgo) }]
        : [],
    updatedAt: iso(minutesAgo),
  };
  const h: HostView = { name: host, describe: "this Mac", status: "connected", agents: [], workspaces: [ws] };
  return { host: h, ws, key: `${host}/${ws.id}` };
}
