# D-206 — One Slack app for several hosts: otterhub holds its socket (2026-10-23)
<!-- claim: 366e105d3d4e42d6a0fe17e8b364410d4edf0198 -->

The developer wants to work on several tasks at once from Slack, with
everything done there, across hosts ("我希望我在slack里可以同时处理多个task，所有功能都能在slack里完成"),
through one Slack app, not one per host. D-203 chose one app per host, and
D-200 chose Slack in otterd's settings over a client of its own. This
supersedes both choices at the developer's word, after a design and its
review on 2026-10-23 ("我认可你的判断，我们可以先实现一个轻量otterhub, otter agent还是跑在各个host上").
D-203 named two costs of a hub. Slack splits an app's events among its
open connections: here only the hub opens one. And D-203 rejected keys
from one host into the others, against "otterd knows one host" (D-182).
otterd still knows one host. The hub is a client like the CLI and the app,
with what they hold: `hosts.toml` and the ssh that reaches each host. That
cost is accepted, not avoided (see the end).

- **`otter hub serve` holds the app's one connection.** It runs as a
  service, often beside an otterd on an always-on machine, with
  `SLACK_BOT_TOKEN` and `SLACK_APP_TOKEN` in its environment (a systemd
  `EnvironmentFile`). It reads them before any thread starts, then takes
  them out of its environment, so nothing it starts inherits them (ssh,
  `otterd dial`, a daemon that dial starts). It is a subcommand of `otter`,
  not a third binary: a release ships `otter` and `otterd` (`release.yml`,
  `install.rs`).
- **The hub only passes events on; each host still posts its own
  messages.** For each event the hub acks it, then gives it to its host
  with `slack.deliver`. The host takes it as its own connection would. It
  runs the same code as before (`slack_heard`, `slack_pressed`,
  `slack_command`), and the same `command_id`s apply it once. Otter's
  messages, need cards, forms, the coding agent's words as written, 👀 and
  `/otter new`'s thread are all posted by the host, through Slack's Web
  API, as the app's one bot. Only events are split among connections,
  never Web API calls. So D-200 to D-203 and their tests hold as they are.
  And while the hub is down, a host still tells the developer what needs
  them.
  - Rejected: the hub renders everything from the hosts' events. That
    would move about 1100 lines of `slack/mod.rs` onto RPC and keep two
    copies of it, and the host's Settings › Slack (members, notify) would
    have to be made again in the hub. It needs no protocol change, which
    was its one advantage.
- **A host takes its events from the hub** (`slack.hub`, `SlackView.hub`,
  state `hub`). It opens no connection, and needs no app token: one already
  set isn't used. The Slack sync runs as before. At start, `otter hub serve`
  takes in every host it serves, or only those named with `--host`. For
  each, it sets hub mode and the bot token, plus the hub's members if the
  host lists none; a host's own members stay its own (the hub says when a
  member of its own isn't among them). It takes in a host only in three
  cases. One: neither Slack token is set there. A host that isn't connected
  says `off` whatever is missing, so `off` alone doesn't mean nothing is
  set. Two: it is connected on this app already; its own connection must
  stop, or it would hear part of what is said. Three: it takes its events
  from a hub on this app. A host on another app, or with tokens of an app
  the hub can't tell is this one, is left as it is, and the hub says why.
  An otterd before the hub is found by asking `slack.owns` first, so
  nothing is set on it. Every host is taken in before the hub connects to
  Slack. A host the hub couldn't reach is tried again every minute.
- **Where things go.**
  - A message at the top goes to the host it begins with (`gpu: …`,
    `@gpu …`; the host hears it without the name). Else to the host whose
    workspace it names (D-182's `route`). Else to the host the conversation
    was with last, then the default host, then the only one. If none fits,
    Otter asks the developer to begin with a name. `otter chat` has no
    conversation to remember, so it uses the default host or asks (D-182).
    In a Slack DM, every host's Otter shares one stream, so "好的，开始吧"
    belongs where the conversation is. A change of host is said ("→ **b** (workspace
    `trainer`)"), as `otter chat` says it on stderr.
  - A reply in a thread, or a press on a message, goes to the host whose
    message it is. That is either a message the hub passed on, as it
    recorded, or one a host says is its own (`slack.owns`). A host owns the
    messages that told a need or began a task's thread (`state/slack.json`),
    and those it posted since otterd started (in memory). A reply under a
    message no host knows goes where the conversation is.
  - `/otter status` and `help` go to every host, and their answers come
    back together, each under its host's name, within Slack's three seconds.
    `/otter new` is routed like a message at the top, by what the hosts
    said last (no time to ask them again).
  - One event at a time in each conversation, as a host's own connection
    took them: a message is placed, and given its first try, before the
    next one is placed, so a reply said at once goes where the message
    before it went. A reply in a thread whose host the hub knows goes to
    that host even while it can't be reached; the developer is told so.
    `/otter new` is given in a task of its own, never cut short by Slack's
    three seconds. A task the host may be making is not asked for again;
    after the deadline the answer says it is starting, and a failure later
    is said through the command's `response_url`.
  - The hub keeps only this: whose each message is, and each
    conversation's last host (`$OTTER_CONFIG_DIR/hub/state.json`, owner
    only). No task state: `(host, task)` is a task wherever it is seen.
- **Delivery.** A host that can't be reached is tried again for about
  half a minute (2, 6, 20 s), then the developer is told in Slack. Trying
  again is safe: the host applies what it was given before only once. A
  host that answers `conflict` (`otter hub leave` was run) isn't served
  again until the hub starts again. A crash after the ack and before the
  host has the event loses that event. That is said here, not built
  around.
- **`otter hub status`** shows each host's Slack: through the hub, on an
  app of its own, or off. **`otter hub leave <host>`** is the rollback: it
  sets `hub: false`, so the host needs an app token of its own again. The
  app's Settings › Slack shows "Through otterhub" and the hub's bot. It
  no longer warns about two hosts that both take the app's events from the
  hub. It still warns when a host on its own connection shares the app
  with them.
- **Cutover from one app per host:** start the hub on the always-on
  machine. A host already on the same app is taken over when the hub
  starts; one on an app of its own needs `otter hub leave` there first, or
  that app's tokens cleared. Run the hub alone on an app: a second hub on
  the same app splits its events again, and no host can tell. To go back,
  stop the hub and run `otter hub leave` on one host, then set its app
  token. A host-level conversation keeps its own Otter, memory and levels
  (D-093, D-113, D-191). One app fronts several Otters, and each host's
  messages read as the same bot's.

**Security, accepted:** the hub's machine reaches every host it serves
with ssh as the CLI does, and that ssh is a shell, not only `otterd dial`.
Slack's member list is now the gate to every host it serves, where before
it was one host's. A key confined by `authorized_keys` forced commands, or
a hub per group of hosts, is what to do when that matters.

**Not done:** each host's messages don't say which host they are from
(only a change of host is said). Events aren't journaled before they are
given. Nothing checks for a second hub on the same app. There is no
desktop view of the hub.

**Revisit** if talking to several Otters through one app turns out wrong
in use (one memory, one conversation): that is the evidence for a turn on
the hub, which this decision doesn't build (architecture-lessons §24–§25).

Tests: e2e `a_host_takes_slack_from_otterhub` (otterd: no connection of
its own and no app token, a message given twice said once and answered
in Slack, `/otter status` answered for the ack, what it posted its own
until a restart, `conflict` once it left).
`a_need_is_told_and_pressed_through_otterhub` (the host tells a need in
hub mode, owns its message, and a press given by the hub decides it).
`one_slack_app_serves_two_hosts_through_the_hub` (CLI, three real
otterds, a stand-in Slack). Covered:
- routing by workspace, by conversation and by name;
- two messages said at once, the second following the first (watched
  failing three times of three with no turns);
- a reply in a thread under the developer's message, recorded, also after
  the hub restarts (watched failing with the ledger not read);
- a reply under a host's `/otter new` thread, found by `slack.owns`
  (watched failing with the hosts not asked);
- `/otter status` from every host;
- no token in any host's environment, including a daemon the hub started
  (watched failing with the tokens left in);
- `leave`.

Unit tests cover the enrollment rule (a host on another app, or with
tokens but not connected, is left), the routing rules, names, the ledger
and the merged answer. The app's tests cover the Slack tab through
otterhub and the twin check (watched failing without it). Reviewed by
Fable before merging. The review found a host with another app's tokens
taken in, a ledger write race, a reply said at once routed before the
message before it, a cut-short call leaving its connection half used,
`/otter new` answered "busy" while its task was being made, and a known
thread's host passed over while it was down. All are fixed above. Not
tried against Slack itself or on the dev VM.
