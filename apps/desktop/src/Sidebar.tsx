import { Glyph } from "./Glyph";
import { ago, groups, headline, workspaceGlyph } from "./model";
import type { HostView, Placed } from "./types";

interface Props {
  placed: Placed[];
  hosts: HostView[];
  selected?: string;
  onSelect: (key: string) => void;
  now: number;
  /** This app's version, to point out hosts running a different workd. */
  appVersion?: string;
  onAddHost: () => void;
  onHost: (name: string) => void;
}

const HOST_STATE: Record<HostView["status"], string> = {
  connecting: "connecting…",
  connected: "connected",
  unreachable: "unreachable · retrying",
  incompatible: "incompatible workd",
  not_installed: "workd not installed",
};

export function Sidebar({ placed, hosts, selected, onSelect, now, appVersion, onAddHost, onHost }: Props) {
  return (
    <nav className="sidebar" aria-label="Workspaces">
      <div className="sidebar-groups">
        {groups(placed).map((g) => (
          <section key={g.id} className="group">
            <h2 className={`group-label ${g.id}`}>
              <span>{g.label}</span>
              <span className="count">{g.items.length}</span>
            </h2>
            {g.items.map((p) => {
              const line = headline(p.ws);
              const stale = p.host.status !== "connected";
              return (
                <button
                  key={p.key}
                  className={`ws-row${p.key === selected ? " selected" : ""}${stale ? " stale" : ""}`}
                  aria-current={p.key === selected ? "page" : undefined}
                  onClick={() => onSelect(p.key)}
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
            })}
          </section>
        ))}
      </div>
      <footer className="hosts">
        <h2 className="group-label hosts-label">
          <span>HOSTS</span>
          <button className="icon-btn" aria-label="Add a host" title="Add a host" onClick={onAddHost}>
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
              <path d="M6 1.5v9M1.5 6h9" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
          </button>
        </h2>
        {hosts.map((h) => (
          <button
            key={h.name}
            className="host-row"
            onClick={() => onHost(h.name)}
            title={[h.describe, h.version && `workd ${h.version}`, h.message].filter(Boolean).join(" — ")}
          >
            <span className={`host-dot ${h.status}`} />
            <span className="host-name">{h.name}</span>
            <span className="host-state">{HOST_STATE[h.status]}</span>
            {h.version && appVersion && h.version !== appVersion && (
              <span className="host-version">workd {h.version}</span>
            )}
          </button>
        ))}
      </footer>
    </nav>
  );
}
