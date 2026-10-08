<p align="center"><img src="docs/assets/otter.png" width="128" alt="Otter"></p>

<h1 align="center">Otter</h1>

Persistent, attention-first workspaces for development work on remote machines.

Run several coding agents, shells, dev servers and test runs across hosts, and
manage them as **workspaces** — closer to browser tabs plus an inbox than to
SSH + tmux. Remote work keeps running when your laptop sleeps.

- `otterd` runs on each host (started on demand over SSH, no open ports).
- `otter` is the control-plane CLI; `apps/desktop` is the desktop app over
  the same client library.

See [`docs/design.md`](docs/design.md) for the architecture and
[`docs/decisions.md`](docs/decisions.md) for implementation decisions.

## Install

Each merge to `main` publishes a [release](https://github.com/alvingaopro/otter/releases)
with the macOS desktop app and `otterd` + `otter` for Linux (x86_64,
aarch64) and macOS. macOS downloads come universal (any Mac) or per
architecture (`arm64` for Apple silicon, `x86_64` for Intel; half the size). Hosts and clients should run
the same version (see [`docs/protocol.md`](docs/protocol.md)).

- **Desktop app:** open the `.dmg` and drag Otter to Applications. It is
  ad-hoc signed but not notarized by Apple yet, so the first launch says
  Apple couldn't verify it: open **System Settings → Privacy & Security** and
  click **Open Anyway** (or run
  `xattr -dr com.apple.quarantine /Applications/Otter.app` once).
  Then **Add a host**: give it a name and an SSH destination (anything
  `ssh <destination>` reaches) or pick this Mac. If the host has no `otterd`
  (or an older one), the app offers to install the matching release into
  `~/.local/bin` there — the host needs tmux 3.2+ and curl or wget.
- **Command line:** in the app, *Otter → Install Command Line Tools…* puts
  `otter` and `otterd` in `~/.local/bin`. Without the app, download the
  release tarball for your platform and copy both binaries onto your `PATH`.
  `otter host install <host>` installs or updates `otterd` on a host.

## Quick start (from source)

On each host (needs tmux ≥ 3.2):

```sh
cargo install --path crates/daemon --root ~/.local     # → ~/.local/bin/otterd
```

On your machine:

```sh
cargo install --path crates/cli                       # → otter
otter host add dev-01                               # anything `ssh dev-01` reaches
otter new scratch                                   # Codex + a shell
otter new billing --repo git@github.com:org/api.git # worktree + .envrc/Nix env
otter ls                                            # NEEDS YOU / WORKING / …
otter attach billing                                # Ctrl-] detaches
```

`otter ls --watch` keeps the overview live.

## Commands

```
otter host add|list|rm|default|status|shutdown
otter new <name> [--host H] [--goal G] [--no-agent] [--no-shell] [--prompt P]
                   [--repo URL [--branch B] [--base REV] | --dir PATH] [--no-wait]
otter ls [--watch] [--table]
otter ack <ws[/session]>                           # mark attention as handled
otter workspace show|prepare <ws>
otter workspace delete <ws> [--yes] [--force]      # --force: discard uncommitted work
otter start <ws> [--name N] [--kind terminal|service|task] [-- command]
otter start <ws> --agent [codex] [--prompt P]
otter session stop|restart|delete <ws/session>    # restart resumes an agent's conversation
otter attach <[host:]ws[/session]>
otter logs <ws/session> [-n LINES]
otter send <ws/session> <text> [--no-enter]
otter events [-n N] [-f]
```

## Status

Implemented: design phases 1–7 (daemon over SSH, workspaces, tmux-backed
sessions, Codex sessions with resume and state detection, Git worktrees on
shared repositories, direnv/Nix environments, events and attention), all
through `otter`, event cursors and attach hardening (D-016, D-017), and
desktop M1, the attention dashboard (D-018). Open decisions are listed at the end of
[`docs/decisions.md`](docs/decisions.md) (D-013).

Add `--json` to any command for machine-readable output.
