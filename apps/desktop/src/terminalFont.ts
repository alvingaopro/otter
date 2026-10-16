// The terminals' font size, set with ⌘= / ⌘- / ⌘0 and remembered on this
// Mac (D-106). One size for every terminal, as in other terminal apps. Their
// font, chosen in Settings and remembered the same way (D-116).

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

const FAMILY_KEY = "otter.terminal-font-family";
/** The bundled font, then the Mac's own. Also the fallback after a chosen one. */
export const DEFAULT_FAMILY = '"JetBrains Mono", "SF Mono", Menlo, monospace';
/** Offered in Settings; any installed font can be typed. */
export const FONT_SUGGESTIONS = ["JetBrains Mono", "SF Mono", "Menlo", "Monaco", "Courier New"];

let typed = readFamily();
const familyListeners = new Set<(stack: string) => void>();

function readFamily(): string {
  try {
    return localStorage.getItem(FAMILY_KEY) ?? "";
  } catch {
    return "";
  }
}

/** The font as the developer typed it; empty for the default. */
export function fontChoice(): string {
  return typed;
}

/** The terminals' CSS font stack. */
export function fontFamily(): string {
  return familyStack(typed);
}

export function setFontFamily(choice: string) {
  if (choice === typed) return;
  typed = choice;
  try {
    if (choice.trim()) localStorage.setItem(FAMILY_KEY, choice);
    else localStorage.removeItem(FAMILY_KEY);
  } catch {
    // Not remembered; fine.
  }
  for (const f of familyListeners) f(fontFamily());
}

export function onFontFamily(f: (stack: string) => void): () => void {
  familyListeners.add(f);
  return () => familyListeners.delete(f);
}

/**
 * A typed font as a CSS stack: one name or a comma-separated list, each
 * quoted; the default follows, for glyphs the chosen font lacks or a name
 * that isn't installed.
 */
export function familyStack(choice: string): string {
  const names = choice
    .split(",")
    .map((n) => n.trim().replace(/^["']+|["']+$/g, "").replace(/["\\]/g, ""))
    .filter(Boolean);
  if (names.length === 0) return DEFAULT_FAMILY;
  const generic = new Set(["monospace", "ui-monospace"]);
  return [...names.map((n) => (generic.has(n) ? n : `"${n}"`)), DEFAULT_FAMILY].join(", ");
}
