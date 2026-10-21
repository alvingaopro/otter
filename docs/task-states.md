# Task states, transitions and checkpoints

How a task goes from a request to an accepted pull request (M2,
D-068), who moves it, and what is kept at each step so that otterd can stop
anywhere and pick up without doing anything twice. Code:
`crates/core/src/task.rs` (`TaskStatus::can_become`),
`crates/daemon/src/controller.rs` (the engine), `tasks.rs` (the
developer's actions), `runs.rs` (runs), `delivery.rs` (publishing).

## States

| State | Meaning | Driven by |
|---|---|---|
| `draft` | Written down, not started. | — |
| `planning` | Finding the goal (D-067), the coding agent's look at the code — an answer or a plan (D-064, D-084) — the plan's review. | engine |
| `implementing` | The next step is a turn in the task's conversation (D-064). | engine |
| `verifying` | The check runs, the work's state is read, criteria are judged (D-062). | engine |
| `review` | Verified (or stopped on evidence): publishing, CI, gates, the report (D-047). | engine, then the developer |
| `done` | Accepted; or a question, answered (D-084). Not final: a new ask plans again (D-050). | developer, engine |
| `blocked` | Waits on the developer: a decision only they make. | developer |
| `paused` | Held by the developer; runs stopped, conversation held. | developer |
| `failed` | A budget ran out or Otter couldn't go on; the plan is kept. | developer (retry) |
| `cancelled` | Final. Nothing of the work is removed. | — |

Active (`planning`, `implementing`, `verifying`) is what the engine steps;
`review` and `blocked` are stepped for what they wait on.

## Transitions and their owners

The engine is deterministic code. A model (the brain) never moves a
task: its plans, verdicts and judgments are *inputs* the engine checks
and acts on — and policy decides what a model may decide at all (D-044,
D-066).

| From → to | Owner | When |
|---|---|---|
| draft → planning | developer | Start. |
| planning → blocked | engine | No workspace; or no request and no message (asks "What should this task do?", D-067); or the agent's plan, sent back once, still has problems (asks "Go ahead with the coding agent's plan?", D-084). |
| planning → implementing | engine | The plan is set — the agent's proposal, reviewed by the brain (D-064, D-084). |
| planning → done | engine | The look at the code came back with no plan: the request was a question, and the agent's words are the answer (D-084). Answer-only, with a plan the developer turned down: the agent's next turn answers in words; if it plans again, the developer isn't asked twice: Otter shows what it said and the plan, not carried out, or says there is no answer without the change (D-158). |
| planning → failed | engine | The look at the code failed or said nothing: no plan is guessed (D-084). |
| implementing → verifying | engine | Every step done or skipped. |
| verifying → implementing | engine | Something unmet, and the work changed since the last verdict, or on the same work fewer unmet than the last verdict left (the fix proved some): a fix step (D-062, D-165). |
| verifying → review | engine | All criteria met; or, after a fix, the same work with no fewer unmet, whatever the verdict says (unverified, D-062, D-165). |
| verifying → blocked | engine | The check can't run here (D-062). |
| review → implementing | engine | CI failed on the code: a fix step, unless it failed the same way after a fix (D-068). |
| review → done | developer | Accept (every gate passed or not applicable — nothing to publish is, D-166 — or `--anyway`, on record). |
| done → planning | developer | A new ask (any message asking for work, D-084): the agent looks again. |
| any active → blocked | engine | A decision only the developer may make (policy `user_only`). |
| blocked → previous | engine | Nothing pending any more (`resume_status`). |
| active, blocked, review → paused | developer | Pause. |
| paused → previous | developer | Resume: the state the pause found (`paused_from`); blocked again while a decision only the developer may make is pending; review stays review (D-168). |
| active, blocked, review → failed | engine | A budget used up, a loop, or an error Otter can't go past. |
| failed → previous | developer | Retry (fresh attempt budget). |
| any → planning | developer | The goal changed (request changes, a revising message). |
| any → cancelled | developer | Cancel. |

## Checkpoints

The task document (`state/tasks/<id>/task.json`, written whole and
fsynced, with a per-task history) is the checkpoint; the conversation's
journal is the agent's. A restarted otterd reads them and goes on.

| Step | What is kept | On resume |
|---|---|---|
| Goal | `request`, or the answered "What should this task do?" decision | Plans from it; asks again only if nothing was said. |
| First run | `started_from`: the workspace's revision as the task's first run there begins (D-167) | Kept; taken again only in another workspace. |
| Planning turn | the planning step's run, its report (a proposal or an answer); a plan sent back carries the review's points (`Step.detail`), an answer-only plan turned down the ask to answer in words (D-158); a plan approval names the planning task | Continues the turn if cut off; else reviews the proposal (or reads the developer's word on it), or posts the answer. |
| Plan | `requirements`, `acceptance` (typed), `steps`, `verify_command` (+ its decision) | Carries on with the next step. |
| Each turn | a `Run`: state, `provider_session_id`, `worked_ms`, summary; the turn in the journal | A cut-off turn continues the same session with a notice (D-063); a finished one isn't repeated (recovered from the journal). |
| Check | `Test` evidence: command, exit, output tail | Re-run on the next verification (cheap, local). |
| Work state | `Working tree` evidence (the change since `started_from` while HEAD descends from it, else since the merge base with the base branch, D-167), `last_verification` (fingerprint + unmet), `judged_runs` (the runs at the last verdict) | The loop stop compares against the mark; the judge sees the commands of the runs after the count (D-165). |
| Nothing to publish | `Nothing to publish (<HEAD>)` in the rationale; the PR and CI gates `not_applicable` (D-166) | Git says nothing to commit, and HEAD is where the task started (or no further than `origin`'s base branch): not asked, not pushed; said once per HEAD. |
| Commit | `Committed <sha>` evidence | A clean tree: nothing to commit again. |
| Push | `delivery.branch`, `head`, `pushed_at` | Pushes only if HEAD moved or work is uncommitted; a push is idempotent. Not pushed when nothing of the task's own is known to be unpublished; the forge's refusal of the pull request after a push is nothing to publish, the push kept (D-166). |
| Pull request | `delivery.pr_number`, `pr_url`, `PullRequest` evidence | An open PR for the branch is adopted (`gh pr view`), never opened twice. |
| CI | `delivery.ci`, `reruns`, `Ci` evidence (pass per HEAD, failure with its log) | Read again; a failure already acted on isn't acted on twice. |
| Done | `gates`, `report`, status | — |
| Every reason | a line in `rationale`, and `Reasoned` in the history at its time (D-141) | — |

External actions — push, pull request, rerun, comment — go through
`delivery.rs` and are idempotent by what they check first. Otter never
merges or deploys.

## Controls

At any checkpoint, deterministically:

- **Pause** — the current turn is interrupted, the run ends, the
  conversation is held; resume continues it with a notice.
- **Resume** — back to the state it was paused in.
- **Interrupt** — the current turn stops; the task goes on.
- **Redirect** — the current turn stops and the developer's message goes
  next, in the same run (D-059).
- **Message** — guidance queued for the agent's next turn, once.
- **Cancel** — runs end and the conversation closes; the workspace, its
  files, the branch and an open pull request stay, and the message says so
  (D-068).
- **Decide** — answer what the task waits on; an approval of an ask
  raised only for where it reaches is remembered for the task (D-066).

## Budgets and stops

Deterministic, whatever a model thinks:

- Time: the agent's working time per run (`OTTER_RUN_TIMEOUT_MS`, 30 min)
  and per task (`max_minutes`), waits excluded; out of time is cut off
  and resumed, not failed (D-063). Silent too long: the same (watchdog).
- Attempts: new work costs one (`max_iterations`, 6); three per step.
- Loops: the same failed run three times; the same work after a fix with
  nothing more proven (→ review, D-165); the same CI failure after a fix,
  times, durations and ids aside (→ the developer, D-139).
- CI that looks like the infrastructure's fault: rerun at most twice per
  pushed commit (D-047).
