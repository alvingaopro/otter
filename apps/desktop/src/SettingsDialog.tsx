import { useEffect, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";

/** Mirrors `otter_protocol::host::ControllerInfo` (D-052). */
export interface ControllerInfo {
  id: string;
  label: string;
  /** The secret holding its API key. */
  secret?: string;
  /** Whether it asks a model, so there are models to choose from. */
  models: boolean;
  /** Used when no model is chosen; absent: one must be chosen. */
  default_model?: string;
}

/** Mirrors `otter_protocol::host::Settings` (D-048). */
export interface HostSettings {
  controller?: string;
  model?: string;
  controller_from_env?: string;
  secrets: { name: string; purpose: string; set: boolean }[];
  /** Absent from an otterd older than D-052. */
  controllers?: ControllerInfo[];
  /** What runs now: what "automatic" chose, or the environment's. */
  active?: string;
}

/** Mirrors `otter_protocol::host::ModelInfo`. */
export interface ModelInfo {
  id: string;
  name?: string;
}

/** For an otterd that doesn't list its controllers. */
const OLD_CONTROLLERS: ControllerInfo[] = [
  { id: "openrouter", label: "OpenRouter", secret: "OPENROUTER_API_KEY", models: true, default_model: "openrouter/auto" },
  { id: "claude", label: "Claude Code (its own sign-in on the host)", models: true },
  { id: "rules", label: "No model: every decision goes to you", models: false },
  { id: "off", label: "Off: features don't run on their own", models: false },
];

type Models = { state: "loading" } | { state: "ready"; list: ModelInfo[] } | { state: "error"; error: string };

/**
 * Settings of one host's Control Agent: which model thinks, and API keys.
 * Keys are sent to that host's otterd and kept there (0600); this app never
 * reads them back, it only knows whether one is set. The models offered are
 * what the provider lists, asked by the host with its key.
 */
export function SettingsDialog({ hosts, defaultHost, onClose }: { hosts: string[]; defaultHost?: string; onClose: () => void }) {
  const [host, setHost] = useState(hosts.includes(defaultHost ?? "") ? defaultHost! : hosts[0]);
  const [settings, setSettings] = useState<HostSettings | null>(null);
  const [controller, setController] = useState("");
  const [model, setModel] = useState("");
  const [keys, setKeys] = useState<Record<string, string>>({});
  const [models, setModels] = useState<Models | null>(null);
  const [reload, setReload] = useState(0);
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

  const controllers = settings?.controllers?.length ? settings.controllers : OLD_CONTROLLERS;
  // The one the model and key below are for: the choice, else what automatic uses.
  const effective = controller || settings?.active || "";
  const info = controllers.find((c) => c.id === effective);
  const keySet = !!settings?.secrets.find((s) => s.name === info?.secret)?.set;
  const listsModels = !!info?.models && !!settings?.controllers?.length;

  useEffect(() => {
    if (!host || !listsModels) {
      setModels(null);
      return;
    }
    let live = true;
    setModels({ state: "loading" });
    invoke<ModelInfo[]>("settings_models", { host, controller: effective })
      .then((list) => live && setModels({ state: "ready", list }))
      .catch((e) => live && setModels({ state: "error", error: String(e) }));
    return () => {
      live = false;
    };
  }, [host, effective, listsModels, keySet, reload]);

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

  function choose(id: string) {
    setController(id);
    // A model belongs to its provider: keep the saved one only for its own.
    setModel(id === (settings?.controller ?? "") ? (settings?.model ?? "") : "");
    setSaved(false);
  }

  const needsModel = !!info?.models && !info.default_model && info.id !== "claude" && !model.trim();

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

  const label = (id: string) => controllers.find((c) => c.id === id)?.label ?? id;
  const keyField = (s: HostSettings["secrets"][number]) => (
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
  );
  const mainKey = settings?.secrets.find((s) => s.name === info?.secret);
  const otherKeys = settings?.secrets.filter((s) => s !== mainKey) ?? [];
  const chosen = models?.state === "ready" ? models.list.find((m) => m.id === model.trim()) : undefined;

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
              <select value={controller} onChange={(e) => choose(e.target.value)} aria-label="Control Agent">
                <option value="">
                  Automatic{settings.active ? ` (now: ${label(settings.active)})` : " (the first provider with a key, else Claude Code)"}
                </option>
                {controllers.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.label}
                  </option>
                ))}
              </select>
            </label>
            {settings.controller_from_env && (
              <p className="notice small">
                otterd on {host} runs with OTTER_CONTROLLER={settings.controller_from_env}, which overrides this.
              </p>
            )}
            {mainKey && keyField(mainKey)}
            {info?.models && (
              <label className="field">
                <span>Model{!controller && info ? ` for ${info.label}` : ""}</span>
                <div className="key-row">
                  <input
                    value={model}
                    onChange={(e) => setModel(e.target.value)}
                    list="settings-models"
                    aria-label="Model"
                    placeholder={
                      info.default_model
                        ? `Default: ${info.default_model}`
                        : info.id === "claude"
                          ? "Default: Claude Code's"
                          : "Choose a model"
                    }
                    spellCheck={false}
                    autoComplete="off"
                  />
                  {listsModels && (
                    <button type="button" className="btn outline" disabled={models?.state === "loading"} onClick={() => setReload((n) => n + 1)}>
                      Refresh
                    </button>
                  )}
                </div>
                <datalist id="settings-models">
                  {models?.state === "ready" &&
                    models.list.map((m) => (
                      <option key={m.id} value={m.id}>
                        {m.name ?? m.id}
                      </option>
                    ))}
                </datalist>
                <span className="muted small" aria-live="polite">
                  {models?.state === "loading" && "Loading models…"}
                  {models?.state === "ready" &&
                    (chosen?.name ? chosen.name : `${models.list.length} models: type to search, or enter any model name.`)}
                  {models?.state === "error" && `Couldn't list models: ${models.error}`}
                  {!models && !listsModels && "Any model name the provider accepts."}
                </span>
              </label>
            )}
            {needsModel && <p className="notice small">{info?.label} has no default model: choose one.</p>}
            {otherKeys.length > 0 && (
              <details className="field">
                <summary className="muted small">
                  Other API keys ({otherKeys.filter((s) => s.set).length} set)
                </summary>
                {otherKeys.map(keyField)}
              </details>
            )}
          </>
        )}
        {error && <p className="error-text small">{error}</p>}
        {saved && <p className="muted small">Saved on {host}.</p>}
        <div className="form-actions">
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Close
          </button>
          <button type="submit" className="btn primary" disabled={!settings || busy || (needsModel && !!controller)}>
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
      {/* A gear (not a sun: that reads as a theme switch). */}
      <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
        <circle cx="12" cy="12" r="3" />
        <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
      </svg>
    </button>
  );
}
