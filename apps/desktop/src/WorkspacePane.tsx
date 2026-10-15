import { useEffect, useRef, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Glyph } from "./Glyph";
import { Dialog } from "./Dialog";
import { Menu, type MenuItem } from "./Menu";
import { NewSessionDialog } from "./NewSessionDialog";
import { FilesPanel } from "./FilesPanel";
import { TimelinePanel } from "./TimelinePanel";
import { PanelGrip, SIDE_PANEL, usePanelWidth } from "./panelResize";
import { BriefDialog, briefSummary } from "./BriefDialog";
import { ChangesPanel, type Changes } from "./ChangesPanel";
import { changesLine } from "./changes";
import { FindPanel } from "./FindPanel";
import { ago, kindLabel, sessionGlyph } from "./model";
import { explain, invokeNewer } from "./newer";
import { RenameDialog } from "./RenameDialog";
import { Terminal, type Ending } from "./Terminal";
import type { AttentionView, Placed, SessionPort, SessionView, WorkspaceChanges } from "./types";

type Side = "files" | "timeline" | "changes";
const SIDE_KEY = "otter.side";

/** The side panel last open, for the next workspace shown (D-106). */
function storedSide(): Side | null {
  try {
    const v = localStorage.getItem(SIDE_KEY);
    return v === "files" || v === "timeline" || v === "changes" ? v : null;
  } catch {
    return null;
  }
}

/** How often the ports of a workspace in front are looked at again. */
const PORTS_EVERY_MS = 10_000;
/** How often an open Changes panel looks again (an agent's edits make no event). */
const CHANGES_EVERY_MS = 30_000;

interface Props {
  placed: Placed;
  session?: string;
  onSession: (id: string) => void;
  now: number;
  theme: "light" | "dark";
  /** This app's version: a host on another otterd version is offered an update. */
  appVersion?: string;
  /** Pin, unpin, move and rename items for the workspace menu. */
  pinItems: MenuItem[];
  /** The workspace shown in the Workspaces view (D-100): kept panes are mounted but hidden. */
  shown: boolean;
  /** Shown, and that view in front: only then does it look at its host or take keys. */
  active: boolean;
  /** Open a port of this host in the browser (forwarded if remote). */
  onOpenPort: (port: number) => void;
  /** Show the task this workspace was made for (D-105). */
  onOpenTask: (taskId: string) => void;
}

type Open = "brief" | "new-session" | "archive-workspace" | "delete-workspace" | "delete-session" | "rename-session" | null;

export function WorkspacePane({
  placed,
  session,
  onSession,
  now,
  theme,
  appVersion,
  pinItems,
  shown,
  active,
  onOpenPort,
  onOpenTask,
}: Props) {
  const { host, ws } = placed;
  const current = ws.sessions.find((s) => s.id === session) ?? ws.sessions[0];
  const [ending, setEnding] = useState<Ending | null>(null);
  const [generation, setGeneration] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<Open>(null);
  const [force, setForce] = useState(false);
  const [closing, setClosing] = useState<string | null>(null);
  // The side panel: files, the timeline or the changes; the one last open
  // opens with the next workspace too.
  const [side, setSideState] = useState<Side | null>(storedSide);
  const setSide = (next: Side | null) => {
    setSideState(next);
    try {
      if (next) localStorage.setItem(SIDE_KEY, next);
      else localStorage.removeItem(SIDE_KEY);
    } catch {
      // Not remembered; fine.
    }
  };
  const [sideWidth, setSideWidth] = usePanelWidth(SIDE_PANEL);
  const toggle = (panel: Side) => setSide(side === panel ? null : panel);
  const [finding, setFinding] = useState(false);
  const [renaming, setRenaming] = useState<SessionView | null>(null);

  // Its Git work (D-102): looked at when it's in front and something
  // happened in it, and now and then while the Changes panel is open.
  const [changes, setChanges] = useState<Changes>({ state: "loading" });
  const [changesTick, setChangesTick] = useState(0);
  const moved = `${ws.updatedAt}|${ws.sessions.map((s) => `${s.status}:${s.agent?.state ?? ""}:${s.agent?.since ?? ""}`).join(",")}`;
  useEffect(() => {
    if (!active || host.status !== "connected") return;
    let live = true;
    const t = setTimeout(() => {
      invokeNewer<WorkspaceChanges>(host.name, "workspace_changes", { host: host.name, workspace: ws.id })
        .then((c) => live && setChanges(c ? { state: "ready", changes: c } : { state: "too_old" }))
        .catch((e) => live && setChanges({ state: "error", error: String(e) }));
    }, 300);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [active, host.name, host.status, ws.id, moved, changesTick]);
  useEffect(() => {
    if (!active || side !== "changes") return;
    const t = setInterval(() => document.hasFocus() && setChangesTick((n) => n + 1), CHANGES_EVERY_MS);
    return () => clearInterval(t);
  }, [active, side]);

  // Ports its sessions listen on (D-104), while it's in front and runs something.
  const [ports, setPorts] = useState<SessionPort[]>([]);
  const runningAny = ws.sessions.some((s) => s.status === "running");
  useEffect(() => {
    if (!active || host.status !== "connected" || !runningAny) {
      setPorts([]);
      return;
    }
    let live = true;
    const look = () =>
      invokeNewer<SessionPort[]>(host.name, "workspace_ports", { host: host.name, workspace: ws.id })
        .then((p) => live && setPorts(p ?? []))
        .catch(() => live && setPorts([]));
    void look();
    const t = setInterval(() => document.hasFocus() && void look(), PORTS_EVERY_MS);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [active, host.name, host.status, ws.id, runningAny]);
  const shownPorts = [...new Map(ports.map((p) => [p.port, p])).values()].sort((a, b) => a.port - b.port);

  // A new session, or a new execution of it, starts a fresh attach.
  const running = current?.status === "running";
  useEffect(() => {
    setEnding(null);
    setError(null);
  }, [placed.key, current?.id]);
  useEffect(() => {
    if (running && ending && ending.kind !== "error") {
      setEnding(null);
      setGeneration((g) => g + 1);
    }
  }, [running]);

  // Lost connection: try again while the host is up.
  useEffect(() => {
    if (ending?.kind !== "lost" || host.status !== "connected") return;
    const t = setTimeout(() => {
      setEnding(null);
      setGeneration((g) => g + 1);
    }, 2000);
    return () => clearTimeout(t);
  }, [ending, host.status]);

  // A session that isn't running, however it got there (also when we never
  // watched it end): the last screen stays visible, dimmed.
  const over: string | null = !current
    ? null
    : current.status === "completed" || current.status === "failed"
      ? `${current.name} exited${current.exitCode !== undefined ? ` with status ${current.exitCode}` : ""}.`
      : current.status === "stopped"
        ? `${current.name} was stopped.`
        : current.status === "lost"
          ? `${current.name}'s process is gone.`
          : current.status === "pending" && current.launchError
            ? `${current.name} failed to start: ${current.launchError}`
            : null;

  const attention: AttentionView | undefined =
    current?.attention ?? ws.attention.find((a) => !a.sessionId);
  const failed = current && (current.status === "failed" || attention?.kind === "failure");
  // Archived (D-036): sessions are stopped, files kept, until unarchived.
  const archived = ws.state === "archived";
  const brief = briefSummary(ws.brief);
  const unarchive = () => act("workspace_unarchive", { host: host.name, workspace: ws.id });

  // Shown again after another workspace or a host's page (D-099, D-100):
  // its terminal didn't attach again, so what attaching settles — a finished
  // turn, a completed command — is settled here, item by item, for the
  // session on screen. Switching views never did, and doesn't.
  const wasShown = useRef(shown);
  useEffect(() => {
    if (shown && !wasShown.current && current) {
      for (const a of ws.attention) {
        if (a.sessionId === current.id && (a.kind === "review" || a.kind === "completion")) {
          void invoke("attention_resolve", { host: host.name, workspace: ws.id, session: null, attention: a.id }).catch(() => {});
        }
      }
    }
    wasShown.current = shown;
  }, [shown]);

  // Its shortcuts, while it's in front and no dialog is open (D-106):
  // ⌘T a new session, ⌘⇧[ / ⌘⇧] the session before / after, ⌘F find.
  const sessionIds = ws.sessions.map((s) => s.id).join(",");
  useEffect(() => {
    if (!active || open !== null) return;
    const onKey = (e: KeyboardEvent) => {
      if (!e.metaKey || e.ctrlKey || e.altKey) return;
      // Not under another dialog (⌘K's, say).
      if (document.querySelector('[aria-modal="true"]')) return;
      const key = e.key.toLowerCase();
      if (!e.shiftKey && key === "t" && !archived) {
        e.preventDefault();
        e.stopPropagation();
        setOpen("new-session");
      } else if (!e.shiftKey && key === "f" && current) {
        e.preventDefault();
        e.stopPropagation();
        setFinding((f) => !f);
      } else if (e.shiftKey && (e.code === "BracketLeft" || e.code === "BracketRight") && current) {
        e.preventDefault();
        e.stopPropagation();
        const ids = ws.sessions.map((s) => s.id);
        const at = ids.indexOf(current.id);
        const next = ids[(at + (e.code === "BracketRight" ? 1 : ids.length - 1)) % ids.length];
        if (next) onSession(next);
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [active, open, archived, current?.id, sessionIds]);
  useEffect(() => setFinding(false), [current?.id]);

  async function act(command: string, args: Record<string, unknown>): Promise<boolean> {
    setBusy(true);
    setError(null);
    try {
      await invoke(command, args);
      return true;
    } catch (e) {
      setError(String(e));
      return false;
    } finally {
      setBusy(false);
    }
  }
  const sessionArgs = () => ({ host: host.name, workspace: ws.id, session: current?.id });

  const resolve = () =>
    act("attention_resolve", { host: host.name, workspace: ws.id, session: attention?.sessionId ?? null });
  const restart = () => current && act("session_restart", { host: host.name, workspace: ws.id, session: current.id });

  return (
    <main className="pane ws-pane" hidden={!shown}>
      <div className="pane-head" data-tauri-drag-region>
        <h1 data-tauri-drag-region>{ws.name}</h1>
        <span className="head-sep" aria-hidden="true" />
        <div className="chips" data-tauri-drag-region>
          <span className="chip mono">
            <span className={`host-dot ${host.status}`} />
            {host.name}
          </span>
          {ws.sourceKind === "git" && <span className="chip mono">{ws.source}</span>}
          {ws.sourceKind === "directory" && <span className="chip">directory</span>}
          {ws.sourceKind === "empty" && <span className="chip">scratch</span>}
          {changes.state === "ready" && changesLine(changes.changes) && (
            <button
              className="chip mono changes-chip"
              title={`What changed${changes.changes.base ? ` since ${changes.changes.base}` : " and isn't committed"}: show the files`}
              onClick={() => setSide("changes")}
            >
              {changesLine(changes.changes)}
            </button>
          )}
          {ws.taskId && (
            <button className="chip changes-chip" title="The task this workspace was made for" onClick={() => onOpenTask(ws.taskId!)}>
              Task ↗
            </button>
          )}
          <span className="root mono">{ws.root}</span>
        </div>
        {brief.text && (
          <button className="brief-line" onClick={() => setOpen("brief")} title={`${brief.text}\n\nEdit the brief`}>
            <span className="brief-text">{brief.text}</span>
            {brief.more > 0 && <span className="brief-more">+{brief.more} more</span>}
          </button>
        )}
        <span className="spacer" data-tauri-drag-region />
        <button
          className={side === "changes" ? "btn outline small-btn files-toggle on" : "btn outline small-btn files-toggle"}
          aria-pressed={side === "changes"}
          onClick={() => toggle("changes")}
          title="What changed in its Git work"
        >
          Changes
        </button>
        <button
          className={side === "timeline" ? "btn outline small-btn files-toggle on" : "btn outline small-btn files-toggle"}
          aria-pressed={side === "timeline"}
          onClick={() => toggle("timeline")}
          title="What happened in this workspace"
        >
          Timeline
        </button>
        <button
          className={side === "files" ? "btn outline small-btn files-toggle on" : "btn outline small-btn files-toggle"}
          aria-pressed={side === "files"}
          onClick={() => toggle("files")}
          title="Browse, download and upload files"
        >
          Files
        </button>
        <Menu
          label="Workspace actions"
          items={[
            {
              label: "Retry preparation",
              hidden: ws.state !== "failed",
              onSelect: () => void act("workspace_prepare", { host: host.name, workspace: ws.id }),
            },
            {
              label: "Archive workspace…",
              hidden: archived || ws.state === "preparing",
              onSelect: () => setOpen("archive-workspace"),
            },
            { label: "Unarchive workspace", hidden: !archived, onSelect: () => void unarchive() },
            ...pinItems,
            { label: brief.text || brief.more > 0 ? "Edit brief…" : "Add a brief…", onSelect: () => setOpen("brief") },
            { label: "Delete workspace…", danger: true, onSelect: () => setOpen("delete-workspace") },
          ]}
        />
      </div>

      {host.status === "connected" && host.version && appVersion && host.version !== appVersion && (
        <div className="notice notice-row">
          <span>
            {host.name} runs otterd {host.version}; this app is {appVersion}. Fixes in this version may need the
            host updated (running sessions keep going).
          </span>
          <button
            className="btn primary"
            disabled={busy}
            onClick={() => void act("host_install", { name: host.name })}
          >
            {busy ? "Updating…" : `Update otterd on ${host.name}`}
          </button>
        </div>
      )}
      {ws.state === "preparing" && <div className="notice">{ws.stateMessage ?? "Preparing workspace…"}</div>}
      {ws.state === "failed" && ws.stateMessage && (
        // Why, not only that it failed (D-076); "Retry preparation" is in the menu.
        <div className="notice failure-detail" role="status">
          <pre className="failure-text">{ws.stateMessage}</pre>
        </div>
      )}
      {host.status !== "connected" && (
        <div className="notice">
          {host.name} is {host.status === "incompatible" ? "running an incompatible otterd" : "not reachable"} — showing
          the last known state. {host.message}
        </div>
      )}

      <div className="tabs" role="tablist" aria-label="Sessions">
        {ws.sessions.map((s) => (
          <div key={s.id} className={s.id === current?.id ? "tab selected" : "tab"}>
            <button
              role="tab"
              aria-selected={s.id === current?.id}
              className="tab-main"
              onClick={() => onSession(s.id)}
              onDoubleClick={() => {
                setRenaming(s);
                setOpen("rename-session");
              }}
              title="Double-click to rename"
            >
              <Glyph kind={sessionGlyph(s)} size={10} />
              <span className="tab-name">{s.name}</span>
              {s.agent && <span className="tab-kind">{kindLabel(s)}</span>}
            </button>
            <button
              className="tab-close"
              aria-label={`Close ${s.name}`}
              title={s.status === "running" ? `Stop and delete ${s.name}` : `Delete ${s.name}`}
              onClick={() => {
                setClosing(s.id);
                setOpen("delete-session");
              }}
            >
              <svg width="9" height="9" viewBox="0 0 10 10" aria-hidden="true">
                <path d="M2 2l6 6M8 2l-6 6" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
              </svg>
            </button>
          </div>
        ))}
        {!archived && (
          <button className="icon-btn new-tab" aria-label="New session" title="New session" onClick={() => setOpen("new-session")}>
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
              <path d="M6 1.5v9M1.5 6h9" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
          </button>
        )}
        <span className="spacer" />
        {shownPorts.map((p) => {
          const owner = ws.sessions.find((s) => s.id === p.session_id)?.name ?? "a session";
          return (
            <button
              key={p.port}
              className="chip mono port-chip"
              title={`Open http://localhost:${p.port} — ${owner}${p.process ? `, ${p.process}` : ""} listens on it`}
              onClick={() => onOpenPort(p.port)}
            >
              :{p.port} ↗
            </button>
          );
        })}
        {current && !current.agent && <span className="tabs-kind">{current.kind}</span>}
        {current && !archived && (
          <Menu
            label={`${current.name} actions`}
            items={[
              { label: "Restart", onSelect: () => void act("session_restart", sessionArgs()) },
              {
                label: "Rename…",
                onSelect: () => {
                  setRenaming(current);
                  setOpen("rename-session");
                },
              },
              { label: "Find in its history… (⌘F)", onSelect: () => setFinding(true) },
              { label: "Stop", hidden: current.status !== "running", onSelect: () => void act("session_stop", sessionArgs()) },
              {
                label: "Delete session…",
                danger: true,
                onSelect: () => {
                  setClosing(current.id);
                  setOpen("delete-session");
                },
              },
            ]}
          />
        )}
      </div>

      {attention && (
        <div className={`banner ${attention.kind === "failure" ? "failure" : attention.needsYou ? "needs" : "info"}`} role="status">
          <div className="banner-text">
            <Glyph kind={attention.kind === "failure" ? "failed" : attention.needsYou ? "needs" : "done"} />
            <span className="banner-summary">{attention.summary}</span>
            <span className="banner-age">{ago(attention.createdAt, now)} ago</span>
          </div>
          {attention.kind === "failure" && current?.tail && (
            // What it printed last, kept when it ended (D-076).
            <pre className="failure-text" aria-label="What it printed last">
              {current.tail}
            </pre>
          )}
          <div className="banner-actions">
            {failed && (
              <button className="btn outline" disabled={busy} onClick={restart}>
                Restart
              </button>
            )}
            <button className="btn" disabled={busy} onClick={resolve}>
              Mark handled
            </button>
          </div>
        </div>
      )}
      {error && <div className="notice error">{error}</div>}

      <div className="work-row" style={{ "--side-width": `${sideWidth}px` } as CSSProperties}>
        <div className="work">
      {archived ? (
        <div className="empty">
          <p>
            {ws.name} is archived: its sessions are stopped and its files are kept on {host.name}. Unarchive it to
            pick up where you left off.
          </p>
          <button className="btn primary" disabled={busy} onClick={() => void unarchive()}>
            Unarchive
          </button>
        </div>
      ) : current ? (
        <>
          {finding && (
            <FindPanel
              host={host.name}
              workspace={ws.id}
              session={current.id}
              name={current.name}
              onClose={() => setFinding(false)}
            />
          )}
          <Terminal
            key={`${placed.key}/${current.id}`}
            host={host.name}
            workspace={ws.id}
            session={current.id}
            generation={generation}
            onEnd={setEnding}
            label={current.name}
            where={host.name}
            dimmed={ending !== null || over !== null}
            theme={theme}
            active={active && !finding}
          />
          {!ending && over && (
            <div className="ending">
              <span>{over}</span>
              <span className="banner-actions">
                <button className="btn outline" disabled={busy} onClick={() => { setClosing(current.id); setOpen("delete-session"); }}>
                  Delete
                </button>
                <button className="btn primary" disabled={busy} onClick={restart}>
                  {current.agent ? "Restart (resumes the conversation)" : "Restart"}
                </button>
              </span>
            </div>
          )}
          {ending && (
            <div className="ending">
              <span>
                {ending.kind === "exited" &&
                  `${current.name} exited${ending.exitCode !== undefined ? ` with status ${ending.exitCode}` : ""}`}
                {ending.kind === "ended" && `${current.name} ended`}
                {ending.kind === "lost" && `Reconnecting to ${host.name}… the session keeps running.`}
                {ending.kind === "error" && ending.message}
              </span>
              {ending.kind !== "lost" && (
                <span className="banner-actions">
                  {ending.kind === "error" && (
                    <button className="btn outline" onClick={() => { setEnding(null); setGeneration((g) => g + 1); }}>
                      Retry
                    </button>
                  )}
                  {(ending.kind === "exited" || ending.kind === "ended") && (
                    <button className="btn" disabled={busy} onClick={restart}>
                      Restart
                    </button>
                  )}
                </span>
              )}
            </div>
          )}
        </>
      ) : (
        <div className="empty">
          <p>No sessions in this workspace.</p>
          <button className="btn primary" onClick={() => setOpen("new-session")}>
            New session
          </button>
        </div>
      )}

        </div>
        {side && (
          <PanelGrip
            panel={SIDE_PANEL}
            width={sideWidth}
            onResize={setSideWidth}
            label={`${side[0].toUpperCase()}${side.slice(1)} width`}
          />
        )}
        {side === "changes" && (
          <ChangesPanel
            host={host.name}
            workspace={ws.id}
            changes={changes}
            onRefresh={() => setChangesTick((n) => n + 1)}
            onClose={() => setSide(null)}
          />
        )}
        {side === "files" && <FilesPanel host={host.name} workspace={ws.id} active={active} onClose={() => setSide(null)} />}
        {side === "timeline" && <TimelinePanel host={host.name} ws={ws} now={now} onClose={() => setSide(null)} />}
      </div>

      {open === "rename-session" && renaming && (
        <RenameDialog
          title={`Rename ${renaming.name}`}
          initial={renaming.name}
          onRename={async (name) => {
            try {
              await invoke("session_rename", { host: host.name, workspace: ws.id, session: renaming.id, name });
            } catch (e) {
              throw explain(host.name, e);
            }
          }}
          onClose={() => {
            setOpen(null);
            setRenaming(null);
          }}
        />
      )}
      {open === "brief" && (
        <BriefDialog host={host.name} workspace={ws.id} name={ws.name} brief={ws.brief} onClose={() => setOpen(null)} />
      )}
      {open === "new-session" && (
        <NewSessionDialog
          host={host}
          workspace={ws.id}
          onClose={() => setOpen(null)}
          onCreated={(id) => {
            setOpen(null);
            onSession(id);
          }}
        />
      )}
      {open === "archive-workspace" &&
        (() => {
          const running = ws.sessions.filter((s) => s.status === "running").length;
          return (
            <Dialog title={`Archive ${ws.name}?`} onClose={() => setOpen(null)}>
              <p className="muted">
                {running > 0 ? `Stops its ${running} running session${running === 1 ? "" : "s"} and moves` : "Moves"} it to
                Archived. Nothing is deleted:{" "}
                {ws.sourceKind === "git" ? "the worktree, its branch and uncommitted changes" : "its files"} stay on{" "}
                {host.name}. Unarchive it to continue; restarted agents resume their conversations.
              </p>
              {error && <div className="notice error">{error}</div>}
              <div className="form-actions">
                <span className="spacer" />
                <button className="btn outline" onClick={() => setOpen(null)}>
                  Cancel
                </button>
                <button
                  className="btn primary"
                  disabled={busy}
                  onClick={() =>
                    void act("workspace_archive", { host: host.name, workspace: ws.id }).then((ok) => ok && setOpen(null))
                  }
                >
                  Archive workspace
                </button>
              </div>
            </Dialog>
          );
        })()}
      {open === "delete-workspace" && (
        <Dialog title={`Delete ${ws.name}?`} onClose={() => setOpen(null)}>
          <p className="muted">
            Stops its {ws.sessions.length} session{ws.sessions.length === 1 ? "" : "s"} and removes the workspace from{" "}
            {host.name}.{" "}
            {ws.sourceKind === "directory"
              ? "The folder itself is kept."
              : ws.sourceKind === "git"
                ? "Its worktree is removed; the branch stays in the repository."
                : "Its files are deleted."}
          </p>
          {ws.sourceKind === "git" && (
            <label className="check">
              <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
              Discard uncommitted changes
            </label>
          )}
          {error && <div className="notice error">{error}</div>}
          <div className="form-actions">
            <span className="spacer" />
            <button className="btn outline" onClick={() => setOpen(null)}>
              Cancel
            </button>
            <button
              className="btn danger"
              disabled={busy}
              onClick={() =>
                void act("workspace_delete", { host: host.name, workspace: ws.id, force }).then((ok) => ok && setOpen(null))
              }
            >
              Delete workspace
            </button>
          </div>
        </Dialog>
      )}
      {open === "delete-session" &&
        (() => {
          const target = ws.sessions.find((s) => s.id === closing) ?? current;
          if (!target) return null;
          const running = target.status === "running";
          return (
            <Dialog title={`${running ? "Stop and delete" : "Delete"} ${target.name}?`} onClose={() => setOpen(null)}>
              <p className="muted">
                {running ? "It is still running; it will be stopped. " : ""}It is removed from {ws.name}; the workspace’s
                files stay.
              </p>
              {error && <div className="notice error">{error}</div>}
              <div className="form-actions">
                <span className="spacer" />
                <button className="btn outline" onClick={() => setOpen(null)}>
                  Cancel
                </button>
                <button
                  className="btn danger"
                  disabled={busy}
                  onClick={() =>
                    void act("session_delete", { host: host.name, workspace: ws.id, session: target.id }).then(
                      (ok) => ok && setOpen(null),
                    )
                  }
                >
                  {running ? "Stop and delete" : "Delete"}
                </button>
              </div>
            </Dialog>
          );
        })()}
    </main>
  );
}
