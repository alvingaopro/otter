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
`~/.config/otter/cm/`) so repeated connections reuse one SSH connection.

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
  nobody should work in the backing checkout.
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
  scrubbed of `user:token@` in URLs.
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
  to open it), auto-update inside the app, and Linux desktop builds.

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
  a per-host off switch.

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
2 s after the prompt appeared → `working` before `PostToolUse`. Once any hook
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
anyway, but detaching without answering doesn't re-raise it). Hooks are
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
  no token and puts none on a command line. Calls: `pr view/create/edit`,
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
