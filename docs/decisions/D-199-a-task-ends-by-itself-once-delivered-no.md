# D-199 — A task ends by itself once delivered: no acceptance (2026-10-22)

The developer asked Otter to resolve a pull request's conflicts; once the
branch had main merged in, the task waited for them to accept it. Accepting
held every task on the developer and added nothing they didn't already
decide by merging: say the result, and where more work is needed, do it.

- **Review is delivery under way**, never the developer's: publishing, CI,
  the gates, the report. There is no acceptance gate (an older task's
  *Your acceptance* is ignored), no `accept_task` tool, no *Accept* or
  *Accept anyway* in the app, no `/accept`, no `otter task accept`.
  `needs()` raises nothing for review (`NeedKind::Review` stays for what
  older otterds sent); the app shows *Delivering*, with what it waits on
  or which gate didn't pass.
- **It ends by itself once settled**: no gate pending, no decision pending,
  nothing going back to work. Every gate passed: done (*Done: every gate
  passed*), the report written and posted on the pull request, with its
  link; merging stays the developer's. A gate that didn't pass: failed,
  the reason saying which, with the pull request — *Stopped: not passed:
  Requirements met (not met: …). The pull request is open: …; merging it
  is yours. Tell me what to change, or retry.* The report is written
  either way. A retry verifies again and delivers again.
- **More work, it does.** CI failing on the code is a fix step, as before
  (D-068). A pull request that conflicts with its base (D-184) is a step,
  *Bring the branch up to date*: merge `origin/<base>`, resolve keeping
  what both sides meant, keep the checks passing; Otter publishes. Once
  per head that conflicts, within the attempts budget. CI failing the
  same way after a fix stops and says so (it waited for *Accept anyway*).
- **Unverified work is delivered as it is** (D-165): its pull request
  opens and CI runs, then it stops with the result, *Requirements met*
  not passed. Before, it waited in review.
- **"That's it" in review** does nothing there: it ends by itself.
  Stopping work in progress is as before; a done task asked for more plans
  again (D-050).
- Compatible: the protocol's `accept {override_gates?}` stays, for older
  clients; their *Accept* still works. An older otterd still waits for
  acceptance and this app has no *Accept*: ask Otter in the conversation
  (it has `accept_task` there).
- **What is lost:** marking a task done past a gate that didn't pass. It
  stops saying why; the developer merges or closes the pull request
  themselves, or tells Otter what to change. D-047's gates and D-193's
  automatic acceptance are amended.

Tests: `otter_publishes_on_its_own_and_the_task_ends_when_ci_passes`,
`a_conflicting_pull_request_goes_back_to_work_and_ends_done` (watched
failing with the step off), `the_same_ci_failure_after_a_fix_stops_with_the_result`,
`verification_stops_when_a_fix_changes_nothing` (delivered as it is, then
stopped with the report), `a_delivered_task_is_done_and_more_plans_again`,
`a_task_splits_into_sub_tasks_and_hears_from_them` (sub-tasks that stop
are heard as over), the CLI's
`a_task_goes_from_the_terminal_to_a_delivered_pull_request`, core's
`needs` and `what_next`, and the app's delivery tests. The e2e tests that
waited for review wait for review or done (`delivered`). Not checked in the
macOS app itself.
