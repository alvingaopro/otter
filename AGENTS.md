# Otter — notes for coding agents

Otter is a control plane for persistent development work on remote machines:
**Workspaces → Sessions → Attention**, not hosts → ssh → tmux → panes.

## Read before you change

Always: [`docs/invariants.md`](docs/invariants.md) — the 12 rules every change
must keep, with the tests that guard them.

| If you are changing…                                   | Read first                                                                 |
|--------------------------------------------------------|----------------------------------------------------------------------------|
| workspace lifecycle, sources, Git, environments        | design §7–§18, §28 · decisions D-010–D-012 · invariants 1, 9, 10           |
| sessions, executions, tmux, attach                     | design §10–§12 · decisions D-007, D-009 · invariants 2, 3, 7               |
| an agent integration (Codex, Claude Code, or a new one) | `crates/daemon/src/agents/mod.rs` docs · decisions D-013, D-015, D-023 · invariants 5, 6 |
| attention, the `otter ls` view                       | design §21, §26 · decision D-014                                           |
| the desktop app                                        | `apps/desktop/README.md` · decision D-018 · invariants 11, 12               |
| the wire protocol, events or client                    | [`docs/protocol.md`](docs/protocol.md) · decisions D-004, D-016 · invariant 12 |
| transport, daemon lifecycle, persistence               | decisions D-003, D-005, D-008 · invariants 4, 8, 11, 12                    |
| the architecture itself                                | [`docs/design.md`](docs/design.md), [`docs/decisions.md`](docs/decisions.md), [`docs/architecture-lessons.md`](docs/architecture-lessons.md) |

Record new decisions and deliberate deviations in `docs/decisions.md`; don't
silently drift from the design. Prefer dogfooding evidence over speculative
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

Inside the daemon (`crates/daemon/src`):

- `daemon.rs` — shared state, request dispatch, host status
- `workspaces.rs` — workspace lifecycle + background preparation pipeline
- `sessions.rs` — sessions/executions, launching
- `reconcile.rs` — process exits + agent observation, every 500 ms
- `attention.rs` — what needs the developer (raise/resolve rules)
- `agents/` — agent providers behind `AgentProvider` (`detect`, `launch_argv`, `observe`);
  `codex.rs` holds everything Codex-specific
- `backend/` — execution backends; tmux is the only one and nothing else may know about tmux
- `git.rs` (shared repos + worktrees), `environment.rs` (direnv), `env.rs` (login env, exec shim)
- `attach.rs` (PTY bridge), `server.rs` (connections), `dial.rs`, `store.rs` / `events.rs`

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
transcript — they never run the real Codex.

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
