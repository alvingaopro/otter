# Workd — notes for coding agents

Workd is a control plane for persistent development work on remote machines:
**Workspaces → Sessions → Attention**, not hosts → ssh → tmux → panes.

- Architecture and intent: [`docs/design.md`](docs/design.md) — read §4, §11,
  §33 and §34 (the architectural rules) before changing anything structural.
- Decisions and deliberate deviations: [`docs/decisions.md`](docs/decisions.md).
  Record new ones there; don't silently drift from the design.

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
- `agents/` — agent providers (Codex: launch/resume argv, rollout discovery and parsing)
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
  `crates/core` and out of user-facing names/messages.
- Session identity ≠ process identity. Restarting creates a new `Execution`.
- Never put environment variables or secrets on a command line or in
  `events.jsonl`. Environments go through `workd internal-exec`'s env file.
- Cleanup only touches managed resources (`~/.workd/workspaces/<id>`).
- Closing the client must never stop remote work.
