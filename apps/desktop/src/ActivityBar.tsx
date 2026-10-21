import { useRef, useState, type KeyboardEvent } from "react";
import { ContextMenu, type MenuItem } from "./Menu";
import { GearIcon } from "./SettingsDialog";
import { VIEWS, viewForArrow, type View } from "./nav";

interface Props {
  view: View;
  onView: (view: View) => void;
  /** How many items need the developer, per view (a dot when > 0). */
  badges: Partial<Record<View, number>>;
  /** Settings menu: Otter's model and keys, theme, hosts, command-line tools. */
  settings: MenuItem[];
}

/** The leftmost column (VS Code-style): one icon per view, Settings at the bottom (D-041). */
export function ActivityBar({ view, onView, badges, settings }: Props) {
  const buttons = useRef(new Map<View, HTMLButtonElement>());
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

  const onKeyDown = (e: KeyboardEvent) => {
    const next = viewForArrow(view, e.key);
    if (!next) return;
    e.preventDefault();
    onView(next);
    buttons.current.get(next)?.focus();
  };

  return (
    <nav className="activity-bar" aria-label="Views">
      <div className="activity-top" data-tauri-drag-region />
      <div role="tablist" aria-orientation="vertical" aria-label="Views" className="activity-views" onKeyDown={onKeyDown}>
        {VIEWS.map((v) => {
          const count = badges[v.id] ?? 0;
          const active = v.id === view;
          return (
            <button
              key={v.id}
              ref={(b) => {
                if (b) buttons.current.set(v.id, b);
              }}
              role="tab"
              id={`view-tab-${v.id}`}
              aria-selected={active}
              aria-controls={`view-${v.id}`}
              tabIndex={active ? 0 : -1}
              className={active ? "activity-btn active" : "activity-btn"}
              title={`${v.label} (⌘${v.digit})`}
              aria-label={count > 0 ? `${v.label}, ${count} need you` : v.label}
              onClick={() => onView(v.id)}
            >
              <ViewIcon view={v.id} />
              {count > 0 && <span className="activity-badge" aria-hidden="true" />}
            </button>
          );
        })}
      </div>
      <span className="spacer" />
      <button
        className="activity-btn"
        title="Settings"
        aria-label="Settings"
        aria-haspopup="menu"
        aria-expanded={menu !== null}
        onClick={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          setMenu(menu ? null : { x: r.right + 4, y: r.top - 8 * settings.length });
        }}
      >
        <GearIcon />
      </button>
      {menu && <ContextMenu x={menu.x} y={menu.y} items={settings} onClose={() => setMenu(null)} />}
    </nav>
  );
}

/** The bar's icons: 22px, a 2px line, as the gear below them (D-177). */
function ViewIcon({ view }: { view: View }) {
  return (
    <svg width="22" height="22" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      {view === "tasks" ? (
        // A speech bubble: talking a task through.
        <path d="M12 4.25c4.83 0 8.75 3.16 8.75 7.06s-3.92 7.06-8.75 7.06c-1.13 0-2.2-.17-3.19-.49L4.5 19.75l1.14-3.84c-1.48-1.25-2.39-2.9-2.39-4.6 0-3.9 3.92-7.06 8.75-7.06z" />
      ) : (
        // Stacked panes: places where work runs.
        <>
          <rect x="3.75" y="4.25" width="16.5" height="6.5" rx="2.25" />
          <rect x="3.75" y="13.25" width="16.5" height="6.5" rx="2.25" />
        </>
      )}
    </svg>
  );
}
