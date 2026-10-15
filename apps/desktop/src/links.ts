// ⌘-click a link in a terminal (D-106). The session has the mouse (tmux
// scrolls with it), so the app finds the link under the pointer itself.

const URL = /\bhttps?:\/\/[^\s<>"'`()[\]{}]+/g;

/** The http(s) link in `text` covering column `col`, if any. */
export function urlAtColumn(text: string, col: number): string | undefined {
  for (const m of text.matchAll(URL)) {
    const url = m[0].replace(/[.,;:!?]+$/, "");
    const start = m.index ?? 0;
    if (col >= start && col < start + url.length) return url;
  }
  return undefined;
}

/**
 * A terminal row's logical line (rows that wrap into it joined), and where
 * the given row starts in it.
 */
export function logicalLine(rows: { text: string; wrapped: boolean }[], at: number): { text: string; offset: number } {
  let first = at;
  while (first > 0 && rows[first]?.wrapped) first--;
  let text = "";
  let offset = 0;
  for (let i = first; i < rows.length && (i === first || rows[i].wrapped); i++) {
    if (i === at) offset = text.length;
    text += rows[i].text;
  }
  return { text, offset };
}
