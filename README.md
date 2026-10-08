# Workd

Persistent, attention-first workspaces for development work on remote machines.

Run several coding agents, shells, dev servers and test runs across hosts, and
manage them as **workspaces** — closer to browser tabs plus an inbox than to
SSH + tmux. Remote work keeps running when your laptop sleeps.

- `workd` runs on each host (started on demand over SSH, no open ports).
- `workctl` is the control-plane CLI; `apps/desktop` is the desktop app over
  the same client library.

See [`docs/design.md`](docs/design.md) for the architecture and
[`docs/decisions.md`](docs/decisions.md) for implementation decisions.

## Install

Each merge to `main` publishes a [release](https://github.com/alvingaopro/workd/releases)
with the macOS desktop app (`.dmg`, universal) and `workd` + `workctl` for
Linux (x86_64, aarch64) and macOS (universal). Hosts and clients should run
the same version (see [`docs/protocol.md`](docs/protocol.md)).

- **Desktop app:** open the `.dmg`. It isn't signed yet: the first time,
  right-click → Open, or `xattr -dr com.apple.quarantine /Applications/Workd.app`.
  Then **Add a host**: give it a name and an SSH destination (anything
  `ssh <destination>` reaches) or pick this Mac. If the host has no `workd`
  (or an older one), the app offers to install the matching release into
  `~/.local/bin` there — the host needs tmux 3.2+ and curl or wget.
- **Command line:** in the app, *Workd → Install Command Line Tools…* puts
  `workctl` and `workd` in `~/.local/bin`. Without the app, download the
  release tarball for your platform and copy both binaries onto your `PATH`.
  `workctl host install <host>` installs or updates `workd` on a host.

## Quick start (from source)

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
through `workctl`, event cursors and attach hardening (D-016, D-017), and
desktop M1, the attention dashboard (D-018). Open decisions are listed at the end of
[`docs/decisions.md`](docs/decisions.md) (D-013).

Add `--json` to any command for machine-readable output.
