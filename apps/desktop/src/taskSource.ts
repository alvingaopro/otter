// Where the Tasks view gets its data (D-042, D-043). The view only talks
// to a `TaskSource`: `daemonSource` reads each connected host's `otterd`;
// `mockSource` is sample data for tests and previews.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { CodingConversation, RuntimeCapabilities } from "./conversation";
import { daemonMemory, mockMemory, type MemorySource } from "./memory";
import { WHOLE_FILE } from "./changes";
import { invokeNewer } from "./newer";
import type { FileDiff, WorkspaceChanges } from "./types";
import { PICTURES_ONLY, type PastedImage } from "./pastedImage";
import { commandPrefix, type Draft, type MessageCommand, type Task, type TaskAction, type TaskEventRecord, type MessageRole, type PlacedTask, type Run } from "./tasks";

/** Where a new task works besides a workspace that exists (D-179): one made for it. */
export type WorkspaceRequest = { new: { repository: string; base?: string } } | { scratch: Record<string, never> };

export interface TaskSource {
  /** True for the stand-in: the UI says so. */
  readonly preview: boolean;
  /** The hosts to read from (connected ones); sources that don't care ignore it. */
  setHosts?(hosts: string[]): void;
  list(): PlacedTask[];
  /** Called with the new list whenever anything changes. Returns an unsubscribe. */
  subscribe(listener: (list: PlacedTask[]) => void): () => void;
  create(host: string, title: string, request: string, workspace?: string | WorkspaceRequest, answerOnly?: boolean): Promise<PlacedTask>;
  /** A message from the developer to Otter, about the task, with pictures if any.
   * With a slash `command` (D-190), `text` is the words after it, and otterd does it. */
  send(key: string, text: string, images?: PastedImage[], command?: MessageCommand): Promise<void>;
  act(key: string, action: TaskAction): Promise<void>;
  /** The task's own history, oldest first. */
  history(key: string): Promise<TaskEventRecord[]>;
  /** A screenshot the task's checks produced (`artifact:<name>`), as a data URL. */
  artifact(key: string, name: string): Promise<string>;
  /** Messages being written right now, oldest first (D-051). */
  drafts?(key: string): Draft[];
  /** Delete a task that isn't running; its workspace stays. */
  remove(key: string): Promise<void>;
  /** The coding agent's conversations for a task, newest first (D-055). */
  conversations?(key: string): Promise<CodingConversation[]>;
  /** Called when one of the task's conversations changes. Returns an unsubscribe. */
  watchConversations?(key: string, onChange: () => void): () => void;
  /** What the host's coding runtime can do (`null`: an otterd that doesn't say). */
  capabilities?(host: string): Promise<RuntimeCapabilities | null>;
  /** Otter's memories (D-113). */
  memory?: MemorySource;
  /** What changed in a workspace of the task's host (D-147); `undefined`: its otterd can't say. */
  changes?(host: string, workspace: string): Promise<WorkspaceChanges | undefined>;
  /** One changed file's diff there. */
  diff?(host: string, workspace: string, path: string): Promise<FileDiff>;
}

/** The coding agent's work on the sample tasks. */
export function sampleConversations(tasks: Task[], now = Date.now()): Record<string, CodingConversation[]> {
  const out: Record<string, CodingConversation[]> = {};
  const active = tasks.find((f) => f.status === "implementing");
  if (active) {
    out[active.id] = [
      {
        id: "conv_mock1",
        provider: "claude",
        backend: "sdk",
        lifecycle: "open",
        generation: 1,
        active_run: "run_1",
        turns: [
          {
            id: "turn_1",
            initiator: "controller",
            input: [{ type: "text", text: "Wire the Export button into the Timeline panel." }],
            delivery: "delivered",
            state: "running",
            run_id: "run_1",
            generation: 1,
            queued_at: iso(6, now),
            started_at: iso(6, now),
          },
          {
            id: "turn_2",
            command_id: "m4",
            initiator: "user",
            input: [{ type: "text", text: "From the developer:\nUse the existing download icon." }],
            delivery: "queued",
            state: "queued",
            queued_at: iso(1, now),
          },
        ],
        messages: [
          {
            id: "msg_a",
            turn_id: "turn_1",
            role: "agent",
            blocks: [{ id: "blk_a", type: "text", text: "Adding the button next to the filter." }],
            lifecycle: "completed",
            at: iso(5, now),
          },
        ],
        tools: [
          {
            id: "tool_1",
            turn_id: "turn_1",
            run_id: "run_1",
            name: "Bash",
            effect: "command",
            summary: "cargo test timeline::csv",
            status: "succeeded",
            started_at: iso(5, now),
            finished_at: iso(5, now),
            result: { summary: "test result: ok. 4 passed" },
          },
          {
            id: "tool_2",
            turn_id: "turn_1",
            run_id: "run_1",
            name: "Edit",
            effect: "edit",
            summary: "src/TimelinePanel.tsx",
            status: "running",
            started_at: iso(1, now),
          },
        ],
        interactions: [],
        resumable: true,
        updated_at: iso(1, now),
      },
    ];
  }
  return out;
}

let counter = 0;
const id = (prefix: string) => `${prefix}_mock${(++counter).toString(36)}`;
const iso = (minutesAgo: number, now: number) => new Date(now - minutesAgo * 60_000).toISOString();

/** Sample tasks covering each state the view must handle. */
export function sampleTasks(now = Date.now()): Task[] {
  const base = (over: Partial<Task> & Pick<Task, "title" | "status">): Task => ({
    id: id("ft"),
    request: over.title,
    requirements: [],
    acceptance: [],
    steps: [],
    runs: [],
    messages: [],
    decisions: [],
    evidence: [],
    budget: { max_iterations: 6, max_minutes: 120, iterations_used: 0 },
    created_at: iso(240, now),
    updated_at: iso(5, now),
    ...over,
  });
  const active = base({
    title: "Export workspace timeline as CSV",
    status: "implementing",
    request: "Let me export a workspace's timeline as CSV from the Timeline panel.",
    requirements: ["An Export button in the Timeline panel", "CSV has time, kind, session, summary columns"],
    acceptance: [
      { id: "ac_1", text: "Clicking Export saves a CSV of the visible events", evidence: [] },
      { id: "ac_2", text: "Unit tests cover escaping of commas and quotes", met: true, evidence: ["ev_1"] },
    ],
    budget: { max_iterations: 6, max_minutes: 120, iterations_used: 2 },
    workspace_id: "ws_demo",
    updated_at: iso(1, now),
  });
  active.steps = [
    { id: "tk_1", title: "Add CSV serializer with tests", status: "done", depends_on: [], workspace_id: "ws_demo", attempts: 1 },
    { id: "tk_2", title: "Wire Export button into Timeline panel", status: "running", depends_on: ["tk_1"], workspace_id: "ws_demo", session_id: "ses_demo", attempts: 1 },
  ];
  active.runs = [{ id: "run_1", step_id: "tk_2", runtime: "claude", state: "running", started_at: iso(6, now) }];
  active.evidence = [{ id: "ev_1", step_id: "tk_1", criterion_id: "ac_2", kind: "test", title: "cargo test timeline::csv (4 passed)", ok: true, at: iso(20, now) }];
  active.messages = [
    { id: "m1", role: "user", text: active.request, at: iso(60, now) },
    { id: "m2", role: "controller", text: "Plan: a serializer first (with tests), then the button. I'll verify the download in the app.", at: iso(59, now) },
    { id: "m3", role: "agent", text: "Serializer done; 4 tests pass. Starting on the button.", at: iso(20, now) },
  ];

  const blocked = base({
    title: "Rotate the staging database password",
    status: "blocked",
    status_reason: "Waiting for your approval: touches production credentials",
    request: "Rotate the staging DB password and update the app config.",
    acceptance: [{ id: "ac_1", text: "The app connects with the new password", evidence: [] }],
    updated_at: iso(3, now),
  });
  blocked.steps = [{ id: "tk_1", title: "Generate and store the new password", status: "blocked", depends_on: [], attempts: 1 }];
  blocked.decisions = [
    {
      id: "dec_1",
      step_id: "tk_1",
      kind: "tool_permission",
      risk: "high",
      summary: "Run `vault kv put secret/staging/db password=…`",
      detail: "Writes a credential. Policy: credentials always ask you.",
      options: [],
      status: "pending",
      created_at: iso(3, now),
    },
  ];
  blocked.messages = [
    { id: "m1", role: "user", text: blocked.request, at: iso(30, now) },
    { id: "m2", role: "controller", text: "This writes a credential, so I need your approval before the agent runs it.", at: iso(3, now) },
  ];

  const failed = base({
    title: "Upgrade xterm to v7",
    status: "failed",
    status_reason: "Stopped after 6 of 6 attempts: the WebGL addon fails to load",
    budget: { max_iterations: 6, max_minutes: 120, iterations_used: 6 },
    updated_at: iso(90, now),
  });
  failed.steps = [{ id: "tk_1", title: "Bump @xterm packages and fix breakages", status: "failed", depends_on: [], attempts: 6, last_error: "WebglAddon: context creation failed" }];
  failed.evidence = [{ id: "ev_1", kind: "test", title: "npm test (1 failed)", ok: false, detail: "Terminal renders: WebGL context lost", at: iso(91, now) }];

  const done = base({
    title: "Show host uptime on the host page",
    status: "done",
    acceptance: [{ id: "ac_1", text: "Host page shows uptime", met: true, evidence: ["ev_1", "ev_2"] }],
    updated_at: iso(600, now),
  });
  done.steps = [{ id: "tk_1", title: "Read uptime in host.status and show it", status: "done", depends_on: [], attempts: 1 }];
  done.evidence = [
    { id: "ev_1", kind: "browser", title: "Host page shows “up 3 days”", ok: true, at: iso(610, now) },
    { id: "ev_2", kind: "pull_request", title: "PR #31", uri: "https://github.com/example/otter/pull/31", at: iso(605, now) },
  ];

  // What Otter keeps an eye on (D-183): one look later, one CI it reported.
  active.watches = [
    {
      id: "wch_mock1",
      kind: "time",
      target: { in_minutes: 30 },
      label: "See how the export is going",
      what: "see how the export is going",
      asked_in: "m1",
      created_at: iso(10, now),
      // `iso` counts back: 20 minutes from now.
      deadline: iso(-20, now),
      status: "waiting",
      findings: 0,
    },
  ];
  done.watches = [
    {
      id: "wch_mock2",
      kind: "ci",
      target: { pr: 31, repo: "example/otter" },
      label: "CI on #31",
      what: "tell me when CI is done",
      asked_in: "m9",
      created_at: iso(640, now),
      deadline: iso(520, now),
      status: "reported",
      readings: [
        { at: iso(639, now), key: "a:none", text: "no checks yet" },
        { at: iso(630, now), key: "a:1/0/2", text: "2 of 3 checks running, 1 passed (at abc1234)" },
        { at: iso(612, now), key: "passed", text: "all 3 checks passed (at abc1234)" },
      ],
      finding: { at: iso(612, now), outcome: "passed", text: "all 3 checks passed (at abc1234)" },
      findings: 1,
      ended_at: iso(611, now),
      report: "m_watch",
    },
  ];
  done.messages = [
    { id: "m_watch", role: "controller", text: "CI on #31 passed: **all 3 checks**. It's yours to merge.", at: iso(611, now), correlation_id: "watch:wch_mock2:1" },
  ];

  const draft = base({ title: "Dark-mode charts legend", status: "draft", updated_at: iso(1440, now) });
  return [active, blocked, failed, done, draft];
}

/** An in-memory source with sample data and canned replies. Nothing it does runs anything. */
export function mockSource(host = "preview", now = Date.now()): TaskSource {
  let list: PlacedTask[] = sampleTasks(now).map((task) => ({ host, task, key: `${host}/${task.id}` }));
  const conversations = sampleConversations(list.map((p) => p.task), now);
  const histories = new Map<string, TaskEventRecord[]>();
  const listeners = new Set<(l: PlacedTask[]) => void>();
  const stamp = () => new Date().toISOString();

  const log = (key: string, type: string, text: string, correlation_id?: string) => {
    const h = histories.get(key) ?? [];
    h.push({ seq: h.length + 1, ts: stamp(), v: 1, type, text, correlation_id });
    histories.set(key, h);
  };
  const update = (key: string, f: (task: Task) => Task) => {
    list = list.map((p) => (p.key === key ? { ...p, task: { ...f(p.task), updated_at: stamp() } } : p));
    listeners.forEach((l) => l(list));
  };

  for (const p of list) {
    log(p.key, "TaskCreated", "Task created");
    if (p.task.status !== "draft") log(p.key, "StatusChanged", `Status: ${p.task.status}`);
  }

  return {
    preview: true,
    memory: mockMemory(),
    list: () => list,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    async create(h, title, request, workspace, answerOnly) {
      const task: Task = {
        ...sampleTasks(Date.now())[4],
        id: id("ft"),
        title,
        request,
        ...(answerOnly ? { answer_only: true } : {}),
        messages: request ? [{ id: id("msg"), role: "user", text: request, at: stamp() }] : [],
        workspace_id: typeof workspace === "string" ? workspace : undefined,
        created_at: stamp(),
        updated_at: stamp(),
      };
      const placed = { host: h, task, key: `${h}/${task.id}` };
      list = [placed, ...list];
      log(placed.key, "TaskCreated", "Task created");
      listeners.forEach((l) => l(list));
      return placed;
    },
    async send(key, text, images = [], command) {
      const msg = id("msg");
      update(key, (f) => ({
        ...f,
        messages: [
          ...f.messages,
          {
            id: msg,
            role: "user",
            // As otterd stores a message of pictures alone, and a command: the line as written.
            text: command ? `${commandPrefix(command)}${text.trim() ? ` ${text.trim()}` : ""}` : text || (images.length ? PICTURES_ONLY : ""),
            at: stamp(),
            ...(images.length ? { attachments: images.map((_, i) => `attachment-${msg}-${i}`) } : {}),
            ...(command ? { command } : {}),
          },
          {
            id: id("msg"),
            role: "system",
            text: "Preview: nothing is connected, so nothing will act on this.",
            at: stamp(),
            correlation_id: msg,
          },
        ],
      }));
      log(key, "MessageAdded", "You sent a message", msg);
    },
    async remove(key) {
      list = list.filter((p) => p.key !== key);
      listeners.forEach((l) => l(list));
    },
    async conversations(key) {
      const f = list.find((p) => p.key === key);
      return f ? (conversations[f.task.id] ?? []) : [];
    },
    async capabilities() {
      return {
        provider: "claude",
        backend: "sdk",
        available: true,
        structured_ready: false,
        features: {
          send_turn: true,
          resume: true,
          interrupt_turn: true,
          permission_requests: true,
          questions: true,
          tool_results: true,
          streaming: true,
          attachments: false,
          usage: true,
          pause: true,
          redirect: true,
        },
        message_commands: true,
      };
    },
    async act(key, action) {
      const next: Partial<Record<TaskAction["action"], Task["status"]>> = {
        start: "planning",
        pause: "paused",
        resume: "implementing",
        cancel: "cancelled",
        retry: "implementing",
        accept: "done",
        request_changes: "implementing",
        set_workspace: "planning",
      };
      if (action.action === "preview" || action.action === "own_workspace") return;
      if (action.action === "cancel_watch" || action.action === "delete_watch") {
        const id = action.watch_id;
        const gone = action.action === "delete_watch";
        update(key, (f) => ({
          ...f,
          watches: (f.watches ?? [])
            .filter((w) => !(gone && w.id === id))
            .map((w) => (w.id === id && (w.status === "waiting" || w.status === "firing") ? { ...w, status: "cancelled" as const, ended_at: stamp() } : w)),
        }));
        log(key, gone ? "WatchDeleted" : "WatchCancelled", gone ? "Deleted a watch" : "Stopped a watch", id);
        return;
      }
      if (action.action === "decide") {
        update(key, (f) => ({
          ...f,
          status: f.status === "blocked" ? "implementing" : f.status,
          status_reason: undefined,
          decisions: f.decisions.map((d) =>
            d.id === action.decision_id
              ? { ...d, status: action.answer ? "answered" : action.approve ? "approved" : "denied", decided_by: "user", answer: action.answer, decided_at: stamp() }
              : d,
          ),
        }));
        log(key, "DecisionResolved", action.approve ? "You approved a request" : "You denied a request", action.decision_id);
        return;
      }
      const status = next[action.action]!;
      update(key, (f) => ({ ...f, status, status_reason: undefined }));
      log(key, "StatusChanged", `Status: ${status}`);
    },
    async history(key) {
      return histories.get(key) ?? [];
    },
    async artifact() {
      return "";
    },
    async changes() {
      return SAMPLE_CHANGES;
    },
    async diff(_host, _workspace, path) {
      return { text: `--- a/${path}\n+++ b/${path}\n@@ -1 +1 @@\n-old\n+new\n`, truncated: false };
    },
  };
}

/** What the sample tasks changed. */
const SAMPLE_CHANGES: WorkspaceChanges = {
  git: true,
  branch: "otter/timeline-csv",
  base: "main",
  ahead: 2,
  behind: 0,
  files: [
    { path: "apps/desktop/src/TimelinePanel.tsx", change: "modified", insertions: 24, deletions: 3, uncommitted: false },
    { path: "apps/desktop/src/timelineCsv.ts", change: "added", insertions: 41, deletions: 0, uncommitted: true },
  ],
  more: 0,
  insertions: 65,
  deletions: 3,
};

/** A unique id per intent: the daemon applies a command once, so a retry is harmless. */
export function commandId(): string {
  try {
    return `cmd_${crypto.randomUUID()}`;
  } catch {
    return `cmd_${Date.now().toString(36)}${Math.random().toString(36).slice(2)}`;
  }
}

const splitKey = (key: string) => {
  const i = key.indexOf("/");
  return { host: key.slice(0, i), task: key.slice(i + 1) };
};

/** How often a host with a live run is read again (D-141). */
const HEARD_REFRESH_MS = 30_000;

const isLive = (r: Run) => r.state === "starting" || r.state === "running" || r.state === "waiting";

/**
 * Tasks from each connected host's daemon. Reloads a host when it says a
 * task changed (`TaskChanged`) or its event stream restarted; a host
 * whose otterd predates tasks simply has none.
 */
export function daemonSource(): TaskSource {
  const byHost = new Map<string, Task[]>();
  let hosts: string[] = [];
  let list: PlacedTask[] = [];
  const listeners = new Set<(l: PlacedTask[]) => void>();

  const rebuild = () => {
    list = hosts.flatMap((host) => (byHost.get(host) ?? []).map((task) => ({ host, task, key: `${host}/${task.id}` })));
    listeners.forEach((l) => l(list));
  };
  // Text streaming in, by task key, then stream id (D-051).
  const drafts = new Map<string, Map<string, Draft>>();
  const load = async (host: string) => {
    try {
      byHost.set(host, await invoke<Task[]>("tasks_list", { host }));
    } catch {
      byHost.delete(host);
    }
    // Written drafts give way to the messages just loaded.
    for (const [key, ds] of drafts) {
      if (!key.startsWith(`${host}/`)) continue;
      for (const [id, d] of ds) if (d.done) ds.delete(id);
      if (ds.size === 0) drafts.delete(key);
    }
    if (hosts.includes(host)) rebuild();
  };
  type StreamRecord = { type: string; task_id: string; stream_id: string; role: MessageRole; text: string; done?: boolean };
  /** A task's live run was just heard from: in place, the stream's redraw shows it. */
  const heard = (host: string, task: string) => {
    const f = byHost.get(host)?.find((x) => x.id === task);
    const run = f?.runs?.find((x) => isLive(x));
    if (run?.heard_at !== undefined) run.heard_at = new Date().toISOString();
  };
  // A live run's "last heard" is the daemon's: read it again now and then,
  // since not every sign of work reaches the app as an event (D-141).
  setInterval(() => {
    for (const host of hosts) if ((byHost.get(host) ?? []).some((f) => f.runs?.some(isLive))) void load(host);
  }, HEARD_REFRESH_MS);
  /** Put a task the daemon just returned in place, ahead of the event. */
  const put = (host: string, task: Task) => {
    const rest = (byHost.get(host) ?? []).filter((f) => f.id !== task.id);
    byHost.set(host, [task, ...rest]);
    rebuild();
  };

  // Who wants to hear that a task's conversations changed.
  const conversationWatchers = new Map<string, Set<() => void>>();
  void listen<{ host: string; record: { type: string } }>("host-event", (e) => {
    const { host, record } = e.payload;
    if (!hosts.includes(host)) return;
    if (record.type === "ConversationChanged") {
      const task = (record as { task_id?: string }).task_id;
      if (task) conversationWatchers.get(`${host}/${task}`)?.forEach((w) => w());
    }
    if (record.type === "TaskChanged") void load(host);
    if (record.type === "TaskDeleted") {
      drafts.delete(`${host}/${(record as { task_id?: string }).task_id}`);
      void load(host);
    }
    // What the coding agent just did (D-069): transient, onto its live run,
    // with its time when the daemon says it (D-141).
    if (record.type === "TaskActivity") {
      const r = record as unknown as { task_id: string; run_id: string; line: string; at?: string };
      const tasks = byHost.get(host) ?? [];
      const f = tasks.find((x) => x.id === r.task_id);
      const run = f?.runs.find((x) => x.id === r.run_id);
      if (f && run) {
        const activity = [...(run.activity ?? []), r.line].slice(-30);
        const timed = r.at !== undefined && (run.activity_at ?? []).length === (run.activity ?? []).length;
        const at = timed ? { activity_at: [...(run.activity_at ?? []), r.at!].slice(-30), heard_at: r.at } : { activity_at: undefined };
        const next = { ...f, runs: f.runs.map((x) => (x.id === run.id ? { ...x, activity, ...at } : x)) };
        byHost.set(
          host,
          tasks.map((x) => (x.id === f.id ? next : x)),
        );
        rebuild();
      }
    }
    if (record.type === "TaskStream") {
      const r = record as StreamRecord;
      const key = `${host}/${r.task_id}`;
      // The coding agent writing is it being heard from (D-141).
      if (r.role === "agent") heard(host, r.task_id);
      const ds = drafts.get(key) ?? new Map<string, Draft>();
      ds.set(r.stream_id, { stream_id: r.stream_id, role: r.role, text: r.text, done: !!r.done });
      drafts.set(key, ds);
      // A new list (same tasks) so views redraw.
      list = [...list];
      listeners.forEach((l) => l(list));
    }
  });
  void listen<{ host: string }>("host-resync", (e) => {
    if (hosts.includes(e.payload.host)) void load(e.payload.host);
  });

  return {
    preview: false,
    memory: daemonMemory(),
    setHosts(next) {
      const added = next.filter((h) => !hosts.includes(h));
      const changed = added.length > 0 || next.length !== hosts.length;
      hosts = next;
      added.forEach((h) => void load(h));
      if (changed) rebuild();
    },
    list: () => list,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    async create(host, title, request, workspace, answerOnly) {
      const task = await invoke<Task>("task_create", {
        host,
        commandId: commandId(),
        title,
        request,
        workspace: workspace ?? null,
        answerOnly: answerOnly ?? false,
      });
      put(host, task);
      return { host, task, key: `${host}/${task.id}` };
    },
    async send(key, text, images, command) {
      const { host, task } = splitKey(key);
      put(host, await invoke<Task>("task_send", { host, commandId: commandId(), task, text, images: images ?? null, command: command ?? null }));
    },
    async act(key, action) {
      const { host, task } = splitKey(key);
      put(host, await invoke<Task>("task_act", { host, commandId: commandId(), task, action }));
    },
    async remove(key) {
      const { host, task } = splitKey(key);
      await invoke("task_delete", { host, task });
      byHost.set(host, (byHost.get(host) ?? []).filter((f) => f.id !== task));
      drafts.delete(key);
      rebuild();
    },
    async history(key) {
      const { host, task } = splitKey(key);
      return invoke<TaskEventRecord[]>("task_events", { host, task, after: null });
    },
    async artifact(key, name) {
      const { host, task } = splitKey(key);
      const data = await invoke<string>("task_artifact", { host, task, name });
      const type = /\.jpe?g$/.test(name) ? "jpeg" : name.endsWith(".webp") ? "webp" : "png";
      return `data:image/${type};base64,${data}`;
    },
    changes(host, workspace) {
      return invokeNewer<WorkspaceChanges>(host, "workspace_changes", { host, workspace });
    },
    diff(host, workspace, path) {
      // The whole file, folded where it's shown (D-111).
      return invoke<FileDiff>("workspace_diff", { host, workspace, path, context: WHOLE_FILE });
    },
    drafts(key) {
      return [...(drafts.get(key)?.values() ?? [])];
    },
    async conversations(key) {
      const { host, task } = splitKey(key);
      try {
        return await invoke<CodingConversation[]>("conversations", { host, task });
      } catch {
        // An otterd from before conversations.
        return [];
      }
    },
    watchConversations(key, onChange) {
      const set = conversationWatchers.get(key) ?? new Set();
      set.add(onChange);
      conversationWatchers.set(key, set);
      return () => {
        set.delete(onChange);
      };
    },
    async capabilities(host) {
      try {
        return await invoke<RuntimeCapabilities>("runtime_capabilities", { host });
      } catch {
        return null;
      }
    },
  };
}
