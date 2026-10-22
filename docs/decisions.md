# Otter — Implementation Decisions

Decisions made while implementing [`design.md`](design.md), including where the
implementation deliberately deviates from it. Newest last. Each entry says what
was decided, why, and what would make us revisit it.

---

## D-001 — Rust, one Cargo workspace (2026-10-07)

**Decision.** `otterd` (daemon) and `otter` (CLI) are Rust, in one Cargo
workspace: `crates/{core,protocol,client,daemon,cli}`, with the Tauri desktop
app to come in `apps/desktop`.

**Why.** The daemon sits on the OS boundary (PTYs, signals, Unix sockets,
process lifecycle) and is expected to grow downward toward systems code. The
Tauri app's core is Rust too, so domain and protocol types are shared verbatim.
Single static-ish binaries keep `scp otterd host:~/.local/bin/` as the install
story. (User decision; see design §32.)

## D-002 — CLI-first dogfooding before any GUI (2026-10-07)

**Decision.** Every phase lands in `otter` first; the desktop UI is the last
phase and is gated on the CLI feeling excellent. (User decision; design §33.)

## D-003 — Daemon listens on a Unix socket, reached via `ssh host otterd dial` (2026-10-07)

**Deviation from design §6**, which proposed `localhost:<port>` plus an SSH
port-forward.

**Decision.** `otterd` listens on `~/.otter/run/workd.sock` (mode `0600`, in a
`0700` directory) and never on TCP. Clients run `ssh <host> ~/.local/bin/otterd
dial`; `dial` bridges the SSH channel's stdin/stdout to the socket and starts
the daemon first if it isn't running. `otter` passes
`-o ControlMaster=auto -o ControlPersist=600` (sockets in
`~/.config/otter/cm/`; `cz/` since D-119 compresses them) so repeated
connections reuse one SSH connection.

**Why.**
- A TCP port on `localhost` is reachable by *every* local user on the host;
  anyone connecting could create sessions, i.e. run commands as you. A Unix
  socket is protected by file permissions.
- No port allocation or tunnel lifecycle to manage, no stale forwards, and it
  works with sshd configs that disable TCP forwarding.
- Auto-start in `dial` means "if `ssh host` works, otterd works" — no service
  manager setup needed.

**Still true:** SSH is the only transport (rule 4); the daemon has no network
exposure.

**Revisit if** a client needs many concurrent streams over one channel (then:
multiplex streams inside one connection rather than one SSH channel each).

## D-004 — Protocol v1: JSON lines + binary attach frames (2026-10-07)

*Open decision in design §35; current choice.*

- Newline-delimited JSON. Server sends `hello {protocol, version}` first; client
  sends `{id, method, params}`; server replies `{type: "response", id, result |
  error}`. Methods are namespaced (`workspace.create`, `session.attach`, …).
- `session.attach` upgrades the connection to binary frames
  (`[kind u8][len u32 BE][payload]`: data, resize, detach, exit). One attach per
  connection.
- `events.subscribe` turns the connection into a stream of `event` messages.
- `PROTOCOL_VERSION` is checked by the client; bump on incompatible change.

Types live in `crates/protocol` and are shared by every client.

## D-005 — Persistence: `state.json` + `events.jsonl` (2026-10-07)

*Open decision in design §35; current choice.*

All durable daemon state is one JSON document, `~/.otter/state/state.json`,
rewritten atomically (temp file + fsync + rename) on every change. Events are
appended to `~/.otter/state/events.jsonl`. Small, transparent, trivially
inspectable; fine for tens of workspaces.

Events carry ids, names, kinds and exit codes only — never commands or
environment values (design §19). Enforced by a test.

**Revisit if** state grows large or needs concurrent writers (→ SQLite), or the
event log needs rotation. *(The event log is rotated since D-037.)*

## D-006 — Launch environment: captured login env, delivered via private file (2026-10-07)

Design §17 says not to rely on interactive shell hooks and §19 says "if it works
after `ssh host`, it should work in Otter".

**Decision.**
- At startup the daemon runs `$SHELL -l -c 'exec otterd internal-dump-env'` once
  and uses that environment (minus per-terminal variables like `TERM`, `PWD`,
  `SSH_TTY`, `TMUX`) for every launched process. This is what puts
  `~/.local/bin` (e.g. `codex`) on `PATH` even though the daemon itself was
  started by a non-interactive SSH command. Fallback: the daemon's own env.
  Interactive (`-i`) capture was rejected: it runs prompt frameworks
  (powerlevel10k/gitstatus) that print noise and misbehave without a TTY.
- Terminal sessions without a command run `$SHELL -l`; commands run as
  `/bin/sh -c '<command>'` (see D-011 for why not `$SHELL`).
- The environment is handed to the process through a `0600` file read and
  deleted by `otterd internal-exec <file> -- <argv>`, which then `exec`s the
  command with exactly that environment. **Never on a command line**: the tmux
  server keeps the argv of the client that started it, and argv is visible to
  all users via `ps`. (Found in testing: API keys from the login env showed up
  in the tmux server's `/proc/<pid>/cmdline`.) Enforced by a test.
- Every process also gets `OTTER_WORKSPACE_ID/NAME`, `OTTER_SESSION_ID/NAME`,
  `OTTER_EXECUTION_ID`.

## D-007 — tmux backend details (2026-10-07)

- Private tmux server per host (`~/.otter/run/tmux.sock`, own generated
  config); the user's own tmux is never touched. Requires tmux ≥ 3.2.
- One tmux session per **execution**, named after the execution id. Restarting
  a session creates a new execution and a new tmux session (rule 7).
- tmux is made invisible: no status bar, no prefix key, `remain-on-exit on`
  (exit status and final output stay readable), `window-size latest`.
  tmux's own client notices (`[detached (from session exec_…)]`) are filtered
  out of attach output.
- Attach = the daemon runs `tmux attach` inside a PTY it owns and bridges it to
  the client; detach uses `tmux detach-client` so the user's terminal is
  restored. Detach key in `otter` is `Ctrl-]`.
- Exits are observed by polling `list-panes` every 500 ms. The same code
  reconciles state at startup: alive → adopt, dead → record exit, missing →
  `lost`.
- **tmux quirks handled** (found in testing, tmux 3.4):
  - When a process exits right after starting while other tmux clients are
    connecting, tmux sometimes marks the pane dead but never reaps the process,
    so it never learns the exit status. We read the zombie's status from
    `/proc/<pid>/stat` (Linux), and otherwise wait up to 2 s for tmux before
    recording an unknown status.
  - Without a UTF-8 locale (typical for daemons started over plain SSH) tmux
    rewrites tabs in command output as `_`. All tmux commands run with `-u`, and
    `list-panes` output uses `|` separators.

**Revisit** polling if many hosts × sessions make it costly (tmux hooks →
`otterd` notification).

## D-008 — Daemon lifecycle (2026-10-07)

- Single instance per `OTTER_HOME` via `flock` on `run/workd.lock`.
- `otterd dial` auto-starts `otterd serve` fully detached (`setsid`, stdio to
  `logs/workd.log`) so it outlives the SSH connection.
- `daemon.shutdown` (`otter host shutdown`) stops the daemon but **not**
  managed processes; the next connection starts a fresh daemon, which
  reconciles. This is also the upgrade path: replace the binary, shut down.
- `OTTER_HOME` / `--home` relocate everything (used by tests and for running
  several daemons on one machine). Socket paths must stay under ~100 bytes.

## D-009 — Sessions and addressing (2026-10-07)

- Kinds: `terminal` (interactive; login shell or any command — e.g. `codex`),
  `service` (long-running, command required), `task` (runs to completion,
  command required), `agent` (a coding agent; see D-013).
- Session status derives from its current execution: `running`, `completed`
  (exit 0), `failed` (non-zero, or failed to start), `stopped`, `lost`.
- Workspace names are unique per host; ids (`ws_…`, `ses_…`, `exec_…`) are
  random and unique across hosts. The CLI addresses
  `[host:]workspace[/session]` and searches all hosts concurrently; the host
  prefix is only needed when a name exists on several hosts.
- Deleting a workspace stops its sessions and removes its directory only if it
  is the managed `~/.otter/workspaces/<id>` (rule: cleanup only touches managed
  resources).

## D-010 — Git: bare backing repository, one worktree per workspace (2026-10-07)

- First use of a repository on a host: `git clone --bare` into
  `~/.otter/repos/<repo-id>/base`, with `remote.origin.fetch` set so branches
  appear as `origin/*` (a bare clone doesn't do that by default). Bare because
  nobody should work in the backing checkout. *(Since D-132: `git init
  --bare` and a fetch, so the remote's branches aren't copied into it.)*
- `<repo-id>` = last path component + a stable hash of the normalized URL
  (`suger-api-1a2b3c4d`): readable, collision-free across orgs.
- Every Git workspace: `git fetch --prune`, then `git worktree add` at
  `~/.otter/workspaces/<id>/repo`.
  - `--branch B` that exists (locally or as `origin/B`) is checked out;
    otherwise a new branch is created — `B`, or `otterd/<name-slug>` — from
    `--base` (default: origin's default branch, via `remote set-head --auto`).
- Fetch/worktree operations are serialized per repository.
- Credentials: whatever `git` on the host already uses; `GIT_TERMINAL_PROMPT=0`
  so a missing credential fails instead of hanging. Error messages are
  scrubbed of `user:token@` in URLs. *Amended 2026-10-20:* Otter's own
  identity and SSH key, once set, in its repositories only (D-133).
- Deleting a workspace removes the worktree but **keeps the branch** (it may
  hold unpushed work) and never touches the backing repository. It refuses if
  the worktree has uncommitted changes unless forced (`--force`).
- Preparation (clone/fetch/worktree/environment) runs in the background: the
  workspace is `preparing` with a progress message, then `ready` (sessions
  start) or `failed` (sessions stay pending; `otter ws prepare` retries). A
  daemon restart resumes interrupted preparation; deleting cancels it.

## D-011 — Environments: direnv, resolved explicitly (2026-10-07)

- A workspace whose root has an `.envrc` gets the **Direnv** environment:
  `direnv export json` (run in the root, with the login environment) is applied
  on top of the login environment, and every managed process in the workspace
  starts with the result. Verified with `use flake` on a real flake: the Nix
  dev shell's tools and `shellHook` exports reach sessions.
- `direnv allow` is run automatically **only** for worktrees Otter created from
  a repository the user asked for. An existing directory must already be
  allowed (otherwise the workspace fails with direnv's message).
- If the environment fails, the workspace fails and its sessions do not start
  (rather than running agents in the wrong environment).
- Resolved environments are kept in memory only (they can contain secrets) and
  re-resolved after a daemon restart.
- Commands (`service`/`task`/terminal with a command) run via `/bin/sh -c`, not
  the user's shell: a POSIX shell doesn't re-run the user's startup files,
  which could reorder `PATH` and undo the resolved environment. Interactive
  terminals still run `$SHELL -l` — the user's shell startup files apply there,
  as they would after `ssh`.

## D-012 — Existing-directory workspaces (2026-10-07)

Design §15 allowed V1 to skip them; they were cheap, so they exist:
`otter new x --dir /path`. The directory is **attached, not managed**: never
created or deleted by Otter (rule §29).

## D-013 — Codex integration: launch with `--no-daemon`, observe the rollout transcript (2026-10-07)

*Open decision in design §35 (Codex state detection); current choice.*

Investigated for codex-cli 0.159.1: `notify` config (turn-complete only, and a
`-c` override replaces the user's own `notify`), a hooks system
(`SessionStart`, `PermissionRequest`, `Stop`, … — per-launch hooks only run
with `--dangerously-bypass-hook-trust`), the experimental app-server protocol
(has exactly the states we want: `waitingOnApproval`, `waitingOnUserInput`),
and the rollout transcript.

**Decision.** Otter changes nothing about Codex's configuration and uses no
"dangerous" flags:

- Launch: `codex --no-daemon [prompt]`; restart: `codex --no-daemon resume
  <id>`. By default the Codex TUI attaches to a shared, self-updating
  background daemon (observed at a different version than the CLI), so the
  agent's work would live outside the process Otter manages; `--no-daemon`
  keeps execution/lost/restart semantics meaningful. Cost: those sessions
  don't appear in `codex agents` / remote control.
  *Amended 2026-10-08:* the flag is passed **only when the installed Codex
  accepts it**. It appeared in codex-cli 0.156.0 (`--help`: "Run without the
  shared background server, even if it is already running"); 0.154.0 and
  0.155.0 exit 2 with `error: unexpected argument '--no-daemon' found`, so
  Otter could not start Codex at all on such hosts. Those versions don't
  route the TUI through a shared daemon (connecting to an app server is the
  opt-in `--remote`; a 0.154.0 TUI in tmux was the pane's only process, with
  no app-server running), so without the flag the agent still lives in the
  session's process. Detection asks the binary rather than guessing from a
  version: `codex --no-daemon --version` exits 0 only if the flag parses
  (checked against 0.154.0, 0.155.0 → 2; 0.156.0–0.159.1 → 0). The answer is
  cached per resolved binary and modification time, warmed by `detect`; a
  probe that can't run or times out (5 s) isn't cached and leaves the flag
  out.
- The agent binary is the session's process (no shell in between).
- **Identity:** the rollout file `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`;
  its first record (`session_meta`) has the conversation id (stored as the
  session's opaque `provider_session_id`, used to resume) and cwd. Codex creates it lazily (after the first interaction), so it's
  discovered after launch: from the process's open files (Linux `/proc`), else
  by cwd + start time, skipping conversations bound to other sessions.
- **State** from transcript records: `task_started` → working;
  `task_complete` → waiting for input (with `last_agent_message`);
  `turn_aborted` → idle.
- **Needs you (amended by D-035):** Codex offers no signal Otter may use
  (see below), so this stays a heuristic: an agent mid-turn whose rollout
  *and screen* have stood still for `OTTER_AGENT_QUIET_SECS` (default 8 s)
  is `blocked`. Originally this used terminal *output*; calibration showed
  that never fired for Codex (it redraws every second). A freshly started
  agent that stands still is `idle`, or `blocked` if it was given a prompt
  (a startup dialog such as folder trust is up).

**Closed (2026-10-08, D-035):**
1. Hooks via `--dangerously-bypass-hook-trust` — not used: it is a
   "dangerous" flag, and the rollout records nothing during a prompt, so
   there is nothing else structured to read.
2. Calibrated against real Codex — codex-cli **0.154.0** (the version
   installed on the dogfooding Mac; it rejects `--no-daemon`, which Otter
   now leaves out for it — see Launch above). In a tmux pane, `approval_policy` on-request, asked
   to `touch` a file outside the sandbox:
   - rollout: `response_item custom_tool_call` (`status: "completed"`, input
     `tools.exec_command({cmd:"touch …", sandbox_permissions:"require_escalated"…})`)
     then `token_usage_record`, then **nothing** for the 84 s the prompt
     ("Would you like to run the following command? … › 1. Yes, proceed (y)")
     stayed up; declining wrote `custom_tool_call_output` "aborted by user
     after 84.0s" and `event_msg turn_aborted` (`reason: interrupted`).
   - terminal: tmux `window_activity` advanced every second throughout, but
     the captured screen was identical for 13+ s.
   - `sleep 25` (running a command) and a 9 s think with no tool (rollout
     silent from `item_completed` to `reasoning`): the screen changed every
     second (`Working (23s • esc to interrupt)`).
   So "screen and rollout still" separates waiting from thinking/running;
   "no terminal output" does not.
3. Folder trust: for a new directory Codex shows "Do you trust the contents
   of this directory? … › 1. Yes, continue 2. No, quit" before writing any
   rollout, and stores the answer as `[projects."<dir>"] trust_level =
   "trusted"` in `$CODEX_HOME/config.toml`. Otter still doesn't pre-trust
   (that would change the user's configuration and approval defaults); it
   reports the dialog instead (still start + prompt → `blocked`).

Everything parses defensively: the rollout format belongs to a self-updating
binary. The message excerpt is stored in `state.json` only, never in
`events.jsonl`.

## D-014 — Attention: derived from transitions, one open item per session (2026-10-07)

- Raised by: agent finished a turn (`review`, with the message excerpt),
  agent waiting on a permission prompt (`approval`) or a question
  (`question`) — reported by the agent where it can, else inferred from a
  still turn (D-035) —, task succeeded (`completion`),
  task/service failed or a process disappeared (`failure`), a session failed to
  start or a workspace failed to prepare (`failure`). Leaving a shell or
  quitting an agent cleanly raises nothing.
- Resolved by: attaching to or typing into the session, restarting / stopping /
  deleting it, the agent starting to work again or going idle (a declined
  prompt, an interrupted turn), a newer item for the same
  session, or `otter ack`.
- Stored on the workspace (`state.json`); `AttentionCreated` /
  `AttentionResolved` events carry ids and kinds only.
- Each workspace derives one **activity** for the primary view:
  NEEDS YOU (question/approval/review) › FAILED › PREPARING › WORKING (an agent
  working, or a service/task running — not an idle shell) › COMPLETED › IDLE.
  `otter ls` groups by it; `otter ls --watch` redraws on every event.
- `otter new` starts Codex + a shell by default (design §27); `--no-agent`,
  `--no-shell` opt out.

## D-015 — Hardening pass: agent provider boundary (2026-10-07)

Audit of the code against [`architecture-lessons.md`](architecture-lessons.md).
Most of it already held (Git optional, sessions ≠ processes, tmux behind the
backend, one daemon, SSH as transport, no local-filesystem assumptions in the
control plane). Changed only what leaked:

- **Observation moved behind the provider.** `AgentProvider` is now `id`,
  `detect`, `launch_argv`, `observe`. Generic reconciliation passes an
  `ObserveContext` (cwd, start time, pid, last terminal output, env) and applies
  the returned generic `Observation`; transcript discovery, reading and the
  quiet-turn heuristic (`OTTER_AGENT_QUIET_SECS`) now live in `agents/codex.rs`.
- **Provider data is opaque.** `AgentInfo.transcript` / `transcript_offset`
  became `provider_state` (JSON only the provider reads); `resume_id` became
  `provider_session_id` (older `state.json` still loads via an alias).
- **Hosts advertise agent providers.** `HostStatus.agents` lists each
  supported provider with availability, version and `can_resume`, from the
  provider registry, instead of `codex` sitting in the tool list.
- The default provider is one constant (`agents::DEFAULT_PROVIDER`).
- Docs: [`invariants.md`](invariants.md) (each invariant → where enforced and
  tested) and `AGENTS.md` as a router.

Deliberately not done (architecture-lessons §24): other providers, plugin
mechanisms, orchestration. Next: dogfooding.

## D-016 — Event cursors, snapshots and protocol v2 (2026-10-07)

Prerequisite for a desktop client that reconnects after sleep or network loss.
Details: [`protocol.md`](protocol.md).

- **Snapshot + events, not event sourcing.** `state.snapshot` returns the
  workspaces and the event `seq` they correspond to, read under the store lock.
  Every store change happens under that lock and its event is emitted after
  the change, so a snapshot reflects every event up to `seq` (maybe more);
  clients apply events idempotently. Emitting an event *before* the change it
  describes would break this.
- **Replay cursors.** `events.subscribe {after}` replays `seq > after` from
  `events.jsonl`, then streams live events, each once and in order. The
  broadcast receiver is taken atomically with the head `seq`, and a lagging
  subscriber catches up from the log instead of dropping events. A cursor the
  log can't serve (older than the oldest retained event, or newer than the
  latest one) fails with `cursor_expired`; the client reloads a snapshot.
- **Storage unchanged** (D-005): replay reads the whole log, which is fine at
  current sizes and happens only on reconnect or lag. Rotation, an index or
  SQLite wait until the log is actually a problem. *(Superseded by D-037:
  the log is rotated and replay reads only the segments it needs.)*
- **Known gap:** a reset log that has since grown past a client's cursor is
  not detected. Fix if it bites: a log id generated with the file, returned by
  `state.snapshot` / `events.subscribe` and echoed by clients. *(Closed by
  D-037, exactly that way; it remains for clients or daemons that predate
  it.)*
- **Protocol v2.** `events.subscribe` gained params and a `{seq}` result, plus
  the `cursor_expired` code; v1 clients couldn't parse those, so the version
  was bumped. v1 was pre-stabilization. From v2 the compatibility rules in
  `protocol.md` apply: optional fields, new methods, new event kinds and new
  error codes are compatible (`Event::Unknown`, `ErrorCode::Unknown` make old
  clients tolerate them); frame changes and semantic changes bump.

## D-017 — Attach hardening (2026-10-07)

Audit of `session.attach` against repeated cycles and failure modes (dogfood
plan §6–§8), with tests for each that can run locally and a manual network
test against a real SSH host. (The SSH transport itself is now covered
automatically over `ssh localhost`, D-034; the network-failure checks below
remain manual.)

- **Teardown typed EOF into the session (fixed).** `portable_pty`'s writer
  sends `"\n"` + `VEOF` into the PTY when dropped. If the daemon stopped
  mid-attach the tmux client was still alive and forwarded it — the attached
  shell read `^D` and exited. The bridge now writes through a plain dup of the
  PTY master fd. Test: `attach_is_disposable_across_daemon_restart`.
- **A lagging attach could miss its own end.** If the bridge fell behind the
  event broadcast it ignored the gap and could keep showing a dead pane; it now
  checks the missed events in the log (as `events.subscribe` does, D-016).
- **Clients never hang on a dead connection.** `otter attach` gives up 5 s
  after Ctrl-] with no answer (restoring the terminal and saying the session
  keeps running), and SSH transports set `ServerAliveInterval=15` /
  `ServerAliveCountMax=3` after the host's own `ssh_args` (so explicit options
  win). Verified by `SIGSTOP`ing the remote sshd mid-attach: with Ctrl-] the
  client returns in 5 s; without input it notices in ~60 s; the session
  survives either way.
- **Initial terminal state:** tmux's own attach redraw shows the current
  screen (tested for a shell; alternate-screen apps rely on the same redraw
  but aren't tested). No `capture-pane` snapshot was added (plan §8: only if
  a client needs it). Scrollback stays server-side.
- Verified unchanged: repeated attach/detach (no leftover tmux clients), UTF-8
  both ways, Ctrl-C reaching the foreground process, restart/delete ending the
  attach with `ended`, re-attach after restart.

Not covered by automated tests: the network-loss timings above (manual), and
terminal rendering of colors/alternate screen in a real terminal emulator.

## D-018 — Desktop client M1: Tauri 2, a thin view over otter-client (2026-10-07)

`apps/desktop`: Tauri 2 + React/TypeScript. The Rust side uses `otter-client`
(which now owns the `hosts.toml` registry, so the app sees exactly the hosts
`otter` does), `otter-protocol` and `otter-core`; it never shells out to
`otter`. It is its own cargo workspace so Tauri's dependency tree stays out
of `cargo build/test` for the daemon and CLI.

- **State:** one task per host: `state.snapshot` → `events.subscribe(after =
  snapshot.seq)` → on each burst of events (150 ms coalescing) a fresh
  snapshot. The whole host list goes to the webview as one `hosts` event. The
  app keeps nothing of its own; a lost host shows its last known workspaces,
  dimmed, and reconnects every 5 s (30 s for a protocol mismatch).
- **Derived values come from core:** the backend flattens workspaces into a
  view model with `activity`, session `status` and per-session attention
  already computed by `otter-core`. TypeScript only groups, orders and words
  them (mirroring `otter ls`).
- **Terminal (the spike):** xterm.js 6 with the WebGL renderer (DOM fallback).
  Output crosses the Tauri boundary as raw bytes —
  `Channel<InvokeResponseBody::Raw>`, an `ArrayBuffer` in JS — not JSON or
  base64; input, resize and detach are commands. One `session.attach`
  connection per visible terminal; switching sessions detaches and attaches
  again, and tmux redraws the screen. Throughput under heavy output (e.g.
  `yes | head -n 200000`) is not measured yet.
- **Notifications:** diffed in the webview from attention ids — the first
  snapshot per host is the baseline, later new needs-you or failure items
  notify once, unless that workspace is on screen with the window focused.
  Replays after sleep therefore notify for what was missed. The notification
  plugin reports no clicks on desktop, so activating the app within two
  minutes of a notification jumps to its workspace and session.
- **Scope held:** no workspace creation, settings, Git/file UI, layouts or
  multiple simultaneous terminals. Actions are the thin RPCs the design shows:
  Mark handled (`attention.resolve`) and Restart (`session.restart`).

Not done: Linux build dependencies (WebKitGTK) in the flake; notification
delivery verified only up to the permission prompt under `tauri dev`.

## D-019 — One version, released on every merge (2026-10-07)

- **One product version** for otterd, otter, the crates and the desktop app
  (the workspace `Cargo.toml`, the desktop `Cargo.toml` and `package.json`;
  Tauri reads the crate's). `scripts/version.py` reads, bumps and writes it,
  including the lockfiles' local entries so `--locked` builds hold. The app
  shows its version in the title bar, and a host's `otterd` version in the
  hosts list when it differs.
- **CI** (`ci.yml`, on pull requests): fmt, clippy `-D warnings` and the full
  test suite on Linux (with tmux); the desktop UI build and clippy on macOS.
- **Release** (`release.yml`, on every push to `main`): bump (patch, or the
  merged PR's `release:minor` / `release:major` label; `release:skip` skips),
  commit `Release vX.Y.Z` and tag it, then build from the tag — the universal
  macOS app (`.dmg` and `.app.tar.gz`) and `otterd`/`otter` for linux-x86_64
  and macos-arm64 — and publish the GitHub release once every artifact is
  attached (a draft until then). The bump is pushed with `GITHUB_TOKEN`, which
  starts no workflows, so it cannot loop; it requires `main` to accept pushes
  from Actions.
- Not yet: code signing and notarization (the app is unsigned; README says how
  to open it), auto-update inside the app (since D-125), and Linux desktop
  builds. *Amended 2026-10-19:* the desktop app is Mac-only, for good: no
  Linux or Windows desktop builds (roadmap, *Not in scope*).

## D-020 — Onboarding: hosts and installs from the app (2026-10-08)

Dogfooding v0.1.1: a fresh install of the app showed "No hosts yet" and
pointed at a `otter` nobody had installed. Fixed by making the app enough on
its own:

- **Hosts are managed in the app** — Add a host (name, SSH destination or
  this Mac, advanced: otterd path and state directory), and per host: status,
  version, install/update otterd, remove. `hosts.toml` stays the one source of
  truth, shared with `otter` through `otter_client::config` (`add_host`,
  `remove_host`); the app reconciles its host tasks against the file at
  startup, after its own edits and whenever the file changes, so
  `otter host add/rm` show up live.
- **Installing otterd from a release** (`otter_client::install`): a POSIX
  script fed to `sh -s` on the host — over the same SSH options as `otterd
  dial`, or locally — picks the build for `uname -sm`, downloads it with curl
  or wget and installs `otterd` and `otter` into `~/.local/bin` (the default
  `otterd_path`). For an update it then stops the running daemon by its pid
  file (checked to be a `otterd … serve`); sessions keep running and the next
  connection starts the new version. Used by Add host (when the check finds
  otterd missing or on another protocol), the host dialog, and `otter host
  install`. Releases now ship linux-x86_64, linux-aarch64 and macos-universal
  binaries, the names the script asks for.
- **Command-line tools** from the app menu (and the first-run screen): the
  same install, locally, without touching a running daemon.
- A host added as "this Mac" points at `~/.local/bin/otterd` explicitly: an app
  started from Finder has no useful `PATH`.

## D-021 — The app covers the whole daily loop; light and dark (2026-10-08)

Dogfooding v0.1.2: with a host connected the app dead-ended at "Create one
with `otter new`". M1 had deliberately left creation out (D-018), but a
client you can't start work from isn't one you can live in. Added, all as thin
RPCs the daemon already had:

- **New workspace** (title bar, ⌘N, first-run screen): name, host, files
  (empty / Git repository with branch and base / existing folder), and what
  starts — Codex with an optional prompt (offered only when the host reports
  `codex` available, from `host.status` fetched once per connection) and a
  shell. The new workspace is selected as soon as it appears.
- **Sessions:** `+` after the tabs (shell, Codex, service, task); per session
  Restart / Stop / Delete. **Workspaces:** Retry preparation, Delete (Git
  worktrees can discard uncommitted changes; existing folders are kept).
- **Command-line tools:** the title bar offers to install or update them when
  `~/.local/bin/otter` is missing or a different version — installing otterd
  on a *host* puts `otter` there, not on this Mac, which is what confused us.
- **Theme:** System / Light / Dark, remembered per Mac, applied to the UI, the
  terminal and the native title bar. Colors are role tokens with one light
  override block.

## D-022 — Faster app releases (2026-10-08)

The app took ~7 min to release: two release compiles of the whole Tauri tree,
arm64 then x86_64, each ~2.5 min, because the template's profile was fat LTO
with one codegen unit (whole-program optimization on one core, every time).
Measured locally on a cached rebuild: fat/1 unit 60 s, thin/16 units 25 s, no
LTO 15 s; binary 5.0 / 6.2 / 6.0 MB.

- Release profile: thin LTO, default codegen units (~2.4x faster, ~1 MB
  larger).
- The two architectures build in parallel jobs (`tauri build --target …
  --no-bundle`), each with its own Rust cache; a third job `lipo`s them into
  the universal binary and runs `tauri bundle` (~20 s for app + dmg).

## D-023 — Claude Code as a second agent provider (2026-10-08)

Asked for directly, so the "no other providers yet" rule (D-015) gives way;
the provider boundary from D-015 made it a new module, not a redesign.

- `agents/claude.rs`, id `claude`: launch `claude [prompt]`, resume
  `claude --resume <session-id>` (both with `--settings <file>` for Otter's
  hooks since D-035).
- Identity: Claude Code's transcript
  `$CLAUDE_CONFIG_DIR|~/.claude/projects/<cwd with non-alphanumerics → '->/<session-id>.jsonl`
  (checked against real transcripts; the cwd is tried as given and with
  symlinks resolved). Bound to the newest transcript for the cwd written
  since the execution started and not bound elsewhere; until the current file
  is written by this execution, a newer one wins (a resume may continue the
  old file or start a new one).
- State from turn records: user prompt / tool result → working, assistant
  `stop_reason: tool_use` (or streaming) → working, `end_turn` →
  waiting for input with its text as the last message, an interruption →
  idle. Sidechain (subagent) and meta records are ignored.
- ~~The quiet-turn heuristic is shared (`agents::settle`).~~ Amended by
  D-035: permission prompts and questions come from Claude Code's hooks,
  passed per launch; the shared heuristic is only the fallback when no hook
  reports (hooks disabled by policy). Observed on Claude Code 2.1.295: while
  a permission prompt is up the transcript's last record is the assistant
  `tool_use` and nothing more is written; approving writes the `tool_result`
  when the tool finishes; declining writes the rejection `tool_result` and
  `[Request interrupted by user for tool use]` (→ idle).
- `otter new --agent claude`; the app offers Codex / Claude Code / none in
  New workspace and Claude Code in New session, per what the host reports.
- Test: a fake `claude` writing transcripts the same way
  (`claude_code_session_is_observed_resumed_and_needs_you_when_done`),
  checked against a deliberately broken end-of-turn rule.

Follow-up: per-architecture macOS downloads. Each desktop architecture job
also bundles and publishes its own `.app`/`.dmg` (~20 s, in parallel), and
`otterd`/`otter` build per macOS architecture in parallel jobs, combined into
the universal tarball (what `otter_client::install` uses) by a small `lipo`
job — the macOS binaries no longer build arm64 then x86_64 in one job.

## D-024 — Renamed to Otter (2026-10-08)

The product is **Otter** (icon: `docs/assets/otter.png`): the app `Otter.app`
(`dev.otter.app`), the CLI `otter` (was `workctl`), the daemon `otterd` (was
`workd`), crates `otter-*`, repository `alvingaopro/otter`, environment
variables `OTTER_*`, release assets `otter-<v>-<target>.tar.gz` and
`Otter_<v>_macos_*.dmg`. Earlier entries in this file use the new names.

Existing installs keep working:

- Config: `~/.config/otter`; `~/.config/workd/hosts.toml` is copied over on
  first use, `WORKCTL_CONFIG_DIR` is still honored, and hosts.toml's
  `workd_path` key is accepted (the old default `~/.local/bin/workd` maps to
  `~/.local/bin/otterd`; custom paths are kept).
- Daemon home: `~/.otter`, but `~/.workd` when only that exists — a host keeps
  its workspaces and running sessions. `WORKD_HOME` and
  `WORKD_AGENT_QUIET_SECS` are still honored.
- The daemon's run files keep their names (`run/workd.sock`, `workd.lock`,
  `workd.pid`, `logs/workd.log`): an old daemon still running on a home holds
  the same lock, so a new one can never run beside it on the same state.
- Installing/updating from the app stops a running old `workd … serve` as
  well as `otterd`, so the next connection starts `otterd` on the same home.
- Sessions now get `OTTER_SESSION_ID` etc. (was `WORKD_*`).

## D-025 — Sign the app bundle (2026-10-08)

v0.2.0's `.dmg`s opened as "Otter is damaged". The bundles were never
signed: Tauri signs only when `bundle.macOS.signingIdentity` is set, so the
app carried just the linker's ad-hoc signature on its binary (arm64: "code
has no resources but signature indicates they must be present") or none at
all after `lipo` (universal). macOS reports an invalid signature on a
download as damaged, with no way past it.

- `signingIdentity: "-"`: Tauri ad-hoc signs the binary and seals the bundle.
  Gatekeeper then treats it as an unnotarized app (System Settings → Open
  Anyway), not a damaged one.
- The release verifies every bundle with `codesign --verify --deep --strict`
  before uploading, and re-signs the `lipo`ed CLI binaries.
- A Developer ID certificate plus notarization (Apple Developer Program) would
  remove the first-launch prompt entirely; not done.

## D-026 — No universal macOS builds (2026-10-08)

With per-architecture downloads (arm64, x86_64), the universal `.dmg` and CLI
tarball were redundant. Dropped both, and with them the jobs that waited for
both architectures and then for another macOS runner to `lipo` (5 minutes of
queueing in one release). The installer picks `macos-arm64` or
`macos-x86_64` from `uname -m` (Rosetta reports x86_64, which runs fine).
Every release job now runs in parallel after the version bump.

## D-027 — Exited sessions and tmux 3.2 (2026-10-08)

Dogfooding v0.2.3:

- **Exited sessions were a dead end in the app.** The "exited · Restart" bar
  only appeared when the exit happened while attached; opening an
  already-exited session showed its last screen as "attached", with Restart
  and Delete only in the tab row's ⋯ menu. Now the pane derives it from the
  session's status (completed, failed, stopped, lost, or failed to start):
  the last screen dimmed, the reason, **Restart** (for agents: resumes the
  conversation) and **Delete**. Tabs have a close button (delete, or stop and
  delete when running, after a confirmation). Agent tabs always name their
  agent.
- **tmux 3.2 showed a config error over the first window** (`invalid option:
  remain-on-exit-format`, a 3.3 option) — the documented minimum is 3.2. The
  option is now written only for tmux ≥ 3.3 (version from `tmux -V`; unknown
  builds are assumed recent); on 3.2 tmux's own "Pane is dead" line is
  stripped from `logs` like ours.
- **No scrolling back.** A session's history lives in tmux; the client only
  ever sees the screen tmux draws, so the app's terminal had nothing to scroll.
  tmux now has `mouse on`: the wheel scrolls the session's history (50,000
  lines) and leaves it at the bottom; text copied there reaches the Mac
  clipboard (OSC 52, `set-clipboard on`, xterm's clipboard addon).
  Option-drag still selects locally in the app.

## D-028 — Copying from the terminal; outdated hosts said plainly (2026-10-08)

- With tmux mouse mode (D-027), dragging selects in tmux, which hands the
  text to the client as OSC 52 on mouse-up. xterm's clipboard addon then used
  the WebView's clipboard API, which refuses writes that don't follow a user
  gesture — nothing was copied. Writes now go through Tauri's native
  clipboard plugin (programs still can't *read* the Mac clipboard via OSC 52),
  and ⌘C copies a local Option-drag selection. E2E tests check the daemon
  side: the wheel scrolls tmux history, and a drag makes tmux send OSC 52
  through the attach bridge.
- Scrolling "didn't work" after v0.2.4 because the host still ran 0.2.3: the
  version mismatch was only a small label in the hosts list. A workspace on a
  host whose otterd differs from the app now says so at the top, with an
  **Update otterd** button.

## D-029 — Host page: usage and port mappings (2026-10-08)

Asked for: a per-host dashboard to tell whether a host is fully occupied
(as in a monitoring tool), and portkeeper's port mappings in both directions.

- **Host page** (click a host): status, version, update/remove, then
  **Usage** — stat tiles (CPU, load per CPU, memory + swap, network), charts
  over the last 10 minutes (CPU, memory, network in/out, disk I/O), disk space
  per filesystem and the busiest processes — and **Ports**.
- **Usage** comes from the daemon: `host.metrics`, sampled every 5 s by a
  background thread with `sysinfo` (Linux and macOS), keeping 120 samples so
  the charts are full on open. Charts follow the dataviz rules (one axis, 2px
  lines, a legend for two series, values as text, a crosshair tooltip);
  the series pair was validated against both theme surfaces (light orange is
  just under 3:1, carried by visible values). Meters escalate to the reserved
  warning/critical colors at 75 % / 90 %, always with their numbers.
- **Ports**: `host.ports` lists listening TCP ports with their process
  (`ss -ltnp` / `lsof`), each with **Map to this Mac**. Mappings follow
  portkeeper: added and removed on the host's existing ControlMaster
  (`ssh -O forward|cancel -L|-R 127.0.0.1:…`), so no new connections and
  nothing listens beyond loopback. `to_local` (-L) opens a host port (or a
  machine it reaches, e.g. a database) here; `to_host` (-R) the reverse.
  The logic lives in `otter_client::forward`; the app re-applies a host's
  mappings whenever its connection's master pid changes (reconnect, sleep,
  app start) and shows each mapping's state. **Keep across restarts** pins it
  in `ports.toml` next to `hosts.toml`.
- Not yet: `otter port …` / `otter host top` in the CLI, mapping a host's
  port ranges, image paste and browser login from portkeeper.

## D-030 — Recorded usage for trends (2026-10-08)

The host page showed only otterd's in-memory 10 minutes, lost on every
restart. otterd now records usage on the host (so it accumulates while the
Mac is closed): `history::Recorder` folds the 5 s samples into per-minute
points (average, and peak for CPU and memory) in `state/metrics/minutes.jsonl`
for 7 days, rolling older minutes into hourly points in `hours.jsonl` for 90
days — so a restart mid-hour loses nothing and the files stay small (rewritten
when trimmed). `host.history {range}` (1h at 1 min, 24h at 5 min, 7d at
30 min, 30d at 1 h) is a new method rather than a parameter on `host.metrics`,
which old clients send without params. The host page gets a range picker
(10 min live · 1 h · 24 h · 7 d · 30 d); trend charts use a time axis so a
stretch with no recording shows as a gap, and plot average with peak.

## D-031 — Menu bar, files, image paste (2026-10-08)

- **Menu bar.** A tray item shows how many workspaces need you (title) and
  lists them; picking one opens the app on it. Closing the window hides it,
  so notifications and the status keep working; the Dock icon and the menu
  bar bring it back, Quit quits.
  *Amended 2026-10-16:* one long line widened the whole menu, so each line is
  cut to 44 characters (task titles to 24), a finished turn says only that it
  finished (the reply is in the app), and the host is named only when a line
  on another host has the same name.
- **Files.** `fs.list/read/write` on otterd (paths relative to the
  workspace root, or absolute; 1 MiB base64 chunks, no message-size limit on
  the wire). The workspace view has a Files panel: browse, click a file to
  download (save dialog), upload by button or by dropping files on the
  window, with progress. Uploads don't replace a file unless confirmed.
- **Image paste (portkeeper's feature, done differently).** Claude Code on
  Linux reads a clipboard image by running `xclip`, then `wl-paste`.
  Portkeeper serves the Mac pasteboard to a stand-in `wl-paste` over a
  reverse-forwarded socket. Otter's terminal sees the keystroke itself, so on
  Ctrl+V the app reads the Mac clipboard image (native plugin), sends it to
  otterd (`session.paste_image`, stored 0600 for that session), then sends
  the key; otterd's stand-in `wl-paste` (first on every session's PATH)
  serves it for two minutes and otherwise defers to a real `wl-paste`. The
  host never gets a way to read the Mac's clipboard. Works in sessions
  attached through the app (not plain `ssh`), and in login shells whose
  profile keeps the inherited PATH.
  *Amended 2026-10-20 (D-145):* on a Mac host, Claude Code reads the
  host's pasteboard, so otterd puts the image there too.

## D-032 — Browser login (2026-10-08)

Portkeeper's browser login, adapted. CLI logins on a host open their sign-in
page in the Mac's browser and finish on the host.

- **Caught where tools open browsers:** `xdg-open` (Go, Node, Rust tools),
  `www-browser` (Python with `TERM`), and `BROWSER=otter-open` set in sessions
  that don't choose a browser (Python without a display). One stand-in in
  `run/bin` (first on every session's PATH) runs `otterd open-url`; a host
  with its own display, a non-URL argument, or a refusal falls through to a
  real command of the same name.
- **Policy (portkeeper's):** https only; trusted sign-in providers only (AWS
  IAM Identity Center, Google, Microsoft, GitHub, HashiCorp); at most one
  callback port — the explicit port of a loopback `redirect_uri`, also inside
  wrapped https URLs (two levels), unprivileged. Never a reverse mapping or
  anything else.
- **Through the existing connection, not a new channel.** otterd queues the
  request and emits `BrowserOpenRequested` (provider and port only — no URL
  in `events.jsonl`); an app subscribed with `browser: true` takes the URL
  once (`browser.take`), maps the callback port with a temporary `-L`
  mapping, and opens the page. With no such app connected the request is
  refused, so the tool prints the URL as it would without a browser. Two
  Macs: the first to take it opens it.
- **Callback mappings clean up:** removed when the host stops listening on the
  port (the tool got its redirect) or after 10 minutes, shown meanwhile as
  "Sign-in" among the host's mappings.
- **`otter attach` does it too** (follow-up): while attached, the CLI
  subscribes with `browser: true` like the app (`otter_client::login`), takes
  pages, maps a callback port over the shared SSH connection (not for a local
  host, where the port is already here) and opens them with `open` /
  `xdg-open`. Same once-only take, so with the app also running exactly one
  of them opens the page. Its callback mappings go when the host stops
  listening, after 10 minutes, or when the attach ends — detaching mid-login
  breaks that login's redirect (rerun it attached). Skipped when `otter`
  itself runs inside an Otter session (its "browser" would be a host's
  stand-in). Only attach: it's where these tools are run; `ls --watch` and
  `events --follow` don't serve logins.
- Not built: asking before opening an untrusted provider, an editable list,
  a per-host off switch. *Amended 2026-10-19:* asking and an editable list
  came in D-121.

## D-034 — End-to-end tests for the CLI and the SSH transport (2026-10-08)

Two gaps: the daemon's e2e tests used the client library directly, so the
`otter` binary (argument parsing, `[host:]workspace[/session]` addressing,
messages, exit codes) was never run end to end; and nothing went through
`ssh <host> otterd dial`.

- **`crates/cli/tests/e2e.rs`** runs the real `otter` with its own
  `OTTER_CONFIG_DIR` against real daemons with their own homes: host add →
  new → start → `otter ls` → stop → delete, the same workspace name on two
  hosts (ambiguity error, then `host:` qualification), and failure paths
  (unknown session/host, no hosts, unreachable otterd, delete without
  `--yes`). Output is asserted by substring; stdout is a pipe, so `otter ls`
  is uncolored.
- **Finding `otterd`:** Cargo only exposes a package's own binaries to its
  tests, so the test uses the `otterd` next to `otter` in the target
  directory (built by any workspace-root `cargo test`, since the daemon's
  tests need it) and builds it with `cargo build -p otterd` if missing. Not
  cross-package `bindeps` (unstable). Caveat: `cargo test -p otter` alone
  uses whatever `otterd` is already there.
- **SSH:** the same flow with the host added as `--ssh localhost` and an
  absolute `--otterd-path` (a non-interactive ssh's PATH has no freshly
  built binary). Skips when `ssh -o BatchMode=yes localhost true` fails,
  like the direnv test; CI sets up a passwordless key for `ssh localhost`
  and sets `OTTER_E2E_REQUIRE_SSH=1`, which turns a skip into a failure.

## D-035 — Knowing when an agent needs you: real signals first (2026-10-08)

"Needs approval" came from `agents::settle` flagging ~8 s of quiet, never
checked against a real prompt, and `AttentionKind::Question` was never
raised. Calibrated against the real CLIs (tmux, scratch git dirs, tiny
prompts; the user's config files only read), then changed:

**Claude Code: its hooks, passed per launch.** Otter writes
`run/agents/<session-id>/claude-settings.json` (hooks only) and launches
`claude --settings <file> …`. Every hook runs `'<otterd>' internal-agent-hook
claude '<dir>/claude-hooks.jsonl'` (`async: true`): otterd keeps event, tool,
notification type, subagent id and a short detail (the question, or the tool
plus Claude Code's own description of the call — never tool input or output)
and appends one line; it prints nothing and always exits 0, so it can't
steer a decision. Why this is within D-013's rule (don't change the user's
agent configuration, no "dangerous" flags): `--settings` is an ordinary
per-invocation flag; nothing of the user's is written; and hooks *merge*
across settings sources — verified: with Otter's settings the user's own
`Stop` hook (`~/.claude/hooks/notify-schedule.sh`) still ran in the same
session. A policy that disables hooks (`disableAllHooks`,
`allowManagedHooksOnly`) silences Otter's too; then the fallback below
applies.

Observed on Claude Code **2.1.295** (permission mode `default`; its default
is now `auto`, which asked nothing for `touch`/`rm -rf` here):

| when | hook stdin (trimmed) |
|------|------------------------|
| tool needs permission | `PreToolUse` then, ~50 ms later, `{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"touch z","description":"Create empty file z"},"permission_suggestions":[…]}` |
| prompt still up ~6 s later | `{"hook_event_name":"Notification","message":"Claude needs your permission","notification_type":"permission_prompt"}` |
| approved | `PostToolUse` (when the tool finishes), later `Stop` |
| declined | no hook; transcript gets the rejection + `[Request interrupted by user for tool use]` |
| AskUserQuestion (also in auto mode) | `PreToolUse` + `PermissionRequest` with `"tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Should the file be named a or b?",…}]}` |
| before the folder-trust dialog is answered | nothing (`SessionStart` comes after) |

`idle_prompt` never arrived (115 s idle after a declined prompt); not used.
During the prompt the screen and terminal were completely still; during a
20 s command the screen changed every second (`Running… (20s)`).

Rules (`agents/claude.rs`): `PermissionRequest` → `blocked` + `approval`
("claude needs your approval: Bash: Create empty file z"); for
`AskUserQuestion` (or `PreToolUse` for it, or MCP `Elicitation`) → `blocked`
+ **`question`** ("claude asks: …"). Cleared by `PostToolUse*` (same
subagent), `UserPromptSubmit`, `Stop`, or a transcript user record after it
(a decline). A long approved command shows as the screen changing more than
2 s after the prompt appeared → `working` before `PostToolUse`. *Amended
2026-10-20:* any change counted, so a prompt that redrew (an auto-deny
countdown, a background task ending) lost its item; now it takes the screen
moving at every look for 2 s, and another tool's `PostToolUse` no longer
clears a prompt (D-136). Once any hook
has reported for the execution the heuristic is off: a long think with a
still screen stays `working`.

Checked end to end with the real Claude Code through a dev otterd: question
→ "NEEDS YOU · claude asks: Should the new file be named a or b?";
`touch` in manual mode → "needs your approval: Bash: Create an empty probe
file" within 3 s, still there at 18 s; Esc → idle, resolved; approved `sleep
15 && touch …` → working during the sleep, then review.

**Codex: still a heuristic, now on the screen.** No signal is usable (D-013,
items 1–2). `settle` now takes "when the screen last changed": the daemon
captures the visible screen of agents that are starting, working or blocked
each pass (one capture each) and keeps a digest in memory; `window_activity`
is no longer read. Codex shows a ticking elapsed time while thinking or
running a command, so only a prompt is still. It can't tell an approval from
a question, so it raises `approval` with the hedged "seems to be waiting for
you (approval or question?)". A status line that changes on its own would
make it miss; it never fires during the ticking states.

Known limits: attaching resizes the pane, which reads as "answered" (shown
as working until the tool finishes; the item was resolved by attaching
anyway, but detaching without answering doesn't re-raise it). *Amended
2026-10-20:* no longer; a resize is one change, which is no answer (D-136).
Hooks are
trusted for the whole execution once one reports; if they stop (otterd
moved — the installer keeps it at `~/.local/bin/otterd`) there is no
fallback until the agent restarts. After an otterd restart the first look
at a screen is not taken as a change, so a pending prompt stays reported.
A Claude Code too old for `--settings` fails to start visibly. Codex was
measured directly in tmux and through a faithful fake, not through otterd:
the installed 0.154.0 rejected `--no-daemon` (since fixed, D-013 Launch).

**Startup dialogs:** an agent given a prompt that stands still before its
transcript exists is `blocked` (folder trust for both CLIs, D-013 item 3);
without a prompt, `idle` as before.

Generic changes: `LaunchContext` (env, the session's private dir under
`run/agents/`, removed with the session; the otterd path) for `launch_argv`;
`ObserveContext` gets the dir and `screen_changed` (replacing `last_output`);
`Observation::blocker` (kind + detail) picks the attention kind and summary.
`AgentState` and the protocol are unchanged. A `blocked` → `idle` transition
now resolves the item.

Tests: unit tests replay the recorded hook/transcript sequence (approve,
long command, long think, question, decline) and the no-hooks fallback;
e2e fakes call the hooks from `--settings` like Claude Code
(`claude_code_prompts_come_from_its_hooks`), ignore them
(`claude_code_without_hooks_falls_back_to_the_quiet_turn`), and a fake Codex
redraws an unchanged prompt every 0.3 s (which the old output-based rule
missed — checked) or ticks for 6 s without a rollout record
(`long_thinking_with_a_ticking_screen_is_not_waiting`).

## D-036 — Archive is not delete (2026-10-08)

Design §28 asks for archiving and deleting to be distinct; `Archived` existed
as a state but nothing set it. Now `workspace.archive` / `workspace.unarchive`
(`otter ws archive|unarchive`, Archive / Unarchive in the app's workspace
menu). Conservative choices, since §28 only names the state:

- **What archiving does:** stops running executions (marked `stopped`, like
  `session.stop`); exited ones keep their retained output (D-027), so their
  last screen stays readable; resolves all its attention; keeps everything
  else — the sessions themselves (identity, agent conversation ids), the
  files, the Git worktree, the branch and uncommitted changes. Nothing on disk changes, so
  the managed-resources-only rule is trivially kept.
- **Only from `ready` or `failed`.** Archiving a `preparing` workspace is
  refused: preparation ends by marking it ready and starting its sessions.
  (`prepare` now also drops its result if the workspace is no longer
  preparing, as a backstop.) Archiving twice is a conflict.
- **Archived is quiet:** `attention::raise` does nothing for an archived
  workspace, `session.create` / `session.restart` / `workspace.prepare` are
  refused ("unarchive it first"). Reconciliation still runs over it (nothing
  is running, so it only settles stale agent states). The state is in
  `state.json`, so it survives daemon restarts; startup neither prepares nor
  starts anything for it.
- **Unarchive = prepare again:** `preparing` → `ready` (files checked,
  environment re-resolved, which a removed `.envrc` or folder can fail like
  any preparation). Sessions that were stopped **stay stopped** until the
  developer restarts them (a new execution; agents resume their
  conversation) — waking every agent at once is not what "bring this back"
  should mean. Sessions that had never started do start, as on any ready.
- **Delete still works on an archived workspace**, with the same rules
  (uncommitted changes protected unless forced, branch kept).
- **The name stays taken** while archived; names are how workspaces are
  addressed.
- **Hidden in clients, not in the daemon.** `workspace.list` and
  `state.snapshot` keep returning archived workspaces (changing what they
  return would be incompatible, D-016); `otter ls` / `otter ws ls` hide them
  unless `--archived` and mention how many are hidden; the app shows them in
  a collapsed ARCHIVED group at the bottom of the sidebar. `Activity` gained
  `archived` (computed by clients, not on the wire).
- **Protocol:** two methods and two events (`WorkspaceArchived`,
  `WorkspaceUnarchived`) — compatible additions, no version bump.
- Not done: COMPLETED and CLEANED from §28's diagram, auto-archiving, and
  freeing disk for archived workspaces.

## D-037 — A bounded event log with an identity (2026-10-08)

`events.jsonl` grew forever, replay from a cursor read all of it, and a log
that started over (files lost or removed) and grew past a client's cursor was
taken for the same log — the client silently missed everything up to its
cursor (D-016's known gap).

- **Rotation, not compaction.** The active file stays `state/events.jsonl`
  (tools and tests read it). Past 4 MiB it is renamed
  `events.<first seq>.jsonl` and a new one started; the newest 3 rotated
  segments are kept, so the log is at most ~16 MiB (~75–100k events). Rotating happens in `emit`, under the log's lock.
  `OTTER_EVENTS_SEGMENT_BYTES` overrides the size (for tests). Nothing is
  compacted into a summary: the daemon isn't event-sourced (D-016), and
  `state.snapshot` comes from `state.json`, so a client whose cursor was
  rotated away gets a complete state from a snapshot.
- **Replay is indexed by segment.** Segment names carry their first `seq`, so
  `since(after)` opens only the segments from the one holding `after + 1`
  on. The newest ~2000 events are also kept in memory (a suffix of what is on
  disk, trimmed with it), which serves reconnects and lagging subscribers
  without reading files. `events.list` reads newest-first until it has
  enough.
- **Log id.** `state/events.id` holds a random `log_…`, generated when there
  are no log files at all (new host, or the log was removed) and once for a
  log from an older daemon. It survives restarts and rotation.
  `state.snapshot` and `events.subscribe` return it as `log_id`; clients echo
  it as `events.subscribe {log_id}`; a different one fails with
  `cursor_expired`.
- **Same error code.** A changed log is reported as `cursor_expired`, not a
  new code: every shipped client already resyncs on it, while a new code
  would be `Unknown` to them (the CLI's follower would retry the same cursor
  forever). The message says which case it is.
- **No protocol bump.** `log_id` is an optional field in three places. The
  cursor's meaning (`seq` in one log) is unchanged; the id only lets the
  daemon refuse cursors it previously had to guess about. Old clients send no
  id and get the `seq`-only checks; new clients talking to an older daemon
  get no id, send none and fall back to the same checks (the gap remains
  there, and only there). The desktop app talks to whatever otterd a host
  has (D-028), so both directions matter and are unit-tested.
- **No silent skips from rotation.** A subscriber that lagged behind the
  broadcast buffer and whose missed events were rotated away is
  disconnected, so its reconnect gets `cursor_expired`, rather than skipping
  ahead (invariant 12).
- **Clients.** `otter_client` carries a `Cursor {seq, log_id}`
  (`StateSnapshot::cursor`, `EventStream::cursor`,
  `ClientError::is_cursor_expired`). `otter events --follow` resumes from the
  stream's cursor and, on `cursor_expired`, says events were missed and
  follows on from a fresh snapshot. `otter ls --watch` redraws on every
  (re)connect. The app already took a snapshot on every connect; it now
  passes the snapshot's log id and re-snapshots on `cursor_expired` instead
  of reporting the host unreachable.
- The log holds only what it held before (ids, names, kinds, exit codes);
  the id file holds only the id.

## D-038 — The brief in the app, and editable (2026-10-08)

Design §8's brief existed in the model, but only `otter new --goal` could set
it, nothing could change it, and the app neither set nor showed it. Since §8
lists `decisions:` and `constraints:`, which accrue while the work goes on,
the brief has to be editable, so it got an RPC end to end:

- **`workspace.set_brief {workspace, brief}` replaces the whole brief**
  rather than patching fields. Every client already holds the current brief
  (snapshots carry it), so read-modify-write is simple and a patch format
  with "clear this field" semantics isn't needed. Two clients editing at once:
  last write wins, which is fine for a human-edited note.
- **The daemon normalizes** (trim; blank text absent, blank list items
  dropped) on create and on set, so an emptied field is absent, not `""`, and
  `Brief::is_empty` means what it says.
- **Any state**, archived included: the brief describes the workspace and
  runs nothing.
- **`WorkspaceBriefChanged {workspace_id}`, with no text.** Brief text is
  user content (it may mention anything), and events stay ids and kinds
  (§19). The event matters anyway: clients re-read on events, so it is what
  updates the app and `otter ls --watch` after an edit elsewhere. An
  unchanged brief emits nothing.
- **CLI:** `otter ws brief <ws>` prints it; `--title/--goal/--description`
  set (`""` clears), repeatable `--constraint/--reference/--decision` append,
  `--clear` starts from empty. `otter ws show` lists the brief too.
- **App, kept compact:** an optional Goal field in New workspace; in the
  workspace pane one line under the name with the goal (or title), "+N more"
  when the brief holds more, and "Add a goal" when it is empty; clicking it
  (or the workspace menu) opens a dialog with all six fields, the lists one
  per line. The sidebar is unchanged.
- Not done: agents reading the brief (e.g. as context for a new agent
  session), and history of brief edits beyond the event.

## D-039 — A workspace timeline in the app (2026-10-08)

Design §33 Phase 9 lists a timeline; only `otter events` showed history. The
workspace pane now has a **Timeline** panel (next to Files; one side panel at
a time): what happened in this workspace, one human line per event, newest
first, with relative times (full time on hover), updating live.

- **No new protocol.** History is `events.list` (the host's most recent 2000
  events, filtered to the workspace in the app's backend); live rows are the
  events the host task already follows for re-snapshots, now also forwarded to
  the UI as `host-event`. Listening starts before the load and both are merged
  by `seq`, so nothing falls between them.
- **Resync = reload.** Whenever a host's event stream (re)starts — reconnect,
  or `cursor_expired` after the log rotated or started over (D-037) — the
  backend emits `host-resync` and an open timeline loads again, replacing
  what it had (seqs from a log that started over mean nothing). Events missed
  while the stream was down are thereby picked up from the log.
- **Bounded.** When the window doesn't reach back to `WorkspaceCreated`
  (busy host, or rotated away), the panel says earlier events aren't in the
  host's recent history rather than paging. A per-workspace filter on
  `events.list` would make this exact; not worth a protocol change until a
  long-lived workspace shows the gap.
- **Wording lives in `timeline.ts`** (presentation, D-018): sessions are named
  from the workspace or from `SessionCreated` events (deleted sessions); agent
  state changes show only real changes (not `starting`, not repeats); a
  failure/completion flagged right after its process exited isn't repeated;
  resolved attention says what it was about. Unknown kinds are left out.
- Not done: filtering, paging, a timeline across workspaces, and the
  terminal's own output (events never carry it).

## D-040 — Desktop chrome, and pinned workspaces (2026-10-08)

The window stacked a full-width title bar, a large workspace header, a tab
row and an inset terminal card before any output. Flattened:

- **The sidebar's top row is the title bar** (overlay style: traffic lights,
  theme, New); the workspace pane's header is one row of the same height:
  name, host, source, path, brief, Timeline, Files, ⋯
  (Timeline and Files left the tab row: they are about the workspace, not a
  session). The brief moves there from the
  line under the name (D-038), cut off when long. "N need you · N working"
  becomes two badges by the Otter mark. The selected tab joins the
  terminal, which runs edge to edge. The hosts list collapses (remembered
  per Mac); its header says "N of M connected" so collapsing never hides a
  host that is down.
- **Pinned workspaces** sit in a Pinned section above the status groups, in
  an order the developer sets (drag, or Move up/down in the menus). Pin from
  the workspace ⋯ menu or by right-clicking a sidebar row.
  - **Kept by the app, in `pins.toml`** next to `hosts.toml`: an ordered list
    of `host/workspace-id`. Not in `otterd`: the list spans hosts and no one
    host could hold the order, and it is a view preference, not runtime
    state (invariant 11). Another Mac has its own pins.
  - A pinned workspace leaves its status group but keeps its status mark
    and still counts in the badges, so one that needs you shows orange in
    Pinned, once.
  - A pin whose workspace isn't loaded (host still connecting, or offline
    with no last state) keeps its place; it is dropped only when its host is
    connected and no longer has the workspace.
  - Reordering uses pointer events, not HTML5 drag-and-drop, which the
    webview's file drop (Files panel) takes over.
- Not done: pins in `otter ls`, which could read the same file.

## D-041 — An Activity Bar: Features and Workspaces are separate views (2026-10-09)

The app grows a product-oriented surface (Features: what should be built)
next to the engineering one (Workspaces: where work runs). A VS Code-style
**Activity Bar** at the far left picks the view; Features is first (the
primary entry), Workspaces second, Settings (theme, add a host, command-line
tools) at the bottom.

- **Switching views never stops anything.** Every view stays mounted and the
  ones behind are only hidden (`hidden`), so a terminal keeps its xterm
  state and its attach; nothing in a view may detach, stop or restart on
  hide. A hidden terminal has no size, so it doesn't refit (it would shrink
  the session for every other viewer).
- Keyboard: the bar is a vertical tab list (arrows, Home/End, roving
  focus); ⌘1 / ⌘2 switch from anywhere. A notification, a tray pick or a new
  workspace brings the Workspaces view to the front.
- The view in front is remembered per Mac in the webview's storage, like the
  theme: a view preference, not runtime state (invariant 11).
- The workspace sidebar narrows on small windows.
- UI tests (`npm test`, vitest in a simulated DOM with Tauri mocked) cover
  navigation, accessibility roles and that switching never unmounts a
  terminal; they run in CI.

## D-042 — Features: a product surface with its own contract (2026-10-09)

A **Feature** is a piece of product work — what should be built — as
opposed to a Workspace, which is where work runs. The Features view (D-041)
lists features with status filters (all, needs you, active, done, failed)
and a create action; a feature shows its conversation with the Control
Agent, a progress line (draft → planning → implementing → verifying →
review → done), and detail tabs: plan (request, requirements, acceptance
criteria, budget), tasks, approvals, evidence (tests, browser checks, CI,
pull requests) and its timeline.

- **The UI contract is typed first** (`apps/desktop/src/features.ts`), as
  the daemon will send it (snake_case, mirroring `otter_core`), and the view
  reads only through a `FeatureSource`. The first source is an in-memory
  mock with sample features in every state; the view says "Preview" while
  it is in use, and its replies say that nothing acts on them.
- **Linked by id, not embedded:** a task names the workspace and session it
  runs in; "Open session" brings that up in the Workspaces view. A
  workspace doesn't know about features.
- Needs-you for features: a pending decision, a blocked feature, or one
  ready for review. The Activity Bar shows a dot on Features for these.

## D-043 — Features are durable daemon state, with commands and history (2026-10-09)

Features (D-042) must outlive the desktop app — work goes on while the
laptop sleeps — so **each host's `otterd` owns its features**, and a feature
works in that host's workspaces. The app only reads and sends commands.

- **Kept apart from workspaces:** `state/features/<id>/feature.json` (the
  feature, rewritten atomically) and `history.jsonl` (append-only), under
  their own lock. A feature links to a workspace, task sessions and runs by
  id; `state.json` never mentions features and no workspace code knows them
  (invariants 5, 11 unchanged).
- **Messages, commands and events are different things.** Messages are the
  conversation (in the feature). Commands are what a client asks
  (`feature.create/send/act`), each with a client-chosen `command_id`.
  Events are what happened: the feature's history, one record per change
  with a per-feature `seq`, a timestamp, a schema version `v` and the
  correlation id of the command, decision or run behind it.
- **Applied once.** A change runs on a copy; the document is then saved
  with the command id in it (the last 256 per feature) before the history is
  appended. A repeated `command_id` returns the feature unchanged, so
  retrying after a lost reply is safe; a failed command isn't remembered. A
  crash between the save and the append loses history lines, never state
  (logged on load).
- **Replay:** every change emits `FeatureChanged {feature_id, history_seq,
  status}` on the host's event log, so the existing cursor replay
  (D-016/D-037) tells a reconnecting client what changed; the feature's own
  history (`feature.events {after}`) fills in the details. It is not rotated
  with the host log: a feature's history is bounded by the feature.
- **Schema versions:** `feature.json` has `schema`; loading migrates older
  documents (schema 1 is the first) and leaves a newer one on disk,
  untouched, for the otterd that wrote it.
- **The lifecycle** is a deterministic table (`FeatureStatus::can_become`):
  draft → planning → implementing → verifying → review → done; verifying and
  review may send work back to implementing; blocked, paused and failed are
  side exits that come back where the work left off (`resume_status`);
  cancelled and done are final. The developer's actions are applied by the
  daemon, not by a model.
- The desktop's Features view now reads from every connected host
  (`daemonSource`); an older `otterd` simply shows no features.

## D-044 — Managed agent runs: structured events, policy-checked approvals (2026-10-09)

Features need a coding agent the daemon can drive and observe without
reading a terminal. A **managed run** is a separate mode next to the
interactive agent sessions (which stay as they are).

- **Contract** (`runtime/mod.rs`): `AgentRuntime::start` (fresh, or resuming
  the agent's own conversation id) → a `RunHandle` with `send_input`,
  `next_event` (the event stream: session id, text, tool use, a decision
  needed, turn ended, exited), `decide`, `cancel`, `inspect`. Adapters map
  their tools to generic kinds (`ToolCall`: read, edit, command, fetch,
  question, plan approval, other).
- **Claude Code adapter** (`agents/claude_stream.rs`): `claude -p` with
  stream-json in and out, `--permission-mode default
  --permission-prompt-tool stdio --setting-sources ""`. Permission prompts,
  `AskUserQuestion` and plan approval come to Otter as `can_use_tool`
  control requests and are answered with allow / deny / an answer. The
  control messages are the Agent SDK's protocol, not a documented CLI
  contract: **observed on Claude Code 2.1.295** by probing the real CLI
  (allow, deny, a question answered, interrupt, `session_id` in `init` and
  `result`), and covered by a fake that plays the same protocol
  (`tests/fake_claude_stream.sh`). Without `--permission-prompt-tool stdio` a
  prompt is denied on the spot.
- **No settings files for managed runs** (`--setting-sources ""`): an allow
  rule in the user's or the project's settings would let a tool run before
  Otter's policy saw it, and a project's settings come with the repository.
  Claude Code's own read-only auto-allow (`echo`, `ls`) still applies.
- **Policy decides first** (`runtime/policy.rs`, deterministic): low risk
  inside the workspace runs (reads, edits under the root, build and test
  commands) and is recorded as decided by policy; medium risk (installs,
  fetches, unknown commands, questions, plans) is a decision the Control
  Agent may take; high risk — destructive, credentials, production or
  publishing, root, anything outside the workspace — is `user_only`, blocks
  the feature, and only the developer can allow it. `policy::resolve` is the
  single gate: a model's "allow" of a `user_only` request is refused, and a
  denial can't be turned around by anyone. Single words in the lists
  (`token`, `secret`, `prod`, `deploy`) match whole words only, so
  `cargo test tokenizer` or a search for "reproduce" stays routine.
- **Every decision is a `DecisionRequest`** in the feature (kind, risk,
  who decided, rationale), so the audit trail includes what policy allowed.
  A request's summary can contain a command line the agent proposed: it is
  kept in `feature.json` (0600), never in `events.jsonl`.
- **Lifetime — a deliberate deviation from invariant 4.** A managed run's
  protocol is its stdin/stdout, so it is a child of `otterd` and ends with
  it, unlike an interactive session in tmux. Its *conversation* survives:
  the run keeps the agent's session id, a new daemon marks interrupted runs
  (`recover_runs`) and the controller resumes them with `--resume`. Nothing
  else changes for sessions.
- **Handing over:** *Take over* stops the managed run (recorded as handed
  off) and opens the same conversation in an interactive agent session in
  the workspace (`SessionSpec.resume`); the feature pauses. *Hand back* is
  refused while that session still runs, so only one process ever drives a
  conversation; then the controller continues it. The session is created
  before the command is recorded, so a failed takeover can be retried.

## D-045 — The Control Agent: a deterministic engine that consults a model (2026-10-09)

A feature moves from request to review without the developer, but a model
decides only where judgment is needed. The engine (`controller.rs`) runs in
`otterd` — so it works with every client closed — woken by feature changes
and by a tick (default 5 s, `OTTER_CONTROLLER_TICK_MS`) that enforces time
limits.

- **Lifecycle** (D-043): planning → implementing → verifying → review,
  with blocked, paused, failed and cancelled to the side. One feature, one
  run at a time; features step concurrently, each never twice at once.
- **The brain** (`brain.rs`, `OTTER_CONTROLLER`) does three things: write
  the plan (requirements, checkable acceptance criteria, tasks with
  dependencies, one check command), decide what policy leaves open, and
  judge the criteria from the evidence. `claude` (default when Claude Code
  is installed) asks `claude -p --json-schema … --tools "" --setting-sources
  ""` — structured output, no tools, the developer's own Claude Code
  sign-in, nothing new to configure or store (`OTTER_CONTROLLER_MODEL` picks
  a model). `openrouter` uses any model on OpenRouter (`OTTER_CONTROLLER_MODEL`,
  default `openrouter/auto`) with `OPENROUTER_API_KEY` from otterd's
  environment — the key goes to `curl` in its config on stdin, never on a
  command line, in events or in a feature; it is the default when the key is
  set and Claude Code isn't installed. `rules` uses no model: one task, every open decision goes to the
  developer, criteria are met when the check passes. `yes` (tests only)
  approves everything, to show that policy still stops it.
- **Code, not the model, enforces:**
  - what may run: the check command goes through policy like any agent
    command (routine checks run; anything else becomes a decision); the
    brain's approvals go through `policy::resolve`, so a `user_only` request
    stays with the developer whatever the model says (it is recorded that
    the Control Agent wanted to allow it);
  - the budget: attempts (default 6 runs) and minutes of active work
    (default 120) per feature, at most 3 attempts per task, a per-run
    timeout (`OTTER_RUN_TIMEOUT_MS`, 30 min), and loop detection (the same
    failure 3 runs in a row since the work last started);
  - a failed check can't be judged away: if the latest deterministic check
    failed, no criterion counts as met.
- **Recovery:** the feature document is the checkpoint. A run cut off by a
  restart, a pause or a takeover resumes its conversation (`--resume`,
  "continue where you left off", plus new messages from the developer)
  without costing an attempt; a failed run starts over with the error in
  its prompt. Failing keeps the plan; *Retry* starts again from it with a
  fresh budget.
- **The developer can always step in:** pause, cancel, take over and hand
  back, decide any open decision, choose the workspace, accept or send back
  in review. Developer messages reach the coding agent with its next run
  (not mid-turn, in this version).
- **Audit:** the plan, every decision (with who decided and why), the
  rationale lines, evidence and the history.
- A feature needs a workspace on its host; without one it blocks with
  "Choose a workspace" (the app's New feature dialog offers them).
- Not yet: several runs in parallel, a model reply in the conversation, a
  plan approval step.

## D-046 — Browser verification: a real browser next to the work (2026-10-09)

The Control Agent checks a UI itself rather than trusting the coding
agent's account of it.

- **Checks are the project's**, in `.otter/browser-checks.json`: a preview
  command with `{port}` (also in `PORT`) and a ready path, then steps — go
  to, click, fill, expect text, expect visible, screenshot, wait. Declared,
  reviewable, deterministic; the model doesn't invent them.
- **Where:** on the host, next to the preview, so a remote workspace is
  verified without any tunnel. The browser is a headless Chrome/Chromium
  (`OTTER_BROWSER`, else found on PATH or in /Applications) driven over the
  DevTools protocol on `--remote-debugging-pipe` (fds 3 and 4): no
  debugging port to expose, no new dependency.
- **The preview** starts in its own process group on a free port, is
  polled until it answers, and is taken down (the whole group) after the
  check. Its command goes through policy like any other: a project file the
  coding agent can edit must not be a way around it. Not routine →
  a decision (asked once; approvals are remembered per command).
- **Isolation:** a throwaway profile per check (removed after), a fresh
  browser context, an environment with only `PATH` and a throwaway `HOME`
  (no workspace secrets), requests to anything but the preview's origin
  blocked (`Fetch`), downloads denied. `OTTER_BROWSER_NO_SANDBOX` (CI only)
  where Chrome's sandbox can't start.
- **Evidence:** per check, each step ✓/✗ with the reason, console errors
  and uncaught exceptions, failed and 4xx/5xx requests (a favicon aside),
  blocked requests, and screenshots kept with the feature
  (`state/features/<id>/artifacts/`, served by name with `feature.artifact`).
  A failed step, a console error or a failed request fails the check, and a
  failed check fails verification (the work goes back to implementing).
- **A model's look is supplementary:** the `claude` brain reviews the last
  screenshot (`claude -p` allowed to Read only the artifacts directory) and
  that evidence is marked uncertain; deterministic results outrank it.
- **The developer's preview:** *Preview* starts the same command as a
  service session in the workspace on a free port (one at a time; it shows
  in the Workspaces view and survives client disconnects). The app opens it
  by forwarding that one port from the host (the existing port mappings) —
  a scoped tunnel — or directly on this Mac.
- Not yet: checks proposed by the Control Agent, visual regression
  baselines, network mocking.

## D-047 — Delivery: a pull request, CI, and gates before done (2026-10-09)

A verified feature isn't done until it has been delivered and independently
checked. In review the controller (`review_step`, `delivery.rs`):

- **Publishes, with permission.** In a Git workspace with an `origin` and
  `gh` on the host, it asks to run `git push origin <branch>` and open a
  pull request — a `user_only` decision (high risk to policy), asked once
  per feature; later pushes of the same branch (after fixes) reuse it. A
  denial keeps delivery local (the PR and CI gates no longer apply). The PR
  description lists the criteria and the checks; bodies go over stdin.
- **GitHub through the host's `gh`**: its sign-in and scope; Otter stores
  no token and puts none on a command line. *Amended 2026-10-20:* once
  Otter's gh is signed in (D-133), its token is in Otter's own gh config
  directory and Otter's calls use that. Still never on a command line.
  Calls: `pr view/create/edit`,
  `pr checks --json`, `run view --log-failed`, `run rerun --failed`,
  `pr comment`. Another forge would implement the same few calls.
- **Follows CI** (every `OTTER_CI_POLL_MS`, 15 s; not within
  `OTTER_CI_SETTLE_MS`, 60 s, of a push, when what `gh` reports is still the
  previous commit's): a failure goes back to
  implementing as a "Fix what CI found" task with the failed log; a failure
  that reads like the infrastructure (a runner lost, a network error, a full
  disk) is rerun instead, at most twice per pushed commit. Budgets and loop
  detection (D-045) bound the round trips.
- **Gates** (deterministic, recomputed each step): requirements met (every
  criterion), local tests (the latest check), browser evidence (if the
  project declares checks), the pull request, CI (pending until checks
  report when the repository has workflows), and the developer's
  acceptance. *Accept* is refused until every other gate passed or doesn't
  apply; *Accept anyway* is the developer's explicit override and goes into
  the rationale and the report.
- **The report** (once every gate but acceptance is clear): changed files,
  criteria, gates, evidence (model judgments marked), decisions and who made
  them, and the unresolved risks. Kept on the feature and posted on the PR.
- **Never automatically:** merging and deploying. Otter has no merge or
  deploy call, and the coding agent's policy treats `gh pr merge`, deploys
  and publishing as the developer's.
- Tested end to end with a real Git remote and a fake `gh`: request →
  verified → publish approved → pushed and PR opened → CI fails → fixed →
  CI green → gates → report → accepted; a flaky failure is rerun; declining
  keeps it local. Not run against real GitHub here.

## D-048 — Host settings: the Control Agent's model and API keys (2026-10-09)

Choosing the Control Agent's model and giving it an API key shouldn't
require editing a host's shell profile and restarting `otterd`. Each host
keeps **settings** that a client sets:

- `state/settings.json`: the controller (`claude`, `openrouter`, `rules`,
  `off`; absent = automatic) and the model.
- `state/secrets.json` (0600): API keys, by known name only (for now
  `OPENROUTER_API_KEY`). **Write-only over the protocol**: `settings.get`
  says whether each key is set, never its value; `settings.set` sets or
  clears one (`null`). Never logged, never in `events.jsonl`, never on a
  command line (OpenRouter gets it through curl's config on stdin).
- Precedence: an explicit `OTTER_CONTROLLER` / `OTTER_CONTROLLER_MODEL` in
  otterd's environment wins (operators, tests — `settings.get` reports
  it); then the settings; a key in the environment is used when none is
  set here. A broken settings file leaves the daemon on defaults.
- `off`: the controller doesn't step features at all; they stay where they
  are until the developer acts. (Tests of the feature state machine use it.)
- In the app: a gear in the sidebar's title bar (Workspaces and Features)
  and *Control Agent and API keys…* in the Activity Bar's Settings menu open
  a dialog per connected host: the controller, the model, and each key as a
  password field showing only "set" / "not set", with Clear. Settings
  belong to a host because its daemon is what uses them.

## D-049 — Ordinary work runs; the Control Agent answers (2026-10-09)

Dogfooding the first features showed two problems: routine commands
(`npm test 2>&1 | tail -9`) waited for approval, and the developer's
messages ("what are you doing", "continue") got no answer.

- **Policy is allow-by-default** (revises D-044). Everything inside the
  workspace runs without asking: reading and writing files, building,
  testing, installing dependencies, the project's scripts, unknown
  commands, fetching docs. The developer is asked only for what is hard to
  undo or reaches beyond the workspace: deleting (`rm`, `rmdir`, `git
  clean`, `git reset --hard`, `git restore`, `-delete`, …), destroying
  resources (`delete`, `destroy`, `terminate`, `prune`, `uninstall`, `DROP
  TABLE`, `kubectl delete`, `docker rm`, …), credentials, pushing /
  publishing / deploying, `sudo`, edits outside the workspace. The Control
  Agent decides the coding agent's questions and plans and unknown tools.
  Words match whole words (`rm`, not `npm run rm-cache`); `_token`-style
  entries match word endings (`API_TOKEN`). The old "only known commands"
  list split `2>&1` at the `&` and flagged a stray `1`.
- **No stuck blocks.** When the Control Agent's answer to a decision comes
  back after the decision was settled (the run ended meanwhile), nothing is
  escalated; a feature blocked with nothing pending picks up its work.
- **The Control Agent answers every message** from the developer, with the
  feature's status, tasks and recent conversation, and — a model's
  judgment, never keyword matching — whether the message asks to go on
  (resume, unblock, start, retry) or to pause, which the controller then
  does. With no model configured it answers with the status and says it
  can't act on messages. A message that arrives while a run is working is
  the coding agent's next turn in that run, right after the current one.

## D-050 — A goal-driven loop, not a pipeline; no takeover (2026-10-09)

The lifecycle of D-043/D-045 read as a line (draft → planning →
implementing → verifying → review → done) and the app drew it as a
progress bar. Real work moves the goal: the developer hands over a task,
the Control Agent plans, implements and verifies as often as it takes, the
developer checks the result and either is satisfied, adjusts, or stops.

- **The loop is the Control Agent's business.** The app shows one state —
  Working (planning, implementing and verifying alike), Needs you, Ready to
  check, Done, Paused, Stopped, Cancelled — and a line on what is happening
  ("task 2 of 3: …"). No progress bar.
- **The goal can change at any time, even after done.** Saying so (or
  `request_changes {note}`) plans again from where the work stands: done
  tasks stay done, unfinished ones are replaced, a run in progress stops,
  the old report and gates are cleared. Planning is reachable from every
  state but cancelled; done is no longer final.
- **The developer's words decide** (a model's judgment, D-049): go on,
  pause, change the goal, or finish — accept what's ready (its gates still
  apply), or stop work in progress.
- **No takeover.** The developer doesn't drive the coding agent; they talk
  to the Control Agent. *Take over* / *Hand back* (D-044) are removed, from
  the app and the protocol. (Interactive agent sessions in workspaces are
  untouched, and `SessionSpec.resume` stays.)

## D-051 — Streaming text: transient events (2026-10-09)

The developer should watch the coding agent and the Control Agent write,
not wait for whole messages.

- **Where text comes from.** The coding agent runs with
  `--include-partial-messages`; Claude Code then sends `stream_event`
  `content_block_delta` / `text_delta` pieces before each whole assistant
  message (observed on 2.1.295). The Control Agent's reply is now plain
  text, streamed — OpenRouter's chat completions with `"stream": true`
  (server-sent events), or `claude -p --output-format stream-json
  --include-partial-messages` — ending in an `INTENT: …` line, the model's
  own judgment of what the developer asked (D-049), which is never shown.
  (Plans, decisions and judgments keep the structured, schema-checked
  calls.)
- **Transient events.** `FeatureStream {feature_id, stream_id, role, text,
  done}` carries the text so far, at most every 100 ms. It is sent to live
  subscribers only: never written to `events.jsonl`, never replayed, and
  stamped with the latest logged `seq` so it never moves a client's cursor
  (D-016, D-037 unchanged). A client that misses some loses nothing: the
  next one carries the whole text, and the finished message is stored in the
  feature as before (`done` says so). A lagging subscriber simply skips
  transient events.
- The app shows a message being written with a cursor, keeps the finished
  one until the feature reloads with it, and doesn't re-snapshot the
  workspaces for feature events.
- An exception to "events carry ids, not content" (design §19): stream
  text is conversation the developer is shown anyway, it is never
  persisted in the log, and it carries no secrets the feature doesn't.

## D-052 — Model providers for the Control Agent, and their model lists (2026-10-09)

The developer chooses the Control Agent's model from what a provider
actually offers, and isn't tied to OpenRouter.

- **Providers.** OpenRouter, Anthropic, OpenAI, Google Gemini, DeepSeek,
  xAI, Mistral and Groq, in one table (`providers.rs`): an id (the
  controller's name), the key's name (the secret, and the environment
  variable), a base URL, and how it's spoken to. All but Anthropic take
  OpenAI's chat completions (Gemini through its OpenAI-compatible
  endpoint); Anthropic takes its Messages API (`x-api-key`,
  `anthropic-version`, `output_config.format` with every object closed).
  Providers without JSON-schema output (DeepSeek, Groq) are asked for a
  JSON object, the schema in the prompt; answers are read leniently as
  before. The OpenAI-style schema request is no longer `strict`: the brain's
  schemas have optional properties and open objects, which strict mode
  rejects on OpenAI itself; Anthropic's closed copy is made for it. Streaming reads either kind of server-sent events. Keys still go
  to curl on stdin, never on a command line (D-045, D-048).
- **Automatic** is the first provider, in table order (OpenRouter first, as
  before), with a key in Settings or otterd's environment; else Claude Code;
  else rules. `settings.get` says which (`active`).
- **Models are listed by the host**, not the app: `settings.models` asks the
  provider's `/models` with the host's key — the key never leaves the host,
  and the list is what that key may use. Non-chat models (embeddings,
  speech, images) are left out. Claude Code's list is its model names. The
  app offers the list as suggestions on a free-text field: any name the
  provider accepts still works, and a provider that can't be reached
  doesn't stop anyone from typing one.
- **Defaults.** Only where a name is stable: `openrouter/auto`,
  `claude-opus-5-5`, `deepseek-chat`, `mistral-large-latest`. Elsewhere a
  model must be chosen; the app says so, and the brain fails with "choose a
  model … in Settings" rather than guess one.
- One `model` setting, for the controller chosen: switching provider in the
  app clears it.

## D-053 — The Control Agent is called Otter (2026-10-09)

"Control Agent" read as jargon next to "coding agent". Everything the
developer sees calls it **Otter**, after the product — like a tech lead, it
plans, directs the coding agent, decides what it may, and checks the
result; the coding agent stays "Coding agent", and the app's own notices
are "System". (Briefly "Lead"; one name for the product and the one you
talk to reads simpler.) Code, protocol and configuration keep their names
(`controller`, `MessageRole::Controller`, `OTTER_CONTROLLER`), so nothing
on the wire changes. Settings explains what Otter does, that it runs on
each host with that host's model and keys, and what each choice means;
the one way into Settings is the gear at the bottom of the Activity Bar.

## D-054 — Features can be deleted (2026-10-09)

Finished, cancelled and abandoned features piled up with no way out.
`feature.delete` removes one for good — its document, history and artifacts
under `state/features/<id>/` (renamed away, then removed, so a crash leaves
no half-feature) — and emits `FeatureDeleted`. Only a feature that isn't
being worked on: planning, implementing or verifying, or a run still going,
is refused until it is paused or cancelled, so nothing is pulled out from
under the controller or a coding agent. The workspace is never touched: it
is a separate resource with its own lifecycle (archive, delete). The app
asks first and says what goes and what stays. (No archive for features yet:
the filters already hide finished ones; add it if dogfooding asks.)

## D-055 — Runtime conversations: the coding agent's work gets its own identity (2026-10-09)

The structured Claude runtime plan (milestone 1) starts here. Today a
managed run is feature-owned and its events lose identity: streamed text
has no message or block id, tool calls have no id and no result, a turn
ends with a boolean, queued messages are joined into one, and cancel both
interrupts and kills. That is enough for one happy path, not for
reconnecting, redirecting or answering exactly the question asked.

- **A conversation** (`ConversationId`, `conv_`) is one coding-agent
  context in one workspace — Claude's native session behind an opaque
  binding that stays on the host (clients see `resumable`). It is made of
  **turns** (`TurnId`): one accepted input each, delivered one at a time,
  never merged; served by **runs** (`RunId`): one live process each, a
  restart is a new run and a new `generation` of the same conversation.
  What the agent says is **messages** of **blocks** (`BlockId`) whose ids
  are shared by the streamed and the finished text, so the finished text
  replaces the stream instead of being added again. What it does is **tool
  records** (`ToolCallId`) whose result is reported or explicitly
  `result_unavailable` — never assumed successful. What it asks is
  **interactions** (reusing `DecisionId`) that keep every question of a
  form with its options and take an answer per question.
- **Kept apart:** a turn `completed` (the provider finished it) is not a
  feature `done` (the controller accepts it, from evidence); an input
  accepted (a receipt) is not an input delivered (`queued → sending →
  delivered`, `unknown` when lost in between — never resent on its own).
- **The rules are pure** (`otter_core::conversation`): same command id and
  payload returns the original receipt, a different payload conflicts; a
  late event, interrupt or answer for an older generation, turn or
  interaction is `stale` and changes nothing; a conversation has at most
  one live run. Receipts say `durable: false` until the journal (M3).
- Features link to conversations (`Run.conversation_id`, `Run.turn_id`);
  the feature stays the owner of goals, tasks, gates and acceptance.
- Read-only protocol first: `runtime.capabilities`, `conversation.list`,
  `conversation.get`. Commands for clients come with durable receipts.
  The runtime contract, the actor and the feature bridge follow in the
  next steps; the legacy CLI adapter stays (`backend: legacy_cli`), and
  capabilities don't call the structured runtime ready.

## D-056 — Runs go through a conversation actor; turns are never merged (2026-10-09)

Milestone 1, second part: managed runs now serve conversations (D-055).

- **The runtime contract** (`runtime/mod.rs`) carries identities and
  limits: `RunSpec` has the conversation, run and generation, the coding
  model and provider limits; `RunHandle` has `send_turn`, `interrupt` (the
  current turn only — the process stays) and `stop` (end the process),
  replacing `send_input` and `cancel`, which did both. Events keep the
  provider's own keys for messages, blocks and tool calls; a turn ends with
  an outcome (`completed`, `interrupted`, `limit_reached`, `failed`, …) and
  an error category, not a boolean; usage says its scope (Claude Code's
  `total_cost_usd` is the session's so far, never summed). Capabilities are
  reported per runtime and say what is missing.
- **The actor** (`runtime/actor.rs`) owns a run's handle: it takes commands
  from a bounded mailbox and events from the runtime, applies both to the
  conversation with generations checked, and reports with Otter ids. It
  never touches features. **The bridge** (`runs.rs`) listens and is the
  only thing that changes the feature, so the feature store's lock is never
  held while the runtime waits.
- **One turn at a time, never merged.** A message the developer sends while
  a run works is its own turn (its command id is the message's), delivered
  after the current turn completes. Before, queued messages were joined
  into one prompt. A turn that doesn't complete (interrupted, failed,
  limits) ends the run, and turns still queued end `cancelled` with why —
  the controller's next prompt carries the conversation forward.
- **Identity across runs.** Continuing a task (after a restart or a pause)
  is a new run, a new generation, of the task's conversation; a fresh
  attempt is a new conversation. A run from before conversations brings its
  session id as the binding (its own record — never "the latest session in
  this directory").
- **The legacy adapter** maps what Claude Code's stream-json gives: message
  ids from `message_start`, block indexes from deltas, tool-call ids and
  results (`tool_result` in `user` messages), the session id. Streamed and
  whole text share an id, so the finished text replaces the stream. A
  subagent's traffic is still left out (its lineage comes with the SDK).
  `Task`/`Agent` stay classified as reads and one answer still goes to
  every question of a form: both change with the controls milestone.
- **Not durable yet.** A conversation is a snapshot rewritten after each
  change; a daemon restart ends its live run (outcome unknown, delivery
  unknown if it was being sent — never resent) and cancels queued turns.
  The journal, replay and receipts that survive a restart are milestone 3.
- Clients read `runtime.capabilities`, `conversation.list`,
  `conversation.get`, and follow `ConversationChanged` (ids only). The
  desktop doesn't use them yet.

## D-057 — The coding agent through the Claude Agent SDK, in a worker (2026-10-10)

Milestone 2 of the structured runtime plan. `legacy_cli` drives Claude
Code's stream-json control protocol, which is observed, not documented. The
`sdk` backend drives Claude through the official Claude Agent SDK instead,
from a small Node worker (`packages/claude-runtime`) that otterd starts per
run and speaks versioned JSONL with (protocol 1). Rust never copies the
SDK's internals; the worker never decides policy.

- **Pinned:** `@anthropic-ai/claude-agent-sdk` 0.3.285 (locked) and the
  Claude Code it bundles, 2.1.285 — never whichever `claude` is on PATH.
  Node 22. The compatibility record and what was observed live are in the
  package's README.
- **Auth:** the host's Claude Code sign-in. Anthropic asks third-party
  products not to offer claude.ai login for SDK apps without approval; the
  developer chose it for their own hosts (the alternative is an API key).
  No credential manager: Otter never sees or stores it. (Since D-142, a
  token the developer made with `claude setup-token` may be kept in
  Settings instead.)
- **Handshake:** nothing reaches Claude before the worker's `ready` (protocol
  checked, SDK initialized), within 90 s (`OTTER_WORKER_INIT_TIMEOUT_MS`).
  The session id arrives with the first turn.
- **Policy is mandatory:** a `PreToolUse` hook asks otterd about every tool
  call (`ToolCheck`); D-049's policy answers allow (routine work, no
  record), deny (recorded) or ask — and "ask" comes back through
  `canUseTool` as a decision, answered per request id. A cancelled wait is
  a denial. Subagents (`Task`, `Agent`) are disallowed for now.
- **Project instructions:** `settingSources: ["project"]` so CLAUDE.md is
  read; Otter's hook runs before any project rule (and Claude Code ignores
  allow-rules of an untrusted workspace).
- **Questions:** forms keep every question; answers go back per question
  (`answers` in a feature's `decide`, comma-joined for multi-select). One
  `answer` for a form still goes to each question — the forms UI comes with
  the conversation UI.
- **Settings:** the coding agent's backend and model are their own settings
  (`coding`), apart from Otter's model; `OTTER_CODING_BACKEND` overrides.
  The default stays `legacy_cli` until the release work; a conversation
  keeps the backend it began with.
- **Bounded and drained:** frames are at most 1 MiB (an oversized one is
  dropped and reported), stderr is drained and its tail kept for errors,
  a worker that doesn't leave on shutdown is killed.
- Interrupts: an aborted result while an interrupt was asked is
  `interrupted`, whatever its subtype; a turn that completed first stays
  completed. Live-checked on both hosts (16/16).

## D-058 — Conversations are kept as journals (2026-10-10)

Milestone 3 of the structured runtime plan: a runtime conversation (D-055)
survives a restart with everything it accepted and everything that
happened, and says plainly what a crash cut off.

- **Ops.** Every change is an `Op` carrying its own ids and times
  (`otter_core::conversation`); applying the same ops to the same start
  gives the same conversation. Nothing changes a conversation any other way.
- **The journal is the authority.** `state/conversations/<id>/journal.jsonl`
  holds one record per change (`{log_id, seq, schema, at, recovery?, kind:
  create | op}`), `manifest.json` its schema and log id, `snapshot.json` a
  checkpoint every 64 records (written to a temporary file, synced,
  renamed, the directory synced — and disposable: a broken one is rebuilt
  from the journal). Files are this user's only.
- **Commit order:** check the change on a copy → append the record →
  `File::sync_all` (on macOS, Rust makes that `F_FULLFSYNC`) → apply in
  memory → tell clients (`ConversationChanged`) → act on it (the runtime is
  sent a turn only after `Sending` is recorded). A change that can't be
  recorded didn't happen; the conversation turns read-only (clients see
  `read_only` with why), and a run that can't record what it does stops
  rather than going on unrecorded. Fsync blocks the calling thread (a few
  milliseconds per change); each conversation has its own lock.
- **Load:** snapshot, then the records after it. A last line cut short by a
  crash is cut off; anything wrong before it — unreadable, out of order,
  not applying — opens the conversation read-only, never with records
  skipped. A conversation from before the journal (a snapshot only) starts
  its journal with it, keeping the old file as `snapshot.v1.json`; an older
  otterd can't read a journaled conversation's snapshot and skips it rather
  than overwrite it.
- **Recovery is recorded.** A run still live in the journal belonged to the
  daemon before this one: its end (`outcome_unknown`; a turn sent but never
  confirmed is `delivery: unknown`, never resent) is journaled as recovery
  records, so a second crash keeps it. Receipts are durable: a command
  retried after a restart gets its receipt back, never a second turn.
- **Queued turns at a restart** end `cancelled` ("carried into the
  continuation"): the controller's continuation restates every developer
  message since the run began, so the guidance arrives exactly once that
  way. Keeping them queued needs the controller to know which messages a
  turn already carries — that comes with the controls milestone.
- **Text being written** is checkpointed (a `WriteBlock` that isn't done)
  every second or 8 KiB; after a crash the last checkpoint shows as an
  interrupted message, never as a finished one. Live updates stay transient
  (D-051).
- `conversation.history {conversation, after?, limit?}` pages through the
  journal (100 records by default, about 1 MiB at most); a cursor is
  `{log_id, seq}`, and one from another journal is `cursor_expired`.
- Found on the way: when otterd itself stops, a run ending with it was
  recorded as failed (an attempt spent) if the bridge saw it first. It is
  now left for the next otterd to mark interrupted and resume.
- **Features follow the journal.** At a restart, a feature run whose last
  turn the journal shows completed (the old daemon died before telling the
  feature) is recorded completed, with what the agent said last; any other
  live run is interrupted and resumable, as before.
- **A run's processes end with it.** Each run starts in a process group of
  its own (`process_group(0)`); stopping it waits for its agent to close its
  output, then ends the group — before the leader is reaped, while the
  group's id can't be anyone else's — so a background process it left (a
  server, a long `sleep`) doesn't outlive it. Dropping a run (otterd going
  away) does the same. The journal records each generation's process (pid
  and start time); a restarted daemon ends a recorded process that is still
  running — the same one by its start time, never just a reused pid. (A
  daemon killed outright can still leave what the agent's process had
  started if that process died first; nothing claims otherwise.)
- **The workspace is where it really is.** Policy (D-049) compares paths
  with symlinks resolved on both sides: a workspace reached through a
  symlink (macOS's `/var` is `/private/var`) is still the workspace, and a
  symlink inside it pointing elsewhere is not. Found by the live check.
- **Live-checked** (`live_sdk_restart`, real Claude through the SDK worker,
  macOS): otterd restarted while a tool ran; the cut-off turn is
  `outcome_unknown` with `delivery: delivered`; the run resumed as
  generation 2 of the same conversation and native session; the feature
  reached review with the work done (turns: unknown, completed).

## D-059 — Controls, typed interactions and a closer policy (2026-10-10)

Milestone 4 of the structured runtime plan. In three steps; this is the
first.

**Policy reads every piece of a command line.** D-049's lists matched the
whole line; a write outside the workspace by redirection or by a plain
`cp` went unnoticed, and so did a symlink out of the workspace. Now:

- A line is split at `;`, `&&`, `||`, `|`, `&` and newlines; what `$(…)`
  and backticks run is read as a command too. The most severe piece
  decides. `2>&1`, `>&2` and writes to `/dev/null`, `/dev/stdout`,
  `/dev/stderr`, `/dev/tty` are not file writes.
- A write to a file outside the workspace asks the developer: `>`/`>>`
  (any descriptor), `cp`/`mv`/`ln`/`install`/`rsync` to an outside
  destination, `tee`/`touch`/`mkdir`/`truncate`/`chmod`/`chown` of an
  outside path, `dd of=`. `~` is home; a `cd` earlier in the line moves
  where later relative paths point. Reading from outside is fine.
- Paths are compared where they really are: symlinks resolved on both sides
  (the deepest existing part of a path, then the rest), so a workspace
  reached through a symlink is the workspace, and a symlink inside it
  pointing elsewhere is not.
- This is still a lexical reading for usability, not a sandbox: quotes,
  variables and what a script does inside aren't understood, and a
  determined command can hide what it does. A variable target (`> $OUT`)
  isn't judged. Ordinary work keeps running without asking (`npm test 2>&1
  | tail -9`, builds, writes inside the workspace).

**Subagents are off for now.** `Task`/`Agent` were classified as reads.
They are a `Delegate` call now, which policy denies until a subagent's
tools, lineage and policy are handled. The sdk backend doesn't offer them
at all (`disallowedTools`); the legacy backend only hears about the tool
calls Claude Code asks permission for, which its capabilities now say.

**Controls go through the conversation.** Pause, cancel, interrupt and
redirect are commands on the coding agent's conversation, applied by its
actor in order and journaled (D-058):

- **Interrupt** stops the current turn only; the run doesn't go on by
  itself. As a feature action it also pauses the feature, so nothing
  restarts until the developer says so; the conversation stays open.
- **Redirect** is guidance that can't wait: the current turn is
  interrupted and the redirect turn goes next — before anything else
  queued — in the same run. A retried redirect (same command id) is applied
  once. As a feature action it is the developer's message, recorded in the
  feature too; without a run to redirect, it is refused (send a message).
- **Pause** is a barrier: the turn is interrupted, nothing more is
  delivered until the conversation is resumed, and the run ends. **Cancel**
  closes the conversation: queued turns are cancelled, no new ones. Both
  end the feature's run as cancelled ("Paused", "Cancelled"), as before.
- An interrupt must settle within 10 s (`OTTER_INTERRUPT_DEADLINE_MS`);
  otherwise the run is stopped and what its turn did is unknown. A turn that
  completes as the interrupt arrives stays completed; a late interrupt of it
  is stale.
- A turn that ends interrupted or cancelled ends its run as cancelled,
  not failed: no attempt is spent, and the task can pick up again.

**Otter decides what reaches the coding agent.** Every developer message
used to be forwarded to a running agent before Otter had read it — a
question about status became an instruction. Now Otter's reply judges it
(D-049's intent line, two more intents): `guide` — instructions for the
current work — is queued as a turn of its own, with the message's id as
its command id; `redirect` — it can't wait — interrupts the current turn
and goes next; a question or chat (`none`) is answered and not passed on;
`revise`, `continue`, `pause`, `finish` act on the feature as before.
Without a model, a message is guidance, as it always was.

**Queued guidance waits, and arrives once.** This supersedes D-058's queue
rule (queued turns ended at a restart, "carried into the continuation"):

- A turn carries its message's id. The controller's continuation prompt
  leaves out the messages a turn of the conversation already carries —
  queued, sent or delivered — and so does its guidance (a redirect's own
  message, answered afterwards, isn't queued again). That is what makes
  keeping the queue safe.
- So queued turns now wait for the next run: across a pause (the
  conversation is held, the run ends, resuming resumes the conversation
  first), across an interrupt, and across an otterd restart. They are
  cancelled only when the run failed or hit a limit, when it was stopped
  for good (a changed goal — the replan restates the task — or a
  timeout), or when the conversation is cancelled.
- Live-checked (`live_sdk_controls`, real Claude through the SDK worker):
  paused mid-tool, resumed as generation 2 of the same native session,
  redirected mid-tool; the redirect ran next in the same run, once, and the
  feature reached review.
- Capabilities now say `pause` and `redirect`.

## D-060 — The Claude worker ships with each release; Node is the host's (2026-10-10)

Milestone 6 of the structured runtime plan, first step: a host gets the
`sdk` backend (D-057) from `otter host install`, like `otterd` itself.

- **Node is the host's.** The worker needs Node.js 20 or newer (`engines`;
  the SDK asks for 18, it's tested on 22). Otter doesn't ship or download a
  Node: where there is none, or an older one, the runtime says so and the
  `sdk` backend isn't available; terminal sessions and `legacy_cli` are
  unaffected. Revisit if hosts commonly lack Node — distributing a runtime
  needs its licensing and platforms checked first.
- **One worker per platform.** The SDK carries Claude Code as a native
  binary per platform (about 200 MB), so each release has
  `otter-claude-runtime-<version>-<platform>.tar.gz` beside the binaries:
  the compiled worker, its locked production dependencies, and the
  Claude Code for that platform only — the glibc build on Linux (musl
  hosts aren't supported). About 100 MB compressed. Its `.sha256` is
  published with it.
- **Installed by release, next to otterd.** `otter host install` downloads
  it, checks the SHA-256, unpacks it into a staging directory, asks it
  `--check` when Node is on PATH, and only then renames it to
  `~/.local/lib/otter/claude-runtime/<version>`. A directory per release:
  an upgrade never replaces files a running worker uses. The release
  before stays; older ones are removed. Already installed: nothing is
  downloaded. Any failure is a note, never a failed install. Nothing is
  fetched when a run starts, so starting works offline (Claude itself
  still needs the network).
- **otterd uses its own release's worker:** `OTTER_CLAUDE_WORKER`, else
  `../lib/otter/claude-runtime/<its version>/dist/main.js` (then the
  unversioned development layouts).
- **`main.js --check`** prints `{protocol_version, worker_version,
  sdk_version, node_version, node_supported, claude_code}` and exits
  non-zero if it can't run here; it starts nothing and reads no
  credentials. `runtime.capabilities` is `available` only when Node, the
  worker and its check all pass, and its notes say which didn't (an old
  Node, another protocol, no Claude Code for this platform). Sign-in isn't
  checked ahead: a missing one fails the first run as `auth_required`.
- **Each run says what it ran on.** A run's `Claim` records the worker,
  SDK and Node versions (`runtime`); a conversation shows the latest as
  `runtime_version`, and its journal keeps each generation's.
- **Still opt-in.** The default backend stays `legacy_cli`: `sdk` is chosen
  per host in Settings (or `OTTER_CODING_BACKEND`). It becomes the default
  for fresh managed runs only after dogfooding (plan stages: development
  flag → explicitly enabled host → dogfood → default). Switching never
  moves a running conversation: one keeps the backend it began with.
  *Amended 2026-10-19:* `sdk` is the default since D-124, at the
  developer's direction, before the dogfooding finished.


## D-061 — Run states say what a crash left; unsafe recovery is the developer's (2026-10-13)

Roadmap M1 (runtime reliability). An audit against the milestone found the
journal (D-058) and controls (D-059) already ordered, idempotent and
recovered, but the feature's run record blurred what happened: a run ended
by an otterd restart was `cancelled` (like a pause), and a run whose agent
process died mid-tool was `failed` — an attempt spent, and the next run
started the task over from scratch, possibly repeating the cut-off step.

- **Two new run states**, both endings: `interrupted` (ended with its
  process while its turn was unsettled; resumable) and `recovery_required`
  (Otter can't safely continue on its own). `RunState::can_become` makes the
  rest explicit: `starting → running ⇄ waiting`, then one ending, after
  which a run never changes; the daemon refuses other moves.
- **A lost agent process isn't a failure.** When the actor ends with
  `outcome_unknown` (the process died, or the journal couldn't be written),
  the run is classified like a run cut off by a restart (`runs::unsettled`):
  resumable with a session id; started again only if nothing had reached
  the agent; otherwise `recovery_required` — also when the conversation's
  journal can't be trusted, and the third time in a row a task's run is cut
  off (no endless resume loop).
- **The resumed agent is told.** Continuing an `interrupted` run, the
  prompt says the last run was cut off and the result of what it was doing
  is unknown: check whether it took effect before doing it again. Otter
  never resends the turn and never calls the step a success; it can't know
  more than that.
- **Recovery is a decision.** `recovery_required` blocks the task and the
  feature and opens a `recovery` decision ("Start “…” over?"), high risk and
  the developer's only: the Control Agent escalates it without asking a
  model, and it survives restarts (it isn't tied to a live run). Approved,
  the task starts over as a fresh run (an attempt). Declined, the task is
  pending and the feature paused; resuming starts it over. Using the
  decision machinery keeps the audit trail, idempotency by `command_id` and
  every client's existing UI.
- **Compatibility.** New enum values in an existing field: clients that
  match on them should treat an unknown run state as over and an unknown
  decision kind as approve/deny. An older otterd can't parse a feature that
  has them and skips it, leaving the file untouched (as with a newer
  schema); no schema bump.
- The state table, crash matrix and an operator's guide are in
  `runtime-states.md`.

## D-062 — Verification can finish (2026-10-14)

M2 step 1 (`docs/plans/m2-feature-to-pr.md`; autonomy audit P0-1, RC-1 and
RC-4). Correct work used to end *Failed, all 6 attempts are used up*: a
check that couldn't run was a failing check, every failure a new fix task,
and the judge saw the check's output and the agent's claims but never the
work. Principles 3 ("the supervisor verifies with deterministic evidence")
and 4 ("stop on evidence, not on attempt counts") of the plan, in part:

- **A check that can't run is the environment's problem.** Exit 126/127, or
  the shell's, `make`'s, `env`'s or npm's own complaint in the output (a
  command not found, no such file, not executable, a missing npm script),
  is recorded as evidence with no verdict, titled "`<cmd>` can't run here".
  `make` reports a missing command as its own exit 2, so the output counts;
  a test's output that merely mentions a missing file doesn't (narrower
  than the plan's `No such file or directory` anywhere). No fix task: the
  feature blocks with that line as the reason, one message, and a
  developer-only question — "Run it again" or "Go on without it". A
  blocked feature with nothing pending is picked up again by the
  controller, so the question is also what keeps it blocked. Going on
  without the check lasts until the next plan, which proposes a check
  again.
- **The judge sees the work.** Each verification reads the workspace's
  state (`work_state.rs`): in Git, HEAD, `git status`, the stat and a
  bounded diff since the merge base with the work's base branch (else
  HEAD) — committed or not — and new files as diffs against nothing,
  without touching the index (no `git add -N`). It is evidence ("Working
  tree: …", no verdict); the judge is told the diff and status are
  deterministic and a check's output proves only what it ran, and sees
  only the newest snapshot. The snapshot is kept in the feature and read
  by the brain's model, which may not be the coding agent's: files that
  may hold secrets by their name (`.env`, `.env.*`, `.netrc`, `.npmrc`,
  `.pypirc`, `credentials`, SSH keys, `*.pem`, `*.key`, `*.p12`, `*.pfx`,
  `*.keystore`) are listed as changed but their content is never shown.
  That's the obvious shapes, not a scanner: a token in an ordinary tracked
  file is shown like any other change. Outside Git there is no diff to
  show, only a fingerprint of the files (path, size, modification time;
  hidden, `node_modules`, `target` and `__pycache__` directories left out).
- **Typed criteria.** A criterion says what proves it (`how`): `check` —
  the check command passing, decided in code from its result whenever the
  check runs — or `diff` — what the change contains, judged by the brain
  (the default, and what untyped model output and older documents read
  as). The brain is asked only about the criteria the check doesn't
  decide, and not at all when there are none. The plan prompt forbids
  criteria about the conversation or the process. Without a model (`rules`)
  every criterion still follows the check, as before.
- **Stop on evidence.** A verification that finds something unmet records
  a fingerprint of the work (the snapshot's, plus each deterministic
  result) and the unmet criteria (`Feature.last_verification`, persisted:
  the feature document is the checkpoint). The same fingerprint and the
  same unmet criteria after a fix means the fix changed nothing: no new
  fix task. It goes to **Review**, not Failed, with "Not verified: nothing
  changed after a fix" and the ✓/✗ list. That trades a little safety for
  completion: the deterministic check still decides its criteria, the
  "Requirements met" gate shows what isn't proven, and Done still needs
  the developer's acceptance (or their override, on record). Planning
  again clears the mark. The attempt and time budgets and
  `repeating_failure` stay, as backstops. *(Revised by D-165: the same
  work after a fix with no fewer unmet criteria stops, whatever the
  verdict says; only a verdict that proves more on the same work gets
  another round.)*
- **Developer-only stays developer-only** (plan principle 6, pulled ahead
  of step 5 because the question above relies on it): the brain isn't
  asked about a `user_only` decision at all, so a model can neither
  approve one (policy already refused that) nor deny one in the
  developer's name (it could). It goes straight to the developer.

## D-063 — A run's time is the agent's work (2026-10-14)

M2 step 2 (`docs/plans/m2-feature-to-pr.md`; autonomy audit P0-3, RC-3;
plan principle 5). A run had 30 minutes of wall clock from its start, so a
run that waited on the developer for an approval was killed right after
they approved (audit E1, twice in the real feature), and running out of time
failed the run and started the task over in a new session. Steps 2–8 are
built on `main`: M1's runtime work (`roadmap-m1` @ `5ebcf4f`, D-061 "an
interrupted run resumes its session, with a cut-off notice") isn't
published, so this decision builds the notice itself.

- **The clock runs while the agent works.** A run keeps `worked_ms` and
  `working_since`: the clock stops when the run waits on a decision
  (`RunState::Waiting`) and at its end, and runs again once every decision
  it waited on is made — whoever made it. That also puts the run back to
  Running: nothing used to, so a run that had asked once stayed Waiting.
  The per-run limit (`OTTER_RUN_TIMEOUT_MS`, 30 min) and the feature's time
  budget (`max_minutes`, 120 — "minutes of active work" in D-045, now
  literally) count this time: the runs' work since the work last started.
  Checks, model calls and waiting on the developer don't.
- **Sleep isn't work.** The controller compares the wall clock with the
  monotonic one, which stands still while the host sleeps; a gap of more
  than 30 s moves the working runs' current stretch on by that much.
- **No-progress watchdog.** A run whose agent has said and done nothing for
  `OTTER_RUN_IDLE_MS` (10 min) while none of its tools runs is cut off too.
  A running tool isn't silence: builds and tests take long without a word.
  The runtime actor keeps the pulse (last event, tools in flight) in
  memory; a restart ends the run anyway.
- **Out of time means cut off and resumed, not failed.** The turn is
  interrupted, the run ends Cancelled ("Out of time after 30 min of work",
  "No progress for 10 min"), and the controller resumes the conversation
  as it does after a restart or a pause — no attempt charged — opening with
  "You ran out of time on “<task>” (…). Continue where you left off: do
  what the task still needs first, then end your turn with a short
  report." Any other cut-off turn opens with "Your last turn was
  interrupted before it finished." The feature's time budget bounds the
  resumes; the attempt budget still counts fresh starts.

## D-064 — One conversation per feature (2026-10-14)

M2 step 3 (`docs/plans/m2-feature-to-pr.md`; autonomy audit P0-2, RC-2;
plan principle 2). Every task, every fix and every retry used to start a
new agent session: the benchmark's features took 6 sessions, the real one
10, and each began cold — re-reading the code, re-deriving what the last
session knew.

- **The feature's conversation goes on.** The next task, a fix that
  verification asked for and a retry after a failed run are new turns in
  the session the feature's work began in (`--resume` with the latest
  bound session id, in the same Otter conversation: one journal for the
  feature). A turn of the same task that was cut off — a restart, a
  pause, a limit (D-063), a takeover — carries on with "continue"; anything
  else gets the task's prompt. A new session starts only when there is
  none yet, when the agent reports that the session is gone (its turn
  failed with `resume_unavailable`), or when the conversation was closed.
  CI failures as turns come with step 7.
- **A warm prompt doesn't repeat itself.** In the feature's conversation a
  task's prompt leaves out the request and requirements (the first turn
  said them), keeps the criteria, the task, what went wrong last time and
  the check, and carries only developer messages the conversation hasn't:
  not ones a turn already delivered, nor ones from before the previous
  turn began (its prompt had them), nor ones the task's own text says.
- **Attempts:** a turn that starts new work (a task's first turn, a fix, a
  retry) costs one, as a fresh run did; carrying on after a cut-off
  doesn't. The plan asked for per-feature accounting by "consecutive turns
  without a state change"; that is D-062's loop stop (same work, same
  verdict → review), so the attempt budget and the three attempts per task
  stay as they were, as backstops.
- The "never two drivers" check covers every task's interactive session,
  since they share one conversation; the agent's instructions say it works
  on a feature, a task per turn.
- **The agent that can see the code plans** (plan principle 1). Planning
  starts with a task of its own, "Look at the code and plan the work"
  (`Task.planning`): the feature's first turn asks the coding agent to read
  the code the request touches and end with a JSON proposal — tasks,
  criteria with `how`, the check command — changing nothing yet. Its
  report is Otter's to read, not a message in the feature's conversation.
  The brain's plan is then a review of the proposal (the model is told to
  keep what is right, tighten criteria and fix the check), no longer a
  guess from the root path and the brief; without a model the proposal is
  the plan, its gaps filled as `rules` would. No proposal (prose, a failed
  turn) → planning goes on as before, without it. The tasks follow as
  turns in the same conversation, so the first one doesn't repeat the
  request. Planning costs no attempt. Planning again (a changed goal)
  doesn't ask again: the session has the code in mind, and the brain
  plans from what the developer said.
- Gaps, for later: "don't edit files" is an instruction, not enforced (a
  run has no read-only mode yet); a developer message sent during the
  planning turn becomes the run's next turn, whose reply then stands in
  for the proposal (planning falls back). `OTTER_PLAN_TURN=0` turns the
  planning turn off; the e2e tests do, except the ones about it.

## D-065 — A turn waits for the agent's background work (2026-10-14)

M2 step 4 (`docs/plans/m2-feature-to-pr.md`; autonomy audit P1-4, RC-8).
A run ended at the agent's first `result`: the worker was stopped and its
process group killed, so an agent that said "the tests run in the
background; I'll wait for them" was cut off, and its task recorded done
(audit R-4, `run_5e6e0ssfr0`).

- **Runtimes report background work.** Claude Code's
  `system/background_tasks_changed` (the whole set, each time it changes)
  becomes `RuntimeEvent::Background { running }` — directly in the
  stream-json parser, through a new `background` frame from the SDK worker.
  Ambient tasks (watchers, housekeeping) don't count. A `result` Claude Code
  produces in a turn it started itself (when background work ends) is
  reported as the last turn Otter sent, not dropped.
- **The actor holds the turn.** A turn that completes while background
  work runs stays open: no next turn is delivered, the run isn't stopped,
  and what the agent says and does when the work ends belongs to it. It
  finishes with the agent's next result once nothing runs in the
  background; or, if the agent says nothing more within
  `OTTER_BACKGROUND_SETTLE_MS` (5 s) of the work ending, as it was; or
  after `OTTER_BACKGROUND_WAIT_MS` (20 min) whatever runs. Interrupting,
  pausing or redirecting a held turn ends it at once as interrupted.
  Runtime-neutral, in the actor, so both Claude backends get it.
- **Background work is work under way.** It counts like a running tool for
  the no-progress watchdog (D-063), and the run's work clock keeps
  running: a run that never goes quiet is still bounded by its limit.
- **An interrupt spares background work** (`perTaskStopAffordance: true`
  on the SDK backend): it stops the agent's turn, not a server or build
  it started on purpose. Ending the run still ends its process group (the
  run's processes don't outlive it, D-058).
- The SDK stays at 0.3.285: it already has the task events and the
  affordance. Not yet probed live (recorded as pending in
  `runtime-validation.md`).

## D-066 — Approvals without noise (2026-10-14)

M2 step 5 (`docs/plans/m2-feature-to-pr.md`; autonomy audit P1-1, P1-2,
P1-5; RC-5, RC-7, RC-9). In the real feature, 16 developer-only asks were
all "high risk" and most weren't: `cat .envrc` (the `.env` rule matched a
substring), build logs redirected to `/tmp`, `rm` of a file the agent had
just made. Identical asks came back seconds apart, and the SDK's own tools
each cost a model decision. The rules' intent was right; the matching was
lexical (audit 03 §3).

- **Secrets files by name.** `.env` and `.env.<anything>` are credentials
  by file name — an operand of a command, an edited path — not wherever
  the letters appear. `.envrc` (direnv's config) and the templates
  (`.env.example`, `.sample`, `.template`, `.dist`, `.defaults`, `.schema`)
  aren't.
- **Scratch space is inside the boundary.** Writing (or deleting) under
  `/tmp`, `/private/tmp`, `/var/tmp` or `$TMPDIR` is routine, the temp
  directory itself excepted. `/dev/zero` joins the streams a redirection
  may name. `$HOME`, system paths and other directories still ask.
- **Deleting the work's own files is routine.** `rm`, `rmdir` and `unlink`
  of paths inside the workspace (or scratch) run without asking — not the
  workspace itself, not `.git`, not a glob at its top (`rm -rf *`), not
  anything outside. The word lists are read piece by piece; only what no
  one may allow (a pipe into a shell) is read across the whole line.
- **Allowed once, allowed for the feature.** An ask that policy raised only
  for where it reaches — a command whose one risk is writing outside the
  workspace, an edit outside it (by directory), an MCP tool — carries a
  grant key (`DecisionRequest.grant`). When the developer approves it, the
  key is kept on the feature (`Feature.grants`), and the same thing later
  in the feature runs without asking, recorded as decided by policy
  ("you allowed this earlier in this feature"). Deleting, credentials,
  pushing and deploying, and root are never granted: asked every time.
  Kept per feature rather than returned to Claude Code as
  `updatedPermissions`, so it works the same on both backends and is in
  the record.
- **Who said no.** A denial reaches the agent as "The developer declined
  this.", "Otter declined this: <why>" or "Otter's policy doesn't allow
  this: <why>" — not always as the developer's. `escalate` blocks only if
  the decision is still pending (no Blocked/unblocked flicker). The brain
  never sees a developer-only decision (D-062).
- **Every built-in tool has a kind.** The agent's own session — its task
  list, background work, plan mode, MCP resources — reads as `Read` (runs);
  `Monitor` runs its command as a `Command`; `Workflow` delegates (off, like
  subagents); tools that reach beyond the run — scheduling (`CronCreate`,
  `ScheduleWakeup`, `RemoteTrigger`), notifying (`PushNotification`,
  `SendFeedback`), other worktrees, the host app's own tools — are a new
  `ToolCall::Outside`, which policy denies: Otter schedules work and tells
  the developer. Only MCP tools are left to the Control Agent.
  `packages/claude-runtime/tools.json` lists the pinned SDK's tools: the
  worker's test fails when the SDK has one the list doesn't, otterd's when
  one in the list has no kind.
- Replayed (unit test): of the dogfooding record's developer-only asks,
  only the write to Claude's memory and `git push` still ask. The fake
  agent's developer-only request is now `rm -rf /opt/shared`: the tests'
  workspaces live in the temp directory, which is scratch.

## D-067 — A feature's request is the developer's word (2026-10-14)

M2 step 6 (`docs/plans/m2-feature-to-pr.md`; autonomy audit P1-3, RC-6).
The "Otter" feature arrived with no request text, and its goal was in the
developer's first message — which Otter then skipped everywhere as "the
request itself" (`skip(1)`): planning had nothing to go on, and the plan
told the agent to confirm the goal with the developer.

- **The request message is found, not assumed.** `Feature::request_message`
  is the developer's first message when its text is the request — what a
  feature created with a request starts with. Developer notes, the
  messages Otter answers, the brief the brain reads and a replan's notes
  leave out that message only; with an empty request, every message counts.
- **No request: the first message is it.** Starting a feature with an
  empty request plans from the developer's first message, which becomes
  the request. With none, Otter asks once — a developer-only question,
  "What should this feature do?", and a message saying they can answer
  there or just write — and the feature waits, blocked on that question
  (which is what keeps it blocked). A plain message answers it. Planning
  doesn't start without a goal.
- **The plan never sends the agent to the developer.** The brain's plan
  prompt and the agent's planning turn say the agent works on its own: no
  task asks, checks with or waits for the developer; Otter asks them when
  something needs them (the agent's own blocking questions still reach
  them, as decisions).
- Not done: a non-blocking question tool (the agent asks and keeps
  working, the answer arrives as input). The plan made it optional,
  behind a decision; blocking `AskUserQuestion` stays as it is.

## D-068 — The golden path's checkpoints (2026-10-14)

M2 step 7 (`docs/plans/m2-feature-to-pr.md`; the M2 spec: plan → implement
→ test → review → PR → CI → done with durable checkpoints, deterministic
controls, bounded retries). Most of it was there — the feature document is
the checkpoint (D-045), a push is `HEAD:refs/heads/<branch>`, an open pull
request for the branch is adopted (`gh pr view`, then `edit`), and since
D-064 a CI fix is a turn in the feature's session. What wasn't:

- **Nothing committed the agent's work.** Delivery pushed HEAD; work the
  agent left uncommitted never reached the pull request. Publishing — the
  action the developer allows — now commits it first (`git add -A`, the
  feature's title as the message), leaving out files that may hold secrets
  by name (the same list as D-062's snapshot); the commit's SHA is evidence
  ("Committed …") and the note names what was left out. Without an
  identity of the host's own, the commit is Otter's. *Amended 2026-10-20:*
  an identity set in Otter (D-133) is the repository's own, so the
  commit is the developer's. A tree with work
  still to commit counts as unpublished, so a fix the agent didn't commit
  is pushed too. Idempotent by construction: after a crash between commit
  and push, the tree is clean and the retry only pushes.
- **The same CI failure after a fix goes to the developer.** CI failing
  with the same checks and the same log as the last failure, once a "Fix
  what CI found" turn has run, doesn't start another fix: one message
  ("CI failed the same way after the fix, so another attempt won't help:
  over to you"), the feature stays in review with the CI gate failed. The
  flaky-rerun budget (D-047) stays.
- **Cancelling says what stays.** Otter removes nothing of the work on
  cancel: the message names the workspace's files, the branch and an open
  pull request (to close on the forge if unwanted), and that removing the
  workspace is the developer's step.
- **Checkpoints, tested by stopping there.** otterd stopped and started
  again while the developer is asked to publish, right after the pull
  request opens, and once the report is written: one run, one pull
  request (`gh pr create` once), one branch, then done. An open pull
  request is adopted, not opened twice.
- **The contract, written down:** `docs/feature-states.md` — the states,
  each transition and who owns it (the developer, or the engine; a model's
  output is only ever an input), what each checkpoint keeps and what a
  resume does with it, the controls, the budgets.
- **From the terminal:** `otter feature accept [--anyway]` (there was no
  way to finish a feature from the CLI). The CLI's end-to-end test runs the
  golden path through `otter` — new, start, decide (publish), accept — in
  a disposable repository with a fake agent and a fake `gh`: a pull
  request with the agent's uncommitted work, CI green, done. With live
  Claude and real GitHub it is recorded as pending (`runtime-validation.md`
  M2-6/M2-7).
- **The default backend stays `legacy_cli`** (audit P1-6). M2's changes
  live in the runtime actor, policy and controller, so both backends have
  them: the work clock and watchdog, one conversation per feature, the
  planning turn, background work, approvals, the checkpoints. Only two
  pieces are the SDK's alone: every tool call checked by policy before it
  runs (legacy sees only what Claude Code asks about) and an interrupt that
  spares background work. D-060's path to making `sdk` the default — after
  dogfooding — stands; M2's live scenarios are that dogfooding's checklist.
  *Amended 2026-10-19:* the default became `sdk` in D-124.

## D-069 — Report once (2026-10-14)

M2 step 8 (`docs/plans/m2-feature-to-pr.md`; autonomy audit P2-1…P2-5,
RC-10; plan principle 7). The dogfooding feature's main column held 69
items: every text block the agent wrote, then its result repeating them;
83 "Decision needed (tool_permission, low risk)" / "approved by policy"
records; and the feature document rewritten (and fsynced) for every tool
the agent used — on the path that answers the agent's policy check.

- **One agent message per turn.** What the agent says along the way
  streams to the app (`FeatureStream`, transient, D-051) and is kept in its
  conversation's journal (the Work tab); the feature keeps each turn's
  result — its summary, else its last text — once. A result that repeats
  the agent's previous message word for word isn't stored again. A
  planning turn's report isn't a message at all (D-064).
- **Activity is transient.** The lines of what the agent did are kept in
  memory with the live run and sent as `FeatureActivity` events; reads of
  the feature show them; they are written into the run when it ends. No
  rewrite of `feature.json` per tool, and no reload of every feature in the
  app for each one.
- **What policy allows is a tool, not a decision.** On `legacy_cli`,
  Claude Code asks about routine tools too; those policy allows used to be
  recorded as decisions with two history events each. They're the tool
  records in the conversation now. Denials, asks, and approvals by an
  earlier grant (D-066) are still decisions.
- `escalate` re-checking that a decision is still pending (P2-4) came with
  D-066.

## D-070 — M3's scope, derived without its spec (2026-10-14)

The roadmap takes each milestone's scope and acceptance from its spec file;
`03-agent-manager-ux.md` isn't on this machine, and the roadmap says later
milestones don't ship ahead of the P0 gates (M1 unpublished, M2's live
scenarios pending). The requester, told so, asked to go on. So M3 starts,
with its scope derived and said to be: from the exit outcome ("parallel
work is understandable and actionable from the desktop inbox"), design §21
and §26, the autonomy audit's attention findings (05), and an audit of the
code — `docs/plans/m3-agent-manager-ux.md`, which marks what is already met
and where each gap comes from. It stays on the read side: no daemon state,
no change to how work runs, no new requests where a derived value does.
When the spec arrives it wins, and the plan is redone against it.

## D-071 — One rule for what a feature needs (2026-10-14)

M3 step 1 (`docs/plans/m3-agent-manager-ux.md`, gaps 2 and 4). A
workspace's activity is computed by `otter-core` and served to the app
(`view.rs`); a feature's "needs you" was the app's own TypeScript, and
left out a stopped feature, which waits on the developer's retry
(audit 05 §4). Features also sorted by their last update.

- **`Feature::needs()`** in `otter-core`: what the feature waits on the
  developer for — the oldest pending decision (a question, or a decision
  to allow or deny; with how many wait), else review once it's ready to
  accept (the report is written) or a gate failed, else blocked, else
  stopped — with a line saying what, and since when: the decision's time,
  or `status_since`, a new field every transition sets (older documents
  fall back to their last update). Review while publishing or waiting for
  CI isn't waiting on the developer.
- **`Feature::activity()`** puts features on the workspaces' scale: needs
  you, failed, working, completed (done), idle (draft, paused), archived
  (cancelled).
- **Served, not re-implemented.** The app's feature calls return each
  feature with both computed (`FeatureView`, flattened, in
  `src-tauri/src/features.rs`, as `view.rs` does for workspaces);
  `features.ts` reads them, keeping a stand-in only for preview data.
  Features that need the developer sort longest-waiting first.

## D-072 — The Inbox (2026-10-14)

M3 step 2 (`docs/plans/m3-agent-manager-ux.md`, gaps 1 and 3). Features and
workspaces were separate views with separate lists; acting on a feature's
decision meant opening it, then its Approvals tab.

- **One list, first in the activity bar** (⌘1; Features ⌘2, Workspaces ⌘3;
  the Inbox is where the app opens the first time, after that the last
  view): NEEDS YOU, WORKING, DONE — design §26 — across features and
  workspaces on every connected host. Needs: longest-waiting first
  (`Feature::needs` and a workspace's oldest attention); working and done:
  newest first, done capped at eight. Drafts, paused and idle work, and
  what's archived, aren't in it; the other views have them.
- **A row says what it is and what's going on** — feature or workspace,
  host, how long — and what waits: the decision's summary, the stop's
  reason, the workspace's headline.
- **What waits is done there.** A feature's oldest pending decision is the
  same card as in its Approvals tab (approve, deny, an option, an answer),
  with how many more wait; review offers Accept (refused while a gate
  waits — the error shows on the row); a stopped feature offers Retry;
  every row opens its feature, or its workspace at the session that needs
  the developer. Nothing new on the daemon: the same `feature.act`.
- The Inbox's badge counts what needs the developer: features, and
  workspaces that need them or failed.

## D-073 — Told when a feature needs you (2026-10-14)

M3 step 3 (`docs/plans/m3-agent-manager-ux.md`, gaps 5 and 6). A feature
needing the developer only changed a badge count: no notification, nothing
in the menu-bar item, and `otter ls` listed workspaces alone.

- **A notification per new need**, once: a decision, review ready, a
  block, a stop (`Feature::needs`, D-071). A need is new when it differs
  from the last one the app saw for that feature; the first time the app
  sees a feature is the baseline, so what waited before it looked isn't
  announced. Not while the Inbox is in front of the developer.
  Activating the app soon after opens that feature, as for workspaces.
- **The menu-bar item lists features with workspaces** (longest-waiting
  first, "N things need you"); picking one opens it.
- **`otter ls` lists features that wait on the developer** under NEEDS
  YOU or FAILED, longest-waiting first with the workspaces, as "feature ·
  <what>" and since when; `--json` gives them per host with `activity` and
  `needs`. An otterd without features shows none.

## D-074 — M4's scope, derived without its spec (2026-10-14)

As for M3 (D-070): the milestone's spec isn't on this machine and the
requester asked to go on, so M4 (*failures can be diagnosed without
exposing secrets or reading raw terminal output*) starts from a scope
derived from the exit outcome, the design's rules on secrets and an audit
of the code — `docs/plans/m4-observability.md`, which marks what is already
met (the env file, events without commands or keys, write-only API keys,
git's redaction) and six gaps: failure text that reaches durable state
unredacted, why a process ended living only in its pane, error categories
without next steps, a failed workspace's reason missing from the app, no
way to diagnose a host, and a feature's failure not traceable from the
terminal. When the spec arrives it wins.

## D-075 — Secrets stay out of what Otter keeps and shows (2026-10-14)

M4 step 1 (`docs/plans/m4-observability.md`, gap 1). Environments and API
keys were already kept out (the env file, write-only keys), and Git's
stderr lost URL credentials; but what other tools printed when they failed
became Otter's as is: direnv's stderr (an `.envrc` exports secrets) in a
workspace's state and events, tmux's in a launch error, the Claude
worker's in a turn's reason — then the run's summary, the task's last
error, a message in the feature — and a coding agent's tool output, words
and errors in its conversation's journal.

- **One redactor, `otter_core::redact`**, by shape: credentials in URLs
  (`scheme://user:pass@`), `Bearer`/`Basic` values, `key=value` and
  `key: value` whose key names a secret (`…token`, `…secret`, `password`,
  `api_key`, `access_key`, `private_key`, `credential`, …, also quoted in
  JSON and in query strings), well-known key shapes (GitHub `ghp_`/`gho_`/
  `github_pat_`…, GitLab, Anthropic `sk-ant-`, OpenAI `sk-`, Slack `xox?-`,
  AWS `AKIA`/`ASIA`, Google `AIza`), and private-key blocks. A secret in
  no known shape passes: shapes, not a scanner. Idempotent.
- **Applied where outside text becomes Otter's:** Git, direnv and tmux
  stderr; the Claude worker's diagnostics; every runtime event the actor
  journals (agent text, tool results, decision summaries, turn results,
  exit errors — tool calls stay whole for policy, and are journaled by a
  redacted summary); what the feature keeps (run summaries, last errors,
  messages, activity, decision summaries) and streams; a workspace's
  failure and environment messages.
- **A repository URL** is kept as given in the workspace's record — Git
  needs it, and keeps it in the repository's own config anyway — and every
  answer (`workspace.list/get/create/…`) and `otter ws show` gives it
  without credentials.
- **Invariant 13** says so, with its tests: the e2e test sends a token
  through a coding agent's tool output, its words, its error and an
  unclonable repository URL, then searches every answer and every file
  Otter keeps.

## D-076 — Failures say why and what next (2026-10-14)

M4 step 2 (`docs/plans/m4-observability.md`, gaps 2, 3, 4 and 6). Why a
process ended was only in its tmux pane — released on restart, gone for a
lost execution; a coding agent's error was a bare category in the CLI; a
failed workspace's reason was missing from the app; a feature's history
had no CLI.

- **A process's last lines are kept with it.** When reconciliation finds a
  session's process dead, it reads the pane's last lines while tmux still
  holds them and keeps them on the execution (`Execution.tail`: twenty
  non-empty lines at most, 2,000 characters, redacted). The failure's
  attention line ends with the last of them ("deploy failed (status 3):
  error: config/prod.toml not found"); `otter ws show` prints them for a
  session that didn't exit cleanly, the app under the failure banner. They
  stay after otterd restarts. A program `internal-exec` couldn't start
  says so there too ("executing …: No such file or directory"). A process
  seen dead is taken as ended 300 ms later (the next pass): tmux can mark
  a pane dead before it has drawn the last lines — the ones that say why
  (CI caught it keeping the line before the error).
- **Every error category explains itself**, in `otter-core`
  (`ErrorCategory::explain`: what happened and what to do — sign in, wait
  for the rate limit, `otter host install`, …). Conversation views carry
  the explanations for the errors their turns hit (`errors`, by category),
  so the CLI and the app say the same; the app shows the next step under
  the turn.
- **A failed workspace shows its reason** in the app, above its sessions.
- **`otter feature history <feature>`** prints a feature's history (`-n`
  for the last lines, `--json`); `otter feature show` gives a failed
  task's last error.

## D-077 — `otter doctor`: what's wrong with a host (2026-10-14)

M4 step 3 (`docs/plans/m4-observability.md`, gap 5). What went wrong with
a host as otterd started — settings it couldn't read, conversations it
couldn't load, a login environment it couldn't capture, tmux missing — and
since — reconciliation failing — was a warning in otterd's log, which
nothing showed and nothing rotated; runtime readiness was a separate
command; the app's host page showed neither.

- **A host says what's wrong with it:** `HostStatus.problems`, each a line
  on what's wrong and one on what to do. otterd notes what it meets
  (settings, conversations, a failing reconciliation — cleared when it
  passes again) and adds what it sees now: tmux or Git missing, a login
  environment that fell back to otterd's own, a coding runtime that isn't
  ready (with its own notes on why), conversations that can't be written
  any more. `HostStatus.log` says where otterd's log is.
- **`otter doctor [host]`** prints each host's problems with what to do,
  counts a host it can't reach as a problem (with how to check it), and
  exits 1 when there's any (`--json` too). The app's host page lists them
  under Problems.
- **otterd's log rotates:** over 10 MiB, it's set aside (`workd.log.1`)
  as otterd starts.

## D-078 — M5's scope, derived without its spec (2026-10-14)

As for M3 and M4 (D-070, D-074): the spec isn't on this machine and the
requester asked to go on. M5 (*several coding agents coordinate without
corrupting shared work or losing human control*) is read narrowly, from
the outcome, the M2 plan's rule against parallel work in a shared
directory, and an audit of the code (`docs/plans/m5-orchestration.md`):
agents already run side by side, one per feature; nothing stops two of
them working in one directory, nothing offers a way out, and nothing
holds them all at once. So: one driver per workspace, a worktree of its
own for a feature whose workspace is taken, and a pause for everything.
Parallel tasks inside one feature (with merging) go against D-064 and have
no evidence yet; they wait for the spec.

## D-079 — One driver per workspace (2026-10-14)

M5 step 1 (`docs/plans/m5-orchestration.md`, gap 1). Nothing stopped two
features' coding agents working in one directory at once — editing the
same files, and delivery pushing one branch with both features' work —
nor a feature's agent working next to one the developer started there.

- **A workspace has one managed run at a time.** A run claims its
  workspace as it starts (atomically: a claim while it starts, then the
  live run itself) and holds it until it ends. A second feature's turn
  doesn't start there: the feature keeps its status and says why —
  "Waiting for <workspace>: “<feature>” is working there" — once, on the
  feature and in its rationale; it isn't blocked (nothing waits on the
  developer) and it costs no attempt. When the run ends, the controller is
  woken and the waiting feature starts by itself; the reason goes. Not
  first-come-first-served across several waiting features: whichever steps
  first after the release.
- **The developer's own agents count.** A run doesn't start in a workspace
  where an agent session the developer started is starting, working or
  waiting on them; the feature waits the same way.
- Not done: telling the developer when they start an agent session in a
  workspace a run is working in.

## D-080 — A feature can have a workspace of its own (2026-10-14)

M5 step 2 (`docs/plans/m5-orchestration.md`, gap 2). With one driver per
workspace (D-079), a second feature on a busy workspace waits; to run side
by side the developer had to make another workspace and move it by hand.

- **`own_workspace`** (`otter feature own-workspace`, the app's waiting
  banner): otterd makes a worktree of the feature's Git workspace's
  repository — named after both (`<workspace>-<feature>`), on a branch for
  the feature (`otter/<feature>-<id end>`), from the same base — and the
  feature moves there; it waits while that workspace is prepared, then
  works. Both features' agents then run at once, each in its own
  directory, and each delivers its own branch.
- **Only before it has worked**: a feature that has run already has its
  work in the workspace it has; moving it would leave that behind.
- A directory workspace can't be split this way (it's not a Git
  repository Otter manages): its features wait their turn.
- A feature waiting for any workspace being prepared says so ("it's being
  prepared"), instead of failing to start.

## D-081 — One control for every agent at work (2026-10-14)

M5 step 3 (`docs/plans/m5-orchestration.md`, gap 3). Pause and cancel were
per feature; with several agents at work there was no single control.

- **`otter feature pause --all [--host h]`** holds every feature at work
  (planning, implementing, verifying, or blocked) on every host — each as a
  pause does: its turn interrupted, its conversation held — and
  `otter feature resume --all` lets every paused one go on. One line per
  feature; a host it can't reach, or a feature that can't be held, is said
  and makes the command fail.
- **The Inbox's WORKING group has "Pause all features"**, the same for
  what the app shows.

## D-082 — M6's scope, derived without its spec (2026-10-14)

As for M3–M5: the spec isn't on this machine and the requester asked to go
on. M6 (*controlled unattended execution: triggers, policies,
idempotency*) is read from the outcome, the M2 plan's "no bypass mode for
unattended work — that's M6, with its own policy", D-066 (what may be
granted at all) and an audit of the code (`docs/plans/m6-automation.md`):
feature creation and delivery are already idempotent, budgets bound work,
publishing can be allowed per feature; nothing starts work by itself,
nothing sets an unattended policy, and unattended work would wait for
ever on its first developer-only ask.

## D-083 — Triggers: unattended work under a policy set in advance (2026-10-14)

M6 (`docs/plans/m6-automation.md`).

- **Triggers live on the host** (`state/triggers.json`, 0600): a name, a
  workspace, the feature to start (title, request), a schedule — every N
  minutes, daily at HH:MM UTC, or by hand only — and a policy. otterd
  fires due ones (every 15 s it looks), and `otter trigger run` fires one
  by hand. `otter trigger add / ls / rm / run`; `trigger.*` requests.
- **One feature per occurrence.** An occurrence's feature is created and
  started with command ids made of the trigger and the occurrence (to the
  millisecond), so firing it again — a retry, a restart, a script that
  names its occurrence (`--occurrence`) — finds the same feature. Missed
  occurrences (otterd was down) fire once, the latest; an occurrence whose
  trigger's last feature is still at work (active or blocked) is skipped,
  and the trigger's history says why.
- **The policy is the developer's, given in advance** — never a model's in
  the moment, never a bypass. Budget caps (attempts, minutes of work) for
  its features; `publish`: pushing the workspace's branch and opening the
  pull request (what D-047 otherwise asks for) is allowed, recorded on the
  feature as an allowed command; a wait limit: a developer-only decision
  that waits longer pauses the feature — its agent stopped, the decision
  set aside, "Waited N min for you on “…”: paused, nothing decided" — and
  the Inbox counts it as waiting on the developer; resuming asks again.
  Everything else policy asks the developer, it still asks them.
- **On record:** a triggered feature carries `started_by` (the trigger, the
  occurrence, the policy as it was).
- An allowance decision that was set aside (cancelled) is asked again,
  rather than read as a denial.
- Not done (for the spec): webhooks or CI events as triggers (a script can
  `otter trigger run`), cron expressions, cross-host triggers,
  notifications beyond the app's.


## D-084 — The coding agent answers or plans; the brain reviews, it doesn't rewrite (2026-10-14)

Dogfooding: "告诉我现在 backport tag 是怎么工作的" — how does the backport
tag work? — became "Plan: 1 task(s). 1. Add a minimal test case", checked
with `npm test`. Every path out of planning needed at least one task
(`parse_proposal` dropped a proposal without one, `Model::plan` failed on
an empty plan, `rules` always made one); the planning turn's words were
never shown; and when they weren't a plan, the brain — which can't see
the code — planned from the brief anyway, and in any case rewrote the
agent's plan. A question had no way out but invented work. This revises
D-064's planning.

- **The agent that reads the code decides what kind of request it is.**
  The planning turn says: a question, an explanation, an investigation —
  answer it, pointing to the code, changing nothing and adding no JSON; a
  change — plan it, as before. Otter only reads what came back: a JSON
  plan with tasks is a plan; anything else is the answer. The answer is
  posted as the agent's message and the feature is **done**
  (`planning → done`, "Answered: no change was asked for"): no tasks, no
  check, no attempt, no pull request. Failing toward showing, not toward
  inventing.
- **No blind plan when the look didn't work.** A planning turn that fails,
  or ends with nothing, fails the feature with why; going on (or Retry)
  looks again. `brain.plan` — planning from the brief alone — is left for
  `OTTER_PLAN_TURN=0` only.
- **Planning again is the agent's turn again.** A changed goal (D-050)
  makes the planning turn due again, in the feature's conversation: the
  request isn't restated, what the developer said since and what is done
  are (D-064 had the brain plan a replan, blind). The feature stays
  planning until a plan is set — resumed, retried or unblocked — and only
  a feature with work planned is "revised". Asking for more on a done
  feature (guidance, go on) plans again: after an answer, "then change it"
  works. Without a model every message reads as guidance, so on a done
  feature any message, a "thanks" too, starts a look at the code.
- **The brain reviews the plan; it doesn't write it.** `review_plan` asks
  only what can be told without the code: does it do what was asked (no
  more, no less), are the criteria checkable facts, does any task send the
  agent to the developer. Its points go back to the agent once, as a turn
  in the same conversation (the agent may keep its plan and say why);
  found again, the developer decides — a developer-only plan approval,
  with the plan and the points, before anything is set. Approve: the
  agent's plan as it is; turn it down: the agent plans again with the
  points; a message is what to change (going on approves). A review that
  can't be had leaves the agent's plan standing. Without a model there is
  no review: the agent's plan, its gaps filled, is the plan.
- The plan's announcement counts the work, not the look at the code.
- Not done: the planning turn's text still isn't streamed while it is
  written (an answer arrives whole); an answer's `path:line` references
  aren't checked; the judge still sees the diff but not the code around
  it; questions in the middle of a feature are still answered by the
  brain, which can't see the code.
## D-085 — The app carries its command-line tools; they're links (2026-10-14)

Dogfooding: every app update was followed by "Update command line tools",
which downloaded the same release's `otter`/`otterd` again (D-020), and
until then this Mac's host ran the old `otterd` (its `otterd_path` is
`~/.local/bin/otterd`). Now, as VS Code does with `code`:

- **The `.app` carries them.** `bundle.externalBin` puts `otter` and
  `otterd` in `Otter.app/Contents/MacOS`, built from the main workspace by
  `apps/desktop/scripts/sidecars.mjs` (Tauri's before-dev/before-build
  command, for its target and profile) and signed with the app. A release
  doesn't build them twice: building them in the desktop job put 2–3
  minutes of compiling in front of the app's own. Instead it compiles the
  app without them (`--no-bundle`, no `externalBin`), meanwhile the
  `binaries` job publishes them to the draft release, and `tauri bundle`
  takes them from there — so the app carries the same build remote hosts
  install. The release checks they're there and, where its runner can run
  them (arm64), that they say its version.
- **`~/.local/bin` holds links to them.** "Install command line tools"
  makes `otter` and `otterd` links into the app (replacing whatever is
  there — the developer asked). At every launch the app re-points links
  that lead into an Otter app, so an update — or the app moved — carries
  them along; a missing one or a binary put there by hand is left alone.
  Nothing is linked from a development build, a disk image or a
  translocated copy, which a link would outlive. A deleted app leaves
  dangling links, as `code` does.
- **This Mac's host installs from the app.** Install/update for a local
  host whose `otterd_path` is `~/.local/bin/otterd`: link, then the release
  script without its binaries part — only the Claude worker (D-060) is
  downloaded, once per release — then stop the daemon as before. Restarting
  the daemon stays the developer's button (it ends in-flight runs, D-061);
  the app never restarts it on its own at launch.
- **Scripted installs replace links, never write through them.** The
  release script removes `~/.local/bin/otter{,d}` before installing, so
  `otter host install` on this Mac can't modify the app's signed binaries.
- **The worker stays a download** (100 MB per platform, needs the host's
  Node; D-060). `otterd` run through its link finds it at
  `~/.local/bin/../lib/otter/claude-runtime/<version>` as before (macOS
  keeps the link's path as the executable's); `otterd` run from inside the
  app also looks in `~/.local/lib/otter/claude-runtime/<version>`.
- Next, separately: the worker is versioned by otter's version, so each
  release downloads it again even when it didn't change. Versioning it by
  its content would make most updates download nothing.

## D-086 — The coding agent's words reach the developer as written (2026-10-14)

Dogfooding: asked how a repository's backport label works, the coding agent
wrote a good answer in Markdown — headings, numbered steps, code, line
references — and the conversation showed one paragraph, cut off at 2000
characters ("如果改了 DB…"). The turn's final reply went through
`agents::excerpt`, which makes a status line: it joins every line into one
and cuts at its limit. The conversation journal had the whole reply; the
run's summary, and the message made from it, didn't.

- **A turn's final reply is a report, not an excerpt.** Both backends keep
  its lines and Markdown, trimmed, and cut only past 20 000 characters —
  what a feature message may hold (`agents::report`/`REPORT_LEN` in otterd,
  `REPORT_LEN` in the Claude worker). `excerpt` stays for what is one line:
  tool summaries, activity, attention, a task's `last_error` (it is quoted
  in reasons and prompts).
- **The app renders what Otter and the coding agent write as Markdown**
  (GitHub's flavour: lists, code, tables), and the final report too. The
  developer's own messages stay as typed. Raw HTML in the text is skipped,
  an image is its description (nothing is fetched on a model's say-so), and
  only http(s) links are links, opened in the browser, never in the app.
- Adds `react-markdown` and `remark-gfm` to the desktop app.

## D-087 — A stand-in defers once; tests keep off the host's openers (2026-10-14)

Dogfooding: Ctrl+V stopped pasting images into Claude Code on a host, and
the browser-login e2e tests hung there — and, run while Portkeeper's Mac
side was up, opened GitHub's sign-in page on the developer's Mac. The host
still had Portkeeper's stand-ins (D-031's predecessor) in `~/.local/bin`,
and `~/.zshenv` put that directory first in every zsh, ahead of otterd's
`run/bin`. Claude Code's `wl-paste` was Portkeeper's, which gives up
without a Wayland display; and when otterd's `xdg-open` deferred to "the
next one on PATH" — Portkeeper's — that one deferred back to the first
other one it found, otterd's: the two called each other for ever.

- **Deferring happens once.** A stand-in (`wl-paste`, `xdg-open`,
  `www-browser`, `otter-open`) marks the environment when it hands over
  (`OTTER_STANDIN_DEFERRED`); one called with the mark exits 1. Another
  tool's stand-in can no longer loop with otterd's.
- **The browser-login tests keep off the test machine's openers.** Their
  session's PATH is otterd's stand-ins, then a host `xdg-open` of the
  test's — one that records the URL and hands straight back, as
  Portkeeper's did — then the system's; no desktop variables. What the
  host's opener was asked to open is asserted: once, and only when no
  client served the page.
- Unchanged, and the limit D-031 states: a profile that puts its own
  directories ahead of `run/bin` can put another tool's `wl-paste` first,
  and the paste never reaches otterd's. On the dogfooding host,
  Portkeeper's stand-ins were removed. `otter attach` doesn't forward
  images (the app does).

## D-088 — The app calls them tasks (2026-10-14)

Dogfooding: what the app called a feature is often a bug fix or just a
question to answer. The desktop app now says **task** wherever it showed
"feature": the activity bar and its view, New task, the Inbox tag, the
empty states, Settings. Only the words the developer reads change: the
model, the protocol (`feature.*`), `otter feature`, the daemon's own
messages and these docs keep "feature".

- A task's plan is made of **steps** (the tab, "1/2 steps", "step 2 of
  2: …"), so the two don't share a name.
- The session kind `task` (a command run to completion) is shown as
  **Command** in New session, for the same reason.
- The message box under a task's conversation starts taller (96 px) and
  its top edge is a handle: drag it, or ↑/↓ on it, between 56 px and 60%
  of the window; a double-click goes back to the default. Its height is
  remembered on this Mac.

## D-089 — Otter answers with tools; authority is the developer's message (2026-10-14)

Plan: [`docs/plans/brain-runtime-v2.md`](plans/brain-runtime-v2.md).
Dogfooding: asked how a repository's backport tag works (D-084), Otter
could only route the message — one model call that sorts it into one of
seven intents (`INTENT:`), with the feature's brief, status and last twelve
messages, and no way to look at a run's events, its decisions or its
evidence. D-084 left questions in the middle of a feature "answered by the
brain, which can't see the code", and D-049 began with "what are you doing"
getting no answer. The transcripts the plan's M0 asks for are still to be
recorded; this decision rests on those two.

- **Otter's reply becomes a bounded, tool-using turn**, where `converse`
  runs now: inside the feature's step, with a hard deadline, and exactly
  one reply per developer message — on a failure too, or the step would
  answer again on every wake. Its tools call the controller's in-process
  functions (`apply_action`, `replan`, `tell_runs`, `redirect_runs`,
  `hold_runs`, `decide`), never a request; a gateway in otterd validates
  them, keeps them to the host and redacts what they return (D-075).
- **Authority is the developer's message.** What Otter does on the
  developer's word — forward, redirect, start, resume, approve the plan,
  accept, retry, revise, create — exists only in a turn their message
  started, and carries its id: today's trust (D-050's "ship it", D-084's
  "going on approves"), made explicit, not widened. The plan approval stays
  the one developer-only decision a message can settle. A turn an event
  started only looks and pauses; unattended work stays with triggers
  (D-083).
- **The developer's words reach the agent as written.** Forwarding takes
  message ids, never text Otter wrote; nothing of Otter's goes through
  `feature.send` or a `request_changes` note, which would read as theirs.
- **Code questions are the coding agent's**, as a read-only turn that
  policy enforces — edits and commands denied, not asked nicely — on the
  SDK runtime only (its hook sees every tool call); elsewhere Otter says it
  can't, rather than ask unenforced.
- **The Claude worker first.** Every other brain — API providers, `claude
  -p` without the worker, `rules` — keeps today's behaviour until it has
  tool calling; `otter doctor` says which a host has. It's "Otter" in the
  app (D-053), "brain" in the code.
- Not changed: D-088 (no internal rename, `feature.*` stays), host scope,
  the controller's authority, the stores. Not built: a scheduler, a new
  store, several tasks run to compare, anything across hosts, a model
  cancel or delete.

## D-090 — Otter's tools are a host setting, off until tried out (2026-10-15)

Plan `brain-runtime-v2` M2 had Otter's tool-using turn (D-089) become the
default for Claude once M1 passed. Two things stand in the way, so the
default waits — a deliberate deviation from the plan:

- **No evidence yet.** M2's exit measures it against dogfooding
  transcripts (`runtime-validation.md` V2-0c), and none exist: only the
  developer, at work on real tasks, can make them. D-089 itself rests on
  dogfooding, and changing every Claude user's reply before any comes in
  would invert that.
- **A worse reply while it's written.** The turn's reply doesn't stream yet
  (D-051): it arrives whole, where `converse`'s is written as it comes.

What lands instead:

- **A setting per host:** *Otter's tools (preview)* in Settings
  (`otter_tools` in `settings.get`/`settings.set`), off by default, kept
  in the host's `settings.json`. `OTTER_BRAIN_V2` in otterd's environment
  overrides it (`1` on, `0` off), as `OTTER_CONTROLLER` does its setting.
  Settings and `otter doctor` say how Otter answers on the host now ("with
  tools (claude)", "without tools (off in Settings)", or why not).
- `settings.set` answers as `settings.get` does (what runs now, how Otter
  answers), not with the bare stored view.
- The default changes in a decision of its own, after streaming and the
  transcripts.

## D-091 — A question for the agent at work is a turn that only reads (2026-10-15)

Plan `brain-runtime-v2` M2.5, for D-084's gap: a question in the middle of
a feature was answered by the brain, which can't see the code.

- **`ask_agent`**, one of Otter's tools on the developer's word: their
  message goes to the coding agent at work as a turn of its own, after its
  current one (D-056), in which it may **only read**. In that turn Otter's
  policy reads, fetches and lets questions to the developer go as usual,
  and denies everything else — edits, commands (all of them: reading a
  command line is lexical, not a guarantee), subagents, other tools —
  whatever policy would otherwise say (`policy::read_only`). The turn's
  mode is its own (`Turn.read_only`), so a restart keeps it.
- **Its answer is the agent's message, not the run's result.** The run
  ends with the work's last turn's result; the question costs no attempt.
- **Only on the SDK runtime**, where every tool call reaches Otter's policy
  (the mandatory hook). On Claude Code's stream, which runs what it thinks
  read-only without asking, `ask_agent` refuses and says to switch.
- **Only while the agent is at work.** With nothing at work, a question of
  its own is answered as a task of its own (D-084); Otter says so. A run
  started for a question alone, outside the task's work, is not built.
- **Coding runs get the search tools.** Live, the agent couldn't list a
  directory in a question's turn: the native Claude Code leaves out Glob and
  Grep unless asked for (it searches through Bash). The worker now asks for
  them in every coding run; Otter's policy still checks each call.
- An older otterd ignores the mode: one started on a journal with a queued
  question's turn would run it unrestricted. No schema bump for that edge —
  a bump would shut an older otterd out of every conversation.
- Not done, and waiting: `DeliveryMode::AnswerOnly` (every turn of a task
  read-only), which the plan pairs with this. The rule is the same, but two
  things aren't decided: how it is set (on creating a task, from the CLI, in
  the app, by Otter on the developer's word — four surfaces) and what
  happens when the agent's look at the code returns a plan anyway (answered,
  refused, or asked). The dogfooding transcripts (V2-0c) are the evidence
  for both.

## D-092 — Answer-only tasks (2026-10-15)

Plan `brain-runtime-v2` §7.3. The developer asked for every milestone
before the dogfooding (V2-0c) was in: the two open points D-091 left are
decided here by default, to revisit with the transcripts.

- **Set when the task is made** — `answer_only` in `feature.create`,
  `otter feature new --answer-only`, *Only an answer* in the app's New task
  — **or by Otter on the developer's word** (`set_answer_only`, on or off:
  "don't change anything"). The app shows *Answer only* on such a task.
- **Every turn of the task's coding agent only reads**, by the rule of a
  question's turn (D-091, `policy::read_only`) — its look at the code too.
- **Only on the SDK runtime.** Elsewhere `feature.create` and the tool
  refuse: never an answer-only task that can't be kept to reading.
- **A plan under it comes to the developer**, as the plan question D-084
  already asks (`PlanApproval`, the developer's only — "go on" in chat
  approves it, as for any plan): approved, the task stops being answer-only
  and the plan is set; turned down, the plan, written out, is the answer
  and the task is done ("Not carried out: this task only answers"). The
  brain doesn't review such a plan: nothing is built unless it's allowed.
  *Amended 2026-10-21 (D-158):* turned down, the agent answers the
  question in words in one more turn; a plan again isn't asked about twice.
- `answer_only` is a new field with a default: no schema bump (D-061).

## D-093 — Otter on the host: a conversation of its own (2026-10-15)

Plan `brain-runtime-v2` M3. The developer asked for every milestone before
the dogfooding (V2-0c): the choices here are defaults, to revisit with it.

- **One conversation per host, with Otter**, about any task there, apart
  from each task's own (`otter.get`/`otter.send`/`otter.events`; events
  `OtterChanged`, `OtterStream`). `otter chat` in the terminal; in the app,
  the **Otter** view (⌘4, last so the other views keep their keys): the host
  it talks to chosen and shown, the reply streamed, a task it names one
  click away, and an otterd too old for it said so.
- **Answered as a turn with Otter's tools — the host's:** `list_tasks`,
  `list_workspaces`, a task's `get_task`/`get_pending_decisions`/
  `get_run_events`, and, on the developer's word, `create_task`,
  `start_task`, `resume_task`; `pause_task` in any turn. What belongs to a
  task — guidance, questions about its code, its decisions — is said in
  that task's conversation, and Otter says so. Without Otter's tools it
  answers with how to turn them on. One turn at a time, one reply per
  message, the limits of a task's turn (D-089).
- **A task Otter makes has the developer's words as its request** — their
  message(s), by id, as written — and a title of Otter's: Otter's words are
  never recorded as theirs.
- **Its changes go the way a client's do** (`feature.create`,
  `feature.act`, with command ids made of the developer's message, the
  tool and the task), not through a task's step: a host turn runs outside
  every task's step, and those paths are already safe beside it.
- **Kept the way a feature is** — `state/otter/chat.json` rewritten
  atomically (synced, renamed), `history.jsonl` appended and synced —
  rather than as a D-058 journal, as the plan had it: a conversation of
  messages and tool records, with no turns to deliver, gains nothing from
  the journal's ops and needs what the feature store already does.

## D-094 — Otter follows up the tasks it made (2026-10-15)

Plan `brain-runtime-v2` M4. A default chosen at the developer's direction
before the dogfooding (V2-0c), to revisit with it.

- **What it follows:** the tasks Otter made from its conversation on the
  host (`create_task`), kept with that conversation (`follows`). Nothing
  else: a task made in the app or the terminal is the developer's to watch,
  in the Inbox (D-072).
- **What a follow-up is for:** a change that may need the developer — done,
  failed, in review, or waiting on them (`Feature::follow_key`: the status
  and when it was reached, so each change is its own).
- **How:** the controller loop sees the change and runs a turn no message
  started (`Origin::Event`): the host's tools to look, and pausing at most —
  nothing on the developer's word. Its answer goes into the host
  conversation, one message, tied to the change (`follow:<task>:<change>`).
  Without Otter's tools, a line says what changed. The developer's messages
  come first; one turn at a time.
- **Once:** the message and the change it is about are written together.
  A crash before leaves the change to be followed up after the restart; a
  change already followed up never is again. A change that happened while
  otterd was down is followed up once, as it stands now; changes it went
  through meanwhile aren't.
- **One narrator per task stays** (D-069): the task's own conversation
  keeps the controller's messages; the follow-up is in the host's
  conversation, about the task.

## D-095 — The API providers answer with Otter's tools (2026-10-15)

Plan `brain-runtime-v2` M5, at the developer's direction (before V2-0c).

- **The tool loop runs in otterd** for a provider's API (`ApiTurn`): each
  round a request with Otter's tools — chat completions' `tools` for the
  OpenAI-style providers, the Messages API's for Anthropic — each tool call
  answered by the same gateway as Claude's turn, until the model answers or
  its round trips run out. Same limits, same one reply per message.
- **Where a provider has its key.** Without one, the brain answers as
  before, and Settings and `otter doctor` say "no API key".
- The key goes as before: in curl's config on stdin, never on a command
  line or in what is sent.
- The reply isn't streamed: it comes whole at the end of the turn.
- `OTTER_PROVIDER_BASE` (tests only) points every provider at a stand-in.
- **`INTENT:` isn't retired.** It still answers where there is no turn —
  `claude` without the worker, a provider without a key, `rules` — and
  retiring it is a decision of its own.
- Tried: OpenRouter, live (V2-6); both API styles offline. The other
  OpenAI-style providers share the code; their own quirks are untried.

## D-096 — Otter's tools are the default (2026-10-15)

Plan `brain-runtime-v2` M2's last step. D-090 held it back until the reply
streamed (done, M2b) and the dogfooding transcripts existed (V2-0c: still
none). **The developer asked for every milestone to be done, so it is
turned on now, before the transcripts** — a deliberate deviation from
D-090's gate, recorded as such.

- `otter_tools` is on unless a host's setting turns it off; `OTTER_BRAIN_V2`
  still overrides it (`0` off, `1` on). The setting is now *Otter's tools*,
  no longer "(preview)".
- Where a brain has no tool-using turn (`claude` without the worker, a
  provider without a key, `rules`), Otter answers as before (`INTENT:`), and
  Settings and `otter doctor` say why.
- It reaches a host only with a release, and every change since D-089 was
  merged without one: when to release stays the developer's.
- The transcripts (V2-0c) are still what tells whether it is better; the
  off switch is there for what they show.

## D-097 — The app's side panels are as wide as the developer drags them (2026-10-15)

Dogfooding: the Files panel (300 px) cut off file names, and the workspace
list (280 px) couldn't be widened either. Both now have a handle on their
inner edge, as the message box has had since D-088:

- **Drag the edge, or ←/→ on it (16 px a step); a double-click goes back to
  the usual width.** The width is remembered on this Mac (`localStorage`),
  like the message box's height.
- **One width for the list on the left:** the Workspaces and Tasks lists
  share it, so switching views doesn't move the column. **One width for the
  panel on the right:** Files and Timeline share it, as they share the place.
- **Bounds:** the list is 180 px or more and leaves the view 400 px; the
  right panel is 220 px or more and leaves the terminal 320 px. The same
  bounds are the panels' `min-width`/`max-width` in CSS, so a window made
  narrower later still leaves the work room; that replaces the fixed
  220/180 px the list took in narrow windows.
- The handle takes no room: it overlaps the panel's border, shows on
  hover, and sits above the title bar's drag region so grabbing the top of
  an edge doesn't move the window.
- Not done: the task view's conversation/detail split, and collapsing a
  panel by dragging it closed (the Files/Timeline buttons close it).

## D-098 — A workspace has no goal (2026-10-15)

Amends D-038 and design §8. Dogfooding: New workspace asked for a goal, and
the workspace pane showed it (or "Add a goal") beside the name, for a line
the developer doesn't want to write — the name says what the workspace is
for, and the task says what the work is (D-043). Nothing ever read the goal:
agents and Otter take a task's brief, never a workspace's.

- **`Brief` has no `goal`.** A brief written before still reads (serde
  ignores the field, no `deny_unknown_fields`) and loses its goal the next
  time it is saved; an older client's `goal` is ignored the same way. No
  other wire change: `workspace.create` and `workspace.set_brief` still take
  a brief (title, description, constraints, references, decisions).
- **CLI:** `otter new --goal` and `otter ws brief --goal` are gone;
  `--title` and `--description` remain.
- **App:** New workspace has no Goal field. The pane's line beside the name
  shows the brief's title (or description) only when there is one, with no
  "Add a goal" in its place; the brief is edited from the workspace menu
  ("Add a brief…" / "Edit brief…").
- A task's own worktree (`feature.start` in a workspace of its own) is
  described by the task's title, and "The feature … in a worktree of its
  own" moves to the description.
- protocol.md lists removing a field as incompatible. An optional field
  that old and new peers both treat as optional breaks neither, so there is
  no bump: an older client's `goal` is ignored, and an older daemon's is
  simply not shown.

## D-099 — Looking at a session doesn't answer it (2026-10-15)

Amends D-014's "resolved by attaching". In the app, showing a session's
terminal attaches, so selecting a workspace was enough to clear its "needs
you": click the notification, glance, click away, and the approval was gone
from the sidebar, the tray and the Inbox while the agent still waited.
Attention is raised only on a transition, so nothing brought it back.
Reproduced with an e2e test (the review of the notification plan).

- **Attaching settles only what was there to be seen:** a finished turn
  (`review`) and a completed command (`completion`). An approval, a question
  and a failure stay until they are answered:
  - the agent moves on (working again, idle, exited);
  - the developer sends to the session (`session.write`: typing the answer
    is answering);
  - the session is restarted, stopped or deleted;
  - it is marked handled (`otter ack`, the banner's button).
- **A blocked agent on another prompt raises again.** While it stays
  `Blocked`, a different blocker (a second approval that came before a
  reconcile saw the agent at work in between) replaces the item. The same
  blocker doesn't, so an item marked handled stays handled.
  `AgentInfo.blocker` remembers what was raised (additive, serde default).
  An inferred blocker that the agent's hooks then describe is replaced by the
  description.
- **The app's half:** its terminals stay attached across workspace switches
  (D-100), so switching back to one no longer attaches. Showing a workspace
  again settles its current session's `review`/`completion` the way attaching
  does, by attention id.
- Not done: the rest of the notification plan. That covers telling a
  confirmed blocker from an inferred one, and suppressing alerts per session
  rather than per workspace.

## D-100 — A workspace's terminal stays attached while another is shown (2026-10-15)

Dogfooding (the workspace review): every switch between workspaces
unmounted the pane, so its terminal detached and attached again on the way
back. Over SSH that is a wait and a redraw, the app's scrollback was lost,
and each attach counted as looking (D-099).

- **The five workspaces shown last keep their pane mounted**, hidden, with
  its terminal attached. One shown again is just shown. Older ones are
  unmounted (detached), as before.
- **A kept pane keeps the session it showed**, so nothing switches or
  attaches behind the developer's back. The one in front still opens on
  whatever needs the developer, as before.
- **Hidden means idle:** it never fits its terminal, so the session isn't
  resized for a pane nobody sees (`window-size latest`, the hidden pane
  sends nothing). It doesn't poll its host and takes no keys. Shown again,
  its terminal takes the keyboard.
- **D-099's half here:** showing a workspace again, after another one or a
  host's page, settles its current session's `review`/`completion` by
  attention id. That is what the re-attach used to do. Switching views
  (⌘1–⌘4) never re-attached and still settles nothing.

## D-101 — Workspaces and sessions can be renamed (2026-10-15)

A task's workspace is named for it (`api-fix-login-bug-2`), and a session
for its command, with no way to change either.

- `workspace.rename` and `session.rename`, with `WorkspaceRenamed` /
  `SessionRenamed` events (names, like `WorkspaceCreated`'s).
- **Only the name changes.** Ids, the directory (`~/.otter/workspaces/<id>`),
  the branch (still `otterd/<old name>` if it was made so) and the backend
  (keyed by execution id) stay, so attaches and references by id carry on.
  Names follow creation's rules and stay unique.
- CLI: `otter ws rename <ws> <name>`, `otter session rename <ws/session>
  <name>`. App: "Rename…" in the workspace's menus (the pane's, the
  sidebar's right-click), and double-click a session's tab, or its menu.

## D-102 — A workspace shows its Git work (2026-10-15)

The pane showed a branch name and nothing of what an agent had changed;
seeing it meant a shell and `git status`. Verification already read the work
(`work_state`, D-062), but for the judge.

- `workspace.changes`: the branch, commits ahead of and behind the base, and
  every file changed since it, committed or not (new files too), with line
  counts. `workspace.diff`: one file's diff. The base is the workspace's
  base branch, else the branch's upstream; with neither, only what isn't
  committed. Read-only, never touching the index, scoped to the workspace
  root. Bounded: 500 files, 512 KiB of diff.
- Unlike the judge's report, nothing is held back: secret-looking files are
  listed and diffed. This is the developer's own workspace, which the Files
  panel already downloads whole.
- The merge-base lookup is shared with `work_state` (`merge_base`).
- App: a chip beside the branch (`3 files · +120 −14 · ↑2`, none when
  there's nothing to say) opens a **Changes** panel, next to Files and
  Timeline, listing the files and each one's diff. It is read when the workspace is shown and something
  happened in it (a session or agent moved). An agent's edits make no event,
  so while the panel is open it reads again every 30 s, and on ↻.
- Not done: staging, committing, or diffs of a single commit.

## D-103 — Files can be previewed, renamed and deleted (2026-10-15)

The Files panel only browsed, downloaded and uploaded: reading a log meant
a download, and a stray file meant a shell.

- **Preview:** clicking a file shows its first 256 KiB as text (UTF-8
  without NULs); anything else says so and offers the download.
- `fs.remove` (a file, a link, or a directory and everything in it) and
  `fs.rename` (a new name in the same directory, never over another). Both
  act **only inside the workspace**: the path's parent is resolved, so a link
  is removed, not its target. A path resolving outside the root, or to the
  root itself, is refused. Reading stays as it was (any path the
  daemon's user can read).
- The app asks before deleting, and says "and everything in it" for a
  folder. Only the panel in front takes files dropped on the window (a
  kept workspace's is mounted too, D-100).
- The path field takes a folder to go to (`~ws/…` is the workspace). A
  dotfiles switch is remembered on this Mac; it starts on, as before. The
  side panel last open opens with the next workspace too.

## D-104 — A workspace knows its sessions' ports (2026-10-15)

A dev server started in a session showed up only on the host's page, with
no telling whose it was.

- `workspace.ports`: the listening ports (`host.ports`) whose process
  descends from one of the workspace's running sessions' processes, found by
  walking up each process's parents (`sysinfo`, at most 64 levels). Read
  when asked, never in the reconcile loop.
- App: a chip per port in the session bar (`:5173 ↗`). One click opens it in
  the browser, forwarding it first for a remote host (D-046's preview path,
  now shared). The pane in front reads them every 10 s while the window has
  focus and a session runs.

## D-105 — A task's own workspace is put away when the task ends (2026-10-15)

Each task given a worktree of its own (D-080) left a workspace behind,
running its agent, after the task was done; D-036 had left auto-archiving
for later.

- **`Workspace.feature_id`** marks a workspace made for a task (additive).
  A workspace the developer chose for a task is never touched.
- **When the task becomes done or cancelled, its own workspace is
  archived** (files, branch and conversations kept). It is skipped while the
  task still asks something, such as a push to allow, which a review step
  can raise just as the task is accepted; answering it tries again. It is
  also skipped while anything the developer may be using runs there: a
  shell, a service, a command, an agent at work or waiting on them. The
  task's conversation says it was archived.
- **Only on the transition:** a developer who unarchives it to look keeps
  it.
- **It comes back when the task needs it again:** asked for more after done
  (D-050), the task unarchives its own workspace and waits while it
  prepares. Another's archived workspace makes it wait with "unarchive it
  to go on", instead of failing the run.
- App: a task's workspace links to its task ("Task ↗").

## D-106 — The workspace view's everyday tools (2026-10-15)

From the same review, the small things a day of work in the view runs into.
All app-side, kept on this Mac where remembered:

- **⌘K** goes to a workspace by part of its name, host, branch, repository,
  brief title or a session's name; the sidebar has the same filter.
  **Idle and archived workspaces are listed the latest active first** (its
  last change, an agent's last move, its newest attention), not by name.
- **Shortcuts in a workspace:** ⌘T a new session; ⌘⇧[ / ⌘⇧] the session
  before / after; ⌘F finds in the session's history. Not while a dialog is
  open. *Amended 2026-10-19:* ⌥1…⌥9 the session in that place
  (D-122).
- **⌘F reads the session's history** (`session.read`, the last 10 000
  lines) and searches it in the app. The history lives in the session
  (tmux's alternate screen keeps it out of the app's terminal), so the
  matches are listed, with line numbers, rather than scrolled to.
- **Text size:** ⌘= / ⌘- / ⌘0, for every terminal at once, remembered.
- **⌘-click a link** in a terminal opens it. The session has the mouse
  (tmux), so the app reads the line under the pointer itself and keeps the
  click from the session. Rows are joined where the terminal knows the line
  wrapped; tmux draws each row itself, so a link split across rows may not
  open (to check in use).
- **New workspace suggests the repositories** the host's workspaces came
  from.
- **Older hosts:** every request above is new. A host that answers
  `unknown variant` is not asked that again until it reconnects (maybe
  updated). Its chips stay hidden, and an action says to update it.

## D-107 — Features are tasks, everywhere (2026-10-15)

D-088 renamed features to tasks where the developer reads them, the app,
and kept "feature" in the model, the protocol, the CLI, the daemon and these
docs. The developer asked for the code to say task too, wire and disk
included. Three names move together, because "task" was taken twice:

| Before | Now |
|---|---|
| a feature (`Feature`, `FeatureId`, `feature.*`, `FeatureChanged`, `feature_id`, `otter feature`, `state/features/`) | a **task** (`Task`, `TaskId`, `task.*`, `TaskChanged`, `task_id`, `otter task`, `state/tasks/`) |
| a plan's task (`Task`, `TaskId`, `tasks`, `task_id`, `TaskChanged` in a history) | a **step** (`Step`, `StepId`, `steps`, `step_id`, `StepChanged`) |
| a session of kind `task` | a session of kind **`command`** |

- **The protocol is v3.** Renaming on the wire is incompatible
  (protocol.md lists every name). App and daemon must match; the app shows
  an older host as "incompatible" and offers to update it. That update
  stops the old daemon by its pid file, never over the protocol, so it
  works across the bump.
- **A v2 host's state still reads.**
  - At start, `state/features/` moves once to `state/tasks/`, and each
    `feature.json` becomes `task.json`. If both directories exist (an older
    otterd ran after a newer one), what's under `state/tasks` stays, and the
    log says so.
  - Old names elsewhere read through serde aliases: events, histories,
    `state.json`, triggers, a plan proposed with `"tasks"`.
  - A conversation needs its own deserializer: it named both its feature
    (`feature_id`) and its step (`task_id`).
  - `a_hosts_features_become_its_tasks` starts a daemon on a real v2 home,
    captured by the ignored `capture_v2_state` before the rename.
  - Going back to an older otterd isn't supported.
- **What doesn't change:**
  - Ids keep their prefixes (`ft_` a task, `tk_` a step): they're quoted in
    histories, transcripts and Otter's replies.
  - This file's history, `docs/plans/` and `docs/audits/`.
  - Others' names: Claude Code's `Task*` tools, Codex's `task_complete`,
    tmux's `terminal-features`, branches named `feature/…`.
  - The capability `RuntimeCapabilities.features`, which is about the
    runtime, not tasks.
- **Old ways in still work:** `otter feature …`, `otter conversation list
  --feature`, `otter start --kind task`, and the app's remembered "features"
  view.
- **The models read it too.** Otter's prompts say task. The coding agent is
  given "Your step (1 of 2)", and the planner proposes `steps`; `tasks`
  still parses.

## D-108 — Deleting a workspace deletes what Otter made for it (2026-10-15)

Deleting a Git workspace removed its worktree but kept its branch, and the
repository's copy under `~/.otter/repos` stayed after its last workspace
went. That copy is a full clone. The developer asked that deleting clean up.

- **Its branch goes too.** Commits on it that are on no remote-tracking
  branch stop the delete without force, the way uncommitted changes do. With
  force both go: `--force` and the app's checkbox now say so.
- **The repository's copy goes with the last workspace that used it.** The
  check runs under the store's lock, so a workspace being made on it is
  counted. *(Since D-135: under the copy's Git lock.)* The copy stays while any branch in it holds commits on no remote,
  for example a branch kept by a delete from before this change.
- **A branch another worktree has checked out stays,** and so does
  everything if removing the worktree failed: nothing half-removed.
- **Never touched:** an existing folder attached as a workspace (design §29),
  and anything outside `~/.otter`.

## D-109 — Attention says whether it is a guess, and notifications say how much it asks (2026-10-15)

The rest of the notification review that D-099 started:

- **`Attention.inferred`** is additive: absent when false, so older clients
  ignore it. It marks an item raised from a still turn (`agents::settle`)
  where the agent didn't say what it waits for. Codex never says, so every
  Codex prompt is a guess and still notifies, now titled "may be waiting
  for you". The workspace's banner tags it "possibly".
- **A finished turn is news, not a call:** its notification is titled "turn
  finished". `needs_you()` (D-014) is unchanged: the CLI and the Inbox still
  count it.
- **Suppressed per session, not per workspace:** an item on a session other
  than the one on screen notifies, even when its workspace is in front.
- **NEEDS YOU lists what blocks first:** prompts an agent reported, then
  guesses, then finished turns, each longest-waiting first.
- No new method and no protocol bump.

## D-110 — Two sessions side by side (2026-10-15)

The pane showed one session; watching an agent and a shell (`nvim`,
`lazygit`) meant switching tabs.

- **The split button, or ⌘\\,** shows the next session beside the one on the
  left. Each has its own attached terminal. The one clicked last has the
  keyboard and ⌘F. A grip sets the right one's width, remembered like the
  other panels' (D-097).
- **The right one's tab is marked.** Clicking it swaps the two; any other tab
  replaces the left. The split itself isn't remembered: a session gone since
  would be worse than none.
- **The terminal and its end banners became `SessionTerminal`,** so the
  pane renders one or two of them.

## D-111 — Diffs side by side, highlighted, with folds that open (2026-10-15)

The Changes panel's diff (D-102) was plain unified text with git's 3 lines
of context. The developer asked for GitHub's way: side by side, more context
on a click, and syntax colours.

- **`workspace.diff` takes an optional `context`** (additive). The app asks
  for the whole file, bounded by the existing 512 KiB, and folds it itself:
  - 3 lines around each change;
  - a fold between changes opens 20 lines from either side, or all of it.

  An older otterd ignores `context` and sends its 3-line hunks; the app
  shows those, with a "⋯" where lines were left out.
- **Side by side by default, or unified,** remembered on this Mac. Removed
  lines pair with the lines that took their place, as GitHub does.
- **Highlighted with highlight.js** for the usual languages, chosen by file
  name. Both the whole old text and the whole new text are highlighted, so
  multi-line comments and strings keep their colour. Text over 300 KB stays
  plain. The theme's own colours apply, light and dark. The Files preview
  (D-103) is highlighted the same way.
- About 150 KB more JavaScript (50 KB gzipped).

## D-112 — The stand-ins stay on PATH when direnv reloads (2026-10-15)

Dogfooding: Ctrl+V stopped pasting images into Claude Code. The image
reached otterd, but the session's `claude` had no `run/bin` on its PATH, so
it never ran otterd's `wl-paste` (D-031). otterd resolves a workspace's
direnv environment, then puts `run/bin` first when it launches. What direnv
exports remembers the PATH it started from, without `run/bin`. A session's
shell hook goes back to that PATH when a file direnv watches changes (here
nix-direnv refreshed the dev shell after otterd restarted) or when the
shell `cd`s elsewhere, and the stand-ins were gone until the session
restarted.

- **direnv runs with `run/bin` already first on PATH**, so the PATH it
  remembers has the stand-ins and a reload or unload keeps them. A session
  still gets `run/bin` first, once.
- **The login environment drops `DIRENV_DIFF`, `DIRENV_DIR`, `DIRENV_FILE`
  and `DIRENV_WATCHES`.** They describe the directory the daemon happened to
  start from, such as a dev shell. Any direnv run, otterd's or a session's,
  would first go back to the PATH from before that directory loaded.
- Limit: after a reload, the dev shell's own directories come before
  `run/bin`. A dev shell that provides `wl-paste` or `xdg-open` would win
  over otterd's. This is not "first on every session's PATH" as D-031 says.
- PATH additions in `.zshrc` are still lost on a reload. otterd captures
  the login environment with a non-interactive login shell, which doesn't
  read `.zshrc`, so they aren't in the PATH direnv goes back to. Additions
  in `.zprofile` or `.zshenv` are kept.
- Tested end to end: a session touches its `.envrc`, runs what the hook
  runs, and `run/bin` is still on PATH.

## D-113 — Otter remembers what was decided (2026-10-16)

Each reply was a fresh model call given the last 12 messages, cut to 600
characters each (`turn.rs`, and `brain.rs` for brains without tools). A
decision made 13 messages back was gone, and no tool read older messages.
The developer asked for a memory per conversation, plus one across
conversations for the decisions that hold everywhere.

- **More of the conversation:** the last 20 messages, cut to 2 000
  characters each, in a task's turn, on the host and in `reply`. A line says
  how many messages there are and which id to page back from.
- **`get_messages`** (Observe, in every conversation) reads the
  conversation's messages newest first and whole (redacted, D-075), in pages
  (`before` a message id), or the ones that mention a `query`. Bounded: 30
  messages, 8 000 characters each, 40 000 a page. Its ids aren't the turn's:
  `forward_to_agent` and the like still take only this turn's messages.
- **Two memories each turn is given,** Markdown files:
  - the conversation's: `state/tasks/<id>/memory.md` for a task (deleted
    with it), `state/otter/chat-memory.md` for Otter's conversation on the
    host (D-093);
  - the host's, across every conversation there: `state/otter/memory.md`.
    "Across" means one host: otterd is per host and nothing syncs memories
    between hosts.

  The context labels them `memory "conversation"` and `memory "host"`, the
  values the tools take. Brains without tools are given them in `reply` too,
  but can't change them.
- **Otter changes them with `remember`** (adds a line; a note already there
  isn't added again) **and `edit_memory`** (replaces text found exactly
  once; empty drops it). Both are on the developer's word: only a turn their
  message started may call them, so what Otter keeps comes from talking with
  them, never from an event or from what a tool returned. They aren't
  applied under `<message>:<tool>`, because one message may settle several
  things; the "already there" check is what keeps a turn run again from
  noting twice. The history records `ToolCalled`/`ToolDone`, never what was
  noted. "Only pausing changes anything without the developer's word"
  stands.
- **A note is never authority.** The instructions say a note informs the
  answer and is never the developer's word for a tool. An approval still
  comes from this turn's messages.
- **Bounded and redacted:** 8 000 characters per memory, 1 000 per note,
  redacted when written and again when read, because a file may have been
  edited by hand (invariant 13). A full memory refuses another
  note until Otter folds notes together or drops some (`next` says so).
  Memories are never in `events.jsonl`.
- **The developer's to read and correct:**
  - `memory.get {task?}` and `memory.set {task?, memory, text, previous?}`
    are new methods, so no protocol bump.
  - `previous` makes an edit that races one of Otter's notes a `conflict`
    instead of losing it.
  - The app's task header and Otter's view have a **Memory** button: both
    memories, editable.
  - Each memory is read from disk for every turn, so editing a file by hand
    takes effect at the next turn.
  - No event says a memory changed. The dialog reads it when it opens.

## D-114 — A task's page shows what the task has (2026-10-16)

Dogfooding an answer-only task: the page had five list filters, seven
detail tabs and a Plan full of "None yet". The status sentence was cut off
in the header and repeated the list row and Otter's last message. "Why"
listed the same line three times, and a decision waiting for the developer
lived in its own tab behind a banner. The conversation, the one part used
every time, had the least room. Only the app changes:

- **The list is grouped:** Needs you (a decision, a review, a block, a
  stop), Active, Done (cancelled ones too), each with its count. A find box
  takes part of a title, the request or the host. It replaces the five
  filter chips.
- **The header holds the task's actions:** status, host / workspace as a
  link, Start / Pause / Resume / Retry / Accept, Interrupt and Preview.
  Memory, Open workspace, Cancel task and Delete task… are in a ⋯ menu.
  The status sentence moved to the panel.
- **Decisions are in the conversation.** One waiting is pinned above the
  message box with its Approve / Deny or its questions. One taken shows
  where it was asked, with who decided. The banner stays only for what has
  no other place: a stop, a block without a decision, waiting for a
  workspace.
- **Two tabs instead of seven:**
  - **Overview** shows only sections with something in them, ordered for
    the task's stage. A working task starts with Now, Progress and Checks.
    One under review starts with Checks, Delivery and Report. A finished
    one starts with Outcome.
    - **Checks** joins acceptance criteria, gates and evidence. Each
      criterion shows the evidence behind it.
    - **Progress** leaves out the coding agent's planning turn.
    - The request folds, because it is also the first message.
    - The budget shows only once half the attempts are used.
  - **Activity** is the task's history and the coding agent's turns in one
    feed, newest first, narrowed to All, Coding agent or Task. Otter's
    reasons ("Why") come after it, a line said several times in a row
    once with ×N. They are real repeats (each follow-up answered again),
    not a bug to fix in the daemon.
- **A long report from the coding agent folds** (over 1 200 characters),
  so the messages around it stay in view.
- Not built: a note in the conversation where Otter saved something to
  memory. Nothing per message says so; the history only records that
  Otter's tool ran.

## D-115 — Otter's view leaves the app for now (2026-10-16)

The developer asked what Otter's view (D-093) was for, and to take it out if
it wasn't used. They didn't use it. A request becomes a task through New
task, and what needs them is in the Inbox. On the dogfooding host the view
stayed empty: a conversation on the host covered nothing the other views
didn't.

- **The app drops the view:** its Activity Bar entry and ⌘4, the pane and
  the commands behind it. A view remembered as Otter opens the Inbox.
  Memory stays in each task's ⋯ menu; its dialog shows the host's memory
  too.
- **The host keeps it.** `otter chat`, `otter.get` / `otter.send`, the
  conversation's memory and the host's tools are unchanged, so the view can
  come back without a protocol change if a use shows up in dogfooding.

## D-116 — The terminal types like a terminal (2026-10-16)

Three things from a day in the workspace view:

- **Holding a key repeats it.** In neovim, holding `k` moved once. The
  app's web view treats a held key as macOS press-and-hold, which waits to
  offer accented letters instead of repeating (VS Code users fix the same
  thing with `defaults write`). At launch the app sets
  `ApplePressAndHoldEnabled` to false in its own defaults. They outrank the
  system-wide setting and apply to Otter only. Uses `objc2-foundation`,
  which was already in the tree through Tauri.
- **The terminal font is the developer's,** in Settings › This Mac: any
  installed font or a list, kept on this Mac like the text size (D-106),
  applied at once. Settings › This Mac also has the text size, alongside
  ⌘= and ⌘-. The bundled JetBrains Mono stays the default and the fallback
  for glyphs a chosen font lacks.
- **Measured after the font loads.** The terminal measures its cells and
  draws its glyphs (WebGL) with whatever font is loaded when it opens. The
  bundled font loads only when first used, and text measured with a
  fallback spreads out. Once the font's faces load (or fail to), the
  terminal measures and draws again.
  *Amended 2026-10-16:* by changing its font option alone, not by also
  clearing its glyphs (`clearTextureAtlas`). Terminals with the same font
  and size share one set of drawn glyphs, and clearing it redraws only the
  terminal that asked; the others keep pointing at glyphs that are gone.
  Every terminal opened did this, so switching back to a workspace (kept
  mounted, D-100) or to the other of two side by side (D-110) showed its
  screen as scraps of the wrong glyphs until a reload. Only cells that
  changed afterward drew correctly. Changing the option already measures
  again, takes glyphs drawn with the loaded font, and redraws the terminal
  in full. Checked in headless Chromium (WebGL): a kept terminal stays
  pixel-identical after another opens, and one opened before the font
  loads matches one opened after.
- **New session lists Claude Code second,** after Shell.

## D-117 — Where a session's history scrolls (2026-10-16)

Dogfooding: scrolling a Claude Code session in the app lags. Measured on the
dogfooding host with a 291×60 pane, replaying a real Claude Code history
into an isolated tmux server with Otter's config:

- **tmux isn't what's slow.** A wheel step costs tmux under 2 ms and 1–2 KB
  (it scrolls the region instead of redrawing; a whole screen is ~14 KB).
  It kept up with a 40-report trackpad flick at ~120 Hz. A multiplexer of
  our own would gain nothing here, and would lose sessions outliving otterd
  (D-007 adopts them at startup).
- **The round trip is.** Every step goes from the Mac over SSH to otterd and
  tmux, then back: about 20 ms on that link before anything is drawn, more
  than a frame. Whatever scrolls on the host has that floor, whether tmux,
  a replacement or the program itself.
- **Claude Code sessions come in two kinds.** *Inline* (how Otter starts
  it): the transcript is in tmux's history, and the wheel puts tmux in copy
  mode. *Full screen* (`CLAUDE_CODE_NO_FLICKER=1`): alternate screen with
  any-event mouse tracking. The wheel, and every pointer move, go to Claude
  Code, which keeps the transcript and redraws the screen each step. Neither
  tmux nor the app can scroll that one locally. Its cost is Claude Code's
  and wasn't measured.
- **Now: three lines a wheel step** in both copy-mode tables, instead of
  tmux's five. xterm.js sends one wheel report per about 3.3 lines of
  trackpad travel (it scales pixel deltas under 50 by 0.3). Five lines a
  report ran 1.5× ahead of the finger, in 81 px jumps. Three keeps it about
  1:1, and is the usual step elsewhere. otterd sources its config on every
  start, so running sessions get it when the host updates. It's a tuning
  knob; the developer judges it.
- **Next, not done: the history in the app.** Smooth, pixel-level scrolling
  needs the history where the wheel is, in xterm's own buffer. tmux control
  mode (as iTerm2 uses it) would feed the pane's output to the app's
  terminal. It would need history seeded from `capture-pane -e` on attach,
  resize through `refresh-client -C`, and flow control. That redesigns
  attach on both ends and helps inline sessions only, so it gets its own
  decision when taken up. *Amended 2026-10-17:* taken up in D-118.

## D-118 — The app's terminal keeps the history (2026-10-17)

Takes up D-117's next step. In the app, each wheel step through a session's
history went to the host and back (about 20 ms on the dogfooding link). Now
the history is in the app's terminal, and scrolling it stays on the Mac. tmux
stays as the backend (design §12), attached in control mode.

- **On the wire:** `session.attach` takes an optional `history` (lines), and
  `"history": true` in the response says the daemon streams. Both are
  optional, so compatible, and there's no version bump. An older daemon
  ignores the request, and the app gets today's attach. The CLI keeps the
  screen attach; a user's own terminal would need the same care.
- **In the daemon:** `ExecutionBackend::open_stream`, none by default (then
  the PTY attach). tmux runs `tmux -C attach-session` over pipes
  (`backend/tmux/control.rs`), and `%output` is the pane's own output. tmux
  stays inside `backend/` (invariant 7). Detaching closes the control
  client's stdin, so nothing is typed into the session (invariant 3).
- **The snapshot** is one command line: `refresh-client -C` (the size,
  clamped to the 1–10000 tmux takes), `display-message` (the pane's state),
  and three `capture-pane`s. tmux runs a line's commands without reading the
  pane in between. Measured with a counter printing as fast as it could: the
  capture ended at N and the first streamed number was N+1, three times out
  of three. Output before the replies is dropped, and output after them is
  forwarded. From it the app's terminal is rebuilt:
  - the screen at once;
  - then, after CAN and RIS and inside synchronized output (xterm.js holds
    it up to a second), up to `history` lines of history, capped at 256 KiB;
  - the normal screen, and the alternate one with its saved cursor;
  - scroll region, origin, wrap, insert, cursor keys, keypad and mouse
    modes, and the cursor.

  The app asks for 2,000 lines. The writer formats the line at the size it
  has, so a resize can't overtake it.
- **Replies are counted:** each command gets one reply, and an error drops
  the rest of its line. So replies are matched to the lines sent, and a
  snapshot that partly failed still shows what came instead of waiting.
- **Styles:** tmux's captures write a style once, and it holds across rows
  until it changes. Screens are captured with `-N`, so styled trailing
  blanks stay, and history with `-J`, which keeps the blanks that wrap a
  line. Each row is written in the style the one before ended in (tracked,
  kept short) and reset before the line break. A line feed that scrolls
  fills the new line with the current background. History cut off by the
  byte cap still passes on its style. Line drawing is marked the same way,
  with SO and SI carrying across rows, and is tracked the same way. The
  snapshot makes G1 the line-drawing set, as tmux does for a terminal
  client. A hyperlink still open at the end of a capture is closed.
- **Not readable on attach:** bracketed paste, focus reporting and cursor
  style; tmux exposes none of them. For bracketed paste, every snapshot turns
  it on in the app, and the daemon delivers a bracketed paste through a paste
  buffer (`paste-buffer -p -r`). tmux adds the brackets only if the program
  asked for them. tmux 3.7 shows control characters in a pasted buffer as
  `^X` (tab, CR and LF pass). A paste with a NUL goes as typing, because
  tmux's strings end at one. Focus reporting and cursor style come back the
  next time the program sets them. *Amended 2026-10-19:* the cursor's
  style and colour are restored on tmux 3.4+ (D-119); focus reporting
  still can't be.
- **Input** goes through `send-keys -H`, byte for byte, including each byte
  of a UTF-8 character (measured). Home and End go as tmux's key names, so
  the program gets what its terminal says (`\e[1~`, `\e[4~`), as with a
  terminal client; xterm.js sends xterm's.
- **Queries:** tmux, the session's terminal, has answered the program's
  queries. The app's xterm.js would answer them again from the raw output,
  and its answers would arrive as typing. In a stream attach the app
  swallows them: DA, DSR, DECRQM, window reports, DECRQSS, color and
  clipboard reads. The app holds output until it knows the attach is a
  stream, so this applies from the first byte.
- **Falling behind:** a stream is every byte the program writes, while a
  screen attach sent only the latest screen. The daemon owns what waits for
  the client. Past 2 MiB it drops all of it, and takes a new snapshot once
  the client has taken what it had. So a stuck client costs at most 2 MiB
  and a snapshot. Those snapshots come at least 500 ms apart, so a client
  slower than the program sees a screen, not a flicker of them. Measured: a 15 MB flood while the client didn't read for
  6 s reached it as 354 KB, with one resync, ending identical to tmux.
  During a 400 MB flood with a client that read nothing for 15 s, otterd
  stayed at 26 MB.
- **Size:** the snapshot sets the client's size first, so it's taken at that
  size. With `window-size latest`, a more recent client of another size (the
  CLI on the same session) sizes the window. The stream is then laid out for
  that size until the app is the latest client again. It's logged, not
  solved. *Amended 2026-10-19:* the client hears the window's size and
  follows it (D-119).
- **Differences left** (the app's terminal behaves like a terminal, not like
  tmux's drawing of one):
  - A screen cleared from its top-left corner goes into tmux's history but
    not the app's; xterm.js discards it, as most terminals do.
  - When a line wraps at the bottom, xterm.js fills the new row with the
    background in effect, and tmux doesn't.
  - tmux's screen attach drew styled blanks at the end of a row as plain
    (`\e[K`). The stream shows them as the program wrote them, and as
    tmux's grid holds them.
  - Characters are laid out with xterm.js's Unicode widths instead of tmux's.
    A Claude Code transcript streamed live came out the same.
  - Full-screen programs that keep their own history (Claude Code with
    `CLAUDE_CODE_NO_FLICKER=1`) still scroll it on the host (D-117).
- **Verified:**
  - End to end against real tmux: history, typing, resize, detach and
    disconnect leaving the session, exit, both paste cases, Home and End, the
    alternate screen with its modes, styles across rows (shown to fail
    without carrying them), and falling behind.
  - Headless xterm.js fed a real otterd's stream, compared with tmux: the
    screen and 2,000 rows of history were identical for a snapshot of a
    Claude Code transcript, the transcript streamed live (769 KB), `less`
    entered live and attached while open, and the 15 MB flood.
  - Cell by cell with colors and line drawing, against tmux's own screen
    attach and copy mode: 120 rows of real and crafted output were
    identical, except rows showing the background differences above.
  - Reviewed twice by a second model before merging. What it found is fixed
    above: unbounded memory, styles, hangs on a failed snapshot, Home and
    End, a resize race, early query answers, NULs, line drawing, links, and
    resync spacing.

## D-119 — Streams hear what the client's terminal is doing (2026-10-19)

Follows D-118, with what dogfooding it left open.

- **Control frames:** kind 5, JSON, both ways, used only once both peers
  said `controls`. Old peers never see one, so it's compatible: no version
  bump, and the Compatibility section now says so.
- **Off screen, nothing is sent.** The app keeps up to five workspaces'
  terminals mounted (D-100). Each got all of its session's output, over SSH,
  parsed by a terminal nobody saw. Now a terminal off screen (its workspace
  not shown, another view in front, the window hidden) tells the host. The
  daemon drops its output and notes that it did. Back on screen the
  terminal gets a snapshot (screen first) if it missed any; otherwise it
  just goes on. This isn't tmux's `refresh-client -A pane:off`: once every
  client turns a pane off, tmux stops reading it and the program blocks.
  Tested: the program runs on while the client gets nothing.
  *Amended 2026-10-21 (D-160):* the output is kept for the terminal, up to
  1 MiB, and sent when it's back; a snapshot past that, or when it comes
  back at another size.
- **SSH is compressed.** Snapshots are escape-heavy text. Real Claude Code
  ones went from 203 KiB to 29 KiB with zlib, which is what SSH uses.
  Otter's ssh passes `Compression=yes` after the host's `ssh_args`, so an
  explicit option there wins (it does override `~/.ssh/config`).
  ControlMaster sockets moved to `cz/`, so masters made before aren't
  reused uncompressed. Keeping more session terminals mounted would make
  switching instant, but the app already holds up to ten terminals, and
  WebKit has about sixteen WebGL contexts.
- **Another client's size.** tmux gives the window to the client that typed
  last (window-size latest). A control client can't take it back, whether
  by typing, by resizing, or with a per-window size (all measured); a new
  control client does. So the daemon reads the window's size: from
  `%layout-change`, and read back with each resize, so notices don't flap.
  When it isn't the stream client's own, the daemon tells the client,
  before any snapshot laid out at it. The app then shows the session at
  that size and says so. A click attaches again and takes the window back.
  When the other client leaves, the window is the app's again, and the app
  is told.
- **The cursor:** tmux 3.4+ has `#{cursor_shape}`, `#{cursor_blinking}` and
  `#{cursor_colour}`. The snapshot restores the shape (DECSCUSR) and the
  colour (OSC 12). The state line is comma-separated now, so a tmux without
  those formats leaves them empty and its state still reads.
- **Focus reporting** still can't be restored: tmux has no format for it,
  and `send-keys FocusIn` types the word (tmux 3.7).
- **History:**
  - The app asks for 5,000 lines on attach: its scrollback, 1 MiB at most,
    about 150 KiB compressed.
  - Scrolled to the top of that, while the host holds more, it offers
    earlier lines: a snapshot with up to 20,000 (4 MiB) that keeps the
    reader's place.
  - Each snapshot says how many lines it brought and whether there are more.
  - Snapshots no longer count as output waiting. A 4 MiB one would have
    made the next output fall behind, and dropped itself.
  - Output frames are cut to 1 MiB; frames take up to 4 MiB.
- **Memory in the app:** xterm.js keeps about 12 bytes a cell. 5,000 lines
  of a 291-column pane is about 17 MB a terminal, and 20,000 is about 70 MB,
  but only for terminals where earlier lines were asked for.
- **Verified:**
  - Unit tests: one snapshot at a time, with none lost; output off screen
    dropped, then caught up; the window notice before the snapshot, and only
    once resizes settle; layout notices; the cursor; more history.
  - End to end against real tmux: off screen and back, with and without
    missed output; another client sizing the window, then leaving; the
    cursor restored; earlier history brought.
  - The desktop crate passes clippy (built here with nix-provided libraries).

## D-120 — `otter attach --scrollback` (2026-10-19)

The CLI gets what D-118 gave the app, as an option: with `--scrollback`, the
user's terminal is the session's terminal. It gets up to 5,000 lines of
history, then the program's own output, and keeps them as its own
scrollback, so scrolling, selecting and finding are the terminal's. After
detaching, the session stays in that scrollback. It's an option and not
the default, because:

- **The user's terminal answers queries.** The app could stop its own
  answers; a terminal the user picked can't be stopped. So for
  `own_terminal` streams the daemon drops queries from the output
  (`backend/queries.rs`): device attributes, cursor and status reports, mode
  and setting requests, the version and keyboard protocol, window reports,
  colour and clipboard reads, iTerm2's reports, kitty's picture query (told
  by its control data, up to its first `;`), sixel graphics attributes,
  modifier-key reports, area checksums and the presentation state, ENQ and
  DECID. Everything else passes as it is, also when a sequence is split
  across chunks. Long strings (pictures, clipboard writes) stream through
  rather than being held.
- **Nothing in it may be wiped.** There's no RIS and no screen drawn ahead
  of the history, since that would land in the scrollback too. Each row is
  written from the start of its line, cleared first: the cursor and the
  lines may hold anything. Clearing after a full row would take its last
  cell on many terminals. Catching up
  after falling behind undoes the program's modes and sends only the screen:
  the history is already there, and sent again it would show twice. What
  was dropped stays missing.
- **The program sets the terminal's modes directly**, as over plain SSH. On
  the way out the CLI undoes them: the alternate screen, the charsets, the
  mouse and its encodings, keys, the scroll region, synchronized output,
  the keyboard protocols (kitty's flags, however many were pushed), and the
  cursor's shape and colour. The window's title, pushed at the start, is
  given back. It then moves below the screen. Ctrl-] also detaches when a
  program has turned on the kitty keyboard protocol (in any of its forms:
  press or repeat, never release) or modifyOtherKeys.
- **Verified:**
  - Unit tests for the query filter, including sequences split across
    chunks.
  - End to end against real tmux: a stream to the user's own terminal gets
    no queries while an app stream does (the control), and the user's
    terminal isn't reset.
  - The `otter` binary in a PTY: the history arrives, typing goes through,
    and Ctrl-] detaches and undoes the modes while the session goes on.

## D-121 — Sign-in pages from other sites: allowed on the Mac, or asked (2026-10-19)

`sf org open` and `sf org login web` opened nothing. Salesforce wasn't on
D-032's list of providers, the list couldn't be changed, and sf hides the
browser opener's refusal. Anyone using a sign-in site Otter didn't know had
the same silent failure.

- **Salesforce is built in:** `login.salesforce.com`, `test.salesforce.com`
  and `*.my.salesforce.com` (orgs, scratch orgs, sandboxes). The last is
  every org's own domain, wider than one company's sign-in page, as
  `*.awsapps.com` is. The list moved to `otter_protocol::sign_in`, so the
  daemon and the Mac use the same one.
- **Any other https page is offered to the Mac, not refused.** The daemon
  keeps it for the usual two minutes and emits `BrowserOpenAsked`, with the
  site only, no URL. The tool still hears an error, so it prints its link
  as it would. Its login waits on its callback or polls, so a page opened a
  moment later still finishes it.
- **The Mac decides.** Sites that open without asking are kept on the Mac,
  in `hosts.toml` beside the hosts (`sign_in_sites`), so the app and the CLI
  share them, and one list serves every host. A host can't widen it: before
  opening any page, the app or `otter attach` checks it again against the
  built-in providers and the Mac's sites. Even for a `BrowserOpenRequested`,
  the host's word isn't enough. The callback port the Mac maps is read from
  the page it opens, not from the event.
- **A host names a site; it never names a pattern.** The site in
  `BrowserOpenAsked` must be a plain host name (no `*`, no path), checked by
  the daemon and again by the Mac, or *Always open* could add a pattern the
  host chose (`*.co.uk`). Only the developer types `*.`, and only leading.
- **The app asks, in its corner:** "Sign in at login.corp.test?", which tool
  wants it (workspace and host), then *Open*, *Always open*, or not now. If
  the window isn't in front, a notification says so too. *Always open* adds
  the site and opens the page, so the login goes on without running the
  command again. The question goes when the host lets the page go (two
  minutes). If the page can't be opened (gone, taken by another Mac), the
  card says so instead of vanishing; if the site can't be kept, the page
  still opens and the card says it wasn't kept.
- **Settings › This Mac › Sign-in pages** lists the sites allowed, each with
  a button to ask again, and a field for a site or a pasted link (`*.` plus
  a domain for its subdomains). Below it is one line naming the providers
  built in.
- **`otter attach`** opens an asked page only if its whole site is allowed on
  the Mac, as the list was when it attached. It can't ask in a terminal
  that's busy with the session, so it leaves the rest to the app, untaken.
- **Verified:**
  - Unit tests for the shared matching (Salesforce's hosts, look-alikes,
    `*.`, path prefixes, what can be typed).
  - End to end: an unknown site is asked about with no URL in the log and
    taken once; a terminal opens only the sites its Mac allowed and leaves
    the others to the app.
  - App tests for the settings list and the question.

## D-122 — ⌥1…⌥9 switch sessions (2026-10-19)

- **In a workspace, ⌥1 to ⌥9 show the session in that place** among its
  tabs, as a click on that tab does. With two sessions side by side, the
  one on the right trades places. Each tab's tooltip says its key.
- **By the key's place (`code`), not what it types.** On a US layout, ⌥2
  types ™. The workspace takes the key before the terminal does, so nothing
  is typed into the session. A digit with no session in its place does
  nothing.
- **The cost:** layouts that type characters with ⌥ and a digit can't type
  them in the app's terminal. On a German layout, ⌥5 to ⌥9 are `[ ] | { }`.
  With Shift as well it's the terminal's key, and so is any key while a
  dialog is open.


## D-123 — Selecting text copies it again (2026-10-19)

Before D-118, tmux had the mouse: a drag selected in tmux, which sent the
text as OSC 52 when the button was let go, and it reached the Mac clipboard.
Since D-118 the app's terminal keeps the history and selects by itself, so
a selection stayed on the screen and only ⌘C copied it. The README still
said selecting copies.

- **Letting go copies.** When the left button is let go (in the terminal or
  outside it), a selection made by that press goes to the Mac clipboard,
  through the native plugin. That includes a drag, a double-click's word,
  a triple-click's line, a Shift-click that extends it, and an Option-drag
  over a program that has the mouse. The selection stays shown, and ⌘C
  still copies it.
- **Only a selection the press made.** A selection left from before isn't
  copied again. Otherwise a click in a program that has the mouse, after
  copying something in another app, would take the clipboard back.
- A program with the mouse (Claude Code's fullscreen mode, tmux's mouse
  on an older host) selects and copies by itself, through OSC 52, as
  before.
- **Verified:** app unit tests for a drag let go outside, a double-click's
  word, and no copy for a click, another button, a press outside the
  terminal, or a selection from before.

## D-124 — The coding agent runs through the SDK by default, and can be tested (2026-10-19)

The developer asked for the Claude Agent SDK as the default, and for a way
to see whether a host is ready for it. That departs from D-060 and D-068,
which kept `legacy_cli` until dogfooding was done. The developer chose not
to wait; the test is what makes the switch safe to see through.

- **`sdk` unless a host says otherwise.** A host that never chose a
  backend now runs new coding work through the worker. `legacy_cli` stays
  one click away (Settings › Coding agent, or `OTTER_CODING_BACKEND`).
  Work under way keeps the backend it began with (D-057). A host whose
  Settings were saved before has `legacy_cli` stored: the old dialog sent
  the backend it showed. It stays until it's changed. The new dialog sends
  the backend only when it's changed, so an untouched one follows the
  default.
- **`runtime.test`: one short reply.** The checks in
  `runtime.capabilities` say whether the agent is installed, not whether
  it's signed in. `runtime.test` starts the agent for real, the way a run
  does and with the same login environment. It runs in a scratch directory
  under the host's state and asks for one word. Every tool call is
  refused, and it's stopped once it answers: two minutes at most. It
  reports two steps, *Installed* and *Signed in and answering*. The second
  isn't tried when the first fails. A failure says what to do (sign in,
  check the model, wait for the rate limit).
  - Otter records nothing: no conversation, no task, and the scratch
    directory is removed. Claude Code still writes its own session under
    `~/.claude/projects/` on the host, as it does for any run.
  - The backend and model are the ones asked for, so the app tests what's
    on screen before it's saved. Absent, they're the host's. With no
    model set, it uses `haiku`: the reply proves the sign-in, and a small
    model costs least. One test runs at a time on a host; a second is
    told the first ends within two minutes. A client that goes away
    doesn't stop it sooner, and nothing is left running once it ends.
- **Where:** Settings › Coding agent › *Run a test* (the status says
  *Installed* until a test has run, then *Ready* or *Not ready*), and
  `otter runtime test [--host] [--backend] [--model]`, which exits 1 when
  a host isn't ready.
- **Tests:** the e2e hosts built on the fake `claude -p` ask for
  `legacy_cli`; the SDK hosts ask for `sdk`, as before.
- **Verified:**
  - End to end with the fake worker: ready, with a small model, the tool
    it tried refused, and nothing left behind. The same host through the
    fake `claude -p` with another model. A worker that can't sign in says
    so without its secrets. A host without the worker isn't tried.
  - The default is `sdk` (`settings.get`).
  - App tests for the button, its steps, the status and an older otterd.

## D-125 — The app updates itself (2026-10-19)

Asked for directly: the app should notice a new release and, on a click,
update itself; and the "Update command line tools" button at the foot of
the sidebar should go, since the app carries the tools (D-085). Until now an
update was a new `.dmg` from the release page, dragged over the old app
(D-019 left auto-update for later).

- **Tauri's updater, fed by the release.** Every release attaches
  `latest.json` (`scripts/update_manifest.py`, in the publish job, before
  the release turns public and latest), naming each architecture's
  `Otter_X.Y.Z_macos_<arch>.app.tar.gz` and its signature. Installed apps
  read it from `releases/latest/download/latest.json`.
- **Signed with a key of its own.** The updater takes only an archive
  signed by the minisign key whose public half is in `tauri.conf.json`. The
  release's bundle step signs with the private half (the repository secret
  `TAURI_SIGNING_PRIVATE_KEY`, no password) and uploads Tauri's archive as
  built — the `.sig` covers those bytes, so the release no longer tars the
  `.app` itself. Only the release turns `createUpdaterArtifacts` on, so a
  local `tauri build` needs no key. Losing the private key means no
  installed app can update again (a new key needs a manual reinstall): it's
  kept outside the repository, and backed up. This is apart from macOS code
  signing, which stays ad hoc (D-019); an archive the app downloads itself
  carries no quarantine, so Gatekeeper doesn't stop the new version.
- **Checked, offered, installed when asked.** A check 10 s after launch,
  then hourly (a release ships with every merge), and on demand from the
  app menu's "Check for Updates…" (a native dialog: up to date, or "Update
  and Restart"). A newer version shows as "Update to vX.Y.Z" at the foot of
  the sidebar, where the tools' button was. A click downloads it (the button
  counts the percent), checks the signature, swaps the bundle where it is
  (asking for an administrator only where its folder isn't writable) and
  restarts. Nothing installs without the click.
- **Only an installed copy.** Not a development build, a disk image or a
  translocated copy (`hosts::installed_app`, the test D-085's links already
  used): those can't be replaced, and "Check for Updates…" says so.
- **The tools follow; the daemon stays.** The bundle keeps its path, so the
  links in `~/.local/bin` (D-085) are the new version's tools once it
  restarts. This Mac's running otterd keeps its old binary: the host shows
  `otterd X` and its Install restarts it, as before — restarting the daemon
  stays the developer's button (D-085, D-061). The restart closes only the
  app, which stops no work (invariant 12).
- **No "Update command line tools".** It showed when `~/.local/bin/otter`
  wasn't the app's version: missing, or a copy a release installed (an app
  before D-085, `otter host install` on this Mac), which D-085's relinking
  at launch left alone. Now such a copy — a regular file that answers
  `--version` with `otter X.Y.Z` / `otterd X.Y.Z` — becomes a link to the
  app's own at launch too (off the main thread, since that runs it), so the
  tools never fall behind. Anything else there is still left alone, and
  nothing is added where nothing was: "Install Command Line Tools…" stays in
  the app menu, the Settings menu ("Command line tools…" once they're there)
  and the first-run screen. The cost: an `otter` built by hand and put in
  `~/.local/bin` is replaced at the next launch; earlier on `PATH` it isn't.
- **The first update is by hand.** Releases before this one have no
  updater: the first release with it is installed from its `.dmg`, and the
  next ones arrive in the app.
- **Verified:** the manifest for fake signatures of both architectures, and
  an error when one is missing; a file signed with the key verifies, the
  way the updater checks it (the public key from `tauri.conf.json`, the
  signature from the manifest), and with a byte added doesn't; app tests for
  the update button (offered, no tools' button while the tools report
  another version, a click installs and counts), its states, and a copy a
  release installed becoming a link while a program that only shares the
  name stays. **Not verified:** an update installed on a Mac, which needs
  two releases with the updater; the administrator prompt; the release's
  signing step, until its first run.

## D-126 — Pinyin's punctuation on the first press (2026-10-19)

With macOS Pinyin, punctuation typed with Shift (？！：（ and the like) took
two presses in the app's terminal.

- **The first ？ was dropped** (xterm.js #6144, in 6.0.0 and 6.1.0-beta.315;
  the fix, #6200, isn't merged). WebKit delivers an input method's commit
  before the character's own keydown: beforeinput, input, keydown. xterm.js
  drops typed text that comes while a keydown it saw isn't let go yet, so as
  not to send a key twice, and the last keydown it saw was the Shift's. The
  first press was lost; the next, after the first's keyup, got through.
- **Now,** when the last key pressed was a modifier alone (Shift, Control,
  Option, ⌘, Caps Lock, which switches the input source), a commit lifts
  that gate, a private field of xterm's, before xterm reads it. Only then:
  when the character's keydown comes first, xterm sends the text itself
  from its textarea, and lifting the gate would send it twice.
- **Not around a composition.** What a composition commits, xterm sends
  once it ends, with whatever came after it in the textarea: `ni` and then
  Shift+/ commits 你 and types ？ in one keystroke, and both go in that one
  send. Lifting the gate then sent the ？ twice (found in review). So the gate
  stays while composing, and a composition's end forgets the modifier.
- **Verified:** app tests with xterm.js itself: without the change the
  first ？ is dropped (kept as a control: when xterm.js fixes it, it fails,
  and the change goes); with it, two presses send two; punctuation without
  Shift goes once; a composition ended by ？ goes once, which fails when
  its end keeps the modifier; a keydown first still sends once, which fails
  when the gate is lifted on every commit. Not yet typed on a Mac.

## D-127 — Settings in tabs by kind (2026-10-19)

Settings was one long dialog. This Mac's font and sign-in sites sat
between its text size and the host's model provider. What applied at once
(this Mac's) sat next to what needed Save (the host's).

- **Tabs on the left, in two groups:** *This Mac* (Appearance, Sign-in
  pages) and *Host* (Otter, Coding agent, API keys), with the host chosen
  between the two groups. Each tab's head says where its settings are kept.
  The tab is remembered on this Mac.
- **Appearance** holds the theme (System, Light, Dark), which left the
  gear menu, and the terminal's font and size, with a preview. **Sign-in
  pages** shows the providers built in and the sites allowed here (D-121).
  **Otter** has the provider, its key, the model and Otter's tools.
  **Coding agent** has how it runs, its model, and the test (D-124).
  **API keys** lists every provider's key, which one is in use, and
  *Add…*, *Replace…* or *Clear*.
- **This Mac's settings apply as they change; the host's wait for Save.**
  A bar with *Discard* and *Save* appears only when a host setting has
  changed, and stays while another tab is shown. Only what changed is
  sent (an unchanged coding backend isn't). Clearing a key saves at once,
  as before, and keeps other keys typed but not saved. Closing (Esc, the
  backdrop, ✕) with changes unsaved asks first, in that bar: *Keep
  editing*, *Discard and close* or *Save*; closing again discards.
- **Answers that come late don't land on the wrong form.** A save's
  answer after another host was chosen is dropped. So is a test's
  answer after the backend or model changed: a test is of what was on
  screen.
- A host with no coding settings (an older otterd) has no Coding agent
  tab. With no host connected, the host's tabs offer *Add a host…*.
- `Dialog` takes a bare variant for this layout, and focuses the current
  tab when it opens. It now focuses once, not again each time its parent
  renders.
- **Verified:** app tests for each tab, the save bar, Discard, only
  changes being sent, closing with changes, late answers, the switch,
  the test, and no host. The tabs were
  rendered in Chromium, dark and light, at full size and narrow.

## D-128 — A performance overlay, under Settings › Developer (2026-10-19)

Asked for directly: the frame-rate readout the developer had been pasting
into the web inspector, in the window's top-right corner, as a setting that
is off by default, with other common measurements beside it.

- **Settings › Developer**, a tab of *This Mac* (D-127): one switch,
  *Performance overlay*, kept on this Mac (`otter.perf-overlay`) and applied
  at once. Off by default; while off nothing is measured — no frame loop, no
  timer, no polling.
- **What it shows**, once a second (measured every frame, drawn once a
  second, so it doesn't cost what it measures): frames per second (green
  from 55, red under 30) and the slowest frame; how late the main thread ran
  a 100 ms timer, at most; the page's element count; what the terminals were
  sent, per second; and the app process's memory and CPU. A hidden window
  draws no frames, so it shows "— fps" then, not 0.
- **Only what WebKit has.** The app's pages run in WKWebView, which has no
  `performance.memory` and no long-task observer: the JS heap shows only
  where the engine says (Chromium, not the Mac), and the timer's lateness
  stands in for long tasks. The memory and CPU are the app process's, from
  `ps` (`perf.rs`, every 2 s while shown): the pages themselves run in
  WebKit's own processes, which the app can't single out, so the line says
  "app".
- **The terminals' bytes are counted always** — one add per chunk in
  `Terminal`, so the first second shown is already right.
- **Verified:** tests for the switch (off by default, remembered, nothing
  sent to a host), the reduction of a second (frames, the slowest one,
  lateness, bytes, and no gap counted after the window was hidden), the
  overlay appearing and leaving with the switch, and `ps` read for the test's
  own process; the Developer tab and the overlay rendered in Chromium, dark
  and light. **Not verified:** the overlay in the Mac app itself.

## D-129 — Install the Claude worker from Settings (2026-10-19)

With the SDK the default (D-124), a host without the Claude worker can't
run coding work. The fix was `otter host install` in a terminal. The app's
host page only offers *Install* or *Update otterd* when otterd itself is
missing or out of date, and its install re-downloads otterd and restarts
the daemon.

- **`runtime.capabilities` says when the worker is missing**
  (`worker_missing`), so a client needn't read the notes. It's set only
  when no worker is found where this otterd looks. It isn't set when
  `OTTER_CLAUDE_WORKER` points at a development build that isn't there:
  a release's worker wouldn't be where it looks. `runtime.test` says it
  too, for the backend it tested. A host saved with the CLI reports
  capabilities of the CLI, so trying the SDK on screen finds the missing
  worker through the test.
- **Settings › Coding agent › *Install*** shows when it is. It installs
  only the worker, from the release of the otterd the host runs (its
  `host.status` version, which is where that otterd looks), with the same
  checks as `otter host install` (SHA-256, `--check`). otterd isn't
  touched or restarted, so nothing under way stops. The host is asked
  again afterwards. Found: *Installed*, and what it still needs if it
  isn't ready (Node.js, say). Not found: what the install said (a release
  with no worker for that platform, say), with *Install* still there to
  try again. Of a backend not saved yet the host can't say, so the install
  is shown and a test suggested.
- **Node.js isn't installed for you.** It's the host's (D-060): the note
  says which version is needed.
- **Verified:**
  - Unit: a development build pointed at but missing isn't
    `worker_missing`.
  - End to end: a host without a worker anywhere it looks says so, in its
    capabilities and its test; one with the fake worker doesn't.
  - App tests for the button, asking the host again, the SDK tried before
    saving, a release with no worker for the platform, and a failed
    install.

## D-130 — The history follows the finger (2026-10-19)

Scrolling a session's history in the app didn't keep up with the finger. The
session was inline, so the history was the app's own (D-118) and the wheel
never went to the host. The display was 60 Hz (a MacBook Air), so WebKit's
60 frames a second weren't the cause either (roadmap M7).

- **Why:** xterm.js 6 draws whole rows. Its scroll position is in pixels,
  but the picture changes only when the position crosses a row: 21 px, at
  the app's 13 px font and 1.25 line height on a Retina display. Measured
  with xterm.js itself in headless Chromium, a real Claude Code history
  (5,053 lines), and trackpad input as WebKit reports it, one report a frame.
  At 120, 300, 600 and 1,200 px/s of finger travel, the text moved in 17,
  40, 73 and 119 of 120 frames, and was up to 10 px from where the scroll
  position put it. A slow read looked like 8 to 20 frames a second, a row
  at a time.
- **Now** the screen is moved by what's left past the row drawn
  (`translateY`, at most half a row), in the same frame as xterm draws that
  row. The move is in whole device pixels, so the text isn't blurred.
  150 ms after scrolling stops, the position settles on the row drawn, eased
  over 80 ms. xterm.js #6039 does the same inside xterm and isn't merged.
  This reads xterm's viewport, scroll position and cell size, which are
  private, and does nothing if they're missing. It goes when #6039 lands.
- **Settling waits for the position to stop, not the output** (found in
  review). xterm's viewport also tells of a scroll when output only
  lengthens the history; counted as scrolling, a stream of output kept the
  shift, and its blank edge, for as long as it flowed. A scroll that comes
  just as it settles follows the finger, not eased.
- **When the position and the row disagree** by more than half a row, the
  row wins at once. That happens when the cells change size (⌘= / ⌘-, a
  font loading), or when xterm moves the row itself, as when output arrives
  with the history full.
- **Left as it is:**
  - While moving, up to half a row at the top or bottom edge is blank,
    because the row partly in view isn't drawn.
  - A mouse wheel's notch moves 50 px, about 2.4 rows, and settles the rest
    after it stops.
  - At rest there's no shift, so clicks and selection see the screen as
    before.
  - Full-screen programs (another screen, or one that takes the mouse, such
    as Claude Code with `CLAUDE_CODE_NO_FLICKER=1`) still scroll themselves,
    on the host (D-117).
- **Verified:**
  - In the same harness, the text moved in 119 of 120 frames at every speed
    (the first frame has nothing to compare with), exactly where the scroll
    position put it. It never moved more than half a row, always on whole
    device pixels, and was on a row with no shift once scrolling stopped.
    Nothing was left at the bottom, on the other screen, or once undone.
    With output written every 40 ms while scrolled up part way into a row,
    it settled on a row 150 ms after the scroll; before the fix it stayed
    shifted until the output stopped.
  - App tests, against a fake of xterm's viewport: the shift and its
    rounding; settling once and only once, which fails when settling's own
    scroll starts another; settling on time while output lengthens the
    history, and a scroll just as it settles not eased, both of which fail
    without the review's fix; following the finger again at once; the row
    winning a disagreement; another screen; an xterm without those parts;
    undoing.
  - Not yet scrolled on a Mac.

## D-131 — Update every host at once (2026-10-19)

Each release, every host shows its older otterd in orange in the sidebar,
and each was updated on its own page. Now the Hosts heading has one
button for all of them.

- **Which hosts:** those whose otterd is older than this app, connected or
  reporting an incompatible otterd. Never one that's newer (its "update"
  would go back a release), one that's unreachable, or one without otterd
  (its page installs it).
- **What it does:** the same install and restart as each host's page
  (`host_install`), on all of them at once. Sessions keep running
  (invariant 4). A host being updated says *updating…*; one that fails
  says *update failed*, with why in its tooltip, and the button offers it
  again. Updated hosts come back on the new version and drop out.
- **Room:** *Update all* (or *Update* for one) when the list is wide
  enough to keep the summary whole; otherwise the orange arrow alone,
  its words in its tooltip and label.
- **Verified:** app tests for which hosts are picked, both at once, the
  states while updating and after a failure, and no button when all are
  current. Rendered dark and light, wide, usual and narrow.


## D-132 — The repository's copy holds only the workspaces' branches (2026-10-20)

`git clone --bare` (D-010) copied every branch the remote had into the
copy's refs/heads, and nothing moved them afterwards. Three things went
wrong from that: `--branch main` checked out main as it was on the day of
the clone, with no upstream; a branch deleted on the remote left a copy
holding commits on no remote, which kept the whole copy after its last
workspace went (D-108); and a branch merged by squash or rebase, then
deleted, looked like unpushed work once a fetch pruned it, so deleting its
workspace asked for force.

- **New copies:** `git init --bare`, `remote add origin` (a relative local
  path made absolute, as `clone` did), fetch with tags. The remote's
  branches are only `origin/*`; refs/heads holds what workspaces made.
- **Copies from before** drop what their clone copied, once (marked by
  `otter.ownBranchesOnly` in the copy's config), when a workspace is
  prepared on them or their last one is deleted. A branch goes only when
  the clone packed it, it has no reflog, and no worktree has it. The reflog
  is the guard: any branch committed on here has one, even after a gc has
  packed it (the loose-ref check only spares branches a workspace made and
  never used). So with `core.logAllRefUpdates` off nothing goes, and each
  branch dropped is logged with its commit.
- **A branch asked for by name** that is here but behind `origin/<name>`
  is caught up, with that as its upstream. One with commits of its own is
  left as it is.
- **Landed work isn't unpushed:** commits on no remote still count unless
  the branch's work is on the default branch under other commits, compared
  by patch id: every one of those commits (a rebase merge, `git cherry`),
  or the whole branch as one commit on its merge base (a squash merge).
  Work added since makes the patches differ, so it still needs force. So
  do: a branch with no net change (an empty patch would match any empty
  commit upstream), a merge commit of its own under a rebase merge, a
  merge into another branch than the default, and a patch that changed on
  the way in (conflicts resolved, neighbouring lines edited).
- **Verified:** e2e tests for a fresh copy (only its workspaces' branches,
  `--branch main` at the remote's tip, a branch deleted upstream doesn't
  keep it), an older copy (cloned branches dropped when preparing and when
  deleting, a branch with work kept, nothing dropped with reflogs off, a
  cloned `main` caught up), and squash and rebase merges (no force; work
  added since and a net-zero branch still ask). Each fails against the code
  before, and each guard fails its test when taken out. Reviewed by a
  second model, which found the net-zero and reflogs-off cases.

## D-133 — Otter's own git on a host: identity, key, GitHub sign-in (2026-10-20)

Otter used the host's git as it found it (D-010, D-047). On a host shared
with another account, or set up as someone else, Otter's commits, pushes
and pull requests went out as whoever the host was. The developer wants
their own identity and credentials set once in Otter, used on every host,
without changing anything else on the host.

- **Only Otter's repositories.** Each repository under `~/.otter/repos`
  (its worktrees share its config) includes one file Otter writes,
  `state/git/gitconfig` (`include.path`, added when a repository is cloned
  or prepared, and to every one at startup). A change in Settings reaches
  them all at once. Nothing else changes: not `~/.gitconfig`, `~/.ssh`,
  the host's agent or the host's gh. Otter's own commits, the coding
  agent's and the developer's in Otter's terminals all use it, since they
  all run git there.
- **Identity:** `user.name` and `user.email`. Cleared, the host's own
  apply again.
- **Key: made on the host.** It's ed25519, `state/git/id_ed25519` (0600),
  and the private half never leaves the host (the developer's choice over
  copying one from the Mac). GitHub's host keys go to
  `state/git/known_hosts`, not the host's.
  - The key is used only once it works. `git.check` runs `ssh -T
    git@github.com` with it; at "Hi alvin!" it writes `core.sshCommand`
    and `url.git@github.com:.pushInsteadOf=https://github.com/`, so pushes
    to an https remote go over the key too.
  - That key alone: `-F /dev/null` as well as `IdentitiesOnly` and no
    agent. Without `-F`, ssh would also try the `IdentityFile`s of the
    host's `~/.ssh/config` after it, and sign in as the host's account
    (found in review). A GitHub reached only through the host's ssh config
    (a `ProxyJump`, say) isn't reached this way.
  - For GitHub alone. A repository's `core.sshCommand` is for all its
    remotes, so it runs a script Otter writes (`state/git/ssh`, 0700, in
    place already executable): a `github.com` or `ssh.github.com` target
    goes to the key, anything else to the host's ssh exactly as before.
  - GitHub's published host key is in `known_hosts` from the start, so the
    first connection is checked, not trusted.
  - Fetches stay as they were, so a key GitHub doesn't know yet can't
    break them. A key GitHub turns away stops being used; one GitHub
    can't be reached for stays as it was.
- **GitHub sign-in: Otter's gh alone.** Its config directory is
  `state/git/gh` (0700), and its token is stored there rather than in a
  keychain (`--insecure-storage`, so it can't meet the host's own gh login
  there). `GH_TOKEN` and the like are taken out of its environment, which
  would otherwise win over it.
  - In the browser: `gh auth login --web` without a terminal prints a code
    and a page and waits. *Amended 2026-10-20:* gh 2.101 prints the code
    as `One-time code (XXXX-XXXX) copied to clipboard`, not after
    `one-time code:` as before, so the code is read from either: the
    `XXXX-XXXX` after "one-time code", in any case. Before this, sign-in
    from a host with gh 2.101 waited 30 s and gave up. `git.github_login` returns once it has them; the
    view carries them (`login: pending {code, url}`) until gh finishes,
    then says who it is.
  - With a token: on gh's stdin.
  - A sign-in GitHub won't say the account of isn't kept: its directory
    goes. Signing out or in again first stops a waiting sign-in and waits
    for its gh to be gone, so it can't write a token afterwards.
  - It asks for `admin:public_key`, so `git.key_add` can put each host's
    key on the account (`gh ssh-key add`, "Otter on <host>"). Without that
    scope, the client shows the public key to add on GitHub.
  - Only Otter's own delivery uses it (D-047). The agent and the terminals
    don't get the token (the developer's choice). They push with the key,
    and the agent's push still needs the developer (D-059). The agent's
    policy counts Otter's state directory and gh's `hosts.yml` as
    credentials.
- **Settings › Git & GitHub**, in a group of its own, *All hosts*.
  - The identity is set once and kept on the Mac in `hosts.toml`, with the
    CLI's. It's sent to every connected host, and to each host as it
    connects when that host has another.
  - It's kept only if a host took it, so one a host refuses (not an email
    address) isn't kept. An otterd from before D-133 accepts the update and
    ignores it, so it's reported as too old.
  - Cleared, it's kept as cleared, so a host that was away is cleared too.
    Closing Settings over an unsaved identity asks first.
  - Each host has its own key and its own sign-in, so a browser sign-in is
    one code per host. Each host's row offers its one next step: make a
    key, sign in to GitHub, add the key to GitHub, or check. *Set up all
    hosts* does them all, adding each key as its sign-in ends.
  - A code of a sign-in started in this window goes to the clipboard and
    GitHub's page opens. One begun elsewhere only shows its code, with
    Copy, Open GitHub and Cancel.
  - A pasted token signs every host in, making missing keys first.
- **Known limits:** `GIT_AUTHOR_*`/`GIT_COMMITTER_*` in a host's login
  environment win over a repository's identity. A project's `.envrc` can
  still set what it sets. gh 2.40 or newer is needed (older ones are told
  to update).
- **Verified:**
  - App tests for the tab: the identity sent to every host and a host
    that didn't take it, one host step by step, all at once, a token,
    a sign-in from elsewhere and its Cancel, an older otterd, and closing
    over an unsaved identity.
  - Unit: the gitconfig written (escaped, the key only once it works),
    what gh and GitHub say, Otter's gh never taking `GH_TOKEN`, and an
    identity on one line.
  - End to end with a fake gh and ssh (real git and ssh-keygen):
    - A commit in a worktree is the developer's; the source repository
      isn't touched.
    - No `core.sshCommand` until the key works. A key made on the host is
      0600.
    - A bad token says why. Sign-in never takes the host's `GH_TOKEN`.
    - Once the key is added through the sign-in, the repositories push
      with it, through the script: GitHub with `-F /dev/null` and the
      key, a GitLab remote with the host's ssh untouched. GitHub's host
      key is there from the start.
    - A token GitHub won't name the account of isn't kept. Signing out
      while a browser sign-in waits leaves nothing signed in, even when
      the code is entered afterwards.
    - The browser sign-in shows its code, then who it is. A cleared
      identity leaves the host's.

## D-134 — A task that stops says why, and can go on (2026-10-20)

A task asked in Chinese failed in planning seconds after it started, twice
more on retry, and was deleted, its history with it. The cause was in
otterd, not the agent: `redact` (D-075) took the byte after the character
before a key as the key's start, so text with a multi-byte character just
before `word:` or `word=` (`修复这个PR，https://…`, a BOM in a CI log)
panicked. The panic ended the run's actor; the bridge failed the run as
"ended unexpectedly", and the task said the coding agent's look at the
code had failed. Retrying couldn't start: the conversation still named the
lost run as its active one until a restart. Nothing of this was in the log
but two bare panic lines, and a panic in the controller's step would have
left the task marked as being stepped, never stepped again.

- **`redact`** finds the key's start by characters. Text in any script
  reads whole.
- **A lost run frees its conversation:** when the actor or the bridge goes
  without ending the run, its conversation's generation ends (outcome
  unknown; queued turns stay queued, as on a restart) if the run still
  holds it, and the run ends as one cut off mid-turn (D-061): resumed when
  its session can carry on, the developer's to decide after three in a
  row. It says otterd lost it.
- **Contained:** a task's step, Otter's follow-ups, its turn on the host
  and a run's bridge each run as a task of their own. A panic there is an
  error like the others: the task fails with *Otter stopped: an internal
  error*, the host's turn can run again, a lost bridge lets its run and
  workspace go. A step that panicked answers what the developer said with
  that, so it isn't taken up again at once — and so does Otter's turn on
  the host, and a follow-up that panicked is said plainly instead; failing
  a task that already failed changes nothing and wakes nothing. A run lost
  after its last turn completed ends completed, as a restart would find
  it.
- **In the log:** a panic, with its time, thread and (with
  `RUST_BACKTRACE`) backtrace, through the log like the rest; a
  task failing, with why (redacted); a task deleted, with its last status
  and why. A failed Otter's turn no longer quotes Node's warning hint as
  what the worker said.
- **Wording:** a failed look at the code says *the look at the code
  failed (why)*, not that the agent's did, and that retrying tries again.
- **A workspace only the developer can free needs them:** a task waiting
  because an agent they started works in its workspace, or because the
  workspace is archived, is *Waiting on you* (Inbox, `otter ls`), not
  working. Waiting for a workspace being prepared, or for another task,
  isn't. `WAITING`, `YOUR_AGENT` and `ARCHIVED` move to `otter-core` so
  the rule and the words are one.
- **CLI:** `otter task retry`; `resume`, `retry` and `cancel` say what
  they do.
- **Known limits:** a step that panics every time on a task still being
  worked fails it once; one that panics on the developer's every message
  answers each with the failure. A panic while a lock is held poisons it,
  which containing can't undo. The *Waiting on you* of a workspace wait
  counts from when the task entered its status, not from when the wait
  began. A panic quoting a string (a slice out of bounds) quotes up to 256
  bytes of it unredacted, as before: redacting in the hook could panic
  again and abort otterd.
- **Verified:** unit tests for the scripts (`，https://`, a BOM, `看看token=`,
  a key after Chinese still masked), a panicking step as an error, and the
  waits that need the developer. An e2e test (`a_request_in_chinese_is_planned`)
  with the fake agent saying Chinese back: it fails on the old `redact` with
  *otterd lost the run*. By hand, with the old `redact` back: the task
  failed, and Retry planned and went on; with the conversation's release
  taken out as well, Retry failed with *run … is still serving this
  conversation*, as on the developer's host. After review, the lost run
  is resumed instead: by hand, it was Interrupted, then resumed and
  finished on its own. Reviewed by a second model, which found the
  answer-loop, the resume and the bridge.

## D-135 — A checkout cut short is made again; deleting doesn't hold the host (2026-10-20)

otterd killed `git` when it stopped, which it does on each update (D-125),
and when a workspace was deleted while preparing. A `git worktree add`
killed mid-checkout leaves its worktree locked "initializing" with files
missing, and the workspace's record without its branch. The next otterd
saw the `.git` and called it ready: agents started in a tree with files
missing, and with no branch recorded, deleting it neither checked for
unpushed commits nor deleted the branch (D-108). Deleting it failed, too:
`git worktree remove --force` refuses a locked worktree. Killing `git` didn't
stop the child that checks out, which went on writing into the tree. A
worktree whose directory went missing couldn't be made again (git:
"missing but already registered"), and each try left an `otterd/<name>-2`.
Apart from that, `workspace.delete` removed the worktree, the directory and
the repository's copy holding the store's lock, so every request on the
host and the reconcile loop waited, often for gigabytes; and
`workspace.create` waited up to 3 s for a Git workspace's fetch and
checkout, which hardly ever finish that soon, so the app's dialog sat on
"Creating…" though the workspace opens while it prepares.

- **What git has there decides, not the record alone.** A branch is
  recorded only once `git worktree add` finishes, so one missing is the
  sign to look. But before this change a checkout cut short was called
  ready without its branch, and may have been worked in since (found in
  review). So a worktree is *nobody's work* only when git says its add
  never finished (its registration locked "initializing", read from
  `worktrees/<id>/locked`, which git-worktree(1) documents) and nothing has
  ever run in the workspace. Sessions start only once it's ready, so one
  cut short under this otterd has run nothing.
  - **Nobody's work, or gone:** made again. What's there goes first:
    `worktree remove --force --force` (git's way to remove a locked one),
    `worktree prune`, then an empty directory. The branch the cut-short
    checkout made goes too if nothing is on it (no remote branch of its
    name, no commit a remote lacks), so the new one has the same name. A
    worktree made before goes back on its recorded branch, with its
    commits, and keeps its recorded base.
  - **Any other worktree is kept as it is,** with whatever is in it,
    unlocked if git left it locked, and its branch recorded, with a base
    as `git worktree add` would have given it (the base asked for, or
    origin's default branch, and their merge base). That covers one
    finished whose branch otterd never got to record, too.
  - **Left alone, and the workspace fails saying why:** a directory with
    files and no worktree of the repository, and a worktree gone from its
    directory whose registration someone other than git locked (git's
    lock for a worktree on a disk not always there).
  - **Known limit:** a worktree moved away by hand, rather than with `git
    worktree move`, loses its registration when its workspace is prepared
    again, and `git worktree repair` can't link it back after that. Otter
    prunes registrations whenever it deletes a worktree anyway, as git's
    gc does after a while.
- **Git runs in its own process group,** killed whole when the call is
  dropped (a preparation cancelled, otterd stopping), so a checkout's child
  doesn't outlive it.
- **Deleting** removes a locked worktree, and the registration of one whose
  directory is gone. Without force it checks for uncommitted changes
  whenever there's a `.git`, and for unpushed commits on the branch it
  finds there when none is recorded, unless the worktree is nobody's work
  (above). A preparation under way is cancelled, and waited for, before
  anything is removed. If the delete then fails, the workspace is failed,
  saying so ("deleting it failed: …"), rather than left preparing with
  nothing preparing it. While a delete is under way, another delete of the
  workspace, `workspace.prepare` and `workspace.unarchive` get `conflict`
  ("being deleted"), and a preparation that finishes meanwhile starts
  nothing.
- **Deleting holds the store's lock only to stop the processes and to drop
  the record.** Running sessions stop first and are marked stopped
  (`SessionStopped`), as when archiving. The worktree, branch and directory
  then go without the lock, and the record last, under it. A failure on the
  way still leaves the record, so deleting again retries. The repository's
  copy goes after that, and its last-workspace check (D-108) is made under
  the copy's Git lock: a workspace being made on it is either counted, or
  fetches only once the copy is gone. The store's lock is never held across
  Git now, so progress messages wait for it rather than being dropped when
  it's busy. They're set around Git's calls, not from inside them: a first
  clone reads "cloning repository" until "creating worktree".
- **`workspace.create` waits** (up to 3 s, as before) **only when there's no
  Git work:** an empty or existing directory, or for `workspace.prepare` and
  `workspace.unarchive` a worktree already there. A Git workspace comes back
  at once, preparing. The CLI already waits for it, and the app already
  selects the new workspace by its id, which comes back with the response
  (unchanged). A preparation that ends, ready or failed, wakes the
  controller, so a task given a worktree of its own (D-080) steps on at
  once. A task whose workspace failed to prepare waits, saying why
  ("preparing it failed (…); prepare it again to go on"), rather than
  starting its agent there. That includes an environment that failed
  (D-011) over a worktree that is fine, where a task used to run anyway.
- **A `git worktree add -b` that fails** deletes the branch git made for it,
  so trying again doesn't take `-2`, `-3`.
- **Verified:** e2e tests in `crates/daemon/tests/e2e/workspace_prep.rs`.
  - A checkout cut short, then otterd restarted: made again on
    `otterd/cut`, whole, unlocked, its session started.
  - A locked worktree deleted, and a half-made one deleted right after a
    restart, without force.
  - A worktree gone: made again on its branch, with its commit. One whose
    registration the developer locked: failed, lock kept.
  - With a `git` that sleeps: create returns at once, `workspace.get`
    answers while a delete removes the worktree, and a delete while
    cloning leaves no child of git behind.
  - Each fails against the code before: ready with a file missing,
    "cannot remove a locked working tree", failed on `otterd/gone-2`,
    3.0 s, 3.5 s, and a child that wrote after the delete. A unit test for
    the `-b` branch fails without its cleanup.
  - The review's case, a record ready without its branch, worked in (a
    commit, an uncommitted file): delete without force refused, for the
    file and then for the commit; prepare and unarchive keep both and
    record `otterd/legacy` from `origin/main`; locked "initializing" as
    well, it's kept and unlocked. A task on a failed workspace waits
    saying why, and goes on once it's prepared. A delete that fails after
    stopping the preparation leaves the workspace failed. These fail
    against this change's first version: the delete went through and
    lost the file, the task's agent started in a folder that was gone,
    the workspace stayed preparing, and a lock was overridden.

## D-136 — A prompt that redraws is still a prompt (2026-10-20)

An approval or a question from an interactive Claude Code session went away
about 2 s after it was raised and never came back, while Claude still
waited at its prompt. D-035 took a prompt as answered once the screen
changed more than 2 s after it appeared, so that a long approved command
shows as working before it ends. The screen's last change only moves
forward, so any change after those 2 s counted, for good. In the live
event log, approval items lasted 1.5–3.0 s: the 2 s plus one look. Two of
them were `rm -rf` commands stopped by Claude Code's own safety check in
auto mode: their prompts stayed up for 120 s and then timed out. Another
question was cleared 0.5 s after it was raised by the `PostToolUse` of an
`Agent` call made alongside it, then raised again by the 6 s `Notification`
as an approval with no detail.

Measured on Claude Code 2.1.304 through a dev otterd, capturing the screen
as otterd does, every 250–500 ms:

- A prompt alone is still: a Bash approval in manual mode for 36 s, a
  question in auto mode for 42 s, a question with a background shell
  running for 40 s. Not one change.
- A background shell ending changes it once (its "· 1 shell still running"
  goes). otterd as it was resolved the question then, with the question
  still on the screen.
- A prompt with an auto-deny window (the safety check; auto mode after
  repeated denials) shows "Claude Code will automatically deny this request
  in M:SS, …", redrawn every 1000 ms: a change at every other look. Read
  from 2.1.304's code; not reproduced live (the probe command was refused).
- A running tool changes it at every look: the spinner (120 ms frames), the
  tool's blinking ●, the elapsed time.
- No hook says a prompt was allowed. 2.1.304's events have
  `PermissionRequest`, `PermissionDenied` and `PostToolUse`, nothing for
  "allowed". `PermissionRequest` carries no `tool_use_id`.

The change:

- **The screen's motion, not its last change.** Each look notes since when
  the screen has changed at every look (`Screen::moving_since`). A look
  that sees no change, or can't capture, ends that. A prompt counts as
  answered once it has been drawn (2 s) and the screen has kept moving for
  2 s: five changes in a row at 500 ms looks. Neither a one-off change nor
  a once-a-second redraw can make that. The motion may have begun before
  the prompt (approved at once).
- **Kept once seen.** `answered` is kept with the prompt until a hook or
  the transcript ends it, so a spinner's hiccup doesn't raise the item
  again. Going back to blocked would resolve and re-raise it: a new
  notification each time, and an `otter ack` undone.
- **Another tool ending isn't the answer.** The prompt keeps the tool it is
  about. `PostToolUse*` clears it only for that tool, or when either is
  unknown (a prompt kept by an older otterd).
- D-035's "attaching reads as answered" is gone: a resize is one change.
- **Known limits:**
  - With `prefersReducedMotion` (or a screen reader) Claude Code's spinner
    stands still, so a running tool may only tick its elapsed time, once a
    second. An approved long command then stays "needs your approval"
    until it ends.
  - Whatever keeps a prompt's screen moving at every look for 2 s still
    reads as an answer: a window dragged larger that long, an answer typed
    or navigated that long, two once-a-second redraws out of step.
  - With looks 750 ms or more apart (an otterd that falls behind), a
    once-a-second redraw can pass for motion.
  - Two calls of one tool side by side: the other's `PostToolUse` still
    clears the prompt.
- **Verified:**
  - Unit (`hooks_drive_prompts_and_only_a_moving_screen_tells_an_answer`,
    which replaces `…_and_the_screen_tells_an_answer`): one change, a
    countdown (with two changes at neighbouring looks), and the turn's
    spinner through the draw leave it blocked. Two seconds of motion make
    it working, kept through a hiccup. Approved at once, it is working
    once drawn. An `Agent` call's `PostToolUse` leaves the question up.
    The one-change, countdown and `Agent` cases each fail against the code
    before.
  - End to end (`a_ticking_prompt_is_not_an_answer_but_a_running_tool_is`):
    a fake Claude Code whose prompt redraws once a second is still blocked
    5 s on, with the screen changed, and the approved tool's spinner reads
    as working before its `PostToolUse`. Fails against the code before
    (working by then).
  - Live, through a dev otterd and Claude Code 2.1.304: a background
    shell ended while a question was up. otterd as it was resolved the
    item; this one keeps it.

## D-137 — A terminal on screen is looking, and two smaller fixes (2026-10-20)

Amends D-099. Only attaching counted as looking, but the app's terminals
stay attached (D-100) and say whether they're on screen (D-119). A turn
that ended in front of the developer raised a review that stayed in the
sidebar, the tray and the Inbox until they clicked into the session they
were watching. In dogfooding, of 46 review items that lived under 30 s, 43
were settled by an attach, not by the agent moving on.

- **On screen is looking.** A stream that takes control frames (the app's
  terminal) looks from its attach until it says it's off screen (`view`),
  and again once it says it's back. otterd keeps each terminal on screen,
  since two can show one session (D-110). It's all in the attach bridge
  and `attention.rs` (`OnScreen`); the backend knows nothing of it.
  - Back on screen settles the session's finished turn or completed
    command, as attaching does.
  - While a session is on screen, a turn ending or a command completing
    raises nothing. Raising it and settling it at once would still have
    the app notify (D-109). The new state still replaces what was open,
    such as an approval the agent moved past.
  - Approvals, questions and failures are unchanged: looking never
    answers them (D-099).
  - On screen is the app's terminal having a size in a window the system
    doesn't report hidden (`document.visibilityState`), focused or not. So
    a turn that ends in a terminal left on screen while the developer
    works in another app no longer notifies "turn finished".
  - The CLI's attach doesn't say whether it's on screen, so it still looks
    only when it attaches.
  - **A client gone without a word** must not hold a session on screen.
    When a Mac sleeps, the host's sshd and `otterd dial` stay up, so the
    attach lives until the connection fails: about 15 minutes with output
    flowing, and up to about 2 hours idle where sshd doesn't probe. Every
    finished turn in that time would be lost, even after the app came back
    and the developer looked elsewhere. Two things end it sooner:
    - A frame that hasn't reached the client in 30 s ends the attach as if
      the client had gone (`OTTER_ATTACH_WRITE_TIMEOUT_MS`). The final exit
      frame is bounded the same way. Before, the write could wait for ever
      once the buffers on the way were full. That kept a stuck attach, and
      its terminal client, for as long as the connection took to fail.
    - On screen lapses. The app says it again every 30 s while a terminal
      is on screen; one otterd hasn't heard that from in 90 s no longer
      counts. Said again, it counts again, but settles nothing: a timer
      isn't the developer looking. Only coming back on screen settles.
    - The 90 s applies to every client that takes control frames. An app
      from before this never says it again, so after 90 s its terminals
      stop counting and attention behaves as it did before this decision
      (D-099), never worse. That mix is short-lived, since the app brings
      and updates otterd (D-085, D-131). The other way, giving the limit
      only to a client once it has said it again, would leave the first
      30 s of every attach without it, and a Mac put to sleep then would
      hold the session on screen until the connection failed.
  - The app's half of D-100 (showing a workspace again settles by id) now
    mostly finds nothing to do. It stays for older daemons.
  - The `view` message is unchanged (protocol.md says what it now means),
    so there's no version bump.
- **Claude transcript discovery** (D-023). Until a Claude session's
  transcript is written during its execution (the trust dialog, or a
  resumed conversation not yet continued), each reconcile read the
  project's transcript directory and stat'ed every file in it, twice a
  second. That directory keeps every conversation had in the cwd.
  - It's read again only when its mtime changed (a new transcript), or
    after 5 s. A resumed older conversation (`/resume`) is only appended
    to, which doesn't change the directory, and neither does a second file
    made within one clock tick.
  - A new execution looks at once.
  - The last look is kept in memory. In `provider_state`, every look would
    be a save.
- **Real-browser tests apart.** Chrome on CI's runner sometimes stops
  answering, which failed the whole test job until rerun.
  - The tests that drive a real Chrome run only with
    `OTTER_TEST_BROWSER=1`, and fail if it's set and no Chrome is found.
    They are `browser::tests`' three and the three end-to-end browser
    checks. The end-to-end ones are included because they took
    `OTTER_BROWSER_NO_SANDBOX` from the main job's environment, and
    without it Chrome can't start on the runner.
  - CI runs them in a "Real browser" job, up to three times. A flake
    passes on the next try with a warning, and a real break fails all
    three and the job. (`continue-on-error` would have hidden a real
    break: with nothing required to pass, a green PR is merged.)
  - Locally, with Chrome or Chromium and python3: `OTTER_TEST_BROWSER=1
    cargo test -p otterd -- browser::tests
    a_browser_check_verifies_the_ui_with_evidence
    a_broken_ui_fails_verification a_preview_runs_as_a_session_on_one_port`.
    Add `OTTER_BROWSER_NO_SANDBOX=1` where Chrome's sandbox can't start.
- **Verified:**
  - End to end (`a_turn_that_ends_on_screen_is_already_seen`):
    - An approval stays while its terminal is on screen, and when it goes
      off and back.
    - Answered in the terminal, the turn ends with nothing raised, while
      a second terminal on the session is off screen.
    - A turn that ended off screen is a review until its terminal is back
      on screen.
    - Against the code before, both halves fail: a review is raised, and
      it stays. Keeping one mark per session instead of one per terminal
      fails it too.
  - End to end, a command (`a_command_that_completes_on_screen_is_already_seen`):
    one that completes on screen raises nothing, and one that completes off
    screen is a completion. Ignoring the terminal for a command fails it.
  - End to end, a client that goes without a word
    (`a_terminal_gone_without_a_word_is_no_longer_on_screen`): once its
    attach is over, the turn that ends is a review. Keeping the mark when
    the attach ends fails it.
  - End to end, a client that stops reading
    (`an_attach_whose_client_stops_reading_is_let_go`, at 1 s): the attach
    is let go while the session runs on. Without the timeout, it waits for
    ever.
  - Unit (`a_terminal_counts_as_on_screen_while_its_client_says_so`): two
    terminals on one session, one going; a mark lapsing at 90 s; said
    again, counting for longer; a lapsed one back, not as coming on screen.
  - The app's tests pass, and so does its type check.
  - Unit (`transcripts_are_looked_for_only_when_there_may_be_a_new_one`):
    - An appended older transcript is found by the rescan at 5 s, not
      before. A new transcript is found on the next look.
    - A new execution looks at once, and a clock gone back looks again.
    - It fails with a look every time (the code before), and with the
      time alone (no mtime).
  - The browser tests skip without the variable, and fail with it when
    there's no Chrome (none on the machine this was built on). On this
    PR's CI, the "Real browser" job ran all six and they passed, and the
    main job skipped them.
  - The whole `cargo test` passes, and the new end-to-end tests passed
    repeated runs, eight at once included. The command test failed twice
    on its first two runs, as if on screen had been ignored, and never
    again in about 25 runs since. Nothing found explains it.


## D-138 — Looking for a risky word isn't doing it (2026-10-20)

Policy's word lists (D-049, D-066) read each piece of a command line
whole, so a command that only searched for a word was taken for doing it:
`grep -rn token src`, `git log --grep delete`, `cat docs/deploy.md`,
`cargo test prune_old` each asked the developer, every time, never
grantable. A task on authentication code asked at almost every search.

- **The command, not what it looks for:** when a piece only reads,
  searches or tests — `grep`, `rg`, `ag`, `ack`, `cat`, `head`, `tail`,
  `wc`, `ls`, `tree`, `stat`, `file`, `diff`, `find`; `git log`, `show`,
  `diff`, `grep`, `blame`, `status`, `shortlog`, `ls-files`; `cargo test`,
  `go test`, `pytest` — the word lists read only the command. Patterns,
  option values, test names and the workspace's own file names aren't what
  it does.
- **Only when nothing can make it do more:** the piece is read whole when
  an environment is set for it (`GIT_DIR=…`), an operand holds a
  substitution, a redirection from a file or a variable, an option runs a
  program, writes or brings in another project (`--pre`, `--pager`,
  `-exec`, `-toolexec`, `--config`, `--manifest-path`, `--output`,
  `--ext-diff`, `--textconv`, `-O`), `find` acts (`-delete`, `-exec`, …),
  or an operand, a short option with a path (`-f/…`), an option's value
  (`--to-file=…`) or a key's (`cache_dir=…`) reaches outside the workspace
  (or scratch). Each word is read as the shell passes it, roughly — its
  quotes and backslashes gone — so `"--pre=…"`, `\-\-pre`, `-ex''ec` and
  `\/etc/…` are what they spell; a short acting option with its value
  glued on (`-Ox`) is still that option, and so is one with two dashes
  for one (Go takes either). The word lists read the words that way too,
  so `dele''te` is `delete`.
- **Credentials by file name stay read whole:** `.ssh`, `id_rsa`,
  `id_ed25519`, `credentials`, Otter's own state and gh's hosts file ask
  wherever they appear, whatever the command, like `.env` (D-066): a key
  copied into the workspace is still a key. So `cat src/credentials.rs`
  still asks, by choice. Grants (D-066) follow the same
  reading, so nothing that deletes, touches credentials or pushes is ever
  granted.
- **Closed on the way:** where a path points is read the same way, so
  `rm -rf \/home/…` and `cp x \/etc/…` are outside the workspace, not a
  routine delete and an inside write as they were. What `<(…)` and `>(…)`
  run is a piece of its own,
  as `$(…)` was; `--output=<file>` and `--output <file>` are writes,
  outside the workspace asked about like `>`. The word lists had hidden
  both.
- **Left for the task discussion:** `git restore`, `git rebase`,
  `git checkout -- …` and `git stash drop` in command position still ask
  every time, though they act only on the work's own repository.
- **Verified:** a unit test of searches, reads and test runs that now run
  (fails on the code before), and of what still asks: doing it, reaching
  outside, a variable, `find -delete`/`-exec`, `.env`, and the ways around
  a first version a second model found (a substitution, `<~/…`, options'
  values and glued paths, acting options, an environment, a key copied
  in; then quoted options and a key's value; then quotes and backslashes
  inside a word, which also let a delete or a write outside pass before
  this change). The existing policy tests pass unchanged.

## D-139 — A CI failure is the same failure whatever its times (2026-10-20)

D-068 sends the same CI failure after a fix to the developer, comparing
the failed log with the last one character for character. GitHub's
`gh run view --log-failed` starts every line with the job, the step and a
timestamp (and a BOM before the first), so two runs of one failure never
match: a task fixes, pushes and fails until its six attempts are gone,
where it should hand over after one fix. Found reading the code for the
live task bench; the offline test missed it because its fake `gh` printed
the same bytes every time.

- **The log as it's read:** the job and step head their lines once; times,
  the BOM and terminal colours go; the last 40 lines are kept, each cut at
  240 characters. Lines, not characters, so a failure whose durations got
  shorter keeps the same lines. The agent's fix turn gets this text too,
  with less noise. Checks of one run share its log: it's added once.
- **The same failure:** the same lines once what differs between runs of
  one failure is set aside: dates and times anywhere in a line (npm's log
  file names, `time="…"`, Python's `18:00:00,123`), durations (`0.01s`,
  `1.5 s`, Go's `1m02.34s`), where in a file (`lib.rs:42:9`, `line 42`: a
  fix moves lines), goroutines, numbers of five digits or more (run ids,
  processes, ports), hex hashes, `0x` addresses and UUIDs. Counts and small
  values stay, so another assertion is another failure. The cost: an
  assertion that differs only in a date, a time of day or a large number
  counts as the same, as does the same message at another line, and the
  task goes to the developer after one fix rather than trying again. Failed checks are compared in
  name order, whatever gh's. Logs kept before this were raw: the first
  comparison against one differs, so that task gets one more fix before
  the rule applies.
- **No credentials:** what git and gh print on failure, the CI log and a
  check's description pass `redact` before they become a task's messages,
  evidence or otterd's log (invariant 13); they didn't before. A log line
  is cut after redacting, so no part of a secret is left. This needs
  D-134's `redact` (no panic on a multi-byte character before `:` or `=`):
  it lands after it.
- **Verified:** unit tests on gh's real shape (two runs of one failure
  differing in times, durations, run id and commit match; another
  assertion doesn't), on npm, Go, Rust, pytest, unittest and logging
  output, and on a secret at a line's cut. The e2e test now gets a log
  whose times and durations change with every view; it fails against the
  old comparison with "all 6 attempts are used up". Reviewed by a second
  model, which found the npm, logging, Go and line-number cases and the
  cut before redacting.

## D-140 — What Node and Claude Code print isn't the agent's word (2026-10-20)

When the coding agent's Claude worker dies, the run's message quotes the
end of what it printed. The worker prints the SDK's
`CLAUDE_SDK_CAN_USE_TOOL_SHADOWED` warning and Node's `--trace-warnings`
hint on every start, so a run lost to a kill read "worker diagnostics:
(node:…) Warning: canUseTool will not be invoked…" — 400 characters
that say nothing about it, in the task's messages. Found by the live task
bench (B01: the worker SIGKILLed mid-turn).

- The worker's diagnostics leave out Node's own lines, by the rule D-134
  gave Otter's turn (now one function, `agents::node_warning`); with
  nothing else said, they read "no diagnostics".
- **An interrupted turn** ended with Claude Code's own
  `[ede_diagnostic] result_type=user … stop_reason=tool_use` in the SDK
  result's `errors`. With no reply, the worker used it as the turn's
  summary, and the conversation showed it as an Agent message before the
  redirect's answer (B04). The worker now leaves `[ede_diagnostic]`
  entries out of a turn's errors: interrupted with nothing said, the
  summary is empty.
- **Verified:** the fake worker now prints that warning before failing;
  `a_failing_worker_says_why` keeps its diagnostic line and drops the
  warning. The worker's `map.test.ts` maps an interrupt carrying that
  diagnostic to no summary. Each fails against the code before.

## D-141 — A task says when it was last heard from, and what comes next (2026-10-20)

A review of a proposed "Execution Observability Layer" (a second,
derived state machine with its own Loop States, Health ratings and a
`ControlDecision` store) found most of it built: the derived activity
(D-071), the run's activity lines (D-069), Otter's reasons
(`rationale`), the task's history (D-043, D-114) and the watchdog's
pulse (D-063). What a developer couldn't see was *when*: an activity line
had no time, the watchdog's last-heard stayed in otterd, the reasons were
a list with no times outside the history, and nothing said what the
engine would do next. Four additions, all compatible (no protocol bump),
no new state:

- **Each activity line has its time** — `Run.activity_at`, as many as
  `activity`, or none on runs from before; `TaskActivity` carries `at`.
  The conversation's working bubble shows each line's age.
- **When the agent was last heard from** — a live run as clients see it
  has `heard_at` (the pulse's time, now kept on the clock beside the
  `Instant`) and `tools_running`; never stored. The Overview's Now says
  "Last heard from the coding agent 2m ago", or "running a tool" while
  one runs: never silence then, and no verdict (Healthy, Stalled) — the
  watchdog already acts on it at `OTTER_RUN_IDLE_MS`. The app reads a host
  with a live run again every 30 s, and takes the agent's streamed text as
  hearing from it, so its copy doesn't go stale between task changes and
  show a silence otterd doesn't believe.
- **What comes next** — `Task::what_next`, in otter-core so the rule and
  the words are one (as D-134 did for waiting): derived from the task's
  own record and the transitions in `task-states.md`, saying both ways
  where the engine hasn't seen the outcome yet ("review if every
  criterion is met; a fix step if one isn't and the work changed").
  Nothing when the task waits on the developer — that is `needs`. Served
  as `Task.next`; the app computes it itself for an otterd before this. In
  Now and in `otter task show`.
- **Reasons in the history** — `Task::reason` keeps the line in
  `rationale` (delivery's report still reads it) and returns
  `TaskEvent::Reasoned {why}`, written with the change it explains, so a
  reason has its time and correlation in the timeline. Redacted (D-075).
  The Activity feed shows them inline, muted, a reason said again right
  after itself once with ×N (D-114's repeats); the "Otter's reasons" fold
  stays only for tasks from before.
- **Not built:** classifying the agent's tools into reading / editing /
  testing (that is reading the provider's data: it would belong behind
  `AgentProvider`, and nothing showed it was needed); a structured
  observation / decision / next per reason (the record's time, its
  correlation and the status change beside it carry most of it; 22 call
  sites didn't justify three fields); health ratings and automatic
  restarts on them.
- **Known limit:** a task from before this that gets a reason after it
  shows only the new ones in Activity: its older reasons aren't in its
  history, and the fold gives way once one is. They stay in `rationale`.
- **Verified:** core unit tests for `what_next` by status, `reason` (kept,
  redacted, first line, read as `Unknown` by an older build) and a run
  from before; e2e: a live run has `heard_at` and the task its `next`, an
  ended run keeps `activity_at` but no `heard_at`, the history has
  `Reasoned`; app tests for Now, the timed activity lines, the reasons in
  the feed, and the source keeping times and re-reading a host with a live
  run.

## D-142 — Claude Code signs in with a token from Settings (2026-10-20)

On a host where Claude Code isn't signed in — a server reached over SSH,
where its browser login is awkward — the coding agent fails its first run
with `auth_required`. The fix was to sign in there, or put
`CLAUDE_CODE_OAUTH_TOKEN` in otterd's login environment and restart it.
Claude Code's own answer for headless use is a long-lived token from
`claude setup-token` (`sk-ant-oat01-…`), made wherever the developer is
signed in. It can now be set in Settings › Coding agent › Claude Code ›
Sign-in.

- **A secret like the providers' keys** (D-048): `CLAUDE_CODE_OAUTH_TOKEN`
  in `state/secrets.json` (0600), write-only over the protocol, never
  logged, in an event or on a command line. Only what `claude setup-token`
  prints is taken (`sk-ant-oat…`): an API key isn't a sign-in. It is no
  controller's key, so it doesn't make Anthropic's API "automatic"'s
  choice.
- **Only Claude Code's processes get it**, in the environment they start
  with: a task's run (`sdk` and `legacy_cli`), `runtime.test`, an
  interactive Claude Code session (through `internal-exec`'s env file),
  and the `claude` brain — its `claude -p` calls and Otter's turn. A shell,
  a service or the check in the same workspace never does: an
  `AgentProvider` names the secrets its processes get (`secrets()`), and
  generic code passes those, unread. Set here, it wins over the same
  variable in otterd's environment, as a key does.
- **Switching a session over:** a Claude Code session started on the
  host's own sign-in, restarted once a token is set, resumes the same
  conversation (`--resume`, as any restart) signed in with the token; a
  task's next run, Otter's next turn and the next test take it up too. A
  token cleared works the same way back.
- **Not a login flow.** D-057 left sign-in to Claude Code because Anthropic
  asks third-party products not to offer claude.ai login. Otter still
  offers none: the developer makes the token with Claude Code, for their
  own hosts, and Otter keeps it as it would the environment variable.
- **Known limits:** Claude Code prefers `ANTHROPIC_API_KEY` (and
  `ANTHROPIC_AUTH_TOKEN`) to this token, so where otterd's environment has
  one, that is what signs in; the token isn't checked when set (Run a test
  does that); work already running keeps how it started.
- **Verified:** unit tests for the setting (refused unless a token, kept
  0600, never in the view, no controller) and for the environment it
  gives; an e2e test with the fake worker, which logs only whether it got
  one: unset, then set for a test and for a task's run, unset again once
  cleared, and the token in no file of otterd's home but `secrets.json`. It
  fails with the run's injection taken out. Another: an interactive
  session restarted after the token is set resumes its conversation with
  it, and a command session beside it doesn't get it. The app's test: the Sign-in
  row with the coding agent, sent once, never shown, not among the API
  keys.

## D-143 — Apple silicon only (2026-10-20)

Each release ran four macOS jobs (the app and `otterd`/`otter`, per
architecture) next to CI's macOS job, and runners queued for them: a
release that builds in about 5 minutes took 9 to 15. Nobody runs Otter on
an Intel Mac, so the x86_64 app, its update and the `macos-x86_64`
binaries are gone, and a release runs two macOS jobs. `latest.json` names
only `darwin-aarch64`. The installer refuses an Intel Mac and, on Apple
silicon under Rosetta (where `uname -m` says x86_64), installs the arm64
build. Supersedes the x86_64 half of D-026.

## D-144 — Claude Code starts on its classic renderer in every session (2026-10-20)

Scrolling up a long Claude Code conversation in the app was slow, in a
session on a host reached over SSH: neither `pixelScroll` (D-130) nor the
WebGL renderer was why, and `seq 1 20000` in a shell on the same host
scrolled smoothly. Claude Code's fullscreen renderer (its default for
anyone who started using it from May 2026) draws on the alternate screen
and takes the mouse, so each wheel step went to the host, Claude Code
redrew its screen, and the redraw came back — about 500 KB/s while
scrolling. Its classic renderer writes the conversation to the
scrollback, which the app keeps and scrolls itself (D-118).

- **Every session gets `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`**, a
  shell too: the developer runs `claude` in a shell as often as in an
  agent session. Claude Code documents it as forcing the classic renderer
  whatever its saved `tui` setting, so `/tui fullscreen` on the host
  doesn't bring the slow scrolling back. Developers don't set anything.
- **The agent provider names it** (`AgentProvider::session_env`, in
  `claude.rs`); generic code passes it unread. Unlike its secrets (D-142),
  which only its own processes get, these are not secret and go to every
  session.
- **The host's own value wins:** the same variable in otterd's login
  environment or a workspace's direnv is left as it is — `0` there gives
  fullscreen back.
- **Known limits:** fullscreen's click-to-expand, in-app selection and
  flat memory go; under tmux the classic renderer may flicker more while
  output streams. Sessions already running keep how they started until
  restarted, and the host's otterd needs this version. Claude Code's
  `claude attach` / agent view sessions are always fullscreen and aren't
  affected.
- **Verified:** an e2e test — a shell session has the variable, and a
  host that sets it to `0` keeps `0`; it fails with the injection taken
  out. Manually: with `/tui default` (the same renderer) the slow
  scrolling went away.
- **Not verified:** that the variable beats a saved `tui: fullscreen`
  (from Claude Code's docs only); and that reading an interactive
  session's prompts from its screen moving (D-136), observed before this
  on whichever renderer ran, holds on the classic one — the agent tests
  use a fake `claude`.

## D-145 — A pasted image reaches Claude Code on a Mac host (2026-10-20)

Dogfooding: the app on a laptop, a Mac mini as the host over SSH. Ctrl+V
with a screenshot pasted the Mac mini's clipboard text instead. The app had
sent the image and otterd had stored it (D-031), but Claude Code on macOS
reads the host's own pasteboard with `osascript` and never runs `wl-paste`,
so the stand-in never served it.

- **On a Mac host, otterd also puts the image on the host's pasteboard**
  (`osascript … «class PNGf»`, the stored file's path as an argument, 5 s
  at most) before the app sends the key. It replaces whatever that
  clipboard held: a host is used from elsewhere, and the paste is what was
  asked for. A failure is logged; the paste itself still succeeds.
- The stand-in stays: Linux hosts need it, and it's harmless on a Mac.
- **Verified:** the paste e2e test runs otterd with `OTTER_OSASCRIPT`, a
  stand-in that keeps what it's given (the real one would take the
  clipboard of whoever runs the tests): on macOS it gets the pasted PNG,
  and the test fails with the call taken out; elsewhere it isn't called.
  By hand on the Mac mini: the same `osascript` line put a stored paste on
  its pasteboard, and Claude Code's Ctrl+V then pasted the image.

## D-146 — A workspace shows the tasks working in it (2026-10-20)

A task's "Open workspace" led to a workspace with no sign of its coding
agent: the agent is a managed run (D-044, D-057), not a session, so it has
no tab, and D-050 took away driving it. The workspace now lists the tasks
still going in it (its own task, or one placed there or with a step there;
not done or cancelled), one row each with the task's mark, title and what
it is doing, above the session tabs; a click opens the task, where the
agent's work is followed. The rows are read-only; the "Task ↗" chip of a
workspace made for a task (D-105) stays, also after the task ends.

## D-147 — A task shows what it changed (2026-10-20)

A task's diff was only in its workspace's Changes panel (D-102), so
checking the work meant leaving the task. The task's Overview now has a
Changes section: the same list as the panel (branch and base, ahead and
behind, each file's change and counts) and a file's diff below it on a
click, read from the task's workspace through the task source
(`changes`, `diff`). It is looked at again when the task changes, every
30 s while it works and the app has focus, and on Refresh. It sits after
Progress while working, first under Now in review, after the report once
ended. Left out without a workspace, outside Git, or with an otterd that
can't say. The list and diff are one component (`ChangesList`) shared with
the panel.

## D-148 — A task waiting on your agent says so; a terminal mid-layout doesn't size the session (2026-10-20)

Two things from a day of use.

- **A task waiting for an agent the developer started** (D-079) looked
  stuck: the header said *Working*, the banner was the blue of a wait that
  ends by itself, and Otter, asked, said nothing waited on the developer —
  `status_text` said "I'm planning" whatever the reason. Now the status
  Otter's turns are given says it hasn't started, why, and whether it waits
  on the developer (D-134's rule) or starts by itself; `get_task` carries
  `waits_on_you`. In the app, such a task's header says *Needs you*, the
  banner is the needs one, and it offers *Open <session>* — the session it
  waits for, found by name among the workspace's — beside *Work in its own
  workspace*.
- **A session's history laid out a dozen columns wide.** A terminal shown
  as the view switched could be measured mid-layout, a few columns wide,
  and the size went to the session: tmux shrank the window and Claude Code
  printed its whole history again at that width, which stayed in the
  history when the terminal grew back. The terminal now never fits to, or
  tells the host, less than 20×4, and gives the session its size only once
  it has held for 150 ms. History already laid out narrow stays so.

## D-149 — A terminal attaches at its own size (2026-10-20)

After an update the app starts again and every terminal shown attaches at
once, while the window is still being laid out; a session's history then
came back laid out narrow, now and then. D-148 kept sizes under 20×4 out,
but the attach still went out at once, with whatever size the terminal had
then (xterm's 80×24 if it couldn't be measured, or one measured with the
fallback font before the bundled one loaded), and each size the session
gets, Claude Code lays its whole history out again at.

A terminal now attaches only once it is on screen at a real size (D-148's
20×4), measured with its own font (loaded, or given up on): the first size
the session gets is the one it is shown at. A kept pane mounted hidden
(D-100) attaches when it is first shown.

## D-150 — Sessions are put in order by dragging their tabs (2026-10-21)

A workspace's tabs came in the order the sessions were made, and so did
⌥1…⌥9 (D-122), ⌘⇧[ / ⌘⇧] and the session a split shows beside.

- **Drag a tab, or Move left / Move right** in the session's ⋯ menu. As the
  sidebar's pins (D-040): pointer events, not HTML5 drag-and-drop (the
  file drop has it), the others measured when the drag starts, and the
  tabs shown in their would-be order meanwhile. Tabs wrap onto more rows,
  so the place is by reading order: a row above the pointer, or its row
  and left of it; above or below the tabs counts as the first or last row.
- **The host keeps the order**, not the app as it does pins: the order is
  one workspace's, on one host, and the same in every client — another
  Mac, `otter ls`, ⌥ numbers. It is the order of the workspace's sessions
  in `state.json`; `session.move {workspace, session, to}` changes it and
  emits `SessionMoved`. A new method and event, so no protocol bump: an
  older otterd answers `invalid_request`, and the app says to update it.
- **Shown at once.** A dropped order shows until the host's order changes
  (to it, normally, with the snapshot `SessionMoved` brings), and goes
  back if the move fails.
- **Not activity.** A move leaves the workspace's `updated_at`, so it
  doesn't come up as recently active in the sidebar; rename does change
  it. The timeline shows no line for it.
- Not done: a command in the CLI (`otter session mv` is rename).

## D-151 — A retry shows at once; a look at the changes reads a bounded amount (2026-10-21)

A review of the workspace view found three things that were wrong in use.

- **A retried preparation looked like nothing.** `workspace.prepare` set the
  workspace to `preparing`, and the steps after it (`fetching`, `creating
  worktree`, …) were saved, without an event; the app takes a new snapshot
  only when one arrives, so the pane kept showing the old failure, and its
  Retry, until preparation ended. New event **`WorkspacePreparing
  {workspace_id}`**, on a retry and on each step; what it is doing is in the
  workspace, as with `WorkspaceBriefChanged`. A compatible addition (older
  clients take it as `Unknown`). The timeline shows no row for it: the steps
  are not history.
- **`workspace.changes` read every new file.** An un-ignored `node_modules`
  meant gigabytes read, on the async runtime, on each state change and every
  30 s with the panel open. Counting lines is now off the runtime and bounded
  like `work_state`'s hashing: 2000 files, 64 MiB in all; past that a new
  file is listed without counts, as a binary one is.
- **The delete dialog's "Discard" box stayed ticked** after Cancel, on a pane
  kept mounted (D-100); it is cleared when the dialog opens.

Not done: the rest of the review's list (push and pull request for a
workspace driven by hand, ahead/behind on a new branch, the partial failure
of archive and delete, splitting `WorkspacePane.tsx`).

## D-152 — A workspace keeps the sessions shown last attached (2026-10-21)

Every switch between a workspace's sessions unmounted the terminal and
attached again: `session.attach` with `history: 5000`, a snapshot of up to
1 MiB (D-118), and the scrollback the reader had made by asking for earlier
history (D-119) was gone. Over SSH that is a wait on every tab. D-100 did
this between workspaces; this is the same between sessions.

- **A pane keeps the terminals of the sessions shown last**, hidden, with
  their attach. One shown again is just shown. The kept ones are per
  workspace (so at most 5 workspaces × the setting), in the order they were
  first kept: the terminals' elements aren't moved, and the left or right
  place is CSS `order`, so a swap of the two sides doesn't re-parent a
  terminal either.
  *Amended 2026-10-21 (D-153):* "just shown" holds only for a session that
  printed nothing while hidden. One that did gets a full snapshot when
  shown (D-119), as an attach would, and loses earlier lines it had loaded.
  *Amended again (D-160):* now it gets what it missed, up to 1 MiB, at the
  same size; a snapshot past that or at another size.
- **Hidden means idle,** as in D-100: a hidden terminal never fits, tells
  the host it isn't on screen and gets nothing until it is (D-119, D-137),
  and takes no keys.
- **Two local settings,** on this Mac, in Settings › Developer, applied at
  once (`keepSessions.ts`, `otter.kept-sessions`, `otter.kept-idle-minutes`):
  - *Sessions kept attached*, 5 by default, 1–20, the ones shown included.
    1 is the behaviour before this. The one hidden longest goes first.
  - *Let go of an idle session after*, 10 minutes by default, 0 for no limit
    (then only the capacity lets one go), up to a day. Looked at every 15 s;
    a session shown again and hidden again starts over. Only sessions not
    shown in their pane count: a hidden workspace's pane is D-100's.
- A session that is deleted is let go; one that ended keeps its last screen
  (dimmed), as when shown.
- Not done: *scrollback loaded with "earlier"* still costs up to 4 MiB on
  its own and is lost when a session is let go. Not measured on a slow link
  either, only the attaches counted (`App.test.tsx`).

## D-153 — A hidden terminal gives up its WebGL context (2026-10-21)

A review of D-152 after it merged found what it had missed, and D-119 had
written down: WebKit has about sixteen WebGL contexts, and each terminal
took one when it was made, hidden or not. D-100 already kept up to ten
terminals; D-152 allowed 25 by default. Past the limit WebKit loses the
oldest context, and that terminal, often the one in front, drew with the
DOM renderer from then on.

- **WebGL while on screen only.** A terminal loads the WebGL renderer when
  it is on screen at a size, and disposes it when it is hidden (its pane, its
  view or its session not shown), the same `ResizeObserver` that already
  stops it fitting. Hidden, xterm draws nothing anyway. xterm's dispose
  only removes the canvas, and WebKit counts its context until it is
  collected, so the terminal gives it up itself (`WEBGL_lose_context`), on
  the canvas the addon added. A context lost while shown is replaced at
  once; one that couldn't be made is tried again when next shown (only no
  WebGL at all stops trying). So the app holds a context per terminal on
  screen: one or two.
- **What D-152 saves, said as it is:** a session that printed nothing while
  hidden is shown at once, with its history. One that printed anything is
  caught up by a snapshot (about 150 KiB over compressed SSH for 5,000
  lines), as an attach would be, without mounting again. The Developer hint
  says so too.
- Also: an older host without control frames (D-119) is never told a
  terminal is hidden, so a kept one there keeps getting its output.
- Not done:
  - Catching up a session that printed a little while hidden without a
    snapshot (the daemon keeping that output, to a budget, instead of
    dropping it). That is D-119's off-screen rule and the next step.
    *Done in D-160.*
  - Each attach is a session on one SSH connection per host, and sshd allows
    ten by default (`MaxSessions`). Not measured past that.
  - Checked in tests only: the renderer switch isn't exercised there (the
    terminal is mocked), and not yet in WebKit.

## D-154 — A workspace can be pushed by hand; it says what no remote has (2026-10-21)

A review of the workspace view (D-151's second half). A task publishes its
work through the controller (D-047), after the developer allows it; a
workspace driven by hand had only a terminal for it, and its ↑ said how far
ahead of the *base* it was — still the same after a push, on a new branch.

- **`workspace.publish {workspace, pull_request?, title?, body?}`**: pushes
  the branch to `origin` and, asked to, opens its pull request, with the
  controller's own calls (`delivery::Git::push`, `GitHub::publish`, Otter's
  `gh` sign-in first, D-133). The dialog in the app is the allowance. Not
  taken from the controller: it **commits nothing** (D-102 left committing
  out, and a hand-driven workspace has no task title for the message): what
  isn't committed is left out, and said so. And a pull request that exists is
  **left as it is**, not edited (`publish` rewrites the description of the
  one it finds; a developer's own words are not the report of a task), and
  creates without looking first, so one opened meanwhile keeps its words too.
  What was left out counts every change, secrets' names included. A
  workspace on an existing branch has that branch as its base, so its pull
  request goes to the repository's default, as does one whose base isn't a
  branch of the remote (a tag, a commit). Otter's tools still cannot
  publish (gateway test).
- **`WorkspaceChanges.unpushed`**: `git rev-list --count HEAD --not
  --remotes`, commits no remote has — what the delete dialog calls "not
  pushed anywhere". A push brings it to 0, a new branch's included; with no
  remote at all it is absent, not every commit. The app
  adds "N not pushed" to the Changes header and the pane's line.
- **Preparing a workspace again fetches** its repository (best effort; offline
  doesn't fail it, and it gives up after 60 s: a network that never answers
  must not hold a workspace in Preparing), so "behind" is as of the retry, an unarchive or
  "Prepare again" (now also in a Ready workspace's menu: the environment is
  read again and sessions not started start). `workspace.changes` itself
  never fetches: it is read every 30 s.
- **Archive and delete** try every session and save and announce the ones
  stopped when another won't; the archive stays as it was, the delete stops
  there. Archive also refuses a workspace being deleted, as the others do.
- **App:** a Files panel keeps only the transfers it started (the others'
  were never cleared from a kept pane); the workspace's dialogs are
  components of their own, each with its own state; a branch not yet known is
  shown as "git", not blank.

Not done: `otter push` in the CLI; committing from the app (D-102); a pull
request's description written from the work.

## D-155 — The update offer is in the sidebar, not in each workspace (2026-10-21)

A workspace's pane showed "<host> runs otterd X; this app is Y" with an
**Update otterd** button above its tabs whenever the host's otterd differed
from the app's. The same offer is already at the bottom of the sidebar (and
on the host's page), once per host, so it repeated on every workspace of that
host and took a row from the terminal. The workspace pane no longer shows it.
A host too old for a command still says so where the command is used
(`explain`).

## D-156 — A task holds its workspace until it's done (2026-10-21)

D-079 gave a workspace one managed run at a time, so delivery wouldn't push
one branch with two tasks' work. But the claim lasted only as long as a
run. A task in review keeps its verified work uncommitted until the
developer allows publishing (D-068), with no run at all. The live task
bench (B03) started a second task in that workspace, then allowed the
first to publish: `git add -A` committed the second's half-made edits under
the first's title and into its pull request. The same gap let a waiting
task slip in between two of a task's runs (its planning turn and its
first step).

- **In a Git repository, another task's work there, not done, holds the
  workspace:** a task that has run in it and isn't done or cancelled — at work, in review,
  paused, blocked or failed. A task wanting it waits as D-079 says, costs
  no attempt, and goes on by itself when the other is accepted or
  cancelled, or moves to a workspace of its own (D-080, the app's button),
  since it hasn't worked yet. While the other is at work the wait reads as
  D-079's ("… is working there") and ends by itself; while it's at rest
  only the developer can end it, so it says so ("… has unfinished work
  there: finish or cancel it, or give this task a workspace of its own")
  and needs them ("Waiting on you").
- **Pausing doesn't hand a Git workspace over:** paused, a task's work is
  still in the tree. Cancelling does. A cancelled task's files stay
  (D-068), and the next task's delivery then takes them as the
  workspace's.
- **Only where delivery commits:** a workspace in a Git repository (at
  its root or below it, as delivery finds it). An empty or plain directory
  has no commit to mix work into, so it keeps D-079's one run at a time.
- **Triggers on a Git workspace** now wait behind the last occurrence's
  task until it's accepted (and later occurrences are skipped as still at
  work), where before each pushed into the same branch and pull request.
  The wait says it needs the developer.
- **No deadlock:** the hold applies only to a task that hasn't run in the
  workspace yet. Two that both have (from before this, or another way
  in) fall back to D-079's one run at a time.
- **Not done** (from the review, 2026-10-21):
  - The hold is about when tasks start, not what is committed: publishing
    still commits whatever is in the tree. A task accepted or cancelled
    without publishing leaves its edits there, and the next task's
    delivery takes them under its own title. A check before committing that
    the tree is the one verified would catch more, but not this case (the
    next task verified with those edits in the tree).
  - A trigger on a Git workspace makes one task and then waits behind it
    until it's accepted; the trigger's wait limit doesn't apply to that
    wait, so an unattended trigger needs the developer each time.
  - Done isn't final (the goal can change after it, D-050): a task
    re-planned after another started in the workspace has runs, so both
    fall back to D-079.
  - A task that has run counts wherever its runs were; a task moved to
    this workspace after running elsewhere isn't held.
- **Not done:** a task accepted but whose pull request isn't merged yet
  leaves its branch with that PR open; a next task in the same workspace
  pushes to that branch and finds that PR. Waiting for the merge would
  need GitHub's word on every step; it's the developer's call when to
  start the next.
- **Verified:** e2e `a_task_waiting_to_publish_keeps_its_workspace` (B03's
  shape: the first asked to publish, the second waits through the publish
  and starts once the first is accepted) fails against the code before;
  D-079's and D-083's tests on plain directories pass unchanged.

## D-157 — A command to decide on is there whole (2026-10-21)

B06 in the task bench: the coding agent ran a long `python3 - <<'E' … E`
script that ended in an `rm`, and policy asked the developer, as it does
for deleting. The decision's `summary` is one line, cut at 200 characters
(`claude_stream::summarize`), and its `detail` only policy's reason. The
`rm` policy matched came after the cut, and the command was nowhere else:
not in the app, not in `otter task show`, not in the conversation's tool
record (cut the same way). The only safe answer was to deny.

- **`DecisionRequest.command`**: the whole command line, when the summary
  doesn't show it as it is (cut, or on several lines). Redacted, then
  bounded at 16 KiB on a character, with a last line saying how many bytes
  more there were (`… N more bytes, not kept`). Redacted first, so a cut
  can't leave half a credential that no longer looks like one. It is set
  where a run's every ask is recorded (`record_ask`), so it holds on both
  backends: `claude -p` and the SDK worker both send the tool's input
  whole. The summary stays the short line it was.
- **A field of its own, not `detail`.** `detail` is policy's reason and is
  read back as that: deciding rebuilds policy's verdict from it, the
  Control Agent's prompt quotes it as the policy note, and the app shows it
  as prose. A command is shown as written, its lines kept. The field is
  optional and absent when the summary says it all, so older clients
  ignore it and an older otterd never sends it: no protocol bump.
- **Where the developer decides:** the app's decision (in the task and in
  the Inbox, one component) shows it under the reason, as written, and
  scrolls past 240 px. `otter task show` prints, under each decision
  waiting on you, its reason (it printed only the summary) and the whole
  command, indented.
- Nothing new goes to `events.jsonl` or the task's history:
  `DecisionRequested` carries no text, and the Blocked reason is still the
  summary. The e2e test reads both and checks.
- **Two leaks next to it, closed** (found in review, older than this):
  - *The summary was cut, then redacted.* A cut inside a URL's password
    (before its `@`), or one that left a token shorter than its shape, was
    no longer seen as a credential, and its start went into the summary,
    the Blocked reason in the task's history, the conversation's permission
    interaction and Otter's prompts. `summarize` now redacts before it
    cuts, as the conversation's tool record already did, and so does a
    run's activity line (`$ …`).
  - *A grant key kept the command as written.* The key (D-066) is the
    command line, kept on the decision and, once approved, in the task's
    grants: `HTTPS_PROXY=http://bob:hunter2@proxy:8080 cargo build >
    /opt/build.log` matched no word list, so it was grantable with the
    password in its key. A line with a credential in it
    (`redact::has_secret`) is now never granted: it is asked every time, as
    D-066 already asks about credentials. Keys keep their format.

Not done: the conversation's tool record and its permission interaction
still carry the 200-character line, so the coding agent's feed in the app
cuts the command as before. Otter's tools (`get_pending_decisions`) and
the Control Agent's decision prompt still see only the summary and the
reason. The Control Agent never decides what only the developer may
(D-066), and policy reads the whole line either way. A tool's output from
`claude -p` is redacted before otterd cuts it to 2,000 characters too;
the SDK worker still cuts it first (it has no `redact`), so that cut
should move to otterd.

## D-158 — A plan turned down on an answer-only task still gets the question answered (2026-10-21)

Bench (B05): an answer-only task asked what `make test` runs and how many
tests there are. The coding agent's look at the code came back with a plan
(confirm the count, write it down), which came to the developer as D-092
says; they turned it down. The task went done, "Answered", and the answer
posted was the plan's own steps and criteria with "Not carried out: this
task only answers." The question was never answered, although the agent
had read the Makefile and the tests to plan.

- **Turned down, the agent answers in words.** The planning step goes back
  to the coding agent once more, marked in its `detail` (as a plan sent back
  is, D-084): its next turn, in the same conversation, is told the developer
  turned down changing the code and to answer the request from what it
  found, pointing to the code, changing nothing, with no JSON. It only reads,
  as every turn of an answer-only task does (D-092), and costs no attempt.
  Its reply is the answer (`planning → done`, "Answered"), as a look at the
  code that found a question is.
- **Not asked twice.** If that turn plans the change again, the developer
  isn't asked again. What it said besides the plan (the JSON and its fence
  left out) is shown, then what it would do, not carried out; with nothing
  besides, Otter says plainly that the agent couldn't answer without
  changing the code, which they turned down. The task is done, "Not
  carried out: the coding agent planned the change you turned down". The
  mark is the guard, kept in the task document, so a restart doesn't ask
  again either. A turn that fails or says nothing fails the look as
  before, and Retry asks for the answer again.
- **A decline that no longer stands is forgotten** (`forget_decline`),
  whatever the planning step is doing — even mid-turn:
  - a new ask (D-050, "then change it", `replan`): a turn under way is
    cut off and carried on with it, and a plan from it is asked about as
    any is;
  - changes allowed (`set_answer_only` off): what comes back is read as
    any look's — a plan goes to the brain's review — and a look still to
    start is an ordinary one.
  The mark is never taken for a plan sent back, so the review's points
  still go back to the agent once before the developer is asked (D-084).
- The question itself now says so: turned down, the agent "answers from
  what it found, changing nothing".
- Known limit: a **redirect** during that turn (D-059) doesn't change the
  goal, so the mark stays, and a plan in the reply ends the task as above
  rather than being asked about — unlike a plan sent back, which still
  reaches the developer's plan question. Saying so again, as a new ask,
  plans again. Tested with the fake worker only (an answer in words; a
  plan again, bare and with words; a new goal and changes allowed while
  that turn runs); not yet with live Claude.

## D-159 — Reconciling asks the backend without holding the store (2026-10-21)

Twice a second, reconciliation took the store's lock and, holding it, asked
the backend what runs (`tmux list-panes`), captured each agent screen it
watches (D-035), one `tmux capture-pane` each, and the last lines of what had
just ended (D-076). Every request that needs the store (a listing, an
attach, a task's step) waited for all of it, and a tmux command costs about
5 ms from otterd, whatever it does.

- **Look, ask, apply.** What runs is read under the lock: the running
  executions, by id, and which agents among them are watched. The lock is
  let go, the backend asked, and the lock taken again to apply what it said,
  with no backend call while it's held. Passes still run one at a time,
  now also by a lock of their own.
- **Only to what is still as it was.** An execution is decided on only if it
  was asked about and is still the current one, running, with the same
  backend ref. One started meanwhile (a restart, a new session) isn't in the
  backend's answer, and would have been taken as lost. One stopped or
  deleted meanwhile isn't running any more, and its screen, seen before it
  went, isn't kept. Backend refs are execution ids, never reused, so an old
  answer can't land on a new process. `daemon.rs` said the lock kept the
  reconciler from seeing an execution in the store and not yet the backend;
  that is now this rule.
- **D-137 holds.** Who is on screen is still read after the lock is taken
  again, in the hold that raises attention, so a terminal that comes on
  screen after it still settles what the pass raised.
- **`settle`'s timing is unchanged.** Each watched screen is still looked at
  once a pass, stamped with the pass's time, taken after the backend said
  what runs. One started while the backend was asked is looked at first by
  the next pass, 500 ms later, against an 8 s quiet threshold.
- **One tmux for all the captures of a pass** (`capture_each`, in
  `backend/`; another backend captures one by one). Each capture is followed
  in the same command by a line made of a number new to that call, which is
  where its output ends; `display-message -p` prints it. tmux stops at a
  command that fails, so the batch is taken only if it exits cleanly with
  every end line and nothing after; otherwise each is captured on its own,
  and one gone costs the others nothing. Only screens the backend just
  listed are asked for, so that is a pane gone in between, not routine. At
  most 32 a command, well within what tmux takes.
- **Measured** with the end-to-end tests' fake Codex held at an approval
  prompt (watched as `blocked`) plus a shell, 40 passes a case, before and
  after back to back, on the 16-core dev VM with a load average of 4 to 12
  from other work. Lock held per pass, average (median):

  | agents watched | before | after | tmux commands a pass |
  |---|---|---|---|
  | 0 | 4.8–5.5 ms (4.8–5.1) | 0.06 ms (0.06) | 1 → 1 |
  | 1 | 9.0–10.7 ms (9.0–9.3) | 0.28 ms (0.27) | 2 → 2 |
  | 5 | 30–54 ms (26–55) | 0.60–0.67 ms (0.60–0.64) | 6 → 2 |

  What's left under the lock is observing agents (reading their
  transcripts) and saving. A client listing workspaces as fast as it could
  waited 1.9–2.2 ms on average with five agents, 1.2–1.4 ms after; its worst
  wait went from 32–65 ms to 2–7 ms. A whole pass, now mostly without the
  lock, takes about 10 ms with five agents (30–54 ms before) and 5 ms with
  none, as before.
- **CPU:** otterd with its tmux clients, in the test's environment (whose
  controller ticks every 200 ms): 2.7% with no agents, before and after;
  with five, 8.5–10.1% before, 3.7–4.3% after. With no agents nothing
  changes: the one `list-panes` a pass stays (about 3 ms of CPU, timed
  from a shell).

Not done: telling otterd when a pane changes instead of asking twice a
second (D-007's revisit: tmux hooks or one control-mode client), which is
what an idle host would need; and reading agents' transcripts outside the
lock.

## D-160 — A terminal back on screen gets what it missed, not a snapshot (2026-10-21)

D-119 dropped a stream's output while its terminal was off screen and sent
a snapshot when it came back. With D-152 keeping a workspace's sessions'
terminals, that is most switches: a session that printed anything while
hidden came back as a snapshot (5,000 lines, up to 1 MiB, about 150 KiB
over compressed SSH), and the snapshot's reset cleared the terminal's
history, earlier lines asked for (D-119) and where the reader was.

- **Kept, then sent.** Off screen, the daemon keeps the pane's output for
  the terminal; back on screen at the same size it sends it, as it would
  have gone. Nothing is reset; its history stays as it was.
- **At another size, a snapshot.** A hidden terminal doesn't fit, but it
  fits itself to its place as soon as it's shown, before it says it's back
  (the window, a panel or the font changed meanwhile). Output kept at the
  old width would wrap wrong in it and stay in its history, so then it gets
  a snapshot at the new size, as before.
- **Up to 1 MiB** (`HELD_BYTES`, a snapshot's own cap): past that a snapshot
  is the cheaper way back, and the terminal gets one, as before. Measured
  with Claude Code launched as Otter launches it (D-144), at 200×50: about
  1.1 KiB a second while it works, nothing while it waits. So about a
  quarter of an hour of work is kept, longer than D-152 keeps a hidden
  session by default (ten minutes). About 1 MiB a hidden terminal in
  otterd (its buffer may briefly be twice that while it grows).
- **Given up, for a snapshot,** when the window's size changes while the
  terminal is hidden (another client sized it: what was kept was laid out
  at the size before, and the client would hear of the new size first),
  when it fell behind, and when a snapshot is asked for meanwhile (it has
  it all, so nothing is sent twice).
- Same path as live output: the user's own terminal's queries are kept out
  of it (D-120), and the client falling behind still gets a fresh snapshot.
- **Verified:** unit tests for kept and sent, too much, back at another
  size, the window sized by another client, and a snapshot asked for or
  come meanwhile; the end-to-end test against real tmux now expects the
  missed output without a reset, and a snapshot after 3 MB printed while
  away. The kept-output tests fail with output dropped as before.
- Not done: an older host still sends a snapshot; the app doesn't need to
  know which it got.

## D-161 — A host's Work tab shows what Otter runs there, strays included (2026-10-21)

During an otterd update, otterds of other builds (one from a bench's home, one
a debug build) were found running for more than ten hours, using CPU, with no
client and no sessions; nothing in the app showed them. A host's page now has
two tabs: **Overview** (what it was: problems, usage, ports) and **Work**.

- **Work** lists by kind: Tasks, Workspaces, Sessions, Agents (a task's coding
  agent, an agent in a session) and Processes (this otterd, other ones,
  orphans). The first three come from what the app already has; their
  processes, owners, CPU and memory come from the new **`host.processes`**. A
  row opens into who owns it and its process tree. The actions are the
  commands that exist (`task.act`, `workspace.*`, `session.*`), asked again
  in the row before they end anything. The tab is remembered per host. A
  badge on the tab counts the strays, so one is seen from the Overview.
- **Strays** are decided conservatively and on the daemon: an `otterd serving`
  that isn't this process, or a process whose working directory is in this
  host's `workspaces/` folder under an id that no workspace has (or whose
  workspace is archived), with no session's or run's process above it. What
  holds owned work up (the parents of a session's or a run's process) and the
  backend's server (a process named `tmux`, daemonized, which keeps the
  directory the otterd was started in) are never strays; a worker this otterd
  started that outlived its run is one. A shell of the developer's own that
  merely sits in such a folder is called a stray too, by the directory it is
  in now; stopping it asks first and says what it is. A
  session of an archived workspace that still runs is not a stray (it is a
  session): the row says so ("Archived, still running").
- **`host.processes.stop {pid, started, force?}`** is the one new thing that
  can end a process, so it is narrow: it takes the pid *and start time*
  `host.processes` gave, finds the process again now and classifies it again,
  and refuses this otterd, a session's or a run's process (stop those where
  they are shown, through `session.stop` and `task.act`), a start time that
  isn't the process's (a reused id), and anything not listed. SIGTERM, or
  SIGKILL when forced, to its process group if it leads one; a signal that
  can't be sent (the process ended, or isn't this user's) is an error, not
  a success, and with no sample yet (the first 5 s after a start) it is
  refused, for the owners can't be found without the tree. The target is **read again at that moment** (executable,
  arguments, directory), not taken from the 5 s sample: a session starts as
  `otterd internal-exec … -- argv` and execs in place, so a sample taken then
  would call its shell "another otterd". The sampler reads those fields anew
  each time, and an `internal-exec` is never an otterd serving. The sample's
  command lines never leave the daemon (invariant 13); an otterd is told from
  other processes by its name and `serve` argument, read in the sampler.
- **Another otterd can be stopped** from here with SIGTERM or SIGKILL. That
  loses no work: `daemon.shutdown` is already "managed processes keep
  running", and the page says its sessions keep running. The version of
  another otterd isn't shown: it would need running its binary; its path and
  start are.
- **Busy** (25% of a CPU or more for ten minutes) is shown on any row, never
  made a stray by itself: a build can be busy for that long. Output isn't
  known to the sampler, so "no output" is not claimed.

- **Threads are not processes.** On Linux the system lists every thread as a
  process of its own, with its process's executable and arguments, so an
  otterd's threads each showed as "another otterd" (17 on one host, with a
  Stop each) — and a signal to a thread's id reaches its whole process, which
  can be this daemon. The sampler skips threads, and a stop refuses a pid
  whose thread group isn't itself (`/proc/<pid>/status`). Found by looking at
  a real host, not by the tests, which had no otterd with threads to look
  at; one does now.

Not done: a Problems line for other otterds (D-077); stale tmux sessions
(needs the backend to list its sessions); disk size per workspace; stopping a
session's or run's process other than through its own commands; a task
agent's "Interrupt" shows only for runs that are live.

## D-162 — A session doesn't take a managed run's transcript for its own (2026-10-21)

A task waited on the developer's `claude` session, saying it was working
there, while the session sat at its prompt. Its agent state was `working`
since the task's last run had started and stayed so. The cause: both write
their Claude Code transcripts in the project's directory, and a session looks
for "the newest transcript written since it started" that no other session
has taken (D-137). A task's run was writing its conversation there when the
session started, so the session took the run's transcript for its own,
followed the run's turns — which ended on a tool result no assistant
answered when the run was interrupted — and stayed *working* with nothing
to end it. It would also have resumed the task's conversation if restarted.

- **`claimed` includes the native sessions of managed runs' conversations**
  (`Conversations::native_sessions`, read each pass without copying them), so
  a session never takes one.
- **A session already bound to one lets go and binds its own** (`managed` in
  the observe context): what its terminal is doing is not known until its own
  transcript says, so it is *idle* until then. A hook said it started and
  nothing since, so the quiet-turn fallback stays off, as before. The
  session id it held is **forgotten** (`Observation::unbind`) unless its own
  is found in the same look, so a restart doesn't resume the run's
  conversation.
- Tested at the observer (a transcript of a run, one of the terminal's own,
  hooks that said only that it started) and on the conversations' list.
  Not tested end to end: a real run and a real terminal in one directory.

Known and left: a terminal that deliberately continues a task's conversation
(Claude Code's `/resume` picker lists the run's transcripts too) never binds
to it now, for the id is claimed for good; and a run's transcript can exist
a moment before the conversation records its id, so a session may take it
and let go of it a pass later (self-correcting, a little noisy). Taking the
identity from the hooks' `transcript_path` would settle both. Also not done:
whether an answer-only task (D-092), which only reads, needs to wait for the
developer's agent at all.

## D-163 — `git rm` of the work's own files is routine, as `rm` is (2026-10-21)

D-066 made deleting the work's own files routine, but only for `rm`,
`rmdir` and `unlink`: the word `rm` inside `git rm` still asked the
developer as "deletes or destroys something". The live task bench (B07,
H02: "remove the demo command and the sample data") asked for
`git rm -q data/sample.csv && … make test` — an ask the developer can only
approve, about a file the task was told to delete.

- **`git rm` with every path inside the workspace runs without asking,**
  under D-066's limits: not the workspace itself, not `.git`, not a glob at
  its top. It is no harsher than `rm`, which was already routine: Git
  refuses a file with changes not committed unless forced, and a committed
  file is still in the history.
- **Unless Git may work elsewhere.** Git's environment moves its
  repository, work tree and index (`GIT_DIR`, `GIT_WORK_TREE`,
  `GIT_INDEX_FILE`), and then a path the policy reads as the workspace's is
  another directory's to Git (the Fable review deleted a file in another
  repository that way, with real Git). So `git rm` still asks on a line
  that sets any environment (`X=… git rm`, `export`, `declare`, `set -a`)
  or may (`source`, `.`, `eval`), or names `GIT_`, `core.worktree`,
  `--work-tree`, `--git-dir` or a `gitdir:` pointer, in any case (Git's
  keys have none), or mentions a function or alias anywhere (`git()
  {…}; git rm x` can be any command). `git -C <dir> rm` and `git -c … rm` ask too.
- **Not in scratch:** `rm` there is routine (D-066), but a repository in
  the temp directory is another's, and its work tree may be anywhere.
- **What Git reads itself still asks:** a pathspec's magic (`:/x`,
  `:(top)x`) points from the repository's top, which can be above the
  workspace, and `--pathspec-from-file` names its paths elsewhere. An
  option quoted (`'-r'`) is still an option, not a path.
- **Not done:** what an earlier command left behind isn't on the line:
  `git config core.worktree <dir>` run on its own (allowed, as before), a
  `gitdir:` written over `.git`, a function or alias named `git` defined
  in an earlier command, or an environment in a file the shell reads at
  start redirects later Git commands, a deleting `git rm` among them. A
  command's write into `.git` isn't asked, while an edit there is. The
  policy is a lexical reading, not a sandbox (D-059).
- Verified: the policy's unit test replays B07's command and the cases that
  still ask; both halves fail with the change, or the guard, taken out.

## D-164 — A delete is routine only where the policy can tell what it deletes (2026-10-21)

The review of D-163 found D-066's routine delete reading two things it
can't read as the workspace's. A substitution is a piece of its own
(D-059), and what stood in its place, a placeholder word, resolved to a
file inside the workspace, so `rm -rf $(echo /)` ran without asking (and
`never` didn't see `rm -rf /` either). And a `cd` the policy couldn't
follow (`cd "$X"`, `cd $(…)`, `cd -`, with an option, `pushd`, a `cd`
inside a substitution) left it wrong about where the shell was, so a
relative `rm -rf x` after it was routine.

- **What a substitution prints isn't a path inside:** a delete operand
  with a substitution in it asks.
- **After a move of the shell it can't follow, a delete isn't routine**
  until a `cd` to an absolute path it can read. It follows `cd` and
  `pushd` to a path it can read, also as `builtin cd`, `command cd` or
  `{ cd`; it can't follow one to a variable or a substitution, `cd -`,
  `popd`, a `cd` with an option, a relative one on a line that names
  `CDPATH`, a subshell's `(cd`, or `eval`. `cd` alone goes home. A `cd`
  inside a substitution runs in its subshell and doesn't move the line's
  shell. Writes and reads after such a move are judged as before: making
  `cd "$(git rev-parse --show-toplevel)" && make test` ask would be the
  noise D-066 removed.
- **Not done:** a write whose target is a variable or a substitution (`cp
  x "$DEST"`) still isn't read as outside; a `cd` in a sourced file, a
  function, or an earlier command isn't seen, nor is `cd x 2>/dev/null`
  followed (it asks: three words); `pushd +N`/`-N` turns the stack and is
  unknown, `pushd -n` isn't read. D-059's limit, unchanged.
- Verified: `a_path_the_policy_cannot_read_is_not_the_workspaces` fails
  with each check taken out (the substitution operand, the unknown `cd`,
  the subshell's `cd`); reading what a substitution names (`grep -n delete
  $(git ls-files)`), `{ cd src; rm -rf gen; }` and `pushd src && rm -rf
  gen` still run.

## D-168 — Resuming goes back to where the pause found the task (2026-10-21)

`docs/task-states.md` says a resume goes back to the state the task was
paused in, but the code worked it out from the steps alone. The live task
bench (B10, P01r) paused a task blocked on "Go ahead with the coding
agent's plan?" with `pause --all`, and `resume --all` put it in
`planning`, with no reason, while the developer-only decision was still
pending. No run started, so it sat there until the plan was approved. A
task paused in review fared worse on resume: with every step done, it went
back to verifying, and the check and the judge ran again on work already
reviewed.

- **A pause remembers where it found the task:** `Task.paused_from`, its
  status and reason (optional, so older documents load, and old peers
  ignore it). It is set by every pause (the developer's, Otter's, the wait
  limit's) and cleared when the task leaves paused.
- **A resume goes back there.** Review stays review, with its reason.
  While a decision only the developer may make is pending, the task is
  blocked on it again, with the reason it had, or "Needs your approval: …"
  for a document paused before this. Otherwise it picks up where its work
  stands, as before. So a decision answered or cancelled during the pause,
  such as the one the wait limit cancels, doesn't block it. `paused →
  blocked` joins the transitions for this. Both the developer's Resume and
  "go on" in Otter's conversation take this path.
- **`pause --all` stays "every task at work"** (D-081): blocked or at work,
  not review, where no agent is at work. Pausing one task in review is
  still `otter task pause`.
- **Verified:** `resuming_goes_back_to_where_the_pause_found_it` (blocked →
  paused → blocked with its reason; review → paused → review with a publish
  ask pending; an old document with no `paused_from`) fails with the
  resume worked out from the steps alone, as before.

## D-165 — Verification stops on the work; the judge sees what the agent ran (2026-10-21)

The task bench, B08 (task H02): the plan's `diff` criteria were facts about
whole files ("README.md and the Makefile no longer mention demo or
sample.csv", "docs/API.md does not mention `demo`"), and the work was right.
The first verdict found API.md unproven and asked for "a check you can rerun",
its output attached as evidence. The fix step ran the grep (no output, exit
1), changed nothing and said so. The second verdict, on the same tree, found
README and the Makefile unproven too: they "rest only on the agent's own grep,
with no captured output". That was another unmet set, so D-062's loop stop
(the same fingerprint *and* the same unmet) bought a second fix step, which
did the same; the third verdict matched the second and went to review
unverified. Two of six attempts went on a tree that never changed, and correct
work ended unverified. Task E01 showed the other half: the agent's
`tb demo | cmp - docs/demo.txt` passed, the judge never saw it, and the agent
added a test file only to give the diff something to show.

- **Stop on the work, not on the verdict's wording.** After a fix, the same
  fingerprint with no fewer unmet criteria goes to review, "Not verified:
  nothing changed after a fix", as D-062's same-mark case did: the same set,
  or another set that isn't a strict subset of the last. On the same work
  only a verdict that proves more (its unmet a strict subset of the last's:
  the fix's evidence proved something) gets another fix step, so each such
  round leaves fewer unmet and they end. Work that changed still gets its
  fix, whatever the verdict. A check that failed with nothing unmet, twice on
  the same work, still stops; one that leaves nothing unmet where something
  was gets one more round. This revises D-062's rule, under which a verdict
  that reworded or widened itself on an unchanged tree bought another round.
  Every verdict that leaves something unmet is kept as the mark
  (`last_verification`), the one that stops too, so the next compares
  against the newest (`another_round`). The mark keeps criteria by their
  text, so they are counted: of two with one text, one met is fewer.
- **The judge sees the commands Otter recorded.** A diff can't show that a
  word is absent from the rest of a file, or that a file equals a command's
  output; the coding agent's commands can, and Otter already journals them
  (D-058): each tool call with its run, its effect, its command line
  (redacted, cut at 200 characters, D-157) and its output (its first 2,000
  characters, redacted). At judging time the controller reads the task's
  conversations (`Conversations::get` for each run's `conversation_id`) and
  takes the `Command` tools (Bash, and Monitor watching a command; not the
  agent's Read, Edit, Grep or Glob tools) that ran to an end, in the runs
  since the last verdict and since the plan, never the look at the code's.
  `Task.judged_runs`, the task's run count at every verdict, met or not,
  marks where: after every criterion was met, review, CI failing and a fix,
  the judge sees that fix's commands, not everything back to the plan. The
  loop stop doesn't read it. The newest ten at most, about 8,000 characters
  in all, redacted again, in the order they ran, with how many earlier ones
  didn't fit (`brain::ran_commands`).
- **Otter's record and what a command printed are told apart.** The command
  line and whether it exited 0 are Otter's record. The output is whatever
  the command printed, and the agent decides that: `echo "verified"`, a
  script of its own that prints a listing, text addressed to the judge. The
  judge is told so next to the rest of its ranking: text a command merely
  prints (`echo` or `printf`, a heredoc, a script the agent wrote or one in
  the diff) is the agent's words, a claim; only what a program that examines
  the code produced (`grep`, `cmp`, `git`, `make`, the test runner) is a
  finding, and only about what it examined, so it reads the command.
  `succeeded` is Otter's record, whatever the output says. A command that
  didn't exit 0 has its exit code first in its output (`Exit code 1`, as
  Claude Code reports a failed Bash call), which for a search that found
  nothing is a finding, not a command that failed to run. The list is fenced
  as data, not instructions (`<commands>…</commands>`, as the plan review
  fences the plan), and a `</` in it is written `<\/` (the same JSON), so
  nothing a command printed closes the fence.
- **What was cut says so.** A command line cut where it was recorded
  (`command_cut`) can't be read whole, so the judge is told it proves
  nothing: a long heredoc or inline script doesn't count, however it ends.
  An output up to 1,500 characters is shown whole. Past that, a record that
  was cut (`ToolResult.truncated`) kept the output's start, so its first
  1,500 are shown, marked "the start of what it printed"; one kept whole
  shows its last 1,500, marked "the end of what it printed", a failed one's
  leading `Exit code N` kept.
- **Read from the journal, not copied into the task.** The commands are in
  the conversation's record already. Copied into `task.evidence`, they would
  grow `task.json` every round with a second copy of the output. The task
  keeps one number more (`judged_runs`); the prompt is built from what the
  journal holds.
- **A `diff` criterion is a fact the change shows.** The planning turn's
  format (`PLAN_FORMAT`, D-064), the brain's own plan prompt and the plan
  review's second check say so: "nothing in X mentions Y", or "X equals a
  command's output", belongs in the check command
  (`make test && ! grep -rn demo README.md`) as a `check` criterion,
  decided in code.
- **Not done:**
  - The command line is the conversation record's: cut at 200 characters,
    and its whitespace flattened (D-157's "Not done"). A command that
    doesn't fit is marked cut and proves nothing to the judge, so a fact
    only a long script shows has to be in the check command.
  - A long output cut where it was recorded shows only its start: its last
    lines, where a test runner sums up, aren't kept anywhere. Recording its
    end too would show both.
  - Commands from before the last verdict aren't shown, even on the same
    tree: an earlier round's grep that a later verdict could use isn't
    there. Output from before an edit may no longer hold; the newest round's
    commands are what the fix did.
  - The tiers are the judge's to apply: Otter doesn't sort commands into
    programs that examine and text that is printed.
  - Criteria with one text are told apart only by count, not by id (the mark
    keeps text): which of them is met isn't known.
  - A subagent's command (subagents are off) is shown as the agent's own.
  - A command run in the background shows only that it started: its later
    output comes through `BashOutput`, a read, which isn't shown.
- **Verified:** controller unit tests
  `the_same_work_with_more_unmet_after_a_fix_stops` (H02's sequence: unmet
  {A}, a fix that changes nothing, unmet {A, B} on the same fingerprint →
  review) and `the_same_work_with_fewer_unmet_gets_another_round` ({A, B}
  then {B} on the same tree → one more fix step, then {B} again stops; {A, A}
  then {A} too); `the_judge_reads_the_runs_since_the_last_verdict` (the
  first verdict, after one left something unmet, after one met all and a CI
  fix, after planning again); brain unit tests
  `the_judge_sees_the_commands_otter_recorded` (a grep that found nothing,
  with its exit; not the Edit or Grep tools, a command still running or
  another run's; a credential an old record kept taken out; the fence and
  the tiers), `the_commands_the_judge_sees_are_bounded` and
  `a_cut_command_or_output_says_so` (a cut line, a cut record's start, an
  output that tries to close the fence). Each fails with its part reverted:
  the whole mark compared again fails the first; no subset rule (the same
  work always stops), or plain sets instead of counts, the second; the
  count ignored, or ignored after a verdict that met everything, the window
  test; the judge's prompt without the commands, without the tiers or the
  fence, with no escape in the fence, no filter on the tool's effect, no
  bounds, no redaction on the way out, a cut line not marked, or a cut
  record read as whole each fail a brain test; `verify_step` not keeping
  the count fails the end-to-end test below. The end-to-end
  `verification_stops_when_a_fix_changes_nothing` (the same verdict on the
  same work) passes unchanged, and now checks the run count its last
  verdict kept. Untested: the glue between the tested halves
  (`commands_ran`: a run's `conversation_id`, the conversation, its
  tools), since the end-to-end brains don't read the commands. Not tried
  with a live model judging.

## D-166 — A task with nothing of its own to publish isn't asked to (2026-10-21)

The live task bench (B11, R03br): a task found its fix already on main,
changed nothing ("Working tree: no changes") and was verified. Review still
asked the developer to publish (`git push … and open a pull request`).
Allowed, delivery pushed a branch equal to main and ran `gh pr create`,
which GitHub refused ("No commits between main and otterd/r03br"), and the
task failed with gh's command line as its reason. A trigger allowed to
publish (D-083) takes the same path without asking, so a nightly check
that finds everything green would fail every night.

- **Nothing to publish:** in review, before anything is asked, a tree with
  nothing to commit (`committable`, which leaves secrets' names out as
  committing does) whose HEAD is where the task started (`started_from`,
  D-167) has nothing of the task's own to publish. So does one whose HEAD
  is no further than `origin`'s base branch, as last fetched (`rev-list
  --count origin/<base>..HEAD` is 0): that is all a task from before D-167
  can be judged by, and it holds for any task. A local `<base>` is no
  stand-in (in a worktree of a shared repository it can be anywhere):
  without `origin/<base>` it asks. A previous task's commit below HEAD,
  unmerged (D-156), is not this task's to publish.
- **Only when Git says so:** "nothing to commit" is what `git status` said
  (`committable` is a `Result` now). A status that fails (an index held by
  another, an I/O error) asks to publish, as before; so does a failure to
  read what a commit would leave out. One transient error must not make a
  task acceptable with work unpublished.
- **Then nothing is asked or pushed** and no pull request is opened: the
  pull request and CI gates are not applicable, "nothing to publish"; one
  message says so ("Nothing to publish: this task left the code as it found
  it, so there's no pull request to open."); and the task goes on to its
  report and waits for acceptance, as finished work does. When the only
  changes are files a commit leaves out, it says that instead ("Nothing to
  publish: only files that may hold secrets changed (.env), which aren't
  committed, …"; the names redacted, at most five). A publish ask still
  pending from before is cancelled. A trigger's allowance doesn't push
  either: the check comes before the allowance.
- **Said once:** the rationale keeps `Nothing to publish (<HEAD>)`. Review
  is stepped on every tick and says it only for a HEAD it hasn't said it
  for. A new commit or uncommitted work afterwards (a new ask, an edit by
  hand) is something to publish again, asked as before.
- **GitHub's word, as a backstop:** `gh pr create` refused with "No commits
  between" (GitHub's base already holds every commit: the local base is
  stale, or a pull request was merged since) is the same case, not a
  failure. It gets the same gates and a message ("Nothing to publish: main
  already has every commit of <branch>, …"), and the marker keeps the next
  step from pushing and asking GitHub again. The branch is on the remote by
  then, holding only commits its base has, so the push is recorded
  (`delivery` with its branch, head and `pushed_at`, no pull request):
  cancelling names the branch as it names anything it leaves (D-068), and
  the app says it was pushed with nothing to publish.
- **No fetch.** The decision needs no network: with a recorded start the
  base branch isn't needed for it, and without one a stale base means
  asking to publish as before, after which GitHub's answer settles it.
- **Delivery's errors are redacted whole.** What Git and gh printed was
  already redacted (`tail`); the command line before it wasn't. `failed`
  now redacts both, review's commit, push and pull request errors are
  redacted as they become a reason, and so is every reason `fail` records
  and otterd's log line for a step's error (invariant 13).
- **A CI log's colours go in gh's spelling too.** gh writes ESC as `^[`
  (caret notation) when its output isn't a terminal, so "Fix what CI found"
  carried `^[[36;1mmake check^[[0m`: D-139 removed only the escape byte.
  `without_colours` removes a control sequence in either spelling. A stray
  `^[` that starts no sequence stays.
- **The files a commit leaves out are named right.** `commit_work` read
  Git's status trimmed, so a first line for a tracked file changed in the
  tree (` M .env`) lost its leading space and the file wasn't named in
  "leaving out what may hold secrets". It reads the status as printed
  (`left_out`, shared with the message above).
- **Not done:** a triggered task with nothing to publish still waits for the
  developer's acceptance (D-156's note on triggers); the workspace's Changes
  panel (D-102, D-147) still counts "ahead" from the base as last fetched.
- **Verified:** e2e `a_task_that_changes_nothing_has_nothing_to_publish`
  (B11's shape: no publish decision, nothing pushed, no `gh pr create`, the
  PR and CI gates `not_applicable: nothing to publish`, said once, accepted
  without an override). With the check taken out it times out waiting for
  the report, the publish ask raised as on the bench. E2e
  `no_commits_between_is_nothing_to_publish` (the fake `gh` refusing as
  GitHub did) fails with the backstop taken out, with the bench's own
  reason: "Otter stopped: opening the pull request: gh pr create … No
  commits between main and feature/csv"; with the push not recorded, it
  fails on the delivery. E2e `a_status_git_cant_read_is_not_nothing_to_publish`
  (a broken index) times out, said to have nothing to publish, with a failed
  status read as clean and a failed read of what a commit leaves out read as
  none (with only the first, the second still asks). E2e
  `only_secrets_changed_is_nothing_to_publish_and_says_so` gets "this task
  left the code as it found it" without its part. Unit tests
  `colours_go_in_either_spelling` and `a_tools_failure_says_no_secret` fail
  with their part taken out, and `no_commits_is_told_apart` reads gh's
  refusal. The fake agent changed nothing, so the delivery tests that
  publish now ask it to write a file (`WRITE:<file>` in the request):
  something to publish.
- **On upgrading:** a task in review whose last CI failure was stored with
  `^[[…` in its log reads the next, cleaned, log as a different failure
  once: one more fix step, not a loop.

## D-167 — What a task changed is measured from where it started (2026-10-21)

B09 in the task bench: P02rb worked in workspace p02r after P02ra, on the
same branch; P02ra had been published, merged on GitHub and accepted.
P02rb's "Working tree" evidence diffed since `merge-base origin/main HEAD`,
and nothing had fetched `origin/main` since the merge, so P02ra's commit
counted as P02rb's change. The judge failed "the only code change to
money.py is …", a fix step had nothing to fix, and a correct task ended
"Not verified". (P01rc's evidence was wrong the same way; its criteria
didn't say "only".) D-156 holds the workspace so that the next task's work
is its own, but verification still read the previous task's commit as
this one's.

- **A task's start, `Task.started_from {workspace_id, revision}`:** the
  workspace's HEAD as the task's first run there begins (the planning turn
  or a step, `start_run`), read before the agent starts. It is kept for the
  task, and taken again only if the task works in another workspace
  (`SetWorkspace` after it ran). Not taken outside Git or before a first
  commit, and not for a task that ran before this was kept: its work may
  be committed already, and a start taken now would hide it.
- **Verification's diff is since the task's start**, while HEAD descends
  from it (`merge-base --is-ancestor`); else (no start recorded, a history
  rewritten) since the merge base with the base branch, as before. The
  evidence says which: "HEAD …; changes since this task started (at …)", or
  "changes since origin/main (merge base …)". The title stays "Working
  tree: …" (the judge reads the newest by it). Older tasks have no start,
  so their fingerprints don't change mid-task.
- **The report's changed files** are the task's own as well: delivery's
  "Changes on <branch>" stat is since the start, when HEAD descends from it.
- **No fetch:** with the start, the base branch's tip doesn't enter the
  diff, so verification still talks to no remote. (Delivery's fallback for
  a task without a start doesn't fetch either: D-166.)
- **An optional field**, absent in older documents. `Task` crosses the
  wire, and old peers ignore an unknown field: no protocol bump
  (protocol.md).
- **D-156's gap, partly closed:** a task after one whose pull request isn't
  merged yet is now verified, reported and judged for publishing on its own
  change. When it does have something to publish, it still pushes to that
  branch and adopts that open pull request (and rewrites its description).
- **Not done:** a task asked for more after it's done (D-050) keeps its
  first start, so its diff holds its earlier rounds too. The workspace's
  Changes panel and the task's Changes section (D-102, D-147) still count
  from the base branch as last fetched. A previous task's edits left
  uncommitted (D-156's first gap) are in the tree when the next task
  starts, so they are still its change. A task from before this, moved
  to another workspace afterwards, takes no start there either (it has
  runs): it is measured from the merge base, as before.
- **Verified:** unit test `a_task_is_diffed_from_where_it_started` (B09's
  shape in a repository: an earlier commit on the branch that the base
  doesn't have, then this task's edit; since the start only the edit shows;
  with no start, or a rewritten history, the merge base) fails with the
  start ignored. E2e `a_second_task_on_the_branch_is_measured_from_where_it_started`
  (the first task publishes on `feature/csv` and is accepted, unmerged; the
  second changes nothing): with verification ignoring the start, the
  second's evidence reads "Working tree: 1 file changed, 1 insertion(+)",
  the first's file, B09's error; with no start recorded, the second is
  also asked to publish the first's commit.

## D-169 — Another otterd is a stray only once it has stayed, and says which it is (2026-10-21)

The Work tab (D-161) counted every other otterd as something to look at.
A task's own tests start one per test, from the task's worktree, for a few
seconds each, so the badge jumped by four or five each time a task ran
`cargo test`, and then emptied. Nothing was left behind.

- **Another otterd younger than two minutes isn't a stray yet**: it is listed
  under Processes, without the flag, and doesn't count towards the tab's
  badge or the strip above the tabs. The rule is the app's (the daemon's
  `stoppable` is unchanged); an orphan is one at once.
- **An otterd's `--home`** is read from its arguments (a directory, never a
  secret; the rest of the command line still never leaves the daemon) and
  shown, so the one under a bench, a test or a build tells itself from a
  leftover: `OtterProcess.home`, an addition.

Not done: a rule by the home's place (a temporary directory is a test); a
test daemon that outlives its test for more than two minutes is still shown
as one to look at, which is the case worth a look.

## D-170 — What a line only prints or hands over isn't what it does; each shell keeps its own `cd` (2026-10-21)

On 0.7.57 the live task bench (H02v) still saw developer-only asks for
the work's own deletion. B12: the coding agent handed over its plan with
`echo '{… "git rm data/sample.csv" …}' >/dev/null`, and the word lists
matched `rm` inside the quoted text. B13: a `python3 - <<'E' … E` heredoc
before `git rm -q data/sample.csv` made D-163's guard read the Python as
shell (`p='…'` as an assignment, `()` as a function). Reviewing the fix
found that D-164's "a `cd` in a substitution stays there" also kept it
from moving the substitution's own shell (`echo $(cd /srv && rm -rf x)`
was read from the workspace), and that substitutions were read before
the pieces that run first (`cd /srv && echo $(rm -rf x)`).

- **A line that only prints does what its redirection does.** One `echo`
  or `printf`, on one line, with no other command, comment, pipe,
  substitution, variable, group or heredoc: its words run nothing, so it
  asks only for where it writes (outside the workspace, a secrets file).
  A printer anywhere else (in a substitution, piped on, next to another
  command) is read as before, since its output may run.
- **A heredoc's body is the input of the command that starts it.** Given
  to `cat` or `tee` whose output runs nowhere, it is the file they write:
  data, not read by the word lists. That is when the `cat` or `tee` isn't in
  a substitution (whose output the line may run: `$(cat <<E …)`), is the
  line's only command (nothing else on it may run what it wrote, by any
  name: `… > run.sh; bash r*.sh`), with no pipe, no process substitution
  (`tee >(sh)`, `> >(bash)`) nor any other on it, and no copy to another
  descriptor (`>&3`); and its word is quoted (`<<'EOF'`) or the body has
  no `$` or backtick: unquoted, the shell expands the body first, and a
  `$(…)` in it runs. Given to anything else (`python3 -`, `bash`) its
  lines are read as before, as a shell of their own. Where a heredoc
  starts is read outside quotes (`'…'`, `"…"`, `$'…'`), backslashes and
  comments, and not in `((…))` or for a word starting with a digit (a
  shift).
- **D-163's guard reads the line's own shell:** not a heredoc's body, and
  not a substitution's assignments, which don't reach it; a command that a
  substitution or a variable names (`$(…) …`, `$X …`) may be anything, so
  it counts. A `git rm` inside a substitution or a heredoc's body is never
  routine: that shell's Git isn't on the line.
- **Each shell keeps its own `cd`, in the order things run.** A
  substitution runs before the piece it stands in, starting where the
  line's shell is by then; a heredoc's body runs with its command. A `cd`
  moves only its own shell. A command a substitution or a variable names
  makes where the shell is unknown (it may be a `cd`).
- D-138's test expected `echo 'rm -rf /' > notes.txt` to ask; it now
  runs: it writes text in the workspace, as an edit would.
- **Not done:** splitting a line into pieces still doesn't understand
  quotes (a `;` in a quoted argument starts a piece), so an argument of
  another command can still trip the word lists; only a line that only
  prints is read whole. A file the agent writes and runs in another call is
  read only as its command (`bash run.sh`), as an edit and a run always
  were (D-059). A command writing into `.git` (`echo hook >
  .git/hooks/pre-commit`) or into a shell through a process substitution
  (`echo x > >(sh)`) isn't asked, though an edit of `.git` is and a `| sh`
  pipe is denied: unchanged, as before this.
- **Verified:** `what_a_line_only_prints_or_hands_over_is_not_what_it_does`
  replays B12's and B13's commands and the routes that still ask, and fails
  with each part taken out: the printer rule, the `cat`/`tee` body as data,
  the guard reading bodies, one `cd` for every shell (with the D-164 test),
  and a heredoc's start read inside quotes.

## D-171 — A message can carry pictures (2026-10-21)

**Why.** A screenshot says what a paragraph can't, and the task's message box
took only text. The terminal already takes a pasted image (D-145); the task
conversation did not.

**Decision.**
- **`task.send` takes `images`**: up to 4, each `{media_type, data}` (`image/png`,
  `image/jpeg`, `image/webp`; base64). At most 700 KB each and 2 MB in all,
  decoded. The bytes must be the type they say (a magic-number check); a
  picture that isn't, isn't base64, or is over a limit refuses the whole
  message, and nothing is written. A message may be pictures alone.
- **They are kept in the task's artifacts** (`task.artifact` serves them) under
  names of the message's own (`attachment-<message id>-<n>.<ext>`), written
  once the message is added, and the message lists them in `attachments`. A
  refused command, or a resend of one already done (a lost reply), writes
  nothing and changes no file: the resend is the message it already made.
  Deleting the task deletes them with its artifacts. The files are written
  first, so a message never names files that aren't there, and taken away
  again when no message was added. A message of pictures alone has the text
  "(pictures, no words)" (`PICTURES_ONLY`), so nothing that reads a message
  as words sees it blank; the app doesn't show that text over the pictures.
- **To the coding agent**: the message's pictures go with the words, in the
  same places the words go — a new run's first prompt, guidance to a live run,
  a redirect — and only the ones no earlier turn carried (the rule the notes
  in the prompt follow). A turn names them as `InputBlock::Attachment` (a file
  on the host); the run's actor reads the file and hands the runtime the
  bytes. On the SDK runtime the worker's `send_turn` takes `images` and builds
  image blocks before the text. `MAX_FRAME`, both sides, goes from 1 MiB to
  4 MiB so a turn with 2 MB of pictures fits one frame; the worker checks the
  same limits again (a command that breaks them is refused whole, as a
  `nack` of category `invalid_input`).
- **A runtime that can't see pictures** (`features.attachments` false: Claude
  Code's stream mode, Codex) is not given them, and the developer is told on
  the task, once per message that had pictures, in a system message —
  never dropped without a word. A picture whose file can't be read or isn't a picture is said in the
  turn's text the same way.
- **The app** takes pictures from the clipboard into the message box only when
  the clipboard has pictures and no text (a copied table has both, and means
  the text), shrinks each to a longest side of 1600 px and under 500 KB (four
  of those are under the total), shows them removable above the box, and
  shows a message's pictures in the conversation. There is no drag-and-drop.
  A message waits while a picture is still being made to fit. Also in
  this change, for the same message box: the Enter that confirms an input
  method's candidate (macOS Pinyin and the like) no longer sends the message
  (it and an Enter within 50 ms of the composition's end are the method's).

**Not done.** Otter's own reply turn (the brain): only the coding agent is
shown the pictures (D-181 shows them to Otter's turn too). Pictures on a
task's first request (`task.create`) and on Otter's host conversation. The
stop-redirect action (`redirect {text}`) takes no pictures.

## D-172 — The app comes to the front after it updates itself (2026-10-21)

After "Update", the app was replaced and started again, but its window didn't
come forward: Otter was running (its menu bar item showed) with no window in
sight, until Open was chosen there. The new copy is started by the old one
(`restart`), a background process that macOS doesn't make the front app; the
window opened behind, or not at all.

- Before it restarts, the app leaves a marker in its local data folder. A
  copy that finds it at start removes it and brings the window forward as the
  menu bar's Open does (`show`: show, unminimize, focus — which activates the
  app), once the window exists and a moment later, since a raise before the
  app has finished starting is lost. An app started any other way is left as
  it was: closed to the menu bar stays closed.
- A marker, not an environment variable or an argument: a restart runs the
  same command, and setting the environment of a running process isn't safe.

Not tested: it is the macOS app being started by another process, and the
tests don't start one. The Tauri crate isn't built on Linux either; CI's
macOS job compiles it.

## D-173 — A quoted separator doesn't cut a command; a home directory's logins are credentials by where they are (2026-10-21)

Milestone 1 of `docs/plans/agent-judged-risk.md`: what can be fixed in the
policy's own code, without a model, before anything is judged by one. (The
design names D-173 for the whole of it; this is the next free number, and the
judged-by-the-agent decision, if it is built, takes the one after.)

Two things the policy got wrong, both found by running lines through
`policy::classify`:

- `grep -E 'a|token' x`, `grep -rn "a|token" src`, `git log --grep
  'delete|token' --oneline` asked the developer ("touches credentials
  (`token`)"). The line was cut at every `|`, quoted or not, so the text after
  it was a command of its own, and its word counted. A search for one word
  (`grep -rn token src`) was fine since D-138; a search for two was not.
- `cat ~/.aws/config`, `ls ~/.kube/`, `cat ~/.netrc`, `cat ~/.docker/config.json`
  and `cat ~/.npmrc` were allowed: only the files in `CREDENTIAL_FILES` and
  the words (`token`, `password`, …) kept a login from being read, and these
  files hold logins without either.

What changed (`crates/daemon/src/runtime/policy.rs`):

- **Quotes decide where a line is cut.** `pieces_cut` cuts a line only at
  the `;`, `&`, `|` and newlines the existing `Lexer` reads as the shell's
  own, so one inside `'…'`, `"…"`, `$'…'`, behind a backslash or in a comment
  is part of a word. `grep -E 'a|token' x` is one piece whose command is
  `grep`, and D-138 reads only the command. A newline inside quotes is part
  of the word too.
- **The fallback.** When the text ends with a quote still open, a trailing
  backslash or an unbalanced `((`, the Lexer isn't sure how the shell reads
  it, and the line is cut at every separator as it always was. A line is
  only read as fewer pieces where its quotes are plainly balanced
  (`grep 'a|token x` and `grep \'a|token x` still ask).
- **A command behind quotes is read whole.** `sh -c 'cd /; rm -rf etc'` is
  one piece that isn't a reader, so the word lists read all of it and `rm`
  asks. It was allowed before: the quoted `;` cut off `rm -rf etc'`, which
  read as a delete inside the workspace, while the shell ran `rm -rf /etc`
  (the same for `bash -c "cd /etc && rm passwd"`).
- **Substitutions and comments follow the shell.** `$(…)`, `<(…)`, `>(…)`
  and backticks are read as commands outside quotes and in double quotes. In
  single quotes, in `$'…'` or behind a backslash they are only characters:
  pulled out as if they ran, the text after one would be read as a command of
  its own and hide what the line really runs (`echo '$(cat x' ; git push origin
  main ; echo x')'` pushes). A `#` starts a comment only at the start of a
  word: not after an escaped space or `;`, nor right after a substitution
  (`cat a\ #b;git push origin main` pushes). Both came out of the review of
  this change and have tests.
- **Home-directory credential places.** `~/.aws`, `~/.azure`, `~/.oci`,
  `~/.kube`, `~/.gnupg`, `~/.config/gcloud`, `~/.config/gh`, `~/.config/doctl`,
  `~/.docker/config.json`, `~/.netrc`, `~/.npmrc`, `~/.pypirc`, `~/.pgpass`,
  `~/.git-credentials` and `~/.vault-token` are credentials for a **command**
  that names them and for an edit, by their path from a home directory (`~/`,
  `$HOME/`, `${HOME}/`, `~user/`, the user's `$HOME`, `/home/<user>/`,
  `/Users/<user>/`, `/root/`), also when the line has `cd`ed there and names
  them relative to it, and in a bind mount (`-v ~/.aws:/root/.aws`). A doubled
  slash and another case (`.AWS`; a Mac's disk is not case-sensitive) are the
  same place. They are never granted for the rest of the task (D-066). A
  project's own `.npmrc`, a hostname such as `s3.aws.amazon.com` and
  `~/.dockerignore` are not them.

What this does not change: no model is asked anything; the word lists, the
reader and test exemptions (D-138), the routine delete (D-164) and what a
line only prints (D-170) are as they were; the grant rules (D-066) are as
they were except that the new places are never granted; `policy::resolve`
and the denials that are final are untouched.

Tests (`crates/daemon/src/runtime/policy.rs`, next to
`looking_for_a_risky_word_is_not_doing_it`): `a_quoted_separator_does_not_cut_a_line`
(the quoted-pipe searches are `Allow`; a command behind quotes, an open or
escaped quote, and a separator outside quotes still ask or are refused) and
`a_home_directorys_logins_are_credentials_by_where_they_are` (both ways, edits
and grants too). Not done: the agent's own Read, Grep and Glob tools carry no
path in `ToolCall::Read`, so a login read through them is not asked about
(only a command or an edit is); paths built through a variable other than
`$HOME`; what a script does inside; and a word such as `token` in a search
that reaches outside the workspace (`grep -rn token ../other-repo` still
asks). Those are for the next steps.

## D-174 — Otter's model judges what a word flags and the reflog brings back; the task's own worktrees are inside (2026-10-21)

Milestone 2 of `docs/plans/agent-judged-risk.md` (D-173 was milestone 1). The
policy stays the floor; what it flags only by a word, where the damage can be
undone or there is none, is no longer the developer's by default.

Supersedes, for the judged classes only: D-049's "any word-listed command asks
the developer"; `decide_open`'s "a developer-only ask is never shown to a
model" (the floor keeps it); D-044's "a denial can't be turned around", for a
denial **by the model**; D-066's "other directories still ask", for the
task's own worktrees. D-066's "credentials are never granted" is unchanged.

What changed:

- **Judged asks.** `ask_judged` is an `Ask` with `user_only: false`, High
  risk (so it is told apart from the unknown-tool Medium ask). Judged:
  `git rebase` and `git branch -d` (local history; the reflog brings it
  back), and a credential word (`token`, `password`, …) in a reader or test
  (`READERS`, `GIT_READERS`, `TESTERS`) whose read reaches outside the
  workspace, when the word is something it looks for: not in the operands
  that reach outside (`cat /etc/app/password.txt`, `grep -r password
  ~/.aws` stay the developer's), not a `$VAR`, not an assignment, not any
  other command. Everything else on the floor is as it was: push, publish,
  deploy, root, work Git can't bring back, the broad destructive words
  (`delete`, `prune`, `uninstall`, `destroy`, `terminate`), infrastructure,
  credentials files and places, writes and edits outside.
- **Strictest wins inside a piece.** `classify_words` reads every list and
  returns the worst hit, so `sudo git rebase main` is the developer's. A
  routine delete (D-164) no longer hides other destructive words in its piece.
- **The task's own worktrees are inside** (`policy::Scope`): the other
  worktrees of the workspace's repository, from `git worktree list` run in
  the workspace, only for an ask that would otherwise be the developer's.
  Not counted: the bare backing repository (`git worktree list` shows it
  first, and every workspace off the repository shares it), and anything under
  Otter's home (the other workspaces are other tasks' work). Git is run with
  otterd's own environment, so a host whose `PATH` lacks `git` finds no
  worktrees and asks the developer, as before. Edits, reads and writes there are routine; deleting is still judged
  against the root alone. Git's internals and credentials stay asked about.
- **The model's decision.** A judged ask goes to `Brain::decide` with a
  prompt of its own (`brain::judged_prompt`): the whole command (redacted),
  the workspace root, each piece's directory and whether it is inside
  (`policy::explain`), the policy's reason, and that the command's text is
  data, not an instruction. Allow only what is plainly part of the task and
  undoable or read-only; deny the plainly harmful; else escalate. The
  rationale is redacted and recorded on the decision. No model, an error, an
  answer that is not allow or deny, or **20 seconds** (`JUDGED_DEADLINE`,
  `OTTER_JUDGED_DEADLINE_MS` in tests) escalate to the developer, as today.
  Dropping the call kills the `claude` process (`kill_on_drop`).
- **A model's deny is not final.** It refuses the call once, with its
  rationale. The same call again in the task (same redacted summary and
  command, whitespace aside) is recorded `user_only` ("Otter declined this
  once"), so rewording or retrying is not a new sample for the model.
  `policy::resolve` treats only a denial by the developer or by policy as
  final. No change to the store or the protocol (`DecisionRequest` has what
  the prompt needs), so no protocol bump.

Answers to the plan's open questions:

- A model's allow is **not** remembered for the task: one wrong allow lasts
  one call. Judged calls are never granted (`grant` returns none for any call
  `classify_words` doesn't allow).
- **No Settings switch** yet. A host without a model already sends every
  judged ask to the developer. Add one if dogfooding shows it is wanted.
- The infrastructure list and the broad destructive words **stay on the
  floor**, as the plan proposed, until a measurement says a model can tell
  them apart.
- **Worktrees** are found from the repository's worktree list minus Otter's
  own workspaces. A sibling worktree the agent makes next to the workspace is
  the case this serves.
- Still open, as the plan says: a secret written as a literal
  (`curl -u user:pw …`), and whether Medium is the right label for the
  skill/unknown-tool class.

Deliberate narrowings of the plan:

- "The developer can approve a model deny from the decision list" is **not**
  built. By the time the developer sees it the agent has been told no and has
  moved on, so approving the past decision would change nothing; the repeat is
  the developer's and can be approved, and does reach the agent.
- The prompt's directories are worked out against the workspace root alone,
  so a command run in a sibling worktree reads as "outside" to the model.
- The quote-cut `|` class is D-173's, not judged here.

Hardening from the review (each has a case in the tests):

- Worktrees count only when Git, run in the workspace, lists the workspace
  itself as one: an empty workspace inside a checkout, or a `.git` file the
  agent pointed elsewhere, finds none. Editing the workspace's `.git` is
  Git's internals (the developer's). Paths are compared where they really
  are (symlinks).
- `git worktree add|move` to a place outside the workspace is a write
  outside, so the developer's: a directory made that way would become inside.
- A read of a directory that is, or holds, the workspace, the home directory
  or a login (`grep -r token ~`, `..`, `/`) is not a search for a word: it
  keeps the floor. So does a piece with a blank inside quotes or behind a
  backslash (split on blanks, a path is read as two), and a credential word
  in any option (`--include=password.txt`).
- An `answer` from the model on a judged call escalates; only allow and deny
  stand.
- A judged read in a sibling worktree is widened like the rest.

Known and left: `git -c x=y rebase main` isn't read as a rebase (the old
`git rebase` entry matched the same way); a repeat is "the same call" by
whitespace only, so `git rebase main #` is a new sample for the model.

Not measured: how often the model allows, denies and escalates the judged
classes (the plan wanted the bench's recorded calls run through it first). If
it escalates nearly everything the problem is not removed; that is the number
to look at after dogfooding.

Tests: `what_is_only_a_word_or_comes_back_is_judged` and
`a_tasks_own_worktrees_are_inside` (policy), `a_judged_call_is_shown_whole_with_where_it_runs`
(brain), and in `crates/daemon/tests/e2e.rs` `otters_model_judges_what_comes_back_from_the_reflog`,
`without_a_model_or_in_time_a_judged_call_is_the_developers` and
`a_model_denial_is_not_final_a_repeat_goes_to_the_developer` (new test brains
`no` and `sleepy`; the fake agent's `ASK_REBASE` and `ASK_REBASE_TWICE`).

## D-175 — A task's conversation is told apart by colour, close together; its details are as wide as you drag them (2026-10-21)

Dogfooding: every message had a line above it (who, and when), Otter's
bubbles took 720 px for one line, a decision taken was a card of two lines,
and the details beside the conversation stayed 380 px. The developer asked
for it closer together, like a chat app, with their words and Otter's and the
coding agent's told apart by colour; for the details to be resizable; and for
a taller message box.

- **Who said it is the bubble's colour:** yours blue (`--you-bg`), on the
  right; Otter's grey (`--raised`); the coding agent's violet (`--agent-bg`),
  which keeps its name above, a third voice. No line above yours or Otter's:
  the time shows beside the bubble on hover, the full date in its tooltip.
  Yours and Otter's are `article`s named for who spoke, for a screen reader
  (the agent's head names it). Otter's mark (D-176) no longer shows in the
  thread, only in the sidebar. On light, the bubbles sit on D-176's white:
  text, the agent's name and its time read at 4.7:1 or better on each.
  Your words break inside a long word or link, as Otter's Markdown does,
  instead of running out of the bubble.
- **Close together:** a bubble is as wide as what it says (up to 720 px), 6 px
  from the next, a little more where the speaker changes; the thread, the
  decision waiting and the message box sit in one column of reading width
  (820 px) in the middle. A decision taken is one line: the verdict, what it
  was on (cut, whole on hover), its risk, kind and time.
- **The details panel** (Overview, Activity) has a handle on its left edge,
  as the Files panel has (D-097, which left this split out): 380 px at first,
  260 px or more, and the conversation keeps 360 px; remembered on this Mac.
  In a window too narrow for both (under 1000 px) they stack and the handle
  goes.
- **The message box** is 120 px at first and at least 84 px (was 96 and 56).
  Since D-171 it hadn't been the height it was given at all: a `flex: 1` from
  before the box had pictures above it sized it by its rows, about 57 px, and
  dragging its edge did nothing. A height remembered below the new least
  opens at the least.
- The conversation's scroll to its end no longer returns what scrolling
  returns: in Chromium 153, where these checks ran, `scrollIntoView` returns
  a promise, which React took for the effect's cleanup and failed on,
  emptying the view. The macOS app's WebKit evidently doesn't (the view
  shows there); it would have broken the day it did.

Checked in Chromium with sample data, dark and light, wide, narrow and
stacked, with the handle dragged; not in the macOS app itself.

## D-176 — The light theme is white and neutral; Otter's mark is one colour (2026-10-21)

The light theme's grounds were warm greige (`#f2f1ed`, `#ebeae5`, `#fbfaf7`:
red over green over blue), so the app read as yellow, and several text roles
fell under 4.5:1 on the ground they sit on (`--faint` 3.72 on a selected row,
`--hl-comment` 3.18, `--failed` 4.36 on its banner, `--done` 4.11 on the
sidebar). The grounds are now neutral, as macOS draws them: the sidebar, bars
and fields `#f5f5f7`, the terminal, panels and cards `#ffffff`, lines
`#e5e5ea`/`#d1d1d6`. Every text colour reads at 4.5:1 or better on each
ground it sits on — hovered (`#ececf0`) and selected (`#e4e4e9`) rows, button
hover (`#e2e2e7`), and the code colours on a diff's added and removed tints,
over white and over a task's changes box; `--working` (`#007aff`), `--needs`
and `--idle` are fills and glyphs only (3:1), and the three places that wrote
text in `--working` use `--working-text`. Dark is unchanged. The proposal, with each ratio:
https://claude.ai/artifact/Qh1BhCMK284Ccf1RwVqiwc.

The terminal follows: `LIGHT`'s background is `#FFFFFF` (with `--term`, which
`.xterm-viewport` is forced to), and on light only xterm's
`minimumContrastRatio` is 4.5, so a program's own colours meant for a dark
ground (Claude Code's dark theme: its greys, its yellow at 1.3:1) are lifted
until they read. xterm lifts faint (SGR 2) text only to half that, and leaves
box-drawing lines as they are. It changes foregrounds only: the dark bar behind
a submitted prompt and the dark green of an added line are Claude Code's theme,
chosen in Claude Code (`/theme`).

Not done here: making Claude Code's *auto* theme follow the app. Auto asks
the terminal its background (OSC 11); in Otter the session's tmux answers
(D-118's client leaves those answers to it), and with no client terminal
behind it tmux answers black, so auto always picks dark. With a pane
`window-style` background tmux answers that colour instead — the way to do
it, measured on tmux 3.7, left for its own change.

The mark is the monochrome otter (concept art traced to curves): the app icon
is it on a white tile on the macOS grid; in the menu bar it is a template
image (macOS colours it for its bar) when nothing needs you and orange when
something does; in the app it is `assets/otter-mark.svg` used as a mask, so
it takes the colour of the text around it in either theme.

## D-177 — A task's conversation is a chat: Otter at the top, the message box floating at the bottom (2026-10-21)

A task's conversation (D-175) now reads like a messaging app, as the
developer asked, after Muse and iMessage:

- The message box is one line in a pill, with **+** (pictures, as a paste
  takes them, D-171) on its left and a round send on its right; it grows with
  what is written, up to half the conversation's height, then scrolls. This replaces
  D-175's taller box and its drag handle (`composerResize`): the developer
  chose the growing box over a fixed height.
- The box, and a decision waiting above it, float at the bottom of the
  conversation, which scrolls under them (a sticky block, last in the
  scroll); scrolled to its end, the last message sits just above them, and
  stays there when what floats grows (a decision arrives, a picture is
  added). Scrolled up, the conversation stays where it was.
- Otter sits at the top, floating the same way: its mark, its name, and what
  it is doing — working (a ring turns and Otter bobs; still if the Mac asks
  for less motion), needs you (an orange ring), replying, or the task's
  status. Otter writing its answer is three moving dots in its bubble.
- Clicking Otter opens the details (Overview, Activity) on the right; × or
  Otter again closes them. They are closed until asked for, then as last left
  on this Mac (`otter.task-details`). Closed, they stay mounted, so a tab and
  its place are kept.

Tests: `keeps the details closed until Otter at the top is clicked, and
remembers it (D-177)`, `adds pictures from the + as from a paste (D-177)`, and
`composerGrow.test.ts`.

## D-178 — Chat and Workspaces; one title bar (2026-10-21)

The Inbox (D-072) repeated what the other views already showed: a task that
waits on you is first in the task list, under NEEDS YOU; a workspace is first
in the sidebar; the menu bar item and the notifications say both. The
Activity Bar had three entries for two kinds of work. Its top 52px held only
the macOS buttons: the bar's edge ran through the yellow one, and the
sidebar's title sat a row lower, under an empty strip.

- **The Inbox leaves the app.** The Activity Bar has Chat (⌘1, a speech
  bubble: the tasks, D-107) and Workspaces (⌘2). The app opens on Chat the
  first time; a view remembered as the Inbox opens Chat. What Chat lists are
  still tasks: New task, Find a task.
- **What the Inbox did goes to the other two.** Its dot becomes two: Chat's
  for tasks that need you, Workspaces' for workspaces that need you or failed
  (the Inbox's rule and the menu bar item's; Workspaces' dot used to leave
  failed ones out). A task's notification is held while Chat is in front, as
  it was while the Inbox was. Pause all tasks (D-081) is under Chat's ACTIVE
  label and holds every task at work, whether the search shows it or not;
  a search that hides every active task hides the button with them.
  Deciding, accepting and retrying from the list are gone: they are on the
  task's page.
- **The bar:** 22px icons with a 2px line (the gear too), 40px round buttons
  in a 52px bar. The selected one sits on a filled circle, without the mark
  at its edge; the dot is 8px, ringed in the bar's colour.
- **One title bar.** On macOS the top 52px is one band over the bar, the
  sidebar and the pane, with one line under it. The traffic lights sit in
  it, and the bar's edge starts below it. The sidebar's title is in the
  band, right of the lights: Otter's mark and name, the two counts and New
  workspace (in Chat, its name and New task). The row the name had is gone,
  so the list starts higher. When the row runs out of room (a sidebar
  under about 235px on a Mac) the name is left out; the counts and the
  button keep their size. The row is the size container for that, not the
  sidebar: a size container could hold the sidebar's menus where older
  WebKit draws them.

The design, before and after: https://claude.ai/artifact/L59aBAQWnDfHi828oii62k.
Checked with the app's tests and a production build, not in the macOS app
itself. Supersedes D-072, and the Inbox's part in D-073 and D-081; D-041's
views are now two.

## D-179 — A task can make its workspace; Otter finds or makes it from what the developer said (2026-10-22)

Plan `docs/plans/chat-bootstrap.md`. Starting work meant choosing a host, a
workspace and a repository by hand before the first word of the task. Otter's
conversation on a host (D-093) could make a task, but only in a workspace that
already existed.

- **`task.create`'s `workspace`** is a workspace by name (as before), `{new:
  {repository, base?, branch?}}` or `{scratch: {}}`. The task and its
  workspace are made by one command: the workspace is named
  `<repository>-<title>`, on a branch of its own, marked as made for the task
  so D-105 puts it away when it ends — the code is `own_workspace`'s (D-080),
  now one function. The old string form is unchanged on the wire, so no
  protocol version bump; an older otterd refuses the object forms.
- **Made once, and never an error after the task.** The applied check comes
  before anything is made, and the workspace is marked for its task in the
  same write that makes it. If making or choosing it fails (a name taken
  meanwhile is looked for again), the task stays, with Otter's message saying
  why and no workspace ("Choose one to go on"), instead of an error that a
  retry — a new command — would answer with a second task. If otterd stops
  after the workspace is made but before it is chosen, a repeat of the command
  finds the workspace made for the task and uses it. (Neither failure path is
  tested: they can't be reached from outside.)
- **A repository Otter read is one it can hand back.** Listings show it
  without its credentials (D-075); `task.create` takes a repository equal to
  a stored one as shown to mean the stored one, so the same backing
  repository is used (D-108) and it authenticates. A branch or base must be
  one word and not start with `-`.
- **Otter's tools.** `create_task` takes `workspace`, `new_workspace` or
  `scratch` (at most one); `list_workspaces` gains the repository, the brief,
  `held_by_task` (a run there, or another task's unfinished work in a Git
  workspace, D-156 — what would make a new task wait) and a `query`; `list_repositories` is new — the
  repositories this host's workspaces come from, no network. It is the
  developer's word that lets Otter make a task; the instructions tell it to
  ask when it is unsure or the repository is its own inference, never to make
  a repository up, and to say a workspace is "being prepared", not how.
- **Cloning is automatic** for a repository the developer named. A task whose
  workspace can't be prepared waits and says why (D-135), as before.
- **No new state.** A task with no workspace was already `Blocked`, "Choose a
  workspace for this task" (D-043).
- **The CLI**: `otter task new --repo <url> [--base <rev>]` and `--scratch`.
- **Choosing a host is the client's** (otterd knows one host, and the
  conversation stays one per host, D-093): `otter chat` and the app's New chat
  will ask every connected host for its workspaces (`workspace.list`) and open
  the chat on the one that has the repository the message names, or ask which.
  Not in this change.

Tests: `chat_bootstrap.rs` (e2e), `workspace_choice_tests` (protocol),
`repo_names_are_the_readable_part`.

## D-180 — A task stops from beside its message box: Pause, or Stop & send (2026-10-21)

The task view had three ways to stop the coding agent, and the developer
couldn't tell them apart: **Interrupt** and **Pause** in the header (both
leave the task paused, the agent's conversation kept; Pause also ends the
run), and **Redirect now** by the message box. The header also had
**Preview**, which the developer found of no use.

- One way to stop: **Pause**, beside the message box while the task is
  planning, implementing, verifying or blocked. **Resume** stays in the
  header, with Start, Retry and Accept: what the task can be told to do next.
  The host's Work tab offers Pause on a run's row where it offered Interrupt.
- **Redirect now** is **Stop & send**: it stops the agent's current turn and
  has it do what is written next, as before (`redirect`).
- Interrupt and Preview are gone from the app. The daemon keeps the
  `interrupt` and `preview` actions, for the CLI and anything else that asks.

Tests: `shows the coding agent's work in Activity and offers pause and stop &
send while it runs` and `offers neither where nothing runs`.

## D-181 — Otter's turn sees the developer's pictures (2026-10-22)

**Why.** Dogfooding D-171: a screenshot pasted into a task, and Otter
answered that it couldn't see it — and couldn't tell whether the picture was
sent at all. Otter's turn was given each message as `[id] text`, the
pictures neither shown nor mentioned, so the model guessed. The coding agent
got them; the one the developer was talking to didn't.

**Decision.**
- **Otter's turn is shown the pictures of the messages it answers**, read
  from the task's files as a run's turn reads them (`turn_input`): one that
  can't be read, or isn't a picture, is said in the text. On the Claude
  worker (`otter` profile) they go on `send_turn` as `images`, as a coding
  run's do; on Anthropic's API, as image blocks before the prompt. A picture
  whose file is gone isn't shown, so the numbers in the prompt are the
  pictures the model has. A command the worker refuses (a `nack`) ends the
  turn with the worker's reason, not at the turn's deadline. A backend
  says whether it sees pictures (`TurnBackend::sees_pictures`): behind the
  OpenAI shape it doesn't — some of those models refuse a request that has
  one, which would lose the whole answer — and the host conversation and a
  follow-up's turn have none to show.
- **Each message says which pictures are its**: `[msg] text [pictures 2–3]`,
  numbered in the order the model is shown them, and the ones it isn't shown
  (`[2 pictures you aren't shown]`, `[…; 1 more you aren't shown]`). The
  recent conversation marks a message that had pictures (`[1 picture]`).
  Otter's instructions say the pictures it is shown come with the messages,
  one it isn't shown it can't see, and a message's pictures go to the coding
  agent with its words.
- **A turn carries at most what the worker takes** (`runs::fitting`): the
  newest pictures, four, 2 MB in all, in the order they were sent. One
  message can't be over that (D-171), but a turn gathers several: Otter's
  over the messages it answers, and a run's first turn over every message
  since the last (the worker refused such a turn whole, as `invalid_input`).
  The ones left out are said: in a run's prompt ("2 older pictures … are
  left out") and, once the run has started, to the developer in a system
  message on the task (they aren't offered again: send again what it still
  needs); in Otter's,
  as "you aren't shown".

**Not done.** Pictures in the OpenAI-shaped providers (it would need to know
which models read them). Otter without its tools (D-096, `reply`): its
recent conversation marks the pictures, but it isn't shown them. Pictures in
Otter's host conversation, and on a task's first request (both still
D-171's). A worker older than D-171 with the Otter profile ignores
`images`; `otter host install` updates it with otterd. The limits are the
same numbers in otterd and the worker, kept by hand; the worker's tests
check it takes the most otterd sends.

Tests: `a_turn_says_whose_the_pictures_are`,
`a_turn_carries_the_newest_pictures_that_fit`, `the_pictures_go_with_the_turn`,
`a_turn_the_worker_refuses_says_why`, `a_first_message_with_pictures`, the
worker's `a turn's pictures are checked`, and end to end
`otters_turn_is_shown_the_developers_pictures` and
`a_runs_first_turn_carries_the_pictures_that_fit`.

## D-181 — A message goes to the host that has the workspace it names (2026-10-22)

Plan `docs/plans/chat-bootstrap.md` P2. otterd knows one host, and Otter's
conversation stays one per host (D-093), so choosing the host is the client's
(D-179). A developer with several hosts had to say `--host` every time, or
pick one in a list, before saying what they wanted.

- **The rule** (`crates/client/src/route.rs`; the app has the same in
  `route.ts`, with the same cases tested in each): from the workspaces each
  host has, a message that names one — a workspace by name, or its
  repository as `owner/name` or an address — belongs to that host. The bare
  name of a repository (four letters or more) counts less than a workspace's
  name or `owner/name`; a name inside a longer one (`billing-engine`) is not
  a match; a credential in a stored address is not a name. The host with the
  best match wins; equals, or none, are not a match. Names are ASCII, so any
  other character ends one: "在billing里加上重试" names `billing`. A workspace
  name needs four characters to count on its own (`api`, `web` and `add` are
  in every sentence). Workspaces put away (D-105) are left out. No model is
  asked, but a message does go to the host it picked: a wrong guess costs a
  switch of host, not a task.
- **`otter chat <message>` and `otter task new`** with no `--host` ask every
  host for its workspaces at once (`workspace.list`, five seconds each; a
  host that is down is left out) and use the one the message names, saying so
  on stderr ("→ gpu (workspace `trainer`)"). Otherwise the default host; with
  none, "several hosts; choose one with --host", naming the hosts that tied
  (the CLI doesn't ask which). Asking is not worth a prompt: the connections
  are made with `BatchMode=yes`, and a host that doesn't answer in five
  seconds is left out and named on stderr ("not asked, unreachable").
  This is a change: with a default host, a message used to go there without
  dialling the others.
  `otter chat` with no message still needs a default or `--host`.
- **The app's New task** has "From what I write" as its host, when there are
  several: what is typed in the title, the request and the repository
  chooses, a line under the box says which host and why, and a host chosen by
  hand wins. With nothing named it starts from the host the last task went
  to (`otter.new-task-host`). Its workspace box gains "A new one, from a
  repository…" and "A new one, an empty directory" (D-179).

**Not done.** The app has no conversation with Otter on a host today (its
chat is the tasks, D-178), so "New chat" there is New task:
the developer still writes the title and request. Otter choosing the
workspace inside a *task's* conversation (tools in a task's turn) is the next
step. A choice among hosts that tie is not offered as a card.

Tests: `route::tests` (Rust), `route.test.ts`, three in `TasksView.test.tsx`
(routing and its hint, a host chosen by hand, a workspace made from a
repository or empty), `a_message_goes_to_the_host_that_has_the_workspace_it_names`
(CLI e2e, two real hosts). The Tauri crate isn't built on Linux: the only
change there is `workspace` taking the new type.
