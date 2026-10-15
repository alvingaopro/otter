// A file's diff as the Changes panel shows it (D-111): parsed from a unified
// diff with the whole file as context, folded to the lines near a change,
// opened further on request, and paired side by side.

export interface DiffLine {
  kind: "same" | "add" | "del";
  text: string;
  /** Line numbers in the old and the new file. */
  old?: number;
  new?: number;
  /** Where it is in the old / new text rebuilt from the diff (for highlighting). */
  oi?: number;
  ni?: number;
}

export interface ParsedDiff {
  lines: DiffLine[];
  /** Where a hunk starts after lines the diff left out (a daemon that sent only some context). */
  breaks: number[];
  binary: boolean;
  /** The old and new text, as far as the diff has them. */
  oldText: string;
  newText: string;
}

/** Lines shown around a change before the rest folds away, as GitHub does. */
export const CONTEXT = 3;
/** Lines one click opens, from either side of a fold. */
export const STEP = 20;

export function parseDiff(text: string): ParsedDiff {
  const lines: DiffLine[] = [];
  const breaks: number[] = [];
  const oldLines: string[] = [];
  const newLines: string[] = [];
  let o = 0;
  let n = 0;
  let inHunk = false;
  let binary = false;
  for (const raw of text.split("\n")) {
    const hunk = raw.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (hunk) {
      if (lines.length > 0) breaks.push(lines.length);
      o = Number(hunk[1]);
      n = Number(hunk[2]);
      inHunk = true;
      continue;
    }
    if (raw.startsWith("diff --git")) {
      inHunk = false;
      continue;
    }
    if (/^Binary files .* differ$/.test(raw)) binary = true;
    if (!inHunk || raw === "" || raw.startsWith("\\")) continue;
    const body = raw.slice(1);
    if (raw[0] === "+") {
      lines.push({ kind: "add", text: body, new: n++, ni: newLines.push(body) - 1 });
    } else if (raw[0] === "-") {
      lines.push({ kind: "del", text: body, old: o++, oi: oldLines.push(body) - 1 });
    } else if (raw[0] === " ") {
      lines.push({ kind: "same", text: body, old: o++, new: n++, oi: oldLines.push(body) - 1, ni: newLines.push(body) - 1 });
    }
  }
  return { lines, breaks, binary, oldText: oldLines.join("\n"), newText: newLines.join("\n") };
}

export type DiffItem =
  | { kind: "lines"; from: number; to: number }
  | { kind: "fold"; from: number; to: number }
  | { kind: "skip" };

/**
 * What to show: runs of lines near a change (or opened), the folds between
 * them (`from`..`to`, hidden), and a skip where the diff itself left lines
 * out.
 */
export function foldDiff(lines: DiffLine[], opened: Set<number>, breaks: number[], context = CONTEXT): DiffItem[] {
  const near = lines.map(() => false);
  lines.forEach((l, i) => {
    if (l.kind === "same") return;
    for (let j = Math.max(0, i - context); j <= Math.min(lines.length - 1, i + context); j++) near[j] = true;
  });
  const shown = lines.map((_, i) => near[i] || opened.has(i));
  const cut = new Set(breaks);
  const items: DiffItem[] = [];
  let i = 0;
  while (i < lines.length) {
    if (cut.has(i) && i > 0) items.push({ kind: "skip" });
    let j = i + 1;
    while (j < lines.length && shown[j] === shown[i] && !cut.has(j)) j++;
    items.push({ kind: shown[i] ? "lines" : "fold", from: i, to: j });
    i = j;
  }
  return items;
}

/** A side-by-side row: the old line on the left, the new on the right. */
export interface SplitRow {
  left?: DiffLine;
  right?: DiffLine;
}

/** Lines `from`..`to` side by side: what was removed paired with what took its place. */
export function splitRows(lines: DiffLine[], from: number, to: number): SplitRow[] {
  const rows: SplitRow[] = [];
  let i = from;
  while (i < to) {
    if (lines[i].kind === "same") {
      rows.push({ left: lines[i], right: lines[i] });
      i++;
      continue;
    }
    const dels: DiffLine[] = [];
    const adds: DiffLine[] = [];
    while (i < to && lines[i].kind === "del") dels.push(lines[i++]);
    while (i < to && lines[i].kind === "add") adds.push(lines[i++]);
    for (let k = 0; k < Math.max(dels.length, adds.length); k++) rows.push({ left: dels[k], right: adds[k] });
  }
  return rows;
}
