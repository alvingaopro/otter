# Otter protocol (v2)

The contract between `otterd` and its clients (the `otter` CLI and the
desktop app). Types live in `crates/protocol`; this document is the prose. Decisions:
D-004 (shape), D-016 (cursors, snapshot, compatibility), D-037 (bounded
log, log id).

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
  take a `Brief`: `{title?, goal?, description?, constraints?, references?,
  decisions?}` (strings and string lists, all optional).
- `workspace.set_brief` → the workspace. It **replaces** the whole brief
  (fields left out are cleared): clients edit by read-modify-write of the
  brief they were shown. The daemon trims text, drops blank text and blank
  list items. Allowed in any state, including `archived`.
- A change emits `WorkspaceBriefChanged {workspace_id}`; setting the same
  brief again emits nothing. The event never carries the brief's text: read
  the workspace.

An addition (no protocol bump): daemons before it answer `invalid_request`.

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
  (`legacy_cli` or `sdk`, D-057) and its Claude model, apart from Otter's
  own; `settings.set {coding: {backend?, model?}}` changes them (`""`
  resets). Absent from older daemons.
- `settings.models {controller}` → `[{id, name?}]`: the models that
  controller offers, as its provider lists them, asked by the host with the
  key set there (OpenRouter's list needs none); `claude` answers Claude
  Code's model names. No key, or the provider unreachable: `conflict`;
  a controller without models: `invalid_argument`.

An addition (no protocol bump): daemons before it answer `invalid_request`.

## Runtime conversations

The coding agent's side of a feature's work (D-055): conversations, their
turns, messages, tool calls and interactions. See
`otter_core::conversation`.

- `runtime.capabilities` → `{provider, backend, available, notes?,
  tested_version?, structured_ready, features: {send_turn, resume,
  interrupt_turn, permission_requests, questions, tool_results, streaming,
  attachments, usage}}`: what the host's managed runtime can do. Missing
  features are reported, never simulated.
- `conversation.list {feature?}` → conversations, newest first;
  `conversation.get {conversation}` → one. Each is the conversation with
  `resumable` in place of the provider's own session id, which stays on
  the host.

- Event `ConversationChanged {conversation_id, revision, feature_id?}`
  after each change: ids only; read the content with `conversation.get`.

Read-only: coding work is sent by Otter, not by clients. Older daemons
answer `invalid_request` (or `unsupported`).

## Features

Product work owned by the daemon (D-043); see `otter_core::feature`.

- `feature.list` → every feature, newest first; `feature.get {feature}` → one.
- `feature.create {command_id, title, request?, workspace?}` → a `draft`
  feature; the request becomes the first message.
- `feature.send {command_id, feature, text}` → the feature, with the
  developer's message added.
- `feature.act {command_id, feature, action, …}` → the feature. `action`:
  `start`, `pause`, `resume`, `cancel`, `retry`, `accept {override_gates?}`,
  `request_changes {note?}` (the goal changed: plan again from where the
  work stands — any state but cancelled, D-050), `decide {decision_id,
  approve, answer?}`, `set_workspace {workspace}`, `preview`. An
  action the lifecycle doesn't allow fails with `conflict`, and so does
  `accept` while a gate (D-047) is open, unless `override_gates`.
- `feature.delete {feature}` → `null`: removes a feature with its history
  and artifacts; the workspace it used stays. A feature being worked on
  (planning, implementing, verifying, or with a run going) fails with
  `conflict`: pause or cancel it first. Emits `FeatureDeleted {feature_id}`.
- `feature.artifact {feature, name}` → a file the feature's checks produced
  (a screenshot), named as evidence refers to it (`artifact:<name>`):
  `{data (base64), size, eof: true}`, at most 8 MiB.
- `feature.events {feature, after?, limit?}` → the feature's own history,
  `seq > after`, oldest first: `{seq, ts, v, correlation_id?, text, type,
  …}`. `seq` starts at 1 per feature and never repeats; `v` is the record's
  schema version.

**Commands are idempotent.** `command_id` is the client's id for one intent
(a random string). A command the daemon has applied is not applied again: a
retry returns the feature as it is. A command that failed was not applied,
so its id may be reused.

**Following a feature:** every change emits `FeatureChanged {feature_id,
history_seq, status}` on the host's event log, so `events.subscribe` with a
cursor (above) tells a reconnecting client which features changed while it
was away; it then reads `feature.events {after: <last seq it saw>}`. The
history is the feature's own: it isn't rotated with the host's log.

**Text being written** (D-051): `FeatureStream {feature_id, stream_id, role,
text, done}` carries a message's text so far while the coding agent or the
Control Agent writes it, at most every 100 ms. It is **transient**: sent to
live `events.subscribe` streams only, never logged or replayed, stamped with
the latest logged `seq` (so it doesn't advance a cursor and may repeat one),
and outside the `seq` order. A client may drop any of them; with `done` the
message is complete and stored in the feature.

An addition (no protocol bump): daemons before it answer `invalid_request`.

## Files and image paste

- `fs.list {workspace, path}` → a directory (`path` relative to the
  workspace root, or absolute): entries with kind, size and modification time,
  directories first.
- `fs.read {workspace, path, offset, len}` → `{data (base64), size, eof}`, at
  most 1 MiB per call; `fs.write {workspace, path, offset, data, create,
  overwrite}` writes a chunk (`create` starts the file and fails if it exists
  unless `overwrite`).
- `session.paste_image {workspace, session, png}` → stores the PNG (≤ 20 MB)
  for that session; the session's stand-in `wl-paste` (first on its PATH)
  serves it for two minutes, so Claude Code's Ctrl+V picks it up.

## Browser login

- `browser.open {url, session?}` — from the host's browser stand-ins
  (`otterd open-url`, behind `xdg-open`, `www-browser` and `BROWSER=otter-open`
  in sessions). Accepted only for https pages of trusted sign-in providers,
  and only while a client (the app, or `otter attach`) is subscribed with
  `events.subscribe {browser: true}`; otherwise an error says why and the
  tool prints the URL itself.
- Accepting it emits `BrowserOpenRequested {request_id, provider,
  callback_port?, workspace_id?}` — the URL is not in the event (or the log).
- `browser.take {request_id}` → `{url, provider, callback_port?}`, once, for
  two minutes; with several browser clients, the first to take it opens it.
  The client opens the URL on the Mac and maps `callback_port` (a
  loopback `redirect_uri` port) to this Mac until the host stops listening on
  it or ten minutes pass.

## Attach mode

After a successful `session.attach` response
(`{"session_id","execution_id"}`), both directions switch to binary frames:

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

- Maximum payload: 4 MiB (`MAX_FRAME_LEN`); larger frames are a protocol error.
- `session.attach` params carry the initial `cols`, `rows` and the client's
  `TERM`; the attach starts at that size and shows the session's current screen.
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
- **Malformed frames** (unknown kind, bad resize length, oversize) end the
  attach as if the client had disconnected.

## Compatibility

`PROTOCOL_VERSION` is bumped only for incompatible changes. From v2 on:

Compatible — no bump; old peers must keep working:

- a new optional request parameter or response field (unknown fields are
  ignored);
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
