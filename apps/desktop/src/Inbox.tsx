import { useEffect, useState } from "react";
import { Decision } from "./FeaturesView";
import { featureGlyph, type FeatureAction, type PlacedFeature } from "./features";
import type { FeatureSource } from "./featureSource";
import { Glyph } from "./Glyph";
import { inboxGroups, inboxItems, type InboxItem } from "./inboxModel";
import { ago, workspaceGlyph } from "./model";
import type { Placed } from "./types";

interface Props {
  source: FeatureSource;
  /** Workspaces on every host, as the Workspaces view gets them. */
  workspaces: Placed[];
  now: number;
  onOpenFeature: (key: string) => void;
  onOpenWorkspace: (key: string) => void;
}

/**
 * Everything going on, in one list (design §26, D-072): what needs you,
 * longest-waiting first, with what to do right there; then what is working;
 * then what is done.
 */
export function Inbox({ source, workspaces, now, onOpenFeature, onOpenWorkspace }: Props) {
  const [features, setFeatures] = useState<PlacedFeature[]>(() => source.list());
  useEffect(() => {
    setFeatures(source.list());
    return source.subscribe(setFeatures);
  }, [source]);
  const groups = inboxGroups(inboxItems(features, workspaces));

  return (
    <main className="pane inbox" aria-label="Inbox">
      <div className="drag-strip" data-tauri-drag-region />
      <div className="inbox-body">
        <h1 className="inbox-title">Inbox</h1>
        {groups.length === 0 ? (
          <p className="muted">Nothing is going on. Start a feature, or a workspace, and it shows up here.</p>
        ) : (
          groups.map((g) => (
            <section key={g.id} className="group inbox-group" aria-label={g.label}>
              <h2 className={`group-label ${g.id}`}>
                <span>{g.label}</span>
                <span className="count">{g.items.length}</span>
              </h2>
              <ul className="plain-list inbox-items">
                {g.items.map((item) => (
                  <InboxRow
                    key={item.id}
                    item={item}
                    source={source}
                    now={now}
                    onOpen={() => (item.feature ? onOpenFeature(item.feature.key) : onOpenWorkspace(item.workspace!.key))}
                  />
                ))}
              </ul>
            </section>
          ))
        )}
      </div>
    </main>
  );
}

function InboxRow({ item, source, now, onOpen }: { item: InboxItem; source: FeatureSource; now: number; onOpen: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const act = (a: FeatureAction) => {
    if (!item.feature || busy) return;
    setBusy(true);
    setError(null);
    source
      .act(item.feature.key, a)
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  };
  const glyph = item.feature ? featureGlyph(item.feature.feature) : workspaceGlyph(item.workspace!.ws);
  const decision =
    item.need?.decision_id !== undefined
      ? item.feature?.feature.decisions.find((d) => d.id === item.need!.decision_id && d.status === "pending")
      : undefined;
  return (
    <li className="inbox-item" aria-label={item.title}>
      <div className="inbox-head">
        <span className="ws-glyph">
          <Glyph kind={glyph} />
        </span>
        <span className="inbox-name">{item.title}</span>
        <span className="tag">{item.feature ? "Feature" : "Workspace"}</span>
        <span className="ws-host">{item.host}</span>
        <span className="spacer" />
        <span className="ws-age" title={item.since}>
          {ago(item.since, now)}
        </span>
      </div>
      {decision ? (
        <>
          {item.need && item.need.decisions > 1 && <p className="muted small">{item.need.decisions} decisions wait; the oldest first.</p>}
          <Decision d={decision} act={act} now={now} />
        </>
      ) : (
        <p className="inbox-summary">{item.summary}</p>
      )}
      <div className="decision-actions">
        {item.need?.kind === "review" && (
          <button className="btn primary" disabled={busy} onClick={() => act({ action: "accept" })}>
            Accept
          </button>
        )}
        {item.need?.kind === "failed" && (
          <button className="btn primary" disabled={busy} onClick={() => act({ action: "retry" })}>
            Retry
          </button>
        )}
        <button className="btn outline" onClick={onOpen}>
          Open
        </button>
      </div>
      {error && (
        <p className="error-text small" role="alert">
          {error}
        </p>
      )}
    </li>
  );
}
