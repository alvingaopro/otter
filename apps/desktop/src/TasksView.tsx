import { Fragment, useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from "react";
import { Dialog } from "./Dialog";
import { Glyph } from "./Glyph";
import { ComposerGrip, useComposerHeight } from "./composerResize";
import { Markdown } from "./Markdown";
import { MemoryDialog } from "./MemoryDialog";
import { Menu } from "./Menu";
import { ago } from "./model";
import otterIcon from "./assets/otter.png";
import type { CodingConversation, Interaction, RuntimeCapabilities } from "./conversation";
import type { TaskSource } from "./taskSource";
import { QuestionForm, Turn, WorkHead } from "./Work";
import {
  GROUPS,
  STATUS_LABEL,
  budgetBinds,
  collapseRepeats,
  findMatch,
  groupOf,
  taskGlyph,
  pendingDecisions,
  sortTasks,
  statusDetail,
  type DecisionRequest,
  type Evidence,
  type Task,
  type TaskAction,
  type TaskEventRecord,
  type PlacedTask,
  type WorkspaceChoice,
} from "./tasks";

interface Props {
  source: TaskSource;
  /** Hosts a task can be created on. */
  hosts: string[];
  /** Workspaces per host, to work in. */
  workspaces: Record<string, WorkspaceChoice[]>;
  now: number;
  /** A task to show, asked for from elsewhere (the Inbox); `at` makes asking again count. */
  focus?: { key: string; at: number };
  /** Show a workspace (and session) in the Workspaces view. */
  onOpenWorkspace: (host: string, workspaceId: string, sessionId?: string) => void;
  /** Open a preview port of a host in the browser (forwarding it if remote). */
  onOpenPreview: (host: string, port: number, path: string) => Promise<void>;
  /** The list's resize handle (D-097): the same width as the Workspaces list. */
  grip?: ReactNode;
}

/** The Tasks view (D-041, D-042): product work, independent of the Workspace view. */
export function TasksView({ source, hosts, workspaces, now, focus, onOpenWorkspace, onOpenPreview, grip }: Props) {
  const [list, setList] = useState<PlacedTask[]>(() => source.list());
  const [find, setFind] = useState("");
  const [selected, setSelected] = useState<string | undefined>();
  const [creating, setCreating] = useState(false);
  // Opened from elsewhere (the Inbox): show that one.
  useEffect(() => {
    if (!focus) return;
    setSelected(focus.key);
    setFind("");
  }, [focus]);

  useEffect(() => {
    setList(source.list());
    return source.subscribe(setList);
  }, [source]);

  // Grouped as the list shows them (D-114): what needs you, what is going on, what is over.
  const groups = useMemo(() => {
    const shown = sortTasks(list).filter((p) => findMatch(p, find));
    return GROUPS.map((g) => ({ ...g, items: shown.filter((p) => groupOf(p.task) === g.id) })).filter((g) => g.items.length > 0);
  }, [list, find]);
  const first = groups[0]?.items[0];
  const current = list.find((p) => p.key === selected) ?? first;

  return (
    <>
      <nav className="sidebar" aria-label="Tasks">
        <div className="sidebar-top" data-tauri-drag-region>
          <span className="brand-name">Tasks</span>
          <span className="spacer" />
          <button className="icon-btn" aria-label="New task" title="New task" onClick={() => setCreating(true)}>
            <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
              <path d="M7 2v10M2 7h10" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
            </svg>
          </button>
        </div>
        {list.length > 0 && (
          <div className="sidebar-filter">
            <input
              type="search"
              aria-label="Find a task"
              placeholder="Find a task"
              value={find}
              onChange={(e) => setFind(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  setFind("");
                  e.currentTarget.blur();
                } else if (e.key === "Enter" && first) {
                  setSelected(first.key);
                }
              }}
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
            />
          </div>
        )}
        <div className="sidebar-groups">
          {list.length === 0 && <p className="muted small sidebar-none">No tasks yet.</p>}
          {list.length > 0 && groups.length === 0 && <p className="muted small sidebar-none">No task matches.</p>}
          {groups.map((g) => (
            <section key={g.id} className="group" aria-label={g.label}>
              <h2 className={`group-label ${g.id}`}>
                <span>{g.label.toUpperCase()}</span>
                <span className="count">{g.items.length}</span>
              </h2>
              <div role="list">
                {g.items.map((p) => (
                  <button
                    key={p.key}
                    role="listitem"
                    className={p.key === current?.key ? "ws-row selected" : "ws-row"}
                    aria-current={p.key === current?.key}
                    onClick={() => setSelected(p.key)}
                  >
                    <span className="ws-glyph">
                      <Glyph kind={taskGlyph(p.task)} />
                    </span>
                    <span className="ws-text">
                      <span className="ws-line">
                        <span className="ws-name">{p.task.title}</span>
                        <span className="ws-host">{p.host}</span>
                      </span>
                      <span className="ws-line sub">
                        <span className="ws-summary">{rowSummary(p.task)}</span>
                        <span className="ws-age">{ago(p.task.updated_at, now)}</span>
                      </span>
                    </span>
                  </button>
                ))}
              </div>
            </section>
          ))}
        </div>
      </nav>
      {grip}
      {current ? (
        <TaskPane
          key={current.key}
          placed={current}
          source={source}
          now={now}
          workspaces={workspaces[current.host] ?? []}
          onOpenWorkspace={onOpenWorkspace}
          onOpenPreview={onOpenPreview}
        />
      ) : (
        <main className="pane empty">
          <div className="drag-strip" data-tauri-drag-region />
          <h1 className="empty-title">Describe a task</h1>
          <p className="muted">Say what you want: a task, a fix or an answer. Otter plans it, directs a coding agent in a workspace and verifies the result.</p>
          <div className="empty-actions">
            <button className="btn primary" onClick={() => setCreating(true)}>
              New task
            </button>
          </div>
        </main>
      )}
      {creating && (
        <NewTaskDialog
          hosts={hosts.length > 0 ? hosts : ["preview"]}
          workspaces={workspaces}
          onClose={() => setCreating(false)}
          onCreate={async (host, title, request, workspace, answerOnly) => {
            const p = await source.create(host, title, request, workspace, answerOnly);
            setFind("");
            setSelected(p.key);
            setCreating(false);
          }}
        />
      )}
    </>
  );
}

function rowSummary(f: Task): string {
  const pending = pendingDecisions(f).length;
  if (pending > 0) return `${pending} decision${pending > 1 ? "s" : ""} waiting`;
  if (f.status_reason) return f.status_reason;
  const work = f.steps.filter((t) => !t.planning);
  const done = work.filter((t) => t.status === "done").length;
  if (work.length > 0) return `${STATUS_LABEL[f.status]} · ${done}/${work.length} steps`;
  return STATUS_LABEL[f.status];
}

type Tab = "overview" | "activity";

const ENDED = ["done", "cancelled"];

function TaskPane({
  placed,
  source,
  now,
  workspaces,
  onOpenWorkspace,
  onOpenPreview,
}: {
  placed: PlacedTask;
  source: TaskSource;
  now: number;
  workspaces: WorkspaceChoice[];
  onOpenWorkspace: Props["onOpenWorkspace"];
  onOpenPreview: Props["onOpenPreview"];
}) {
  const f = placed.task;
  const wsName = workspaces.find((w) => w.id === f.workspace_id)?.name;
  const [deleting, setDeleting] = useState(false);
  const [remembering, setRemembering] = useState(false);
  const [tab, setTab] = useState<Tab>("overview");
  const [error, setError] = useState<string | null>(null);
  // The coding agent's side (D-055…D-059), and what its runtime can do.
  const [conversations, setConversations] = useState<CodingConversation[]>([]);
  const [caps, setCaps] = useState<RuntimeCapabilities | null>(null);
  useEffect(() => {
    let live = true;
    const load = () =>
      void source.conversations?.(placed.key).then((c) => live && setConversations(c));
    load();
    const stop = source.watchConversations?.(placed.key, load);
    return () => {
      live = false;
      stop?.();
    };
  }, [source, placed.key, f.updated_at]);
  useEffect(() => {
    let live = true;
    void source.capabilities?.(placed.host).then((c) => live && setCaps(c));
    return () => {
      live = false;
    };
  }, [source, placed.host]);
  const interactions = useMemo(
    () => new Map(conversations.flatMap((c) => c.interactions.map((i) => [i.id, i] as const))),
    [conversations],
  );

  const act = (action: TaskAction) => {
    setError(null);
    source.act(placed.key, action).catch((e) => setError(String(e)));
  };

  return (
    <main className="pane task-pane" aria-label={f.title}>
      <header className="pane-head" data-tauri-drag-region>
        <Glyph kind={taskGlyph(f)} />
        <h1 className="task-title">{f.title}</h1>
        <span className={`status-pill ${taskGlyph(f)}`}>{STATUS_LABEL[f.status]}</span>
        {f.answer_only && (
          <span className="chip" title="The coding agent only reads; a plan to change things comes to you first">
            Answer only
          </span>
        )}
        <span className="task-where">
          {placed.host}
          {f.workspace_id && (
            <>
              {" / "}
              <button className="link-btn" title="Open the workspace" onClick={() => onOpenWorkspace(placed.host, f.workspace_id!)}>
                {wsName ?? f.workspace_id}
              </button>
            </>
          )}
        </span>
        <span className="spacer" />
        {f.workspace_id && !ENDED.includes(f.status) && (
          <button
            className="btn"
            title={f.preview ? `Port ${f.preview.port} on ${placed.host}` : "Start the app's preview in its workspace"}
            onClick={async () => {
              setError(null);
              try {
                if (!f.preview) await source.act(placed.key, { action: "preview" });
                const p = source.list().find((x) => x.key === placed.key)?.task.preview ?? f.preview;
                if (p) await onOpenPreview(placed.host, p.port, p.path);
              } catch (e) {
                setError(String(e));
              }
            }}
          >
            {f.preview ? "Open preview" : "Preview"}
          </button>
        )}
        <TaskActions f={f} act={act} caps={caps} />
        <Menu
          label="More actions"
          items={[
            { label: "Memory", onSelect: () => setRemembering(true), hidden: !source.memory },
            { label: "Open workspace", onSelect: () => onOpenWorkspace(placed.host, f.workspace_id!), hidden: !f.workspace_id },
            { label: "Cancel task", onSelect: () => act({ action: "cancel" }), hidden: ENDED.includes(f.status) },
            { label: "Delete task…", onSelect: () => setDeleting(true), danger: true, hidden: !deletable(f) },
          ]}
        />
      </header>
      {remembering && source.memory && (
        <MemoryDialog
          source={source.memory}
          host={placed.host}
          task={f.id}
          conversation="This task"
          onClose={() => setRemembering(false)}
        />
      )}
      {deleting && (
        <DeleteTaskDialog
          placed={placed}
          workspace={wsName ?? f.workspace_id}
          onDelete={() => source.remove(placed.key)}
          onClose={() => setDeleting(false)}
        />
      )}
      {source.preview && (
        <p className="notice" role="note">
          Preview with sample data: nothing here runs.
        </p>
      )}
      <StatusBanner f={f} act={act} />
      {!f.workspace_id && !ENDED.includes(f.status) && (
        <div className="banner info" role="group" aria-label="Workspace">
          <span className="banner-text">Choose the workspace this task's work happens in.</span>
          <span className="banner-actions">
            <select
              aria-label="Workspace"
              defaultValue=""
              onChange={(e) => e.target.value && act({ action: "set_workspace", workspace: e.target.value })}
            >
              <option value="" disabled>
                {workspaces.length ? "Workspace…" : "No workspaces on this host"}
              </option>
              {workspaces.map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </select>
          </span>
        </div>
      )}
      {error && <p className="notice error-text">{error}</p>}
      <div className="task-body">
        <Conversation placed={placed} source={source} now={now} caps={caps} act={act} interactions={interactions} onError={setError} />
        <section className="task-detail" aria-label="Details">
          <div className="detail-tabs" role="tablist" aria-label="Task details">
            {(["overview", "activity"] as const).map((t) => (
              <button
                key={t}
                role="tab"
                id={`ftab-${t}`}
                aria-selected={tab === t}
                aria-controls={`fpanel-${t}`}
                className={tab === t ? "detail-tab on" : "detail-tab"}
                onClick={() => setTab(t)}
              >
                {t === "overview" ? "Overview" : "Activity"}
              </button>
            ))}
          </div>
          <div className="detail-body" role="tabpanel" id={`fpanel-${tab}`} aria-labelledby={`ftab-${tab}`}>
            {tab === "overview" ? (
              <Overview placed={placed} now={now} onOpenWorkspace={onOpenWorkspace} shot={(name) => source.artifact(placed.key, name)} />
            ) : (
              <ActivityFeed placed={placed} source={source} conversations={conversations} now={now} />
            )}
          </div>
        </section>
      </div>
    </main>
  );
}

/** Not being worked on: nothing is planning, implementing, verifying or running. */
export function deletable(f: Task): boolean {
  return (
    !["planning", "implementing", "verifying"].includes(f.status) &&
    !f.runs.some((r) => ["starting", "running", "waiting"].includes(r.state))
  );
}

function DeleteTaskDialog({
  placed,
  workspace,
  onDelete,
  onClose,
}: {
  placed: PlacedTask;
  workspace?: string;
  onDelete: () => Promise<void>;
  onClose: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <Dialog title="Delete task" onClose={onClose}>
      <p>
        Delete <strong>{placed.task.title}</strong> from {placed.host}? Its conversation, plan, history and
        screenshots are removed for good.
      </p>
      {workspace && <p className="muted small">The workspace {workspace} and its files stay as they are.</p>}
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

/** What the task can be told to do now; cancelling and deleting are in the ⋯ menu. */
function TaskActions({
  f,
  act,
  caps,
}: {
  f: Task;
  act: (a: TaskAction) => void;
  caps?: RuntimeCapabilities | null;
}) {
  const live = ["planning", "implementing", "verifying", "blocked"].includes(f.status);
  const running = f.runs.some((r) => ["starting", "running", "waiting"].includes(r.state));
  return (
    <span className="task-actions">
      {running && caps?.features.interrupt_turn && (
        <button
          className="btn"
          title="Stop what the coding agent is doing now, and wait for you"
          onClick={() => act({ action: "interrupt" })}
        >
          Interrupt
        </button>
      )}
      {f.status === "draft" && (
        <button className="btn primary" onClick={() => act({ action: "start" })}>
          Start
        </button>
      )}
      {live && (
        <button className="btn" onClick={() => act({ action: "pause" })}>
          Pause
        </button>
      )}
      {f.status === "paused" && (
        <button className="btn primary" onClick={() => act({ action: "resume" })}>
          Resume
        </button>
      )}
      {f.status === "failed" && (
        <button className="btn primary" onClick={() => act({ action: "retry" })}>
          Retry
        </button>
      )}
      {f.status === "review" &&
        (gatesClear(f) ? (
          <button className="btn primary" onClick={() => act({ action: "accept" })}>
            Accept
          </button>
        ) : (
          <button
            className="btn outline"
            title="Some gates haven't passed; accepting anyway is recorded in the report"
            onClick={() => act({ action: "accept", override_gates: true })}
          >
            Accept anyway
          </button>
        ))}
    </span>
  );
}

/**
 * Only what has no other place: a stop, a block without a decision to
 * answer, waiting for a workspace. A decision waits above the message box.
 */
function StatusBanner({ f, act }: { f: Task; act: (a: TaskAction) => void }) {
  const pending = pendingDecisions(f).length;
  if (f.status === "blocked") {
    if (pending > 0) return null;
    return (
      <div className="banner needs" role="status">
        <span className="banner-text">
          <Glyph kind="needs" />
          <span className="banner-summary">{f.status_reason ?? "Waiting on you"}</span>
        </span>
      </div>
    );
  }
  if (f.status === "failed") {
    return (
      <div className="banner failure" role="status">
        <span className="banner-text">
          <Glyph kind="failed" />
          <span className="banner-summary">{f.status_reason ?? "The task failed"}</span>
        </span>
      </div>
    );
  }
  // Waiting for its workspace (D-079): or a worktree of its own (D-080).
  if (pending === 0 && f.status_reason?.startsWith("Waiting for ")) {
    return (
      <div className="banner info" role="status">
        <span className="banner-text">
          <Glyph kind="working" />
          <span className="banner-summary">{f.status_reason}</span>
        </span>
        {(f.status_reason.endsWith("is working there") || f.status_reason.endsWith("a workspace of its own")) && f.runs.length === 0 && (
          <span className="banner-actions">
            <button className="btn" title="A worktree of the same repository, on a branch for this task" onClick={() => act({ action: "own_workspace" })}>
              Work in its own workspace
            </button>
          </span>
        )}
      </div>
    );
  }
  return null;
}

const ROLE: Record<string, string> = { user: "You", controller: "Otter", agent: "Coding agent", system: "System" };

/** Who said it; Otter with its face. */
export function Speaker({ role }: { role: string }) {
  return (
    <span className="message-role">
      {role === "controller" && <img src={otterIcon} width={16} height={16} alt="" className="message-avatar" />}
      {ROLE[role] ?? role}
    </span>
  );
}

/** What was said: the developer's words as typed; Otter's and the coding
 * agent's as the Markdown they write (D-086). */
export function MessageText({ role, text, children }: { role: string; text: string; children?: ReactNode }) {
  return role === "controller" || role === "agent" ? (
    <Markdown className="message-text" text={text}>
      {children}
    </Markdown>
  ) : (
    <div className="message-text">
      {text}
      {children}
    </div>
  );
}

/** A coding agent's report this long folds, so the conversation around it stays in view. */
const FOLD_CHARS = 1200;

function Folded({ long, children }: { long: boolean; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  if (!long) return <>{children}</>;
  return (
    <>
      <div className={open ? "fold open" : "fold"}>{children}</div>
      <button className="link-btn fold-toggle" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
        {open ? "Show less" : "Show all"}
      </button>
    </>
  );
}

function Conversation({
  placed,
  source,
  now,
  caps,
  act,
  interactions,
  onError,
}: {
  placed: PlacedTask;
  source: TaskSource;
  now: number;
  caps?: RuntimeCapabilities | null;
  act: (a: TaskAction) => void;
  interactions: Map<string, Interaction>;
  onError?: (e: string) => void;
}) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [inputHeight, setInputHeight] = useComposerHeight();
  const end = useRef<HTMLDivElement>(null);
  const messages = placed.task.messages;
  const pending = pendingDecisions(placed.task);
  // A decision taken shows where it was asked; one waiting, above the message box.
  const said = useMemo(() => {
    const decided = placed.task.decisions.filter((d) => d.status !== "pending");
    const items = [
      ...messages.map((m) => ({ at: Date.parse(m.at), key: m.id, message: m, decision: undefined })),
      ...decided.map((d) => ({ at: Date.parse(d.created_at), key: d.id, message: undefined, decision: d })),
    ];
    return items.sort((a, b) => a.at - b.at);
  }, [messages, placed.task.decisions]);
  const live = placed.task.runs.find((r) => ["starting", "running", "waiting"].includes(r.state));
  // Being written right now (D-051); a written one shows until its message loads.
  const drafts = (source.drafts?.(placed.key) ?? []).filter(
    (d) => !(d.done && messages.some((m) => m.role === d.role && m.text.trim() === d.text.trim())),
  );
  const draftText = drafts.map((d) => d.text).join("");
  const activity = live?.activity ?? [];
  // The developer spoke last: Otter's answer is on its way.
  const last = messages[messages.length - 1];
  const replying = drafts.some((d) => d.role === "controller");
  const awaitingReply =
    !replying && last?.role === "user" && messages.length > 1 && !ENDED.includes(placed.task.status);
  useEffect(
    () => end.current?.scrollIntoView?.({ block: "end" }),
    [messages.length, activity.length, awaitingReply, draftText.length],
  );

  async function submit(e: FormEvent) {
    e.preventDefault();
    const t = text.trim();
    if (!t || busy) return;
    setBusy(true);
    try {
      await source.send(placed.key, t);
      setText("");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="conversation" aria-label="Conversation">
      <div className="messages" role="log" aria-live="polite">
        {messages.length === 0 && <p className="muted small">No messages yet. Describe what you want, or refine the request.</p>}
        {said.map(({ key, message: m, decision }) =>
          m ? (
            <div key={key} className={`message ${m.role}`}>
              <div className="message-head">
                <Speaker role={m.role} />
                <span className="message-at">{ago(m.at, now)}</span>
              </div>
              <Folded long={m.role === "agent" && m.text.length > FOLD_CHARS}>
                <MessageText role={m.role} text={m.text} />
              </Folded>
            </div>
          ) : (
            <DecidedNote key={key} d={decision!} now={now} />
          ),
        )}
        {drafts.map((d) => (
          <div key={d.stream_id} className={`message ${d.role} streaming`}>
            <div className="message-head">
              <Speaker role={d.role} />
              <span className="message-at">{d.done ? "now" : "writing…"}</span>
            </div>
            <MessageText role={d.role} text={d.text}>
              {!d.done && <span className="cursor" aria-hidden="true" />}
            </MessageText>
          </div>
        ))}
        {live && (
          <div className="message agent working" aria-live="polite">
            <div className="message-head">
              <span className="message-role">Coding agent</span>
              <span className="message-at">{live.state === "waiting" ? "waiting for a decision" : "working…"}</span>
            </div>
            {activity.length > 0 && (
              <ul className="activity">
                {activity.slice(-6).map((a, i) => (
                  <li key={`${activity.length}-${i}`}>{a}</li>
                ))}
              </ul>
            )}
          </div>
        )}
        {awaitingReply && <p className="muted small replying">Otter is replying…</p>}
        <div ref={end} />
      </div>
      {pending.length > 0 && (
        <section className="pending-decisions" aria-label="Waiting for your decision">
          <ul className="plain-list decisions">
            {pending.map((d) => (
              <Decision key={d.id} d={d} act={act} now={now} interaction={interactions.get(d.id)} />
            ))}
          </ul>
        </section>
      )}
      <form className="composer" onSubmit={submit}>
        <ComposerGrip height={inputHeight} onResize={setInputHeight} />
        <textarea
          aria-label="Message to Otter"
          style={{ height: inputHeight }}
          placeholder={
            placed.task.status === "review" || placed.task.status === "done"
              ? "Happy with it? Or say what to change…"
              : "Tell Otter what you want…"
          }
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void submit(e as unknown as FormEvent);
            }
          }}
        />
        <span className="composer-actions">
          <button className="btn primary" type="submit" disabled={busy || !text.trim()}>
            Send
          </button>
          {live && caps?.features.redirect && (
            <button
              type="button"
              className="btn outline"
              title="Stop what the coding agent is doing and have it do this next"
              disabled={busy || !text.trim()}
              onClick={async () => {
                const t = text.trim();
                if (!t) return;
                setBusy(true);
                try {
                  await source.act(placed.key, { action: "redirect", text: t });
                  setText("");
                } catch (e) {
                  onError?.(String(e));
                } finally {
                  setBusy(false);
                }
              }}
            >
              Redirect now
            </button>
          )}
        </span>
      </form>
    </section>
  );
}

const VERDICT: Record<string, string> = { approved: "Approved", denied: "Denied", answered: "Answered", cancelled: "Withdrawn" };

function decider(d: DecisionRequest): string | undefined {
  if (d.decided_by === "user") return "you";
  if (d.decided_by === "controller") return "Otter";
  return d.decided_by;
}

/** A decision already taken, where it was asked in the conversation. */
function DecidedNote({ d, now }: { d: DecisionRequest; now: number }) {
  const by = decider(d);
  return (
    <div className="decided">
      <div className="decided-head">
        <Glyph kind={d.status === "denied" ? "failed" : d.status === "cancelled" ? "idle" : "done"} />
        <span className="decided-verdict">
          {VERDICT[d.status] ?? d.status}
          {by && ` by ${by}`}
        </span>
        <span className="muted small">
          {d.risk} risk · {d.kind.replace("_", " ")} · {ago(d.decided_at ?? d.created_at, now)}
        </span>
      </div>
      <div className="decided-summary">{d.summary}</div>
      {d.answer && <div className="muted small">{d.answer}</div>}
    </div>
  );
}

export function Decision({
  d,
  act,
  now,
  interaction,
}: {
  d: DecisionRequest;
  act: (a: TaskAction) => void;
  now: number;
  interaction?: Interaction;
}) {
  const [answer, setAnswer] = useState("");
  // A form of questions, as asked: each answered on its own (D-059).
  const form = d.status === "pending" && interaction?.type === "questions" && (interaction.questions?.length ?? 0) > 0;
  return (
    <li className={d.status === "pending" ? "decision pending" : "decision"}>
      <div className="decision-head">
        <span className={`risk ${d.risk}`}>{d.risk} risk</span>
        <span className="muted small">{d.kind.replace("_", " ")}</span>
        <span className="spacer" />
        <span className="muted small">{ago(d.created_at, now)}</span>
      </div>
      <div className="decision-summary">{d.summary}</div>
      {d.detail && <div className="muted small">{d.detail}</div>}
      {form ? (
        <QuestionForm
          interaction={interaction!}
          onAnswer={(answers) => act({ action: "decide", decision_id: d.id, approve: true, answers })}
        />
      ) : d.status === "pending" ? (
        d.options.length > 0 || d.kind === "question" ? (
          <div className="decision-actions">
            {d.options.map((o) => (
              <button key={o} className="btn" onClick={() => act({ action: "decide", decision_id: d.id, approve: true, answer: o })}>
                {o}
              </button>
            ))}
            {d.options.length === 0 && (
              <form
                className="decision-answer"
                onSubmit={(e) => {
                  e.preventDefault();
                  if (answer.trim()) act({ action: "decide", decision_id: d.id, approve: true, answer: answer.trim() });
                }}
              >
                <input aria-label="Answer" value={answer} onChange={(e) => setAnswer(e.target.value)} placeholder="Your answer" />
                <button className="btn primary" type="submit">
                  Answer
                </button>
              </form>
            )}
          </div>
        ) : (
          <div className="decision-actions">
            <button className="btn primary" onClick={() => act({ action: "decide", decision_id: d.id, approve: true })}>
              Approve
            </button>
            <button className="btn danger" onClick={() => act({ action: "decide", decision_id: d.id, approve: false })}>
              Deny
            </button>
          </div>
        )
      ) : (
        <div className="muted small">
          {d.status}
          {d.decided_by && ` by ${d.decided_by === "user" ? "you" : d.decided_by}`}
          {d.answer && `: ${d.answer}`}
          {d.rationale && ` — ${d.rationale}`}
        </div>
      )}
    </li>
  );
}

function Section({ title, count, children }: { title: string; count?: string; children: ReactNode }) {
  return (
    <section className="ov-section" aria-label={title}>
      <h2 className="ov-title">
        <span>{title}</span>
        {count && <span className="count">{count}</span>}
      </h2>
      {children}
    </section>
  );
}

/**
 * The task at a glance (D-114): only sections that have something, in the
 * order its stage needs them.
 */
function Overview({
  placed,
  now,
  onOpenWorkspace,
  shot,
}: {
  placed: PlacedTask;
  now: number;
  onOpenWorkspace: Props["onOpenWorkspace"];
  shot: (name: string) => Promise<string>;
}) {
  const f = placed.task;
  const parts: Record<string, ReactNode> = {
    now: <Now f={f} />,
    outcome: <Outcome f={f} />,
    progress: <Progress placed={placed} onOpenWorkspace={onOpenWorkspace} />,
    checks: <Checks f={f} now={now} shot={shot} />,
    delivery: <DeliveryInfo f={f} />,
    report: f.report ? (
      <Section title="Report">
        <Markdown className="report" text={f.report} />
      </Section>
    ) : null,
    decisions: <Decisions f={f} now={now} />,
    request: <Request f={f} />,
  };
  const order = ENDED.includes(f.status)
    ? ["outcome", "report", "checks", "delivery", "progress", "decisions", "request"]
    : f.status === "review"
      ? ["now", "checks", "delivery", "report", "progress", "decisions", "request"]
      : ["now", "progress", "checks", "delivery", "report", "decisions", "request"];
  return (
    <div className="overview">
      {order.map((k) => (
        <Fragment key={k}>{parts[k]}</Fragment>
      ))}
    </div>
  );
}

/** Where it stands; the budget only once it constrains the work. A stop or a block is in the banner. */
function Now({ f }: { f: Task }) {
  if (ENDED.includes(f.status)) return null;
  const detail = f.status === "failed" || f.status === "blocked" ? undefined : statusDetail(f);
  const line = f.status === "draft" ? "Not started: Start when the request says what you want." : detail;
  const budget = budgetBinds(f);
  if (!line && !budget) return null;
  return (
    <Section title="Now">
      {line && <p className="ov-text">{line}</p>}
      {budget && (
        <p className="muted small ov-budget">
          {f.budget.iterations_used} of {f.budget.max_iterations} attempts used · up to {f.budget.max_minutes} min
        </p>
      )}
    </Section>
  );
}

/** How it ended. */
function Outcome({ f }: { f: Task }) {
  if (!ENDED.includes(f.status)) return null;
  return (
    <Section title="Outcome">
      <p className="ov-text ov-outcome">
        <Glyph kind={f.status === "done" ? "done" : "idle"} />
        <span>{f.status_reason ?? STATUS_LABEL[f.status]}</span>
      </p>
    </Section>
  );
}

const STEP_GLYPH = { pending: "idle", running: "working", blocked: "needs", done: "done", failed: "failed", skipped: "idle" } as const;

/** The plan's steps; the coding agent's planning turn isn't one. */
function Progress({ placed, onOpenWorkspace }: { placed: PlacedTask; onOpenWorkspace: Props["onOpenWorkspace"] }) {
  const f = placed.task;
  const steps = f.steps.filter((t) => !t.planning);
  if (steps.length === 0) return null;
  const done = steps.filter((t) => t.status === "done").length;
  return (
    <Section title="Progress" count={`${done} of ${steps.length}`}>
      <ul className="plain-list steps">
        {steps.map((t) => (
          <li key={t.id} className="step">
            <Glyph kind={STEP_GLYPH[t.status]} />
            <span className="step-text">
              <span className="step-title">{t.title}</span>
              <span className="muted small">
                {t.status}
                {t.attempts > 1 && ` · attempt ${t.attempts}`}
                {t.depends_on.length > 0 && ` · after ${t.depends_on.map((d) => steps.findIndex((x) => x.id === d) + 1).join(", ")}`}
              </span>
              {t.last_error && <span className="error-text small">{t.last_error}</span>}
            </span>
            {t.workspace_id && (
              <button className="link-btn" onClick={() => onOpenWorkspace(placed.host, t.workspace_id!, t.session_id)}>
                {t.session_id ? "Open session" : "Open workspace"}
              </button>
            )}
          </li>
        ))}
      </ul>
    </Section>
  );
}

const GATE_GLYPH = { passed: "done", failed: "failed", pending: "working", not_applicable: "idle" } as const;

/** Every gate but the developer's own acceptance passed (or doesn't apply). */
function gatesClear(f: Task): boolean {
  const gates = f.gates ?? [];
  return gates.length > 0 && gates.filter((g) => g.name !== "Your acceptance").every((g) => g.status === "passed" || g.status === "not_applicable");
}

/**
 * What says the work is right: each acceptance criterion with the evidence
 * behind it, the gates (D-047), and evidence that backs no criterion.
 */
function Checks({ f, now, shot }: { f: Task; now: number; shot: (name: string) => Promise<string> }) {
  const gates = f.gates ?? [];
  const backing = (id: string, ids: string[]) => f.evidence.filter((e) => ids.includes(e.id) || e.criterion_id === id);
  const used = new Set(f.acceptance.flatMap((c) => backing(c.id, c.evidence).map((e) => e.id)));
  const rest = f.evidence.filter((e) => !used.has(e.id));
  if (f.acceptance.length === 0 && gates.length === 0 && rest.length === 0 && !f.verify_command) return null;
  const met = f.acceptance.filter((c) => c.met === true).length;
  return (
    <Section title="Checks" count={f.acceptance.length > 0 ? `${met} of ${f.acceptance.length}` : undefined}>
      {f.acceptance.length > 0 && (
        <ul className="plain-list checks">
          {f.acceptance.map((c) => (
            <li key={c.id} className="check">
              <Glyph kind={c.met === true ? "done" : c.met === false ? "failed" : "idle"} />
              <span className="step-text">
                <span>{c.text}</span>
                <span className="muted small">{c.met === undefined ? "not checked yet" : c.met ? "met" : "not met"}</span>
                {backing(c.id, c.evidence).map((e) => (
                  <EvidenceLine key={e.id} e={e} now={now} shot={shot} />
                ))}
              </span>
            </li>
          ))}
        </ul>
      )}
      {gates.length > 0 && (
        <ul className="plain-list checks gates">
          {gates.map((g) => (
            <li key={g.name} className="check">
              <Glyph kind={GATE_GLYPH[g.status]} />
              <span className="step-text">
                <span>{g.name}</span>
                <span className="muted small">
                  {g.status === "not_applicable" ? "n/a" : g.status}
                  {g.detail ? ` · ${g.detail}` : ""}
                </span>
              </span>
            </li>
          ))}
        </ul>
      )}
      {rest.length > 0 && (
        <ul className="plain-list checks">
          {rest.map((e) => (
            <li key={e.id} className="check">
              <EvidenceLine e={e} now={now} shot={shot} glyph />
            </li>
          ))}
        </ul>
      )}
      {f.verify_command && (
        <p className="muted small ov-check">
          Checked with <code>{f.verify_command}</code>
        </p>
      )}
    </Section>
  );
}

function EvidenceLine({ e, now, shot, glyph }: { e: Evidence; now: number; shot: (name: string) => Promise<string>; glyph?: boolean }) {
  return (
    <span className="evidence-item">
      {glyph && <Glyph kind={e.ok === true ? "done" : e.ok === false ? "failed" : "idle"} />}
      <span className="step-text">
        <span>
          {e.uri && !e.uri.startsWith("artifact:") ? (
            <a href={e.uri} target="_blank" rel="noreferrer">
              {e.title}
            </a>
          ) : (
            e.title
          )}
        </span>
        <span className="muted small">
          {e.kind.replace("_", " ")}
          {e.ok === true && " · passed"}
          {e.ok === false && " · failed"}
          {e.uncertain && " · judgment, not a check"} · {ago(e.at, now)}
        </span>
        {e.detail && <span className="muted small evidence-detail">{e.detail}</span>}
        {e.kind === "screenshot" && e.uri?.startsWith("artifact:") && <Shot load={() => shot(e.uri!)} alt={e.title} />}
      </span>
    </span>
  );
}

/** The change as published: its pull request and CI. */
function DeliveryInfo({ f }: { f: Task }) {
  const d = f.delivery;
  if (!d) return null;
  return (
    <Section title="Delivery">
      {d.declined ? (
        <p className="small muted">You chose not to publish; the change stays on {d.branch}.</p>
      ) : d.pr_url ? (
        <p className="small">
          <a href={d.pr_url} target="_blank" rel="noreferrer">
            {d.pr_url}
          </a>{" "}
          <span className="muted">· {d.branch}</span>
        </p>
      ) : (
        <p className="small muted">Waiting to publish {d.branch}.</p>
      )}
      {d.ci.length > 0 && (
        <ul className="plain-list small ci">
          {d.ci.map((c) => (
            <li key={c.name}>
              <Glyph kind={c.state === "pass" || c.state === "skipping" ? "done" : c.state === "pending" ? "working" : "failed"} /> {c.name}{" "}
              <span className="muted">{c.state}</span>
            </li>
          ))}
        </ul>
      )}
      {d.reruns > 0 && <p className="small muted">Reran flaky CI {d.reruns}×.</p>}
    </Section>
  );
}

/** Decisions already taken; one waiting is above the message box. */
function Decisions({ f, now }: { f: Task; now: number }) {
  const decided = f.decisions.filter((d) => d.status !== "pending");
  if (decided.length === 0) return null;
  return (
    <Section title="Decisions" count={String(decided.length)}>
      <ul className="plain-list checks">
        {[...decided].reverse().map((d) => (
          <li key={d.id} className="check">
            <Glyph kind={d.status === "denied" ? "failed" : d.status === "cancelled" ? "idle" : "done"} />
            <span className="step-text">
              <span className="ov-clip">{d.summary}</span>
              <span className="muted small">
                {VERDICT[d.status] ?? d.status}
                {decider(d) && ` by ${decider(d)}`} · {d.risk} risk · {ago(d.decided_at ?? d.created_at, now)}
              </span>
            </span>
          </li>
        ))}
      </ul>
    </Section>
  );
}

/** What was asked for: it is also the first message, so it folds. */
function Request({ f }: { f: Task }) {
  const [open, setOpen] = useState(false);
  if (!f.request && f.requirements.length === 0) return null;
  const long = f.request.length > 240 || f.request.split("\n").length > 3;
  return (
    <Section title="Request">
      {f.request && <p className={open || !long ? "plan-request" : "plan-request clamped"}>{f.request}</p>}
      {long && (
        <button className="link-btn" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
          {open ? "Show less" : "Show all"}
        </button>
      )}
      {f.requirements.length > 0 && (
        <>
          <h3 className="ov-sub">Requirements</h3>
          <ul className="plain-list requirements">
            {f.requirements.map((r) => (
              <li key={r}>{r}</li>
            ))}
          </ul>
        </>
      )}
    </Section>
  );
}

/** A screenshot, fetched from the host when shown. */
function Shot({ load, alt }: { load: () => Promise<string>; alt: string }) {
  const [src, setSrc] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    load()
      .then((s) => live && s && setSrc(s))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);
  return src ? <img className="evidence-shot" src={src} alt={alt} /> : null;
}

type Who = "all" | "agent" | "task";
const WHO: { id: Who; label: string }[] = [
  { id: "all", label: "All" },
  { id: "agent", label: "Coding agent" },
  { id: "task", label: "Task" },
];

/**
 * What happened, newest first (D-114): the task's own history and the
 * coding agent's turns in one feed, then Otter's reasons.
 */
function ActivityFeed({
  placed,
  source,
  conversations,
  now,
}: {
  placed: PlacedTask;
  source: TaskSource;
  conversations: CodingConversation[];
  now: number;
}) {
  const [events, setEvents] = useState<TaskEventRecord[] | null>(null);
  const [who, setWho] = useState<Who>("all");
  const updated = placed.task.updated_at;
  useEffect(() => {
    let live = true;
    source
      .history(placed.key)
      .then((h) => live && setEvents(h))
      .catch(() => live && setEvents([]));
    return () => {
      live = false;
    };
  }, [placed.key, updated, source]);
  const items = [
    ...(who === "agent"
      ? []
      : (events ?? []).map((e) => ({
          at: Date.parse(e.ts),
          key: `e${e.seq}`,
          node: (
            <li className="feed-event">
              <span className="muted small">{ago(e.ts, now)}</span>
              <span>{e.text}</span>
            </li>
          ),
        }))),
    ...(who === "task"
      ? []
      : conversations.flatMap((c) =>
          c.turns.map((t) => ({ at: Date.parse(t.started_at ?? t.queued_at), key: `t${c.id}/${t.id}`, node: <Turn c={c} t={t} now={now} /> })),
        )),
  ].sort((a, b) => b.at - a.at);
  const reasons = collapseRepeats(placed.task.rationale ?? []).reverse();
  return (
    <div className="feed">
      <div className="seg" role="group" aria-label="Show what happened">
        {WHO.map((w) => (
          <button key={w.id} className={who === w.id ? "seg-btn on" : "seg-btn"} aria-pressed={who === w.id} onClick={() => setWho(w.id)}>
            {w.label}
          </button>
        ))}
      </div>
      {who !== "task" && conversations.map((c) => <WorkHead key={c.id} c={c} />)}
      {items.length > 0 ? (
        <ol className="plain-list feed-list">
          {items.map((i) => (
            <Fragment key={i.key}>{i.node}</Fragment>
          ))}
        </ol>
      ) : (
        <p className="muted small">{events === null && who !== "agent" ? "Loading…" : who === "agent" ? "No coding work yet: it starts when a task runs." : "Nothing yet."}</p>
      )}
      {who !== "agent" && reasons.length > 0 && (
        <details className="reasons">
          <summary>Otter's reasons ({reasons.length})</summary>
          <ul className="plain-list">
            {reasons.map((r, i) => (
              <li key={i}>
                {r.text}
                {r.times > 1 && <span className="muted small"> ×{r.times}</span>}
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  );
}

function NewTaskDialog({
  hosts,
  workspaces,
  onCreate,
  onClose,
}: {
  hosts: string[];
  workspaces: Record<string, WorkspaceChoice[]>;
  onCreate: (host: string, title: string, request: string, workspace?: string, answerOnly?: boolean) => Promise<void>;
  onClose: () => void;
}) {
  const [host, setHost] = useState(hosts[0]);
  const choices = workspaces[host] ?? [];
  const [workspace, setWorkspace] = useState<string>("");
  const [answerOnly, setAnswerOnly] = useState(false);
  const [title, setTitle] = useState("");
  const [request, setRequest] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (!title.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      await onCreate(host, title.trim(), request.trim(), workspace || undefined, answerOnly);
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  }

  return (
    <Dialog title="New task" onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <label className="field">
          <span>Title</span>
          <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Export the timeline as CSV" />
        </label>
        <label className="field">
          <span>What do you want?</span>
          <textarea rows={5} value={request} onChange={(e) => setRequest(e.target.value)} placeholder="Describe the outcome, constraints and how you'd check it." />
        </label>
        {hosts.length > 1 && (
          <label className="field">
            <span>Host</span>
            <select
              value={host}
              onChange={(e) => {
                setHost(e.target.value);
                setWorkspace("");
              }}
            >
              {hosts.map((h) => (
                <option key={h}>{h}</option>
              ))}
            </select>
          </label>
        )}
        <label className="field">
          <span>Workspace (where the work happens)</span>
          <select value={workspace} onChange={(e) => setWorkspace(e.target.value)}>
            <option value="">Choose later</option>
            {choices.map((w) => (
              <option key={w.id} value={w.id}>
                {w.name}
              </option>
            ))}
          </select>
        </label>
        <label className="check">
          <input
            type="checkbox"
            aria-label="Only an answer"
            checked={answerOnly}
            onChange={(e) => setAnswerOnly(e.target.checked)}
          />
          Only an answer — the coding agent reads, changes nothing
        </label>
        {error && <p className="error-text small">{error}</p>}
        <div className="form-actions">
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={!title.trim() || busy}>
            Create draft
          </button>
        </div>
      </form>
    </Dialog>
  );
}

