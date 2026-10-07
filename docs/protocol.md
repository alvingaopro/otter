# Workd protocol (v2)

The contract between `workd` and its clients (`workctl`, later the desktop
app). Types live in `crates/protocol`; this document is the prose. Decisions:
D-004 (shape), D-016 (cursors, snapshot, compatibility).

## Connection

A connection is a byte stream to one host's daemon: the Unix socket
`~/.workd/run/workd.sock`, reached through `workd dial` (locally, or as
`ssh host workd dial`). `workd dial` starts the daemon if it isn't running.

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

`workd` is not event-sourced: current state comes from a snapshot, events say
what changed after it.

### `state.snapshot`

```json
{"seq": 103, "workspaces": [ … ]}
```

`workspaces` include their sessions, executions and attention. (`host.status`
is separate: it probes the host's tools and is slower.)

`seq` is the event cursor the snapshot corresponds to. The snapshot reflects
**every event up to `seq`, and possibly some later ones** — so clients apply
events idempotently (simplest: treat an event as "refetch what it names").

### Events

```json
{"type":"event","event":{"seq":104,"ts":"…","type":"AttentionCreated","workspace_id":"ws_…", …}}
```

`seq` is per host, strictly increasing and survives daemon restarts. It is not
guaranteed contiguous (a log line the daemon cannot parse is skipped), so
clients compare cursors with `>`, never count them.

### `events.subscribe`

Params: `{"after": 103}` (optional). Result: `{"seq": 110}` — the latest event
when the subscription started. Then the connection carries `event` messages:

- with `after`: every logged event with `seq > after`, in order, then live
  events — each exactly once, with no gap between replay and live;
- without `after`: live events with `seq > result.seq`.

If the daemon falls behind its internal buffer it catches up from the log
rather than dropping events. Anything the client sends after subscribing is
ignored; closing the connection ends the subscription.

**Cursor errors.** If `after` cannot be served, the request fails with
`cursor_expired` and the connection stays in RPC mode:

- it is older than the oldest retained event (the log was rotated — not done
  yet, but clients must handle it), or
- it is newer than the latest event (the host's log was reset).

The client then reloads a snapshot and subscribes after its `seq`. Events are
never silently skipped.

Known gap: a log that was reset *and* has since grown past the client's old
cursor is not detected (no log identity yet; D-016).

### Client recipes

```text
fresh client:   snapshot ─► subscribe(after = snapshot.seq) ─► apply events
reconnecting:   subscribe(after = last seen seq)
                  └─ cursor_expired ─► fresh client
```

`workctl events --follow` is the reference implementation (it resumes from the
last event it printed whenever its connection drops).

`events.list {limit}` returns the most recent `limit` events (oldest first) for
display; it is not a sync mechanism.

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
  unaffected. An attach is disposable; the execution is durable.
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
