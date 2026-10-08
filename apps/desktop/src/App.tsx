import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { Sidebar } from "./Sidebar";
import { AddHostDialog } from "./AddHostDialog";
import { HostPage } from "./HostPage";
import { CliDialog } from "./CliDialog";
import { WorkspacePane } from "./WorkspacePane";
import { NewWorkspaceDialog } from "./NewWorkspaceDialog";
import { useTheme } from "./theme";
import { defaultSession, groups, headline, placeAll } from "./model";
import type { ForwardView, HostsPayload, Placed } from "./types";
import otterIcon from "./assets/otter.png";
import "./App.css";

/** Activating the app this soon after a notification jumps to its workspace. */
const JUMP_WINDOW_MS = 2 * 60 * 1000;
/** On macOS the header doubles as the title bar (overlay style, see tauri.conf.json). */
const IS_MAC = navigator.userAgent.includes("Mac");

type Open = { kind: "add" } | { kind: "cli" } | { kind: "new" } | null;

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
  const [open, setOpen] = useState<Open>(null);
  /** A host's page, shown instead of the selected workspace. */
  const [hostPage, setHostPage] = useState<string | null>(null);
  const [forwards, setForwards] = useState<ForwardView[]>([]);
  const [cli, setCli] = useState<{ version?: string } | null>(null);
  const { choice: themeChoice, resolved: theme, cycle: cycleTheme } = useTheme();
  /** A workspace just created here: select it once it shows up. */
  const wanted = useRef<string | null>(null);

  const focused = useRef(document.hasFocus());
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  /** Attention ids already seen per host; the first snapshot is the baseline. */
  const seen = useRef(new Map<string, Set<string>>());
  const jump = useRef<Jump | null>(null);

  useEffect(() => {
    void invoke<HostsPayload>("hosts_get").then(setPayload);
    void getVersion().then(setVersion);
    void invoke<ForwardView[]>("forwards_get").then(setForwards);
    const unforwards = listen<ForwardView[]>("forwards", (e) => setForwards(e.payload));
    // A workspace picked in the menu bar item.
    const unjump = listen<string>("jump", (e) => {
      setSelected(e.payload);
      setHostPage(null);
    });
    void invoke<{ version?: string }>("cli_status").then(setCli);
    // ⌘N: new workspace (capture phase, so the terminal doesn't swallow it).
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "n") {
        e.preventDefault();
        setOpen({ kind: "new" });
      }
    };
    window.addEventListener("keydown", onKey, true);
    const unlisten = listen<HostsPayload>("hosts", (e) => setPayload(e.payload));
    const unmenu = listen<string>("menu", (e) => e.payload === "install-cli" && setOpen({ kind: "cli" }));
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
      void unmenu.then((f) => f());
      void unforwards.then((f) => f());
      void unjump.then((f) => f());
      void unfocus.then((f) => f());
      window.removeEventListener("keydown", onKey, true);
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

  // The menu bar item shows what needs you.
  useEffect(() => {
    const waiting = placed
      .filter((p) => p.ws.activity === "needs_you" || p.ws.activity === "failed")
      .map((p) => ({ key: p.key, label: `${p.ws.name} — ${headline(p.ws).text} (${p.host.name})` }));
    void invoke("tray_update", { waiting }).catch(() => {});
  }, [placed]);

  // Keep a valid selection: the most urgent workspace by default.
  useEffect(() => {
    if (selected && placed.some((p) => p.key === selected)) {
      if (wanted.current === selected) wanted.current = null;
      return;
    }
    if (selected && selected === wanted.current) return; // still on its way
    if (placed.length > 0) setSelected(groups(placed)[0]?.items[0]?.key);
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
    <div className={IS_MAC ? "app mac" : "app"}>
      <header className="titlebar" data-tauri-drag-region>
        <div className="brand" data-tauri-drag-region>
          <img src={otterIcon} width={22} height={22} alt="" className="brand-icon" />
          <span className="brand-name" data-tauri-drag-region>Otter</span>
          {placed.length > 0 && (
            <span className="brand-sub" data-tauri-drag-region>
              {needs} need you · {working} working
            </span>
          )}
        </div>
        <div className="titlebar-actions">
          {cli && version && cli.version !== version && (
            <button className="link-btn" onClick={() => setOpen({ kind: "cli" })}>
              {cli.version ? "Update command line tools" : "Install command line tools"}
            </button>
          )}
          <button
            className="icon-btn"
            onClick={cycleTheme}
            aria-label={`Theme: ${themeChoice}`}
            title={`Theme: ${themeChoice === "system" ? "match system" : themeChoice} (click to change)`}
          >
            {themeChoice === "light" ? (
              <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
                <circle cx="8" cy="8" r="3" fill="none" stroke="currentColor" strokeWidth="1.5" />
                <path d="M8 1.5v1.5M8 13v1.5M1.5 8H3M13 8h1.5M3.4 3.4l1 1M11.6 11.6l1 1M3.4 12.6l1-1M11.6 4.4l1-1" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
              </svg>
            ) : themeChoice === "dark" ? (
              <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
                <path d="M13 9.5A5.5 5.5 0 0 1 6.5 3a5.5 5.5 0 1 0 6.5 6.5z" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" />
              </svg>
            ) : (
              <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
                <circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" strokeWidth="1.5" />
                <path d="M8 2a6 6 0 0 1 0 12z" fill="currentColor" />
              </svg>
            )}
          </button>
          <button className="btn outline new-btn" onClick={() => setOpen({ kind: "new" })} title="New workspace (⌘N)">
            <svg width="11" height="11" viewBox="0 0 12 12" aria-hidden="true">
              <path d="M6 1.5v9M1.5 6h9" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
            </svg>
            New
            <span className="kbd">⌘N</span>
          </button>
          {version && <span className="app-version">v{version}</span>}
        </div>
      </header>
      <div className="body">
        {payload && (
          <Sidebar
            placed={placed}
            hosts={payload.hosts}
            selected={selected}
            onSelect={(key) => {
              setSelected(key);
              setHostPage(null);
            }}
            hostPage={hostPage ?? undefined}
            now={now}
            appVersion={version}
            onAddHost={() => setOpen({ kind: "add" })}
            onHost={setHostPage}
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
            <h1 className="empty-title">Add your first host</h1>
            <p className="muted">
              A machine where your work runs: a server or VM you reach with <code>ssh</code>, or this Mac.
            </p>
            <div className="empty-actions">
              <button className="btn primary" onClick={() => setOpen({ kind: "add" })}>
                Add a host
              </button>
              <button className="btn outline" onClick={() => setOpen({ kind: "cli" })}>
                Install command line tools
              </button>
            </div>
          </main>
        ) : hostPage && payload.hosts.some((h) => h.name === hostPage) ? (
          <HostPage
            key={hostPage}
            host={payload.hosts.find((h) => h.name === hostPage)!}
            version={version}
            forwards={forwards}
            onRemoved={() => setHostPage(null)}
          />
        ) : current ? (
          <WorkspacePane
            key={current.key}
            placed={current}
            session={currentSession}
            onSession={(id) => setSessions((m) => ({ ...m, [current.key]: id }))}
            now={now}
            theme={theme}
            appVersion={version}
          />
        ) : (
          <main className="pane empty">
            <h1 className="empty-title">Start your first workspace</h1>
            <p className="muted">A place on a host for one piece of work: its files, a Codex session, a shell.</p>
            <div className="empty-actions">
              <button className="btn primary" onClick={() => setOpen({ kind: "new" })}>
                New workspace
              </button>
            </div>
          </main>
        )}
      </div>
      {open?.kind === "add" && <AddHostDialog version={version} onClose={() => setOpen(null)} />}
      {open?.kind === "cli" && (
        <CliDialog
          version={version}
          onClose={() => {
            setOpen(null);
            void invoke<{ version?: string }>("cli_status").then(setCli);
          }}
        />
      )}
      {open?.kind === "new" && payload && (
        <NewWorkspaceDialog
          hosts={payload.hosts}
          defaultHost={current?.host.name}
          onClose={() => setOpen(null)}
          onCreated={(host, id) => {
            const key = `${host}/${id}`;
            wanted.current = key;
            setSelected(key);
            setOpen(null);
          }}
        />
      )}
    </div>
  );
}
