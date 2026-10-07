# Workd

Persistent, attention-first workspaces for development work on remote machines.

Run several coding agents, shells, dev servers and test runs across hosts, and
manage them as **workspaces** — closer to browser tabs plus an inbox than to
SSH + tmux. Remote work keeps running when your laptop sleeps.

- `workd` runs on each host (started on demand over SSH, no open ports).
- `workctl` is the control-plane CLI (a desktop app will follow).

See [`docs/design.md`](docs/design.md) for the architecture and
[`docs/decisions.md`](docs/decisions.md) for implementation decisions.

## Quick start

On each host (needs tmux ≥ 3.2):

```sh
cargo install --path crates/daemon --root ~/.local     # → ~/.local/bin/workd
```

On your machine:

```sh
cargo install --path crates/cli                       # → workctl
workctl host add dev-01                               # anything `ssh dev-01` reaches
workctl new scratch                                   # Codex + a shell
workctl new billing --repo git@github.com:org/api.git # worktree + .envrc/Nix env
workctl ls                                            # NEEDS YOU / WORKING / …
workctl attach billing                                # Ctrl-] detaches
```

`workctl ls --watch` keeps the overview live.

## Commands

```
workctl host add|list|rm|default|status|shutdown
workctl new <name> [--host H] [--goal G] [--no-agent] [--no-shell] [--prompt P]
                   [--repo URL [--branch B] [--base REV] | --dir PATH] [--no-wait]
workctl ls [--watch] [--table]
workctl ack <ws[/session]>                           # mark attention as handled
workctl workspace show|prepare <ws>
workctl workspace delete <ws> [--yes] [--force]      # --force: discard uncommitted work
workctl start <ws> [--name N] [--kind terminal|service|task] [-- command]
workctl start <ws> --agent [codex] [--prompt P]
workctl session stop|restart|delete <ws/session>    # restart resumes an agent's conversation
workctl attach <[host:]ws[/session]>
workctl logs <ws/session> [-n LINES]
workctl send <ws/session> <text> [--no-enter]
workctl events [-n N] [-f]
```

## Status

Implemented: design phases 1–7 (daemon over SSH, workspaces, tmux-backed
sessions, Codex sessions with resume and state detection, Git worktrees on
shared repositories, direnv/Nix environments, events and attention), all
through `workctl`. Next: CLI dogfooding (phase 8), then the Tauri desktop app
(phase 9). Open decisions are listed at the end of
[`docs/decisions.md`](docs/decisions.md) (D-013).

Add `--json` to any command for machine-readable output.
