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
event log needs rotation.

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
- The agent binary is the session's process (no shell in between).
- **Identity:** the rollout file `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`;
  its first record (`session_meta`) has the conversation id (stored as the
  session's opaque `provider_session_id`, used to resume) and cwd. Codex creates it lazily (after the first interaction), so it's
  discovered after launch: from the process's open files (Linux `/proc`), else
  by cwd + start time, skipping conversations bound to other sessions.
- **State** from transcript records: `task_started` → working;
  `task_complete` → waiting for input (with `last_agent_message`);
  `turn_aborted` → idle.
- **Heuristic (unverified against real approval prompts):** approval requests
  and questions aren't in the transcript. An agent that is mid-turn and whose
  terminal *and* transcript have been quiet for `OTTER_AGENT_QUIET_SECS`
  (default 8 s) is marked `blocked`. If the Codex spinner keeps animating
  during an approval prompt, this misses (never cries wolf during long model
  thinking). Likewise a freshly started agent that goes quiet becomes `idle`.

**Open for the user to decide:**
1. Use hooks via `--dangerously-bypass-hook-trust` for exact approval detection
   (`PermissionRequest`)? Precise, but bypasses Codex's hook trust checks for
   that process.
2. Calibrate the quiet heuristic with a few real Codex turns.
3. Codex asks "trust this folder?" for every new worktree path and stores the
   answer in `~/.codex/config.toml`. Otter could pre-trust its own worktrees
   with `-c projects."<root>".trust_level=…`, but trust level changes Codex's
   approval defaults, so that's left to the user.

Everything parses defensively: the rollout format belongs to a self-updating
binary. The message excerpt is stored in `state.json` only, never in
`events.jsonl`.

## D-014 — Attention: derived from transitions, one open item per session (2026-10-07)

- Raised by: agent finished a turn (`review`, with the message excerpt),
  agent quiet mid-turn (`approval`, heuristic), task succeeded (`completion`),
  task/service failed or a process disappeared (`failure`), a session failed to
  start or a workspace failed to prepare (`failure`). Leaving a shell or
  quitting an agent cleanly raises nothing.
- Resolved by: attaching to or typing into the session, restarting / stopping /
  deleting it, the agent starting to work again, a newer item for the same
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
  SQLite wait until the log is actually a problem.
- **Known gap:** a reset log that has since grown past a client's cursor is
  not detected. Fix if it bites: a log id generated with the file, returned by
  `state.snapshot` / `events.subscribe` and echoed by clients.
- **Protocol v2.** `events.subscribe` gained params and a `{seq}` result, plus
  the `cursor_expired` code; v1 clients couldn't parse those, so the version
  was bumped. v1 was pre-stabilization. From v2 the compatibility rules in
  `protocol.md` apply: optional fields, new methods, new event kinds and new
  error codes are compatible (`Event::Unknown`, `ErrorCode::Unknown` make old
  clients tolerate them); frame changes and semantic changes bump.

## D-017 — Attach hardening (2026-10-07)

Audit of `session.attach` against repeated cycles and failure modes (dogfood
plan §6–§8), with tests for each that can run locally and a manual network
test against a real SSH host.

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
  `claude --resume <session-id>`.
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
- The quiet-turn heuristic (permission prompts aren't in the transcript) is
  now shared: `agents::settle`, used by both providers.
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
