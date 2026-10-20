import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";
import type { RuntimeCapabilities, RuntimeTest } from "./conversation";
import { explain as needsNewer } from "./newer";
import { GitSettings } from "./GitSettings";
import { SignInSites } from "./SignIn";
import { FONT_SUGGESTIONS, fontChoice, fontFamily, fontSize, onFontSize, setFontFamily, setFontSize } from "./terminalFont";
import { onPerfOverlay, perfOverlay, setPerfOverlay } from "./perf";
import type { ThemeChoice } from "./theme";

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

/** How the coding agent can run on a host (D-057): the SDK first, the default (D-124). */
const BACKENDS: { id: string; name: string; hint: string }[] = [
  { id: "sdk", name: "Agent SDK", hint: "The Claude Agent SDK, the default. Needs Node.js on the host." },
  { id: "legacy_cli", name: "Claude Code CLI", hint: "The claude command on the host." },
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
type Test = { state: "running" } | { state: "done"; result: RuntimeTest } | { state: "error"; error: string };
type Install = { state: "running" } | { state: "done"; output: string } | { state: "error"; error: string };

/** The tabs (D-127): this Mac's, then the chosen host's. */
export type SettingsTab = "appearance" | "signin" | "developer" | "git" | "otter" | "coding" | "keys";
const TABS: SettingsTab[] = ["appearance", "signin", "developer", "git", "otter", "coding", "keys"];
const HOST_TABS: SettingsTab[] = ["otter", "coding", "keys"];
const TAB_KEY = "otter.settings-tab";

function storedTab(): SettingsTab {
  try {
    const v = localStorage.getItem(TAB_KEY) as SettingsTab | null;
    if (v && TABS.includes(v)) return v;
  } catch {
    // Storage unavailable: the first tab.
  }
  return "appearance";
}

/**
 * Settings, in tabs by kind (D-127): this Mac's (applied as they change,
 * kept here) and the chosen host's (saved there, with Save). Keys are sent
 * to that host's otterd and kept there (0600); this app never reads them
 * back, it only knows whether one is set. The models offered are what the
 * provider lists, asked by the host with its key.
 */
export function SettingsDialog({
  hosts,
  defaultHost,
  theme,
  onTheme,
  onAddHost,
  version,
  onClose,
}: {
  hosts: string[];
  defaultHost?: string;
  theme: ThemeChoice;
  onTheme: (t: ThemeChoice) => void;
  onAddHost?: () => void;
  version?: string;
  onClose: () => void;
}) {
  const [tab, setTab] = useState<SettingsTab>(storedTab);
  const [host, setHost] = useState(hosts.includes(defaultHost ?? "") ? defaultHost! : hosts[0]);
  const [settings, setSettings] = useState<HostSettings | null>(null);
  const [controller, setController] = useState("");
  const [model, setModel] = useState("");
  const [keys, setKeys] = useState<Record<string, string>>({});
  // The key whose field is open: to replace one that's set (Replace…), or on
  // the API keys tab to add one (Add…).
  const [replacing, setReplacing] = useState<string | null>(null);
  const [models, setModels] = useState<Models | null>(null);
  const [reload, setReload] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);
  // The coding agent's own settings, apart from Otter's (D-057).
  const [backend, setBackend] = useState("");
  const [codingModel, setCodingModel] = useState("");
  const [caps, setCaps] = useState<RuntimeCapabilities | null>(null);
  const [test, setTest] = useState<Test | null>(null);
  // Installing the Claude worker the host's otterd looks for (D-129).
  const [install, setInstall] = useState<Install | null>(null);
  const [recheck, setRecheck] = useState(0);
  // Otter answers with its tools (D-089): tried out per host.
  const [otterTools, setOtterTools] = useState(false);

  function go(t: SettingsTab) {
    setTab(t);
    try {
      localStorage.setItem(TAB_KEY, t);
    } catch {
      // Not remembered; fine.
    }
  }

  // The form as the host has it.
  function take(s: HostSettings) {
    setController(s.controller ?? "");
    setModel(s.model ?? "");
    setBackend(s.coding?.backend ?? "");
    setCodingModel(s.coding?.model ?? "");
    setOtterTools(s.otter_tools ?? false);
    setKeys({});
    setReplacing(null);
  }

  useEffect(() => {
    if (!host) return;
    let live = true;
    setSettings(null);
    setError(null);
    setSaved(false);
    setTest(null);
    setInstall(null);
    setKeys({});
    invoke<HostSettings>("settings_get", { host })
      .then((s) => {
        if (!live) return;
        setSettings(s);
        take(s);
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
  }, [host, settings, recheck]);

  // What's on screen now, for answers that come back after it changed.
  const now = useRef({ host, backend, codingModel });
  now.current = { host, backend, codingModel };
  // A test is of what was on screen; changed, it goes.
  useEffect(() => setTest(null), [backend, codingModel]);
  // Closing with changes not saved asks first (once).
  const [leaving, setLeaving] = useState(false);
  // The git identity edited but not saved (D-133).
  const [gitUnsaved, setGitUnsaved] = useState(false);

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
    const at = host;
    const sent = Object.keys(update.secrets ?? {});
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      const s = await invoke<HostSettings>("settings_set", { host: at, update });
      // Another host was chosen meanwhile: this answer isn't its.
      if (now.current.host !== at) return;
      setSettings(s);
      // The keys sent are gone from the form; others typed stay.
      setKeys((k) => Object.fromEntries(Object.entries(k).filter(([name]) => !sent.includes(name))));
      setReplacing((r) => (r && sent.includes(r) ? null : r));
      setSaved(true);
    } catch (e) {
      if (now.current.host === at) setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  function choose(id: string) {
    setController(id);
    // A model belongs to its provider: keep the saved one only for its own.
    setModel(id === (settings?.controller ?? "") ? (settings?.model ?? "") : "");
  }

  const needsModel = !!info?.models && !info.default_model && info.id !== "claude" && !model.trim();
  const codingSaved = settings?.coding?.backend;
  const toolsShown = settings?.otter_tools !== undefined && !settings.otter_tools_from_env;
  const typed = Object.entries(keys).filter(([, v]) => v.trim());
  const dirty =
    !!settings &&
    (controller !== (settings.controller ?? "") ||
      model !== (settings.model ?? "") ||
      typed.length > 0 ||
      (!!codingSaved && (backend !== codingSaved || codingModel !== (settings.coding?.model ?? ""))) ||
      (toolsShown && otterTools !== settings.otter_tools));
  useEffect(() => {
    if (!dirty && !gitUnsaved) setLeaving(false);
  }, [dirty, gitUnsaved]);

  function submit() {
    if (!settings || busy || (needsModel && !!controller)) return;
    const secrets: Record<string, string> = {};
    for (const [name, value] of typed) secrets[name] = value.trim();
    // Only what changed of the coding agent's: an unchanged backend stays
    // the host's default rather than becoming a choice.
    const coding: { backend?: string; model?: string } = {};
    if (codingSaved) {
      if (backend !== codingSaved) coding.backend = backend;
      if (codingModel !== (settings.coding?.model ?? "")) coding.model = codingModel;
    }
    void save({
      controller,
      model,
      secrets,
      ...(Object.keys(coding).length ? { coding } : {}),
      // Only where the host has it, and the environment doesn't decide.
      ...(toolsShown && otterTools !== settings.otter_tools ? { otter_tools: otterTools } : {}),
    });
  }

  async function runTest() {
    const asked = { host, backend, codingModel };
    const same = () => now.current.host === asked.host && now.current.backend === asked.backend && now.current.codingModel === asked.codingModel;
    setTest({ state: "running" });
    try {
      const result = await invoke<RuntimeTest>("runtime_test", { host, backend, model: codingModel });
      // Of what was on screen then: shown only while it still is.
      if (same()) setTest({ state: "done", result });
    } catch (e) {
      if (same()) setTest({ state: "error", error: needsNewer(host, e) });
    }
  }

  async function installWorker() {
    const at = host;
    setInstall({ state: "running" });
    setTest(null);
    try {
      const output = await invoke<string>("host_install_worker", { name: at });
      if (now.current.host !== at) return;
      setInstall({ state: "done", output });
    } catch (e) {
      if (now.current.host === at) setInstall({ state: "error", error: String(e) });
    }
    // Whatever came of it, ask the host again.
    setRecheck((n) => n + 1);
  }

  function close() {
    if ((dirty || gitUnsaved) && !leaving) return setLeaving(true);
    onClose();
  }

  const label = (id: string) => controllers.find((c) => c.id === id)?.label ?? id;
  const keyInput = (s: HostSettings["secrets"][number], focus = false) => (
    <input
      autoFocus={focus}
      type="password"
      className="settings-input"
      autoComplete="off"
      spellCheck={false}
      aria-label={s.name}
      placeholder={s.set ? "Paste the new key" : "Paste the key"}
      value={keys[s.name] ?? ""}
      onChange={(e) => setKeys((k) => ({ ...k, [s.name]: e.target.value }))}
    />
  );
  const mainKey = settings?.secrets?.find((s) => s.name === info?.secret);
  const chosen = models?.state === "ready" ? models.list.find((m) => m.id === model.trim()) : undefined;
  const coding = codingSaved ? caps : null;
  const tested = test?.state === "done" ? test.result : null;
  // What the host says of the backend on screen (it says it of the saved one).
  const capsHere = coding && coding.backend === backend ? coding : null;
  // Installing the worker would help: as the host says, or as a test found
  // of a backend not saved yet (D-129).
  const offerInstall = !!capsHere?.worker_missing || (!!tested?.worker_missing && tested.backend === backend);
  const hostTab = HOST_TABS.includes(tab);
  // An older host has no coding settings: its tab goes.
  const shown = tab === "coding" && settings && !codingSaved ? "otter" : tab;

  const heads: Record<SettingsTab, [string, string]> = {
    appearance: ["Appearance", "On this Mac. Changes apply right away."],
    signin: ["Sign-in pages", "Pages that tools on your hosts open in this Mac's browser. Kept on this Mac, for every host."],
    developer: ["Developer", "On this Mac. For working on the app itself; changes apply right away."],
    git: ["Git & GitHub", "For every host, in the repositories Otter manages there; each host's own git stays as it is."],
    otter: ["Otter", host ? `On ${host}. Plans each task, decides, and checks the work, also while this app is closed.` : "Kept on each host."],
    coding: ["Coding agent", host ? `On ${host}. Claude Code, which writes the code.` : "Kept on each host."],
    keys: ["API keys", host ? `On ${host}. Kept there, never read back.` : "Kept on each host."],
  };
  const [title, caption] = heads[shown];

  const nav = (id: SettingsTab, name: string, icon: ReactNode, extra?: ReactNode) => (
    <button
      type="button"
      className={shown === id ? "settings-nav-item on" : "settings-nav-item"}
      aria-current={shown === id ? "page" : undefined}
      data-autofocus={shown === id ? "" : undefined}
      onClick={() => go(id)}
    >
      {icon}
      <span>{name}</span>
      {extra}
    </button>
  );

  return (
    <Dialog title="Settings" onClose={close} bare="settings-dialog">
      <nav className="settings-nav" aria-label="Settings">
        <h1 className="settings-nav-title">Settings</h1>
        <div className="settings-nav-group">This Mac</div>
        {nav("appearance", "Appearance", <Icon d="appearance" />)}
        {nav("signin", "Sign-in pages", <Icon d="signin" />)}
        {nav("developer", "Developer", <Icon d="developer" />)}
        <div className="settings-nav-group">All hosts</div>
        {nav("git", "Git & GitHub", <Icon d="git" />)}
        <div className="settings-nav-group">Host</div>
        <div className="settings-host">
          <span className={host ? "status-dot ok" : "status-dot"} aria-hidden="true" />
          {hosts.length > 1 ? (
            <select value={host} onChange={(e) => setHost(e.target.value)} aria-label="Host">
              {hosts.map((h) => (
                <option key={h}>{h}</option>
              ))}
            </select>
          ) : (
            <span className="settings-host-name">{host ?? "No host connected"}</span>
          )}
        </div>
        {nav("otter", "Otter", <Icon d="otter" />)}
        {(!settings || codingSaved) && nav("coding", "Coding agent", <Icon d="coding" />)}
        {nav(
          "keys",
          "API keys",
          <Icon d="keys" />,
          settings?.secrets && <span className="settings-nav-count">{settings.secrets.filter((s) => s.set).length} set</span>,
        )}
        <span className="spacer" />
        {version && <span className="settings-version">Otter {version}</span>}
      </nav>

      <section className="settings-main">
        <header className="settings-head">
          <div>
            <h2>{title}</h2>
            <p>{caption}</p>
          </div>
          <button type="button" className="icon-btn settings-close" aria-label="Close" title="Close (Esc)" onClick={close}>
            <Icon d="close" />
          </button>
        </header>

        <div className="settings-body">
          {shown === "appearance" && <Appearance theme={theme} onTheme={onTheme} />}
          {shown === "signin" && <SignInSites />}
          {shown === "developer" && <Developer />}
          {shown === "git" && <GitSettings hosts={hosts} onUnsaved={setGitUnsaved} />}
          {hostTab && !host && (
            <div className="settings-empty">
              <strong>No host connected</strong>
              <p className="muted small">Otter's settings live on a host, and it works there while this app is closed. Connect one to change them.</p>
              {onAddHost && (
                <button type="button" className="btn primary" onClick={onAddHost}>
                  Add a host…
                </button>
              )}
            </div>
          )}
          {hostTab && host && !settings && !error && <p className="muted small">Loading…</p>}
          {hostTab && settings && (
            <form
              className="settings-form"
              onSubmit={(e) => {
                e.preventDefault();
                submit();
              }}
            >
              {shown === "otter" && (
                <>
                  <Group title="Model">
                    <Row label="Provider" hint={explain(effective, info, !controller)}>
                      <select className="settings-input" value={controller} onChange={(e) => choose(e.target.value)} aria-label="Model provider">
                        <option value="">{settings.active ? `Automatic — ${short(label(settings.active))}` : "Automatic"}</option>
                        {controllers.map((c) => (
                          <option key={c.id} value={c.id}>
                            {short(c.label)}
                          </option>
                        ))}
                      </select>
                    </Row>
                    {mainKey && (
                      <Row label="API key" hint={<span className="mono">{mainKey.name}</span>}>
                        {mainKey.set && replacing !== mainKey.name ? (
                          <div className="settings-inline">
                            <span className="key-state set">Set</span>
                            <button type="button" className="btn outline" onClick={() => setReplacing(mainKey.name)}>
                              Replace…
                            </button>
                          </div>
                        ) : (
                          keyInput(mainKey, replacing === mainKey.name)
                        )}
                      </Row>
                    )}
                    {info?.models && (
                      <Row
                        label="Model"
                        hint={
                          models?.state === "loading"
                            ? "Loading models…"
                            : models?.state === "ready"
                              ? (chosen?.name ?? `${models.list.length} models — type to search, or enter any name`)
                              : models?.state === "error"
                                ? `Couldn't list models: ${models.error}`
                                : "Any model name the provider accepts"
                        }
                      >
                        <div className="settings-inline">
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
                  </Group>
                  {settings.controller_from_env && (
                    <p className="notice small">
                      OTTER_CONTROLLER={settings.controller_from_env} in otterd's environment on {host} overrides this.
                    </p>
                  )}
                  {needsModel && !!controller && <p className="notice small">{short(info?.label ?? "")} has no default model: choose one.</p>}
                  {settings.otter_tools !== undefined && (
                    <Group title="Answers">
                      <Row
                        label="Look things up before answering"
                        hint={
                          settings.otter_tools_from_env
                            ? `Set by OTTER_BRAIN_V2=${settings.otter_tools_from_env} in otterd's environment.`
                            : `The task, its runs, what waits on you; never a shell. Needs Claude Code.${settings.otter_answers ? ` Now: ${settings.otter_answers}.` : ""}`
                        }
                      >
                        <input
                          type="checkbox"
                          role="switch"
                          className="switch"
                          aria-label="Otter answers with tools"
                          checked={otterTools}
                          disabled={!!settings.otter_tools_from_env}
                          onChange={(e) => setOtterTools(e.target.checked)}
                        />
                      </Row>
                    </Group>
                  )}
                </>
              )}

              {shown === "coding" && codingSaved && (
                <>
                  <Group title="Claude Code">
                    <Row label="Runs via" hint={BACKENDS.find((b) => b.id === backend)?.hint}>
                      <Segmented
                        label="Coding agent runs through"
                        name="backend"
                        value={backend}
                        options={BACKENDS.map((b) => [b.id, b.name])}
                        onChange={setBackend}
                      />
                    </Row>
                    <Row label="Model" hint="Empty: Claude Code's own default.">
                      <input
                        className="settings-input"
                        aria-label="Coding model"
                        value={codingModel}
                        onChange={(e) => setCodingModel(e.target.value)}
                        placeholder="opus, sonnet…"
                        spellCheck={false}
                        autoComplete="off"
                      />
                    </Row>
                    <Row
                      label="Status"
                      hint={
                        tested
                          ? tested.ready
                            ? "It's installed, signed in and answering."
                            : "It can't run coding work yet: see below."
                          : coding && coding.backend === backend
                            ? offerInstall
                              ? `The Claude worker isn't installed on ${host}. Install puts it there; otterd keeps running.`
                              : !coding.available && coding.notes?.[0]
                                ? coding.notes[0]
                                : "Run a test: one short reply, no tools, to check it's signed in."
                            : "Run a test to check this before you save it."
                      }
                    >
                      <div className="settings-inline">
                        {/* Installed, from what the host says; ready only once a test has run. */}
                        {(tested || (coding && coding.backend === backend)) && (
                          <span role="status" className="settings-status">
                            <span className={(tested ? tested.ready : coding!.available) ? "status-dot ok" : "status-dot bad"} aria-hidden="true" />
                            {tested ? (tested.ready ? "Ready" : "Not ready") : coding!.available ? "Installed" : "Not ready"}
                          </span>
                        )}
                        {offerInstall && (
                          <button
                            type="button"
                            className="btn primary"
                            disabled={install?.state === "running"}
                            onClick={() => void installWorker()}
                            title={`Install the Claude worker on ${host}; otterd keeps running`}
                          >
                            {install?.state === "running" ? "Installing…" : "Install"}
                          </button>
                        )}
                        <button
                          type="button"
                          className="btn outline"
                          disabled={test?.state === "running" || install?.state === "running"}
                          onClick={() => void runTest()}
                        >
                          {test?.state === "running" ? "Testing…" : "Run a test"}
                        </button>
                      </div>
                    </Row>
                    {install && <InstallResult install={install} host={host} caps={capsHere} />}
                    {test && <TestResult test={test} />}
                  </Group>
                  <p className="muted small">For work that starts after you save; work under way keeps how it started.</p>
                </>
              )}

              {shown === "keys" && (
                <>
                  <Group title="Model providers">
                    {(settings.secrets ?? []).map((k) => {
                      const open = replacing === k.name;
                      return (
                        <div key={k.name} className="settings-key">
                          <Row
                            label={
                              <>
                                {k.purpose}
                                {k.name === info?.secret && <span className="settings-tag">In use</span>}
                              </>
                            }
                            hint={<span className="mono">{k.name}</span>}
                          >
                            <div className="settings-inline">
                              <span className={k.set ? "key-state set" : "key-state"}>{k.set ? "Set" : "Not set"}</span>
                              {k.set && (
                                <button type="button" className="btn ghost" disabled={busy} onClick={() => void save({ secrets: { [k.name]: null } })}>
                                  Clear
                                </button>
                              )}
                              <button
                                type="button"
                                className="btn outline"
                                onClick={() => {
                                  setReplacing(open ? null : k.name);
                                  setKeys(({ [k.name]: _, ...rest }) => rest);
                                }}
                              >
                                {open ? "Cancel" : k.set ? "Replace…" : "Add…"}
                              </button>
                            </div>
                          </Row>
                          {open && <div className="settings-key-field">{keyInput(k, true)}</div>}
                        </div>
                      );
                    })}
                  </Group>
                  <p className="muted small">Automatic uses the first provider with a key. Keys go to {host} and stay there; this app only knows whether one is set.</p>
                </>
              )}
            </form>
          )}
          {error && <p className="error-text small">{error}</p>}
        </div>

        {dirty ? (
          <footer className={leaving ? "settings-foot leaving" : "settings-foot"}>
            <span className="status-dot unsaved" aria-hidden="true" />
            <span className="muted small" role={leaving ? "alert" : undefined}>
              {leaving ? `Close without saving the changes on ${host}?` : `Unsaved changes on ${host}`}
            </span>
            <span className="spacer" />
            {leaving && (
              <button type="button" className="btn ghost" onClick={() => setLeaving(false)}>
                Keep editing
              </button>
            )}
            <button
              type="button"
              className="btn outline"
              disabled={busy}
              onClick={() => (leaving ? onClose() : settings && take(settings))}
            >
              {leaving ? "Discard and close" : "Discard"}
            </button>
            <button type="button" className="btn primary" disabled={busy || (needsModel && !!controller)} onClick={submit}>
              Save
            </button>
          </footer>
        ) : leaving && gitUnsaved ? (
          // Only the git identity isn't saved: it has its own Save above.
          <footer className="settings-foot leaving">
            <span className="status-dot unsaved" aria-hidden="true" />
            <span className="muted small" role="alert">
              Close without saving your git identity?
            </span>
            <span className="spacer" />
            <button type="button" className="btn ghost" onClick={() => setLeaving(false)}>
              Keep editing
            </button>
            <button type="button" className="btn outline" onClick={onClose}>
              Discard and close
            </button>
          </footer>
        ) : (
          saved &&
          hostTab && (
            <footer className="settings-foot">
              <span className="muted small">Saved on {host}.</span>
            </footer>
          )
        )}
      </section>
    </Dialog>
  );
}

/** This Mac's look (D-116, D-127): kept here, applied as it changes. */
function Appearance({ theme, onTheme }: { theme: ThemeChoice; onTheme: (t: ThemeChoice) => void }) {
  const [font, setFont] = useState(fontChoice);
  const [size, setSize] = useState(fontSize);
  useEffect(() => onFontSize(setSize), []);
  return (
    <>
      <Group title="Theme">
        <Row label="Theme" hint="The app and its terminals.">
          <Segmented
            label="Theme"
            name="theme"
            value={theme}
            options={[
              ["system", "System"],
              ["light", "Light"],
              ["dark", "Dark"],
            ]}
            onChange={(t) => onTheme(t as ThemeChoice)}
          />
        </Row>
      </Group>
      <Group title="Terminal">
        <Row label="Font" hint="Any font on this Mac, or a list of them. Empty: JetBrains Mono, which comes with the app.">
          <input
            className="settings-input"
            list="terminal-fonts"
            aria-label="Terminal font"
            placeholder="JetBrains Mono"
            value={font}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            onChange={(e) => {
              setFont(e.target.value);
              setFontFamily(e.target.value);
            }}
          />
          <datalist id="terminal-fonts">
            {FONT_SUGGESTIONS.map((f) => (
              <option key={f} value={f} />
            ))}
          </datalist>
        </Row>
        <Row label="Text size" hint="⌘= and ⌘- in a terminal too; ⌘0 goes back to 13.">
          <div className="stepper">
            <button type="button" aria-label="Smaller" onClick={() => setFontSize(size - 1)}>
              −
            </button>
            <input
              type="number"
              aria-label="Terminal text size"
              min={9}
              max={24}
              value={size}
              onChange={(e) => e.target.value && setFontSize(Number(e.target.value))}
            />
            <button type="button" aria-label="Larger" onClick={() => setFontSize(size + 1)}>
              +
            </button>
          </div>
        </Row>
        <div className="settings-preview" aria-label="Preview" style={{ fontFamily: fontFamily(), fontSize: size }}>
          <div>
            <span className="settings-preview-path">~/otter</span> <span className="muted">main</span> $ otter attach api
          </div>
          <div className="muted">0O 1lI {"{} [] -> => != =="}</div>
        </div>
      </Group>
    </>
  );
}

/** How installing the Claude worker went (D-129). */
function InstallResult({ install, host, caps }: { install: Install; host: string; caps: RuntimeCapabilities | null }) {
  if (install.state === "running") {
    return (
      <p className="settings-test muted small" role="status">
        Installing the Claude worker on {host}…
      </p>
    );
  }
  if (install.state === "error") return <p className="settings-test error-text small">{install.error}</p>;
  // Found now: say so, and what else it needs. Not found, or the host can't
  // say of this backend before it's saved: what the install said.
  if (caps && !caps.worker_missing) {
    return (
      <p className="settings-test muted small" role="status">
        {caps.available ? "Installed. Run a test to check it's signed in." : `Installed. Still not ready: ${caps.notes?.[0] ?? "see above."}`}
      </p>
    );
  }
  return (
    <>
      <pre className="output settings-output">{install.output}</pre>
      {!caps && <p className="settings-test muted small">Run a test to check it.</p>}
    </>
  );
}

/** A test run's steps (D-124), or that it runs. */
function TestResult({ test }: { test: Test }) {
  if (test.state === "running") {
    return (
      <p className="settings-test muted small" role="status">
        Starting Claude Code for one short reply, up to two minutes…
      </p>
    );
  }
  if (test.state === "error") return <p className="settings-test error-text small">{test.error}</p>;
  return (
    <ul className="settings-test" aria-label="Test run">
      {test.result.steps.map((s) => (
        <li key={s.name} className={`test-step ${s.state}`}>
          <span className="test-mark" aria-label={s.state}>
            {s.state === "passed" ? "✓" : s.state === "failed" ? "✕" : "–"}
          </span>
          <span>
            <span className="test-name">{s.name}</span>
            <span className="muted small"> {s.detail}</span>
          </span>
        </li>
      ))}
    </ul>
  );
}

/** Choices side by side, one on (radios underneath). */
function Segmented({
  label,
  name,
  value,
  options,
  onChange,
}: {
  label: string;
  name: string;
  value: string;
  options: [string, string][];
  onChange: (v: string) => void;
}) {
  return (
    <div className="segmented" role="radiogroup" aria-label={label}>
      {options.map(([id, text]) => (
        <label key={id} className={value === id ? "on" : ""}>
          <input type="radio" name={name} value={id} checked={value === id} onChange={() => onChange(id)} />
          {text}
        </label>
      ))}
    </div>
  );
}

/** For working on the app itself (D-128): this Mac's, applied at once. */
function Developer() {
  const [overlay, setOverlay] = useState(perfOverlay);
  useEffect(() => onPerfOverlay(setOverlay), []);
  return (
    <Group title="Performance">
      <Row
        label="Performance overlay"
        hint="In the window's top-right corner: frames per second and the slowest frame, how late the main thread runs, the page's size, what the terminals receive, and the app process's memory and CPU. While it's off, nothing is measured."
      >
        <input
          type="checkbox"
          role="switch"
          className="switch"
          aria-label="Show the performance overlay"
          checked={overlay}
          onChange={(e) => setPerfOverlay(e.target.checked)}
        />
      </Row>
    </Group>
  );
}

/** Settings that belong together, in one card under a name. */
function Group({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="settings-group" aria-label={title}>
      <h3>{title}</h3>
      <div className="settings-card">{children}</div>
    </section>
  );
}

/** A setting: its name and a short hint on the left, the control on the right. */
function Row({ label, hint, children }: { label: ReactNode; hint?: ReactNode; children?: ReactNode }) {
  return (
    <div className="settings-row">
      <div className="settings-text">
        <div className="settings-label">{label}</div>
        {hint && <div className="settings-hint">{hint}</div>}
      </div>
      {children && <div className="settings-control">{children}</div>}
    </div>
  );
}

const ICONS: Record<string, ReactNode> = {
  appearance: (
    <>
      <circle cx="8" cy="8" r="5.8" />
      <path d="M8 2.2a5.8 5.8 0 0 1 0 11.6z" fill="currentColor" stroke="none" />
    </>
  ),
  signin: (
    <>
      <path d="M10 2.8h2a1.2 1.2 0 0 1 1.2 1.2v8a1.2 1.2 0 0 1-1.2 1.2h-2" />
      <path d="M6.5 5l3 3-3 3" />
      <path d="M9.5 8H2.8" />
    </>
  ),
  otter: <path d="M8 1.8v3.4M8 10.8v3.4M1.8 8h3.4M10.8 8h3.4M3.6 3.6l2.2 2.2M10.2 10.2l2.2 2.2M12.4 3.6l-2.2 2.2M5.8 10.2l-2.2 2.2" />,
  coding: (
    <>
      <path d="M5.5 4.5L2 8l3.5 3.5" />
      <path d="M10.5 4.5L14 8l-3.5 3.5" />
    </>
  ),
  keys: (
    <>
      <circle cx="5.5" cy="10.5" r="2.7" />
      <path d="M7.4 8.6L13 3" />
      <path d="M11 5l1.6 1.6" />
      <path d="M9.4 6.6L11 8.2" />
    </>
  ),
  developer: <path d="M1.8 8.5h2.6l1.8-4.3 3.2 7.6 1.8-3.3h3" />,
  git: (
    <>
      <circle cx="4.5" cy="3.8" r="1.6" />
      <circle cx="4.5" cy="12.2" r="1.6" />
      <circle cx="11.5" cy="6" r="1.6" />
      <path d="M4.5 5.4v5.2" />
      <path d="M11.5 7.6c0 2.4-2.2 3-5.6 3.9" />
    </>
  ),
  close: <path d="M4 4l8 8M12 4l-8 8" />,
};

function Icon({ d }: { d: keyof typeof ICONS }) {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      {ICONS[d]}
    </svg>
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
