// Settings › All hosts › Slack (D-208): one Slack app for all your hosts.
// One host, always on, runs otterhub: its otterd keeps it running and starts
// it with the machine. The hub holds the app's one connection and gives each
// host what is its own; the app's tokens and your member ID are set there,
// once. Each host still answers in Slack itself (D-206).

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { HostSettings } from "./SettingsDialog";

/** Mirrors `otter_protocol::host::HubView`. */
export interface HubView {
  enabled: boolean;
  name: string;
  users: string[];
  default_host?: string;
  /** Every host it serves, its own first. */
  hosts: string[];
  /** `off`, `starting`, `running` or `failed` (`detail` says why). */
  state: string;
  detail?: string;
  /** What the hub says of its connection to Slack. */
  slack?: string;
  /** What the hub says of each host it serves. */
  served?: { host: string; line: string }[];
  /** How its otterd starts with the machine, or why it doesn't. */
  boot?: string;
}

/** A host as this page knows it. */
type HostHub = { state: "loading" } | { state: "unreachable"; error: string } | { state: "ready"; settings: HostSettings };

/** How long Set up waits to see the hub connected to Slack. */
const SETUP_WAIT_MS = 45_000;
const STATES: Record<string, string> = { off: "Off", starting: "Starting…", running: "Running", failed: "Not running" };

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export function HubSettings({ hosts }: { hosts: string[] }) {
  const [views, setViews] = useState<Record<string, HostHub>>({});
  const [chosen, setChosen] = useState("");
  const [bot, setBot] = useState("");
  const [app, setApp] = useState("");
  const [members, setMembers] = useState("");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function load(host: string): Promise<HostSettings | null> {
    try {
      const settings = await invoke<HostSettings>("settings_get", { host });
      setViews((v) => ({ ...v, [host]: { state: "ready", settings } }));
      return settings;
    } catch (e) {
      setViews((v) => ({ ...v, [host]: { state: "unreachable", error: String(e) } }));
      return null;
    }
  }

  // (By the hosts' names: the list itself is made anew on each render.)
  const hostsKey = hosts.join("\n");
  useEffect(() => {
    for (const h of hostsKey.split("\n").filter(Boolean)) void load(h);
  }, [hostsKey]);

  const settingsOf = (h: string) => {
    const v = views[h];
    return v?.state === "ready" ? v.settings : undefined;
  };
  const runsOn = hosts.find((h) => settingsOf(h)?.hub?.enabled);
  const hub = runsOn ? settingsOf(runsOn)?.hub : undefined;
  // An otterd that can run the hub always says how it is (D-208).
  const able = hosts.filter((h) => settingsOf(h)?.hub !== undefined);
  const target = chosen && able.includes(chosen) ? chosen : (able[0] ?? "");
  const there = settingsOf(target);
  const tokensThere = (name: string) => there?.secrets.some((k) => k.name === name && k.set) ?? false;
  const membersThere = there?.slack?.users ?? [];

  // While it starts, or after it failed (otterd starts it again), look again.
  useEffect(() => {
    if (!runsOn || busy || hub?.state === "running") return;
    const t = setTimeout(() => void load(runsOn), 2000);
    return () => clearTimeout(t);
  }, [runsOn, busy, hub?.state, hub?.detail]);

  async function setUp() {
    if (!target) return;
    const users = (members.trim() ? members : membersThere.join(" ")).split(/[\s,]+/).filter(Boolean);
    const secrets: Record<string, string> = {};
    if (bot.trim()) secrets.SLACK_BOT_TOKEN = bot.trim();
    if (app.trim()) secrets.SLACK_APP_TOKEN = app.trim();
    setBusy(`Setting up on ${target}…`);
    setError(null);
    try {
      await invoke("settings_set", {
        host: target,
        update: { hub: { enabled: true, name: target, users, hosts: [] }, secrets },
      });
      setBot("");
      setApp("");
      const deadline = Date.now() + SETUP_WAIT_MS;
      for (;;) {
        const s = await load(target);
        const h = s?.hub;
        if ((h?.state === "running" && h.slack?.startsWith("connected")) || h?.state === "failed" || Date.now() > deadline) break;
        await sleep(1000);
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  async function stop() {
    if (!runsOn || !hub) return;
    setBusy(`Stopping on ${runsOn}…`);
    setError(null);
    try {
      await invoke("settings_set", {
        host: runsOn,
        update: { hub: { enabled: false, name: hub.name, users: hub.users, hosts: [] } },
      });
      await load(runsOn);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  if (hosts.length === 0) return <p className="muted small">No host connected.</p>;

  return (
    <>
      {runsOn && hub ? (
        <section className="settings-group" aria-label="Where it runs">
          <h3>Where it runs</h3>
          <div className="settings-card">
            <div className="settings-row">
              <div className="settings-text">
                <div className="settings-label">On {runsOn}</div>
                <div className="settings-hint">{hub.detail ?? hub.boot ?? "Its otterd keeps it running."}</div>
              </div>
              <div className="settings-control">
                <span className={`key-state${hub.state === "running" ? " set" : ""}`}>{STATES[hub.state] ?? hub.state}</span>
              </div>
            </div>
            {hub.slack && (
              <div className="settings-row">
                <div className="settings-text">
                  <div className="settings-label">Slack</div>
                  <div className="settings-hint">{hub.slack}</div>
                </div>
              </div>
            )}
            {hub.detail && hub.boot && (
              <div className="settings-row">
                <div className="settings-text">
                  <div className="settings-label">With the machine</div>
                  <div className="settings-hint">{hub.boot}</div>
                </div>
              </div>
            )}
          </div>
          <div className="git-setup">
            <button type="button" className="btn" disabled={!!busy} onClick={() => void stop()}>
              {busy ?? "Stop"}
            </button>
            <span className="muted small">To move it, stop it here, then set it up on the other host with the tokens again.</span>
          </div>
        </section>
      ) : (
        <section className="settings-group" aria-label="Set up">
          <h3>Set up</h3>
          {able.length === 0 ? (
            <p className="muted small">
              {hosts.some((h) => views[h]?.state === "loading") ? "Asking your hosts…" : "No connected host can run it yet: update otterd on one (Hosts › Update)."}
            </p>
          ) : (
            <div className="settings-card">
              <div className="settings-row">
                <div className="settings-text">
                  <div className="settings-label">Run it on</div>
                  <div className="settings-hint">A host that is always on, not a laptop: Slack hears nothing while it is off.</div>
                </div>
                <div className="settings-control">
                  <select aria-label="Hub host" value={target} onChange={(e) => setChosen(e.target.value)}>
                    {able.map((h) => (
                      <option key={h}>{h}</option>
                    ))}
                  </select>
                </div>
              </div>
              {(
                [
                  ["SLACK_BOT_TOKEN", "Bot token", "xoxb-… the Bot User OAuth Token, under Install App", bot, setBot],
                  ["SLACK_APP_TOKEN", "App token", "xapp-… an app-level token with connections:write", app, setApp],
                ] as const
              ).map(([name, label, placeholder, value, set]) => (
                <div className="settings-row" key={name}>
                  <div className="settings-text">
                    <div className="settings-label">{label}</div>
                    <div className="settings-hint">{tokensThere(name) ? `Set on ${target}: leave it empty to keep it.` : "Kept on that host, never read back."}</div>
                  </div>
                  <div className="settings-control">
                    <input
                      type="password"
                      className="settings-input"
                      aria-label={label}
                      placeholder={placeholder}
                      autoComplete="off"
                      value={value}
                      onChange={(e) => set(e.target.value)}
                    />
                  </div>
                </div>
              ))}
              <div className="settings-row">
                <div className="settings-text">
                  <div className="settings-label">Member ID</div>
                  <div className="settings-hint">Only your direct messages to the app are yours. In Slack: your profile › ⋯ › Copy member ID.</div>
                </div>
                <div className="settings-control">
                  <input
                    className="settings-input"
                    aria-label="Hub member ID"
                    placeholder={membersThere.join(" ") || "U…"}
                    spellCheck={false}
                    autoComplete="off"
                    value={members}
                    onChange={(e) => setMembers(e.target.value)}
                  />
                </div>
              </div>
            </div>
          )}
          {able.length > 0 && (
            <div className="git-setup">
              <button
                type="button"
                className="btn primary"
                disabled={
                  !!busy ||
                  (!bot.trim() && !tokensThere("SLACK_BOT_TOKEN")) ||
                  (!app.trim() && !tokensThere("SLACK_APP_TOKEN")) ||
                  (!members.trim() && membersThere.length === 0)
                }
                onClick={() => void setUp()}
              >
                {busy ?? `Set up on ${target}`}
              </button>
              <span className="muted small">Its otterd runs the hub from now on, and starts with the machine.</span>
            </div>
          )}
        </section>
      )}
      {error && (
        <p className="error-text small" role="alert">
          {error}
        </p>
      )}

      <section className="settings-group" aria-label="Hosts">
        <h3>Your hosts</h3>
        <div className="settings-card">
          {hosts.map((h) => (
            <div className="settings-row" key={h} aria-label={h}>
              <div className="settings-text">
                <div className="settings-label">{h}</div>
                <div className="settings-hint">{hostLine(h, views[h], hub, runsOn)}</div>
              </div>
            </div>
          ))}
        </div>
      </section>
      <p className="muted small">
        The hub holds the Slack app's one connection and gives each host what is its own; Otter on each host answers in Slack itself, as the
        same bot.
      </p>
    </>
  );
}

/** What a host is to the hub, in a line. */
function hostLine(host: string, v: HostHub | undefined, hub: HubView | undefined, runsOn: string | undefined): string {
  if (!v || v.state === "loading") return "Asking…";
  if (v.state === "unreachable") return `Not reached: ${v.error}`;
  const said = hub?.served?.find((s) => s.host === host)?.line;
  if (said) return said;
  if (v.settings.hub === undefined) return "Its otterd is too old for the hub: update it.";
  if (runsOn && !hub?.hosts.includes(host)) return `Not served yet: the hub on ${runsOn} reaches only its own machine for now.`;
  if (v.settings.slack?.hub) return "Takes Slack's events from the hub.";
  return "Not set up for Slack.";
}
