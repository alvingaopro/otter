import { useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";
import type { Brief } from "./types";

const lines = (items?: string[]) => (items ?? []).join("\n");
const unlines = (text: string) =>
  text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

/** Edit why a workspace exists (design §8). Saves the whole brief; the daemon tidies blanks. */
export function BriefDialog({
  host,
  workspace,
  name,
  brief,
  onClose,
}: {
  host: string;
  workspace: string;
  name: string;
  brief: Brief;
  onClose: () => void;
}) {
  const [title, setTitle] = useState(brief.title ?? "");
  const [goal, setGoal] = useState(brief.goal ?? "");
  const [description, setDescription] = useState(brief.description ?? "");
  const [constraints, setConstraints] = useState(lines(brief.constraints));
  const [references, setReferences] = useState(lines(brief.references));
  const [decisions, setDecisions] = useState(lines(brief.decisions));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    const next: Brief = {
      title: title.trim() || undefined,
      goal: goal.trim() || undefined,
      description: description.trim() || undefined,
      constraints: unlines(constraints),
      references: unlines(references),
      decisions: unlines(decisions),
    };
    try {
      await invoke("workspace_set_brief", { host, workspace, brief: next });
      onClose();
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  }

  return (
    <Dialog title={`Brief for ${name}`} onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <label className="field">
          <span>Goal</span>
          <textarea value={goal} onChange={(e) => setGoal(e.target.value)} rows={2} placeholder="Why this workspace exists" />
        </label>
        <label className="field">
          <span>Title</span>
          <input className="prose" value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Fix GCP renewal validation" />
        </label>
        <label className="field">
          <span>Description</span>
          <textarea value={description} onChange={(e) => setDescription(e.target.value)} rows={3} />
        </label>
        <details className="advanced" open={listed(brief) > 0}>
          <summary>Constraints, references, decisions</summary>
          <label className="field">
            <span>Constraints — one per line</span>
            <textarea value={constraints} onChange={(e) => setConstraints(e.target.value)} rows={2} />
          </label>
          <label className="field">
            <span>References — one per line</span>
            <textarea value={references} onChange={(e) => setReferences(e.target.value)} rows={2} spellCheck={false} />
          </label>
          <label className="field">
            <span>Decisions — one per line</span>
            <textarea value={decisions} onChange={(e) => setDecisions(e.target.value)} rows={2} />
          </label>
        </details>
        {error && <div className="notice error">{error}</div>}
        <div className="form-actions">
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={busy}>
            {busy ? "Saving…" : "Save"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}

/** One line for the pane header: the goal (or title), and how much more the brief holds. */
export function briefSummary(b: Brief): { text?: string; more: number } {
  const text = b.goal ?? b.title ?? b.description;
  const more =
    [b.title, b.goal, b.description].filter(Boolean).length -
    (text ? 1 : 0) +
    listed(b);
  return { text, more };
}

const listed = (b: Brief) => (b.constraints?.length ?? 0) + (b.references?.length ?? 0) + (b.decisions?.length ?? 0);
