import { useEffect, useMemo, useState, type MouseEvent, type ReactNode } from "react";
import { Dialog } from "./Dialog";
import { Glyph } from "./Glyph";
import { Markdown } from "./Markdown";
import { ContextMenu, Menu, type MenuItem } from "./Menu";
import { ago } from "./model";
import {
  actionText,
  findItem,
  groupAutomations,
  isOpen,
  kindLabel,
  localTime,
  nextText,
  originText,
  outcomeLabel,
  pastDeadline,
  policyText,
  refusalText,
  scheduleText,
  triggerLine,
  untilText,
  watchGlyph,
  watchLine,
  watchState,
  watchTarget,
  type Firing,
  type Item,
  type OtterAction,
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
  /** Open Settings at what Otter may do on a host (D-190). */
  onSettings?: (host: string) => void;
  /** In front: what Otter did is read only then, as it reads every task's history. */
  active?: boolean;
  /** The list's resize handle: the same width as the other lists (D-097). */
  grip?: ReactNode;
}

/** What Otter did on a host is selected by this key. */
const LEDGER = "did:";
const ledgerHost = (key: string | undefined) => (key?.startsWith(LEDGER) ? key.slice(LEDGER.length) : undefined);

/** What the page shows: a watch, a trigger, or what Otter did on a host. */
type Shown = Item | { kind: "ledger"; host: string };

/** What an action said, about the item it was on. */
type Said = { key: string; error: string } | { key: string; firing: Firing };

/**
 * What Otter does without a message from you (D-185), under Automations in
 * the Activity Bar: the watches it keeps for you and the hosts' triggers —
 * where they stand, what they did, and stopping or deleting them — and what
 * Otter did on its tools, with Undo where something undoes it (D-192).
 */
export function AutomationsView({ tasks, source, triggers, now, workspaceName, onOpenTask, onSettings, active = true, grip }: Props) {
  const [triggerList, setTriggerList] = useState<PlacedTrigger[]>(() => triggers.list());
  useEffect(() => {
    setTriggerList(triggers.list());
    return triggers.subscribe(setTriggerList);
  }, [triggers]);
  const groups = useMemo(() => groupAutomations(tasks, triggerList), [tasks, triggerList]);
  const ledger = useLedger(tasks, source, active);
  const ledgerHosts = Object.keys(ledger.byHost).sort();
  const [selected, setSelected] = useState<string | undefined>();
  const first =
    groups.watching[0]?.key ?? groups.scheduled[0]?.key ?? groups.history[0]?.key ?? (ledgerHosts[0] !== undefined ? LEDGER + ledgerHosts[0] : undefined);
  const shown = (key: string | undefined): Shown | undefined => {
    const host = ledgerHost(key);
    if (host !== undefined) return ledger.byHost[host] ? { kind: "ledger", host } : undefined;
    return findItem(groups, key);
  };
  const current = shown(selected) ?? shown(first);
  const currentKey = current?.kind === "ledger" ? LEDGER + current.host : current?.item.key;
  const [menu, setMenu] = useState<{ x: number; y: number; item: Item } | null>(null);
  const [deleting, setDeleting] = useState<Item | null>(null);
  /** An action on its way: nothing more is sent until it's back (a second Run now, a second Stop). */
  const [busy, setBusy] = useState(false);
  const [said, setSaid] = useState<Said | null>(null);
  // What was said is about the item it was on: another one starts clean.
  useEffect(() => setSaid(null), [currentKey]);

  const run = (key: string, action: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    setSaid(null);
    action()
      .catch((e) => setSaid({ key, error: String(e) }))
      .finally(() => setBusy(false));
  };
  const stopWatch = (p: PlacedWatch) => run(p.key, () => source.act(p.task.key, { action: "cancel_watch", watch_id: p.watch.id }));
  const setEnabled = (p: PlacedTrigger) => run(p.key, () => triggers.setEnabled(p.key, !p.trigger.enabled));
  const runNow = (p: PlacedTrigger) =>
    run(p.key, async () => {
      // Its own occurrence, taken now: sent twice, still one task.
      const t = await triggers.run(p.key, new Date().toISOString());
      const firing = t.history[t.history.length - 1];
      if (firing) setSaid({ key: p.key, firing });
    });
  const actions = (it: Item): MenuItem[] => {
    if (it.kind === "watch") {
      return [
        { label: "Stop watching", hidden: busy || !isOpen(it.item.watch), onSelect: () => stopWatch(it.item) },
        { label: "Open the task", onSelect: () => onOpenTask(it.item.task.key) },
        { label: "Delete…", danger: true, hidden: busy, onSelect: () => setDeleting(it) },
      ];
    }
    return [
      { label: it.item.trigger.enabled ? "Pause" : "Resume", hidden: busy, onSelect: () => setEnabled(it.item) },
      { label: "Run now", hidden: busy, onSelect: () => runNow(it.item) },
      { label: "Delete…", danger: true, hidden: busy, onSelect: () => setDeleting(it) },
    ];
  };
  const openMenu = (e: MouseEvent, item: Item) => {
    e.preventDefault();
    // An action in flight: no menu of nothing.
    if (busy) return;
    setMenu({ x: e.clientX, y: e.clientY, item });
  };
  const empty = groups.watching.length + groups.scheduled.length + groups.history.length + ledgerHosts.length === 0;
  const saidHere = said && said.key === currentKey ? said : null;
  const taskOf = (host: string, id: string) => tasks.find((x) => x.host === host && x.task.id === id);

  const watchRow = (p: PlacedWatch, ended: boolean) => (
    <button
      key={p.key}
      role="listitem"
      className={p.key === currentKey ? "ws-row selected" : "ws-row"}
      aria-current={p.key === currentKey}
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
                    className={["ws-row", p.key === currentKey ? "selected" : "", p.trigger.enabled ? "" : "stale"].filter(Boolean).join(" ")}
                    aria-current={p.key === currentKey}
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
          {ledgerHosts.length > 0 && (
            <section className="group" aria-label="What Otter did">
              <h2 className="group-label archived">
                <span>WHAT OTTER DID</span>
              </h2>
              <div role="list">
                {ledgerHosts.map((host) => {
                  const key = LEDGER + host;
                  const latest = ledger.byHost[host][0];
                  return (
                    <button
                      key={key}
                      role="listitem"
                      className={key === currentKey ? "ws-row selected" : "ws-row"}
                      aria-current={key === currentKey}
                      onClick={() => setSelected(key)}
                    >
                      <span className="ws-glyph">
                        <LedgerGlyph />
                      </span>
                      <span className="ws-text">
                        <span className="ws-line">
                          <span className="ws-name">Otter's actions</span>
                          <span className="ws-host">{host}</span>
                        </span>
                        <span className="ws-line sub">
                          <span className="ws-summary">{latest ? `${actionText(latest.tool)} · ${latest.task_title}` : "Nothing yet"}</span>
                          {latest && <span className="ws-age">{ago(latest.at, now)}</span>}
                        </span>
                      </span>
                    </button>
                  );
                })}
              </div>
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
          busy={busy}
          error={saidHere && "error" in saidHere ? saidHere.error : null}
          actions={actions(current)}
          onStop={() => stopWatch(current.item)}
          onOpenTask={onOpenTask}
        />
      ) : current?.kind === "ledger" ? (
        <LedgerPane
          key={currentKey}
          host={current.host}
          actions={ledger.byHost[current.host]}
          now={now}
          busy={busy}
          error={saidHere && "error" in saidHere ? saidHere.error : null}
          undone={ledger.undone}
          taskOf={taskOf}
          onUndo={(a) =>
            run(LEDGER + current.host, async () => {
              await source.act(`${current.host}/${a.undo!.task_id}`, a.undo!.action);
              ledger.markUndone(current.host, a);
            })
          }
          onOpenTask={onOpenTask}
          onSettings={onSettings}
        />
      ) : current?.kind === "trigger" ? (
        <TriggerPane
          key={current.item.key}
          p={current.item}
          now={now}
          busy={busy}
          said={saidHere}
          actions={actions(current)}
          taskOf={taskOf}
          workspace={workspaceName?.(current.item.host, current.item.trigger.workspace_id) ?? current.item.trigger.workspace_id}
          onToggle={() => setEnabled(current.item)}
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

/** How long after the last change of a task what Otter did is read again: changes come in bursts. */
const LEDGER_SETTLE_MS = 300;

/**
 * What Otter did on each host (D-192), read while the view is in front and
 * again when a task's history grows (that's where it's kept). A host whose
 * otterd can't say isn't in it; one that fails to answer once keeps what it
 * said last.
 */
function useLedger(tasks: PlacedTask[], source: TaskSource, active: boolean) {
  const [byHost, setByHost] = useState<Record<string, OtterAction[]>>({});
  /** Undone from here, by `host/task/seq`: Undo isn't offered twice. */
  const [undone, setUndone] = useState<Set<string>>(() => new Set());
  const [reload, setReload] = useState(0);
  // Every task's history, as far as it goes: any change can be one of Otter's.
  const histories = tasks.map((p) => `${p.key}:${p.task.history_seq ?? p.task.updated_at}`).join("\n");
  useEffect(() => {
    const read = source.actions?.bind(source);
    if (!read || !active) return;
    const hosts = [...new Set(tasks.map((p) => p.host))];
    let live = true;
    const t = setTimeout(() => {
      for (const host of hosts) {
        read(host)
          .then((list) => {
            if (!live) return;
            setByHost((m) => {
              const { [host]: _, ...rest } = m;
              return list ? { ...rest, [host]: list } : rest;
            });
          })
          .catch(() => {
            // A hiccup: what it said last stays.
          });
      }
    }, LEDGER_SETTLE_MS);
    return () => {
      live = false;
      clearTimeout(t);
    };
    // `histories` stands for the tasks.
  }, [source, histories, active, reload]);
  // A host gone from the list goes from here too.
  const hosts = new Set(tasks.map((p) => p.host));
  const shown = Object.fromEntries(Object.entries(byHost).filter(([h]) => hosts.has(h)));
  return {
    byHost: shown,
    undone,
    markUndone(host: string, a: OtterAction) {
      setUndone((s) => new Set(s).add(`${host}/${a.task_id}/${a.seq}`));
      setReload((n) => n + 1);
    },
  };
}

/** What Otter did's mark: lines of a list, in the idle colour. */
function LedgerGlyph() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true" className="glyph">
      <path d="M2 3h8M2 6h8M2 9h5" fill="none" stroke="var(--idle)" strokeWidth="1.4" strokeLinecap="round" />
    </svg>
  );
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

function ErrorNotice({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <p className="notice error" role="alert">
      {error}
    </p>
  );
}

function WatchPane({
  p,
  now,
  busy,
  error,
  actions,
  onStop,
  onOpenTask,
}: {
  p: PlacedWatch;
  now: number;
  busy: boolean;
  error: string | null;
  actions: MenuItem[];
  onStop: () => void;
  onOpenTask: (key: string) => void;
}) {
  const w = p.watch;
  const target = watchTarget(w);
  const readings = [...(w.readings ?? [])].reverse();
  const report = w.report ? p.task.task.messages.find((m) => m.id === w.report) : undefined;
  const overdue = pastDeadline(w, now);
  return (
    <main className="pane automation-pane" aria-label={w.label}>
      <header className="pane-head" data-tauri-drag-region>
        <Glyph kind={watchGlyph(w)} />
        <h1 className="task-title">{w.label}</h1>
        <span className={`status-pill ${watchGlyph(w)}`}>{watchState(w)}</span>
        <span className="task-where">{p.host}</span>
        <span className="spacer" />
        {isOpen(w) && (
          <button className="btn outline small-btn" disabled={busy} onClick={onStop}>
            Stop watching
          </button>
        )}
        <Menu label="More actions" items={actions} />
      </header>
      <ErrorNotice error={error} />
      <div className="automation-body">
        <section>
          <h2 className="ov-title">Asked</h2>
          <p className="ov-text">{w.what}</p>
          <p className="muted small automation-from">
            {w.asked_in ? "You asked in " : "Set by Otter in "}
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
          <dd>{overdue ? untilText(w, now) : localTime(w.deadline, now)}</dd>
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
                {outcomeLabel(w.finding.outcome)}: {w.finding.text}
              </span>
            </p>
            <p className="muted small">Read at {localTime(w.finding.at, now)}.</p>
          </section>
        )}
        {w.report && (
          <section aria-label="Otter's report">
            <h2 className="ov-title">Otter said</h2>
            {report ? <Markdown className="automation-report" text={report.text} /> : <p className="muted small">It isn't in the task any more.</p>}
            <button className="link-btn automation-open" onClick={() => onOpenTask(p.task.key)}>
              Open the task
            </button>
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
  busy,
  said,
  actions,
  taskOf,
  workspace,
  onToggle,
  onOpenTask,
}: {
  p: PlacedTrigger;
  now: number;
  busy: boolean;
  said: Said | null;
  actions: MenuItem[];
  taskOf: (host: string, id: string) => PlacedTask | undefined;
  workspace: string;
  onToggle: () => void;
  onOpenTask: (key: string) => void;
}) {
  const t = p.trigger;
  const firings = [...t.history].reverse();
  const ran = said && "firing" in said ? said.firing : undefined;
  return (
    <main className="pane automation-pane" aria-label={t.name}>
      <header className="pane-head" data-tauri-drag-region>
        <ClockGlyph />
        <h1 className="task-title">{t.name}</h1>
        <span className={t.enabled ? "status-pill working" : "status-pill"}>{t.enabled ? "on" : "paused"}</span>
        <span className="task-where">{p.host}</span>
        <span className="spacer" />
        <button className="btn outline small-btn" disabled={busy} onClick={onToggle}>
          {t.enabled ? "Pause" : "Resume"}
        </button>
        <Menu label="More actions" items={actions} />
      </header>
      <ErrorNotice error={said && "error" in said ? said.error : null} />
      {ran && (
        <p className="notice" role="status">
          {ran.task_id ? `Started ${taskOf(p.host, ran.task_id)?.task.title ?? ran.task_id}.` : `Skipped: ${ran.skipped ?? "nothing started"}.`}
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
          <dd>{scheduleText(t.schedule, now)}</dd>
          <dt>Next</dt>
          <dd>{nextText(t, now)}</dd>
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
              {firings.map((f) => {
                const started = f.task_id ? taskOf(p.host, f.task_id) : undefined;
                return (
                  <li key={f.occurrence}>
                    <span className="automation-at">{localTime(f.occurrence, now)}</span>
                    {started ? (
                      <button className="link-btn" onClick={() => onOpenTask(started.key)}>
                        {started.task.title}
                      </button>
                    ) : f.task_id ? (
                      <span className="muted">{f.task_id} (task deleted)</span>
                    ) : (
                      <span className="muted">Skipped: {f.skipped ?? "no task"}</span>
                    )}
                  </li>
                );
              })}
            </ol>
          )}
        </section>
      </div>
    </main>
  );
}

/** The words of your message Otter acted on, when the task is here to say. */
function asked(task: PlacedTask | undefined, id: string | undefined): string | undefined {
  const text = id ? task?.task.messages.find((m) => m.id === id)?.text.trim() : undefined;
  if (!text) return undefined;
  const line = text.split("\n")[0];
  return line.length > 80 || text.includes("\n") ? `${line.slice(0, 80).trimEnd()}…` : line;
}

/**
 * What Otter did on a host's tasks (D-192), newest first: each thing in a
 * few words, on which task, who started it, and Undo where something undoes
 * it.
 */
function LedgerPane({
  host,
  actions,
  now,
  busy,
  error,
  undone,
  taskOf,
  onUndo,
  onOpenTask,
  onSettings,
}: {
  host: string;
  actions: OtterAction[];
  now: number;
  busy: boolean;
  error: string | null;
  undone: Set<string>;
  taskOf: (host: string, id: string) => PlacedTask | undefined;
  onUndo: (a: OtterAction) => void;
  onOpenTask: (key: string) => void;
  onSettings?: (host: string) => void;
}) {
  return (
    <main className="pane automation-pane" aria-label={`What Otter did on ${host}`}>
      <header className="pane-head" data-tauri-drag-region>
        <LedgerGlyph />
        <h1 className="task-title">What Otter did</h1>
        <span className="task-where">{host}</span>
        <span className="spacer" />
      </header>
      <ErrorNotice error={error} />
      <div className="automation-body">
        <section>
          <p className="muted small automation-from">
            What Otter did with its tools in {host}'s tasks: on its own where a tool is automatic, or on your word. Undo puts back what can be put back.
          </p>
          {onSettings && (
            <button className="link-btn ledger-settings" onClick={() => onSettings(host)}>
              Change what Otter may do
            </button>
          )}
        </section>
        <section>
          {actions.length === 0 ? (
            <p className="muted small">Nothing yet.</p>
          ) : (
            <ol className="plain-list automation-readings ledger" aria-label="What Otter did, newest first">
              {actions.map((a) => {
                const task = taskOf(host, a.task_id);
                const said = asked(task, a.authority);
                const done = undone.has(`${host}/${a.task_id}/${a.seq}`);
                return (
                  <li key={`${a.task_id}/${a.seq}`} className={a.error ? "ledger-row refused" : "ledger-row"}>
                    <span className="automation-at">{localTime(a.at, now)}</span>
                    <span className="ledger-text">
                      <span className="ledger-line">
                        <span className="ledger-what">{actionText(a.tool)}</span>
                        {a.error && (
                          <span className="ledger-error" title="It did nothing">
                            <Glyph kind="failed" />
                            {refusalText(a.error)}
                          </span>
                        )}
                        <span className="muted">·</span>
                        {task ? (
                          <button className="link-btn" onClick={() => onOpenTask(task.key)}>
                            {a.task_title}
                          </button>
                        ) : (
                          <span>{a.task_title}</span>
                        )}
                      </span>
                      {a.detail && <span className="ledger-detail">{a.detail}</span>}
                      <span className="muted small">
                        {originText(a.origin)}
                        {said && `: “${said}”`}
                      </span>
                    </span>
                    {a.undo &&
                      (done ? (
                        <span className="muted small ledger-undo">Undone</span>
                      ) : (
                        <button className="btn outline small-btn ledger-undo" disabled={busy} onClick={() => onUndo(a)}>
                          Undo
                        </button>
                      ))}
                  </li>
                );
              })}
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
  // While it's being deleted it stays open: what comes of it is said here.
  const close = () => {
    if (!busy) onClose();
  };
  return (
    <Dialog title={watch ? "Delete watch" : "Delete trigger"} onClose={close}>
      <p>
        Delete <strong>{name}</strong> from {item.item.host}?{" "}
        {watch
          ? isOpen(item.item.watch)
            ? "It stops watching, and its readings are removed."
            : "Its readings are removed; Otter's report stays in the task."
          : "It won't start any more tasks; the tasks it started stay."}
      </p>
      {error && (
        <p className="error-text small" role="alert">
          {error}
        </p>
      )}
      <div className="form-actions">
        <span className="spacer" />
        <button type="button" className="btn outline" disabled={busy} onClick={close}>
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
