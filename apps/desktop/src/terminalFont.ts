// The terminals' font size, set with ⌘= / ⌘- / ⌘0 and remembered on this
// Mac (D-106). One size for every terminal, as in other terminal apps.

const KEY = "otter.terminal-font-size";
export const DEFAULT_FONT = 13;
const MIN = 9;
const MAX = 24;

let size = read();
const listeners = new Set<(n: number) => void>();

function read(): number {
  try {
    const v = Number(localStorage.getItem(KEY));
    return v >= MIN && v <= MAX ? v : DEFAULT_FONT;
  } catch {
    return DEFAULT_FONT;
  }
}

export function fontSize(): number {
  return size;
}

export function setFontSize(n: number) {
  const next = Math.min(MAX, Math.max(MIN, Math.round(n)));
  if (next === size) return;
  size = next;
  try {
    localStorage.setItem(KEY, String(next));
  } catch {
    // Not remembered; fine.
  }
  for (const f of listeners) f(next);
}

export function onFontSize(f: (n: number) => void): () => void {
  listeners.add(f);
  return () => listeners.delete(f);
}

/** ⌘= / ⌘+ larger, ⌘- smaller, ⌘0 back: whether `e` was one of them. */
export function zoomKey(e: { metaKey: boolean; ctrlKey: boolean; altKey: boolean; key: string }): boolean {
  if (!e.metaKey || e.ctrlKey || e.altKey) return false;
  if (e.key === "=" || e.key === "+") setFontSize(size + 1);
  else if (e.key === "-" || e.key === "_") setFontSize(size - 1);
  else if (e.key === "0") setFontSize(DEFAULT_FONT);
  else return false;
  return true;
}
