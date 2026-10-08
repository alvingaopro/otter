import { useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";

interface Outcome {
  added: boolean;
  needsInstall: boolean;
  message: string;
}

/** Register a host: over SSH (anything `ssh` reaches) or this Mac. */
export function AddHostDialog({ version, onClose }: { version?: string; onClose: () => void }) {
  const [name, setName] = useState("");
  const [where, setWhere] = useState<"ssh" | "local">("ssh");
  const [destination, setDestination] = useState("");
  const [workdPath, setWorkdPath] = useState("");
  const [home, setHome] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [needsInstall, setNeedsInstall] = useState<string | null>(null);

  async function add(install: boolean, force: boolean) {
    setBusy(install ? `Installing workd ${version ?? ""} on ${name}…` : `Connecting to ${name}…`);
    setError(null);
    try {
      const out = await invoke<Outcome>("host_add", {
        name,
        destination: where === "ssh" ? destination || name : null,
        workdPath: workdPath || null,
        home: home || null,
        install,
        force,
      });
      if (out.added) onClose();
      else if (out.needsInstall) setNeedsInstall(out.message);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  function submit(e: FormEvent) {
    e.preventDefault();
    if (!busy && name.trim()) void add(false, false);
  }

  return (
    <Dialog title="Add a host" onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <label className="field">
          <span>Name</span>
          <input
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              setNeedsInstall(null);
            }}
            placeholder="dev-01"
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            required
          />
        </label>

        <fieldset className="field">
          <legend>Where</legend>
          <div className="segmented">
            <label className={where === "ssh" ? "on" : ""}>
              <input type="radio" name="where" checked={where === "ssh"} onChange={() => setWhere("ssh")} />
              Over SSH
            </label>
            <label className={where === "local" ? "on" : ""}>
              <input type="radio" name="where" checked={where === "local"} onChange={() => setWhere("local")} />
              This Mac
            </label>
          </div>
        </fieldset>

        {where === "ssh" && (
          <label className="field">
            <span>SSH destination</span>
            <input
              value={destination}
              onChange={(e) => setDestination(e.target.value)}
              placeholder={name ? `${name}  (as in ~/.ssh/config, or user@host)` : "a ~/.ssh/config host, or user@host"}
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
            />
          </label>
        )}

        <details className="advanced">
          <summary>Advanced</summary>
          <label className="field">
            <span>workd path on the host</span>
            <input
              value={workdPath}
              onChange={(e) => setWorkdPath(e.target.value)}
              placeholder="~/.local/bin/workd"
              spellCheck={false}
            />
          </label>
          <label className="field">
            <span>State directory on the host</span>
            <input value={home} onChange={(e) => setHome(e.target.value)} placeholder="~/.workd" spellCheck={false} />
          </label>
        </details>

        {needsInstall && (
          <div className="notice">
            <p>{needsInstall}</p>
            <p className="muted">
              Install workd {version} on {name}? It goes to ~/.local/bin; the host needs tmux 3.2+ and curl or wget.
            </p>
          </div>
        )}
        {error && <div className="notice error">{error}</div>}
        {busy && <div className="notice">{busy}</div>}

        <div className="form-actions">
          {needsInstall && (
            <button type="button" className="btn outline" disabled={!!busy} onClick={() => void add(false, true)}>
              Add without installing
            </button>
          )}
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Cancel
          </button>
          {needsInstall ? (
            <button type="button" className="btn primary" disabled={!!busy} onClick={() => void add(true, false)}>
              Install workd and add
            </button>
          ) : (
            <button type="submit" className="btn primary" disabled={!!busy || !name.trim()}>
              Add host
            </button>
          )}
        </div>
      </form>
    </Dialog>
  );
}
