import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { Sidebar } from "./Sidebar";
import { WorkspacePane } from "./WorkspacePane";
import { defaultSession, groups, placeAll } from "./model";
import type { HostsPayload, Placed } from "./types";
import "./App.css";

/** Activating the app this soon after a notification jumps to its workspace. */
const JUMP_WINDOW_MS = 2 * 60 * 1000;

interface Jump {
  key: string;
  session?: string;
  at: number;
}

export default function App() {
  const [payload, setPayload] = useState<HostsPayload | null>(null);
  const [selected, setSelected] = useState<string | undefined>();
  const [sessions, setSessions] = useState<Record<string, string>>({});
  const [now, setNow] = useState(Date.now());
  const [version, setVersion] = useState<string | undefined>();

  const focused = useRef(document.hasFocus());
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  /** Attention ids already seen per host; the first snapshot is the baseline. */
  const seen = useRef(new Map<string, Set<string>>());
  const jump = useRef<Jump | null>(null);

  useEffect(() => {
    void invoke<HostsPayload>("hosts_get").then(setPayload);
    void getVersion().then(setVersion);
    const unlisten = listen<HostsPayload>("hosts", (e) => setPayload(e.payload));
    const tick = setInterval(() => setNow(Date.now()), 15_000);
    void (async () => {
      if (!(await isPermissionGranted())) await requestPermission();
    })();
    const unfocus = getCurrentWindow().onFocusChanged(({ payload: isFocused }) => {
      focused.current = isFocused;
      const j = jump.current;
      if (isFocused && j && Date.now() - j.at < JUMP_WINDOW_MS) {
        setSelected(j.key);
        if (j.session) setSessions((m) => ({ ...m, [j.key]: j.session! }));
      }
      if (isFocused) jump.current = null;
    });
    return () => {
      void unlisten.then((f) => f());
      void unfocus.then((f) => f());
      clearInterval(tick);
    };
  }, []);

  const placed = useMemo(() => (payload ? placeAll(payload) : []), [payload]);

  // Notify on attention that is new since we last looked: needs-you items and
  // failures, unless that workspace is already in front of the user.
  useEffect(() => {
    if (!payload) return;
    for (const host of payload.hosts) {
      if (host.status !== "connected") continue;
      const items = host.workspaces.flatMap((ws) =>
        ws.attention.filter((a) => a.needsYou || a.kind === "failure").map((a) => ({ ws, a })),
      );
      const known = seen.current.get(host.name);
      if (!known) {
        seen.current.set(host.name, new Set(items.map((i) => i.a.id)));
        continue;
      }
      for (const { ws, a } of items) {
        if (known.has(a.id)) continue;
        known.add(a.id);
        const key = `${host.name}/${ws.id}`;
        if (focused.current && selectedRef.current === key) continue;
        jump.current = { key, session: a.sessionId, at: Date.now() };
        sendNotification({
          title: a.needsYou ? `${ws.name} needs you` : `${ws.name}: something failed`,
          body: `${a.summary} · ${host.name}`,
        });
      }
    }
  }, [payload]);

  // Keep a valid selection: the most urgent workspace by default.
  useEffect(() => {
    if (placed.length === 0) return;
    if (!selected || !placed.some((p) => p.key === selected)) {
      setSelected(groups(placed)[0]?.items[0]?.key);
    }
  }, [placed, selected]);

  const current: Placed | undefined = placed.find((p) => p.key === selected);
  const currentSession = current
    ? current.ws.sessions.some((s) => s.id === sessions[current.key])
      ? sessions[current.key]
      : defaultSession(current.ws)
    : undefined;

  const needs = placed.filter((p) => p.ws.activity === "needs_you" || p.ws.activity === "failed").length;
  const working = placed.filter((p) => p.ws.activity === "working").length;

  return (
    <div className="app">
      <header className="titlebar">
        <div className="brand">
          <svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true">
            <rect x="1.5" y="1.5" width="15" height="15" rx="4" fill="none" stroke="currentColor" strokeWidth="1.5" />
            <path d="M5 7l2.5 2.5L5 12M9.5 12H13" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
          <span className="brand-name">Workd</span>
          {placed.length > 0 && (
            <span className="brand-sub">
              {needs} need you · {working} working
            </span>
          )}
        </div>
        {version && <span className="app-version">v{version}</span>}
      </header>
      <div className="body">
        {payload && (
          <Sidebar
            placed={placed}
            hosts={payload.hosts}
            selected={selected}
            onSelect={setSelected}
            now={now}
            appVersion={version}
          />
        )}
        {!payload ? (
          <main className="pane empty">Loading…</main>
        ) : payload.configError ? (
          <main className="pane empty">
            <p>Couldn’t read the host list: {payload.configError}</p>
          </main>
        ) : payload.hosts.length === 0 ? (
          <main className="pane empty">
            <p>No hosts yet.</p>
            <p className="muted">
              Add one with <code>workctl host add &lt;name&gt;</code> — the app reads {payload.configDir}/hosts.toml.
            </p>
          </main>
        ) : current ? (
          <WorkspacePane
            key={current.key}
            placed={current}
            session={currentSession}
            onSession={(id) => setSessions((m) => ({ ...m, [current.key]: id }))}
            now={now}
          />
        ) : (
          <main className="pane empty">
            <p>No workspaces yet.</p>
            <p className="muted">
              Create one with <code>workctl new &lt;name&gt;</code>.
            </p>
          </main>
        )}
      </div>
    </div>
  );
}
