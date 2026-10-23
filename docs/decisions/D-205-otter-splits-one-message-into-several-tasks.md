# D-205 — Otter splits one message into several tasks, approves a pull request and answers a task's decision from the host conversation (2026-10-23)
<!-- claim: aaa5724d5d724df3206083b56515fcfff422b6e3 -->

Dogfooding Otter in Slack (D-200) on 2026-10-23. The developer sent one
direct message: "approve就够了， 另外这几个PR也帮忙review一下。可以分别创建多个任务来review。"
with five pull request links. Otter called `create_task` five times, once
per pull request, and got the first task back each time. It made one task,
titled for the first pull request, with all five links in its request.
That task's coding agent also approved a pull request it was only meant to
review, because "approve就够了" (about another pull request) was part of
the request it was given. Earlier the same morning, "approve 这个PR" became a
coding-agent task, because Otter's Bash only reads (D-193). That task's
request held no link, so the agent guessed the repository wrong and asked
which one it was. The developer answered in the direct message rather than
under the question's own Slack message. Otter on the host had no way to
pass the answer on, so it made yet another task. The developer asked for
all three fixes ("都按你的建议，两个都做").

- **One message, several tasks.** The host's `create_task` made each task
  under `<message>:create_task`, so every call in a turn was one command:
  the second to fifth calls were repeats of the first (`tasks.rs`'s
  commands), and their answer said `created` anyway. Each task is now made
  under that command plus a digest of what tells it apart: its title, the
  messages it takes, its `scope`, and where it works, as sub-tasks already
  are (D-194). A repeat in the same turn answers `already` with the task it
  made. The cost is D-194's: a turn run again that picks a different title
  makes a second task.
- **Each task's part, in Otter's words.** A task's request stays the
  developer's message, as written (D-093). The host's `create_task` takes
  `scope`: that task's part of the request, in Otter's words. It is
  written into the task before the task starts, as a message of Otter's to
  the coding agent (`tell:<command>`, D-193). The agent's first prompt
  lists it under "From Otter, your tech lead (not the developer)", apart
  from the developer's words. HOST_INSTRUCTIONS say to call `create_task`
  once per part, each with the same messages, its own title and its own
  scope. Anything asked of every part ("only review, don't approve") goes
  in each scope.
- **Otter's review of the work sees Otter's instructions.** `brain::brief`
  is what plan review, verification and Otter's turns are told. It had
  only the request and the developer's messages, so a plan for one pull
  request out of five would fail review's first check ("nothing asked for
  is missing"). It now lists Otter's instructions to the coding agent under
  a heading of their own, saying they are not the developer's words and may
  narrow the request to a part. That holds for every `tell_agent` too.
- **`review_pull_request {url, event, body?}`**, in the host's
  conversation. `event` is `approve`, `comment` or `request_changes`; a
  comment and a request for changes need a `body`, which goes in on stdin
  (`gh pr review <n> --approve|--comment|--request-changes [--body-file -]`).
  It uses Otter's GitHub sign-in when it has one (D-133), as `read_github`
  does, else the host's gh. The answer says which account it went out as
  (`as`). It is a tool of its own, so Otter's Bash still only reads (D-193).
  Its level is `developer` by default: an approval goes out under the
  developer's account and can't be taken back, and the coding agent's
  unasked approval of #8463 is the evidence of overreach that
  [bold permissions] waits for. Settings can still make it `auto`.
  It is done once per message: kept under `<message>:review_pull_request:<digest>`
  in the host conversation's commands, written only once GitHub took it.
  A refusal ("Can not approve your own pull request") is said with
  GitHub's words, and asking again tries again. A crash between GitHub
  taking the review and the record can review twice, which is safer than a
  refusal that blocks the retry. Like publishing, it has no undo.
- **`answer_decision {task_id, decision_id, approve?, answer?}`**, in the
  host's conversation. It answers a decision a task waits on, as the
  developer's message says. This is done through `task.act decide`, as the
  app's card and Slack's buttons do (D-201), recorded as the developer's
  (`Decider::User`), once per message and decision. A question takes the
  developer's answer, or `approve: false`. Anything else takes
  `approve: true|false` and no answer.
  - **A deliberate deviation from D-089 §3.2 rule 2**, kept narrow. That
    rule let a message settle only the plan. Here a message also settles a
    question to the developer and an approval that isn't developer-only,
    when it plainly answers it. Still only by their own press: a decision
    marked `user_only` (other than the plan), a recovery, merging, deploying,
    and a form of several questions (D-202). For those, Otter says where to
    press: the app, or the decision's own message in Slack. "Yes, delete
    it" in chat still resolves nothing that only a press may.
  - Only in a turn a message of the developer's started, at any level. A
    turn no message started is refused `needs_developer` even at `auto`,
    since the record says the developer decided.
  - Host conversation only. In a task's conversation, D-201's buttons and
    thread replies already answer what the task asks.
- HOST_INSTRUCTIONS no longer send decisions to the task's conversation.
  They say when to use the two tools: only what the developer plainly said,
  ask if it isn't clear which decision they mean, and a review that found
  nothing wrong is not an approval.
- No protocol change: tools and their levels are otterd's. Settings lists
  the two new tools from the catalog (groups `task` and `deliver`).

**Revisit** if an answer lands on a decision the developer didn't mean,
which would mean narrowing `answer_decision` to a decision named in their
words or a task with one decision waiting. Revisit too if an approval goes
out that they didn't ask for (back to `off`), or if a turn run again makes
duplicate tasks often enough to matter.

**Not done:** the host's `create_task` with `scope` from a task's own
conversation (sub-tasks have their own request, D-194); `retry_task` in
the host conversation (Slack's Retry button covers it); a review's
comments on lines of the diff.

Tests: e2e `host_chat::one_message_makes_a_task_for_each_part_it_asks_for`
covers two tasks from one message, a repeat answered `already`, each
request the developer's words, and each agent's first prompt carrying only
its own part, as Otter's. It was watched failing with the old command id:
`created, already, already`.
`chat_bootstrap::several_tasks_from_one_message_each_make_a_workspace_from_one_repository`
covers three workspaces made from one repository at once.
`host_chat::otter_reviews_a_pull_request_on_the_developers_word` covers an
approval made once per message, a comment's body on stdin, refusal from a
turn no message started, and GitHub's refusal said and tried again.
`host_chat::otter_answers_a_tasks_decision_as_the_developer_said` covers a
plan approved once as the developer's, refusal from an event turn at both
`developer` and `auto`, and a decision already made.
`host_chat::a_command_only_the_developer_may_allow_takes_their_press` was
watched failing with the floor removed: Otter approved `rm`. Unit tests:
`controller::a_tasks_part_of_the_request_is_said_as_otters` (the planning
prompt and `brief`), the gateway's classes, and the levels' defaults.
Checked against the stand-ins (the scripted brain and the fake gh), not
yet with real Claude or against GitHub.

[bold permissions]: the developer's 2026-10-22 direction, "otter的限制太多了，我们可以bold一些，等发现权限过大我们在去限制"
