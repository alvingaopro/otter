import { useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";
import type { HostView } from "./types";

type Kind = "terminal" | "codex" | "claude" | "service" | "task";

const KINDS: [Kind, string, string][] = [
  ["terminal", "Shell", "An interactive shell (or a command you run interactively)."],
  ["codex", "Codex", "A Codex session; it remembers its conversation across restarts."],
  ["claude", "Claude Code", "A Claude Code session; it remembers its conversation across restarts."],
  ["service", "Service", "A long-running command, like a dev server."],
  ["task", "Task", "A command that runs to completion, like tests; you’re told if it fails."],
];

/** Start another session in a workspace. */
export function NewSessionDialog({
  host,
  workspace,
  onCreated,
  onClose,
}: {
  host: HostView;
  workspace: string;
  onCreated: (sessionId: string) => void;
  onClose: () => void;
}) {
  const [kind, setKind] = useState<Kind>("terminal");
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const isAgent = kind === "codex" || kind === "claude";
  const agent = isAgent ? host.agents.find((a) => a.provider === kind) : undefined;
  const needsCommand = kind === "service" || kind === "task";

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    const spec = {
      kind: isAgent ? "agent" : kind,
      name: name.trim() || undefined,
      command: isAgent ? undefined : command.trim() || undefined,
      provider: isAgent ? kind : undefined,
      prompt: isAgent ? prompt.trim() || undefined : undefined,
    };
    try {
      onCreated(await invoke<string>("session_create", { host: host.name, workspace, spec }));
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  }

  return (
    <Dialog title="New session" onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <fieldset className="field">
          <legend>Kind</legend>
          <div className="segmented">
            {KINDS.map(([value, label]) => (
              <label key={value} className={kind === value ? "on" : ""}>
                <input type="radio" name="kind" checked={kind === value} onChange={() => setKind(value)} />
                {label}
              </label>
            ))}
          </div>
          <span className="muted small">{KINDS.find((k) => k[0] === kind)![2]}</span>
        </fieldset>

        {agent && !agent.available && (
          <div className="notice error">
            {KINDS.find((k) => k[0] === kind)![1]} isn’t installed on {host.name}.
          </div>
        )}
        {isAgent ? (
          <label className="field">
            <span>Prompt</span>
            <textarea value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder="Optional" rows={3} />
          </label>
        ) : (
          <label className="field">
            <span>Command{needsCommand ? "" : " (optional)"}</span>
            <input
              value={command}
              onChange={(e) => setCommand(e.target.value)}
              placeholder={kind === "terminal" ? "your login shell" : kind === "service" ? "npm run dev" : "cargo test"}
              spellCheck={false}
              required={needsCommand}
            />
          </label>
        )}
        <label className="field">
          <span>Name (optional)</span>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="derived from the command" spellCheck={false} />
        </label>

        {error && <div className="notice error">{error}</div>}
        <div className="form-actions">
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={busy || (needsCommand && !command.trim())}>
            {busy ? "Starting…" : "Start"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}
