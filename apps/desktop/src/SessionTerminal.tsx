import { useEffect, useRef, useState } from "react";
import { Terminal, type Ending } from "./Terminal";
import type { HostView, SessionView } from "./types";

/**
 * One session's terminal, and what is said under it when the session isn't
 * running or its attach ended. A workspace shows one, or two side by side
 * (D-110), and keeps the ones shown before attached, hidden (D-152).
 */
export function SessionTerminal({
  host,
  workspace,
  wsKey,
  session,
  theme,
  active,
  shown = true,
  right = false,
  busy,
  onRestart,
  onDelete,
  onFocus,
}: {
  host: HostView;
  workspace: string;
  /** The workspace's `host/id`, for the terminal's identity. */
  wsKey: string;
  session: SessionView;
  theme: "light" | "dark";
  /** It has the keyboard. */
  active: boolean;
  /** In front. Not shown, it stays attached and idle (D-152). */
  shown?: boolean;
  /** The right one of two. */
  right?: boolean;
  busy: boolean;
  onRestart: () => void;
  onDelete: () => void;
  /** Clicked: it should get the keyboard. */
  onFocus?: () => void;
}) {
  const [ending, setEnding] = useState<Ending | null>(null);
  const [generation, setGeneration] = useState(0);

  // A new execution of it starts a fresh attach. A restart stops and starts
  // it at once, so the session may never be seen not running: what changes
  // is the execution, and its attach may end before or after that is seen.
  const running = session.status === "running";
  const attached = useRef(session.execution);
  useEffect(() => {
    if (running && ending && ending.kind !== "error" && session.execution !== attached.current) {
      attached.current = session.execution;
      setEnding(null);
      setGeneration((g) => g + 1);
    }
  }, [running, session.execution, ending]);

  // Lost connection: try again while the host is up.
  useEffect(() => {
    if (ending?.kind !== "lost" || host.status !== "connected") return;
    const t = setTimeout(() => {
      setEnding(null);
      setGeneration((g) => g + 1);
    }, 2000);
    return () => clearTimeout(t);
  }, [ending, host.status]);

  // Not running, however it got there (also when we never watched it end):
  // the last screen stays visible, dimmed.
  const over: string | null =
    session.status === "completed" || session.status === "failed"
      ? `${session.name} exited${session.exitCode !== undefined ? ` with status ${session.exitCode}` : ""}.`
      : session.status === "stopped"
        ? `${session.name} was stopped.`
        : session.status === "lost"
          ? `${session.name}'s process is gone.`
          : session.status === "pending" && session.launchError
            ? `${session.name} failed to start: ${session.launchError}`
            : null;

  return (
    <div
      className={right ? "session-col beside" : "session-col"}
      // Where it goes among the others without moving its element.
      style={{ order: right ? 2 : 0 }}
      hidden={!shown}
      onMouseDown={onFocus}
    >
      <Terminal
        key={`${wsKey}/${session.id}`}
        host={host.name}
        workspace={workspace}
        session={session.id}
        generation={generation}
        onEnd={setEnding}
        label={session.name}
        where={host.name}
        dimmed={ending !== null || over !== null}
        theme={theme}
        active={active}
      />
      {!ending && over && (
        <div className="ending">
          <span>{over}</span>
          <span className="banner-actions">
            <button className="btn outline" disabled={busy} onClick={onDelete}>
              Delete
            </button>
            <button className="btn primary" disabled={busy} onClick={onRestart}>
              {session.agent ? "Restart (resumes the conversation)" : "Restart"}
            </button>
          </span>
        </div>
      )}
      {ending && (
        <div className="ending">
          <span>
            {ending.kind === "exited" &&
              `${session.name} exited${ending.exitCode !== undefined ? ` with status ${ending.exitCode}` : ""}`}
            {ending.kind === "ended" && `${session.name} ended`}
            {ending.kind === "lost" && `Reconnecting to ${host.name}… the session keeps running.`}
            {ending.kind === "error" && ending.message}
          </span>
          {ending.kind !== "lost" && (
            <span className="banner-actions">
              {ending.kind === "error" && (
                <button
                  className="btn outline"
                  onClick={() => {
                    setEnding(null);
                    setGeneration((g) => g + 1);
                  }}
                >
                  Retry
                </button>
              )}
              {(ending.kind === "exited" || ending.kind === "ended") && (
                <button className="btn" disabled={busy} onClick={onRestart}>
                  Restart
                </button>
              )}
            </span>
          )}
        </div>
      )}
    </div>
  );
}
