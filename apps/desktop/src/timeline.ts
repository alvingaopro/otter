// Wording for a workspace's timeline: host events (otter-protocol `EventRecord`,
// flattened JSON) turned into one human line each. Pure, so it is easy to read.

import type { AttentionKind, Glyph, WorkspaceView } from "./types";

/** A host event as the backend forwards it. Fields depend on `type`. */
export interface EventRecord {
  seq: number;
  ts: string;
  type: string;
  workspace_id?: string;
  session_id?: string;
  attention_id?: string;
  name?: string;
  kind?: string;
  state?: string;
  exit_code?: number;
  message?: string;
  provider?: string;
}

export interface Row {
  seq: number;
  ts: string;
  glyph: Glyph;
  text: string;
  detail?: string;
}

const ATTENTION: Record<AttentionKind, string> = {
  question: "asked a question",
  approval: "needs approval",
  failure: "needs a look after a failure",
  review: "has work to review",
  completion: "finished",
};

const NOUN: Record<AttentionKind, string> = {
  question: "question",
  approval: "approval request",
  failure: "failure",
  review: "review",
  completion: "completion",
};

const AGENT: Record<string, [Glyph, string] | undefined> = {
  working: ["working", "working"],
  blocked: ["needs", "seems to be waiting on you"],
  waiting_for_input: ["done", "finished its turn"],
  idle: ["idle", "idle"],
  exited: ["idle", "agent exited"],
  // `starting` is implied by "started".
};

const firstLine = (s?: string) => s?.split("\n")[0];

/**
 * Rows for a workspace's events, newest first. Sessions are named from the
 * workspace as it is now, or from the events (deleted sessions).
 */
export function timelineRows(records: EventRecord[], ws: WorkspaceView): Row[] {
  const sorted = [...records].sort((a, b) => a.seq - b.seq);
  const names = new Map(ws.sessions.map((s) => [s.id, s.name]));
  for (const r of sorted) if (r.type === "SessionCreated" && r.session_id && r.name && !names.has(r.session_id)) names.set(r.session_id, r.name);
  const who = (r: EventRecord) => (r.session_id && names.get(r.session_id)) || "a session";
  // What an attention item was about, for when it is resolved.
  const attention = new Map<string, EventRecord>();
  const lastAgent = new Map<string, string>();
  // The latest event type per session.
  const lastType = new Map<string, string>();

  const rows: Row[] = [];
  for (const r of sorted) {
    const row = (glyph: Glyph, text: string, detail?: string) => rows.push({ seq: r.seq, ts: r.ts, glyph, text, detail });
    switch (r.type) {
      case "WorkspaceCreated":
        row("idle", "Workspace created");
        break;
      case "WorkspaceReady":
        row("done", "Ready");
        break;
      case "WorkspaceFailed":
        row("failed", "Preparation failed", firstLine(r.message));
        break;
      case "WorkspaceArchived":
        row("idle", "Archived");
        break;
      case "WorkspaceUnarchived":
        row("idle", "Unarchived");
        break;
      case "WorkspaceBriefChanged":
        row("idle", "Brief edited");
        break;
      case "EnvironmentPreparing":
        row("working", "Preparing the environment");
        break;
      case "EnvironmentReady":
        row("done", "Environment ready");
        break;
      case "EnvironmentFailed":
        row("failed", "Environment failed", firstLine(r.message));
        break;
      case "SessionCreated":
        row("idle", `${r.name ?? who(r)} created`, r.kind);
        break;
      case "SessionStopped":
        row("idle", `${who(r)} stopped`);
        break;
      case "SessionDeleted":
        row("idle", `${who(r)} deleted`);
        break;
      case "ExecutionStarted":
        if (r.session_id) lastAgent.delete(r.session_id);
        row("working", `${who(r)} started`);
        break;
      case "ExecutionExited":
        if (r.exit_code === undefined) row("idle", `${who(r)} ended`);
        else if (r.exit_code === 0) row("done", `${who(r)} exited`);
        else row("failed", `${who(r)} exited with status ${r.exit_code}`);
        break;
      case "ExecutionLost":
        row("failed", `${who(r)}’s process is gone`);
        break;
      case "ExecutionFailed":
        row("failed", `${who(r)} failed to start`, firstLine(r.message));
        break;
      case "AgentStateChanged": {
        const said = r.state && AGENT[r.state];
        // Only changes worth a line: skip `starting` and repeats.
        if (!said || !r.session_id || lastAgent.get(r.session_id) === r.state) break;
        lastAgent.set(r.session_id, r.state!);
        row(said[0], `${who(r)} ${said[1]}`);
        break;
      }
      case "AttentionCreated": {
        if (r.attention_id) attention.set(r.attention_id, r);
        const kind = r.kind as AttentionKind;
        const glyph: Glyph = kind === "failure" ? "failed" : kind === "completion" || kind === "review" ? "done" : "needs";
        // A failed or finished process already has its line ("exited…").
        if ((kind === "failure" || kind === "completion") && r.session_id && lastType.get(r.session_id)?.startsWith("Execution")) break;
        row(glyph, r.session_id ? `${who(r)} ${ATTENTION[kind] ?? kind}` : `Workspace ${ATTENTION[kind] ?? kind}`);
        break;
      }
      case "AttentionResolved": {
        const was = r.attention_id ? attention.get(r.attention_id) : undefined;
        // Resolved by the developer, or because the agent moved on.
        const noun = was && (NOUN[was.kind as AttentionKind] ?? was.kind);
        const what = noun ? noun[0].toUpperCase() + noun.slice(1) : "Attention";
        row("idle", was?.session_id ? `${what} from ${who(was)} resolved` : `${what} resolved`);
        break;
      }
      case "BrowserOpenRequested":
        row("idle", "Sign-in page requested", r.provider);
        break;
      default:
      // Kinds this app doesn't know yet: left out.
    }
    if (r.session_id) lastType.set(r.session_id, r.type);
  }
  return rows.reverse();
}

/** Local date and time, for a row's tooltip. */
export const when = (iso: string) => new Date(iso).toLocaleString();
