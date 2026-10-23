# D-208 — otterd runs otterhub on the host chosen for it (2026-10-23)
<!-- claim: cd8c801332836e53bf17d6938e7839f196967243 -->

D-206 put one Slack app's connection in `otter hub serve`, but the hub was
set up by hand. On code-server that meant `hosts.toml` with the right
`--home`, the tokens copied into an env file, and a systemd unit. The
developer asked for it to be chosen in Settings and configured once
("我们在setting里选择otterhub安装在哪个主机，并且在otterhub里配置slack"). They
settled the four choices the design left open: otterd runs the hub, a
confined key for other machines, host keys pushed from the Mac, and the
tokens pasted again when the hub moves. The plan is
`docs/plans/otterhub-managed.md`. This decision is its first milestone: the
hub otterd runs, for the hosts on its own machine.

- **otterd runs the hub.** `settings.set {hub: {enabled, name, users,
  default_host, hosts}}` on a host has its otterd run `otter hub serve`.
  That is the `otter` beside otterd, as `otter hub serve --here <name>`.
  - It runs with the `hosts.toml` otterd writes in `state/otterhub/`: this
    host first, local, with otterd's own path and home, so the `--home`
    can't be wrong. The others follow, and that file is written through
    `otter_client::config`, as the CLI writes its own.
  - The hub gets this host's `SLACK_BOT_TOKEN` and `SLACK_APP_TOKEN` in its
    environment, never on a command line, and takes them out before it
    starts anything (D-206).
  - otterd starts it again when it stops, waiting longer each time it stops
    soon (up to a minute). It starts it again at once when its settings or
    the tokens change, and stops it with `enabled: false`, or when otterd
    stops.
  - Its output goes to otterd's log. It is the Claude worker's pattern.
  - otterd keeps the hub's list of hosts for it. It is a deviation from
    "otterd knows one host" (D-182): otterd still dials no one.
- **otterd starts with its machine, on the hub's host** (`boot.rs`). On
  Linux that is a systemd user unit `otterd[-<home>].service`, enabled and
  not started, since this otterd runs already, with lingering. On macOS it
  is a LaunchAgent, loaded at the next login.
  - Only otterd is stopped with it (`KillMode=process`,
    `AbandonProcessGroup`): the sessions' tmux server outlives otterd, as
    always.
  - A second otterd started this way while one runs finds the lock taken
    and exits 0, so `Restart=on-failure` doesn't loop. That was read in
    `serve`'s `acquire_lock`.
  - Settings says how it is (`boot`): set, set without lingering (with the
    `sudo loginctl enable-linger` to run), or why not.
  - `OTTER_BOOT_SERVICE=off` (tests) leaves the machine as it is.
- **The hub says how it is** in `state/otterhub/hub/status.json`: Slack's
  connection and each host's line, as they change and every 30 s.
  - The view's `hub.state` is `running` only while the hub has said so in
    the last 90 s. A process that runs and says nothing is `starting`.
  - Slack's hello counts the app's open connections. With more than one,
    the hub says so: another hub, or a host on its own connection, hears
    part of what is said. Telling two hubs apart across machines is A2's
    claim.
- **The hub's own host is its own.** The tokens there are the hub's. Until
  that host connects, it reads `off`, but with tokens set, and D-206's
  `take_in` leaves a host like that alone. So otterd tells the hub its name
  (`--here`), and the hub takes it in with nothing to give. The e2e test
  found that.
- **`otter hub setup --on <host> [--user U…]`** gives the host the hub's
  settings. It takes the tokens from its environment (`SLACK_BOT_TOKEN`,
  `SLACK_APP_TOKEN`, taken out before anything starts, as `serve` does), or
  uses those the host has. Members default to those the host lists. It
  then waits to see the hub connected.
  - It serves the host and those its machine reaches as local. The others
    are named "not served yet" until A2 reaches other machines.
  - It is refused while the hub runs on another host: stop it there first,
    then paste the tokens again (moving, by the developer's choice).
  - **`otter hub stop`** turns it off where it runs, and **`otter hub
    status`** says where it runs, how, and how otterd starts there.
  - An otterd before this has no `hub` view; one that can run a hub always
    has it (`off` where never set), so setup can tell the two apart.
- Compatible additions only: no protocol bump.

**Not done** (the plan's next steps): Settings in the app (B), hosts on
other machines (A2: the confined key, host keys from the Mac, a claim that
tells two hubs apart), and saying which host a message is from.

**Revisit** if the hub needs to run where otterd can't, or a host's boot
start needs root and lingering is refused there.

Tests: e2e `otterd_runs_the_hub_where_it_is_set_up` (CLI, two real
otterds, the stand-in Slack):
- setup gives the hub, and otterd runs it;
- both hosts are taken in, and a message goes to the host it names;
- the ledger is in `state/otterhub/`;
- setup elsewhere is refused;
- killed, the hub is started again (watched failing with otterd not
  starting it again);
- stopped, it is gone, and set up again it needs no tokens.

Unit tests: the settings as kept, the hub's `hosts.toml` (this host first,
local), the unit and the agent (`KillMode=process`, one per home), paths a
unit can't take, and the hub's own host taken in. Not tried on macOS (the
LaunchAgent) nor against Slack itself yet.
