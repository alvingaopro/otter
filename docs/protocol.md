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
  the branch has commits not pushed anywhere; `force` discards both.

## Names

- `workspace.rename {workspace, name}` → the workspace; `session.rename
  {workspace, session, name}` → the session (D-101). Names follow the same
  rules as at creation (no `/` or `:`, trimmed), unique among the host's
  workspaces / the workspace's sessions (`already_exists`). Only the name
  changes: ids, the directory, the branch and running processes stay, so an
  attach or a reference by id carries on.
- They emit `WorkspaceRenamed {workspace_id, name}` and `SessionRenamed
  {workspace_id, session_id, name}`.

## A workspace's work

- `workspace.changes {workspace}` → `{git, branch?, base?, ahead, behind,
  files, more, insertions, deletions}` (D-102). In a Git work tree: the
  changes since the base — the workspace's base branch, else the branch's
  upstream; with neither, what isn't committed — committed or not, new files
  included, each `{path, change, insertions?, deletions?, uncommitted}`
  (`change`: `added`, `modified`, `deleted`, `renamed`, `untracked`; no
  counts for a binary file). Paths are relative to the workspace root and
  limited to it. At most 500 files; `more` counts the rest. Outside Git only
  `git: false`. Read-only: the index is never touched.
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
  can do. Missing tasks are reported, never simulated. For `sdk`,
  `available` needs Node, the installed worker and its `--check` (D-060);
  `notes` say which is missing. `worker_missing?` says the worker this
  otterd looks for isn't installed, which installing it would fix (D-129).
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
  task; the request becomes the first message.
- `task.send {command_id, task, text}` → the task, with the
  developer's message added.
- `task.act {command_id, task, action, …}` → the task. `action`:
  `start`, `pause`, `resume`, `cancel`, `retry`, `accept {override_gates?}`,
  `request_changes {note?}` (the goal changed: plan again from where the
  work stands — any state but cancelled, D-050), `decide {decision_id,
  approve, answer?, answers?}` (`answers`: per question, for a form),
  `set_workspace {workspace}`, `preview`, `interrupt` (stop the coding
  agent's current turn and pause, D-059), `redirect {text}` (the current
  turn stops and this goes next; refused when nothing is running). An
  action the lifecycle doesn't allow fails with `conflict`, and so does
  `accept` while a gate (D-047) is open, unless `override_gates`.
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
conversation.

**What the agent did** (D-069): `TaskActivity {task_id, run_id, line}`
— one line per tool the coding agent used (`$ cargo test`, `✎ src/a.rs`).
Transient in the same way. While a run works, `task.get`/`task.list`
show its latest lines in `runs[].activity` (kept in memory, not written to
the task for each tool); when the run ends they are stored with it. A
client adds each line to the run's `activity` (keeping the last 30) rather
than reloading the task.

An addition (no protocol bump): daemons before it answer `invalid_request`.

## Otter on the host

Otter's conversation on a host (D-093): the developer and Otter about any
task there, apart from each task's own conversation. Answered as a turn with
Otter's tools (the host's: looking across tasks, making, starting, resuming
and pausing them) when *Otter's tools* are on; otherwise Otter says how to
turn them on.

- `otter.get` → `OtterChat {messages, history_seq}` (the newest 200
  messages; the same `Message` as a task's).
- `otter.send {command_id, text}` → the same. The same `command_id` again
  adds nothing. Otter's reply is a `Controller` message whose
  `correlation_id` is the developer's message's id.
- `otter.events {after?, limit?}` → `TaskEventRecord[]`: the history of
  Otter's tool calls there (`ToolCalled`, `ToolDone`).
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

A triggered task carries `started_by {trigger_id, trigger, occurrence,
policy}`. An addition (no protocol bump).

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
    (D-137).
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
