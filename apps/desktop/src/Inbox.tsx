import { useEffect, useState } from "react";
import { Decision } from "./TasksView";
import { taskGlyph, type TaskAction, type PlacedTask } from "./tasks";
import type { TaskSource } from "./taskSource";
import { Glyph } from "./Glyph";
import { inboxGroups, inboxItems, type InboxItem } from "./inboxModel";
import { ago, workspaceGlyph } from "./model";
import type { Placed } from "./types";

interface Props {
  source: TaskSource;
  /** Workspaces on every host, as the Workspaces view gets them. */
  workspaces: Placed[];
  now: number;
  onOpenTask: (key: string) => void;
  onOpenWorkspace: (key: string) => void;
}

/**
 * Everything going on, in one list (design §26, D-072): what needs you,
 * longest-waiting first, with what to do right there; then what is working;
 * then what is done.
 */
export function Inbox({ source, workspaces, now, onOpenTask, onOpenWorkspace }: Props) {
  const [tasks, setTasks] = useState<PlacedTask[]>(() => source.list());
  useEffect(() => {
    setTasks(source.list());
    return source.subscribe(setTasks);
  }, [source]);
  const groups = inboxGroups(inboxItems(tasks, workspaces));

  return (
    <main className="pane inbox" aria-label="Inbox">
      <div className="drag-strip" data-tauri-drag-region />
      <div className="inbox-body">
        <h1 className="inbox-title">Inbox</h1>
        {groups.length === 0 ? (
          <p className="muted">Nothing is going on. Start a task, or a workspace, and it shows up here.</p>
        ) : (
          groups.map((g) => (
            <section key={g.id} className="group inbox-group" aria-label={g.label}>
              <h2 className={`group-label ${g.id}`}>
                <span>{g.label}</span>
                <span className="count">{g.items.length}</span>
              </h2>
              {g.id === "working" && g.items.some((i) => i.task) && <PauseAll source={source} items={g.items} />}
              <ul className="plain-list inbox-items">
                {g.items.map((item) => (
                  <InboxRow
                    key={item.id}
                    item={item}
                    source={source}
                    now={now}
                    onOpen={() => (item.task ? onOpenTask(item.task.key) : onOpenWorkspace(item.workspace!.key))}
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

/** Hold every task at work at once (D-081): one control for all the agents. */
function PauseAll({ source, items }: { source: TaskSource; items: InboxItem[] }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pause = async () => {
    setBusy(true);
    setError(null);
    const failed: string[] = [];
    for (const i of items) {
      if (!i.task) continue;
      await source.act(i.task.key, { action: "pause" }).catch((e) => failed.push(`${i.title}: ${String(e)}`));
    }
    if (failed.length > 0) setError(failed.join("; "));
    setBusy(false);
  };
  return (
    <div className="inbox-group-actions">
      <button className="btn outline small-btn" disabled={busy} onClick={() => void pause()} title="Hold every task at work: what runs is interrupted, nothing new starts">
        Pause all tasks
      </button>
      {error && (
        <span className="error-text small" role="alert">
          {error}
        </span>
      )}
    </div>
  );
}

function InboxRow({ item, source, now, onOpen }: { item: InboxItem; source: TaskSource; now: number; onOpen: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const act = (a: TaskAction) => {
    if (!item.task || busy) return;
    setBusy(true);
    setError(null);
    source
      .act(item.task.key, a)
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  };
  const glyph = item.task ? taskGlyph(item.task.task) : workspaceGlyph(item.workspace!.ws);
  const decision =
    item.need?.decision_id !== undefined
      ? item.task?.task.decisions.find((d) => d.id === item.need!.decision_id && d.status === "pending")
      : undefined;
  return (
    <li className="inbox-item" aria-label={item.title}>
      <div className="inbox-head">
        <span className="ws-glyph">
          <Glyph kind={glyph} />
        </span>
        <span className="inbox-name">{item.title}</span>
        <span className="tag">{item.task ? "Task" : "Workspace"}</span>
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
