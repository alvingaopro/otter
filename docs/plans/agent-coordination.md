# Otter coordinates the work itself — implementation plan

**Design:** the developer's doc "Otter 自主协调资源 · 设计" (Claude Docs,
2026-10-22). **Answers:** the doc's recommended answers, taken by the
developer, plus two changes they made: every milestone in one pull request
(M4 too, though the doc recommended waiting for dogfooding), and
`approve_plan` / `accept_task` automatic by default. **Principle (the
developer's):** be bold — let Otter act, and restrict once real use shows it
reaching too far. Everything Otter does on its own is recorded where the
developer can see it and turn it down a level.

## 0. What started it

Task "Eyes On CICD" (two GitHub Actions run links, a read-only question)
waited behind the developer's own agent in a directory workspace (D-079),
couldn't be given a workspace of its own (D-080: Git workspaces only), and
Otter refused to move it on the developer's word (D-186: only a task with no
workspace). Then Otter couldn't read the runs (no tool) nor watch them
(`ci` watches only the task's own pull request). Four gates, each reasonable
alone; together Otter routes messages instead of coordinating.

## 1. The tool table (M3's core; built first — everything else uses it)

- **Levels.** Every tool Otter has — gateway tools, the Claude Agent SDK's
  built-ins it may use (`WebSearch`, `WebFetch`, `Bash`) and three things the
  controller does on its own (`publish`, `approve_plan`, `accept_task`) — has
  one level: `auto` (any turn may use it, a lifecycle event's too),
  `developer` (only a turn the developer's message started: today's
  `DeveloperWord`), `off` (Otter isn't offered it; the controller asks the
  developer where it would have done it itself).
- **Defaults: `auto` for everything** but `remember` / `edit_memory`
  (`developer`: memory comes from talking with the developer, never from a CI
  log).
- **Where:** a host setting (`settings.json` `tool_levels`, only the ones
  changed from the default), shown and set in Settings › Otter. A task may
  tighten, never loosen: its own `tool_levels`, set from the CLI (`otter task
  tools`) and kept on the task; the stricter of the two applies. A trigger's
  policy is a preset of that: a triggered task without `publish` in its
  policy gets `publish: developer`.
- **The hard floor is not in the table.** Merging, force-pushing or pushing
  a default branch, deleting outside Otter's own directories, stopping an
  agent the developer started, credentials, two writers in one Git tree
  (D-079, D-156): checks under every tool, no level loosens them. `Bash` is
  the exception the doc names: its floor is the command policy (D-173,
  D-174), which is lexical.
- **Gateway** (`gateway.rs`): `tools_for` leaves out what is `off` for the
  task; `checked` replaces the `DeveloperWord`-needs-authority rule with the
  level. A change made without the developer's word is applied under the
  turn's own command id (`<event>:<tool>`), as a pause already is.
- **Turn budgets:** a turn that coordinates (an event's, a request's, a
  sub-task's report) gets 24 tool calls and 180 s; a reply keeps 12 and 60 s
  (`OTTER_TURN_COORD_MAX_CALLS`, `OTTER_TURN_COORD_DEADLINE_MS`).

## 2. M1 — where a task works can change

1. **`set_workspace` over a workspace the task has**, as the app's
   `SetWorkspace` allows: while it hasn't worked (no runs) and nothing runs.
   A task that has worked keeps its workspace (its work is there).
2. **A directory workspace inside a Git repository can be split.**
   `own_workspace` reads the repository's `origin` and the branch checked out,
   and makes a workspace of its own from that repository (a clone in Otter's
   repositories, D-108), based on that branch when the remote has it. No
   remote: refused as before. The message no longer says "worktree".
3. **The controller moves a task that would wait** (the "resource broker"):
   an unworked, writing task whose workspace is taken — another task's run or
   unfinished work, or an agent the developer started — gets a workspace of
   its own (2) when `set_workspace` is `auto`, and says so in its
   conversation. It doesn't take another existing workspace on its own:
   publishing commits whatever is in the tree (D-156), and a workspace the
   developer keeps may hold their uncommitted work. Archived, preparing and
   failed workspaces wait as before. Nothing to split it from: it waits.
4. **Reading doesn't take the workspace.** A run of an answer-only task on
   the SDK runtime (where the hook keeps it to reading) neither claims the
   workspace nor waits for it; writers don't wait for it either. It stays
   read-only for its whole life, even if answer-only is turned off meanwhile
   (the next run claims as usual). Elsewhere (`legacy_cli`) nothing changes.

## 3. M2 — Otter's own eyes and hands

1. **`read_github {url}`** (observe): an Actions run (and its jobs; the
   failed jobs' log tail, cleaned as CI logs are, D-139), a job, or a pull
   request (state, mergeability, checks). `gh` from Otter's own directory with
   Otter's GitHub sign-in (D-133), redacted and bounded.
2. **The SDK's built-ins in Otter's turn.** The worker's `otter` profile has
   `tools: []` today; it gets the built-ins otterd passes (`otter_builtins`)
   and a `PreToolUse` hook that asks otterd about each call, as a coding run's
   does: the level decides, every call is recorded. `Bash` runs in Otter's
   empty directory and goes through the command policy: routine runs, what
   only the developer may allow is refused (Otter can't wait for an answer
   mid-turn), what the policy denies is denied. A worker without the
   capability (`--check` lists `otter-builtins`) gets none. API-provider
   brains (D-095) have no built-ins: Anthropic's server-side search and a
   self-hosted search for the others are later.
3. **Watches on any GitHub target.** `watch` kind `run` with a run's URL,
   until it finishes; kind `ci` with a pull request's `url`, any pull
   request's CI, not only the task's own.
4. **A request that needs no code needs no workspace.** The turn on the
   request (D-186) may read GitHub and the web, watch, and
   `finish_with_answer`: the request was answered in Otter's reply, nothing is
   to change, so the task ends done without a workspace or a coding agent.

## 4. M3 — Otter acts on its own, and it's on record

1. **Event turns act** with whatever is `auto` (§1): a watch that fired, a
   sub-task that changed (§5), a followed task (D-094). Their prompt says
   they may act; what they did is in the reply.
2. **`tell_agent {text, now?}`**: Otter's own instructions to the coding
   agent, said as Otter's (never as the developer's, D-089). `now`
   interrupts the agent's turn, as `redirect_agent` does.
3. **`publish` automatic**: review allows pushing the task's branch and
   opening its pull request without asking (recorded like a trigger's
   allowance, D-083). `publish` as a tool allows it on the developer's word
   when the level is `developer`.
4. **`approve_plan` automatic**: a plan the brain's review had points on goes
   back to the agent once (as now), then is set as approved by Otter instead
   of waiting on the developer. Answer-only tasks still ask: the developer
   said not to change anything.
5. **`accept_task` automatic**: a task whose gates all passed is accepted by
   Otter once its report is written. Merging stays the developer's.
   *Superseded by D-198: there is no acceptance; a delivered task is done.*
6. **What Otter did (the ledger).** Every tool call is already in the task's
   history (`ToolCalled` with its origin, `ToolDone`); the controller's own
   calls (§2.3, §4.3–4.5) are recorded the same way. `otter.actions` lists
   them across tasks, newest first; the Automations view shows them with the
   task, and Settings › Otter is where a tool is turned down a level.

## 5. M4 — sub-tasks

1. **`create_task` in a task's conversation**: a task of its own with this
   task as its parent, its request written by Otter (said so, as it is
   Otter's word, not the developer's), where it works (`workspace`,
   `new_workspace` or `scratch`), `answer_only`, started at once. At most 3
   unfinished sub-tasks per task.
2. **`list_tasks`** in a task's conversation: its sub-tasks, and `get_task`
   with a `task_id` reads one of them.
3. **A sub-task's change wakes the parent**: done, failed, in review or
   waiting on the developer, once per change (like D-094's follow-ups, kept on
   the parent): an event turn in the parent's conversation says what it came
   to and may act.
4. The app shows a sub-task's parent, and a task's sub-tasks.

## 6. Not changed

The wire protocol's version (fields and one request are added; older
clients ignore them, older daemons refuse the request); the host scope (no
cross-host); the coding agent's command approvals (D-173, D-174); merging.

## 7. Tests

Each piece gets an e2e test that fails without it, in the scripted brain's
`CALL:` lines (as every test of Otter's tools): a taken directory workspace
and an answer-only task that runs anyway; `set_workspace` over the one the
task has; the controller splitting a taken workspace; `read_github` and a
`run` watch against the fake `gh`; `finish_with_answer` from the request; an
event turn that resumes; levels `developer` and `off` refusing; auto publish,
plan approval and acceptance; the ledger; a sub-task created, capped, and
reported to its parent. The worker's hook and options in its own tests.

## 8. As built (D-191–D-194)

Where the build differs from the plan above:

- **The request turn before a task waits** (§2.3) runs only when a placed
  task is about to wait on others, not on every started task: the case the
  screenshot showed, without a reply to every request.
- **A watch's own turn sets no other watch**: the one part of D-183's
  rule kept, so a watch can't look again for ever.
- **A trigger's preset** keeps `set_workspace` at `developer` too: an
  occurrence stays where its trigger puts it (D-156).
- **The built-ins need no capability flag**: a worker before them ignores
  `otter_builtins`, and the turn has none.
- **Undo** is its own history event, `ToolUndo {tool, task_id, action}`;
  the ledger lists what was done, not what was refused, and only tasks'
  histories (not the host conversation's).
- **A task with no workspace waits** on what Otter watches for it, or on
  its sub-tasks, instead of asking the developer for a workspace.
- `finish_with_answer` on a task already done does nothing.
