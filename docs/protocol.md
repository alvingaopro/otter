# Otter protocol (v3)

The contract between `otterd` and its clients (the `otter` CLI and the
desktop app). Types live in `crates/protocol`; this document is the prose. Decisions:
D-004 (shape), D-016 (cursors, snapshot, compatibility), D-037 (bounded
log, log id), D-107 (v3: features are tasks).

## Connection

A connection is a byte stream to one host's daemon: the Unix socket
`~/.otter/run/workd.sock`, reached through `otterd dial` (locally, or as
`ssh host otterd dial`). `otterd dial` starts the daemon if it isn't running.

1. The server sends `{"type":"hello","protocol":2,"version":"0.1.0"}`.
   `protocol` is checked by the client; a mismatch is a clear error
   ("install matching versions"), never a best-effort session.
2. **RPC mode.** Newline-delimited JSON. The client sends
   `{"id":N,"method":"…","params":{…}}` (`params` omitted for methods without
   any); the server answers each with
   `{"type":"response","id":N,"result":…}` or `…"error":{"code","message"}}`.
   Requests on one connection are answered in order.
3. Two methods take the connection over for good:
   - `events.subscribe` → **event stream** (below).
   - `session.attach` → **attach mode**, binary frames (below).

Closing a connection never affects remote work (invariant 3).

## Control plane vs. data plane

Events and RPC results carry control-plane state only: workspaces, sessions,
executions, attention, lifecycle. Terminal bytes, input and resizes travel only
in attach frames, and are never written to `events.jsonl` or turned into
events. Events never contain commands, environment variables or other
potentially secret data.

## State and events

`otterd` is not event-sourced: current state comes from a snapshot, events say
what changed after it.

### `state.snapshot`

```json
{"seq": 103, "log_id": "log_…", "workspaces": [ … ]}
```

`workspaces` include their sessions, executions and attention. (`host.status`
is separate: it probes the host's tools and is slower.)

`seq` is the event cursor the snapshot corresponds to, in the event log named
by `log_id`. The snapshot reflects **every event up to `seq`, and possibly some
later ones** — so clients apply events idempotently (simplest: treat an event
as "refetch what it names").

A snapshot is read from the daemon's current state, not rebuilt from events,
so it is complete however much of the log has been rotated away: it is always
the way to resync.

### Events

```json
{"type":"event","event":{"seq":104,"ts":"…","type":"AttentionCreated","workspace_id":"ws_…", …}}
```

`seq` is per event log, strictly increasing and survives daemon restarts. It
is not guaranteed contiguous (a log line the daemon cannot parse is skipped),
so clients compare cursors with `>`, never count them.

### The event log

The host keeps a bounded log (D-037): about the last 75–100k events (at most
~16 MiB in `~/.otter/state/`, in 4 MiB segments; the oldest segment is dropped
as a new one starts). A cursor is a `seq` **and** the `log_id` it came from.
The log id stays the same across daemon restarts and rotation; it changes only
when the log starts over (its files were removed), and `seq` then starts again
from 1.

### `events.subscribe`

Params: `{"after": 103, "log_id": "log_…"}` (both optional; send `log_id`
whenever the cursor came with one). Result: `{"seq": 110, "log_id": "log_…"}`
— the latest event when the subscription started, and the log. Then the
connection carries `event` messages:

- with `after`: every logged event with `seq > after`, in order, then live
  events — each exactly once, with no gap between replay and live;
- without `after`: live events with `seq > result.seq`.

If the daemon falls behind its internal buffer it catches up from the log
rather than dropping events; if what it missed has meanwhile been rotated away
it closes the connection instead (the client's next subscribe gets
`cursor_expired`). Anything the client sends after subscribing is ignored;
closing the connection ends the subscription.

Replay reads only the log segments from the cursor on, and recent cursors
(up to the last ~2000 events) are served from memory.

**Cursor errors.** If `after` cannot be served, the request fails with
`cursor_expired`, after which the server closes the connection (open a new
one for the snapshot):

- `log_id` names a different log (the host's log started over — even if it
  has since grown past `after`),
- `after` is older than the oldest retained event (rotated away), or
- `after` is newer than the latest event (the log started over; the only way
  to notice that when the client sends no `log_id`).

The client then reloads a snapshot and subscribes after its cursor. Events are
never silently skipped.

Versions: daemons before D-037 send no `log_id` and ignore it in requests;
clients then fall back to `seq` alone (where a log that started over and grew
past the cursor goes unnoticed). Clients before D-037 send no `log_id`; the
daemon checks their `seq` as before.

### Client recipes

```text
fresh client:   snapshot ─► subscribe(after = snapshot.seq, log_id = snapshot.log_id) ─► apply events
reconnecting:   subscribe(after = last seen seq, log_id = Subscribed.log_id)
                  └─ cursor_expired ─► fresh client
```

`otter events --follow` is the reference implementation (it resumes from the
last event it printed whenever its connection drops).

`events.list {limit}` returns the most recent `limit` events (oldest first) for
display; it is not a sync mechanism.

## Host views

- `host.metrics` → resource usage for the host page: CPUs, load, uptime,
  memory and swap, disk space per filesystem, the busiest processes, and a
  history of samples (CPU %, memory, network and disk I/O rates) every
  `interval_secs` for about the last 10 minutes. Sampled in the background
  from the daemon's start; fails with `unavailable` until the first sample.
- `host.history {range}` → recorded usage for `1h` (1-minute points), `24h`
  (5 min), `7d` (30 min) or `30d` (1 h): averages, with peaks for CPU and
  memory. Recorded on the host and kept for 7 days per minute, 90 days per
  hour; missing stretches (otterd not running) are absent.
- `host.ports` → TCP ports listening on the host (`ss` on Linux, `lsof` on
  macOS), with the process when the daemon's user may see it.

Both are additions (no protocol bump); daemons before them answer
`invalid_request`.

- `host.processes` → `{sampled_at, processes: [{pid, started, parent?, name,
  exe?, cwd?, cpu_percent, memory, busy_secs, owner, stoppable}]}` (D-161): the
  processes Otter started or must answer for, as of the last 5 s sample
  (`unavailable` until the first). `started` (seconds since the epoch) with
  `pid` names a process. `home` (D-169) is an otterd's `--home`, which one it is. `owner` is `{kind: "daemon", this}` (an otterd: this
  one, or another), `{kind: "execution", workspace_id, session_id,
  execution_id}` or `{kind: "run", task_id?, workspace_id}` (in the process
  tree of a session's running execution, or of a coding agent's run, found
  through each process's parents), or `{kind: "orphan", why}` (working in a
  workspace folder of this host whose workspace is gone or archived, with no
  such tree above it). `busy_secs`: how long it has used 25% of a CPU or more.
  A command line is never sent: it can carry a token.
- `host.processes.stop {pid, started, force?}` → `null` (D-161). Ends what
  `host.processes` lists with `stoppable: true` — an otterd that isn't this
  one, or an orphan — by SIGTERM, or SIGKILL with `force`, to its process
  group when it leads one. `conflict` for this otterd, for a session's or a
  run's process ("stop it there"), for a `started` that isn't the process's
  now (a reused id), and for anything not listed.

## Archiving workspaces

- `workspace.archive {workspace}` → the workspace, now `state: "archived"`.
  Running sessions are stopped (`SessionStopped`; exited ones keep their
  output for `session.read`), open attention is resolved
  (`AttentionResolved`), then `WorkspaceArchived {workspace_id, name}`. Files,
  Git worktree and branch are kept. Only `ready` or `failed` workspaces can be
  archived (`conflict` otherwise, also while `preparing`).
- `workspace.unarchive {workspace}` → the workspace, `preparing` (or already
  `ready`/`failed`): emits `WorkspaceUnarchived {workspace_id}`, then the usual
  `WorkspaceReady` / `WorkspaceFailed`. Stopped sessions stay stopped until
  `session.restart`. `conflict` unless archived.
- `workspace.prepare` and each step of preparation emit
  `WorkspacePreparing {workspace_id}` (what it is doing is in the workspace).
- While archived, `session.create`, `session.restart` and `workspace.prepare`
  fail with `conflict`; `workspace.delete` works as usual.
- `workspace.list` and `state.snapshot` still return archived workspaces;
  clients hide them (by `state`) unless asked to show them.

All additions (no protocol bump): daemons before them answer
`invalid_request`, and `archived` was already a `WorkspaceState` value.

## Workspace brief

- `workspace.create {…, brief}` and `workspace.set_brief {workspace, brief}`
  take a `Brief`: `{title?, description?, constraints?, references?,
  decisions?}` (strings and string lists, all optional). There is no `goal`
  (D-098): an older client's is ignored, and a stored one dropped on the
  next save.
- `workspace.set_brief` → the workspace. It **replaces** the whole brief
  (fields left out are cleared): clients edit by read-modify-write of the
  brief they were shown. The daemon trims text, drops blank text and blank
  list items. Allowed in any state, including `archived`.
- A change emits `WorkspaceBriefChanged {workspace_id}`; setting the same
  brief again emits nothing. The event never carries the brief's text: read
  the workspace.

An addition (no protocol bump): daemons before it answer `invalid_request`.

## Deleting workspaces

- `workspace.delete {workspace, force?}` stops the workspace's sessions and
  removes what Otter made for it (D-108). That is its directory under
  `~/.otter/workspaces/`, never an attached folder. For a Git workspace it
  is the worktree and its branch, and the repository's copy under
  `~/.otter/repos/` once no workspace uses it.
- The copy is kept, though, while any branch in it holds commits on no
  remote.
- Without `force`, `conflict` when the worktree has uncommitted changes or
  the branch has commits not pushed anywhere; `force` discards both. The
  branch is the one recorded, or else the one git has checked out there. A
  checkout git never finished, in a workspace where nothing ever ran, has
  neither (D-135).
- Running sessions are stopped first (`SessionStopped`), then the files go,
  then `WorkspaceDeleted`. Meanwhile another `workspace.delete`,
  `workspace.prepare` or `workspace.unarchive` of it gets `conflict`
  (D-135). A delete that fails after stopping the workspace's preparation
  leaves it `failed` (`WorkspaceFailed`), saying so.
- Deleting the workspace made for a task (D-189) clears the task's
  `workspace_id` while it points there, and sets `Task.workspace_gone {name,
  repository?, base?, at, count, merged?}`. When Otter deleted it once pull
  request `merged` was merged, a new one is made if more is asked of the
  task.
- otterd deletes a task's own archived workspace by itself a day after the
  task's pull request is merged, unless something is only there. Then the
  workspace keeps `kept` (the reason), cleared when it is unarchived
  (D-189). The view of `settings.get` has `delete_merged_workspaces` (on by
  default); `settings.set {delete_merged_workspaces: bool}` turns it on or
  off. All additions (no protocol bump), absent from older daemons.

## Names and order

- `workspace.rename {workspace, name}` → the workspace; `session.rename
  {workspace, session, name}` → the session (D-101). Names follow the same
  rules as at creation (no `/` or `:`, trimmed), unique among the host's
  workspaces / the workspace's sessions (`already_exists`). Only the name
  changes: ids, the directory, the branch and running processes stay, so an
  attach or a reference by id carries on.
- They emit `WorkspaceRenamed {workspace_id, name}` and `SessionRenamed
  {workspace_id, session_id, name}`.
- An agent session the developer hasn't named takes the agent's own name
  for its conversation (D-197: Claude Code's title, `/` and `:` as `／` and
  `：`, unique with `-2`…), with the same `SessionRenamed`. A session's
  `name_from` says where its name came from: `default`, `agent` or
  `developer` (given at creation, or `session.rename`); absent on a
  session from before it. Only `default` and `agent` names follow the
  agent.
- `session.move {workspace, session, to}` → the workspace (D-150): the
  session goes to place `to` (from 0) among the workspace's sessions, past
  the end to the last; the others keep their order. That order is the one
  `workspace.get` and the snapshot list them in, and new sessions go last.
  It emits `SessionMoved {workspace_id, session_id, to}`, and nothing when
  the session was there already. The workspace's `updated_at` stays: the
  order is how it is shown, not work in it.

## A workspace's work

- `workspace.changes {workspace}` → `{git, branch?, base?, ahead, behind,
  files, more, insertions, deletions}` (D-102). In a Git work tree: the
  changes since the base — the workspace's base branch, else the branch's
  upstream; with neither, what isn't committed — committed or not, new files
  included, each `{path, change, insertions?, deletions?, uncommitted}`
  (`change`: `added`, `modified`, `deleted`, `renamed`, `untracked`; no
  counts for a binary file). Paths are relative to the workspace root and
  limited to it. At most 500 files; `more` counts the rest. Outside Git only
  `git: false`. Read-only: the index is never touched. `unpushed` (D-154) is
  the commits on HEAD that no remote has, whatever the base; absent from an
  older otterd. New files' lines are counted for at most 2000 files and
  64 MiB in all (D-151); past that they are listed without counts.
- `workspace.diff {workspace, path, context?}` → `{text, truncated}`: that
  file's unified diff since the same base (a new file against nothing), at
  most 512 KiB, with `context` lines around each change (git's 3 when
  absent; a large number gives the whole file, which the app folds, D-111).
- An attention item may carry `inferred: true` (D-109): the agent seems to
  be waiting (a still turn) but didn't say so. Absent when false; older
  clients ignore it.
- `workspace.ports {workspace}` → `[{session_id, port, address, process?}]`
  (D-104): the TCP ports listened on by a process in one of the workspace's
  running sessions' process trees, found from `host.ports` and each
  process's ancestry. Read when asked, never in the background.

- `workspace.publish {workspace, pull_request?, title?, body?}` →
  `{branch, uncommitted, pull_request?: {number, url, opened}, note?}`
  (D-154): `git push -u origin HEAD:refs/heads/<branch>` from the worktree
  and, with `pull_request`, the branch's open pull request — found and left as
  it is (`opened: false`), else opened through `gh` (Otter's sign-in when it
  has one, D-133) with `title` (the last commit's subject if absent). A
  developer's action asked for by a client: nothing is committed for them
  (`uncommitted` says what was left out), and Otter's own tools cannot call it
  (D-047). `conflict` unless a Ready Git workspace on a branch with an
  `origin`; without `gh` the branch is pushed and `note` says why there is no
  pull request.

Additions (no protocol bump): daemons before them answer `invalid_request`
(`unknown variant`), which clients take as "update this host".

## Host settings

The Control Agent's model and API keys on this host (D-048).

- `settings.get` → `{controller?, model?, controller_from_env?, secrets:
  [{name, purpose, set}]}`. A secret's value is never returned.
- `settings.set {controller?, model?, secrets?: {NAME: value | null}}` →
  the same view. Fields left out stay as they are; `""` resets the
  controller to automatic and the model to the default; `null` clears a
  secret. Unknown secret names and controllers fail with `invalid_request`.
- Besides each provider's key, `secrets` lists `CLAUDE_CODE_OAUTH_TOKEN`
  (D-142): Claude Code's sign-in, the token `claude setup-token` prints
  (`sk-ant-oat01-…`; anything else fails with `invalid_request`). It is no
  controller's key. Absent from daemons before it.
- The view also lists `controllers: [{id, label, secret?, models,
  default_model?}]` — every model provider (with the secret holding its
  key), `claude`, `rules` and `off` — and `active`, the controller in use
  now (what automatic chose). Both are absent from daemons before D-052.
- The view also has `coding: {backend, model?}` — how the coding agent runs
  (`legacy_cli` or `sdk`, D-057; `sdk` unless set, D-124) and its Claude
  model, apart from Otter's own; `settings.set {coding: {backend?, model?}}`
  changes them (`""` resets). Absent from older daemons.
- The view also has `otter_tools` (Otter answers with its tools, D-089/D-090),
  `otter_tools_from_env` (`OTTER_BRAIN_V2`, which overrides it) and
  `otter_answers` ("with tools (claude)", or "without tools…" and why);
  `settings.set {otter_tools: bool}` turns it on or off. `settings.set`
  answers with the same view as `settings.get`. Absent from older daemons.
- The view also has `git?: {name?, email?, ssh_key?: {public,
  fingerprint?}, key_account?, github?, login?: {state: pending, code, url} |
  {state: failed, error}}`: Otter's own git on the host (D-133).
  `settings.set {git: {name?, email?}}` sets who commits in Otter's
  repositories (`""` clears). Its other calls:
  - `git.key_create {replace?}` → the view: makes Otter's ed25519 key there.
  - `git.check` → `{ssh, github, git}`, each line `{ok, detail, account?}`:
    whether the key signs in to GitHub (from then on the repositories use
    it) and who Otter's gh is.
  - `git.key_add` → the same, after adding the key to the GitHub account
    Otter's gh is signed in to.
  - `git.github_login {token?}` → the view: with a token, signed in at
    once; without, a browser sign-in starts and `login` carries its code
    until it ends.
  - `git.github_logout` → the view.
  An older daemon answers `invalid_request`.
- The view also has `tools?: [{name, description, group, level,
  default_level}]` (D-191): each of Otter's tools — its own, the Claude
  Agent SDK's `WebSearch`, `WebFetch` and `Bash` it may use, and what the
  controller does on its own (`publish`, `approve_plan`; `accept_task` is
  gone, D-199: a delivered task is done by itself) —
  and how far it may go on this host: `auto` (Otter uses it on its own, in
  any turn), `developer` (only in a turn the developer's message started)
  or `off` (not offered). `group` is `look`, `place`, `task`, `delegate`,
  `wait`, `deliver` or `remember`. `settings.set {tool_levels: {name:
  level | null}}` sets them (`null`: back to the default); an unknown tool
  is `invalid_request`. `OTTER_TOOL_LEVELS` in otterd's environment
  (`name=level,…`) overrides them. Absent from older daemons.
- The view also has `slack?: {users, state, detail?, team?, bot?}` (D-200):
  Otter in Slack from this host. `users` are the Slack member IDs (`U…`)
  whose direct messages to the app are the developer's; `state` is `off`
  (`detail` says what is missing), `connecting`, `connected` (`team` and
  `bot`: the Slack workspace and the app's bot there) or `failed` (`detail`
  says why; otterd tries again); clients show an unknown one as it is.
  `settings.set {slack: {users}}` sets the members (`[]`: none). The app's
  two tokens are among `secrets`: `SLACK_BOT_TOKEN` (`xoxb-…`) and
  `SLACK_APP_TOKEN` (`xapp-…`, with `connections:write`); anything else
  for either is `invalid_request`. A direct message from one of `users`
  is said in Otter's conversation on the host (`otter.send` with
  `command_id` `slack:<channel>:<ts>[:<thread>]`, which is its message's
  `correlation_id`), and Otter's messages after it go back to Slack. Absent
  from older daemons. The view's `notify?` (D-201; absent from older
  daemons) says whether Slack is told when a task needs the developer —
  a direct message with the app's decision buttons, done as `task.act`
  (`command_id` `slack:act:<action_ts>`); a reply in that message's thread
  is said in the task (`task.send`, or `decide` with the reply as the
  answer to a question with no options); `settings.set {slack: {notify}}`
  turns it on or off.
- `settings.models {controller}` → `[{id, name?}]`: the models that
  controller offers, as its provider lists them, asked by the host with the
  key set there (OpenRouter's list needs none); `claude` answers Claude
  Code's model names. No key, or the provider unreachable: `conflict`;
  a controller without models: `invalid_argument`.

An addition (no protocol bump): daemons before it answer `invalid_request`.

## Runtime conversations

The coding agent's side of a task's work (D-055): conversations, their
turns, messages, tool calls and interactions. See
`otter_core::conversation`.

- `runtime.capabilities` → `{provider, backend, available, notes?,
  tested_version?, structured_ready, tasks: {send_turn, resume,
  interrupt_turn, permission_requests, questions, tool_results, streaming,
  attachments, usage, pause, redirect}}`: what the host's managed runtime
  can do. Missing tasks are reported, never simulated. `attachments` is
  whether a turn can carry pictures (D-171): true for `sdk`; for a runtime
  where it is false the pictures of a message are not sent, and the task gets
  one system message saying so. For `sdk`,
  `available` needs Node, the installed worker and its `--check` (D-060);
  `notes` say which is missing. `worker_missing?` says the worker this
  otterd looks for isn't installed, which installing it would fix (D-129).
  `message_commands?` says `task.send` takes a slash `command` (D-190); an
  otterd before it leaves it out.
- `runtime.test {backend?, model?}` → `{backend, ready, steps: [{name,
  state: passed|failed|skipped, detail}], worker_missing?}` (D-124, D-129): the coding agent
  started for one short reply, in a scratch directory under the host's
  state, every tool call refused, up to two minutes. Absent fields: the
  host's settings; no model set: a small one (`haiku`). Nothing is saved
  and no conversation is recorded. One at a time (`conflict` otherwise);
  an older daemon answers `invalid_request`.
- `conversation.list {task?}` → conversations, newest first;
  `conversation.get {conversation}` → one. Each is the conversation with
  `resumable` in place of the provider's own session id, which stays on
  the host, and `runtime_version?` — what its latest run ran on (D-060).

- `conversation.history {conversation, after?: {log_id, seq}, limit?}` →
  `{log_id, records, next?}`: the conversation's journal (D-058), one record
  per change, oldest first; at most `limit` (default 100, at most 500) and
  about 1 MiB. `next` is where the following page starts. A cursor from
  another journal fails with `cursor_expired`.
- A conversation whose journal can't be written or read is `read_only`
  (with why): its recorded history stays readable.
- Event `ConversationChanged {conversation_id, revision, task_id?}`
  after each change: ids only; read the content with `conversation.get`.

Read-only: coding work is sent by Otter, not by clients. Older daemons
answer `invalid_request` (or `unsupported`).

## Tasks

Product work owned by the daemon (D-043); see `otter_core::task`.

- `task.list` → every task, newest first; `task.get {task}` → one.
- `task.create {command_id, title, request?, workspace?}` → a `draft`
  task; the request becomes the first message. `workspace` is a workspace by
  id or name (a string, as always), `{"new": {"repository", "base"?,
  "branch"?}}` (a workspace made for the task from that repository, on a
  branch of its own) or `{"scratch": {}}` (an empty directory made for it)
  (D-179). The task is returned with the workspace set; a Git one is still
  being prepared, and the task waits for it. An older otterd answers the
  object forms `invalid_request`.
- `task.send {command_id, task, text, images?}` → the task, with the
  developer's message added. `images`: up to 4 pictures, each a `TaskImage`
  `{media_type, data}` (`image/png`, `image/jpeg` or `image/webp`; `data`
  base64, at most 700 KB decoded, 2 MB in all); `text` may then be empty. The
  daemon keeps each in the task's artifacts (`task.artifact`) and lists the
  names on the message's `attachments` (D-171). The pictures reach the coding
  agent's turn, and Otter's (D-181), as the worker's
  `send_turn {turn_id, text, images?}`, where
  `images` is the same list of `{media_type, data}` (the worker checks the
  same limits); worker frames, both ways, are at most 4 MiB (`MAX_FRAME`,
  was 1 MiB) so a turn with its pictures fits one.
  `command?` is a slash command (D-190): `{"name": "ask" | "tell" |
  "revise" | "remember" | "watch_ci"}`, `{"name": "remind", "in_minutes"}`
  (1 to a week) or `{"name": "answer_only", "on"}`, and `text` is then only
  the words after it (`ask`, `tell`, `revise` and `remember` need some;
  pictures will do but for `remember`). The message is stored with
  `command` and the line as written (`/remind 1h30m check the deploy`).
  otterd does it with the tool Otter's turn would have called (`ask_agent`,
  `forward_to_agent`, `revise_task`, `remember`, `watch`, `set_answer_only`),
  passing on the words without the command, and adds Otter's message about
  what came of it, correlated `command:<message id>`. Otter's turn doesn't
  answer it. An otterd before it ignores the field and takes the words as a
  plain message; clients send it only where `runtime.capabilities` says
  `message_commands`.
- `task.act {command_id, task, action, …}` → the task. `action`:
  `start`, `pause`, `resume`, `cancel`, `retry`, `accept {override_gates?}`,
  `request_changes {note?}` (the goal changed: plan again from where the
  work stands — any state but cancelled, D-050), `decide {decision_id,
  approve, answer?, answers?}` (`answers`: per question, for a form),
  `set_workspace {workspace}`, `preview`, `interrupt` (stop the coding
  agent's current turn and pause, D-059), `redirect {text}` (the current
  turn stops and this goes next; refused when nothing is running),
  `cancel_watch {watch_id}` (stop a watch; one already ended is a
  `conflict`), `delete_watch {watch_id}` (remove one; a waiting one is
  stopped first) (D-183; an older otterd answers these two
  `invalid_request`), `set_tool_level {tool, level?}` (this task's level
  for one of Otter's tools, D-191: it only tightens the host's; no `level`
  takes the host's again; an unknown tool is `invalid_request`). `set_workspace` and
  `own_workspace` move a task that hasn't worked (D-192); `own_workspace`
  splits a directory workspace in a repository too: a workspace of its own
  from its `origin`, on the branch it has checked out. An
  action the lifecycle doesn't allow fails with `conflict`, and so does
  `accept` while a gate (D-047) is open, unless `override_gates`. `accept`
  is for older clients only: a task in review ends by itself once its
  delivery settles — done, or failed saying which gate didn't pass — and
  never waits on the developer (D-199).
- `task.delete {task}` → `null`: removes a task with its history
  and artifacts; the workspace it used stays. A task being worked on
  (planning, implementing, verifying, or with a run going) fails with
  `conflict`: pause or cancel it first. Emits `TaskDeleted {task_id}`.
- `task.artifact {task, name}` → a file the task's checks produced
  (a screenshot), named as evidence refers to it (`artifact:<name>`):
  `{data (base64), size, eof: true}`, at most 8 MiB.
- `task.events {task, after?, limit?}` → the task's own history,
  `seq > after`, oldest first: `{seq, ts, v, correlation_id?, text, type,
  …}`. `seq` starts at 1 per task and never repeats; `v` is the record's
  schema version.

**Runs and recovery** (D-061): a task's `runs[].state` is `starting`,
`running`, `waiting`, or how it ended — `completed`, `failed`, `cancelled`,
`handed_off`, `interrupted` (ended with its process mid-turn; it resumes),
`recovery_required` (Otter won't continue it on its own). The latter opens
a `recovery` decision, `user_only`: `decide {approve: true}` starts the step
over, `approve: false` pauses the task. States and the crash matrix:
[`runtime-states.md`](runtime-states.md). Clients treat an unknown run state
as ended.

**A decision's whole command** (D-157; an optional field, old peers ignore
it): a decision on a coding agent's command that its one-line `summary`
cuts, or that runs over several lines, carries `command?`, the command as
written — redacted, at most 16 KiB, and when longer a last line `… N more
bytes, not kept`. Absent when the summary shows it all. `detail` stays the
reason it is asked.

**Where a task's own work starts** (D-167; an optional field, old peers
ignore it): a task has `started_from? {workspace_id, revision}`, the
revision its workspace was at when the task first worked there — absent
until then, outside Git, and in older documents. Verification's `Working
tree` evidence is the change since it; a task with nothing of its own to
publish there (D-166) has its `Pull request` and `CI` gates
`not_applicable`, detail `nothing to publish`.

**Watches** (D-183; an optional field, old peers ignore it): a task has
`watches?` — what Otter looks at again later for the developer, the open
ones and the last 20 ended: `{id, kind ("time", "ci"; clients show an
unknown kind by its label), target (the kind's own; `ci`: `{pr, repo}`),
label, what, asked_in?, created_at, deadline, status ("waiting", "firing",
"reported", "cancelled"; an unknown one reads as ended), readings? [{at, key, text}] (each change of what
it saw, the last 50), finding? {at, outcome, text}, findings, ended_at?,
report? (the message that said it)}`. `outcome` is `passed`, `failed`,
`stuck`, `gone`, `due`, `expired` or `unreadable`; an unknown one reads as
`unknown`. Watches are set by Otter on the developer's word in a task's
conversation; their changes come as `TaskChanged`. History: `WatchSet`,
`WatchFiring`, `WatchReported`, `WatchCancelled`, `WatchDeleted`, each with
`watch_id` and `label`.

**Watch kinds** (D-193): `ci` takes any pull request by its `url`
(`target {pr, repo}` as before), and `run` watches a GitHub Actions run
until it finishes (`target {repo, run}`): `passed`, `failed` or `gone`
(cancelled).

**Otter's own doing** (D-191–D-194; optional fields, old peers ignore
them): a task has `tool_levels?` (its own levels, above), `parent?` (the
task whose Otter made it, a sub-task) and `children_seen?` (each sub-task's
change its Otter has heard). History: `ToolCalled` carries `detail?` (the
command Otter ran, the sub-task it made, where it moved the task) and its
`origin` may be `auto` (the controller, where the tool is automatic);
`ToolUndo {tool, task_id, action}` says what undoes a change (a
`task.act` action on that task). A task Otter answered itself is `done`
with no runs; Otter's own instructions to the coding agent are `Controller`
messages starting "To the coding agent: ", their `correlation_id`
`tell:…`; a sub-task's change is said in its parent as a `Controller`
message whose `correlation_id` is `child:<task>:<change>`.

**What CI was, and when** (D-184; optional fields): `delivery.mergeable?`
is `MERGEABLE` or `CONFLICTING` as last read (absent: unknown) — a pull
request that conflicts gets no `pull_request` CI, and its `Pull request`
and `CI` gates say so (`failed`); `delivery.ci_read_at?` is when what
`ci` and `mergeable` say was read.

**Commands are idempotent.** `command_id` is the client's id for one intent
(a random string). A command the daemon has applied is not applied again: a
retry returns the task as it is. A command that failed was not applied,
so its id may be reused.

**Following a task:** every change emits `TaskChanged {task_id,
history_seq, status}` on the host's event log, so `events.subscribe` with a
cursor (above) tells a reconnecting client which tasks changed while it
was away; it then reads `task.events {after: <last seq it saw>}`. The
history is the task's own: it isn't rotated with the host's log.

**Text being written** (D-051): `TaskStream {task_id, stream_id, role,
text, done}` carries a message's text so far while the coding agent or the
Control Agent writes it, at most every 100 ms. It is **transient**: sent to
live `events.subscribe` streams only, never logged or replayed, stamped with
the latest logged `seq` (so it doesn't advance a cursor and may repeat one),
and outside the `seq` order. A client may drop any of them; with `done` the
message is complete. The coding agent's text along the way isn't stored in
the task: only each turn's result is (D-069); the rest is in its
conversation. Otter (role `controller`) begins with an empty `text` as
soon as it starts on a message — a reply, its look at a request for a
workspace, a watch's report — before its first word (D-196). An empty
`text` with `done` means it ended with nothing to say there (the message,
if any, is in the task); a client drops that draft. After a stream is
picked up again (a reconnect), what was being written may never get its
`done`: a client forgets those drafts and takes what comes next.

**What the agent did** (D-069): `TaskActivity {task_id, run_id, line}`
— one line per tool the coding agent used (`$ cargo test`, `✎ src/a.rs`).
Transient in the same way. While a run works, `task.get`/`task.list`
show its latest lines in `runs[].activity` (kept in memory, not written to
the task for each tool); when the run ends they are stored with it. A
client adds each line to the run's `activity` (keeping the last 30) rather
than reloading the task.

**When, and what next** (D-141; optional fields, old peers ignore them):
`TaskActivity` carries `at?`, when the line was noted, and a run's
`activity_at[]` holds each line's time — as many as `activity`, or none
(a daemon before it: a client then shows no times rather than wrong
ones). A live run as `task.get`/`task.list`/`task.send`/`task.act`/
`task.create` show it has `heard_at?` (when its agent last gave any sign of
work: a word, a tool, a background command — what the no-progress
watchdog reads, D-063) and `tools_running` (absent: 0); neither is ever
stored. These calls also give the task's `next?`: what the engine does
after what it does now (`Task::what_next`), absent when the task waits on
the developer or isn't being worked. The history has `Reasoned {why}`:
each of Otter's reasons (`rationale`), at its time; its `text` is the
reason's first line.

An addition (no protocol bump): daemons before it answer `invalid_request`.

## Otter on the host

Otter's conversation on a host (D-093): the developer and Otter about any
task there, apart from each task's own conversation. Answered as a turn with
Otter's tools (the host's: looking across tasks, making, starting, resuming
and pausing them) when *Otter's tools* are on; otherwise Otter says how to
turn them on.

- `otter.get` → `OtterChat {messages, history_seq}` (the newest 200
  messages; the same `Message` as a task's).
- `otter.send {command_id, text, images?}` → the same. The same `command_id`
  again adds nothing. Otter's reply is a `Controller` message whose
  `correlation_id` is the developer's message's id. `images` (D-202) are
  pictures as `task.send` takes them (`TaskImage`, the same limits); `text`
  may then be empty. They are kept with the conversation
  (`state/otter/artifacts/`, named on the message's `attachments`) and shown
  to Otter's turn. An older otterd ignores them.
- `otter.events {after?, limit?}` → `TaskEventRecord[]`: the history of
  Otter's tool calls there (`ToolCalled`, `ToolDone`).
- `otter.actions {limit?}` → `OtterAction[]` (D-193): what Otter did across
  the host's tasks, newest first (default 100, at most 500): `{task_id,
  task_title, seq, at, tool, origin, authority?, detail?, error?, undo?:
  {task_id, action}}` — each `ToolCalled` in the tasks' histories, with
  what came of it and what undoes it. Refusals that changed nothing aren't
  listed. An older daemon answers `invalid_request`.
- Events: `OtterChanged {history_seq}`; `OtterStream {stream_id, text,
  done}` — the reply as it is written, transient like `TaskStream`.
- A task made from it is an ordinary task; its request is the
  developer's message(s) as written.

An addition (no protocol bump): daemons before it answer `invalid_request`;
`otter chat` says the host's otterd is too old.

Otter's memory (D-113): what Otter noted from what the developer decided.
There is one memory for each conversation and one for the host, across
every conversation there. Every turn is given both. Each is Markdown, at
most 8 000 characters, redacted.

- `memory.get {task?}` → `OtterMemory {host, conversation, max_chars}`.
  `conversation` is the task's memory, or Otter's conversation on the host
  when no task is given.
- `memory.set {task?, memory: "host" | "conversation", text, previous?}` →
  the same. `text` replaces all of that memory. `previous`, what it said when
  the edit began, makes the call a `conflict` if Otter has changed it since.
  An unknown task is `not_found`, and too much text is `invalid_argument`.
- Neither memory is in an event. A client reads them when it shows them.
- Also an addition: older daemons answer `invalid_request`, and the app says
  the host's otterd is too old.

## Triggers

What starts unattended work on a host (D-083), kept in
`state/triggers.json`:

- `trigger.create {name, workspace, title?, request, schedule, policy}` →
  the trigger. `schedule` is `{kind: "every", minutes}`, `{kind: "daily",
  at: "HH:MM"}` (UTC) or `{kind: "manual"}`; `policy` is `{publish,
  max_iterations?, max_minutes?, wait_limit_secs?}` — what its tasks may
  do without asking, set in advance.
- `trigger.list` → the triggers, each with `next_at` and its `history` (the
  last occurrences: the task each started, or why it was skipped).
- `trigger.delete {trigger}` → `null` (its tasks stay).
- `trigger.run {trigger, occurrence?}` → the trigger, fired now — for
  `occurrence` (RFC 3339) when given: one task per occurrence, so a
  script that retries with the same one starts no second task.
- `trigger.update {trigger, enabled?}` → the trigger (D-185). `enabled:
  false` pauses it: it doesn't fire on its schedule (`trigger.run` still
  does); `true` resumes it from now — what it missed meanwhile isn't
  fired. An older otterd answers `invalid_request`.

Every change — made, paused or resumed, removed, fired — emits
`TriggersChanged {}`; a client reads `trigger.list` again (D-185). A
triggered task carries `started_by {trigger_id, trigger, occurrence,
policy}`. Additions (no protocol bump).

## Files and image paste

- `fs.list {workspace, path}` → a directory (`path` relative to the
  workspace root, or absolute): entries with kind, size and modification time,
  directories first.
- `fs.read {workspace, path, offset, len}` → `{data (base64), size, eof}`, at
  most 1 MiB per call; `fs.write {workspace, path, offset, data, create,
  overwrite}` writes a chunk (`create` starts the file and fails if it exists
  unless `overwrite`).
- `fs.remove {workspace, path}` removes a file, a link, or a directory with
  everything in it; `fs.rename {workspace, path, name}` gives one another
  name in the same directory, never over an existing one (`already_exists`)
  (D-103). Both act only inside the workspace — never on its root, nor on
  anything a path resolves to outside it — and on a link itself, not what it
  points to. Additions: daemons before them answer `invalid_request`.
- `session.paste_image {workspace, session, png}` → stores the PNG (≤ 20 MB)
  for that session; the session's stand-in `wl-paste` (on its PATH ahead of
  the host's own, even after direnv reloads in the session, D-112) serves it
  for two minutes, so Claude Code's Ctrl+V picks it up.

## Browser login

- `browser.open {url, session?}` — from the host's browser stand-ins
  (`otterd open-url`, behind `xdg-open`, `www-browser` and `BROWSER=otter-open`
  in sessions). Only https pages, and only while a client (the app, or
  `otter attach`) is subscribed with `events.subscribe {browser: true}`;
  otherwise an error says why and the tool prints the URL itself.
- A page from a sign-in provider built in (`otter_protocol::sign_in::BUILT_IN`)
  emits `BrowserOpenRequested {request_id, provider, callback_port?,
  workspace_id?}`. A page from any other site emits `BrowserOpenAsked
  {request_id, site, callback_port?, workspace_id?}` (D-121), and the tool
  still hears an error, so it prints the URL as it would. `site` is the
  page's host, never a pattern; a client ignores one that isn't. The Mac
  opens such a page if its developer allowed the site there, or asks. An app
  from before D-121 ignores the event. The URL is in neither event (nor the
  log).
- Before opening any page, a client checks it again against the built-in
  providers and the sites allowed on its Mac. The host's word isn't enough.
- `browser.take {request_id}` → `{url, provider, callback_port?}`, once, for
  two minutes; with several browser clients, the first to take it opens it.
  The client opens the URL on the Mac and maps its callback port (a
  loopback `redirect_uri` port, which a D-121 client reads from the URL
  itself) to this Mac until the host stops listening on it or ten minutes
  pass.

## Attach mode

After a successful `session.attach` response
(`{"session_id","execution_id","history"?}`), both directions switch to binary frames:

```text
+---------+--------------------+------------------+
| kind u8 | length u32 (BE)    | payload (length) |
+---------+--------------------+------------------+
```

| kind | name   | direction        | payload                                         |
|------|--------|------------------|-------------------------------------------------|
| 1    | data   | both             | raw terminal bytes (client→daemon: input; daemon→client: output) |
| 2    | resize | client → daemon  | `cols u16 BE`, `rows u16 BE` (exactly 4 bytes)  |
| 3    | detach | client → daemon  | empty                                           |
| 4    | exit   | daemon → client  | JSON `{"reason", "exit_code"?, "message"?}`     |
| 5    | control | both, once both said they take it | JSON `{"type", …}` (below)    |

- Maximum payload: 4 MiB (`MAX_FRAME_LEN`); larger frames are a protocol error.
- `session.attach` params carry the initial `cols`, `rows` and the client's
  `TERM`; the attach starts at that size and shows the session's current screen.
  An optional `history` (lines) asks for a **stream attach**, below.
- Data is bytes, not text: UTF-8 sequences may be split across frames.
- **Detach**: the client sends `detach`; the daemon ends the attach and sends
  `exit {"reason":"detached"}`. The session keeps running.
- **Exit**: `exit` is always the daemon's last frame. Reasons: `detached`,
  `exited` (the process ended; `exit_code` if known), `ended` (stopped, deleted
  or lost), `error` (`message` says why). The daemon then closes the
  connection.
- **Connection close**: if the client disappears without detaching (crash,
  network loss), the daemon tears down its side of the attach; the session is
  unaffected. If the daemon goes away, the client sees the connection close
  without an `exit` frame; the session is unaffected and can be attached again
  once a daemon is back. An attach is disposable; the execution is durable.
- **Dead connections**: a client must not wait forever. `otter attach` gives
  up 5 s after sending `detach` without an `exit`, and SSH transports use
  keepalives (`ServerAliveInterval=15`, `ServerAliveCountMax=3`) so a dead
  network is noticed within about a minute even when idle.
- **Initial screen**: attaching redraws the session's current screen (tmux
  attach semantics); scrollback stays in the session (`session.read`).
- **Stream attach** (D-118): with `history`, a daemon that can answers
  `"history": true`, and the client's terminal becomes the session's terminal:
  - Output starts with a snapshot that rebuilds the terminal: the screen at
    once, then (shown together, in synchronized output) up to `history` lines
    of history, the normal and alternate screens, cursor and modes. After it
    comes the program's own output. A client that falls far behind gets a
    fresh snapshot instead of the backlog; each starts with CAN and RIS.
  - The client keeps the history, so it scrolls it without asking the host.
    The daemon may send fewer lines than asked (it caps the bytes).
  - The session's terminal has already answered the program's queries
    (cursor position, device attributes, colors…); the client must not
    answer them, or the answers arrive as typing.
  - Every snapshot turns on bracketed paste. The client sends pastes
    bracketed and the daemon delivers them bracketed only if the program asked.
  - Without `"history": true` in the response (an older daemon), it's the
    screen attach above. The CLI uses that unless `--scrollback`.
  - With `"own_terminal": true` (D-120), the client's terminal is the user's
    own (`otter attach --scrollback`). The daemon keeps the program's queries
    out of the output, never resets that terminal (no RIS), and draws no
    screen ahead of the history. When it catches the client up, it undoes
    the program's modes instead and sends no history again.
- **Control frames** (D-119): a stream client that sends `"controls": true`
  on `session.attach` and gets `"controls": true` back may send and must
  take kind-5 frames. Neither side sends one otherwise. A `type` it doesn't
  know is ignored.
  - `{"type":"view","visible",cols,rows}` (client → daemon): whether the
    client's terminal is on screen. Off screen, the daemon sends it no output.
    Back on screen, at that size, it gets a snapshot if it missed any. On
    screen (as it is until it says otherwise) is the developer looking at
    the session: coming on screen settles what attaching does, and while
    it's on screen a finished turn or a completed command raises nothing
    (D-137). A client says it again while on screen, about every 30 s; one
    not heard from for 90 s no longer counts as on screen. Said again, it
    changes nothing else.
  - `{"type":"history","lines"}` (client → daemon): rebuild the client's
    terminal with up to that many lines of history.
  - `{"type":"window","size":[cols,rows]|null}` (daemon → client): another
    client sizes the session's window, so output is laid out for that size.
    It comes before a snapshot taken at it. `null` means the size is the
    client's own again.
  - `{"type":"snapshot","lines","more"}` (daemon → client): a snapshot's
    bytes just ended. `lines` is how much history it brought, and `more`
    says the session holds earlier lines.
- **Malformed frames** (unknown kind, bad resize length, oversize) end the
  attach as if the client had disconnected.

## Compatibility

`PROTOCOL_VERSION` is bumped only for incompatible changes. From v2 on:

Compatible — no bump; old peers must keep working:

- a new optional request parameter or response field (unknown fields are
  ignored);
- a new frame kind sent only once both peers said they take it (as
  `controls` does for kind 5);
- a new RPC method (old daemons answer `invalid_request`);
- a new event kind (old clients see it as `Unknown` and ignore it);
- a new error code (old clients see `Unknown`).

Incompatible — bump:

- changing the frame format or adding a frame kind;
- changing the meaning of an existing method, parameter, field or event;
- removing or renaming anything, or making an optional thing required;
- changing cursor semantics.

Clients must tolerate the compatible additions above: never fail on unknown
fields, event kinds or error codes.

### v3 (D-107)

What was a feature is a task, on the wire too; the client and daemon must
match (a v2 peer is told the versions differ, and the app offers to update
the host). Renamed:

| v2 | v3 |
|---|---|
| `feature.list` `.get` `.create` `.send` `.act` `.events` `.artifact` `.delete` | `task.*`, the same |
| params `feature` (and `conversation.list {feature}`) | `task` |
| `FeatureChanged` `FeatureDeleted` `FeatureStream` `FeatureActivity`, their `feature_id` | `TaskChanged` `TaskDeleted` `TaskStream` `TaskActivity`, `task_id` |
| `ConversationChanged.feature_id`, `Workspace.feature_id`, a conversation's `feature_id` | `task_id` |
| a plan's `tasks`, `TaskId` | `steps`, `StepId` (`Run.step_id`, a decision's and evidence's `step_id`) |
| history `FeatureCreated`, `TaskChanged` (a step) | `TaskCreated`, `StepChanged` |
| session kind `task` | `command` |

Ids keep their prefixes (`ft_` a task, `tk_` a step). What a v2 daemon
stored still reads: `state/features/` moves to `state/tasks/` (each
`feature.json` to `task.json`) once, at start, and the old names in
`state.json`, `events.jsonl`, histories, conversations and triggers read
through aliases.
