import { useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";
import type { HostView } from "./types";

type Source = "empty" | "git" | "directory";

/** Coding agents the app can start, by provider id. */
export const AGENTS: [string, string][] = [
  ["codex", "Codex"],
  ["claude", "Claude Code"],
];

/** Create a workspace on a host: where its files come from, what starts in it. */
export function NewWorkspaceDialog({
  hosts,
  defaultHost,
  onCreated,
  onClose,
}: {
  hosts: HostView[];
  defaultHost?: string;
  onCreated: (host: string, workspaceId: string) => void;
  onClose: () => void;
}) {
  const usable = hosts.filter((h) => h.status === "connected");
  const [host, setHost] = useState(
    usable.find((h) => h.name === defaultHost)?.name ?? usable[0]?.name ?? "",
  );
  const [name, setName] = useState("");
  const [source, setSource] = useState<Source>("empty");
  const [repo, setRepo] = useState("");
  const [branch, setBranch] = useState("");
  const [base, setBase] = useState("");
  const [dir, setDir] = useState("");
  const [agent, setAgent] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [shell, setShell] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const hostAgents = usable.find((h) => h.name === host)?.agents ?? [];
  const available = (id: string) => hostAgents.find((a) => a.provider === id)?.available ?? false;
  // Default to the first agent this host can run; "none" once chosen sticks.
  const chosen = agent ?? AGENTS.find(([id]) => available(id))?.[0] ?? "none";

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    const sessions = [];
    if (chosen !== "none" && available(chosen))
      sessions.push({ kind: "agent", provider: chosen, prompt: prompt.trim() || undefined });
    if (shell) sessions.push({ kind: "terminal" });
    const spec =
      source === "git"
        ? { type: "git", repository: repo.trim(), branch: branch.trim() || undefined, base: base.trim() || undefined }
        : source === "directory"
          ? { type: "directory", path: dir.trim() }
          : { type: "empty" };
    try {
      const id = await invoke<string>("workspace_create", { host, name, source: spec, sessions });
      onCreated(host, id);
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  }

  if (usable.length === 0) {
    return (
      <Dialog title="New workspace" onClose={onClose}>
        <p className="muted">No host is connected right now. Add a host, or wait for one to reconnect.</p>
        <div className="form-actions">
          <span className="spacer" />
          <button className="btn" onClick={onClose}>
            Close
          </button>
        </div>
      </Dialog>
    );
  }

  return (
    <Dialog title="New workspace" onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <div className="row-2">
          <label className="field">
            <span>Name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="billing-fix"
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              required
            />
          </label>
          <label className="field">
            <span>Host</span>
            <select value={host} onChange={(e) => setHost(e.target.value)}>
              {usable.map((h) => (
                <option key={h.name} value={h.name}>
                  {h.name}
                </option>
              ))}
            </select>
          </label>
        </div>

        <fieldset className="field">
          <legend>Files</legend>
          <div className="segmented">
            {(
              [
                ["empty", "Empty folder"],
                ["git", "Git repository"],
                ["directory", "Existing folder"],
              ] as const
            ).map(([value, label]) => (
              <label key={value} className={source === value ? "on" : ""}>
                <input type="radio" name="source" checked={source === value} onChange={() => setSource(value)} />
                {label}
              </label>
            ))}
          </div>
        </fieldset>

        {source === "git" && (
          <>
            <label className="field">
              <span>Repository</span>
              <input
                value={repo}
                onChange={(e) => setRepo(e.target.value)}
                placeholder="git@github.com:org/repo.git"
                spellCheck={false}
                required
              />
            </label>
            <div className="row-2">
              <label className="field">
                <span>Branch</span>
                <input
                  value={branch}
                  onChange={(e) => setBranch(e.target.value)}
                  placeholder={name ? `otterd/${name}` : "existing, or a new one"}
                  spellCheck={false}
                />
              </label>
              <label className="field">
                <span>Start from</span>
                <input value={base} onChange={(e) => setBase(e.target.value)} placeholder="default branch" spellCheck={false} />
              </label>
            </div>
          </>
        )}
        {source === "directory" && (
          <label className="field">
            <span>Folder on {host}</span>
            <input value={dir} onChange={(e) => setDir(e.target.value)} placeholder="~/src/project" spellCheck={false} required />
          </label>
        )}

        <fieldset className="field">
          <legend>Start</legend>
          <div className="segmented">
            {[...AGENTS, ["none", "No agent"] as [string, string]].map(([id, label]) => {
              const missing = id !== "none" && !available(id);
              return (
                <label
                  key={id}
                  className={chosen === id ? "on" : missing ? "off" : ""}
                  title={missing ? `${label} isn’t installed on ${host}` : undefined}
                >
                  <input type="radio" name="agent" checked={chosen === id} disabled={missing} onChange={() => setAgent(id)} />
                  {label}
                </label>
              );
            })}
          </div>
          {AGENTS.some(([id]) => !available(id)) && (
            <span className="muted small">
              Not installed on {host}:{" "}
              {AGENTS.filter(([id]) => !available(id))
                .map(([, label]) => label)
                .join(", ")}
            </span>
          )}
          {chosen !== "none" && (
            <textarea
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              placeholder={`What should ${AGENTS.find(([id]) => id === chosen)?.[1]} do? (optional)`}
              rows={3}
            />
          )}
          <label className="check">
            <input type="checkbox" checked={shell} onChange={(e) => setShell(e.target.checked)} />
            Shell
          </label>
        </fieldset>

        {error && <div className="notice error">{error}</div>}
        <div className="form-actions">
          {source === "git" && <span className="muted small">The repository is cloned on {host}; the workspace opens while it prepares.</span>}
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={busy || !name.trim()}>
            {busy ? "Creating…" : "Create"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}
