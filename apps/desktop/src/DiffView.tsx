import { useEffect, useMemo, useState } from "react";
import { CONTEXT, STEP, foldDiff, parseDiff, splitRows, type DiffLine } from "./diff";
import { highlightLines, languageOf } from "./highlight";

type Mode = "split" | "unified";
const MODE_KEY = "otter.diff-mode";

function storedMode(): Mode {
  try {
    return localStorage.getItem(MODE_KEY) === "unified" ? "unified" : "split";
  } catch {
    return "split";
  }
}

/**
 * One file's diff (D-111): side by side, as GitHub shows it, or unified;
 * highlighted for its language; the lines far from a change folded, each
 * fold opening 20 lines at a time from either side, or all at once.
 */
export function DiffView({ text, path, truncated }: { text: string; path: string; truncated: boolean }) {
  const [mode, setModeState] = useState<Mode>(storedMode);
  const [opened, setOpened] = useState<Set<number>>(() => new Set());
  useEffect(() => setOpened(new Set()), [text]);
  const parsed = useMemo(() => parseDiff(text), [text]);
  const html = useMemo(() => {
    const lang = languageOf(path);
    return { old: highlightLines(parsed.oldText, lang), new: highlightLines(parsed.newText, lang) };
  }, [parsed, path]);
  const items = foldDiff(parsed.lines, opened, parsed.breaks);

  function setMode(m: Mode) {
    setModeState(m);
    try {
      localStorage.setItem(MODE_KEY, m);
    } catch {
      // Not remembered; fine.
    }
  }
  function open(from: number, to: number) {
    setOpened((o) => {
      const next = new Set(o);
      for (let i = from; i < to; i++) next.add(i);
      return next;
    });
  }
  const code = (l: DiffLine | undefined, side: "old" | "new") => {
    if (!l) return "";
    const at = side === "old" ? l.oi : l.ni;
    return (at !== undefined ? (side === "old" ? html.old : html.new)[at] : undefined) ?? "";
  };

  if (parsed.binary) return <p className="muted files-empty">A binary file: no lines to compare.</p>;
  if (parsed.lines.length === 0) return <p className="muted files-empty">No changes in its lines.</p>;

  return (
    <div className={`diff-view ${mode}`}>
      <div className="diff-modes" role="group" aria-label="Diff layout">
        {(["split", "unified"] as const).map((m) => (
          <button key={m} className={mode === m ? "on" : ""} aria-pressed={mode === m} onClick={() => setMode(m)}>
            {m === "split" ? "Side by side" : "Unified"}
          </button>
        ))}
      </div>
      <div className="diff-body" role="table" aria-label={`Changes to ${path}`}>
        {items.map((item, k) => {
          if (item.kind === "skip") {
            return (
              <div key={k} className="diff-skip" role="row">
                ⋯
              </div>
            );
          }
          if (item.kind === "fold") {
            const n = item.to - item.from;
            const first = item.from === 0;
            const last = item.to === parsed.lines.length;
            return (
              <div key={k} className="diff-fold" role="row">
                {n <= STEP ? (
                  <button onClick={() => open(item.from, item.to)}>
                    ↕ Show {n} line{n === 1 ? "" : "s"}
                  </button>
                ) : (
                  <>
                    {!first && (
                      <button title={`The ${STEP} lines after the change above`} onClick={() => open(item.from, item.from + STEP)}>
                        ↓ {STEP}
                      </button>
                    )}
                    {!last && (
                      <button title={`The ${STEP} lines before the change below`} onClick={() => open(item.to - STEP, item.to)}>
                        ↑ {STEP}
                      </button>
                    )}
                    <button onClick={() => open(item.from, item.to)}>Show all {n} lines</button>
                  </>
                )}
              </div>
            );
          }
          if (mode === "split") {
            return splitRows(parsed.lines, item.from, item.to).map((r, i) => (
              <div key={`${k}-${i}`} className="diff-row split" role="row">
                <span className="diff-num">{r.left?.old}</span>
                <span className={`diff-code hl ${r.left ? r.left.kind : "none"}`} dangerouslySetInnerHTML={{ __html: code(r.left, "old") }} />
                <span className="diff-num">{r.right?.new}</span>
                <span className={`diff-code hl ${r.right ? r.right.kind : "none"}`} dangerouslySetInnerHTML={{ __html: code(r.right, "new") }} />
              </div>
            ));
          }
          return parsed.lines.slice(item.from, item.to).map((l, i) => (
            <div key={`${k}-${i}`} className="diff-row unified" role="row">
              <span className="diff-num">{l.old}</span>
              <span className="diff-num">{l.new}</span>
              <span className="diff-sign">{l.kind === "add" ? "+" : l.kind === "del" ? "−" : ""}</span>
              <span className={`diff-code hl ${l.kind}`} dangerouslySetInnerHTML={{ __html: code(l, l.kind === "del" ? "old" : "new") }} />
            </div>
          ));
        })}
      </div>
      {truncated && <p className="files-hint muted">Cut short: the diff is longer than the host sends.</p>}
      <p className="files-hint muted">
        {CONTEXT} lines around each change; open a fold to see more.
      </p>
    </div>
  );
}
