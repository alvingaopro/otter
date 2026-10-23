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
`docs/plans/otterhub-managed.md`. This decision is its first two
milestones: the hub otterd runs, for the hosts on its own machine, and
Settings › All hosts › Slack to set it up.

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
  - Its output goes to otterd's log. It is the Claude worker's pattern. It
    runs with otterd's own process environment (the CLI's that started it,
    or its unit's), not a login environment.
  - Its process is kept in `state/otterhub/hub.pid`. An otterd killed
    outright can't stop its hub, so the next one stops the hub left running
    before it starts its own: never two on the app.
  - otterd keeps the hub's list of hosts for it. It is a deviation from
    "otterd knows one host" (D-182): otterd still dials no one.
- **otterd starts with its machine, on the hub's host** (`boot.rs`), set up
  once per otterd, not on every start of the hub. On
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
    the last 90 s, since its process started. The file is removed before
    each start, so what a hub before it said never counts. A process that
    runs and says nothing is `starting`.
  - Slack's hello counts the app's open connections. With more than one,
    the hub says so: another hub, or a host on its own connection, hears
    part of what is said. Telling two hubs apart across machines is A2's
    claim.
- **The hub's own host is its own.** The tokens there are the hub's. Until
  that host connects, it reads `off`, but with tokens set, and D-206's
  `take_in` leaves a host like that alone. So otterd tells the hub its name
  (`--here`), and the hub takes it in with nothing to give. The e2e test
  found that. A host taken in is always switched to take its events from
  the hub unless it does already, the hub's own host too: one with members
  and a connection of its own (D-200) would otherwise keep that connection
  beside the hub's.
- **`otter hub setup --on <host> [--user U…]`** gives the host the hub's
  settings. It takes the tokens from its environment (`SLACK_BOT_TOKEN`,
  `SLACK_APP_TOKEN`, taken out before anything starts, as `serve` does), or
  uses those the host has. Members default to those the host lists. It
  then waits until the hub says it is connected to Slack, not merely
  running. A host it can't reach is named: it may run a hub of its own.
  - It serves the host and those its machine reaches as local. The others
    are named "not served yet" until A2 reaches other machines.
  - It is refused while the hub runs on another host: stop it there first,
    then paste the tokens again (moving, by the developer's choice).
  - **`otter hub stop`** turns it off where it runs, and **`otter hub
    status`** says where it runs, how, and how otterd starts there.
  - An otterd before this has no `hub` view; one that can run a hub always
    has it (`off` where never set), so setup can tell the two apart.
- **Settings › All hosts › Slack** in the app does what `setup` does.
  - Choose the host to run it on; only hosts whose otterd can are offered.
  - Paste the tokens and the member ID once; a token the host has already
    may be left empty.
  - It then shows where the hub runs, how (running, starting, or failed and
    why), what it says of Slack, how otterd starts with that machine, and a
    line per host: served, too old, not reached, or not served yet.
  - Stop turns it off there; moving it is Stop, then Set up elsewhere with
    the tokens again.
  - The host's own Slack page is now "Slack here". The app needs nothing
    new from Tauri: it is `settings.set` and `settings.get`.
- Compatible additions only: no protocol bump.

**Not done** (the plan's next steps): hosts on other machines (A2: the
confined key, host keys from the Mac, a claim that tells two hubs apart),
and saying which host a message is from.

**Revisit** if the hub needs to run where otterd can't, or a host's boot
start needs root and lingering is refused there.

Tests: e2e `otterd_runs_the_hub_where_it_is_set_up` (CLI, two real
otterds, the stand-in Slack):
- setup gives the hub, and otterd runs it; setup is still waiting while
  Slack hasn't said hello (watched failing with "running" taken for
  connected);
- both hosts are taken in, and a message goes to the host it names;
- the ledger is in `state/otterhub/`;
- setup elsewhere is refused;
- killed, the hub is started again (watched failing with otterd not
  starting it again);
- otterd itself killed, the next otterd stops the hub left running and runs
  its own, one only (watched failing with the hub left running);
- stopped, it is gone, and set up again it needs no tokens.

The app's tests cover setting up with the tokens and member a host has, and
asking for the tokens it doesn't have. The tokens are sent once and never
shown. Only hosts able to run the hub are offered (watched failing with
every host offered), and a hub that runs, or failed, is said with no other
offered.

Unit tests: only what this hub said lately counts, not what one before it
said; a host taken in is switched unless it is already; the settings as kept, the hub's `hosts.toml` (this host first,
local), the unit and the agent (`KillMode=process`, one per home), paths a
unit can't take, and the hub's own host taken in. Not tried on macOS (the
LaunchAgent) nor against Slack itself yet.

Reviewed by Fable before merging. The review found:
- the hub's own host kept its own connection when it had members;
- setup took "running" for connected;
- a restarted hub was shown running from what the one before it said;
- an otterd killed outright left its hub running beside the next one's;
- the boot unit set up on every start;
- an empty `$USER` in the linger hint.

All are fixed above.
