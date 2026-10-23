# D-207 — Each task begun from Slack has a thread of its own; Stop and Delete there (2026-10-23)
<!-- claim: 010f065cdca9fceedd57a761048f27778c23e18b -->

The developer wants to work on several tasks at once from Slack, all of it
there ("我希望我在slack里可以同时处理多个task，所有功能都能在slack里完成"). On 2026-10-23
they handed Otter work in Slack all day, and an audit of that day (0.7.93,
the dev VM's `~/.workd`) found what kept sending them to the app. D-205 and
D-206 closed some of it: splitting a message, answering a task's decision
from the host conversation, one app for several hosts. This closes the
rest the developer agreed to, in one change ("按这个范围开PR"):

- Tasks Otter on the host made from a Slack message had no thread. Their
  request is made under `<the host message>:create_task…`, which isn't a
  Slack origin, so nothing such a task said reached Slack. The only signal
  was Otter's follow-ups on the host, all of them in the top level of the
  direct messages, and a reply there went to whichever host's Otter the
  conversation was with (D-206).
- After a button press, nothing came back. Where a task's messages go
  depends on where the developer's last message came from, and a press
  is not a message.
- A blocked task was told twice: once by Otter's follow-up, once by the
  need's message with its buttons. The developer answered the first one.
- Nothing stops or deletes a task from Slack. The question that day had
  two options and no way to say "not needed any more".
- Otter's prompts never mention Slack, so it kept saying "open the task".

What changes:

- **A task's own message.** A task Otter on the host makes from the
  developer's message in Slack gets a message of its own at the top of
  that conversation, just like `/otter new`'s (D-202). The sync loop makes
  it, because only the sync loop writes `state/slack.json`. It finds the
  developer's message from the request's command: the host message by id,
  and that message's `slack:` origin. The message is kept as the task's
  `started` post, so a reply in its thread goes to the task, and
  `slack.owns` routes it through otterhub. Its request counts as said in
  that thread (`Post.said`), so what the task says after it — Otter's
  messages, the coding agent's words as they are written — goes there. The
  message says how the task stands and is shown again when that changes
  (`Post.shows`, `needs::card_key`). It has Stop while the task isn't over,
  and Delete once nothing is at work in it. `/otter new`'s message becomes
  the same. A deleted task's message says it went.
  - Only a task that isn't over, made less than six hours ago (`STALE`,
    D-200's limit for Otter's messages), that Slack has no message of yet.
    An otterd before this one made tasks it never told: one still at work
    gets its message once this one runs, and one that is over doesn't.
  - Each task's need is still told in a message of its own at the top:
    that is what notifies, and its buttons are there.
- **A press counts as the developer speaking.** The last press on each
  task's messages is kept (`State.pressed`, folded in by the sync from
  memory, as `/otter new`'s threads are). What the task says after it
  goes to the thread of the message pressed on, until the developer next
  speaks somewhere else. Of their last message and their last press, the
  later one decides. A press is recorded before what it does, so a message
  it causes always finds it.
- **Otter's follow-ups on such a task.** One that says the task is done
  goes into the task's thread, also sent to the conversation
  (`reply_broadcast`), so the end is seen at the top. One that says it
  waits or stopped isn't sent to Slack: the need's message says that, with
  its buttons. A follow-up still goes only where D-200 sends Otter's
  messages, which is while the developer last spoke from Slack.
- **Stop and Delete in Slack.** A need's message has Stop task. For a task
  that stopped (Retry), it has Delete task instead, once nothing is at
  work in it. A form has Stop next to Submit. Each asks again before it is
  done (Slack's `confirm`). Stop is `task.act cancel`, as the app's. Delete
  is `task.delete`, refused while something is at work in the task. The
  rule is `tasks::deletable`, the same one `task.delete` checks. Delete
  pressed twice finds the task gone, and that is what was asked. A stopped
  task's need now says "No longer waiting: the task was stopped." rather
  than that its run ended.
- **Otter's `cancel_task` and `delete_task`**, a deliberate deviation from
  brain-runtime-v2's exclusions ("`cancel_task` … stays a button";
  `delete_task`). On Slack the developer asks in words ("停掉", "把这几个都删了"),
  and they asked for Otter to be able to do it (bold permissions,
  2026-10-22). The deviation is kept narrow:
  - `cancel_task` is in the host's conversation (a task it names) and in a
    task's (that task). `delete_task` is in the host's only: a task's own
    conversation would delete itself mid-turn.
  - Both are `developer` by default. Whatever the level, only a turn the
    developer's message started may call them (`ending_floor`), as
    `answer_decision` (D-205): cancelled and deleted don't come back.
  - `delete_task` refuses a task with something at work in it and says to
    cancel it first. Deleted already, it answers `already`.
  - The gateway test that listed them as never Otter's now checks that
    they are the developer's word.
- **Otter knows the developer is on Slack.** When their latest message on
  the host came from Slack, a host turn (an answer or a follow-up) is told
  so, and told what Slack offers: a task's own message and its thread,
  Stop, Delete, and needs with their buttons. A task's turn is told the
  same while its developer talks to it from Slack (`on_slack`: their last
  word, as routed above). The host's instructions say where guidance goes
  ("on Slack, the thread of the task's own message there; else the task in
  the app"), and both conversations' instructions say when to stop a task
  for good.
- **Which host said it, through otterhub.** D-206 left out which host a
  message is from. `settings.set {slack: {name}}` is the name otterhub
  knows the host by, and the hub sets it as it takes the host in. While
  the hub serves it, what the host posts at the top of the conversation
  has "from *name*" under it: answers, need messages, task messages, and
  those shown again. A reply in a thread isn't labelled, because its first
  message says whose it is. An optional field: older daemons ignore it,
  older hubs don't set it.

**Not tried against real Slack:** `reply_broadcast` in direct messages,
`confirm` on buttons, and whether a reply in a thread the bot began
notifies the developer (Slack notifies for threads one follows; the task's
message is the bot's). The stand-in Slack takes anything.

**Not done:** Otter's conversation on a host still runs one turn at a
time, so follow-ups wait behind the developer's messages, and they behind
it. A task's own thread takes most of the traffic off that conversation.
There is no `/otter` command to follow a task made elsewhere (the app, a
trigger). Its need's message is its thread until then. Otter on the host
can't pass words on to a task that is at work, which its thread covers.
The coding agent still sees only the project's settings, not the
developer's own skills (`setting_sources`). That is a decision of its own.

**Revisit** if the developer misses a task's end in its thread (Slack
didn't notify): then the end goes at the top as well. Revisit if
`cancel_task` or `delete_task` ever ends a task the developer didn't name,
or if the top level of the direct messages is still too crowded with
several tasks at once.

Tests: e2e `slack::a_task_made_from_slack_has_a_thread_of_its_own` covers
a task made from a Slack message with its own message (Stop), what it says
in its thread, a reply there going to the task, its message shown done
with Delete, Otter's done follow-up in the thread and at the top, Delete
pressed (and pressed again), and its message saying it went. It was
watched failing with the request not counted as said in the thread.
`slack::a_press_is_answered_where_it_was_pressed_and_stop_ends_a_task`
covers a task made in the app whose plan is approved in Slack and what
follows under that need, and Stop task pressed on another, said in its
thread, its need no longer waiting. It was watched failing with presses
not counted.
`slack::a_task_made_from_slack_before_gets_its_message_only_while_it_works`
is the upgrade rule. `slack::a_host_takes_slack_from_otterhub` and the
CLI's `one_slack_app_serves_two_hosts_through_the_hub` cover the host's
name: set, shown in the view, and under what it says at the top.
`host_chat::otter_stops_and_deletes_a_task_on_the_developers_word` covers
refusals from a turn no message started, at both levels, a task at work
not deleted, then cancelled, deleted, and deleted again (`already`).
`host_chat::otter_stops_its_task_on_the_developers_word` covers the same
in a task's conversation. Unit tests: where a task made from Slack speaks
and where a press sends it (`destination`), the host message a task came
from (`made_from`), and the task's own message and the need messages'
buttons (`needs`).
