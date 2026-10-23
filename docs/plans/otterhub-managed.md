# otterhub, set up from Settings (D-208)

The goal: choose in Settings which host runs otterhub and configure Slack
there once. Otter does the rest: it starts the hub, keeps it running, and
brings it back after the machine restarts.

Before this, otterhub (D-206) was set up by hand, as on code-server:
`hosts.toml`, a copy of the tokens, a systemd unit, the right `--home`.

## The fact the design turns on

otterd doesn't start with its machine. `otterd dial` starts the daemon when
it isn't running (`dial.rs` `spawn_daemon`), and `otter host install` sets up
nothing at boot. After a restart nothing runs on the host until a client
dials it: no hub, and no watch or trigger either, since those run in otterd.
A hub meant to be always there needs something on its host that comes back
by itself.

## Decided (2026-10-23, the developer's word)

1. **otterd runs the hub** on the host chosen for it. It starts `otter hub
   serve` as a child, starts it again when it stops, and stops it when the
   hub is turned off. It does this as it runs the Claude worker. otterd sets
   itself to start with the machine: a systemd user unit with lingering, or
   a LaunchAgent. Rejected: the app writing unit files over ssh. That needs
   two platforms' unit files, on/off and state over ssh, and otterd still
   wouldn't start at boot.
2. **A confined key** for the hub's ssh to other hosts:
   `restrict,command="<otterd> --home <home> dial"` in each host's
   `authorized_keys`, put there by the Mac over its own ssh. With it the
   key can only dial otterd. Watched working on code-server on 2026-10-23:
   the hello came back, and the command asked for wasn't run.
3. **Host keys come from the Mac.** It reads each host's public host keys
   over its own connection, already verified, and writes them into the hub's
   `known_hosts` under the address the hub uses. Any address then works,
   and one that reaches another machine fails.
4. **Moving the hub means pasting the tokens again.** Nothing keeps them but
   the hub's host, where they are write-only.

## Milestones

- **A1: the managed hub, for the hosts on its own machine** (D-208, done).
  - `settings.hub` and the view's `hub`.
  - otterd running `otter hub serve --here <name>` with the tokens in its
    environment, the hub's `hosts.toml` (this host local, with its home),
    `boot.rs`, and the hub's `status.json`.
  - `otter hub setup --on <host>`, `otter hub stop`, and `otter hub status`
    saying where it runs.
  - The hub warns when Slack's hello counts more than one connection to its
    app.
  - This replaces code-server's hand-made unit.
- **B: Settings › All hosts › Slack** (D-208, done). Choose the host, paste the tokens and
  the member ID once, see each host's state, Set up, Stop. These are thin
  calls over `settings.set` and `settings.get`; the Tauri side stays as it is.
- **A2: hosts on other machines.**
  - `hub.key` makes the key on the hub host (D-133's `key_create`).
  - The client resolves each host's address with `ssh -G`; a `ProxyJump`
    or `ProxyCommand` can't be moved there, so that host is said and
    skipped.
  - The client puts the confined key on each host and reads its host keys.
  - A `{transport: "ssh"}` entry in `hub.hosts`.
  - A claim with a heartbeat (`slack.claim`), so that a hub elsewhere on the
    same app is refused. Moving the hub stops the old one first, and
    `otter hub leave` clears the claim.
- **After:** say which host each message is from, once a second host joins.

## Moving the hub, and the hand-made one

- Moving: `otter hub stop` (or Stop in Settings), then set it up on the
  other host with the tokens again.
- code-server ran the hub by hand from D-206 on. Nothing told two hubs
  apart then. So the order there was: stop and disable the hand-made
  `otterhub.service`, then `otter hub setup --on code-server`, then remove
  the unit and `~/.config/otter/hub.env`.
