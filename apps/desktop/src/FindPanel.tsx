import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** How much of a session's history ⌘F looks through. */
export const HISTORY_LINES = 10_000;
const SHOWN = 300;

export interface Found {
  line: number;
  /** The line cut into plain and matching parts, in order. */
  parts: { text: string; hit: boolean }[];
}

/** Lines of `text` holding `query` (any case), the latest last; at most `limit`. */
export function findLines(text: string, query: string, limit = SHOWN): { found: Found[]; total: number } {
  const q = query.toLowerCase();
  if (!q) return { found: [], total: 0 };
  const found: Found[] = [];
  let total = 0;
  text.split("\n").forEach((line, i) => {
    const low = line.toLowerCase();
    if (!low.includes(q)) return;
    total++;
    const parts: Found["parts"] = [];
    let at = 0;
    for (let hit = low.indexOf(q); hit >= 0; hit = low.indexOf(q, at)) {
      if (hit > at) parts.push({ text: line.slice(at, hit), hit: false });
      parts.push({ text: line.slice(hit, hit + q.length), hit: true });
      at = hit + q.length;
    }
    if (at < line.length) parts.push({ text: line.slice(at), hit: false });
    found.push({ line: i + 1, parts });
  });
  return { found: found.slice(-limit), total };
}

/**
 * ⌘F: find text in what a session printed (D-106). Its history lives in the
 * session, not in the app's terminal, so it is read once when this opens
 * (↻ reads it again) and searched here.
 */
export function FindPanel({
  host,
  workspace,
  session,
  name,
  onClose,
}: {
  host: string;
  workspace: string;
  session: string;
  name: string;
  onClose: () => void;
}) {
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [generation, setGeneration] = useState(0);

  useEffect(() => {
    let live = true;
    setError(null);
    invoke<string>("session_history", { host, workspace, session, lines: HISTORY_LINES })
      .then((t) => live && setText(t))
      .catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [host, workspace, session, generation]);

  const { found, total } = useMemo(() => findLines(text ?? "", query), [text, query]);

  return (
    <div className="find" role="search" aria-label={`Find in ${name}`}>
      <div className="find-bar">
        <input
          autoFocus
          className="find-input"
          placeholder={`Find in ${name}'s history…`}
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              e.preventDefault();
              onClose();
            }
          }}
          spellCheck={false}
        />
        <span className="muted small find-count">
          {text === null && !error ? "reading…" : query ? `${total} line${total === 1 ? "" : "s"}` : ""}
        </span>
        <button className="icon-btn" aria-label="Read the history again" title="Read again" onClick={() => setGeneration((g) => g + 1)}>
          ↻
        </button>
        <button className="icon-btn" aria-label="Close find" title="Close (Esc)" onClick={onClose}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M2 2l6 6M8 2l-6 6" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
        </button>
      </div>
      {error && <div className="notice error">{error}</div>}
      {query && found.length > 0 && (
        <ol className="find-list mono">
          {found.map((f) => (
            <li key={f.line}>
              <span className="find-line muted">{f.line}</span>
              <span className="find-text">
                {f.parts.map((p, i) => (p.hit ? <mark key={i}>{p.text}</mark> : <span key={i}>{p.text}</span>))}
              </span>
            </li>
          ))}
        </ol>
      )}
      {query && total > found.length && <p className="muted small find-more">The latest {found.length} of {total} shown.</p>}
    </div>
  );
}
