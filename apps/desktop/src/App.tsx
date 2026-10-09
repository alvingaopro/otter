import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { Sidebar } from "./Sidebar";
import { ActivityBar } from "./ActivityBar";
import { FeaturesView } from "./FeaturesView";
import { daemonSource } from "./featureSource";
import { needsYou as featureNeedsYou } from "./features";
import { saveView, storedView, viewForShortcut, type View } from "./nav";
import { AddHostDialog } from "./AddHostDialog";
import { HostPage } from "./HostPage";
import { CliDialog } from "./CliDialog";
import { WorkspacePane } from "./WorkspacePane";
import { NewWorkspaceDialog } from "./NewWorkspaceDialog";
import { useTheme } from "./theme";
import { defaultSession, groups, headline, movePin, pinnedOf, placeAll, stalePins, togglePin } from "./model";
import type { MenuItem } from "./Menu";
import type { ForwardView, HostsPayload, Placed } from "./types";
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
  /** The view in front (D-041); the others stay mounted, so nothing they run is lost. */
  const [view, setViewState] = useState<View>(storedView);
  const setView = (v: View) => {
    setViewState(v);
    saveView(v);
  };
  const [sessions, setSessions] = useState<Record<string, string>>({});
  const [now, setNow] = useState(Date.now());
  const [version, setVersion] = useState<string | undefined>();
  const [open, setOpen] = useState<Open>(null);
  /** A host's page, shown instead of the selected workspace. */
  const [hostPage, setHostPage] = useState<string | null>(null);
  const [forwards, setForwards] = useState<ForwardView[]>([]);
  const [cli, setCli] = useState<{ version?: string } | null>(null);
  /** Pinned workspace keys, top first, from `pins.toml` (D-040). */
  const [pins, setPinsState] = useState<string[]>([]);
  const { choice: themeChoice, resolved: theme, cycle: cycleTheme } = useTheme();
  /** Features, from each connected host's otterd (D-043). */
  const [featureSource] = useState(() => daemonSource());
  const [featureNeeds, setFeatureNeeds] = useState(() => featureSource.list().filter((p) => featureNeedsYou(p.feature)).length);
  useEffect(
    () => featureSource.subscribe((list) => setFeatureNeeds(list.filter((p) => featureNeedsYou(p.feature)).length)),
    [featureSource],
  );
  /** A workspace just created here: select it once it shows up. */
  const wanted = useRef<string | null>(null);

  const focused = useRef(document.hasFocus());
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  const viewRef = useRef(view);
  viewRef.current = view;
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
      setView("workspaces");
    });
    void invoke<{ version?: string }>("cli_status").then(setCli);
    void invoke<string[]>("pins_get").then(setPinsState).catch(() => {});
    // ⌘N: new workspace (capture phase, so the terminal doesn't swallow it).
    const onKey = (e: KeyboardEvent) => {
      const v = viewForShortcut(e);
      if (v) {
        e.preventDefault();
        setView(v);
        return;
      }
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
        setView("workspaces");
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
  const connectedHosts = (payload?.hosts ?? []).filter((h) => h.status === "connected").map((h) => h.name);
  const connectedKey = connectedHosts.join("\n");
  useEffect(() => featureSource.setHosts?.(connectedKey ? connectedKey.split("\n") : []), [featureSource, connectedKey]);

  const setPins = (next: string[]) => {
    setPinsState(next);
    void invoke("pins_set", { workspaces: next }).catch(() => {});
  };
  // A pinned workspace deleted elsewhere drops out once its host says so.
  useEffect(() => {
    if (!payload) return;
    const stale = stalePins(pins, payload.hosts);
    if (stale.length > 0) setPins(pins.filter((k) => !stale.includes(k)));
  }, [payload, pins]);

  const shownPins = pinnedOf(placed, pins).map((p) => p.key);
  const pinMenu = (key: string): MenuItem[] => {
    const at = shownPins.indexOf(key);
    return [
      { label: at < 0 ? "Pin" : "Unpin", onSelect: () => setPins(togglePin(pins, key)) },
      { label: "Move up", hidden: at <= 0, onSelect: () => setPins(movePin(pins, shownPins, key, at - 1)) },
      {
        label: "Move down",
        hidden: at < 0 || at === shownPins.length - 1,
        onSelect: () => setPins(movePin(pins, shownPins, key, at + 1)),
      },
    ];
  };

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
        if (focused.current && viewRef.current === "workspaces" && selectedRef.current === key) continue;
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

  return (
    <div className={IS_MAC ? "app mac" : "app"}>
      <div className="body">
        <ActivityBar
          view={view}
          onView={setView}
          badges={{ features: featureNeeds, workspaces: placed.filter((p) => p.ws.activity === "needs_you").length }}
          settings={[
            { label: `Theme: ${themeChoice === "system" ? "match system" : themeChoice}`, onSelect: cycleTheme },
            { label: "Add a host…", onSelect: () => setOpen({ kind: "add" }) },
            {
              label: cli?.version ? "Update command line tools…" : "Install command line tools…",
              onSelect: () => setOpen({ kind: "cli" }),
            },
          ]}
        />
        <div className="view" id="view-features" role="tabpanel" aria-labelledby="view-tab-features" hidden={view !== "features"}>
          <FeaturesView
            source={featureSource}
            hosts={connectedHosts}
            now={now}
            onOpenWorkspace={(host, workspace, session) => {
              const key = `${host}/${workspace}`;
              wanted.current = key;
              setSelected(key);
              setHostPage(null);
              if (session) setSessions((m) => ({ ...m, [key]: session }));
              setView("workspaces");
            }}
          />
        </div>
        <div className="view" id="view-workspaces" role="tabpanel" aria-labelledby="view-tab-workspaces" hidden={view !== "workspaces"}>
        <Sidebar
          placed={placed}
          hosts={payload?.hosts ?? []}
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
          onNew={() => setOpen({ kind: "new" })}
          themeChoice={themeChoice}
          onTheme={cycleTheme}
          cliAction={
            cli && version && cli.version !== version
              ? cli.version
                ? "Update command line tools"
                : "Install command line tools"
              : undefined
          }
          onCli={() => setOpen({ kind: "cli" })}
          pins={pins}
          pinMenu={pinMenu}
          onMovePin={(key, to) => setPins(movePin(pins, shownPins, key, to))}
        />
        {!payload ? (
          <main className="pane empty">
            <div className="drag-strip" data-tauri-drag-region />
            Loading…
          </main>
        ) : payload.configError ? (
          <main className="pane empty">
            <div className="drag-strip" data-tauri-drag-region />
            <p>Couldn’t read the host list: {payload.configError}</p>
          </main>
        ) : payload.hosts.length === 0 ? (
          <main className="pane empty">
            <div className="drag-strip" data-tauri-drag-region />
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
            pinItems={pinMenu(current.key)}
          />
        ) : (
          <main className="pane empty">
            <div className="drag-strip" data-tauri-drag-region />
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
            setView("workspaces");
            setOpen(null);
          }}
        />
      )}
    </div>
  );
}
