import { useEffect, useRef, useState, type FormEvent } from "react";
import { Dialog } from "./Dialog";

/** Give a workspace, a session or a file another name (D-101, D-103). */
export function RenameDialog({
  title,
  initial,
  onRename,
  onClose,
}: {
  title: string;
  initial: string;
  /** Rejects with what to show. */
  onRename: (name: string) => Promise<void>;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => input.current?.select(), []);

  async function submit(e: FormEvent) {
    e.preventDefault();
    const next = name.trim();
    if (busy || !next) return;
    if (next === initial) return onClose();
    setBusy(true);
    setError(null);
    try {
      await onRename(next);
      onClose();
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  }

  return (
    <Dialog title={title} onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <label className="field">
          <span>Name</span>
          <input
            ref={input}
            value={name}
            onChange={(e) => setName(e.target.value)}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
          />
        </label>
        {error && <div className="notice error">{error}</div>}
        <div className="form-actions">
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={busy || !name.trim()}>
            {busy ? "Renaming…" : "Rename"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}
