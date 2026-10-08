// Mirrors src-tauri/src/view.rs (the only daemon-derived shapes the UI sees).

export type Activity = "needs_you" | "failed" | "working" | "completed" | "idle" | "preparing";
export type HostStatus = "connecting" | "connected" | "unreachable" | "incompatible" | "not_installed";
export type SessionKind = "agent" | "terminal" | "service" | "task";
export type SessionStatus = "pending" | "running" | "completed" | "failed" | "stopped" | "lost";
export type AgentState = "starting" | "idle" | "working" | "blocked" | "waiting_for_input" | "exited";
export type AttentionKind = "question" | "approval" | "failure" | "review" | "completion";

export interface AttentionView {
  id: string;
  sessionId?: string;
  kind: AttentionKind;
  needsYou: boolean;
  summary: string;
  createdAt: string;
}

export interface AgentView {
  provider: string;
  state: AgentState;
  since: string;
  lastMessage?: string;
}

export interface SessionView {
  id: string;
  name: string;
  kind: SessionKind;
  status: SessionStatus;
  agent?: AgentView;
  exitCode?: number;
  launchError?: string;
  attention?: AttentionView;
}

export interface WorkspaceView {
  id: string;
  name: string;
  activity: Activity;
  state: "preparing" | "ready" | "failed" | "archived";
  stateMessage?: string;
  root: string;
  sourceKind: "git" | "directory" | "empty";
  source: string;
  sessions: SessionView[];
  attention: AttentionView[];
  updatedAt: string;
}

export interface HostView {
  name: string;
  /** How it is reached: `ssh dev-01`, `this Mac`. */
  describe: string;
  status: HostStatus;
  message?: string;
  /** The host's otterd version, once connected. */
  version?: string;
  /** Agent providers and whether this host can run them. */
  agents: { provider: string; available: boolean; version?: string; can_resume: boolean }[];
  workspaces: WorkspaceView[];
}

export interface HostsPayload {
  hosts: HostView[];
  configError?: string;
  configDir: string;
}

export type AttachEvent =
  | { type: "exit"; reason: "detached" | "exited" | "ended" | "error"; exitCode?: number; message?: string }
  | { type: "closed" };

/** A workspace together with the host it lives on. */
export interface Placed {
  host: HostView;
  ws: WorkspaceView;
  key: string;
}

/** The status glyph shown for a workspace or a session. */
export type Glyph = "needs" | "working" | "done" | "failed" | "idle";

// Host page (src-tauri: otter_protocol::host, forwards.rs).

export interface MetricsSample {
  at: string;
  cpu_percent: number;
  memory_used: number;
  net_rx_bps: number;
  net_tx_bps: number;
  disk_read_bps: number;
  disk_write_bps: number;
}

export interface HostMetrics {
  sampled_at: string;
  cpus: number;
  load: [number, number, number];
  uptime_secs: number;
  memory: { total: number; used: number; available: number; swap_total: number; swap_used: number };
  disks: { mount: string; file_system: string; total: number; available: number }[];
  top: { pid: number; name: string; cpu_percent: number; memory: number }[];
  history: MetricsSample[];
  interval_secs: number;
}

export interface ListeningPort {
  port: number;
  address: string;
  process?: string;
  pid?: number;
}

export type Direction = "to_local" | "to_host";

export interface ForwardView {
  host: string;
  direction: Direction;
  listenPort: number;
  targetHost: string;
  targetPort: number;
  pinned: boolean;
  state: "pending" | "active" | "failed";
  message?: string;
}

export interface TrendPoint {
  at: string;
  cpu_avg: number;
  cpu_max: number;
  memory_avg: number;
  memory_max: number;
  load_avg: number;
  net_rx_bps: number;
  net_tx_bps: number;
  disk_read_bps: number;
  disk_write_bps: number;
}

export interface HostHistory {
  resolution_secs: number;
  points: TrendPoint[];
}
