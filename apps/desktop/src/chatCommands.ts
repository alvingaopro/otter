// Slash commands in the chat boxes (D-190). Every command is listed; one
// that can't be used now is dimmed, with why. In a task's conversation a
// command acts at once (as the task's buttons do), opens something in the
// app, or goes as a message that otterd does (`task.send`'s `command`). In
// the empty Chat page's box, a command is a condition on the new task.
//
// What the buttons and the menu offer comes from here, so they agree.

import type { RuntimeCapabilities } from "./conversation";
import type { WorkspaceRequest } from "./taskSource";
import {
  HELD,
  deletable,
  gatesClear,
  pendingDecisions,
  type MessageCommand,
  type Task,
  type TaskAction,
  type WorkspaceChoice,
} from "./tasks";

/** One value the word after a command can be. */
export interface Choice {
  value: string;
  label: string;
  detail?: string;
}

export interface Command<C, E> {
  name: string;
  /** The menu's heading it is under. */
  group: string;
  /** What goes after it, to show: `what next`. */
  arg?: string;
  /** What it does, in a line. */
  what: string;
  /** Said above the box while it's there: what Enter does. */
  does: (ctx: C) => string;
  /** The send button's label meanwhile. */
  button: string;
  /** `true`, or why not now. */
  available: (ctx: C) => true | string;
  /** What the word after it can be; picking one completes it. */
  choices?: (ctx: C) => Choice[];
  /** Words may follow (its choice, if it has choices). */
  words?: boolean;
  /** Asks for Enter twice: what it says the second time. */
  confirm?: (ctx: C) => string | undefined;
  /** What it comes to with `rest` after it, or what's missing. */
  effect: (rest: string, ctx: C) => E | { problem: string };
}

export function isProblem<E>(e: E | { problem: string }): e is { problem: string } {
  return typeof e === "object" && e !== null && "problem" in e;
}

// --- The box ---------------------------------------------------------------

/** `、` and `／` are what `/` types with Pinyin's punctuation: at the start, they are `/`. */
export function slashed(text: string): string {
  return /^[、／]/.test(text) ? `/${text.slice(1)}` : text;
}

/** The box as a command: its name, what follows, and whether the name is
 * finished (a space came after it). `null`: not a command. */
export function parse(text: string): { name: string; rest: string; named: boolean } | null {
  const m = /^\/([^\s/]*)(\s+([\s\S]*))?$/.exec(slashed(text));
  if (!m) return null;
  return { name: m[1].toLowerCase(), rest: m[3] ?? "", named: m[2] !== undefined };
}

/** The first word and what follows it. */
function firstWord(rest: string): [string, string] {
  const m = /^(\S*)\s*([\s\S]*)$/.exec(rest.trim());
  return m ? [m[1], m[2].trim()] : ["", ""];
}

export function find<C, E>(commands: Command<C, E>[], name: string): Command<C, E> | undefined {
  return commands.find((c) => c.name === name);
}

/** The commands for what is typed after `/`, as the menu lists them: by
 * heading, those usable now first; with a query, the best matches first. */
export function matches<C, E>(commands: Command<C, E>[], query: string, ctx: C): { command: Command<C, E>; why?: string }[] {
  const q = query.toLowerCase();
  const groups = [...new Set(commands.map((c) => c.group))];
  // A word of what it does finds it too, once there is enough of the word.
  const said = (c: Command<C, E>) =>
    q.length >= 3 && `${c.arg ?? ""} ${c.what}`.toLowerCase().split(/[^a-z0-9-]+/).some((w) => w.startsWith(q));
  const score = (c: Command<C, E>) => (!q ? 1 : c.name.startsWith(q) ? 3 : c.name.includes(q) ? 2 : said(c) ? 1 : 0);
  return commands
    .map((command, i) => {
      const ok = command.available(ctx);
      return { command, why: ok === true ? undefined : ok, score: score(command), i };
    })
    .filter((r) => r.score > 0)
    .sort((a, b) =>
      q
        ? Number(!!a.why) - Number(!!b.why) || b.score - a.score || a.i - b.i
        : groups.indexOf(a.command.group) - groups.indexOf(b.command.group) || Number(!!a.why) - Number(!!b.why) || a.i - b.i,
    )
    .map(({ command, why }) => ({ command, why }));
}

/** A command's choices for the word typed so far. */
export function choicesFor<C, E>(command: Command<C, E>, typed: string, ctx: C): Choice[] {
  const q = typed.toLowerCase();
  const all = command.choices?.(ctx) ?? [];
  return all
    .map((c) => ({ c, s: c.value.toLowerCase().startsWith(q) ? 2 : `${c.value} ${c.label} ${c.detail ?? ""}`.toLowerCase().includes(q) ? 1 : 0 }))
    .filter((x) => x.s > 0)
    .sort((a, b) => b.s - a.s)
    .map((x) => x.c);
}

/** Minutes from `15m`, `1h`, `1h30m`, `90` (minutes). */
export function parseMinutes(word: string): number | undefined {
  const w = word.trim().toLowerCase();
  if (/^\d+$/.test(w)) return Number(w);
  const m = /^(?:(\d+)h)?(?:(\d+)m(?:in)?)?$/.exec(w);
  if (!m || (!m[1] && !m[2])) return undefined;
  return Number(m[1] ?? 0) * 60 + Number(m[2] ?? 0);
}

// --- A task's conversation ------------------------------------------------

/** What a task's command reads. */
export interface TaskCtx {
  task: Task;
  /** A run of the coding agent is at work. */
  live?: boolean;
  caps?: RuntimeCapabilities | null;
  workspaces?: WorkspaceChoice[];
  /** Otter's memory can be read here. */
  memory?: boolean;
  /** The details are open beside the conversation. */
  details?: boolean;
}

/** What a task's command comes to: an action, a message otterd does, or something to show. */
export type TaskEffect =
  | { kind: "act"; action: TaskAction }
  | { kind: "send"; command: MessageCommand; words: string }
  | { kind: "show"; show: "details" | "activity" | "changes" | "open" | "memory" | "delete" };

type TaskCommand = Command<TaskCtx, TaskEffect>;

const act = (action: TaskAction): TaskEffect => ({ kind: "act", action });
const send = (command: MessageCommand, words = ""): TaskEffect => ({ kind: "send", command, words });
const show = (s: Extract<TaskEffect, { kind: "show" }>["show"]): TaskEffect => ({ kind: "show", show: s });
const ok = (yes: boolean, why: string): true | string => (yes ? true : why);

/** The statuses Pause stops. */
const AT_WORK = ["planning", "implementing", "verifying", "blocked"];
const ended = (f: Task) => f.status === "done" || f.status === "cancelled";

/** Waiting for a workspace another task or the developer's agent holds (D-079, D-080, D-156). */
export function waitingForWorkspace(f: Task): boolean {
  const why = f.status_reason ?? "";
  return (
    pendingDecisions(f).length === 0 &&
    why.startsWith("Waiting for ") &&
    (why.endsWith("is working there") || why.endsWith(HELD)) &&
    f.runs.length === 0
  );
}

/** A message command needs an otterd that takes it; until the host says, it isn't known. */
const otterdTakes = (ctx: TaskCtx) =>
  ok(!!ctx.caps?.message_commands, ctx.caps ? "Update otterd on this host to use this" : "This host hasn't said yet whether it takes this");

const both = (...checks: (true | string)[]) => checks.find((c) => c !== true) ?? true;

const openWatchesOf = (f: Task) => (f.watches ?? []).filter((w) => w.status === "waiting");

export const TASK_COMMANDS: TaskCommand[] = [
  {
    name: "start",
    group: "Task",
    what: "Start this draft",
    does: () => "Enter starts the task",
    button: "Start",
    available: ({ task }) => ok(task.status === "draft", "Already started"),
    effect: () => act({ action: "start" }),
  },
  {
    name: "resume",
    group: "Task",
    what: "Go on from where it paused",
    does: () => "Enter goes on from where it paused",
    button: "Resume",
    available: ({ task }) => ok(task.status === "paused", "Not paused"),
    effect: () => act({ action: "resume" }),
  },
  {
    name: "retry",
    group: "Task",
    what: "Try again with a fresh budget",
    does: () => "Enter tries again, with a fresh budget",
    button: "Retry",
    available: ({ task }) => ok(task.status === "failed", "Hasn't failed"),
    effect: () => act({ action: "retry" }),
  },
  {
    name: "accept",
    group: "Task",
    what: "Accept the work in review",
    does: () => "Enter accepts the work",
    button: "Accept",
    available: ({ task }) => ok(task.status === "review", "Not in review yet"),
    confirm: ({ task }) => (gatesClear(task) ? undefined : "Some gates haven't passed: Enter again accepts anyway, and the report says so"),
    effect: (_, { task }) => act(gatesClear(task) ? { action: "accept" } : { action: "accept", override_gates: true }),
  },
  {
    name: "pause",
    group: "Task",
    what: "Stop the work; it waits until you resume",
    does: () => "Enter stops the work; it waits until you resume",
    button: "Pause",
    available: ({ task }) => ok(AT_WORK.includes(task.status), "Nothing to pause"),
    effect: () => act({ action: "pause" }),
  },
  {
    name: "stop",
    group: "Task",
    arg: "what next",
    what: "Stop the agent's turn and have it do this",
    does: () => "Enter stops the coding agent's turn and has it do this next",
    button: "Stop & send",
    words: true,
    available: ({ live, caps }) =>
      both(ok(!!live, "The coding agent isn't working"), ok(!!caps?.features.redirect, "This host's coding agent can't be stopped mid-turn")),
    effect: (rest) => (rest.trim() ? act({ action: "redirect", text: rest.trim() }) : { problem: "Say what it should do instead" }),
  },
  {
    name: "workspace",
    group: "Task",
    arg: "name",
    what: "Choose where the task works",
    does: () => "Enter sets the task's workspace",
    button: "Set workspace",
    available: ({ task, live }) => both(ok(!ended(task), "The task is over"), ok(!live, "Pause first: the agent is working")),
    choices: ({ task, workspaces = [] }) =>
      workspaces.map((w) => ({ value: w.name, label: w.name, detail: w.id === task.workspace_id ? "where it works now" : w.repository })),
    effect: (rest, { task, workspaces = [] }) => {
      const name = rest.trim().toLowerCase();
      if (!name) return { problem: "Say which workspace" };
      const w = workspaces.find((w) => w.name.toLowerCase() === name || w.id === rest.trim());
      if (!w) return { problem: `No workspace “${rest.trim()}” on this host` };
      if (w.id === task.workspace_id) return { problem: "It works there already" };
      return act({ action: "set_workspace", workspace: w.id });
    },
  },
  {
    name: "own-workspace",
    group: "Task",
    what: "Work in a workspace of its own (a worktree)",
    does: () => "Enter gives the task a worktree of its own",
    button: "Own workspace",
    available: ({ task }) => ok(waitingForWorkspace(task), "Not waiting for a workspace"),
    effect: () => act({ action: "own_workspace" }),
  },
  {
    name: "unwatch",
    group: "Task",
    arg: "watch",
    what: "Stop a watch",
    does: () => "Enter stops the watch",
    button: "Stop watch",
    available: ({ task }) => ok(openWatchesOf(task).length > 0, "Nothing is watched"),
    choices: ({ task }) => openWatchesOf(task).map((w) => ({ value: w.id, label: w.label, detail: w.kind === "ci" ? "CI" : "reminder" })),
    effect: (rest, { task }) => {
      const open = openWatchesOf(task);
      const q = rest.trim().toLowerCase();
      const w = q ? open.find((w) => w.id === rest.trim() || w.label.toLowerCase().startsWith(q)) : open.length === 1 ? open[0] : undefined;
      return w ? act({ action: "cancel_watch", watch_id: w.id }) : { problem: q ? "No watch like that" : "Say which watch" };
    },
  },
  {
    name: "cancel",
    group: "Task",
    what: "Cancel the task",
    does: () => "Enter cancels the task",
    button: "Cancel task",
    available: ({ task }) => ok(!ended(task), "Already over"),
    confirm: () => "Enter again to cancel the task · Esc keeps it",
    effect: () => act({ action: "cancel" }),
  },
  {
    name: "ask",
    group: "Tell",
    arg: "question",
    what: "Ask the coding agent; it only reads",
    does: () => "Enter asks the coding agent; it only reads, and answers here",
    button: "Ask",
    words: true,
    available: (ctx) =>
      both(
        otterdTakes(ctx),
        ok(!!ctx.live, "The coding agent isn't working: ask while it works"),
        ok(ctx.caps?.backend === "sdk", "Needs the coding agent on the SDK runtime (Settings)"),
      ),
    effect: (rest) => (rest.trim() ? send({ name: "ask" }, rest) : { problem: "Say what to ask" }),
  },
  {
    name: "tell",
    group: "Tell",
    arg: "message",
    what: "Pass this to the coding agent as written",
    does: () => "Enter passes this to the coding agent as you wrote it, after its turn",
    button: "Tell",
    words: true,
    available: (ctx) => both(otterdTakes(ctx), ok(!!ctx.live, "The coding agent isn't working")),
    effect: (rest) => (rest.trim() ? send({ name: "tell" }, rest) : { problem: "Say what to pass on" }),
  },
  {
    name: "revise",
    group: "Tell",
    arg: "what changed",
    what: "The goal changed: plan again",
    does: () => "Enter plans again from where the work is, with this",
    button: "Revise",
    words: true,
    available: (ctx) =>
      both(otterdTakes(ctx), ok(ctx.task.status !== "cancelled", "The task was cancelled"), ok(ctx.task.status !== "draft", "Start it first")),
    effect: (rest) => (rest.trim() ? send({ name: "revise" }, rest) : { problem: "Say what changed" }),
  },
  {
    name: "remember",
    group: "Tell",
    arg: "note",
    what: "Add a line to this task's memory",
    does: () => "Enter notes this in the task's memory",
    button: "Remember",
    words: true,
    available: (ctx) => both(otterdTakes(ctx), ok(ctx.task.status !== "cancelled", "The task was cancelled")),
    effect: (rest) => (rest.trim() ? send({ name: "remember" }, rest) : { problem: "Say what to remember" }),
  },
  {
    name: "watch",
    group: "Tell",
    arg: "ci",
    what: "Watch the pull request's CI and say when it's done",
    does: () => "Enter watches the pull request's CI",
    button: "Watch",
    words: true,
    available: (ctx) => both(otterdTakes(ctx), ok(!!ctx.task.delivery?.pr_number || !!ctx.task.delivery?.pr_url, "No pull request yet")),
    choices: () => [{ value: "ci", label: "ci", detail: "the pull request's checks" }],
    effect: (rest) => {
      const [kind, after] = firstWord(rest);
      return kind.toLowerCase() === "ci" ? send({ name: "watch_ci" }, after) : { problem: "Only CI can be watched: /watch ci" };
    },
  },
  {
    name: "remind",
    group: "Tell",
    arg: "30m",
    what: "Look at this task again later",
    does: () => "Enter has Otter look at this again then",
    button: "Remind",
    words: true,
    available: (ctx) => both(otterdTakes(ctx), ok(ctx.task.status !== "cancelled", "The task was cancelled")),
    choices: () =>
      [
        ["15m", "in 15 minutes"],
        ["30m", "in 30 minutes"],
        ["1h", "in an hour"],
        ["2h", "in 2 hours"],
        ["4h", "in 4 hours"],
      ].map(([value, detail]) => ({ value, label: value, detail })),
    effect: (rest) => {
      const [when, after] = firstWord(rest);
      const n = parseMinutes(when);
      if (!when || n === undefined || n < 1) return { problem: "Say when: 15m, 30m, 1h…" };
      if (n > 7 * 24 * 60) return { problem: "At most a week from now" };
      return send({ name: "remind", in_minutes: n }, after);
    },
  },
  {
    name: "answer-only",
    group: "Tell",
    arg: "on|off",
    what: "Only an answer is wanted, or changes are fine",
    does: () => "Enter changes whether the task may change things",
    button: "Set",
    available: (ctx) => both(otterdTakes(ctx), ok(!ended(ctx.task), "The task is over")),
    choices: () => [
      { value: "on", label: "on", detail: "only an answer: the coding agent only reads" },
      { value: "off", label: "off", detail: "changes are fine" },
    ],
    effect: (rest, { task }) => {
      const w = rest.trim().toLowerCase();
      if (w !== "on" && w !== "off") return { problem: "on or off" };
      const on = w === "on";
      if (on === !!task.answer_only) return { problem: on ? "It's answer-only already" : "It isn't answer-only" };
      return send({ name: "answer_only", on });
    },
  },
  {
    name: "details",
    group: "Show",
    what: "Show or hide the details",
    does: ({ details }) => (details ? "Enter hides the details" : "Enter shows the details"),
    button: "Details",
    available: () => true,
    effect: () => show("details"),
  },
  {
    name: "activity",
    group: "Show",
    what: "What the coding agent did",
    does: () => "Enter shows the coding agent's activity",
    button: "Activity",
    available: () => true,
    effect: () => show("activity"),
  },
  {
    name: "changes",
    group: "Show",
    what: "What the task changed",
    does: () => "Enter shows what the task changed",
    button: "Changes",
    available: ({ task }) => ok(!!task.workspace_id, "No workspace yet"),
    effect: () => show("changes"),
  },
  {
    name: "open",
    group: "Show",
    what: "Open the workspace",
    does: () => "Enter opens the task's workspace",
    button: "Open",
    available: ({ task }) => ok(!!task.workspace_id, "No workspace yet"),
    effect: () => show("open"),
  },
  {
    name: "memory",
    group: "Show",
    what: "Otter's memory for this task",
    does: () => "Enter opens Otter's memory",
    button: "Memory",
    available: ({ memory }) => ok(!!memory, "This host keeps no memory for Otter"),
    effect: () => show("memory"),
  },
  {
    name: "delete",
    group: "Show",
    what: "Delete the task…",
    does: () => "Enter asks before deleting the task",
    button: "Delete…",
    available: ({ task }) => ok(deletable(task), "Not while it works"),
    effect: () => show("delete"),
  },
];

/** Whether the task command `name` can be used now (the buttons ask this too). */
export function can(name: string, ctx: TaskCtx): boolean {
  return find(TASK_COMMANDS, name)?.available(ctx) === true;
}

// --- A new task, from the empty Chat page --------------------------------

export interface NewTaskCtx {
  hosts: string[];
  workspaces: Record<string, WorkspaceChoice[]>;
}

/** A condition on the task the empty page's box starts. */
export type NewTaskOption =
  | { kind: "in"; host: string; workspace: string; label: string }
  | { kind: "repo"; repository: string }
  | { kind: "scratch" }
  | { kind: "host"; host: string }
  | { kind: "answer_only" }
  | { kind: "draft" };

type NewTaskCommand = Command<NewTaskCtx, NewTaskOption>;

/** Every workspace on every host, named `name@host` where a name is on several. */
function everyWorkspace(ctx: NewTaskCtx): { host: string; w: WorkspaceChoice; value: string }[] {
  const all = Object.entries(ctx.workspaces).flatMap(([host, ws]) => ws.map((w) => ({ host, w })));
  const named = (n: string) => all.filter((x) => x.w.name === n).length;
  return all.map(({ host, w }) => ({ host, w, value: named(w.name) > 1 ? `${w.name}@${host}` : w.name }));
}

function repositories(ctx: NewTaskCtx): string[] {
  return [...new Set(Object.values(ctx.workspaces).flatMap((ws) => ws.flatMap((w) => (w.repository ? [w.repository] : []))))];
}

export const NEW_TASK_COMMANDS: NewTaskCommand[] = [
  {
    name: "in",
    group: "New task",
    arg: "workspace",
    what: "Work in this workspace",
    does: () => "Enter has the task work in this workspace",
    button: "Use",
    available: (ctx) => ok(everyWorkspace(ctx).length > 0, "No workspaces yet"),
    choices: (ctx) => everyWorkspace(ctx).map(({ host, w, value }) => ({ value, label: value, detail: [host, w.repository].filter(Boolean).join(" · ") })),
    words: true,
    effect: (rest, ctx) => {
      const [name] = firstWord(rest);
      if (!name) return { problem: "Say which workspace" };
      const found = everyWorkspace(ctx).filter((x) => x.value.toLowerCase() === name.toLowerCase() || x.w.name.toLowerCase() === name.toLowerCase());
      if (found.length === 0) return { problem: `No workspace “${name}”` };
      if (found.length > 1) return { problem: `“${name}” is on several hosts: say ${found.map((x) => `${x.w.name}@${x.host}`).join(" or ")}` };
      return { kind: "in", host: found[0].host, workspace: found[0].w.id, label: found[0].value };
    },
  },
  {
    name: "repo",
    group: "New task",
    arg: "repository",
    what: "A workspace of its own from this repository",
    does: () => "Enter makes the task a workspace of its own from this repository",
    button: "Use",
    available: () => true,
    choices: (ctx) => repositories(ctx).map((r) => ({ value: r, label: r })),
    words: true,
    effect: (rest) => {
      const [repository] = firstWord(rest);
      return repository ? { kind: "repo", repository } : { problem: "Say which repository" };
    },
  },
  {
    name: "scratch",
    group: "New task",
    what: "Work in an empty directory",
    does: () => "Enter has the task work in an empty directory",
    button: "Use",
    available: () => true,
    effect: () => ({ kind: "scratch" }),
  },
  {
    name: "host",
    group: "New task",
    arg: "name",
    what: "Start it on this host",
    does: () => "Enter starts the task on this host",
    button: "Use",
    available: ({ hosts }) => ok(hosts.length > 1, "Only one host"),
    choices: ({ hosts }) => hosts.map((h) => ({ value: h, label: h })),
    words: true,
    effect: (rest, { hosts }) => {
      const [name] = firstWord(rest);
      const h = hosts.find((h) => h.toLowerCase() === name.toLowerCase());
      return h ? { kind: "host", host: h } : { problem: name ? `No host “${name}”` : "Say which host" };
    },
  },
  {
    name: "answer-only",
    group: "New task",
    what: "Only an answer: nothing is changed",
    does: () => "Enter makes it a task that only answers",
    button: "Use",
    available: () => true,
    effect: () => ({ kind: "answer_only" }),
  },
  {
    name: "draft",
    group: "New task",
    what: "Keep it as a draft; don't start it",
    does: () => "Enter keeps the task a draft",
    button: "Use",
    available: () => true,
    effect: () => ({ kind: "draft" }),
  },
];

/** The words a new-task command leaves in the box: its choice is used up. */
export function leftOver(command: NewTaskCommand, rest: string): string {
  return command.choices ? firstWord(rest)[1] : rest.trim();
}

/** The conditions so far with `o` added: where it works is one of in, repo or scratch. */
export function withOption(options: NewTaskOption[], o: NewTaskOption): NewTaskOption[] {
  const where = ["in", "repo", "scratch"];
  const kept = (x: NewTaskOption) =>
    x.kind !== o.kind &&
    !(where.includes(o.kind) && where.includes(x.kind)) &&
    // A workspace is on its host; a host is where its workspace is.
    !(o.kind === "in" && x.kind === "host") &&
    !(o.kind === "host" && x.kind === "in" && x.host !== o.host);
  return [...options.filter(kept), o];
}

/** What a condition says, on its chip. */
export function optionLabel(o: NewTaskOption): string {
  switch (o.kind) {
    case "in":
      return `in ${o.label}`;
    case "repo":
      return `new from ${o.repository}`;
    case "scratch":
      return "empty directory";
    case "host":
      return `on ${o.host}`;
    case "answer_only":
      return "only an answer";
    case "draft":
      return "draft";
  }
}

/** The new task's host, workspace, answer-only and start, from its conditions. */
export function newTask(options: NewTaskOption[]): {
  host?: string;
  workspace?: string | WorkspaceRequest;
  answerOnly: boolean;
  start: boolean;
} {
  let host: string | undefined;
  let workspace: string | WorkspaceRequest | undefined;
  for (const o of options) {
    if (o.kind === "host") host = o.host;
    if (o.kind === "in") {
      host = o.host;
      workspace = o.workspace;
    }
    if (o.kind === "repo") workspace = { new: { repository: o.repository } };
    if (o.kind === "scratch") workspace = { scratch: {} };
  }
  return { host, workspace, answerOnly: options.some((o) => o.kind === "answer_only"), start: !options.some((o) => o.kind === "draft") };
}
