import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Glyph } from "./Glyph";
import { ago, kindLabel, sessionGlyph } from "./model";
import { Terminal, type Ending } from "./Terminal";
import type { AttentionView, Placed } from "./types";

interface Props {
  placed: Placed;
  session?: string;
  onSession: (id: string) => void;
  now: number;
}

export function WorkspacePane({ placed, session, onSession, now }: Props) {
  const { host, ws } = placed;
  const current = ws.sessions.find((s) => s.id === session) ?? ws.sessions[0];
  const [ending, setEnding] = useState<Ending | null>(null);
  const [generation, setGeneration] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

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

  const attention: AttentionView | undefined =
    current?.attention ?? ws.attention.find((a) => !a.sessionId);
  const failed = current && (current.status === "failed" || attention?.kind === "failure");

  async function act(command: string, args: Record<string, unknown>) {
    setBusy(true);
    setError(null);
    try {
      await invoke(command, args);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  const resolve = () =>
    act("attention_resolve", { host: host.name, workspace: ws.id, session: attention?.sessionId ?? null });
  const restart = () => current && act("session_restart", { host: host.name, workspace: ws.id, session: current.id });

  return (
    <main className="pane">
      <div className="pane-head">
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

      {ws.state === "preparing" && <div className="notice">{ws.stateMessage ?? "Preparing workspace…"}</div>}
      {host.status !== "connected" && (
        <div className="notice">
          {host.name} is {host.status === "incompatible" ? "running an incompatible workd" : "not reachable"} — showing
          the last known state. {host.message}
        </div>
      )}

      <div className="tabs" role="tablist" aria-label="Sessions">
        {ws.sessions.map((s) => (
          <button
            key={s.id}
            role="tab"
            aria-selected={s.id === current?.id}
            className={s.id === current?.id ? "tab selected" : "tab"}
            onClick={() => onSession(s.id)}
          >
            <Glyph kind={sessionGlyph(s)} size={10} />
            <span className="tab-name">{s.name}</span>
            <span className="tab-kind">{kindLabel(s)}</span>
          </button>
        ))}
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
            dimmed={ending !== null}
          />
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
        <div className="empty">This workspace has no sessions.</div>
      )}
    </main>
  );
}
