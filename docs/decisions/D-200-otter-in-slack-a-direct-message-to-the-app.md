# D-200 — Otter in Slack: a direct message to the app, answered there (2026-10-23)
<!-- claim: 1da76c031249f747bc1d25cfc17bdea1007cc60f -->

The developer asked how Otter's chat could be in Slack, then for it to be
set up in Otter's settings and simply answer messages. They aren't their
Slack workspace's admin; the workspace doesn't ask for app approval, so they
made and installed the app themselves from a manifest (Socket Mode; bot
scopes `chat:write`, `im:write`, `im:history`, `groups:history`,
`reactions:write`, `files:read`, `commands`).

- **Set up in Settings › Slack, on one host.** The app's bot token
  (`SLACK_BOT_TOKEN`, `xoxb-…`) and app-level token (`SLACK_APP_TOKEN`,
  `xapp-…`, `connections:write`) are secrets like a provider's key (D-048,
  D-142): write-only, in `state/secrets.json` (0600), never logged, in an
  event or on a command line; each is refused without its prefix, since the
  two are easy to swap. Whose messages are the developer's is a setting:
  Slack member IDs (`U…`). The view says what is still missing, or how the
  connection is: connecting, connected (the Slack workspace and the bot) or
  failed and why. otterd tries again, waiting longer each time up to two
  minutes (starting over after a connection that worked), and at once when
  the settings change — the view then says connecting, not the last
  failure; a failure is a problem `otter doctor` shows. No frame from
  Slack for 150 s is a dead connection, whatever else happens on the host. One host per app: Slack shares an app's events
  among its open connections, so two hosts would each hear some.
- **In otterd, not a bridge beside it.** The first design was a client of
  its own, next to the CLI and the app, reaching every host; the developer
  chose Otter's settings. D-003 still holds: Socket Mode is a WebSocket
  otterd opens, outbound as its calls to model providers, Git and `gh` are,
  and nothing listens. The Web API (`auth.test`, `apps.connections.open`,
  `chat.postMessage`, `reactions.add`) goes through curl with the token in
  its config on stdin (D-095). The WebSocket is `tokio-tungstenite` on
  rustls with ring and the bundled roots (no cmake, no system store):
  otterd's dependencies go from 135 crates to 175. Its trace logs the
  address it connects to, which carries Slack's ticket, so otterd keeps it
  at info unless `OTTER_LOG` names it.
- **A direct message is said in Otter's conversation on the host** (D-093,
  the one `otter chat` has): only a message written to the app in a direct
  message (no edit, no bot's — Otter's own among them) by a listed member;
  others are acked and dropped, logged without their words. Its
  `command_id` is `slack:<channel>:<ts>[:<thread>]`: Slack sends an event
  again until it is acked, and the same command adds nothing. It is also
  the message's `correlation_id`, so where a message came from is in the
  conversation and needs no store of its own. Slack's markup becomes what
  was written (a link as its address, `&amp;` as `&`). Once taken it gets
  👀; one Otter couldn't take is answered with why. Both go out apart
  from reading the socket, so neither holds up the next ack.
- **Otter's messages go back while the developer last spoke from Slack**:
  each of Otter's messages after the developer's latest, when that one came
  from Slack, goes where it came from (its thread too) — answers, and
  Otter's follow-ups (D-094). Said from the terminal, the answer stays
  there. Sent as Slack's `markdown` block (no conversion), cut at 11 000
  characters, redacted again. `state/slack.json` keeps the last message
  looked at, so a restart neither repeats nor drops one; the first time,
  what is there already isn't sent, and after a long break nothing older
  than six hours is; if the message it kept is gone from the conversation
  (it started over), sending picks up from its end. A message Slack
  refuses is logged and not tried again; one not delivered (Slack not
  reached, rate-limited, or Slack's own trouble, a 5xx) is, 30 s later. A
  post cut off by a restart, or otterd stopping between a post and its
  note, is sent once more.
- **Authority:** a listed member's direct message is the developer's word,
  as a line in `otter chat` is (D-089). Signing in to Slack as the developer
  becomes as good as their SSH key for what Otter does on a message.
- **Compatible, no version bump:** optional fields (`Settings.slack`,
  `SettingsUpdate.slack`) and two secret names. The app shows the tab only
  where the host has `slack`. Its API keys tab lists neither token, and its
  count leaves them out — and Claude Code's token, which the tab already
  didn't list. `xapp-` joins the redactor's token shapes (D-075).

**Not done:** a task's conversation as a thread, a notification when a task
needs the developer, buttons for decisions (all three since D-201), `/otter`, pictures (the words
sent with one are taken, the picture isn't), the answer streamed as it is
written, several hosts from one app, other IMs.

Tests: `slack::socket::tests` (envelopes, what is taken, where an answer
goes, the markup, cutting), `slack::tests` (member IDs; what Settings says
is missing; the tokens refused swapped and never in the view), `redact`
(`xapp-`), e2e `slack::a_direct_message_in_slack_is_answered_there`
(against a stand-in Slack — its Web API over HTTP, Socket Mode over a
WebSocket: heard once, seen, answered in the direct message and in a
thread; a redelivery, another member and a bot not taken; an answer to the
terminal not sent; the tokens in no file but the secrets') and
`slack::slack_says_what_it_needs_and_why_it_failed`, and the app's
`sets Slack up with its two tokens…`. Watched failing: with Otter's
messages sent after any message from Slack rather than the latest, the
terminal's answer goes to Slack; with a bot's message taken, Otter answers
it too. Checked with the app's tests and a production build, not in the
macOS app, and not yet against Slack itself.

Reviewed against the code (Fable) before merging. The review found a dead
connection never noticed on a busy host (every event on the host restarted
the 150 s wait), Settings showing the old failure after the token was
fixed, sending stopped for good once the conversation started over, the
seen mark and a refusal holding up the socket, the waiting never starting
over, the ticket in a trace, the app's form left unsaved for `@U…`, and a
5xx dropping a message; all are fixed above, with
`otters_messages_go_where_the_developer_last_spoke`,
`sending_picks_up_after_the_last_message_looked_at`,
`slacks_own_trouble_is_tried_again_and_a_refusal_is_not` and the token
fixed in `slack_says_what_it_needs_and_why_it_failed` (watched failing
with the restart not saying connecting).
