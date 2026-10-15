import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";
import type { RuntimeCapabilities } from "./conversation";

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
  /** How the coding agent runs (D-057); without a backend from older daemons. */
  coding?: { backend?: string; model?: string };
  /** Otter answers with its tools (D-089); absent from older daemons. */
  otter_tools?: boolean;
  /** `OTTER_BRAIN_V2` in otterd's environment, which overrides the setting. */
  otter_tools_from_env?: string;
  /** How Otter answers on the host now: "with tools (…)", or "without tools…" and why. */
  otter_answers?: string;
}

/** How the coding agent can run on a host (D-057). */
const BACKENDS: [string, string][] = [
  ["legacy_cli", "Claude Code CLI"],
  ["sdk", "Claude Agent SDK (needs Node.js)"],
];

/** Mirrors `otter_protocol::host::ModelInfo`. */
export interface ModelInfo {
  id: string;
  name?: string;
}

/** For an otterd that doesn't list its controllers. */
const OLD_CONTROLLERS: ControllerInfo[] = [
  { id: "openrouter", label: "OpenRouter", secret: "OPENROUTER_API_KEY", models: true, default_model: "openrouter/auto" },
  { id: "claude", label: "Claude Code", models: true },
  { id: "rules", label: "No model", models: false },
  { id: "off", label: "Off", models: false },
];

type Models = { state: "loading" } | { state: "ready"; list: ModelInfo[] } | { state: "error"; error: string };

/**
 * Settings of Otter on one host: which model thinks, and API keys.
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
  // The coding agent's own settings, apart from Otter's (D-057).
  const [backend, setBackend] = useState("");
  const [codingModel, setCodingModel] = useState("");
  const [caps, setCaps] = useState<RuntimeCapabilities | null>(null);
  // Otter answers with its tools (D-089): tried out per host.
  const [otterTools, setOtterTools] = useState(false);

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
        setBackend(s.coding?.backend ?? "");
        setCodingModel(s.coding?.model ?? "");
        setOtterTools(s.otter_tools ?? false);
      })
      .catch((e) => live && setError(`${e} (an otterd older than this app has no settings)`));
    return () => {
      live = false;
    };
  }, [host]);

  // Whether the coding agent can run here, as the host says (after a save too).
  useEffect(() => {
    if (!host || !settings?.coding?.backend) return;
    let live = true;
    invoke<RuntimeCapabilities>("runtime_capabilities", { host })
      .then((c) => live && setCaps(c))
      .catch(() => live && setCaps(null));
    return () => {
      live = false;
    };
  }, [host, settings]);

  const controllers = settings?.controllers?.length ? settings.controllers : OLD_CONTROLLERS;
  // The one the model and key below are for: the choice, else what automatic uses.
  const effective = controller || settings?.active || "";
  const info = controllers.find((c) => c.id === effective);
  const keySet = !!settings?.secrets?.find((s) => s.name === info?.secret)?.set;
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

  async function save(update: {
    controller?: string;
    model?: string;
    secrets?: Record<string, string | null>;
    coding?: { backend?: string; model?: string };
    otter_tools?: boolean;
  }) {
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
    void save({
      controller,
      model,
      secrets,
      ...(settings?.coding?.backend ? { coding: { backend, model: codingModel } } : {}),
      // Only where the host has it, and the environment doesn't decide.
      ...(settings?.otter_tools !== undefined && !settings.otter_tools_from_env ? { otter_tools: otterTools } : {}),
    });
  }

  if (hosts.length === 0) {
    return (
      <Dialog title="Settings" onClose={onClose}>
        <p className="muted">Connect a host first: Otter runs on a host, and its settings are kept there.</p>
      </Dialog>
    );
  }

  const label = (id: string) => controllers.find((c) => c.id === id)?.label ?? id;
  const keyField = (s: HostSettings["secrets"][number]) => (
    <div key={s.name} className="settings-key">
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
      <span className="muted small">
        <span className={s.set ? "key-state set" : "key-state"}>{s.set ? "set" : "not set"}</span> {s.purpose}. Kept on{" "}
        {host}, never read back.
      </span>
    </div>
  );
  const mainKey = settings?.secrets?.find((s) => s.name === info?.secret);
  const otherKeys = settings?.secrets?.filter((s) => s !== mainKey) ?? [];
  const chosen = models?.state === "ready" ? models.list.find((m) => m.id === model.trim()) : undefined;

  const coding = settings?.coding?.backend ? caps : null;
  return (
    <Dialog title="Settings" onClose={onClose}>
      <form className="form settings" onSubmit={submit}>
        <Row label="Host" hint="Each host keeps its own settings; Otter works there while this app is closed.">
          {hosts.length > 1 ? (
            <select value={host} onChange={(e) => setHost(e.target.value)} aria-label="Host">
              {hosts.map((h) => (
                <option key={h}>{h}</option>
              ))}
            </select>
          ) : (
            <span className="settings-host">{host}</span>
          )}
        </Row>
        {!settings && !error && <p className="muted small">Loading…</p>}
        {settings && (
          <>
            <section className="settings-section" aria-label="Otter">
              <h2>
                Otter <span>plans each task, decides, and checks the work</span>
              </h2>
              <Row label="Model provider" hint={explain(effective, info, !controller)}>
                <select value={controller} onChange={(e) => choose(e.target.value)} aria-label="Model provider">
                  <option value="">
                    {settings.active ? `Automatic — ${short(label(settings.active))}` : "Automatic"}
                  </option>
                  {controllers.map((c) => (
                    <option key={c.id} value={c.id}>
                      {short(c.label)}
                    </option>
                  ))}
                </select>
              </Row>
              {settings.controller_from_env && (
                <p className="notice small">
                  OTTER_CONTROLLER={settings.controller_from_env} in otterd's environment on {host} overrides this.
                </p>
              )}
              {mainKey && <Row label="API key">{keyField(mainKey)}</Row>}
              {info?.models && (
                <Row
                  label="Model"
                  hint={
                    models?.state === "loading"
                      ? "Loading models…"
                      : models?.state === "ready"
                        ? chosen?.name ?? `${models.list.length} models — type to search, or enter any name`
                        : models?.state === "error"
                          ? `Couldn't list models: ${models.error}`
                          : "Any model name the provider accepts"
                  }
                >
                  <div className="key-row">
                    <ModelPicker
                      value={model}
                      onChange={setModel}
                      models={models?.state === "ready" ? models.list : []}
                      placeholder={info.default_model ? `Default: ${info.default_model}` : info.id === "claude" ? "Default" : "Choose a model"}
                    />
                    {listsModels && (
                      <button
                        type="button"
                        className="btn outline icon-only"
                        aria-label="Reload the models"
                        title="Reload the models"
                        disabled={models?.state === "loading"}
                        onClick={() => setReload((n) => n + 1)}
                      >
                        ↻
                      </button>
                    )}
                  </div>
                </Row>
              )}
              {needsModel && <p className="notice small">{short(info?.label ?? "")} has no default model: choose one.</p>}
              {settings.otter_tools !== undefined && (
                <Row
                  label="Tools"
                  hint={
                    settings.otter_tools_from_env
                      ? `Set by OTTER_BRAIN_V2=${settings.otter_tools_from_env} in otterd's environment.`
                      : settings.otter_answers
                        ? `Now: ${settings.otter_answers}.`
                        : undefined
                  }
                >
                  <label className="check">
                    <input
                      type="checkbox"
                      aria-label="Otter answers with tools"
                      checked={otterTools}
                      disabled={!!settings.otter_tools_from_env}
                      onChange={(e) => setOtterTools(e.target.checked)}
                    />
                    <span>
                      Look things up before answering
                      <span className="muted small"> — the task, its runs, what waits on you; never a shell. Needs Claude Code.</span>
                    </span>
                  </label>
                </Row>
              )}
            </section>
            {settings.coding?.backend && (
              <section className="settings-section" aria-label="Coding agent">
                <h2>
                  Coding agent <span>Claude Code, which writes the code — for work that starts after you save</span>
                </h2>
                <Row label="Runs via">
                  <select aria-label="Coding agent runs through" value={backend} onChange={(e) => setBackend(e.target.value)}>
                    {BACKENDS.map(([id, name]) => (
                      <option key={id} value={id}>
                        {name}
                      </option>
                    ))}
                  </select>
                </Row>
                <Row label="Model">
                  <input
                    aria-label="Coding model"
                    value={codingModel}
                    onChange={(e) => setCodingModel(e.target.value)}
                    placeholder="Default (e.g. opus, sonnet)"
                    spellCheck={false}
                    autoComplete="off"
                  />
                </Row>
                {coding && coding.backend === settings.coding.backend && (
                  <Row label="Status">
                    <div role="status" className="settings-status">
                      <span className={coding.available ? "status-dot ok" : "status-dot bad"} />
                      {coding.available ? "Ready" : "Not ready"}
                      {!coding.available && coding.notes?.[0] && <span className="muted"> — {coding.notes[0]}</span>}
                      {(coding.notes?.length ?? 0) > (coding.available ? 0 : 1) && (
                        <details className="settings-details">
                          <summary className="muted small">Details</summary>
                          <p className="muted small">{(coding.notes ?? []).slice(coding.available ? 0 : 1).join(" ")}</p>
                        </details>
                      )}
                    </div>
                  </Row>
                )}
              </section>
            )}
            {otherKeys.length > 0 && (
              <details className="settings-section settings-keys">
                <summary>
                  Other API keys <span className="muted small">({otherKeys.filter((s) => s.set).length} set)</span>
                </summary>
                <p className="muted small">
                  For switching providers without entering them again. Automatic uses the first one with a key.
                </p>
                {otherKeys.map((k) => (
                  <Row key={k.name} label={k.name}>
                    {keyField(k)}
                  </Row>
                ))}
              </details>
            )}
          </>
        )}
        {error && <p className="error-text small">{error}</p>}
        <div className="form-actions">
          {saved && <span className="muted small">Saved on {host}.</span>}
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

/** A setting: its name on the left, the control and a short hint on the right. */
function Row({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="settings-row">
      <span className="settings-label">{label}</span>
      <div className="settings-control field">
        {children}
        {hint && <span className="muted small">{hint}</span>}
      </div>
    </div>
  );
}

/** A provider's name without what an older otterd wrote after it. */
export function short(label: string): string {
  return label.replace(/\s*\(.*\)\s*$/, "").replace(/:.*$/, "");
}

/** What a choice of provider means, in a line. */
function explain(id: string, info: ControllerInfo | undefined, automatic: boolean): string {
  const how = !info
    ? "No key and no Claude Code: no model, every decision comes to you."
    : info.secret
      ? `Through ${short(info.label)}'s API, billed to the key below.`
      : id === "claude"
        ? "Claude Code's own sign-in on the host — no key needed."
        : id === "rules"
          ? "No model: every decision comes to you; a passing check counts as done."
          : id === "off"
            ? "Tasks don't move on their own."
            : "";
  return automatic ? `Automatic: the first provider with a key, else Claude Code. ${how}` : how;
}

/** At most this many matches are shown; typing narrows them. */
const SHOWN = 60;

/** Models matching every word typed, in id or name. */
export function matchModels(models: ModelInfo[], query: string): ModelInfo[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return models;
  return models.filter((m) => {
    const hay = `${m.id} ${m.name ?? ""}`.toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

/**
 * A model field with its own suggestion list: the web view's `<datalist>`
 * doesn't show one on macOS. Any text is still a valid value.
 */
function ModelPicker({
  value,
  onChange,
  models,
  placeholder,
}: {
  value: string;
  onChange: (v: string) => void;
  models: ModelInfo[];
  placeholder: string;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const list = useRef<HTMLUListElement>(null);
  // Showing the chosen model's whole list until the text is edited.
  const [query, setQuery] = useState<string | null>(null);
  const matches = matchModels(models, query ?? "");
  const shown = matches.slice(0, SHOWN);

  useEffect(() => {
    list.current?.children[active]?.scrollIntoView?.({ block: "nearest" });
  }, [active]);

  function pick(m: ModelInfo) {
    onChange(m.id);
    setQuery(null);
    setOpen(false);
  }

  function key(e: KeyboardEvent<HTMLInputElement>) {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!open) return setOpen(true);
      const n = shown.length;
      if (n) setActive((a) => (a + (e.key === "ArrowDown" ? 1 : n - 1)) % n);
    } else if (e.key === "Enter" && open && shown[active]) {
      e.preventDefault();
      pick(shown[active]);
    } else if (e.key === "Escape" && open) {
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
    }
  }

  return (
    <div className="combo">
      <input
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          setQuery(e.target.value);
          setActive(0);
          setOpen(true);
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onKeyDown={key}
        aria-label="Model"
        role="combobox"
        aria-expanded={open && shown.length > 0}
        aria-controls="model-options"
        aria-autocomplete="list"
        placeholder={placeholder}
        spellCheck={false}
        autoComplete="off"
      />
      {open && shown.length > 0 && (
        <ul className="combo-list" id="model-options" role="listbox" ref={list}>
          {shown.map((m, i) => (
            <li
              key={m.id}
              role="option"
              aria-selected={i === active}
              className={i === active ? "active" : undefined}
              // Before the input's blur closes the list.
              onMouseDown={(e) => {
                e.preventDefault();
                pick(m);
              }}
              onMouseEnter={() => setActive(i)}
            >
              <span className="combo-name">{m.name ?? m.id}</span>
              {m.name && <span className="combo-id">{m.id}</span>}
            </li>
          ))}
          {matches.length > SHOWN && (
            <li className="combo-more" aria-disabled="true">
              {matches.length - SHOWN} more: keep typing to narrow
            </li>
          )}
        </ul>
      )}
    </div>
  );
}

/** A gear: Settings (not a sun, which reads as a theme switch). */
export function GearIcon({ size = 18 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
    </svg>
  );
}
