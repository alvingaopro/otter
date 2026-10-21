// The Work tab of a host page (D-161): what Otter runs there, by kind, with
// each row's owner and the real processes under it. Pure: the rows are built
// from the snapshot, the tasks and `host.processes`, so they can be tested.

import type { Task } from "./tasks";
import type { OtterProcess, SessionView, WorkspaceView } from "./types";

export type Kind = "tasks" | "workspaces" | "sessions" | "agents" | "processes";
export const KINDS: { id: Kind; label: string }[] = [
  { id: "tasks", label: "Tasks" },
  { id: "workspaces", label: "Workspaces" },
  { id: "sessions", label: "Sessions" },
  { id: "agents", label: "Agents" },
  { id: "processes", label: "Processes" },
];

/** Busy this long (25% of a CPU or more) is worth a flag. */
export const BUSY_FLAG_SECS = 600;

/**
 * Another otterd younger than this isn't a stray yet: the end-to-end tests of
 * a task's work start one per test, for seconds. It is still listed.
 */
export const STRAY_AFTER_SECS = 120;

/** An otterd that isn't this one and has been running long enough to be looked at. */
function lingering(p: OtterProcess, nowMs: number): boolean {
  return p.owner.kind !== "daemon" || nowMs / 1000 - p.started >= STRAY_AFTER_SECS;
}

export type Flag = "busy" | "orphan" | "stale" | "other";

export interface Usage {
  cpu: number;
  memory: number;
  /** The longest any of its processes has been busy. */
  busy: number;
  count: number;
  /** The process the row stands for: the root of its tree. */
  root?: OtterProcess;
}

/** The processes of one owner, as one: CPU and memory summed, the tree's root found. */
export function usageOf(rows: OtterProcess[]): Usage {
  const pids = new Set(rows.map((p) => p.pid));
  const root = rows.find((p) => p.parent === undefined || !pids.has(p.parent));
  return {
    cpu: rows.reduce((a, p) => a + p.cpu_percent, 0),
    memory: rows.reduce((a, p) => a + p.memory, 0),
    busy: rows.reduce((a, p) => Math.max(a, p.busy_secs), 0),
    count: rows.length,
    root,
  };
}

export interface SessionRow {
  workspace: WorkspaceView;
  session: SessionView;
  procs: OtterProcess[];
  usage: Usage;
  flags: Flag[];
}

export function sessionRows(workspaces: WorkspaceView[], procs: OtterProcess[]): SessionRow[] {
  const bySession = new Map<string, OtterProcess[]>();
  for (const p of procs) {
    if (p.owner.kind !== "execution") continue;
    const k = `${p.owner.workspace_id}/${p.owner.session_id}`;
    bySession.set(k, [...(bySession.get(k) ?? []), p]);
  }
  return workspaces.flatMap((workspace) =>
    workspace.sessions.map((session) => {
      const mine = bySession.get(`${workspace.id}/${session.id}`) ?? [];
      const usage = usageOf(mine);
      const flags: Flag[] = [];
      if (usage.busy >= BUSY_FLAG_SECS) flags.push("busy");
      // Running in an archived workspace: put away, but not stopped.
      if (workspace.state === "archived" && mine.length > 0) flags.push("stale");
      return { workspace, session, procs: mine, usage, flags };
    }),
  );
}

export interface ProcessRow {
  proc: OtterProcess;
  flags: Flag[];
}

/** Other otterds, orphans, and this otterd: the rows nothing but the host owns. */
export function processRows(procs: OtterProcess[], nowMs: number): ProcessRow[] {
  return procs
    .filter((p) => p.owner.kind === "daemon" || p.owner.kind === "orphan")
    .map((proc) => {
      const flags: Flag[] = [];
      if (proc.owner.kind === "orphan") flags.push("orphan");
      if (proc.owner.kind === "daemon" && !proc.owner.this && lingering(proc, nowMs)) flags.push("other");
      if (proc.busy_secs >= BUSY_FLAG_SECS) flags.push("busy");
      return { proc, flags };
    })
    .sort((a, b) => Number(!!b.proc.stoppable) - Number(!!a.proc.stoppable) || a.proc.pid - b.proc.pid);
}

/** What the strip above the tabs lists, and the Work tab's badge counts. */
export function strays(procs: OtterProcess[], nowMs: number): OtterProcess[] {
  return procs.filter((p) => p.stoppable && lingering(p, nowMs));
}

export interface AgentRow {
  /** `run`: a task's coding agent; `session`: an agent in a terminal. */
  kind: "run" | "session";
  key: string;
  name: string;
  owner: string;
  state: string;
  taskId?: string;
  workspace?: WorkspaceView;
  session?: SessionView;
  usage: Usage;
  flags: Flag[];
}

export function agentRows(workspaces: WorkspaceView[], tasks: Task[], procs: OtterProcess[]): AgentRow[] {
  const rows: AgentRow[] = [];
  for (const s of sessionRows(workspaces, procs)) {
    if (!s.session.agent) continue;
    rows.push({
      kind: "session",
      key: `${s.workspace.id}/${s.session.id}`,
      name: s.session.agent.provider,
      owner: `${s.workspace.name} · ${s.session.name}`,
      state: s.session.agent.state.replace(/_/g, " "),
      workspace: s.workspace,
      session: s.session,
      usage: s.usage,
      flags: s.flags,
    });
  }
  const byTask = new Map<string, OtterProcess[]>();
  for (const p of procs) {
    if (p.owner.kind !== "run" || !p.owner.task_id) continue;
    byTask.set(p.owner.task_id, [...(byTask.get(p.owner.task_id) ?? []), p]);
  }
  for (const [taskId, mine] of byTask) {
    const task = tasks.find((t) => t.id === taskId);
    const usage = usageOf(mine);
    rows.push({
      kind: "run",
      key: `run/${taskId}`,
      name: "Coding agent",
      owner: task ? task.title : `task ${taskId}`,
      state: task ? task.status : "running",
      taskId,
      usage,
      flags: usage.busy >= BUSY_FLAG_SECS ? ["busy"] : [],
    });
  }
  return rows;
}

/** "14h", "5m", "40s". */
export function span(secs: number): string {
  if (secs >= 86400) return `${Math.floor(secs / 86400)}d`;
  if (secs >= 3600) return `${Math.floor(secs / 3600)}h`;
  if (secs >= 60) return `${Math.floor(secs / 60)}m`;
  return `${Math.floor(secs)}s`;
}

export const FLAG_LABEL: Record<Flag, string> = {
  busy: "Busy",
  orphan: "No owner",
  stale: "Archived, still running",
  other: "Another otterd",
};
