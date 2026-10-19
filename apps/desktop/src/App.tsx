import { useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Sidebar } from "./Sidebar";
import { ActivityBar } from "./ActivityBar";
import { TasksView } from "./TasksView";
import { Inbox } from "./Inbox";
import { newNeeds, trayTasks } from "./inboxModel";
import type { PlacedTask } from "./tasks";
import { daemonSource } from "./taskSource";
import { needsYou as taskNeedsYou } from "./tasks";
import { saveView, storedView, viewForShortcut, type View } from "./nav";
import { AddHostDialog } from "./AddHostDialog";
import { HostPage } from "./HostPage";
import { CliDialog } from "./CliDialog";
import { WorkspacePane } from "./WorkspacePane";
import { NewWorkspaceDialog } from "./NewWorkspaceDialog";
import { SettingsDialog } from "./SettingsDialog";
import { ASK_TTL_MS, SignInAsks, type SignInAsk } from "./SignIn";
import { PanelGrip, SIDEBAR, usePanelWidth } from "./panelResize";
import { QuickOpen } from "./QuickOpen";
import { RenameDialog } from "./RenameDialog";
import { explain, forgetHost } from "./newer";
import { zoomKey } from "./terminalFont";
import { useTheme } from "./theme";
import { offered, progressed, type UpdateOffer } from "./update";
import { defaultSession, groups, headline, movePin, noticeTitle, pinnedOf, placeAll, stalePins, togglePin, trayLabels } from "./model";
import type { MenuItem } from "./Menu";
import type { ForwardView, HostsPayload, Placed } from "./types";
import "./App.css";

/** Activating the app this soon after a notification jumps to its workspace. */
const JUMP_WINDOW_MS = 2 * 60 * 1000;
/** On macOS the header doubles as the title bar (overlay style, see tauri.conf.json). */
const IS_MAC = navigator.userAgent.includes("Mac");

type Open =
  | { kind: "add" }
  | { kind: "cli" }
  | { kind: "new" }
  | { kind: "settings" }
  | { kind: "quick" }
  | { kind: "rename"; key: string }
  | null;

/** How many workspaces keep their pane, and so their terminals, while not shown (D-100). */
const KEPT_PANES = 5;

interface Jump {
  key: string;
  session?: string;
  at: number;
  /** A task (in the Tasks view), not a workspace. */
  task?: boolean;
}

export default function App() {
  const [payload, setPayload] = useState<HostsPayload | null>(null);
  const payloadRef = useRef(payload);
  payloadRef.current = payload;
  /** Sign-in pages from sites this Mac doesn't open by itself (D-121). */
  const [asks, setAsks] = useState<SignInAsk[]>([]);
  const [selected, setSelected] = useState<string | undefined>();
  /** The view in front (D-041); the others stay mounted, so nothing they run is lost. */
  const [view, setViewState] = useState<View>(storedView);
  const setView = (v: View) => {
    setViewState(v);
    saveView(v);
  };
  const [sessions, setSessions] = useState<Record<string, string>>({});
  /** Workspaces shown lately, the latest first: their panes stay mounted (D-100). */
  const [kept, setKept] = useState<string[]>([]);
  /** The session each kept pane showed when it was last in front. */
  const shownSession = useRef(new Map<string, string>());
  // One width for the Tasks and Workspaces lists (D-097).
  const [sidebarWidth, setSidebarWidth] = usePanelWidth(SIDEBAR);
  const sidebarGrip = <PanelGrip panel={SIDEBAR} width={sidebarWidth} onResize={setSidebarWidth} label="Sidebar width" />;
  const [now, setNow] = useState(Date.now());
  const [version, setVersion] = useState<string | undefined>();
  const [open, setOpen] = useState<Open>(null);
  /** A host's page, shown instead of the selected workspace. */
  const [hostPage, setHostPage] = useState<string | null>(null);
  const [forwards, setForwards] = useState<ForwardView[]>([]);
  const [cli, setCli] = useState<{ version?: string } | null>(null);
  /** A newer release of this app (D-125). */
  const [update, setUpdate] = useState<UpdateOffer | null>(null);
  /** Pinned workspace keys, top first, from `pins.toml` (D-040). */
  const [pins, setPinsState] = useState<string[]>([]);
  const { choice: themeChoice, resolved: theme, set: setTheme } = useTheme();
  /** Tasks, from each connected host's otterd (D-043). */
  const [taskSource] = useState(() => daemonSource());
  const [tasks, setTasks] = useState<PlacedTask[]>(() => taskSource.list());
  useEffect(() => taskSource.subscribe(setTasks), [taskSource]);
  const taskNeeds = tasks.filter((p) => taskNeedsYou(p.task)).length;
  /** A task to show in the Tasks view, opened from the Inbox. */
  const [taskFocus, setTaskFocus] = useState<{ key: string; at: number } | undefined>();
  /** What each task needed when last looked at (D-073). */
  const taskSeen = useRef(new Map<string, string | null>());
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
    // A workspace (or a task) picked in the menu bar item.
    const unjump = listen<string>("jump", (e) => {
      if (e.payload.startsWith("task:")) {
        setTaskFocus({ key: e.payload.slice("task:".length), at: Date.now() });
        setView("tasks");
        return;
      }
      setSelected(e.payload);
      setHostPage(null);
      setView("workspaces");
    });
    void invoke<{ version?: string }>("cli_status").then(setCli);
    void invoke<string | null>("update_get").then((v) => setUpdate((u) => offered(u, v)));
    const unupdate = listen<string | null>("update", (e) => setUpdate((u) => offered(u, e.payload)));
    const unprogress = listen<{ downloaded: number; total?: number }>("update-progress", (e) =>
      setUpdate((u) => progressed(u, e.payload.downloaded, e.payload.total)),
    );
    const unfailed = listen<string>("update-failed", (e) => setUpdate((u) => u && { version: u.version, error: e.payload }));
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
        return;
      }
      // ⌘,: Settings (the app menu has it too).
      if (e.metaKey && !e.shiftKey && !e.altKey && !e.ctrlKey && e.key === ",") {
        e.preventDefault();
        e.stopPropagation();
        setOpen({ kind: "settings" });
        return;
      }
      // ⌘K: go to a workspace (D-106).
      if (e.metaKey && !e.shiftKey && !e.altKey && !e.ctrlKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        e.stopPropagation();
        setOpen((o) => (o?.kind === "quick" ? null : { kind: "quick" }));
        return;
      }
      // ⌘= / ⌘- / ⌘0: the terminals' text size (D-106).
      if (zoomKey(e)) {
        e.preventDefault();
        e.stopPropagation();
      }
    };
    window.addEventListener("keydown", onKey, true);
    const unlisten = listen<HostsPayload>("hosts", (e) => setPayload(e.payload));
    // A tool on a host wants a sign-in page from another site: ask, in the
    // corner, and by notification if the window isn't in front.
    const unasked = listen<{ host: string; requestId: string; site: string; workspaceId?: string }>("sign-in-ask", (e) => {
      const { host, requestId, site, workspaceId } = e.payload;
      const workspace = payloadRef.current?.hosts
        .find((h) => h.name === host)
        ?.workspaces.find((w) => w.id === workspaceId)?.name;
      setAsks((a) => [...a.filter((x) => x.requestId !== requestId), { host, requestId, site, workspace }]);
      setTimeout(() => setAsks((a) => a.filter((x) => x.requestId !== requestId)), ASK_TTL_MS);
      if (!focused.current) sendNotification({ title: `Sign in at ${site}?`, body: `A tool on ${host} wants to open it` });
    });
    const unmenu = listen<string>("menu", (e) => {
      if (e.payload === "install-cli") setOpen({ kind: "cli" });
      if (e.payload === "settings") setOpen({ kind: "settings" });
    });
    const tick = setInterval(() => setNow(Date.now()), 15_000);
    void (async () => {
      if (!(await isPermissionGranted())) await requestPermission();
    })();
    const unfocus = getCurrentWindow().onFocusChanged(({ payload: isFocused }) => {
      focused.current = isFocused;
      const j = jump.current;
      if (isFocused && j && Date.now() - j.at < JUMP_WINDOW_MS) {
        if (j.task) {
          setTaskFocus({ key: j.key, at: Date.now() });
          setView("tasks");
        } else {
          setSelected(j.key);
          setView("workspaces");
          if (j.session) setSessions((m) => ({ ...m, [j.key]: j.session! }));
        }
      }
      if (isFocused) jump.current = null;
    });
    return () => {
      void unlisten.then((f) => f());
      void unasked.then((f) => f());
      void unmenu.then((f) => f());
      void unforwards.then((f) => f());
      void unjump.then((f) => f());
      void unupdate.then((f) => f());
      void unprogress.then((f) => f());
      void unfailed.then((f) => f());
      void unfocus.then((f) => f());
      window.removeEventListener("keydown", onKey, true);
      clearInterval(tick);
    };
  }, []);

  const placed = useMemo(() => (payload ? placeAll(payload) : []), [payload]);
  // A host that comes back may have been updated: ask it again what it knows.
  const connectedNow = (payload?.hosts ?? []).filter((h) => h.status === "connected").map((h) => `${h.name}@${h.version ?? ""}`).join("\n");
  const connectedBefore = useRef("");
  useEffect(() => {
    const before = new Set(connectedBefore.current.split("\n"));
    for (const h of connectedNow.split("\n")) if (h && !before.has(h)) forgetHost(h.slice(0, h.lastIndexOf("@")));
    connectedBefore.current = connectedNow;
  }, [connectedNow]);
  const connectedHosts = (payload?.hosts ?? []).filter((h) => h.status === "connected").map((h) => h.name);
  const connectedKey = connectedHosts.join("\n");
  useEffect(() => taskSource.setHosts?.(connectedKey ? connectedKey.split("\n") : []), [taskSource, connectedKey]);

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
      { label: "Rename…", onSelect: () => setOpen({ kind: "rename", key }) },
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
  // failures, unless the session it is about is already in front of the
  // user — another session of the same workspace still notifies (D-109).
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
        const onScreen = !a.sessionId || a.sessionId === shownSession.current.get(key);
        if (focused.current && viewRef.current === "workspaces" && selectedRef.current === key && onScreen) continue;
        jump.current = { key, session: a.sessionId, at: Date.now() };
        sendNotification({ title: noticeTitle(ws.name, a), body: `${a.summary} · ${host.name}` });
      }
    }
  }, [payload]);

  // A task newly needing you notifies once (D-073), unless the Inbox is
  // in front of you.
  useEffect(() => {
    for (const n of newNeeds(taskSeen.current, tasks)) {
      if (focused.current && viewRef.current === "inbox") continue;
      jump.current = { key: n.key, at: Date.now(), task: true };
      sendNotification({
        title: n.need.kind === "failed" ? `${n.title} stopped` : `${n.title} needs you`,
        body: `${n.need.summary} · ${n.host}`,
      });
    }
  }, [tasks]);

  // The menu bar item shows what needs you: workspaces and tasks.
  useEffect(() => {
    const waiting = trayLabels([
      ...placed
        .filter((p) => p.ws.activity === "needs_you" || p.ws.activity === "failed")
        .map((p) => ({ key: p.key, name: p.ws.name, what: headline(p.ws).text, host: p.host.name })),
      ...trayTasks(tasks),
    ]);
    void invoke("tray_update", { waiting }).catch(() => {});
  }, [placed, tasks]);

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
  const sessionOf = (p: Placed) =>
    p.ws.sessions.some((s) => s.id === sessions[p.key]) ? sessions[p.key] : defaultSession(p.ws);
  const currentSession = current ? sessionOf(current) : undefined;
  // The workspace in front joins the kept ones; one not shown keeps the
  // session it showed, so nothing attaches behind the developer's back.
  useEffect(() => {
    if (!current) return;
    setKept((k) => (k[0] === current.key ? k : [current.key, ...k.filter((x) => x !== current.key)].slice(0, KEPT_PANES)));
  }, [current?.key]);
  useEffect(() => {
    if (current && currentSession) shownSession.current.set(current.key, currentSession);
  }, [current?.key, currentSession]);
  const keptKeys = current ? [current.key, ...kept.filter((k) => k !== current.key)].slice(0, KEPT_PANES) : kept;
  const keptPanes = keptKeys.flatMap((k) => placed.find((p) => p.key === k) ?? []);
  const workspacesInFront = view === "workspaces" && !hostPage;

  // A port of a host, in the browser: a remote host's is forwarded first (D-046).
  async function openPort(host: string, port: number, path = "/") {
    const local = payload?.hosts.find((h) => h.name === host)?.describe === "this Mac";
    if (!local) {
      await invoke("forward_add", {
        host,
        direction: "to_local",
        listenPort: port,
        targetHost: null,
        targetPort: port,
        pinned: false,
      }).catch((e) => {
        if (!String(e).includes("already")) throw e;
      });
    }
    await openUrl(`http://127.0.0.1:${port}${path}`);
  }

  return (
    <div className={IS_MAC ? "app mac" : "app"} style={{ "--sidebar-width": `${sidebarWidth}px` } as CSSProperties}>
      <div className="body">
        <ActivityBar
          view={view}
          onView={setView}
          badges={{
            inbox: taskNeeds + placed.filter((p) => p.ws.activity === "needs_you" || p.ws.activity === "failed").length,
            tasks: taskNeeds,
            workspaces: placed.filter((p) => p.ws.activity === "needs_you").length,
          }}
          settings={[
            { label: "Settings… (⌘,)", onSelect: () => setOpen({ kind: "settings" }) },
            { label: "Add a host…", onSelect: () => setOpen({ kind: "add" }) },
            {
              label: cli?.version ? "Command line tools…" : "Install command line tools…",
              onSelect: () => setOpen({ kind: "cli" }),
            },
          ]}
        />
        <div className="view" id="view-inbox" role="tabpanel" aria-labelledby="view-tab-inbox" hidden={view !== "inbox"}>
          <Inbox
            source={taskSource}
            workspaces={placed.filter((p) => p.host.status === "connected")}
            now={now}
            onOpenTask={(key) => {
              setTaskFocus({ key, at: Date.now() });
              setView("tasks");
            }}
            onOpenWorkspace={(key) => {
              wanted.current = key;
              setSelected(key);
              setHostPage(null);
              setView("workspaces");
            }}
          />
        </div>
        <div className="view" id="view-tasks" role="tabpanel" aria-labelledby="view-tab-tasks" hidden={view !== "tasks"}>
          <TasksView
            source={taskSource}
            focus={taskFocus}
            hosts={connectedHosts}
            workspaces={Object.fromEntries(
              (payload?.hosts ?? []).map((h) => [
                h.name,
                h.workspaces.filter((w) => w.state !== "archived").map((w) => ({ id: w.id, name: w.name })),
              ]),
            )}
            now={now}
            onOpenWorkspace={(host, workspace, session) => {
              const key = `${host}/${workspace}`;
              wanted.current = key;
              setSelected(key);
              setHostPage(null);
              if (session) setSessions((m) => ({ ...m, [key]: session }));
              setView("workspaces");
            }}
            grip={sidebarGrip}
            onOpenPreview={openPort}
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
          update={update ?? undefined}
          onUpdate={() => {
            // Downloads, installs and restarts; the events say how far it got.
            setUpdate((u) => u && { version: u.version, progress: null });
            invoke("update_install").catch((e) => setUpdate((u) => u && { version: u.version, error: String(e) }));
          }}
          pins={pins}
          pinMenu={pinMenu}
          onMovePin={(key, to) => setPins(movePin(pins, shownPins, key, to))}
        />
        {sidebarGrip}
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
        ) : current ? null : (
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
        {payload &&
          !payload.configError &&
          keptPanes.map((p) => (
            <WorkspacePane
              key={p.key}
              placed={p}
              shown={!hostPage && p.key === current?.key}
              active={workspacesInFront && p.key === current?.key}
              session={p.key === current?.key ? currentSession : (shownSession.current.get(p.key) ?? sessionOf(p))}
              onSession={(id) => setSessions((m) => ({ ...m, [p.key]: id }))}
              now={now}
              theme={theme}
              appVersion={version}
              pinItems={pinMenu(p.key)}
              onOpenPort={(port) => void openPort(p.host.name, port).catch(() => {})}
              onOpenTask={(id) => {
                setTaskFocus({ key: `${p.host.name}/${id}`, at: Date.now() });
                setView("tasks");
              }}
            />
          ))}
        </div>
      </div>
      {open?.kind === "quick" && (
        <QuickOpen
          placed={placed}
          onOpen={(key) => {
            wanted.current = key;
            setSelected(key);
            setHostPage(null);
            setView("workspaces");
            setOpen(null);
          }}
          onClose={() => setOpen(null)}
        />
      )}
      {open?.kind === "rename" &&
        (() => {
          const p = placed.find((x) => x.key === open.key);
          if (!p) return null;
          return (
            <RenameDialog
              title={`Rename ${p.ws.name}`}
              initial={p.ws.name}
              onRename={async (name) => {
                try {
                  await invoke("workspace_rename", { host: p.host.name, workspace: p.ws.id, name });
                } catch (e) {
                  throw explain(p.host.name, e);
                }
              }}
              onClose={() => setOpen(null)}
            />
          );
        })()}
      <SignInAsks asks={asks} onDone={(id) => setAsks((a) => a.filter((x) => x.requestId !== id))} />
      {open?.kind === "add" && <AddHostDialog version={version} onClose={() => setOpen(null)} />}
      {open?.kind === "settings" && (
        <SettingsDialog
          hosts={connectedHosts}
          defaultHost={current?.host.name}
          theme={themeChoice}
          onTheme={setTheme}
          onAddHost={() => setOpen({ kind: "add" })}
          version={version}
          onClose={() => setOpen(null)}
        />
      )}
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
