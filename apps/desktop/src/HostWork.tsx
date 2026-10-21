import { Fragment, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { bytes } from "./format";
import { agentRows, FLAG_LABEL, KINDS, processRows, sessionRows, span, strays, usageOf, type Flag, type Kind, type Usage } from "./workRows";
import { ago } from "./model";
import { explain } from "./newer";
import { STATUS_LABEL, type Task, type TaskAction, type TaskStatus } from "./tasks";
import type { HostView, OtterProcess } from "./types";

/** What the Work tab knows of the host's processes: asked, not there yet, or not served by this otterd. */
export type Procs = { state: "loading" } | { state: "ready"; procs: OtterProcess[] } | { state: "too_old" } | { state: "error"; error: string };

interface Act {
  label: string;
  /** Asked again in the row, with what it does. */
  ask?: string;
  danger?: boolean;
  run: () => Promise<unknown>;
}
interface Row {
  id: string;
  cells: ReactNode[];
  flags: Flag[];
  acts: Act[];
  /** What opens under the row: who owns it, and its processes. */
  owner: [string, string][];
  procs: OtterProcess[];
}

const COLUMNS: Record<Kind, string[]> = {
  tasks: ["Task", "Status", "Updated"],
  workspaces: ["Workspace", "State", "Sessions", "Updated"],
  sessions: ["Session", "Status", "PID", "CPU", "Memory"],
  agents: ["Agent", "State", "PID", "CPU", "Memory"],
  processes: ["Process", "Role", "PID", "CPU", "Memory", "Started"],
};

const pct = (n: number) => `${n >= 10 ? Math.round(n) : n.toFixed(1)}%`;
const cell = (main: string, sub?: string) => (
  <div className="work-name">
    <div>{main}</div>
    {sub && <div className="work-sub">{sub}</div>}
  </div>
);
const usageCells = (u: Usage) => [
  <span className="mono" key="pid">{u.root?.pid ?? "—"}</span>,
  <span className="mono" key="cpu">{u.count ? pct(u.cpu) : "—"}</span>,
  <span className="mono" key="mem">{u.count ? bytes(u.memory) : "—"}</span>,
];

/** The process tree, one line per process, indented under its parent. */
export function treeText(procs: OtterProcess[]): string {
  const pids = new Set(procs.map((p) => p.pid));
  const kids = new Map<number | undefined, OtterProcess[]>();
  for (const p of procs) {
    const at = p.parent !== undefined && pids.has(p.parent) ? p.parent : undefined;
    kids.set(at, [...(kids.get(at) ?? []), p]);
  }
  const lines: string[] = [];
  const walk = (parent: number | undefined, depth: number) => {
    for (const p of kids.get(parent) ?? []) {
      lines.push(`${"  ".repeat(depth)}${p.name}  ${p.pid}   ${pct(p.cpu_percent)}  ${bytes(p.memory)}`);
      walk(p.pid, depth + 1);
    }
  };
  walk(undefined, 0);
  return lines.join("\n") || "Nothing is running for this row.";
}

/**
 * What Otter runs on a host, by kind (D-161): its tasks, workspaces,
 * sessions, coding agents, and the processes nothing else owns. A row opens
 * into who owns it and its processes; its actions are the commands that
 * exist, asked again in the row before they end anything.
 */
export function HostWork({
  host,
  tasks,
  procs,
  now,
  onTask,
  onOpenWorkspace,
}: {
  host: HostView;
  tasks: Task[];
  procs: Procs;
  now: number;
  onTask: (task: string, action: TaskAction) => Promise<unknown>;
  onOpenWorkspace?: (workspace: string, session?: string) => void;
}) {
  const [kind, setKind] = useState<Kind>("sessions");
  const [onlyFlagged, setOnlyFlagged] = useState(false);
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const [asking, setAsking] = useState<{ row: string; act: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const list = procs.state === "ready" ? procs.procs : [];
  const ws = host.workspaces;
  const wsName = (id?: string) => ws.find((w) => w.id === id)?.name ?? id ?? "none";
  const session = (workspace: string, id: string) => ({ host: host.name, workspace, session: id });
  const stop = (p: OtterProcess, force: boolean): Act => ({
    label: force ? "Force stop" : "Stop",
    ask: force ? "Ends it at once (SIGKILL)." : "Asks it to end (SIGTERM).",
    danger: true,
    run: () => invoke("host_process_stop", { host: host.name, pid: p.pid, started: p.started, force }),
  });

  const rows: Record<Kind, Row[]> = {
    tasks: tasks.map((t) => {
      const act = (label: string, action: TaskAction, danger?: boolean, ask?: string): Act => ({ label, danger, ask, run: () => onTask(t.id, action) });
      const acts: Act[] = [];
      const s: TaskStatus = t.status;
      if (s === "draft") acts.push(act("Start", { action: "start" }));
      if (s === "paused") acts.push(act("Resume", { action: "resume" }));
      if (s === "failed") acts.push(act("Retry", { action: "retry" }));
      if (["planning", "implementing", "verifying"].includes(s)) acts.push(act("Pause", { action: "pause" }));
      if (!["done", "cancelled", "draft"].includes(s)) acts.push(act("Cancel", { action: "cancel" }, true, "Ends its run and the task."));
      return {
        id: t.id,
        cells: [cell(t.title, `workspace ${wsName(t.workspace_id)}`), <Pill key="s" tone={tone(s)}>{STATUS_LABEL[s]}</Pill>, ago(t.updated_at, now)],
        flags: [],
        acts,
        owner: [["Task", t.title], ["Workspace", wsName(t.workspace_id)], ["Status", t.status_reason ?? STATUS_LABEL[s]]],
        procs: list.filter((p) => p.owner.kind === "run" && p.owner.task_id === t.id),
      };
    }),
    workspaces: ws.map((w) => {
      const sessions = sessionRows([w], list);
      const mine = sessions.flatMap((s) => s.procs);
      const running = w.sessions.filter((s) => s.status === "running").length;
      const flags: Flag[] = w.state === "archived" && mine.length > 0 ? ["stale"] : [];
      const acts: Act[] = [];
      if (w.state === "ready" || w.state === "failed") acts.push({ label: "Prepare again", run: () => invoke("workspace_prepare", { host: host.name, workspace: w.id }) });
      if (w.state === "ready" || w.state === "failed")
        acts.push({ label: "Archive", ask: "Stops its sessions, keeps its files.", danger: true, run: () => invoke("workspace_archive", { host: host.name, workspace: w.id }) });
      acts.push({
        label: "Delete…",
        ask: w.sourceKind === "git" ? "Removes it and its worktree; refused while work isn't pushed." : "Removes it from this host.",
        danger: true,
        run: () => invoke("workspace_delete", { host: host.name, workspace: w.id, force: false }),
      });
      return {
        id: w.id,
        cells: [cell(w.name, w.sourceKind === "git" ? `git · ${w.source || "branch not known yet"}` : w.sourceKind), <Pill key="s" tone={w.state === "failed" ? "bad" : w.state === "archived" ? "warn" : "ok"}>{w.state}</Pill>, `${running} running of ${w.sessions.length}`, ago(w.updatedAt, now)],
        flags,
        acts,
        owner: [["Workspace", w.name], ["Where", w.root], ["Task", w.taskId ?? "none"]],
        procs: mine,
      };
    }),
    sessions: sessionRows(ws, list).map((r) => {
      const { session: s, workspace: w } = r;
      const a = session(w.id, s.id);
      return {
        id: `${w.id}/${s.id}`,
        cells: [cell(s.name, w.name), <Pill key="s" tone={s.status === "running" ? "run" : s.status === "failed" ? "bad" : ""}>{s.status}</Pill>, ...usageCells(r.usage)],
        flags: r.flags,
        acts: [
          { label: "Open", run: async () => onOpenWorkspace?.(w.id, s.id) },
          ...(s.status === "running" ? [{ label: "Stop", ask: "Ends its process; the session stays.", danger: true, run: () => invoke("session_stop", a) }] : []),
          { label: "Restart", run: () => invoke("session_restart", a) },
          { label: "Delete", ask: "Removes the session.", danger: true, run: () => invoke("session_delete", a) },
        ],
        owner: [["Session", s.name], ["Workspace", w.name], ["Process", r.usage.root ? `pid ${r.usage.root.pid}` : "not running"]],
        procs: r.procs,
      };
    }),
    agents: agentRows(ws, tasks, list).map((r) => ({
      id: r.key,
      cells: [cell(r.name, r.owner), <Pill key="s" tone="run">{r.state}</Pill>, ...usageCells(r.usage)],
      flags: r.flags,
      acts:
        r.kind === "run" && r.taskId
          ? [
              { label: "Interrupt", run: () => onTask(r.taskId!, { action: "interrupt" }) },
              { label: "Cancel task", ask: "Ends its run and the task.", danger: true, run: () => onTask(r.taskId!, { action: "cancel" }) },
            ]
          : r.workspace && r.session
            ? [{ label: "Stop", ask: "Ends the agent's process; the session stays.", danger: true, run: () => invoke("session_stop", session(r.workspace!.id, r.session!.id)) }]
            : [],
      owner: [["Agent", r.name], ["Owner", r.owner]],
      procs: [],
    })),
    processes: processRows(list).map(({ proc: p, flags }) => ({
      id: String(p.pid),
      cells: [
        cell(p.name, p.exe ?? p.cwd),
        <Pill key="r" tone={p.owner.kind === "orphan" ? "bad" : p.owner.kind === "daemon" && p.owner.this ? "ok" : "warn"}>{p.owner.kind === "orphan" ? "No owner" : p.owner.kind === "daemon" && p.owner.this ? "This otterd" : "Another otterd"}</Pill>,
        <span className="mono" key="p">{p.pid}</span>,
        <span className="mono" key="c">{pct(p.cpu_percent)}</span>,
        <span className="mono" key="m">{bytes(p.memory)}</span>,
        ago(new Date(p.started * 1000).toISOString(), now),
      ],
      flags,
      acts: p.stoppable ? [stop(p, false), stop(p, true)] : [],
      owner: [["Process", `${p.name} · pid ${p.pid}`], ["Where", p.cwd ?? p.exe ?? "unknown"], ["Why", p.owner.kind === "orphan" ? p.owner.why : p.owner.kind === "daemon" && !p.owner.this ? "An otterd that isn't the one this app talks to. Stopping it leaves its sessions running." : "This otterd"]],
      procs: [p],
    })),
  };

  const stray = strays(list);
  const shown = rows[kind].filter((r) => (!onlyFlagged || r.flags.length > 0) && (!query || JSON.stringify(r.owner).toLowerCase().includes(query.toLowerCase())));
  const flagged = (k: Kind) => rows[k].filter((r) => r.flags.length > 0).length;

  async function run(a: Act) {
    setBusy(true);
    setError(null);
    try {
      await a.run();
      setAsking(null);
    } catch (e) {
      setError(explain(host.name, e));
    } finally {
      setBusy(false);
    }
  }
  const actions = (r: Row, only?: Act[]) =>
    (only ?? r.acts).map((a) =>
      asking?.row === r.id && asking.act === a.label ? (
        <span key={a.label} className="work-ask">
          <span className="muted">{a.ask}</span>
          <button className="btn danger" disabled={busy} onClick={() => void run(a)}>{a.label.replace("…", "")}</button>
          <button className="btn outline" onClick={() => setAsking(null)}>Keep</button>
        </span>
      ) : (
        <button
          key={a.label}
          className={`btn ${a.danger ? "ghost danger-text" : "outline"}`}
          disabled={busy}
          onClick={(e) => {
            e.stopPropagation();
            if (a.ask) setAsking({ row: r.id, act: a.label });
            else void run(a);
          }}
        >
          {a.label}
        </button>
      ),
    );

  return (
    <div className="host-work">
      {procs.state === "too_old" && <p className="notice">{host.name}'s otterd is too old to list its processes: update it from the sidebar. Tasks, workspaces and sessions below still work.</p>}
      {procs.state === "error" && <p className="notice error">{procs.error}</p>}
      {error && <p className="notice error">{error}</p>}

      {stray.length > 0 && (
        <div className="work-strays" role="region" aria-label="Needs a look">
          <div className="work-strays-head">
            <strong>{stray.length} process{stray.length === 1 ? "" : "es"} nothing here owns</strong>
            <span className="muted">another otterd, or left over from a workspace that is gone</span>
          </div>
          <ul className="plain-list">
            {stray.map((p) => {
              const row = rows.processes.find((r) => r.id === String(p.pid))!;
              return (
                <li key={p.pid} className="work-stray">
                  <div>
                    <span className="work-name">{p.name}</span> <span className="mono muted">pid {p.pid}</span>
                    <div className="work-sub">
                      {p.exe ?? p.cwd ?? ""} · up {span(Math.max(0, now / 1000 - p.started))} · {pct(p.cpu_percent)}
                      {p.busy_secs >= 60 ? ` · busy ${span(p.busy_secs)}` : ""}
                    </div>
                  </div>
                  <div className="work-acts">{actions(row)}</div>
                </li>
              );
            })}
          </ul>
        </div>
      )}

      <div className="work-tabs" role="tablist" aria-label="Kind">
        {KINDS.map((k) => (
          <button key={k.id} role="tab" aria-selected={kind === k.id} className="work-tab" onClick={() => { setKind(k.id); setOpen(null); setAsking(null); }}>
            {k.label}
            <span className="work-count">{rows[k.id].length}</span>
            {flagged(k.id) > 0 && <span className="work-count flag">{flagged(k.id)}</span>}
          </button>
        ))}
      </div>
      <div className="work-tools">
        <button className={`filter ${onlyFlagged ? "on" : ""}`} aria-pressed={onlyFlagged} onClick={() => setOnlyFlagged(!onlyFlagged)}>Needs a look</button>
        <input className="work-search" type="search" aria-label="Filter" placeholder={`Filter ${kind}`} value={query} onChange={(e) => setQuery(e.target.value)} />
      </div>

      <div className="work-table-wrap">
        <table className="work-table">
          <thead>
            <tr>{COLUMNS[kind].map((c) => <th key={c}>{c}</th>)}<th /></tr>
          </thead>
          <tbody>
            {shown.length === 0 && (
              <tr><td className="muted work-empty" colSpan={COLUMNS[kind].length + 1}>{onlyFlagged ? `No ${kind} need a look.` : `No ${kind} on ${host.name}.`}</td></tr>
            )}
            {shown.map((r) => (
              <Fragment key={r.id}>
                <tr className={`work-row ${open === r.id ? "open" : ""}`} tabIndex={0} aria-expanded={open === r.id} onClick={() => setOpen(open === r.id ? null : r.id)} onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), setOpen(open === r.id ? null : r.id))}>
                  {r.cells.map((c, i) => (
                    <td key={i}>
                      {c}
                      {i === 0 && r.flags.length > 0 && (
                        <div className="work-flags">{r.flags.map((f) => <span key={f} className={`work-flag ${f}`}>{FLAG_LABEL[f]}{f === "busy" ? ` ${span(r.procs.reduce((a, p) => Math.max(a, p.busy_secs), 0))}` : ""}</span>)}</div>
                      )}
                    </td>
                  ))}
                  <td className="work-acts" onClick={(e) => e.stopPropagation()}>{open === r.id ? null : actions(r, r.acts.slice(0, 2).concat(r.acts.filter((a, i) => i >= 2 && asking?.row === r.id && asking.act === a.label)))}</td>
                </tr>
                {open === r.id && (
                  <tr className="work-detail">
                    <td colSpan={COLUMNS[kind].length + 1}>
                      <div className="work-detail-grid">
                        <div>
                          <h4>Owned by</h4>
                          <dl>{r.owner.map(([k, v]) => <div key={k}><dt>{k}</dt><dd>{v}</dd></div>)}</dl>
                        </div>
                        <div>
                          <h4>Processes</h4>
                          <pre className="work-tree">{treeText(r.procs)}</pre>
                        </div>
                      </div>
                      <div className="work-acts work-detail-acts">{actions(r)}</div>
                    </td>
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      </div>
      <p className="muted small work-note">
        Busy: 25% of a CPU or more for 10 minutes. No owner: a process in a workspace folder of this host whose workspace is gone or archived, with no session or run above it.
      </p>
    </div>
  );
}

function tone(s: TaskStatus): string {
  if (["planning", "implementing", "verifying"].includes(s)) return "run";
  if (s === "done" || s === "review") return "ok";
  if (s === "failed") return "bad";
  if (s === "blocked") return "warn";
  return "";
}

function Pill({ tone, children }: { tone: string; children: ReactNode }) {
  return (
    <span className={`work-pill ${tone}`}>
      <i />
      {children}
    </span>
  );
}

export { usageOf };
