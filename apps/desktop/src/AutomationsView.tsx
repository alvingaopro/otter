import { useEffect, useMemo, useState, type MouseEvent, type ReactNode } from "react";
import { Dialog } from "./Dialog";
import { Glyph } from "./Glyph";
import { ContextMenu, Menu, type MenuItem } from "./Menu";
import { ago } from "./model";
import {
  OUTCOME_LABEL,
  findItem,
  groupAutomations,
  isOpen,
  kindLabel,
  localTime,
  policyText,
  scheduleText,
  triggerLine,
  watchGlyph,
  watchLine,
  watchState,
  watchTarget,
  type Item,
  type PlacedTrigger,
  type PlacedWatch,
} from "./automations";
import type { PlacedTask } from "./tasks";
import type { TaskSource } from "./taskSource";
import type { TriggerSource } from "./triggerSource";

interface Props {
  /** Every task of every connected host: their watches (D-183). */
  tasks: PlacedTask[];
  /** Where a watch is stopped or deleted (`task.act`). */
  source: TaskSource;
  /** The hosts' triggers (D-083). */
  triggers: TriggerSource;
  now: number;
  /** A workspace's name on a host, for a trigger's. */
  workspaceName?: (host: string, id: string) => string | undefined;
  /** Show a task in Chat. */
  onOpenTask: (key: string) => void;
  /** The list's resize handle: the same width as the other lists (D-097). */
  grip?: ReactNode;
}

/**
 * What Otter does without a message from you (D-185), under Automations in
 * the Activity Bar: the watches it keeps for you and the hosts' triggers —
 * where they stand, what they did, and stopping or deleting them.
 */
export function AutomationsView({ tasks, source, triggers, now, workspaceName, onOpenTask, grip }: Props) {
  const [triggerList, setTriggerList] = useState<PlacedTrigger[]>(() => triggers.list());
  useEffect(() => {
    setTriggerList(triggers.list());
    return triggers.subscribe(setTriggerList);
  }, [triggers]);
  const groups = useMemo(() => groupAutomations(tasks, triggerList), [tasks, triggerList]);
  const [selected, setSelected] = useState<string | undefined>();
  const first = groups.watching[0]?.key ?? groups.scheduled[0]?.key ?? groups.history[0]?.key;
  const current = findItem(groups, selected) ?? findItem(groups, first);
  const [menu, setMenu] = useState<{ x: number; y: number; item: Item } | null>(null);
  const [deleting, setDeleting] = useState<Item | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = (p: Promise<void>) => {
    setError(null);
    p.catch((e) => setError(String(e)));
  };
  const actions = (it: Item): MenuItem[] => {
    if (it.kind === "watch") {
      const { task, watch } = it.item;
      return [
        { label: "Stop watching", hidden: !isOpen(watch), onSelect: () => run(source.act(task.key, { action: "cancel_watch", watch_id: watch.id })) },
        { label: "Open the task", onSelect: () => onOpenTask(task.key) },
        { label: "Delete…", danger: true, onSelect: () => setDeleting(it) },
      ];
    }
    const { key, trigger } = it.item;
    return [
      { label: trigger.enabled ? "Pause" : "Resume", onSelect: () => run(triggers.setEnabled(key, !trigger.enabled)) },
      { label: "Run now", onSelect: () => run(triggers.run(key)) },
      { label: "Delete…", danger: true, onSelect: () => setDeleting(it) },
    ];
  };
  const openMenu = (e: MouseEvent, item: Item) => {
    e.preventDefault();
    setMenu({ x: e.clientX, y: e.clientY, item });
  };
  const empty = groups.watching.length + groups.scheduled.length + groups.history.length === 0;

  const watchRow = (p: PlacedWatch, ended: boolean) => (
    <button
      key={p.key}
      role="listitem"
      className={p.key === currentKey(current) ? "ws-row selected" : "ws-row"}
      aria-current={p.key === currentKey(current)}
      onClick={() => setSelected(p.key)}
      onContextMenu={(e) => openMenu(e, { kind: "watch", item: p })}
    >
      <span className="ws-glyph">
        <Glyph kind={watchGlyph(p.watch)} />
      </span>
      <span className="ws-text">
        <span className="ws-line">
          <span className="ws-name">{p.watch.label}</span>
          <span className="ws-host">{p.host}</span>
        </span>
        <span className="ws-line sub">
          <span className="ws-summary">{watchLine(p, now)}</span>
          {ended && <span className="ws-age">{ago(p.watch.ended_at ?? p.watch.created_at, now)}</span>}
        </span>
      </span>
    </button>
  );

  return (
    <>
      <nav className="sidebar" aria-label="Automations">
        <div className="sidebar-top" data-tauri-drag-region>
          <span className="brand-name" data-tauri-drag-region>
            Automations
          </span>
          <span className="spacer" data-tauri-drag-region />
        </div>
        <div className="sidebar-groups">
          {empty && <p className="muted small sidebar-none">Nothing yet.</p>}
          {groups.watching.length > 0 && (
            <section className="group" aria-label="Watching">
              <h2 className="group-label working">
                <span>WATCHING</span>
                <span className="count">{groups.watching.length}</span>
              </h2>
              <div role="list">{groups.watching.map((p) => watchRow(p, false))}</div>
            </section>
          )}
          {groups.scheduled.length > 0 && (
            <section className="group" aria-label="Scheduled">
              <h2 className="group-label">
                <span>SCHEDULED</span>
                <span className="count">{groups.scheduled.length}</span>
              </h2>
              <div role="list">
                {groups.scheduled.map((p) => (
                  <button
                    key={p.key}
                    role="listitem"
                    className={["ws-row", p.key === currentKey(current) ? "selected" : "", p.trigger.enabled ? "" : "stale"].filter(Boolean).join(" ")}
                    aria-current={p.key === currentKey(current)}
                    onClick={() => setSelected(p.key)}
                    onContextMenu={(e) => openMenu(e, { kind: "trigger", item: p })}
                  >
                    <span className="ws-glyph">
                      <ClockGlyph />
                    </span>
                    <span className="ws-text">
                      <span className="ws-line">
                        <span className="ws-name">{p.trigger.name}</span>
                        <span className="ws-host">{p.host}</span>
                      </span>
                      <span className="ws-line sub">
                        <span className="ws-summary">{triggerLine(p.trigger, now)}</span>
                      </span>
                    </span>
                  </button>
                ))}
              </div>
            </section>
          )}
          {groups.history.length > 0 && (
            <section className="group" aria-label="History">
              <h2 className="group-label archived">
                <span>HISTORY</span>
                <span className="count">{groups.history.length}</span>
              </h2>
              <div role="list">{groups.history.map((p) => watchRow(p, true))}</div>
            </section>
          )}
        </div>
      </nav>
      {grip}
      {current?.kind === "watch" ? (
        <WatchPane
          key={current.item.key}
          p={current.item}
          now={now}
          error={error}
          actions={actions(current)}
          onStop={() => run(source.act(current.item.task.key, { action: "cancel_watch", watch_id: current.item.watch.id }))}
          onOpenTask={onOpenTask}
        />
      ) : current?.kind === "trigger" ? (
        <TriggerPane
          key={current.item.key}
          p={current.item}
          now={now}
          error={error}
          actions={actions(current)}
          tasks={tasks}
          workspace={workspaceName?.(current.item.host, current.item.trigger.workspace_id) ?? current.item.trigger.workspace_id}
          onToggle={() => run(triggers.setEnabled(current.item.key, !current.item.trigger.enabled))}
          onOpenTask={onOpenTask}
        />
      ) : (
        <main className="pane empty">
          <div className="drag-strip" data-tauri-drag-region />
          <h1 className="empty-title">Nothing automatic yet</h1>
          <p className="muted">
            Ask Otter in a task to keep an eye on something — “tell me when CI is done”, “look again in 30 minutes” — and it shows here, with
            what it found. A host's triggers show here too.
          </p>
        </main>
      )}
      {menu && <ContextMenu x={menu.x} y={menu.y} items={actions(menu.item)} onClose={() => setMenu(null)} />}
      {deleting && (
        <DeleteDialog
          item={deleting}
          onDelete={() =>
            deleting.kind === "watch"
              ? source.act(deleting.item.task.key, { action: "delete_watch", watch_id: deleting.item.watch.id })
              : triggers.remove(deleting.item.key)
          }
          onClose={() => setDeleting(null)}
        />
      )}
    </>
  );
}

function currentKey(it: Item | undefined): string | undefined {
  return it?.item.key;
}

/** A trigger's mark: a clock face, in the idle colour. */
function ClockGlyph() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true" className="glyph">
      <circle cx="6" cy="6" r="4.75" fill="none" stroke="var(--idle)" strokeWidth="1.4" />
      <path d="M6 3.4V6l1.8 1.2" fill="none" stroke="var(--idle)" strokeWidth="1.4" strokeLinecap="round" />
    </svg>
  );
}

function WatchPane({
  p,
  now,
  error,
  actions,
  onStop,
  onOpenTask,
}: {
  p: PlacedWatch;
  now: number;
  error: string | null;
  actions: MenuItem[];
  onStop: () => void;
  onOpenTask: (key: string) => void;
}) {
  const w = p.watch;
  const target = watchTarget(w);
  const readings = [...(w.readings ?? [])].reverse();
  return (
    <main className="pane automation-pane" aria-label={w.label}>
      <header className="pane-head" data-tauri-drag-region>
        <Glyph kind={watchGlyph(w)} />
        <h1 className="task-title">{w.label}</h1>
        <span className={`status-pill ${watchGlyph(w)}`}>{watchState(w)}</span>
        <span className="task-where">{p.host}</span>
        <span className="spacer" />
        {isOpen(w) && (
          <button className="btn outline small-btn" onClick={onStop}>
            Stop watching
          </button>
        )}
        <Menu label="More actions" items={actions} />
      </header>
      {error && (
        <p className="notice error" role="alert">
          {error}
        </p>
      )}
      <div className="automation-body">
        <section>
          <h2 className="ov-title">Asked</h2>
          <p className="ov-text">{w.what}</p>
          <p className="muted small automation-from">
            {w.asked_in ? "You asked in " : "Set when you accepted before CI finished, in "}
            <button className="link-btn" onClick={() => onOpenTask(p.task.key)}>
              {p.task.task.title}
            </button>
            {` · ${ago(w.created_at, now)} ago`}
          </p>
        </section>
        <dl className="automation-facts">
          <dt>Watches</dt>
          <dd>{target ? `${kindLabel(w.kind)} · ${target}` : kindLabel(w.kind)}</dd>
          <dt>{w.kind === "time" ? "Due" : "Until"}</dt>
          <dd>{localTime(w.deadline, now)}</dd>
          {w.ended_at && (
            <>
              <dt>{w.status === "cancelled" ? "Stopped" : "Ended"}</dt>
              <dd>{localTime(w.ended_at, now)}</dd>
            </>
          )}
        </dl>
        {w.finding && (
          <section>
            <h2 className="ov-title">Came to</h2>
            <p className="ov-outcome">
              <Glyph kind={watchGlyph(w)} />
              <span>
                {OUTCOME_LABEL[w.finding.outcome]}: {w.finding.text}
              </span>
            </p>
            <p className="muted small">Read at {localTime(w.finding.at, now)}.</p>
            {w.report && (
              <button className="link-btn" onClick={() => onOpenTask(p.task.key)}>
                Otter's report, in the task
              </button>
            )}
          </section>
        )}
        <section>
          <h2 className="ov-title">
            Readings <span className="count">{readings.length}</span>
          </h2>
          {readings.length === 0 ? (
            <p className="muted small">{w.kind === "time" ? "Nothing to read: the time is what it waits for." : "Not read yet."}</p>
          ) : (
            <ol className="plain-list automation-readings" aria-label="Readings, newest first">
              {readings.map((r, i) => (
                <li key={`${r.at}-${i}`}>
                  <span className="automation-at">{localTime(r.at, now)}</span>
                  <span>{r.text}</span>
                </li>
              ))}
            </ol>
          )}
          <p className="muted small automation-note">Each change of what Otter read, not every look.</p>
        </section>
      </div>
    </main>
  );
}

function TriggerPane({
  p,
  now,
  error,
  actions,
  tasks,
  workspace,
  onToggle,
  onOpenTask,
}: {
  p: PlacedTrigger;
  now: number;
  error: string | null;
  actions: MenuItem[];
  tasks: PlacedTask[];
  workspace: string;
  onToggle: () => void;
  onOpenTask: (key: string) => void;
}) {
  const t = p.trigger;
  const firings = [...t.history].reverse();
  const title = (id: string) => tasks.find((x) => x.host === p.host && x.task.id === id)?.task.title;
  return (
    <main className="pane automation-pane" aria-label={t.name}>
      <header className="pane-head" data-tauri-drag-region>
        <ClockGlyph />
        <h1 className="task-title">{t.name}</h1>
        <span className={t.enabled ? "status-pill working" : "status-pill"}>{t.enabled ? "on" : "paused"}</span>
        <span className="task-where">{p.host}</span>
        <span className="spacer" />
        <button className="btn outline small-btn" onClick={onToggle}>
          {t.enabled ? "Pause" : "Resume"}
        </button>
        <Menu label="More actions" items={actions} />
      </header>
      {error && (
        <p className="notice error" role="alert">
          {error}
        </p>
      )}
      <div className="automation-body">
        <section>
          <h2 className="ov-title">Starts a task</h2>
          <p className="ov-text">
            <strong>{t.title}</strong>
          </p>
          <p className="ov-text automation-request">{t.request}</p>
        </section>
        <dl className="automation-facts">
          <dt>When</dt>
          <dd>{scheduleText(t.schedule)}</dd>
          <dt>Next</dt>
          <dd>{!t.enabled ? "paused" : t.next_at ? localTime(t.next_at, now) : "only when run by hand"}</dd>
          <dt>Workspace</dt>
          <dd>{workspace}</dd>
          <dt>Without you</dt>
          <dd>{policyText(t.policy).join("; ")}</dd>
        </dl>
        <section>
          <h2 className="ov-title">
            Runs <span className="count">{firings.length}</span>
          </h2>
          {firings.length === 0 ? (
            <p className="muted small">It hasn't run yet.</p>
          ) : (
            <ol className="plain-list automation-readings" aria-label="Runs, newest first">
              {firings.map((f) => (
                <li key={f.occurrence}>
                  <span className="automation-at">{localTime(f.occurrence, now)}</span>
                  {f.task_id ? (
                    <button className="link-btn" onClick={() => onOpenTask(`${p.host}/${f.task_id}`)}>
                      {title(f.task_id) ?? f.task_id}
                    </button>
                  ) : (
                    <span className="muted">Skipped: {f.skipped ?? "no task"}</span>
                  )}
                </li>
              ))}
            </ol>
          )}
        </section>
      </div>
    </main>
  );
}

function DeleteDialog({ item, onDelete, onClose }: { item: Item; onDelete: () => Promise<void>; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const watch = item.kind === "watch";
  const name = watch ? item.item.watch.label : item.item.trigger.name;
  return (
    <Dialog title={watch ? "Delete watch" : "Delete trigger"} onClose={onClose}>
      <p>
        Delete <strong>{name}</strong> from {item.item.host}?{" "}
        {watch
          ? isOpen(item.item.watch)
            ? "It stops watching, and its readings are removed."
            : "Its readings are removed; Otter's report stays in the task."
          : "It won't start any more tasks; the tasks it started stay."}
      </p>
      {error && <p className="error-text small">{error}</p>}
      <div className="form-actions">
        <span className="spacer" />
        <button type="button" className="btn outline" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="btn danger"
          disabled={busy}
          onClick={async () => {
            setBusy(true);
            try {
              await onDelete();
              onClose();
            } catch (e) {
              setError(String(e));
              setBusy(false);
            }
          }}
        >
          Delete
        </button>
      </div>
    </Dialog>
  );
}
