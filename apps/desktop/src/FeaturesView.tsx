import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { Dialog } from "./Dialog";
import { Glyph } from "./Glyph";
import { SettingsButton } from "./SettingsDialog";
import { ago } from "./model";
import type { FeatureSource } from "./featureSource";
import {
  FILTERS,
  STATUS_LABEL,
  featureGlyph,
  matches,
  pendingDecisions,
  sortFeatures,
  statusDetail,
  type DecisionRequest,
  type Feature,
  type FeatureAction,
  type FeatureEventRecord,
  type FeatureFilter,
  type PlacedFeature,
  type WorkspaceChoice,
} from "./features";

interface Props {
  source: FeatureSource;
  /** Hosts a feature can be created on. */
  hosts: string[];
  /** Workspaces per host, to work in. */
  workspaces: Record<string, WorkspaceChoice[]>;
  now: number;
  /** Show a workspace (and session) in the Workspaces view. */
  onOpenWorkspace: (host: string, workspaceId: string, sessionId?: string) => void;
  /** Open a preview port of a host in the browser (forwarding it if remote). */
  onOpenPreview: (host: string, port: number, path: string) => Promise<void>;
  /** Open Settings (Lead, API keys). */
  onSettings?: () => void;
}

/** The Features view (D-041, D-042): product work, independent of the Workspace view. */
export function FeaturesView({ source, hosts, workspaces, now, onOpenWorkspace, onOpenPreview, onSettings }: Props) {
  const [list, setList] = useState<PlacedFeature[]>(() => source.list());
  const [filter, setFilter] = useState<FeatureFilter>("all");
  const [selected, setSelected] = useState<string | undefined>();
  const [creating, setCreating] = useState(false);

  useEffect(() => {
    setList(source.list());
    return source.subscribe(setList);
  }, [source]);

  const sorted = useMemo(() => sortFeatures(list), [list]);
  const shown = sorted.filter((p) => matches(p.feature, filter));
  const current = list.find((p) => p.key === selected) ?? shown[0];

  return (
    <>
      <nav className="sidebar" aria-label="Features">
        <div className="sidebar-top" data-tauri-drag-region>
          <span className="brand-name">Features</span>
          <span className="spacer" />
          {onSettings && <SettingsButton onClick={onSettings} />}
          <button className="icon-btn" aria-label="New feature" title="New feature" onClick={() => setCreating(true)}>
            <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true">
              <path d="M7 2v10M2 7h10" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
            </svg>
          </button>
        </div>
        <div className="feature-filters" role="group" aria-label="Filter features">
          {FILTERS.map((f) => {
            const count = sorted.filter((p) => matches(p.feature, f.id)).length;
            return (
              <button
                key={f.id}
                className={filter === f.id ? "filter-chip on" : "filter-chip"}
                aria-pressed={filter === f.id}
                onClick={() => setFilter(f.id)}
              >
                {f.label}
                <span className="filter-count">{count}</span>
              </button>
            );
          })}
        </div>
        <div className="sidebar-groups">
          {shown.length === 0 ? (
            <p className="muted small feature-none">{list.length === 0 ? "No features yet." : "Nothing matches this filter."}</p>
          ) : (
            <div className="group" role="list">
              {shown.map((p) => (
                <button
                  key={p.key}
                  role="listitem"
                  className={p.key === current?.key ? "ws-row selected" : "ws-row"}
                  aria-current={p.key === current?.key}
                  onClick={() => setSelected(p.key)}
                >
                  <span className="ws-glyph">
                    <Glyph kind={featureGlyph(p.feature)} />
                  </span>
                  <span className="ws-text">
                    <span className="ws-line">
                      <span className="ws-name">{p.feature.title}</span>
                      <span className="ws-host">{p.host}</span>
                    </span>
                    <span className="ws-line sub">
                      <span className="ws-summary">{rowSummary(p.feature)}</span>
                      <span className="ws-age">{ago(p.feature.updated_at, now)}</span>
                    </span>
                  </span>
                </button>
              ))}
            </div>
          )}
        </div>
      </nav>
      {current ? (
        <FeaturePane
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
          <h1 className="empty-title">Describe a feature</h1>
          <p className="muted">Say what you want built. The Lead plans it, directs a coding agent in a workspace and verifies the result.</p>
          <div className="empty-actions">
            <button className="btn primary" onClick={() => setCreating(true)}>
              New feature
            </button>
          </div>
        </main>
      )}
      {creating && (
        <NewFeatureDialog
          hosts={hosts.length > 0 ? hosts : ["preview"]}
          workspaces={workspaces}
          onClose={() => setCreating(false)}
          onCreate={async (host, title, request, workspace) => {
            const p = await source.create(host, title, request, workspace);
            setFilter("all");
            setSelected(p.key);
            setCreating(false);
          }}
        />
      )}
    </>
  );
}

function rowSummary(f: Feature): string {
  const pending = pendingDecisions(f).length;
  if (pending > 0) return `${pending} decision${pending > 1 ? "s" : ""} waiting`;
  if (f.status_reason) return f.status_reason;
  const done = f.tasks.filter((t) => t.status === "done").length;
  if (f.tasks.length > 0) return `${STATUS_LABEL[f.status]} · ${done}/${f.tasks.length} tasks`;
  return STATUS_LABEL[f.status];
}

type Tab = "plan" | "tasks" | "approvals" | "evidence" | "delivery" | "timeline";

function FeaturePane({
  placed,
  source,
  now,
  workspaces,
  onOpenWorkspace,
  onOpenPreview,
}: {
  placed: PlacedFeature;
  source: FeatureSource;
  now: number;
  workspaces: WorkspaceChoice[];
  onOpenWorkspace: Props["onOpenWorkspace"];
  onOpenPreview: Props["onOpenPreview"];
}) {
  const f = placed.feature;
  const wsName = workspaces.find((w) => w.id === f.workspace_id)?.name;
  const pending = pendingDecisions(f);
  const [tab, setTab] = useState<Tab>(pending.length > 0 ? "approvals" : "plan");
  const [error, setError] = useState<string | null>(null);

  const act = (action: FeatureAction) => {
    setError(null);
    source.act(placed.key, action).catch((e) => setError(String(e)));
  };

  const tabs: { id: Tab; label: string; count?: number }[] = [
    { id: "plan", label: "Plan" },
    { id: "tasks", label: "Tasks", count: f.tasks.length },
    { id: "approvals", label: "Approvals", count: pending.length },
    { id: "evidence", label: "Evidence", count: f.evidence.length },
    { id: "delivery", label: "Delivery" },
    { id: "timeline", label: "Timeline" },
  ];

  return (
    <main className="pane feature-pane" aria-label={f.title}>
      <header className="pane-head" data-tauri-drag-region>
        <Glyph kind={featureGlyph(f)} />
        <h1 className="feature-title">{f.title}</h1>
        <span className={`status-pill ${featureGlyph(f)}`}>{STATUS_LABEL[f.status]}</span>
        {statusDetail(f) && <span className="status-detail">{statusDetail(f)}</span>}
        <span className="chips">
          <span className="chip">{placed.host}</span>
          {f.workspace_id && (
            <button className="chip link-btn" onClick={() => onOpenWorkspace(placed.host, f.workspace_id!)}>
              {wsName ?? f.workspace_id}
            </button>
          )}
        </span>
        <span className="spacer" />
        {f.workspace_id && !["done", "cancelled"].includes(f.status) && (
          <button
            className="btn"
            title={f.preview ? `Port ${f.preview.port} on ${placed.host}` : "Start the app's preview in its workspace"}
            onClick={async () => {
              setError(null);
              try {
                if (!f.preview) await source.act(placed.key, { action: "preview" });
                const p = source.list().find((x) => x.key === placed.key)?.feature.preview ?? f.preview;
                if (p) await onOpenPreview(placed.host, p.port, p.path);
              } catch (e) {
                setError(String(e));
              }
            }}
          >
            {f.preview ? "Open preview" : "Preview"}
          </button>
        )}
        <FeatureActions f={f} act={act} />
      </header>
      {source.preview && (
        <p className="notice" role="note">
          Preview with sample data: nothing here runs.
        </p>
      )}
      <StatusBanner f={f} onApprovals={() => setTab("approvals")} act={act} />
      {!f.workspace_id && !["done", "cancelled"].includes(f.status) && (
        <div className="banner info" role="group" aria-label="Workspace">
          <span className="banner-text">Choose the workspace this feature's work happens in.</span>
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
      <div className="feature-body">
        <Conversation placed={placed} source={source} now={now} />
        <section className="feature-detail" aria-label="Details">
          <div className="detail-tabs" role="tablist" aria-label="Feature details">
            {tabs.map((t) => (
              <button
                key={t.id}
                role="tab"
                id={`ftab-${t.id}`}
                aria-selected={tab === t.id}
                aria-controls={`fpanel-${t.id}`}
                className={tab === t.id ? "detail-tab on" : "detail-tab"}
                onClick={() => setTab(t.id)}
              >
                {t.label}
                {t.count ? <span className={t.id === "approvals" ? "filter-count needs" : "filter-count"}>{t.count}</span> : null}
              </button>
            ))}
          </div>
          <div className="detail-body" role="tabpanel" id={`fpanel-${tab}`} aria-labelledby={`ftab-${tab}`}>
            {tab === "plan" && <Plan f={f} />}
            {tab === "tasks" && <Tasks placed={placed} onOpenWorkspace={onOpenWorkspace} />}
            {tab === "approvals" && <Approvals f={f} act={act} now={now} />}
            {tab === "evidence" && <EvidenceList f={f} now={now} shot={(name) => source.artifact(placed.key, name)} />}
            {tab === "delivery" && <DeliveryPanel f={f} />}
            {tab === "timeline" && <Timeline placed={placed} source={source} now={now} />}
          </div>
        </section>
      </div>
    </main>
  );
}

function FeatureActions({ f, act }: { f: Feature; act: (a: FeatureAction) => void }) {
  const live = ["planning", "implementing", "verifying", "blocked"].includes(f.status);
  return (
    <span className="feature-actions">
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
      {f.status === "review" && (
        <>
          {gatesClear(f) ? (
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
          )}
        </>
      )}
      {!["done", "cancelled"].includes(f.status) && (
        <button className="btn outline" onClick={() => act({ action: "cancel" })}>
          Cancel
        </button>
      )}
    </span>
  );
}

function StatusBanner({ f, onApprovals, act }: { f: Feature; onApprovals: () => void; act: (a: FeatureAction) => void }) {
  const pending = pendingDecisions(f).length;
  if (pending > 0 || f.status === "blocked") {
    return (
      <div className="banner needs" role="status">
        <span className="banner-text">
          <Glyph kind="needs" />
          <span className="banner-summary">{f.status_reason ?? `${pending} decision${pending === 1 ? "" : "s"} waiting for you`}</span>
        </span>
        {pending > 0 && (
          <span className="banner-actions">
            <button className="btn" onClick={onApprovals}>
              Review
            </button>
          </span>
        )}
      </div>
    );
  }
  if (f.status === "failed") {
    return (
      <div className="banner failure" role="status">
        <span className="banner-text">
          <Glyph kind="failed" />
          <span className="banner-summary">{f.status_reason ?? "The feature failed"}</span>
        </span>
        <span className="banner-actions">
          <button className="btn" onClick={() => act({ action: "retry" })}>
            Retry
          </button>
        </span>
      </div>
    );
  }
  if (f.status === "review") {
    return (
      <div className="banner needs" role="status">
        <span className="banner-text">
          <Glyph kind="needs" />
          <span className="banner-summary">{f.status_reason ?? "Ready for your review"}</span>
        </span>
      </div>
    );
  }
  return null;
}

const ROLE: Record<string, string> = { user: "You", controller: "Lead", agent: "Coding agent", system: "Otter" };

function Conversation({ placed, source, now }: { placed: PlacedFeature; source: FeatureSource; now: number }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const end = useRef<HTMLDivElement>(null);
  const messages = placed.feature.messages;
  const live = placed.feature.runs.find((r) => ["starting", "running", "waiting"].includes(r.state));
  // Being written right now (D-051); a written one shows until its message loads.
  const drafts = (source.drafts?.(placed.key) ?? []).filter(
    (d) => !(d.done && messages.some((m) => m.role === d.role && m.text.trim() === d.text.trim())),
  );
  const draftText = drafts.map((d) => d.text).join("");
  const activity = live?.activity ?? [];
  // The developer spoke last: the Lead's answer is on its way.
  const last = messages[messages.length - 1];
  const replying = drafts.some((d) => d.role === "controller");
  const awaitingReply =
    !replying && last?.role === "user" && messages.length > 1 && !["done", "cancelled"].includes(placed.feature.status);
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
        {messages.map((m) => (
          <div key={m.id} className={`message ${m.role}`}>
            <div className="message-head">
              <span className="message-role">{ROLE[m.role] ?? m.role}</span>
              <span className="message-at">{ago(m.at, now)}</span>
            </div>
            <div className="message-text">{m.text}</div>
          </div>
        ))}
        {drafts.map((d) => (
          <div key={d.stream_id} className={`message ${d.role} streaming`}>
            <div className="message-head">
              <span className="message-role">{ROLE[d.role] ?? d.role}</span>
              <span className="message-at">{d.done ? "now" : "writing…"}</span>
            </div>
            <div className="message-text">
              {d.text}
              {!d.done && <span className="cursor" aria-hidden="true" />}
            </div>
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
        {awaitingReply && <p className="muted small replying">The Lead is replying…</p>}
        <div ref={end} />
      </div>
      <form className="composer" onSubmit={submit}>
        <textarea
          aria-label="Message to the Lead"
          placeholder={
            placed.feature.status === "review" || placed.feature.status === "done"
              ? "Happy with it? Or say what to change…"
              : "Tell the Lead what you want…"
          }
          value={text}
          rows={2}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void submit(e as unknown as FormEvent);
            }
          }}
        />
        <button className="btn primary" type="submit" disabled={busy || !text.trim()}>
          Send
        </button>
      </form>
    </section>
  );
}

function Plan({ f }: { f: Feature }) {
  return (
    <div className="plan">
      <h2 className="sub-title">Request</h2>
      <p className="plan-request">{f.request || <span className="muted">—</span>}</p>
      <h2 className="sub-title">Requirements</h2>
      {f.requirements.length === 0 ? (
        <p className="muted small">None yet: the Lead writes them while planning.</p>
      ) : (
        <ul className="plain-list">
          {f.requirements.map((r) => (
            <li key={r}>{r}</li>
          ))}
        </ul>
      )}
      <h2 className="sub-title">Acceptance criteria</h2>
      {f.acceptance.length === 0 ? (
        <p className="muted small">None yet.</p>
      ) : (
        <ul className="plain-list criteria">
          {f.acceptance.map((c) => (
            <li key={c.id}>
              <Glyph kind={c.met === true ? "done" : c.met === false ? "failed" : "idle"} />
              <span>{c.text}</span>
              <span className="muted small">{c.met === undefined ? "not verified" : c.met ? "met" : "not met"}</span>
            </li>
          ))}
        </ul>
      )}
      {f.verify_command && (
        <>
          <h2 className="sub-title">Check</h2>
          <p className="small">
            <code>{f.verify_command}</code>
          </p>
        </>
      )}
      <h2 className="sub-title">Budget</h2>
      <p className="small">
        {f.budget.iterations_used} of {f.budget.max_iterations} attempts used · up to {f.budget.max_minutes} min
      </p>
      {f.rationale && f.rationale.length > 0 && (
        <>
          <h2 className="sub-title">Why</h2>
          <ul className="plain-list small">
            {[...f.rationale].reverse().map((r, i) => (
              <li key={i}>{r}</li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

const TASK_GLYPH = { pending: "idle", running: "working", blocked: "needs", done: "done", failed: "failed", skipped: "idle" } as const;

function Tasks({ placed, onOpenWorkspace }: { placed: PlacedFeature; onOpenWorkspace: Props["onOpenWorkspace"] }) {
  const f = placed.feature;
  if (f.tasks.length === 0) return <p className="muted small">No tasks yet: the plan comes first.</p>;
  return (
    <ul className="plain-list tasks">
      {f.tasks.map((t) => (
        <li key={t.id} className="task">
          <Glyph kind={TASK_GLYPH[t.status]} />
          <span className="task-text">
            <span className="task-title">{t.title}</span>
            <span className="muted small">
              {t.status}
              {t.attempts > 1 && ` · attempt ${t.attempts}`}
              {t.depends_on.length > 0 && ` · after ${t.depends_on.map((d) => f.tasks.findIndex((x) => x.id === d) + 1).join(", ")}`}
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
  );
}

function Approvals({ f, act, now }: { f: Feature; act: (a: FeatureAction) => void; now: number }) {
  if (f.decisions.length === 0) return <p className="muted small">Nothing has needed a decision.</p>;
  const sorted = [...f.decisions].sort((a, b) => Number(b.status === "pending") - Number(a.status === "pending"));
  return (
    <ul className="plain-list decisions">
      {sorted.map((d) => (
        <Decision key={d.id} d={d} act={act} now={now} />
      ))}
    </ul>
  );
}

function Decision({ d, act, now }: { d: DecisionRequest; act: (a: FeatureAction) => void; now: number }) {
  const [answer, setAnswer] = useState("");
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
      {d.status === "pending" ? (
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

function EvidenceList({ f, now, shot }: { f: Feature; now: number; shot: (name: string) => Promise<string> }) {
  if (f.evidence.length === 0) return <p className="muted small">No evidence yet: tests, browser checks and CI results land here.</p>;
  return (
    <ul className="plain-list evidence">
      {f.evidence.map((e) => (
        <li key={e.id} className="evidence-item">
          <Glyph kind={e.ok === true ? "done" : e.ok === false ? "failed" : "idle"} />
          <span className="task-text">
            <span>
              {e.uri ? (
                <a href={e.uri} target="_blank" rel="noreferrer">
                  {e.title}
                </a>
              ) : (
                e.title
              )}
            </span>
            <span className="muted small">
              {e.kind.replace("_", " ")}
              {e.uncertain && " · judgment, not a check"} · {ago(e.at, now)}
            </span>
            {e.detail && <span className="muted small evidence-detail">{e.detail}</span>}
            {e.kind === "screenshot" && e.uri?.startsWith("artifact:") && <Shot load={() => shot(e.uri!)} alt={e.title} />}
          </span>
        </li>
      ))}
    </ul>
  );
}

/** Every gate but the developer's own acceptance passed (or doesn't apply). */
function gatesClear(f: Feature): boolean {
  const gates = f.gates ?? [];
  return gates.length > 0 && gates.filter((g) => g.name !== "Your acceptance").every((g) => g.status === "passed" || g.status === "not_applicable");
}

const GATE_GLYPH = { passed: "done", failed: "failed", pending: "working", not_applicable: "idle" } as const;

/** The gates, the pull request and its CI, and the final report (D-047). */
function DeliveryPanel({ f }: { f: Feature }) {
  const gates = f.gates ?? [];
  const d = f.delivery;
  return (
    <div className="plan">
      <h2 className="sub-title">Gates</h2>
      {gates.length === 0 ? (
        <p className="muted small">Checked once the work is verified.</p>
      ) : (
        <ul className="plain-list criteria">
          {gates.map((g) => (
            <li key={g.name}>
              <Glyph kind={GATE_GLYPH[g.status]} />
              <span>{g.name}</span>
              <span className="muted small">{g.status === "not_applicable" ? "n/a" : g.status}{g.detail ? ` · ${g.detail}` : ""}</span>
            </li>
          ))}
        </ul>
      )}
      {d && (
        <>
          <h2 className="sub-title">Pull request</h2>
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
            <ul className="plain-list small">
              {d.ci.map((c) => (
                <li key={c.name}>
                  <Glyph kind={c.state === "pass" || c.state === "skipping" ? "done" : c.state === "pending" ? "working" : "failed"} /> {c.name}{" "}
                  <span className="muted">{c.state}</span>
                </li>
              ))}
            </ul>
          )}
          {d.reruns > 0 && <p className="small muted">Reran flaky CI {d.reruns}×.</p>}
        </>
      )}
      <h2 className="sub-title">Report</h2>
      {f.report ? <pre className="report">{f.report}</pre> : <p className="muted small">Written when every gate has passed.</p>}
    </div>
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

function Timeline({ placed, source, now }: { placed: PlacedFeature; source: FeatureSource; now: number }) {
  const [events, setEvents] = useState<FeatureEventRecord[] | null>(null);
  const updated = placed.feature.updated_at;
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
  if (!events) return <p className="muted small">Loading…</p>;
  if (events.length === 0) return <p className="muted small">No history yet.</p>;
  return (
    <ol className="plain-list feature-timeline">
      {[...events].reverse().map((e) => (
        <li key={e.seq}>
          <span className="muted small">{ago(e.ts, now)}</span>
          <span>{e.text}</span>
        </li>
      ))}
    </ol>
  );
}

function NewFeatureDialog({
  hosts,
  workspaces,
  onCreate,
  onClose,
}: {
  hosts: string[];
  workspaces: Record<string, WorkspaceChoice[]>;
  onCreate: (host: string, title: string, request: string, workspace?: string) => Promise<void>;
  onClose: () => void;
}) {
  const [host, setHost] = useState(hosts[0]);
  const choices = workspaces[host] ?? [];
  const [workspace, setWorkspace] = useState<string>("");
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
      await onCreate(host, title.trim(), request.trim(), workspace || undefined);
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  }

  return (
    <Dialog title="New feature" onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <label className="field">
          <span>Title</span>
          <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Export the timeline as CSV" />
        </label>
        <label className="field">
          <span>What should it do?</span>
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

