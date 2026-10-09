import { useEffect, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";

/** Mirrors `otter_protocol::host::Settings` (D-048). */
export interface HostSettings {
  controller?: string;
  model?: string;
  controller_from_env?: string;
  secrets: { name: string; purpose: string; set: boolean }[];
}

const CONTROLLERS: [string, string][] = [
  ["", "Automatic (OpenRouter if a key is set, else Claude Code)"],
  ["claude", "Claude Code (its own sign-in on the host)"],
  ["openrouter", "OpenRouter (API key below)"],
  ["rules", "No model: every decision goes to you"],
  ["off", "Off: features don't run on their own"],
];

const MODEL_HINT: Record<string, string> = {
  claude: "e.g. sonnet or opus (blank: Claude Code's default)",
  openrouter: "e.g. anthropic/claude-sonnet-4.5 (blank: openrouter/auto)",
};

/**
 * Settings of one host's Control Agent: which model thinks, and API keys.
 * Keys are sent to that host's otterd and kept there (0600); this app never
 * reads them back, it only knows whether one is set.
 */
export function SettingsDialog({ hosts, defaultHost, onClose }: { hosts: string[]; defaultHost?: string; onClose: () => void }) {
  const [host, setHost] = useState(hosts.includes(defaultHost ?? "") ? defaultHost! : hosts[0]);
  const [settings, setSettings] = useState<HostSettings | null>(null);
  const [controller, setController] = useState("");
  const [model, setModel] = useState("");
  const [keys, setKeys] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!host) return;
    let live = true;
    setSettings(null);
    setError(null);
    setKeys({});
    invoke<HostSettings>("settings_get", { host })
      .then((s) => {
        if (!live) return;
        setSettings(s);
        setController(s.controller ?? "");
        setModel(s.model ?? "");
      })
      .catch((e) => live && setError(`${e} (an otterd older than this app has no settings)`));
    return () => {
      live = false;
    };
  }, [host]);

  async function save(update: { controller?: string; model?: string; secrets?: Record<string, string | null> }) {
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      const s = await invoke<HostSettings>("settings_set", { host, update });
      setSettings(s);
      setKeys({});
      setSaved(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  function submit(e: FormEvent) {
    e.preventDefault();
    const secrets: Record<string, string> = {};
    for (const [name, value] of Object.entries(keys)) if (value.trim()) secrets[name] = value.trim();
    void save({ controller, model, secrets });
  }

  if (hosts.length === 0) {
    return (
      <Dialog title="Settings" onClose={onClose}>
        <p className="muted">Connect a host first: settings belong to the host whose Control Agent uses them.</p>
      </Dialog>
    );
  }

  return (
    <Dialog title="Settings" onClose={onClose}>
      <form className="form" onSubmit={submit}>
        {hosts.length > 1 && (
          <label className="field">
            <span>Host</span>
            <select value={host} onChange={(e) => setHost(e.target.value)}>
              {hosts.map((h) => (
                <option key={h}>{h}</option>
              ))}
            </select>
          </label>
        )}
        {!settings && !error && <p className="muted small">Loading…</p>}
        {settings && (
          <>
            <label className="field">
              <span>Control Agent</span>
              <select value={controller} onChange={(e) => setController(e.target.value)} aria-label="Control Agent">
                {CONTROLLERS.map(([id, label]) => (
                  <option key={id} value={id}>
                    {label}
                  </option>
                ))}
              </select>
            </label>
            {settings.controller_from_env && (
              <p className="notice small">
                otterd on {host} runs with OTTER_CONTROLLER={settings.controller_from_env}, which overrides this.
              </p>
            )}
            {(controller === "claude" || controller === "openrouter" || controller === "") && (
              <label className="field">
                <span>Model</span>
                <input
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                  placeholder={MODEL_HINT[controller] ?? "blank: the default"}
                  spellCheck={false}
                />
              </label>
            )}
            {settings.secrets.map((s) => (
              <fieldset key={s.name} className="field">
                <legend>
                  {s.name} <span className={s.set ? "key-state set" : "key-state"}>{s.set ? "set" : "not set"}</span>
                </legend>
                <span className="muted small">{s.purpose}. Kept on {host}; it can't be read back.</span>
                <div className="key-row">
                  <input
                    type="password"
                    autoComplete="off"
                    spellCheck={false}
                    aria-label={s.name}
                    placeholder={s.set ? "Replace the key…" : "Paste the key"}
                    value={keys[s.name] ?? ""}
                    onChange={(e) => setKeys((k) => ({ ...k, [s.name]: e.target.value }))}
                  />
                  {s.set && (
                    <button type="button" className="btn outline" disabled={busy} onClick={() => void save({ secrets: { [s.name]: null } })}>
                      Clear
                    </button>
                  )}
                </div>
              </fieldset>
            ))}
          </>
        )}
        {error && <p className="error-text small">{error}</p>}
        {saved && <p className="muted small">Saved on {host}.</p>}
        <div className="form-actions">
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Close
          </button>
          <button type="submit" className="btn primary" disabled={!settings || busy}>
            Save
          </button>
        </div>
      </form>
    </Dialog>
  );
}

/** The gear that opens Settings. */
export function SettingsButton({ onClick }: { onClick: () => void }) {
  return (
    <button className="icon-btn" aria-label="Settings" title="Settings: Control Agent and API keys" onClick={onClick}>
      <svg width="15" height="15" viewBox="0 0 18 18" aria-hidden="true">
        <circle cx="9" cy="9" r="2.4" fill="none" stroke="currentColor" strokeWidth="1.5" />
        <path
          d="M9 1.8v2M9 14.2v2M1.8 9h2M14.2 9h2M3.9 3.9l1.4 1.4M12.7 12.7l1.4 1.4M3.9 14.1l1.4-1.4M12.7 5.3l1.4-1.4"
          stroke="currentColor"
          strokeWidth="1.5"
          strokeLinecap="round"
        />
      </svg>
    </button>
  );
}
