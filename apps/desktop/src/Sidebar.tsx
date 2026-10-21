import { useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Glyph } from "./Glyph";
import { ContextMenu, type MenuItem } from "./Menu";
import { ago, groups, headline, matchScore, pinnedOf, workspaceGlyph } from "./model";
import type { HostView, Placed } from "./types";
import { installing, updateLabel, type UpdateOffer } from "./update";

interface Props {
  placed: Placed[];
  hosts: HostView[];
  selected?: string;
  onSelect: (key: string) => void;
  now: number;
  /** This app's version, to point out hosts running a different otterd. */
  appVersion?: string;
  onAddHost: () => void;
  onHost: (name: string) => void;
  /** The host whose page is open. */
  hostPage?: string;
  onNew: () => void;
  /** A newer release of the app, on offer or installing (D-125). */
  update?: UpdateOffer;
  onUpdate: () => void;
  /** Pinned workspace keys, top first (D-040). */
  pins: string[];
  /** Pin, unpin and move items for a workspace's menus. */
  pinMenu: (key: string) => MenuItem[];
  /** Move a pinned workspace to `to` among the shown pinned ones. */
  onMovePin: (key: string, to: number) => void;
}

const HOST_STATE: Record<HostView["status"], string> = {
  connecting: "connecting…",
  connected: "connected",
  unreachable: "unreachable · retrying",
  incompatible: "incompatible otterd",
  not_installed: "otterd not installed",
};

const HOSTS_OPEN_KEY = "otter.hostsOpen";

/** Whether release `a` (X.Y.Z) comes before `b`. */
export function older(a: string, b: string): boolean {
  const parts = (v: string) => v.split(/[.+-]/).slice(0, 3).map((x) => parseInt(x, 10) || 0);
  const [x, y] = [parts(a), parts(b)];
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] < y[i];
  return false;
}

function storedHostsOpen(): boolean {
  try {
    return localStorage.getItem(HOSTS_OPEN_KEY) !== "false";
  } catch {
    return true;
  }
}

export function Sidebar(props: Props) {
  const { placed, hosts, selected, onSelect, now, appVersion, onAddHost, onHost, hostPage } = props;
  // Updating every host whose otterd is older than this app, at once
  // (D-131): which are under way, and why one didn't.
  const [updating, setUpdating] = useState<Set<string>>(() => new Set());
  const [failed, setFailed] = useState<Record<string, string>>({});
  // Never one newer than the app: its update would be a step back.
  const behind = appVersion
    ? hosts.filter((h) => (h.status === "connected" || h.status === "incompatible") && !!h.version && older(h.version, appVersion))
    : [];
  const updateText = updating.size > 0 ? "Updating…" : behind.length > 1 ? "Update all" : "Update";
  async function updateAll() {
    const names = behind.map((h) => h.name).filter((n) => !updating.has(n));
    setUpdating((u) => new Set([...u, ...names]));
    setFailed((f) => Object.fromEntries(Object.entries(f).filter(([n]) => !names.includes(n))));
    // Side by side: each host is its own download and restart.
    await Promise.all(
      names.map(async (name) => {
        try {
          await invoke<string>("host_install", { name });
        } catch (e) {
          setFailed((f) => ({ ...f, [name]: String(e) }));
        } finally {
          setUpdating((u) => {
            const next = new Set(u);
            next.delete(name);
            return next;
          });
        }
      }),
    );
  }
  // Archived workspaces are out of the way until asked for (D-036).
  const [showArchived, setShowArchived] = useState(false);
  const [hostsOpen, setHostsOpen] = useState(storedHostsOpen);
  const toggleHosts = () => {
    setHostsOpen((open) => {
      try {
        localStorage.setItem(HOSTS_OPEN_KEY, String(!open));
      } catch {
        // Storage unavailable: the choice lasts until the app quits.
      }
      return !open;
    });
  };

  const [menu, setMenu] = useState<{ key: string; x: number; y: number } | null>(null);
  // Only the workspaces matching what's typed (D-106).
  const [filter, setFilter] = useState("");
  const visible = filter.trim() ? placed.filter((p) => matchScore(p, filter) > 0) : placed;
  // Reordering pins with the pointer (HTML5 drag-and-drop is taken by file drops).
  const [drag, setDrag] = useState<{ key: string; to: number } | null>(null);
  const dragged = useRef(false);
  const pinRows = useRef(new Map<string, HTMLElement>());

  const pinned = pinnedOf(visible, props.pins);
  const pinnedKeys = pinned.map((p) => p.key);
  const rest = visible.filter((p) => !props.pins.includes(p.key));
  const grouped = groups(rest);
  // The badges count every workspace, pinned or not.
  const all = groups(placed);
  const inGroup = (id: string) => all.find((g) => g.id === id)?.items ?? [];
  const needs = inGroup("needs");
  const working = inGroup("working");
  const connected = hosts.filter((h) => h.status === "connected").length;

  function startDrag(e: ReactPointerEvent, key: string) {
    if (e.button !== 0) return;
    const startY = e.clientY;
    const from = pinnedKeys.indexOf(key);
    let to = from;
    let active = false;
    // Rows as laid out when the drag starts; the target is how many of the
    // others sit above the pointer.
    const mids = pinnedKeys
      .filter((k) => k !== key)
      .map((k) => {
        const r = pinRows.current.get(k)?.getBoundingClientRect();
        return r ? r.top + r.height / 2 : 0;
      });
    const move = (ev: PointerEvent) => {
      if (!active && Math.abs(ev.clientY - startY) < 4) return;
      active = true;
      dragged.current = true;
      to = mids.filter((m) => m < ev.clientY).length;
      setDrag({ key, to });
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      if (!active) return;
      setDrag(null);
      if (to !== from) props.onMovePin(key, to);
      // The click that ends a drag doesn't select.
      setTimeout(() => (dragged.current = false), 0);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }

  // While dragging, show the pins in their would-be order.
  const shownPins = (() => {
    if (!drag) return pinned;
    const others = pinned.filter((p) => p.key !== drag.key);
    others.splice(drag.to, 0, pinned.find((p) => p.key === drag.key)!);
    return others;
  })();

  const row = (p: Placed, pin: boolean) => {
    const line = headline(p.ws);
    const stale = p.host.status !== "connected";
    const cls = [
      "ws-row",
      p.key === selected && !hostPage && "selected",
      stale && "stale",
      pin && "pin-row",
      drag?.key === p.key && "dragging",
    ];
    return (
      <button
        key={p.key}
        ref={pin ? (el) => void (el ? pinRows.current.set(p.key, el) : pinRows.current.delete(p.key)) : undefined}
        className={cls.filter(Boolean).join(" ")}
        aria-current={p.key === selected ? "page" : undefined}
        onClick={() => !dragged.current && onSelect(p.key)}
        onPointerDown={pin ? (e) => startDrag(e, p.key) : undefined}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ key: p.key, x: e.clientX, y: e.clientY });
        }}
      >
        <span className="ws-glyph">
          <Glyph kind={workspaceGlyph(p.ws)} />
        </span>
        <span className="ws-text">
          <span className="ws-line">
            <span className="ws-name">{p.ws.name}</span>
            <span className="ws-host">{p.host.name}</span>
          </span>
          <span className="ws-line sub">
            <span className="ws-summary">{line.text}</span>
            <span className="ws-age">{ago(line.since, now)}</span>
          </span>
        </span>
      </button>
    );
  };

  return (
    <nav className={drag ? "sidebar reordering" : "sidebar"} aria-label="Workspaces">
      <div className="sidebar-top" data-tauri-drag-region>
        <span className="spacer" data-tauri-drag-region />
        <button className="icon-btn" aria-label="New workspace" title="New workspace (⌘N)" onClick={props.onNew}>
          <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
            <path d="M7 2v10M2 7h10" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
        </button>
      </div>

      <div className="brand" data-tauri-drag-region>
        <span className="otter-mark brand-icon" aria-hidden="true" />
        <span className="brand-name" data-tauri-drag-region>Otter</span>
        <span className="spacer" data-tauri-drag-region />
        <CountBadge kind="needs" count={needs.length} label="need you" onClick={() => onSelect(needs[0].key)} />
        <CountBadge kind="working" count={working.length} label="working" onClick={() => onSelect(working[0].key)} />
      </div>

      <div className="sidebar-filter">
        <input
          type="search"
          aria-label="Filter workspaces"
          placeholder="Filter workspaces"
          title="Name, host, branch or session; ⌘K to go to one"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              setFilter("");
              e.currentTarget.blur();
            } else if (e.key === "Enter" && visible[0]) {
              onSelect((pinned[0] ?? groups(rest)[0]?.items[0] ?? visible[0]).key);
            }
          }}
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
        />
      </div>

      <div className="sidebar-groups">
        {filter.trim() && visible.length === 0 && <p className="muted small sidebar-none">No workspace matches.</p>}
        {pinned.length > 0 && (
          <section className="group" aria-label="Pinned">
            <h2 className="group-label pinned">
              <span>PINNED</span>
              <span className="count">{pinned.length}</span>
            </h2>
            {shownPins.map((p) => row(p, true))}
          </section>
        )}
        {grouped.map((g) => {
          const collapsed = g.id === "archived" && !showArchived && !filter.trim();
          return (
            <section key={g.id} className="group">
              {g.id === "archived" ? (
                <h2 className="group-label archived">
                  <button
                    className="group-toggle"
                    aria-expanded={!collapsed}
                    onClick={() => setShowArchived((v) => !v)}
                  >
                    <span className="caret" aria-hidden="true">{collapsed ? "▸" : "▾"}</span>
                    <span>{g.label}</span>
                  </button>
                  <span className="count">{g.items.length}</span>
                </h2>
              ) : (
                <h2 className={`group-label ${g.id}`}>
                  <span>{g.label}</span>
                  <span className="count">{g.items.length}</span>
                </h2>
              )}
              {!collapsed && g.items.map((p) => row(p, false))}
            </section>
          );
        })}
      </div>

      <footer className="hosts">
        <h2 className="group-label hosts-label">
          <button className="group-toggle" aria-expanded={hostsOpen} aria-controls="host-list" onClick={toggleHosts}>
            <span className="caret" aria-hidden="true">{hostsOpen ? "▾" : "▸"}</span>
            <span>HOSTS</span>
            {hosts.length > 0 && (
              <span className={connected < hosts.length ? "hosts-summary warn" : "hosts-summary"}>
                {connected < hosts.length ? `${connected} of ${hosts.length} connected` : `${connected} connected`}
              </span>
            )}
          </button>
          {(behind.length > 0 || updating.size > 0) && (
            <button
              className="hosts-update"
              aria-label={updateText}
              disabled={updating.size > 0}
              onClick={() => void updateAll()}
              title={
                updating.size > 0
                  ? `Updating otterd on ${[...updating].join(", ")}…`
                  : `Update otterd on ${behind.map((h) => h.name).join(", ")} to ${appVersion}; sessions keep running`
              }
            >
              <svg width="11" height="11" viewBox="0 0 12 12" aria-hidden="true">
                <path d="M6 10.5V4M3.2 6.5L6 3.7l2.8 2.8M2 1.5h8" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
              <span className="hosts-update-label">{updateText}</span>
            </button>
          )}
          <button className="icon-btn" aria-label="Add a host" title="Add a host" onClick={onAddHost}>
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
              <path d="M6 1.5v9M1.5 6h9" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
          </button>
        </h2>
        {hostsOpen && (
          <div id="host-list" className="host-list">
            {hosts.map((h) => (
              <button
                key={h.name}
                className={h.name === hostPage ? "host-row selected" : "host-row"}
                aria-current={h.name === hostPage ? "page" : undefined}
                onClick={() => onHost(h.name)}
                title={[h.describe, h.version && `otterd ${h.version}`, h.message, failed[h.name]].filter(Boolean).join(" — ")}
              >
                <span className={`host-dot ${h.status}`} />
                <span className="host-name">{h.name}</span>
                {updating.has(h.name) ? (
                  <span className="host-state">updating…</span>
                ) : failed[h.name] ? (
                  <span className="host-state warn">update failed</span>
                ) : (
                  <span className="host-state">{HOST_STATE[h.status]}</span>
                )}
                {h.version && appVersion && h.version !== appVersion && (
                  <span className="host-version">otterd {h.version}</span>
                )}
              </button>
            ))}
          </div>
        )}
        <div className="sidebar-foot">
          {props.update && (
            <button
              className="btn small-btn update-btn"
              onClick={props.onUpdate}
              disabled={installing(props.update)}
              title={props.update.error ?? `Restarts Otter into v${props.update.version}; your sessions keep running`}
            >
              <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
                <path d="M6 1.5v6.5M3.2 5.5L6 8.3l2.8-2.8M2 10.5h8" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
              {updateLabel(props.update)}
            </button>
          )}
          <span className="spacer" />
          {appVersion && <span className="app-version">v{appVersion}</span>}
        </div>
      </footer>
      {menu && (
        <ContextMenu x={menu.x} y={menu.y} items={props.pinMenu(menu.key)} onClose={() => setMenu(null)} />
      )}
    </nav>
  );
}

/** "N need you" / "N working": dim at zero; otherwise jumps to the first one. */
function CountBadge({ kind, count, label, onClick }: { kind: "needs" | "working"; count: number; label: string; onClick: () => void }) {
  return (
    <button
      className={count > 0 ? `count-badge ${kind} on` : `count-badge ${kind}`}
      disabled={count === 0}
      aria-label={`${count} ${label}`}
      title={`${count} ${label}`}
      onClick={onClick}
    >
      <span className="count-dot" />
      {count}
    </button>
  );
}
