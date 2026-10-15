import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { CHANGE_LETTER } from "./changes";
import { DiffView } from "./DiffView";
import { explain } from "./newer";
import type { FileDiff, WorkspaceChanges } from "./types";

/** Context enough for any file the host sends: the diff carries the whole of it. */
const WHOLE_FILE = 1_000_000;

/** What `WorkspacePane` holds of a workspace's changes. */
export type Changes = { state: "loading" } | { state: "ready"; changes: WorkspaceChanges } | { state: "too_old" } | { state: "error"; error: string };

/**
 * A workspace's Git work (D-102): the files changed since its base,
 * committed or not, and one file's diff.
 */
export function ChangesPanel({
  host,
  workspace,
  changes,
  onRefresh,
  onClose,
}: {
  host: string;
  workspace: string;
  changes: Changes;
  onRefresh: () => void;
  onClose: () => void;
}) {
  const [file, setFile] = useState<string | null>(null);
  const [diff, setDiff] = useState<FileDiff | null>(null);
  const [error, setError] = useState<string | null>(null);
  const version = changes.state === "ready" ? changes.changes : null;

  useEffect(() => {
    if (!file) return;
    let live = true;
    setError(null);
    // The whole file, folded here (D-111); an older otterd sends its usual hunks.
    invoke<FileDiff>("workspace_diff", { host, workspace, path: file, context: WHOLE_FILE })
      .then((d) => live && setDiff(d))
      .catch((e) => live && setError(explain(host, e)));
    return () => {
      live = false;
    };
  }, [host, workspace, file, version]);

  const c = changes.state === "ready" ? changes.changes : null;
  return (
    <aside className="files changes" aria-label="Changes">
      <div className="files-head">
        <span className="files-title">{file ? "Diff" : "Changes"}</span>
        <span className="files-actions">
          <button className="icon-btn" aria-label="Refresh" title="Refresh" onClick={onRefresh}>
            ↻
          </button>
          <button className="icon-btn" aria-label="Close changes" title="Close" onClick={onClose}>
            <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
              <path d="M2 2l6 6M8 2l-6 6" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
          </button>
        </span>
      </div>
      {changes.state === "too_old" && (
        <p className="notice">{host}'s otterd is too old to show changes: update it from the host's page.</p>
      )}
      {changes.state === "error" && <div className="notice error">{changes.error}</div>}
      {c && !c.git && <p className="muted files-empty">Not a Git repository.</p>}
      {c?.git && !file && (
        <>
          <div className="files-path changes-base muted">
            <span className="mono files-where" title={c.base ? `Changes since ${c.base}` : "What isn't committed"}>
              {c.branch ?? "detached HEAD"}
              {c.base ? ` · since ${c.base}` : " · not committed"}
            </span>
            {(c.ahead > 0 || c.behind > 0) && (
              <span className="mono" title={`${c.ahead} commits ahead, ${c.behind} behind`}>
                ↑{c.ahead} ↓{c.behind}
              </span>
            )}
          </div>
          <ul className="files-list">
            {c.files.map((f) => (
              <li key={f.path}>
                <button className="files-row" onClick={() => setFile(f.path)} title={`${f.path}: show the diff`}>
                  <span className={`change-letter ${f.change}`} aria-label={f.change}>
                    {CHANGE_LETTER[f.change]}
                  </span>
                  <span className="files-name" dir="rtl">
                    <bdi>{f.path}</bdi>
                  </span>
                  {f.uncommitted && <span className="change-dot" title="Not committed yet" aria-label="not committed" />}
                  <span className="change-count mono">
                    {f.insertions === undefined ? (
                      <span className="muted">bin</span>
                    ) : (
                      <>
                        <span className="add">+{f.insertions}</span> <span className="del">−{f.deletions ?? 0}</span>
                      </>
                    )}
                  </span>
                </button>
              </li>
            ))}
            {c.files.length === 0 && <li className="muted files-empty">No changes.</li>}
            {c.more > 0 && <li className="muted files-empty">… and {c.more} more files</li>}
          </ul>
        </>
      )}
      {file && (
        <>
          <div className="files-path">
            <button className="icon-btn" aria-label="Back to the changes" title="Back" onClick={() => setFile(null)}>
              ←
            </button>
            <span className="mono files-where" title={file}>
              {file}
            </span>
          </div>
          {error && <div className="notice error">{error}</div>}
          {diff ? <DiffView text={diff.text} path={file} truncated={diff.truncated} /> : !error && <p className="muted files-empty">Reading…</p>}
        </>
      )}
    </aside>
  );
}
