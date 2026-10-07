# Workd — notes for coding agents

Workd is a control plane for persistent development work on remote machines:
**Workspaces → Sessions → Attention**, not hosts → ssh → tmux → panes.

## Read before you change

Always: [`docs/invariants.md`](docs/invariants.md) — the 12 rules every change
must keep, with the tests that guard them.

| If you are changing…                                   | Read first                                                                 |
|--------------------------------------------------------|----------------------------------------------------------------------------|
| workspace lifecycle, sources, Git, environments        | design §7–§18, §28 · decisions D-010–D-012 · invariants 1, 9, 10           |
| sessions, executions, tmux, attach                     | design §10–§12 · decisions D-007, D-009 · invariants 2, 3, 7               |
| an agent integration (Codex, or adding a provider)     | `crates/daemon/src/agents/mod.rs` docs · decisions D-013, D-015 · invariants 5, 6 |
| attention, the `workctl ls` view                       | design §21, §26 · decision D-014                                           |
| the wire protocol or client                            | `crates/protocol/src/lib.rs` docs · decision D-004                         |
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
| `crates/client`   | `Connection` over `workd dial` (local or `ssh host workd dial`)      |
| `crates/daemon`   | `workd` — host daemon (managers, tmux backend, attach bridge)        |
| `crates/cli`      | `workctl` — control-plane CLI                                         |

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

In the CLI, `dashboard.rs` is the grouped `workctl ls` view.

## Build & test

```sh
cargo build
cargo test            # unit + end-to-end (real workd + real tmux, isolated per test)
cargo clippy --all-targets
cargo fmt --all
```

End-to-end tests live in `crates/daemon/tests/e2e.rs`. Each test gets its own
`WORKD_HOME` under a short temp dir (Unix socket paths are length-limited) and
its own tmux server. Requires `tmux` ≥ 3.2 and `git`; the direnv test skips if
direnv is missing. Agent tests use a fake `codex` script that writes a rollout
transcript — they never run the real Codex.

Manual dogfooding without touching `~/.workd` / `~/.config/workd`:

```sh
export WORKCTL_CONFIG_DIR=$PWD/.dev/ctl          # .dev/ is git-ignored
./target/debug/workctl host add here --local --home $PWD/.dev/home
./target/debug/workctl new scratch && ./target/debug/workctl attach scratch
```

## Rules that are easy to break

- tmux, SSH, Git, Codex are implementation details: keep them out of
  `crates/core` and out of user-facing names/messages. Generic code never
  interprets `provider_session_id` / `provider_state`.
- Session identity ≠ process identity. Restarting creates a new `Execution`.
- Never put environment variables or secrets on a command line or in
  `events.jsonl`. Environments go through `workd internal-exec`'s env file.
- Cleanup only touches managed resources (`~/.workd/workspaces/<id>`).
- Closing the client must never stop remote work.
