# D-203 — One Slack app per host; the coding agent's words shown as they are written (2026-10-23)
<!-- claim: bcb548ecbac1b7e66d3b943351c2b9f8e9f8edfa -->

The developer asked for what D-200 and D-202 still left undone. Other IMs
stay out, as they said earlier ("其他IM现在不考虑"). For several hosts,
they chose one Slack app per host over a hub host whose otterd would reach
the others over SSH.

- **One Slack app per host.** Slack shares an app's events among every
  connection the app has open, so two hosts on one app would each hear part
  of what the developer writes. Each host gets its own app, a bot of its
  own in Slack, and its own direct messages; nothing reaches across hosts.
  - Settings › Slack copies this host's manifest — the one D-200 started
    with, named "Otter (<host>)", its bot "Otter-<host>" — to paste at
    api.slack.com/apps › Create New App › From a manifest. A manifest isn't
    copied out of a terminal any more, which is how the first one broke.
  - The view says which bot a host is connected as (`bot_id`, from
    `auth.test`). Where another connected host reports the same one in the
    same workspace, Settings says so: Slack is splitting messages between
    them. The other hosts are asked when the Slack tab is shown, and again
    only when the hosts connected change.
  - The hub is not built: it would make otterd a client of other otterds
    over SSH. That is a new role, with keys from one host to the others, and
    it goes against "otterd knows one host" (D-182).
- **The coding agent's words as it says them.** In a task's thread, while
  the developer last spoke there (D-200's rule), the coding agent's words
  are shown in one message per task. What it says goes in as it says it,
  at most every 1.5 s, and each piece is shown whole once said (at most
  every 0.4 s, so a chatty agent stays within Slack's limit). When its
  turn's message lands, it takes that message's place, as the app shows
  the agent's words and then keeps one message per turn (D-069). The next
  turn starts a message of its own. Words that come after the turn's
  message (within 30 s, a beginning of it) are dropped — so a new turn
  whose first words begin the last message waits until they differ. A turn
  that ends with no message of its own (the same words again) leaves its
  last piece shown, whole, and the next turn starts a message of its own,
  where the developer is then. A turn's message is kept while its run goes
  on, however long and however silent (a long build); a turn is taken to
  have ended only when no run is going, as read before any words since.
- Compatible, no version bump: `SlackView.bot_id` is optional.

**Not done:** a hub host for several (see above), other IMs.

Tests: e2e `the_coding_agents_words_are_shown_as_it_says_them` (with the
SDK worker's stand-in: its form answered from Slack after a reply in its
thread; "Do …" shown in the thread, then "Done." in its place, not posted
again; watched failing with the agent's message posted anew), the bot's
id in `a_direct_message_in_slack_is_answered_there`, and the app's
`slackManifest.test.ts` (names within Slack's limits, the hosts sharing an
app) and the Slack tab's test (another host on the same app is said, the
manifest copied, named after the host; watched failing with the hosts
never compared). Checked with the app's tests and a production build, not
in the macOS app, and not yet against Slack itself.

Reviewed against the code (Fable) before merging. The review found a long
turn's message split in two (its entry let go ten minutes after it began,
words or not), the other hosts asked again on every render of the app,
a turn that ends with no message keeping its message for the next one (and
where it went), and pieces said whole shown with no interval at all; its
second look, a new turn's words ended as the old turn's when they came
while the sync was sending, and a silent run's message let go. All are
fixed above All
are fixed above; the app's test now re-renders and checks the other hosts
were asked once.
