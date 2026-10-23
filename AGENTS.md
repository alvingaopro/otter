# Otter — notes for coding agents

Otter is a control plane for persistent development work on remote machines:
**Workspaces → Sessions → Attention**, not hosts → ssh → tmux → panes.

## Read before you change

Always: [`docs/invariants.md`](docs/invariants.md) — the 13 rules every change
must keep, with the tests that guard them.

| If you are changing…                                   | Read first                                                                 |
|--------------------------------------------------------|----------------------------------------------------------------------------|
| workspace lifecycle, sources, Git, environments        | design §7–§18, §28 · decisions D-010–D-012 · invariants 1, 9, 10           |
| sessions, executions, tmux, attach                     | design §10–§12 · decisions D-007, D-009 · invariants 2, 3, 7               |
| an agent integration (Codex, Claude Code, or a new one) | `crates/daemon/src/agents/mod.rs` docs · decisions D-013, D-015, D-023 · invariants 5, 6 |
| attention, the `otter ls` view                       | design §21, §26 · decisions D-014, D-099                                   |
| the desktop app                                        | `apps/desktop/README.md` · decisions D-018, D-085, D-125 · invariants 11, 12 |
| the wire protocol, events or client                    | [`docs/protocol.md`](docs/protocol.md) · decisions D-004, D-016 · invariant 12 |
| tasks, the controller, delivery                     | [`docs/task-states.md`](docs/task-states.md) · decisions D-045, D-047, D-062–D-068 |
| Otter's conversation, the brain's tools                | [`docs/plans/brain-runtime-v2.md`](docs/plans/brain-runtime-v2.md) · decisions D-084, D-089–D-096 |
| watches, triggers, the app's Automations view          | [`docs/plans/watches.md`](docs/plans/watches.md) · decisions D-083, D-183–D-185 |
| transport, daemon lifecycle, persistence               | decisions D-003, D-005, D-008 · invariants 4, 8, 11, 12                    |
| the architecture itself                                | [`docs/design.md`](docs/design.md), [`docs/decisions.md`](docs/decisions.md) and [`docs/decisions/`](docs/decisions/), [`docs/architecture-lessons.md`](docs/architecture-lessons.md) |

Record new decisions and deliberate deviations as a file of their own in
`docs/decisions/`, made with `python3 scripts/decisions.py new "Title"` when
the work starts, before the number goes into code or docs: it claims the
number on `origin`, so two agents never take the same one (D-198). Never
append to `docs/decisions.md`, which keeps D-001–D-197. Don't silently drift
from the design. Prefer dogfooding evidence over speculative
architecture (architecture-lessons §24–§25).

## Layout

| Path              | What                                                                 |
|-------------------|----------------------------------------------------------------------|
| `crates/core`     | Domain types: `Workspace`, `Session`, `Execution`, `HostStatus`, ids |
| `crates/protocol` | Wire protocol: requests, responses, events, attach frames            |
| `crates/client`   | `Connection` over `otterd dial` (local or `ssh host otterd dial`)      |
| `crates/daemon`   | `otterd` — host daemon (managers, tmux backend, attach bridge)        |
| `crates/cli`      | `otter` — control-plane CLI                                         |
| `apps/desktop`    | Desktop app (Tauri 2 + React); own cargo workspace, see its README    |
| `packages/claude-runtime` | The Claude worker otterd runs for the `sdk` backend (Node, the Claude Agent SDK); see its README |

Inside the daemon (`crates/daemon/src`):

- `daemon.rs` — shared state, request dispatch, host status
- `workspaces.rs` — workspace lifecycle + background preparation pipeline
- `sessions.rs` — sessions/executions, launching
- `reconcile.rs` — process exits + agent observation, every 500 ms
- `attention.rs` — what needs the developer (raise/resolve rules)
- `merged.rs` — a task's own archived workspace is deleted once its pull
  request is merged (D-189)
- `tasks.rs` — tasks (D-043): their own store under `state/tasks/`,
  idempotent commands, per-task history
- `controller.rs` + `brain.rs` — the Control Agent (D-045): the deterministic
  engine that steps tasks, and the model calls it makes (`OTTER_CONTROLLER`);
  `work_state.rs` reads the work's diff, status and fingerprint for
  verification (D-062)
- `turn.rs` + `gateway.rs` — Otter's tool-using turn (D-089; on by
  default, the host setting *Otter's tools* turns it off, D-096): the bounded turn that
  answers the developer, and the
  tools it may call — checked by what started the turn, applied once, written
  to the task's history; `agents/claude_otter.rs` runs Claude's turn
  through the worker's `otter` profile (no Claude Code tools, only Otter's);
  `chat.rs` is Otter's conversation on the host (D-093), with the host's
  tools, `otter chat` in the CLI; `memory.rs` is Otter's memory (D-113),
  each conversation's and the host's, given to every turn; `commands.rs`
  does the developer's slash commands in a task's conversation with the
  same tools, no model (D-190)
- `watches/` — watches (D-183): what Otter looks at again later on the
  developer's word. One engine (`mod.rs`: probing with no model, only
  changes written, firing before the turn, the turn in the task's step);
  each kind of thing watched behind `WatchKind` (`time.rs`, `ci.rs`).
  `triggers.rs` — recurring work that starts tasks (D-083)
- `levels.rs` — how far each of Otter's tools may go (D-191): the catalog,
  the host's and a task's levels, and `otter.actions` (what Otter did,
  D-193)
- `runs.rs` + `runtime/` — managed agent runs (D-044): the runtime contract,
  the approval `policy`, the conversation `actor` and registry (D-055/D-056),
  and the bridge from a run to its task; `agents/claude_stream.rs` is
  Claude Code's stream-json mode (`legacy_cli`), `agents/claude_sdk.rs` the
  Claude Agent SDK worker's protocol (`sdk`, D-057)
- `agents/` — agent providers behind `AgentProvider` (`detect`, `launch_argv`, `observe`);
  `codex.rs` holds everything Codex-specific, `claude.rs` everything Claude Code-specific,
  and `mod.rs` the shared quiet-turn heuristic (`settle`)
- `backend/` — execution backends; tmux is the only one and nothing else may know about tmux
- `slack/` — Otter in Slack (D-200): the developer's direct messages to the
  Slack app, over Socket Mode (a WebSocket otterd opens; nothing listens),
  said in Otter's conversation on the host, and its answers sent back;
  `needs.rs` tells Slack when a task needs the developer, with buttons, and
  a reply in its thread goes to that task (D-201)
- `git.rs` (shared repos + worktrees), `git_account.rs` (Otter's own git identity, key and
  GitHub sign-in, which its repositories include, D-133), `environment.rs` (direnv), `env.rs` (login env, exec shim)
- `attach.rs` (PTY bridge, or a stream whose client keeps the history:
  `backend/tmux/control.rs`, tmux's control mode, D-118), `server.rs`
  (connections), `dial.rs`, `store.rs` / `events.rs`

In the CLI, `dashboard.rs` is the grouped `otter ls` view.

## Build & test

```sh
cargo build
cargo test            # unit + end-to-end (real otterd + real tmux, isolated per test)
cargo clippy --all-targets
cargo fmt --all
```

End-to-end tests live in `crates/daemon/tests/e2e.rs`. Each test gets its own
`OTTER_HOME` under a short temp dir (Unix socket paths are length-limited) and
its own tmux server. Requires `tmux` ≥ 3.2 and `git`; the direnv test skips if
direnv is missing. Agent tests use a fake `codex` script that writes a rollout
transcript — they never run the real Codex. The tests that drive a real
Chrome (browser checks, D-046) skip unless `OTTER_TEST_BROWSER=1`; CI runs
them in a job of their own (D-137).

`crates/cli/tests/e2e.rs` drives the real `otter` binary (its own
`OTTER_CONFIG_DIR`) against such daemons, using the `otterd` built next to it.
Its SSH test runs over `ssh localhost` and skips when
`ssh -o BatchMode=yes localhost true` fails; `OTTER_E2E_REQUIRE_SSH=1` (set in
CI) turns that skip into a failure.

Manual dogfooding without touching `~/.otter` / `~/.config/otter`:

```sh
export OTTER_CONFIG_DIR=$PWD/.dev/ctl          # .dev/ is git-ignored
./target/debug/otter host add here --local --home $PWD/.dev/home
./target/debug/otter new scratch && ./target/debug/otter attach scratch
```

## Rules that are easy to break

- tmux, SSH, Git, Codex are implementation details: keep them out of
  `crates/core` and out of user-facing names/messages. Generic code never
  interprets `provider_session_id` / `provider_state`.
- Session identity ≠ process identity. Restarting creates a new `Execution`.
- Never put environment variables or secrets on a command line or in
  `events.jsonl`. Environments go through `otterd internal-exec`'s env file.
- Cleanup only touches managed resources (`~/.otter/workspaces/<id>`).
- Closing the client must never stop remote work.
