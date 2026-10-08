import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Glyph } from "./Glyph";
import { Dialog } from "./Dialog";
import { Menu } from "./Menu";
import { NewSessionDialog } from "./NewSessionDialog";
import { ago, kindLabel, sessionGlyph } from "./model";
import { Terminal, type Ending } from "./Terminal";
import type { AttentionView, Placed } from "./types";

interface Props {
  placed: Placed;
  session?: string;
  onSession: (id: string) => void;
  now: number;
  theme: "light" | "dark";
  /** This app's version: a host on another otterd version is offered an update. */
  appVersion?: string;
}

type Open = "new-session" | "delete-workspace" | "delete-session" | null;

export function WorkspacePane({ placed, session, onSession, now, theme, appVersion }: Props) {
  const { host, ws } = placed;
  const current = ws.sessions.find((s) => s.id === session) ?? ws.sessions[0];
  const [ending, setEnding] = useState<Ending | null>(null);
  const [generation, setGeneration] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<Open>(null);
  const [force, setForce] = useState(false);
  const [closing, setClosing] = useState<string | null>(null);

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
    <main className="pane">
      <div className="pane-head">
        <Menu
          label="Workspace actions"
          items={[
            {
              label: "Retry preparation",
              hidden: ws.state !== "failed",
              onSelect: () => void act("workspace_prepare", { host: host.name, workspace: ws.id }),
            },
            { label: "Delete workspace…", danger: true, onSelect: () => setOpen("delete-workspace") },
          ]}
        />
        <div className="pane-title">
          <h1>{ws.name}</h1>
          <div className="chips">
            <span className="chip mono">{host.name}</span>
            {ws.sourceKind === "git" && <span className="chip mono">{ws.source}</span>}
            {ws.sourceKind === "directory" && <span className="chip">directory</span>}
            {ws.sourceKind === "empty" && <span className="chip">scratch</span>}
            <span className="root mono">{ws.root}</span>
          </div>
        </div>
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
      {host.status !== "connected" && (
        <div className="notice">
          {host.name} is {host.status === "incompatible" ? "running an incompatible otterd" : "not reachable"} — showing
          the last known state. {host.message}
        </div>
      )}

      <div className="tabs" role="tablist" aria-label="Sessions">
        {ws.sessions.map((s) => (
          <div key={s.id} className={s.id === current?.id ? "tab selected" : "tab"}>
            <button role="tab" aria-selected={s.id === current?.id} className="tab-main" onClick={() => onSession(s.id)}>
              <Glyph kind={sessionGlyph(s)} size={10} />
              <span className="tab-name">{s.name}</span>
              <span className="tab-kind">{kindLabel(s)}</span>
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
        <button className="icon-btn new-tab" aria-label="New session" title="New session" onClick={() => setOpen("new-session")}>
          <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
            <path d="M6 1.5v9M1.5 6h9" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
        </button>
        <span className="spacer" />
        {current && (
          <Menu
            label={`${current.name} actions`}
            items={[
              { label: "Restart", onSelect: () => void act("session_restart", sessionArgs()) },
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

      {current ? (
        <>
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
