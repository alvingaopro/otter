import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { Glyph } from "./Glyph";
import { headline, lastActive, matchScore, workspaceGlyph } from "./model";
import type { Placed } from "./types";

const SHOWN = 30;

/** The workspaces matching `query`, best first; the latest active among equals. */
export function quickMatches(placed: Placed[], query: string): Placed[] {
  return placed
    .map((p) => ({ p, score: matchScore(p, query) - (p.ws.activity === "archived" ? 0.5 : 0) }))
    .filter((m) => m.score > 0)
    .sort((a, b) => b.score - a.score || lastActive(b.p.ws).localeCompare(lastActive(a.p.ws)))
    .map((m) => m.p);
}

/** ⌘K: go to a workspace by typing part of its name, host or branch (D-106). */
export function QuickOpen({ placed, onOpen, onClose }: { placed: Placed[]; onOpen: (key: string) => void; onClose: () => void }) {
  const [query, setQuery] = useState("");
  const [at, setAt] = useState(0);
  const list = useRef<HTMLUListElement>(null);
  const matches = useMemo(() => quickMatches(placed, query).slice(0, SHOWN), [placed, query]);
  useEffect(() => setAt(0), [query]);
  useEffect(() => {
    list.current?.querySelector<HTMLElement>(`[data-at="${at}"]`)?.scrollIntoView?.({ block: "nearest" });
  }, [at]);

  function key(e: KeyboardEvent) {
    if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
      e.preventDefault();
      setAt((i) => Math.min(i + 1, matches.length - 1));
    } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
      e.preventDefault();
      setAt((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const m = matches[at];
      if (m) onOpen(m.key);
    } else if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    }
  }

  return (
    <div className="backdrop quick-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="quick" role="dialog" aria-modal="true" aria-label="Go to a workspace">
        <input
          className="quick-input"
          autoFocus
          placeholder="Go to a workspace…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={key}
          aria-label="Workspace name, host or branch"
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
        />
        <ul className="quick-list" role="listbox" ref={list}>
          {matches.map((p, i) => (
            <li key={p.key}>
              <button
                role="option"
                aria-selected={i === at}
                data-at={i}
                className={i === at ? "quick-row on" : "quick-row"}
                onMouseMove={() => setAt(i)}
                onClick={() => onOpen(p.key)}
              >
                <Glyph kind={workspaceGlyph(p.ws)} size={10} />
                <span className="quick-name">{p.ws.name}</span>
                <span className="quick-what muted">{headline(p.ws).text}</span>
                <span className="quick-host muted">{p.host.name}</span>
              </button>
            </li>
          ))}
          {matches.length === 0 && <li className="muted quick-none">No workspace matches.</li>}
        </ul>
      </div>
    </div>
  );
}
