# D-211 — A task's thread says while the coding agent works quietly (2026-10-23)
<!-- claim: 01d175eb9c44635972e5c7cbeaa6da4987f59375 -->

The developer, working on tasks from Slack: when the coding agent works on
something for a long time without a word, its task's thread should say the
agent is working, "这样我们至少知道code agent还在工作". In the app, the
conversation's working bubble shows the agent's activity lines and their
ages (D-141). In Slack, D-203 shows the agent's words as it writes them,
but a long command or a long think writes nothing. The thread then looks
the same whether the agent is busy or stuck.

What changes:

- **Shown at work after two minutes of quiet.** The task must have a run at
  work (starting or running), and nothing may be waiting on the developer:
  a need is told in its own message. The agent's words must be going to a
  thread of the developer's in Slack (D-200's rule, as D-203 and D-207
  route them). When it has said nothing for two minutes
  (`OTTER_SLACK_AGENT_QUIET_MS`), its thread says:
  `_The coding agent is working… nothing said for 3 minutes._` and, from its
  run's latest activity line, `Latest: $ cargo test`. The activity line is
  read from the live run, because a run's activity reaches the task only
  when it ends. It is redacted, as the app's is.
- **One message for the agent's turn, as before.** That line is part of the
  turn's message (D-203). If the agent has said nothing yet, the line is that
  message, posted in the thread, and its first words take its place. If it
  has spoken, the line goes under its words so far. When it speaks again,
  the line goes at once. When the turn's message lands, it replaces the
  whole message.
- **Kept current without notifying.** The line is shown again with
  `chat.update`, which doesn't notify, as the minutes or the latest activity
  change, at most every 10 s. The sync looks every 30 s (a quarter of the
  quiet time, at most 30 s) as well as on news, so a quiet agent is noticed
  without an event.
- **Taken away.** When the run waits on the developer (a need), or the
  agent's words go elsewhere, the line goes: the message keeps its words,
  or is deleted if the line was all it had. When the run ends, the same
  happens in `slack_turns_ended`, which also deletes a message that was only
  ever the line, instead of leaving it empty.

Only the first post of the line can notify (a new message in a thread the
developer follows), once per quiet stretch of a turn. Everything after that
is an update.

Not done: Otter's own long turns already show "Otter is writing…" (D-196,
D-202). The task's own message at the top keeps its status (D-207) and isn't
given the agent's activity. There is no app setting for the quiet time.

**Revisit** if the first post of the line notifies too often to be wanted,
or if two minutes is the wrong wait (a build often takes about that long).

Tests: e2e `slack::a_quiet_coding_agent_is_shown_at_work_in_its_thread`. A
form is answered in its need's thread, then the fake worker (mode `busy`)
runs `make check` for 2 s before it says anything. With a 300 ms quiet time,
the thread is shown the line with `Latest: `$ make check``, once. The agent's
words, then its turn's message "Done.", take that message's place, and
nothing is posted again. It was watched failing (timing out on the line)
with the sync not showing it, and failing on the missing `Latest:` while the
activity was read from the task rather than the live run. The unit test
`slack::tests::a_quiet_agent_is_shown_at_work` covers the wording. The D-203
test `the_coding_agents_words_are_shown_as_it_says_them` shares its setup
and is unchanged.
