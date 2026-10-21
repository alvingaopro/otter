import { useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";
import { explain } from "./newer";
import type { SessionView, WorkspaceView } from "./types";

/**
 * A confirmation that runs one command: it owns its busy flag, its error and
 * what it asks of the developer, so none of it outlives the dialog (a kept
 * pane stays mounted, D-100).
 */
function Confirm({
  title,
  host,
  command,
  args,
  label,
  danger,
  onClose,
  children,
}: {
  title: string;
  host: string;
  command: string;
  args: Record<string, unknown>;
  label: string;
  danger?: boolean;
  onClose: () => void;
  children: ReactNode;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function run() {
    setBusy(true);
    setError(null);
    try {
      await invoke(command, args);
      onClose();
    } catch (e) {
      setError(explain(host, e));
      setBusy(false);
    }
  }
  return (
    <Dialog title={title} onClose={onClose}>
      {children}
      {error && <div className="notice error">{error}</div>}
      <div className="form-actions">
        <span className="spacer" />
        <button className="btn outline" onClick={onClose}>
          Cancel
        </button>
        <button className={danger ? "btn danger" : "btn primary"} disabled={busy} onClick={() => void run()}>
          {label}
        </button>
      </div>
    </Dialog>
  );
}

export function ArchiveWorkspaceDialog({ host, ws, onClose }: { host: string; ws: WorkspaceView; onClose: () => void }) {
  const running = ws.sessions.filter((s) => s.status === "running").length;
  return (
    <Confirm
      title={`Archive ${ws.name}?`}
      host={host}
      command="workspace_archive"
      args={{ host, workspace: ws.id }}
      label="Archive workspace"
      onClose={onClose}
    >
      <p className="muted">
        {running > 0 ? `Stops its ${running} running session${running === 1 ? "" : "s"} and moves` : "Moves"} it to Archived.
        Nothing is deleted: {ws.sourceKind === "git" ? "the worktree, its branch and uncommitted changes" : "its files"} stay
        on {host}. Unarchive it to continue; restarted agents resume their conversations.
      </p>
    </Confirm>
  );
}

export function DeleteWorkspaceDialog({ host, ws, onClose }: { host: string; ws: WorkspaceView; onClose: () => void }) {
  // Never ticked from before: the dialog is made new each time it opens.
  const [force, setForce] = useState(false);
  return (
    <Confirm
      title={`Delete ${ws.name}?`}
      host={host}
      command="workspace_delete"
      args={{ host, workspace: ws.id, force }}
      label="Delete workspace"
      danger
      onClose={onClose}
    >
      <p className="muted">
        Stops its {ws.sessions.length} session{ws.sessions.length === 1 ? "" : "s"} and removes the workspace from {host}.{" "}
        {ws.sourceKind === "directory"
          ? "The folder itself is kept."
          : ws.sourceKind === "git"
            ? `Its worktree and its branch${ws.source ? ` ${ws.source}` : ""} are removed, and the repository's copy on ${host} once no workspace uses it.`
            : "Its files are deleted."}
      </p>
      {ws.sourceKind === "git" && (
        <label className="check">
          <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
          Discard uncommitted changes and commits not pushed anywhere
        </label>
      )}
    </Confirm>
  );
}

export function DeleteSessionDialog({
  host,
  ws,
  session,
  onClose,
}: {
  host: string;
  ws: WorkspaceView;
  session: SessionView;
  onClose: () => void;
}) {
  const running = session.status === "running";
  return (
    <Confirm
      title={`${running ? "Stop and delete" : "Delete"} ${session.name}?`}
      host={host}
      command="session_delete"
      args={{ host, workspace: ws.id, session: session.id }}
      label={running ? "Stop and delete" : "Delete"}
      danger
      onClose={onClose}
    >
      <p className="muted">
        {running ? "It is still running; it will be stopped. " : ""}It is removed from {ws.name}; the workspace’s files stay.
      </p>
    </Confirm>
  );
}
