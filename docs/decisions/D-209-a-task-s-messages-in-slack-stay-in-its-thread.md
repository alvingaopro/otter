# D-209 — A task's messages in Slack stay in its thread (2026-10-23)
<!-- claim: 29ed8ed17d80f32ab3cba6a6bf1e22eab860e16b -->

D-207 gave each task begun from Slack a message of its own, with the task's
conversation in that message's thread. One message broke that rule on
purpose: Otter's follow-up saying the task was done went into the thread
*and* to the top of the direct messages (`reply_broadcast`). D-207 wasn't
sure Slack would notify for a reply in a thread the bot started, so it
showed the end at the top as well.

The developer tried it live on 0.7.97 (2026-10-23, the dev VM, through
otterhub) with two tasks. Three "replied to a thread" messages then sat at
the top among their own: two tasks done, and one again after a revision.
They asked why some messages were in the thread and some came straight to
them: "如果是某个任务的消息，就应该一直在thread里回复我". This answers D-207's
*Revisit* clause the other way. The developer saw the end at the top and
didn't want it there.

The same live run showed two more things that made a task's thread read
badly:

- **A follow-up spoke of other tasks.** Task a's done follow-up, in a's
  thread, ended "另一个任务 b 还在 planning". Task b's ended "这样两个任务都做完了". A
  follow-up's turn sees every task on the host, and nothing told it to keep
  to the one that changed.
- **Titles said twice.** Otter wrote `“在 scratch 工作区里创建文件 a” [ft_arj3135qj8]`. Slack
  shows each `[ft_…]` as the task's title, which gave
  `“在 scratch 工作区里创建文件 a” “在 scratch 工作区里创建文件 a”`. The follow-up's own
  fallback (`[ft_…] title is now done`) had the same fault.

What changes:

- **Otter's done follow-up on a task with a thread of its own goes to that
  thread only.** Nothing has `reply_broadcast` any more, and `post_in` is
  gone. A follow-up that says the task waits or stopped still isn't sent
  (D-207): the need's own message says so, with its buttons. The top of the
  conversation still has the task's own message, which shows ✅ Done, and
  each need's message.
- **A follow-up speaks of its task only.** The host's follow-up prompt says
  so: "each other task's change has a follow-up of its own". That is true in
  the app's conversation too, so it isn't specific to Slack.
- **A task is named once.** `needs::titled` leaves an id out when its
  title is right beside it, before or after, set off by “”, "", ** or * or
  「」, or bare. Elsewhere an id still becomes “title”.

**Revisit** if the developer misses a task's end: the task's own message
changes to ✅ Done through `chat.update`, which doesn't notify. Whether a
reply in the task's thread notifies them is still unconfirmed (D-207). If
it doesn't, the end needs a notice that stays out of the conversation's
top, for example one ephemeral line, rather than the broadcast back.

Tests: e2e `slack::a_task_made_from_slack_has_a_thread_of_its_own` now
checks that Otter's done follow-up is in the task's thread without
`reply_broadcast`, and that no follow-up is posted at the top. It was
watched failing on the broadcast. The unit test
`turn::tests::a_follow_up_keeps_to_its_task` pins the follow-up prompt, and
`slack::needs::tests::a_title_beside_its_id_is_not_said_twice` uses the live
run's sentence. Both were watched failing first.

Reviewed against the code (Fable) before merging. The review found nothing
blocking. Its edge cases in `titled` are fixed: a bare title counts only as a
word of its own (the title "a" isn't the end of "schema"), and only spaces
are taken out between an id and its title, never a line break.
