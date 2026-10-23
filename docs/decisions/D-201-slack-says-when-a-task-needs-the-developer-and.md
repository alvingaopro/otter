# D-201 — Slack says when a task needs the developer, and takes their answer there (2026-10-23)
<!-- claim: a4a9d120b963a96ece208a404f2cc249db1f4355 -->

D-200 put Otter's conversation on the host in Slack: the developer could
talk to Otter there, but a task waiting on them still said so only in the
app. Away from the Mac, an approval waited until they came back. The
developer chose, from the follow-ups D-200 listed, to be told when a task
needs them, to answer from Slack, and to talk to that task there.

- **Told when a task needs the developer.** A need is what `Task::needs`
  says (D-071): a decision, a question, a task that stopped, one waiting on
  them. Slack is told once per need, in a direct message of its own (a new
  message notifies; a reply in a thread may not), to each listed member,
  with the need's key (`kind:decision` or `kind:since`, D-073's "new when
  it differs from the last one seen"). What waits when Slack first connects
  on the host is the baseline, taken as the connection opens, and isn't
  told. A need that began while Slack was away is told when it comes back,
  however long ago: it still waits. Unlike the app, a task first seen with
  a need is told: otterd sees every task from its start. A setting, on by
  default: Settings › Slack › *Tell me when a task needs me*
  (`slack.notify`), read as it is used, so switching it needs no new
  connection. Off, needs are still noted, so turning it on doesn't tell what
  waited meanwhile.
- **The app's decision card, in Slack** (`slack/needs.rs`): a question's
  options as buttons (ten at most); a question with none, "Reply in this
  thread to answer"; any other decision, *Approve* and *Deny*; a task that
  stopped, *Retry*; the whole command a decision is about (D-157) below
  its summary. No *Accept*: D-199 takes review out of what needs the
  developer. A Review need from before it says to accept in the app.
- **A press is done as the app does it**, as `task.act` (`decide`,
  `retry`) with `command_id` `slack:act:<action_ts>`: a press Slack sends
  again is one command. Only a listed member's press counts — anyone who
  sees the message can press it. Nothing writes the outcome but the task:
  when its need changes, its message's buttons go and a line says what came
  of it ("✅ Approved by you.", "✅ Answered by you: …", "No longer waiting:
  the task is done."), whether it was decided in Slack, in the app or by
  Otter. A refusal is said in the thread.
- **A need's thread is its task's.** A reply under a need's message is said
  in that task's conversation (`task.send`), not Otter's on the host; under
  the message of a question with no options, while that question is open,
  it is the answer (`decide`, approve, its words), as the app's answer box
  is — under another need's message it is said in the task, even if a
  question is open then. Otter's and the coding agent's messages (D-086)
  after it come back into the thread, by D-200's rule: while the developer
  last spoke from Slack. A press or an answer is a decision, not a message,
  so what follows it comes back once the developer writes in the thread;
  the next need is told anyway. Every need's message maps to its task, so a
  reply under an older one still reaches it — up to the newest 200; a reply
  under one older than that goes to Otter's conversation on the host.
- **Tasks named by their titles** in what Slack is sent: `[ft_…]` becomes
  “title”.
- **Kept in `state/slack.json`:** each task's need last seen, the need
  messages (the newest 200; what each said, so an update keeps it), and
  each threaded task's last message looked at. A task's thread starts where
  its conversation was when its first need was told; a need's message is
  kept as soon as it is posted, and one already posted for that need and
  member isn't posted again. Changes are looked at 300 ms after they
  happen, so a run's bursts are looked at once, and a task's conversation
  is read only when it has something new.
- Compatible, no version bump: `SlackView.notify` and `SlackUpdate.notify`
  are optional. The Slack app needs nothing new: its manifest already had
  interactivity, `im:write` and `chat:write`.

**Not done:** a form of several questions in one decision (a reply would
answer it with one string; the app has the form; since D-202), buttons on a
review from an otterd before D-199, a need told to a channel rather than
direct messages, threads for tasks that never needed the developer (since
D-202 for one made with `/otter new`), a `/remind`-style snooze, pictures
(since D-202).

Tests: `slack::needs::tests` (a decision's buttons and command, a
question's options or its reply, Retry, what came of a need and who
decided, titles for ids), `slack::socket::tests` (an interactive envelope,
a press), `slack::tests` (the coding agent's words back to the thread, a
reply's task), and e2e `slack::a_task_that_needs_you_is_told_in_slack_and_answered_there`
(what waited before isn't told; a new need is, once, with Approve and
Deny; a reply in its thread is said in the task and answered in the
thread; a press from someone not listed does nothing; the developer's
approves once, however often pressed, and the message loses its buttons
and says so; with telling off a new need isn't told, nor once it is on
again). Watched failing: with presses from anyone taken, Approve from a
member not listed approves; with replies not routed by their thread, the
reply goes to Otter's conversation on the host. Writing it found that
taking the starting point at the first look, after the 300 ms wait, lost
the answer to a message sent in those 300 ms; it is taken as the
connection opens. App: the switch, sent alone and saved. Checked with the
app's tests and a production build, not in the macOS app, and not yet
against Slack itself.

Reviewed against the code (Fable) before merging. The review found a reply
under an older need's message taken as the answer to a question opened
since, a need's message lost from the record (and posted again, its
buttons never taken off) when telling the next member failed, every
threaded task's conversation copied under the store's lock on each change,
an answer done in the read loop (a restart could cut it short before it
reached the coding agent), a need's age measured from a status that may be
old, deleted tasks kept in the record, and what is told not redacted
again. All are fixed above (`a_reply_answers_only_the_question_it_is_under`
for the first). It also found that what follows a press or an answer
doesn't come back by itself; that is D-200's rule, said above.
