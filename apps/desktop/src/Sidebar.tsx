import { useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { Glyph } from "./Glyph";
import { ContextMenu, type MenuItem } from "./Menu";
import { ago, groups, headline, pinnedOf, workspaceGlyph } from "./model";
import type { ThemeChoice } from "./theme";
import type { HostView, Placed } from "./types";
import otterIcon from "./assets/otter.png";

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
  themeChoice: ThemeChoice;
  onTheme: () => void;
  /** Set when the command-line tools are missing or another version. */
  cliAction?: string;
  onCli: () => void;
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

function storedHostsOpen(): boolean {
  try {
    return localStorage.getItem(HOSTS_OPEN_KEY) !== "false";
  } catch {
    return true;
  }
}

export function Sidebar(props: Props) {
  const { placed, hosts, selected, onSelect, now, appVersion, onAddHost, onHost, hostPage } = props;
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
  // Reordering pins with the pointer (HTML5 drag-and-drop is taken by file drops).
  const [drag, setDrag] = useState<{ key: string; to: number } | null>(null);
  const dragged = useRef(false);
  const pinRows = useRef(new Map<string, HTMLElement>());

  const pinned = pinnedOf(placed, props.pins);
  const pinnedKeys = pinned.map((p) => p.key);
  const rest = placed.filter((p) => !props.pins.includes(p.key));
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
        <ThemeButton choice={props.themeChoice} onClick={props.onTheme} />
        <button className="icon-btn" aria-label="New workspace" title="New workspace (⌘N)" onClick={props.onNew}>
          <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
            <path d="M7 2v10M2 7h10" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
        </button>
      </div>

      <div className="brand" data-tauri-drag-region>
        <img src={otterIcon} width={24} height={24} alt="" className="brand-icon" />
        <span className="brand-name" data-tauri-drag-region>Otter</span>
        <span className="spacer" data-tauri-drag-region />
        <CountBadge kind="needs" count={needs.length} label="need you" onClick={() => onSelect(needs[0].key)} />
        <CountBadge kind="working" count={working.length} label="working" onClick={() => onSelect(working[0].key)} />
      </div>

      <div className="sidebar-groups">
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
          const collapsed = g.id === "archived" && !showArchived;
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
                title={[h.describe, h.version && `otterd ${h.version}`, h.message].filter(Boolean).join(" — ")}
              >
                <span className={`host-dot ${h.status}`} />
                <span className="host-name">{h.name}</span>
                <span className="host-state">{HOST_STATE[h.status]}</span>
                {h.version && appVersion && h.version !== appVersion && (
                  <span className="host-version">otterd {h.version}</span>
                )}
              </button>
            ))}
          </div>
        )}
        <div className="sidebar-foot">
          {props.cliAction && (
            <button className="btn small-btn cli-btn" onClick={props.onCli}>
              <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
                <path d="M6 1.5v6.5M3.2 5.5L6 8.3l2.8-2.8M2 10.5h8" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
              </svg>
              {props.cliAction}
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

function ThemeButton({ choice, onClick }: { choice: ThemeChoice; onClick: () => void }) {
  return (
    <button
      className="icon-btn"
      onClick={onClick}
      aria-label={`Theme: ${choice}`}
      title={`Theme: ${choice === "system" ? "match system" : choice} (click to change)`}
    >
      {choice === "light" ? (
        <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
          <circle cx="8" cy="8" r="3" fill="none" stroke="currentColor" strokeWidth="1.5" />
          <path d="M8 1.5v1.5M8 13v1.5M1.5 8H3M13 8h1.5M3.4 3.4l1 1M11.6 11.6l1 1M3.4 12.6l1-1M11.6 4.4l1-1" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        </svg>
      ) : choice === "dark" ? (
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
  );
}
