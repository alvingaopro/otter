# D-214 — A workspace that can't be prepared is said and cleaned up (2026-10-24)
<!-- claim: 526343c15fec1d4ed92575b8e723c6529d379a1c -->

The developer asked in a task's Slack thread "失败了咋不跟我说": why didn't
Otter tell them it failed. The task, "主机级 Otter 支持设 watch/定时任务和往任务对话发消息",
was given a workspace of its own, `otter-otter-watch`, and its clone failed
(`'alvingaopro/otter' does not appear to be a git repository`). The app
showed the workspace under NEEDS YOU, but nothing else said anything:

- The task waited, still "Working · planning", with the reason `Waiting for
  otter-otter-watch: preparing it failed (…); prepare it again to go on`.
  `Task::needs()` counted a wait only when it ended with `YOUR_AGENT`,
  `ARCHIVED` or `HELD` (D-134, D-156), so this wait wasn't a need. No need
  meant no message in Slack (D-201), no follow-up (`follow_key` covered only
  done, failed and blocked), and no word to the parent task's Otter (D-194).
- Nothing was said in the task's conversation either. A wait sets only the
  reason, so the task's Otter and its Slack thread had nothing to show.
- Otter on the host follows only the tasks it made (D-094). A task the
  developer made in the app, a trigger's task, or one begun with
  `/otter new` failed without Otter on the host ever hearing of it.

## Telling it

- **A wait on a workspace whose preparation failed is a need.** It ends with
  `PREP_FAILED` (`; prepare it again to go on`), which `needs()` counts beside
  `YOUR_AGENT`, `ARCHIVED` and `HELD`: a Blocked need, with the reason as its
  summary. The app's NEEDS YOU, Slack's need message and the follow-ups all
  come from that.
- **A wait counts from when it began.** A wait isn't a status of its own:
  the task stays planning or implementing. `Task.waiting_since` (optional)
  is written when the reason changes to a wait, and cleared when the wait
  ends. A need from such a wait counts from then, unless that is older than
  the status. The wait's follow key and Slack's need key are built from
  that time, so each wait is its own change. A failure that comes back
  after the workspace was prepared again is told again. Documents from
  before keep their keys: they have no `waiting_since`.
- **`follow_key` covers any task that needs the developer**, not only a
  blocked one. Done, failed and blocked keep the keys they had, so nothing
  followed up already comes up again.
- **The task's conversation says it once.** When the wait begins, a line of
  Otter's is posted, correlated `waits:<follow key>`: "The work can't go
  on: preparing its workspace X failed (why). Prepare it again, or give this
  task another workspace, and it goes on by itself." It is redacted, and
  never said twice for one wait. It doesn't answer the developer's messages
  (its correlation isn't a `msg_` id). Its Slack thread shows it where the
  developer last spoke to the task.
- **Otter on the host hears of failures in tasks it didn't make.** Once it
  begins to look (`OtterChat.failures_from`, written the first time; what
  had failed before isn't news), it follows up a task it didn't make when
  `Task::failure()` says so: the task failed, or waits on something only the
  developer can end. A decision isn't one: it is said where it is asked (the
  app's card, its message in Slack). A sub-task isn't one either: its
  parent's Otter hears it (D-194). From then on the task is followed with
  `Follow.failures_only`, once per change. That it is done isn't followed
  up. Tasks Otter made keep every follow-up, as before.
- **Slack says it once.** While Slack tells needs (`slack_notify`), a
  follow-up saying that a task waits or stopped isn't sent: the need's own
  message says it, with its buttons. This extends D-209's rule from tasks
  with a thread to every task. A follow-up saying a task is done is sent as
  before. With needs not told, follow-ups are sent as they were.

## Cleaning it up

The developer then asked why the failed workspace stayed in the list, with
the task shown under it as "Working · planning" after the task had moved
to `otter-otter-watch-2`.

- **A workspace made for a task whose preparation fails goes**
  (`failed_workspace_goes`). Nothing is in it. If the task still works
  there, it is told why first, once (a line correlated
  `prep-failed:<workspace>`). It is untied, and `workspace_gone` is set
  with no merge, as when the developer deletes it (D-189). An active task
  then waits on the developer: blocked, "Choose a workspace for this task:
  X couldn't be prepared (why)". A draft says so when it starts. The
  workspace is deleted through `delete_workspace`, never forced. The check
  that it is still failed and unused runs once the delete has begun, so
  it can't race a new preparation. A workspace with a session in use stays.
- **What couldn't be made isn't made again by itself.** Otter's look at
  the request for where the task works (D-186) is skipped once the task's
  workspace was taken away without a merge. Where it goes on is the
  developer's, as D-189 said for a workspace they deleted.
- **A workspace the developer made stays failed**, for them to prepare
  again (D-135). The task waiting on it is told (above).
- **A task that moves takes what isn't done with it.** `SetWorkspace`
  points every step not done or skipped at the new workspace. The app
  shows a task under every workspace one of its steps names (`tasksIn`), so
  the planning step kept it "at work" in the one it left. A failed
  workspace made for the task that it just left goes.
- **At start, what was left from before goes.** `resume_workspaces` gives
  each failed workspace made for a task to `failed_workspace_goes`. One
  whose task works elsewhere or is gone is deleted. One its task still
  points at is handled as a failure is now. This clears `otter-otter-watch`
  on the next update.
- **Otter deletes a workspace on the developer's word**: `delete_workspace`
  on the host, level `developer`, and never in a turn no message of theirs
  started, whatever the level (as `cancel_task`, D-207). It is applied once
  per message. It is refused while a task not done or cancelled works
  there, a run holds it or a session runs there, and when deleting it
  would lose uncommitted changes or commits pushed nowhere (D-108). It is
  never forced: the app's Delete is where the developer may.

Not done: a bare `owner/name` given as a repository is still cloned as a
path, which is what failed here. Saying it plainly and cleaning up was
asked for; what a repository name may be is its own decision.

## Revisit

- If the developer misses Otter's own words on a failure in Slack: today
  they get the need's message and its buttons, not Otter's reading of it.
- If following every failure on a busy host costs too many turns: each is
  one bounded turn of Otter's (`Limits::coordinating`).

## Tests

- `task::tests::a_workspace_only_the_developer_can_free_waits_on_them`
  (the failed wait is a need), and
  `task::tests::each_wait_on_the_developer_is_its_own_change`.
- e2e `workspace_prep::a_task_on_a_failed_workspace_says_why_and_waits`:
  the wait is a need and is said once in the task's conversation, however
  often the task is stepped.
- e2e `host_chat::otter_hears_when_a_task_it_didnt_make_fails`: a task made
  in the app gets no follow-up for its plan's question, one for the failed
  wait, and none once it is done.
- e2e `slack::a_failure_is_told_in_slack_once`: the need's message is
  posted, Otter's follow-up is in its conversation, and Slack gets one
  message about it.
- e2e `workspace_prep::a_tasks_own_workspace_that_cant_be_prepared_goes`:
  the workspace is deleted, the task is told once and waits on the
  developer, and nothing is made again or started.
- e2e `workspace_prep::a_moved_task_leaves_nothing_on_its_old_workspace`.
- e2e `workspace_prep::failed_workspaces_left_by_their_task_are_swept_at_start`:
  a failed workspace left by its task, and one its task still points at,
  go at start; the developer's own stays.
- e2e `host_chat::otter_deletes_a_workspace_on_the_developers_word`:
  refused in an event turn (also at `auto`), with work to lose, and where a
  task works; deleted once per message.
- e2e `chat_bootstrap::a_task_from_a_repository_that_is_not_there_waits_and_says_why`
  now expects the workspace gone and the task waiting on the developer.
