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
