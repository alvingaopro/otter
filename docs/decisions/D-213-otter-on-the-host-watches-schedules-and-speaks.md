# D-213 — Otter on the host watches, schedules and speaks into a task (2026-10-24)
<!-- claim: 4356d27001d2271a393f4d0b12638c211a2db9f2 -->

The developer asked Otter, in their direct messages with the Slack app, for
two things it couldn't do there (2026-10-24):

- set a watch or a schedule: keep an eye on a pull request's CI (any, not
  only a task's own), and do something or say how things stand at fixed
  times;
- say something in a task's conversation, guidance for its coding agent,
  without going to that task's thread in Slack.

Otter on the host (D-093) had only the tools to look across tasks and to
make, start, pause, stop and delete them. Watches (D-183) lived only in a
task's conversation. Triggers (D-083) had no tool at all: only the CLI and
the app's Automations view (D-185) made, paused and deleted them. The
host's instructions told Otter to send the developer to the task's thread
for guidance.

## Speaking in a task: `send_to_task`

- **The developer's own words, never Otter's.** `send_to_task {task_id,
  message_ids}` takes only this turn's own messages (`chosen`, as
  `create_task` and `forward_to_agent` do), word for word. A turn no message
  of theirs started has none, so it is refused there. Otter's own view goes
  in its reply on the host. A task has `tell_agent` for Otter's own words,
  but that is a different voice.
- **Through `task.send`, as any message of theirs.** Each message is
  written once in the task's conversation, under
  `<host message>:send_to_task:<task>`. Sending it again, or a turn run
  again, adds nothing. From then on it is the developer's message there:
  the task's own Otter answers it with the task's tools. The message goes
  to the coding agent after its current turn, or with its next one (D-059).
  A paused or done task goes on if they said so. Nothing new decides what
  reaches the coding agent.
- **Pictures stay on the host.** Only the words are said in the task. The
  answer counts the pictures not sent (`pictures_not_sent`), so Otter can
  say so.
- **In Slack, in the task's thread.** When the host's message came from
  Slack and the task has a thread there (its own message's, else its latest
  need's), the words are shown in that thread as "_You, to Otter:_ …". They
  are counted as said there (`Post.said`), so what the task says next goes
  to that thread, as the request of a task made from Slack does (D-207).
  They aren't shown at the top.

## Schedules: triggers from the host

- `list_triggers` reads them: name, workspace, schedule, paused, next time,
  request, and the last three occurrences.
- `create_trigger {name, message_ids, title?, workspace, every_minutes |
  daily_at}`: a trigger whose request is the developer's message(s), word
  for word. A task's request is the same. Each occurrence is a task of its
  own, with no publishing granted (`Policy::default()`). An occurrence
  whose last task is still at work is skipped (D-083). It is made once per
  message and name.
- `pause_trigger` is the safe direction and may run in any turn.
  `create_trigger`, `resume_trigger` and `delete_trigger` run only in a
  turn the developer's message started, whatever their level, as
  `cancel_task` does (D-207). Recurring work is never started by Otter on
  its own. Their default level is `developer`.
- Recurring work stays a trigger: a task each time (D-083). Recurring
  *looks*, saying how something stands every so many minutes without
  starting work, are watches (below).

## Watches on the host

This is the watches plan's slice 2 (`docs/plans/watches.md`), in this form.

- **One engine, two homes.** A watch lives in the conversation that asked
  for it (`watches::Home`): a task's, as before (D-183), or Otter's on the
  host, in `OtterChat.watches` (optional, so no protocol bump). Probing, a
  reading kept only when it changes, firing before the turn, and reporting
  in the same write that marks the watch reported are the same code for
  both (`on_watches` writes to the task's document or the host's
  conversation). `WatchKind::target` takes the task when there is one. On
  the host `ci` needs the pull request's url and watches any repository's.
  `run` and `time` never needed a task.
- **Its turn is the host's.** A firing watch on the host is said where the
  host's follow-ups are (D-094): in `run_controller`, behind the
  `host_turn` flag, after the developer's unanswered messages and before a
  follow-up. It is a turn no message started (`Origin::Event`, keyed
  `watch:<id>:<n>`), with the host's tools and a prompt of its own
  (`Turn.event` without a task). As in a task, it never stays firing: no
  answer, an error or a panic is said as a plain line. With the controller
  off, a plain line is said.
- **A watch's own turn sets no other watch**, on the host as in a task.
- **Recurring looks: a deviation from D-183's "it fires once".** A `time`
  watch with `every_minutes` (at least 1) and `until` (`for_minutes`, 60
  unless said, at most 7 days) is reported each time it comes due. In the
  same write it waits again for its next time (`Watch::again`), until
  `until`. Each time is its own firing (`findings`, and so its key
  `watch:<id>:<n>`), a turn of its own, and a message of its own. Only
  `time` comes back: a `ci` or `run` watch still ends when it comes to
  something. A look that comes back is set only in a turn the developer's
  message started, whatever the level, in a task as on the host. Being
  stopped while it is being said is refused (`not_ready`), as for any
  firing watch, so a report is never dropped half-said.
- **Bounds.** At most 5 open on the host (`MAX_HOST_WAITING`), 3 per task as
  before. Ended watches are pruned to the last 20.
- **Tools on the host:** `watch` (`ci`/`run` by url, `time` with
  `in_minutes` or `every_minutes`/`for_minutes`), `list_watches` (the
  host's, and every task's open watches), `cancel_watch` and `delete_watch`
  (`watch_id`, and `task_id` for a task's, done as the app's Stop and
  Delete through `task.act`). All are in the levels catalog: `watch` and
  `cancel_watch` as before, `list_watches` and `delete_watch` new, all
  `auto`.

## Tests

- e2e `host_chat::otter_schedules_recurring_work_on_the_developers_word`:
  refused in an event turn even at `auto`; made once per message with
  their words as its request; listed; paused in an event turn, with resume
  and delete refused there; resumed and deleted on their word.
- e2e `host_chat::otter_says_the_developers_words_in_a_task`: refused in
  an event turn; said once as theirs (`<msg>:send_to_task:<task>`); passed
  to the coding agent at work by the task's Otter ("From the developer:");
  a paused task resumed on their word.
- unit `slack::tests::words_said_for_the_developer_go_to_the_tasks_thread`,
  and e2e `slack::words_said_for_a_task_show_in_its_thread`: the words in
  the task's thread, its answer there, nothing at the top.
- unit `levels::tests::everything_is_automatic_but_memory_decisions_reviews_and_endings`
  pins the new tools' levels; `gateway::tests::each_tool_has_one_class_and_a_closed_schema`
  their classes.
- unit `watch::tests::a_recurring_look_waits_again_until_its_end`.
- e2e `watches::a_host_ci_watch_on_any_pull_request_says_once_what_ci_came_to`:
  a pull request of another repository watched from the host by its url;
  otterd stops right before the report, and after the restart it is said
  once, on the stored finding.
- e2e `watches::a_recurring_look_on_the_host_comes_back_until_it_is_stopped`:
  refused in a turn no message started; reported every minute, each its
  own (`watch:<id>:1`, `:2`, `:3`); its own turn's `watch` refused; listed;
  once stopped, it comes back no more.
