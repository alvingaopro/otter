import { useRef, useState, type KeyboardEvent } from "react";
import { ContextMenu, type MenuItem } from "./Menu";
import { VIEWS, viewForArrow, type View } from "./nav";

interface Props {
  view: View;
  onView: (view: View) => void;
  /** How many items need the developer, per view (a dot when > 0). */
  badges: Partial<Record<View, number>>;
  /** Settings menu: theme, hosts, command-line tools. */
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
        <svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true">
          <circle cx="9" cy="9" r="2.4" fill="none" stroke="currentColor" strokeWidth="1.5" />
          <path
            d="M9 1.8v2M9 14.2v2M1.8 9h2M14.2 9h2M3.9 3.9l1.4 1.4M12.7 12.7l1.4 1.4M3.9 14.1l1.4-1.4M12.7 5.3l1.4-1.4"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
        </svg>
      </button>
      {menu && <ContextMenu x={menu.x} y={menu.y} items={settings} onClose={() => setMenu(null)} />}
    </nav>
  );
}

function ViewIcon({ view }: { view: View }) {
  if (view === "features") {
    // A flag: a goal to reach.
    return (
      <svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true">
        <path d="M4 16V2.5" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        <path d="M4 3h9.5l-2 3.25 2 3.25H4" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" />
      </svg>
    );
  }
  // Stacked panes: places where work runs.
  return (
    <svg width="18" height="18" viewBox="0 0 18 18" aria-hidden="true">
      <rect x="2.5" y="3" width="13" height="5" rx="1.5" fill="none" stroke="currentColor" strokeWidth="1.5" />
      <rect x="2.5" y="10" width="13" height="5" rx="1.5" fill="none" stroke="currentColor" strokeWidth="1.5" />
    </svg>
  );
}
