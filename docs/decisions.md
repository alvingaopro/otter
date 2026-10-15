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
  No credential manager: Otter never sees or stores it.
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
  `repeating_failure` stay, as backstops.
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
  identity of the host's own, the commit is Otter's. A tree with work
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
  open.
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
