import type { ChangeKind, WorkspaceChanges } from "./types";

/** One line for the pane header: how much changed since the base (D-102); nothing when there's nothing to say. */
export function changesLine(c: WorkspaceChanges): string | undefined {
  if (!c.git) return undefined;
  const n = c.files.length + c.more;
  const parts: string[] = [];
  if (n > 0) parts.push(`${n} file${n === 1 ? "" : "s"}`, `+${c.insertions} −${c.deletions}`);
  if (c.ahead > 0) parts.push(`↑${c.ahead}`);
  if (c.behind > 0) parts.push(`↓${c.behind}`);
  return parts.length > 0 ? parts.join(" · ") : undefined;
}

export const CHANGE_LETTER: Record<ChangeKind, string> = {
  added: "A",
  modified: "M",
  deleted: "D",
  renamed: "R",
  untracked: "U",
};

export type DiffLine = { kind: "add" | "del" | "hunk" | "meta" | "same"; text: string };

/** A unified diff's lines, by what each is. */
export function diffLines(text: string): DiffLine[] {
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  let inHunk = false;
  return lines.map((t) => {
    if (t.startsWith("@@")) {
      inHunk = true;
      return { kind: "hunk", text: t };
    }
    if (t.startsWith("diff --git")) inHunk = false;
    if (!inHunk) return { kind: "meta", text: t };
    if (t.startsWith("+")) return { kind: "add", text: t };
    if (t.startsWith("-")) return { kind: "del", text: t };
    return { kind: "same", text: t };
  });
}
