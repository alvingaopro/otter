// Where the Features view gets its data (D-042). The view only talks to a
// `FeatureSource`; `mockSource` stands in until a daemon serves features.

import type { Feature, FeatureAction, FeatureEventRecord, PlacedFeature } from "./features";

export interface FeatureSource {
  /** True for the stand-in: the UI says so. */
  readonly preview: boolean;
  list(): PlacedFeature[];
  /** Called with the new list whenever anything changes. Returns an unsubscribe. */
  subscribe(listener: (list: PlacedFeature[]) => void): () => void;
  create(host: string, title: string, request: string): Promise<PlacedFeature>;
  /** A message from the developer to the feature's Control Agent. */
  send(key: string, text: string): Promise<void>;
  act(key: string, action: FeatureAction): Promise<void>;
  /** The feature's own history, oldest first. */
  history(key: string): Promise<FeatureEventRecord[]>;
}

let counter = 0;
const id = (prefix: string) => `${prefix}_mock${(++counter).toString(36)}`;
const iso = (minutesAgo: number, now: number) => new Date(now - minutesAgo * 60_000).toISOString();

/** Sample features covering each state the view must handle. */
export function sampleFeatures(now = Date.now()): Feature[] {
  const base = (over: Partial<Feature> & Pick<Feature, "title" | "status">): Feature => ({
    id: id("ft"),
    request: over.title,
    requirements: [],
    acceptance: [],
    tasks: [],
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
  active.tasks = [
    { id: "tk_1", title: "Add CSV serializer with tests", status: "done", depends_on: [], workspace_id: "ws_demo", attempts: 1 },
    { id: "tk_2", title: "Wire Export button into Timeline panel", status: "running", depends_on: ["tk_1"], workspace_id: "ws_demo", session_id: "ses_demo", attempts: 1 },
  ];
  active.runs = [{ id: "run_1", task_id: "tk_2", runtime: "claude", state: "running", started_at: iso(6, now) }];
  active.evidence = [{ id: "ev_1", task_id: "tk_1", criterion_id: "ac_2", kind: "test", title: "cargo test timeline::csv (4 passed)", ok: true, at: iso(20, now) }];
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
  blocked.tasks = [{ id: "tk_1", title: "Generate and store the new password", status: "blocked", depends_on: [], attempts: 1 }];
  blocked.decisions = [
    {
      id: "dec_1",
      task_id: "tk_1",
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
  failed.tasks = [{ id: "tk_1", title: "Bump @xterm packages and fix breakages", status: "failed", depends_on: [], attempts: 6, last_error: "WebglAddon: context creation failed" }];
  failed.evidence = [{ id: "ev_1", kind: "test", title: "npm test (1 failed)", ok: false, detail: "Terminal renders: WebGL context lost", at: iso(91, now) }];

  const done = base({
    title: "Show host uptime on the host page",
    status: "done",
    acceptance: [{ id: "ac_1", text: "Host page shows uptime", met: true, evidence: ["ev_1", "ev_2"] }],
    updated_at: iso(600, now),
  });
  done.tasks = [{ id: "tk_1", title: "Read uptime in host.status and show it", status: "done", depends_on: [], attempts: 1 }];
  done.evidence = [
    { id: "ev_1", kind: "browser", title: "Host page shows “up 3 days”", ok: true, at: iso(610, now) },
    { id: "ev_2", kind: "pull_request", title: "PR #31", uri: "https://github.com/example/otter/pull/31", at: iso(605, now) },
  ];

  const draft = base({ title: "Dark-mode charts legend", status: "draft", updated_at: iso(1440, now) });
  return [active, blocked, failed, done, draft];
}

/** An in-memory source with sample data and canned replies. Nothing it does runs anything. */
export function mockSource(host = "preview", now = Date.now()): FeatureSource {
  let list: PlacedFeature[] = sampleFeatures(now).map((feature) => ({ host, feature, key: `${host}/${feature.id}` }));
  const histories = new Map<string, FeatureEventRecord[]>();
  const listeners = new Set<(l: PlacedFeature[]) => void>();
  const stamp = () => new Date().toISOString();

  const log = (key: string, type: string, text: string, correlation_id?: string) => {
    const h = histories.get(key) ?? [];
    h.push({ seq: h.length + 1, ts: stamp(), v: 1, type, text, correlation_id });
    histories.set(key, h);
  };
  const update = (key: string, f: (feature: Feature) => Feature) => {
    list = list.map((p) => (p.key === key ? { ...p, feature: { ...f(p.feature), updated_at: stamp() } } : p));
    listeners.forEach((l) => l(list));
  };

  for (const p of list) {
    log(p.key, "FeatureCreated", "Feature created");
    if (p.feature.status !== "draft") log(p.key, "StatusChanged", `Status: ${p.feature.status}`);
  }

  return {
    preview: true,
    list: () => list,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    async create(h, title, request) {
      const feature: Feature = {
        ...sampleFeatures(Date.now())[4],
        id: id("ft"),
        title,
        request,
        messages: request ? [{ id: id("msg"), role: "user", text: request, at: stamp() }] : [],
        created_at: stamp(),
        updated_at: stamp(),
      };
      const placed = { host: h, feature, key: `${h}/${feature.id}` };
      list = [placed, ...list];
      log(placed.key, "FeatureCreated", "Feature created");
      listeners.forEach((l) => l(list));
      return placed;
    },
    async send(key, text) {
      const msg = id("msg");
      update(key, (f) => ({
        ...f,
        messages: [
          ...f.messages,
          { id: msg, role: "user", text, at: stamp() },
          {
            id: id("msg"),
            role: "system",
            text: "Preview: no Control Agent is connected, so nothing will act on this.",
            at: stamp(),
            correlation_id: msg,
          },
        ],
      }));
      log(key, "MessageAdded", "You sent a message", msg);
    },
    async act(key, action) {
      const next: Partial<Record<FeatureAction["action"], Feature["status"]>> = {
        start: "planning",
        pause: "paused",
        resume: "implementing",
        cancel: "cancelled",
        retry: "implementing",
      };
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
  };
}
