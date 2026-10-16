import { useEffect, useState, type FormEvent } from "react";
import { Dialog } from "./Dialog";
import type { MemoryScope, MemorySource, OtterMemory } from "./memory";
import { tooOld } from "./otterChat";

type Loaded = { state: "loading" } | { state: "ready"; memory: OtterMemory } | { state: "too_old" } | { state: "error"; error: string };

/**
 * Otter's memories as one conversation is given them (D-113): its own, and
 * the host's across every conversation. The developer reads and corrects
 * them; Otter adds to them from what the developer decides.
 */
export function MemoryDialog({
  source,
  host,
  task,
  conversation,
  onClose,
}: {
  source: MemorySource;
  host: string;
  /** The task whose conversation it is; none for Otter's on the host. */
  task?: string;
  /** What the conversation's memory is called here ("This task"). */
  conversation: string;
  onClose: () => void;
}) {
  const [loaded, setLoaded] = useState<Loaded>({ state: "loading" });
  const [texts, setTexts] = useState<Record<MemoryScope, string>>({ host: "", conversation: "" });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = async () => {
    setError(null);
    try {
      const memory = await source.get(host, task);
      setLoaded({ state: "ready", memory });
      setTexts({ host: memory.host, conversation: memory.conversation });
    } catch (e) {
      setLoaded(tooOld(e) ? { state: "too_old" } : { state: "error", error: String(e) });
    }
  };

  useEffect(() => {
    void load();
  }, [host, task]);

  const was = loaded.state === "ready" ? loaded.memory : undefined;
  const changed = (["conversation", "host"] as MemoryScope[]).filter((m) => was && texts[m].trim() !== was[m].trim());

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (!was || busy) return;
    setBusy(true);
    setError(null);
    try {
      for (const m of changed) await source.set(host, task, m, texts[m], was[m]);
      onClose();
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  }

  const field = (m: MemoryScope, label: string, hint: string) => (
    <label className="field">
      <span>
        {label}
        <span className="muted small memory-count">
          {texts[m].trim().length} / {was?.max_chars ?? 8000}
        </span>
      </span>
      <textarea
        className="memory-text"
        value={texts[m]}
        onChange={(e) => setTexts((t) => ({ ...t, [m]: e.target.value }))}
        rows={7}
        placeholder={hint}
        spellCheck={false}
      />
    </label>
  );

  return (
    <Dialog title="Otter's memory" onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <p className="muted small">
          What Otter noted from what you decided, so it outlasts the recent messages. Otter is given both every time it
          answers here. Correct or clear what is wrong.
        </p>
        {loaded.state === "loading" && <p className="muted small">Loading…</p>}
        {loaded.state === "too_old" && (
          <p className="notice small">This host's otterd is too old for Otter's memory: update it.</p>
        )}
        {loaded.state === "error" && <p className="error-text small">{loaded.error}</p>}
        {loaded.state === "ready" && (
          <>
            {field("conversation", conversation, "Nothing noted in this conversation yet.")}
            {field("host", `Every conversation on ${host}`, "Nothing noted across conversations yet.")}
          </>
        )}
        {error && (
          <div className="notice error">
            {error}{" "}
            <button type="button" className="link-btn" onClick={() => void load()}>
              Look again
            </button>
          </div>
        )}
        <div className="form-actions">
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            {changed.length ? "Cancel" : "Close"}
          </button>
          <button type="submit" className="btn primary" disabled={busy || changed.length === 0}>
            {busy ? "Saving…" : "Save"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}
