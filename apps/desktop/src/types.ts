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
  /** The host's workd version, once connected. */
  version?: string;
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
