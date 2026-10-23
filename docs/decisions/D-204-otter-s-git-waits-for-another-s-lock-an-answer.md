# D-204 — Otter's git waits for another's lock; an answer settles what was asked before it (2026-10-23)
<!-- claim: 5d7ab885b1c8165f71f163be4fdb00e5ddb2104a -->

Two end-to-end tests failed now and then when the suite ran under load, and
each was a real race in otterd, not in the test.

- **A commit failed when another git held the index.**
  `delivery_resumes_at_each_checkpoint_without_repeating_itself` failed 3 of
  11 full runs: delivery's `git add` found `index.lock` there and gave up, and
  the task stopped ("Otter stopped: committing the work: … index.lock': File
  exists"). Git gives up at once when another git holds a worktree's index:
  here one still running from before otterd restarted; outside tests, the
  developer's editor reading `git status` in the tree, which takes the lock
  to refresh the index. Now:
  - otterd's git never takes that optional lock: `GIT_OPTIONAL_LOCKS=0` on
    every git it runs (`delivery::run`, which `work_state` and `changes` use
    too, and `GitManager::git`), so its reading never stands in the way of
    a commit, its own or anyone's;
  - delivery's git (`delivery::Git::output`) waits when one of git's locks
    is held (the index, `HEAD`, a branch: the quoted `….lock'` path, which
    git says in every language) and tries again, about 8 s in all (100 ms
    doubling to 1.6 s). A git that found the lock did nothing, so trying
    again is safe.
- **An answer settled what came after it.**
  `quiet_agent_mid_turn_is_flagged_and_answering_clears_it` failed now and
  then: the developer typed an answer, the agent finished its turn at once,
  the reconcile raised "finished" (a Review), and then the answer's own
  "settled" resolved every item of the session, that one too. A finished
  turn went unsaid. `session.write` now notes the time before it sends, while
  nothing else can raise, and settles only items raised by then
  (`attention::answered`); what the agent said after is news, and stays.
  Known: a still prompt Otter only guessed at, raised again with what the
  agent really asks after the answer, outlives it for a pass or two, until
  the agent is seen at work.
- **The merged sweep asks for its setting again before it acts.** CI found
  `a_tasks_workspace_goes_once_its_pull_request_is_merged` deleting a
  workspace just after the setting was turned off: a pass checked the
  setting when it began, then asked GitHub, slowly, and acted on its old
  answer. `delete_merged` now reads the setting first (D-189).
- **The harness says why.** A test's otterd now logs to its
  `logs/workd.log`, as one started by `dial` does, and a `wait_task` that
  times out, an `eventually_on`, or a connection that fails prints the end
  of it. (`a_hosts_features_become_its_tasks` failed once that way here:
  `otterd dial exited (status 0)`, the daemon closing the connection at
  once, with a home to migrate; not looked into further.)
  `a_crash_after_a_forward_still_delivers_it_once` and
  `a_merged_tasks_workspace_stays_while_something_is_only_there` failed now
  and then before, while other sessions' suites ran alongside. Here, in 11
  full runs, 2 more with two suites at once, and 12 runs of the second
  alone under load, the first never failed and the second once (two suites
  at once, "timed out waiting for: kept"), when nothing was logged. Their
  cause is not found; the next failure will show otterd's side.

Tests: `a_commit_waits_for_another_git_to_let_go_of_the_index` (a held
index: git alone fails; Otter's add waits and goes through; fails without
the wait), `an_answer_settles_what_was_asked_before_it_not_after` (fails
when an answer resolves the session's every item, as before),
`a_test_daemon_keeps_its_log`. Full suite: 11 runs before the fixes had 4
failures among these two tests.
