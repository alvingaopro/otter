# Structured Claude runtime — validation record

What has been checked against real Claude, and what hasn't yet, for the `sdk`
backend (D-055–D-060). Each entry says how it was run and what was seen. No
keys or transcripts are kept here. "Pending" means not yet run, not failed.

## Version tuple

| | |
|---|---|
| Otter | 0.6.16 (`main` at the merge of #49) |
| Worker | 0.1.0, protocol 1 |
| SDK | `@anthropic-ai/claude-agent-sdk` 0.3.285, bundling Claude Code 2.1.285 |
| Node | 22.23.3 (macOS), 22.23.2 (Linux) |
| Hosts | macOS 27.0.1 arm64; Ubuntu 24.04 x86_64 (glibc 2.39) |
| Auth | the host's Claude Code sign-in (no API key) |
| Model | `haiku` for the daemon-level live tests; the SDK default for the probes |

## How

- **Worker probes:** `node packages/claude-runtime/probes/live.mjs`, on both
  hosts. 16 checks; see the worker's README for what each covers.
- **Daemon live tests:** isolated `OTTER_HOME` per test, a disposable
  workspace, the `rules` controller (no Otter model).
  `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime cargo test -p otterd
  --test e2e live_sdk -- --ignored --test-threads=1`. Last run 2026-10-10,
  macOS: `live_sdk_restart` and `live_sdk_controls` passed (59 s).
- **Packaging (D-060):** release v0.6.18 is the first with the worker
  (about 104–114 MB per platform, each with its `.sha256`).
  - `otter host install` from that release, into a throwaway `HOME`:
    - **macOS arm64 (local):** otterd 0.6.18 and the worker were installed;
      `runtime status` on the `sdk` backend said ready; a second install
      found the worker already there.
    - **Ubuntu x86_64 over SSH:** the same; the installed worker's `--check`
      passed (Node 22.23.2, its Claude Code present).
  - Installing 0.6.17, which has no worker, gave a note and a working
    otterd, reported "not ready".
  - The hosts' real installs weren't touched.

## The acceptance script (plan M6)

| # | Scenario | Status | Evidence |
|---|---|---|---|
| 1 | Non-Git coding task | Partial | `live_sdk_restart`: a disposable directory workspace; Claude ran a command and wrote the file it was asked for, and the task reached review. Not yet tried: planning by a model and real tests. |
| 2 | Existing directory with `.envrc`/flake | Pending | — |
| 3 | Native context across runs | Passed | Probes on both hosts: a new run resumed the native session by id and recalled a fact. `live_sdk_restart` and `live_sdk_controls`: generation 2 continued the same native session. |
| 4 | Real interaction (permission, deny, form) | Passed at worker level | Probes: policy "ask" reached a permission request; a denied command didn't run; a two-question form was answered per question. Not yet tried: the same through the desktop app on a real task. |
| 5 | Mid-run guidance | Partial | `live_sdk_controls`: a redirect mid-tool ran next in the same run, once, and the task reached review. Not yet tried: queue one message *then* redirect, checking order. |
| 6 | Connectivity (app/SSH drop, reconnect) | Pending | Reconnect is covered offline (CLI and daemon e2e); not with real Claude over SSH. |
| 7 | Daemon crash mid-turn | Partial | `live_sdk_restart`: otterd **stopped** (graceful shutdown) mid-tool. The turn became `outcome_unknown`, its input `delivered`, and the next run resumed it as generation 2. Not yet tried: `SIGKILL` of otterd with real Claude (the leftover process-group cleanup is tested offline only). |
| 8 | Missing binding / auth / runtime | Partial | Missing worker, old Node and a worker that can't be checked are reported (`runtime status`; unit tests). Not yet tried: a missing sign-in or transcript with real Claude. |
| 9 | Limits | Pending | — |
| 10 | Git review | Pending | Needs an authorized disposable repository; publishing a PR needs explicit authorization. |

## Roadmap M1 — runtime reliability

The milestone's acceptance scenarios (`roadmap.md`, D-061). Offline evidence
is the daemon e2e suite with a fake agent speaking Claude Code's protocol
(`tests/fake_claude_stream.sh`): real otterd, real process deaths, no model.
The table of states and crashes is `runtime-states.md`.

| Scenario | Offline | Live Claude |
|---|---|---|
| Kill the agent's process mid-tool: the recovered state doesn't claim success or repeat the tool call | Passed — `a_run_lost_mid_tool_resumes_without_claiming_the_step`: the turn is `outcome_unknown` (`worker_crashed`), the tool `result_unavailable`, the run `interrupted`; the next run resumed the same native session (no fresh start, no attempt spent) and was told to check before repeating. `a_run_lost_before_its_conversation_was_kept_starts_over_only_when_allowed`: with nothing to resume, nothing restarted until the developer approved. | Pending |
| Kill otterd before/after an approval decision is persisted: neither lost nor applied twice | Passed — decisions are applied by `task.act` with a `command_id` (a retry returns the result, `a_run_that_keeps_getting_lost_waits_for_the_developer` repeats one); a decision of a run that died with otterd is cancelled, not carried over (`recover_runs`); a `recovery` decision survives a restart and nobody decides it for the developer (same test). Not run: `SIGKILL` between the task store's write and its rename. | Pending |
| Disconnect SSH/desktop during a run: the run continues; reconnecting shows ordered history | Passed — `client_disconnect_does_not_affect_session`, `event_replay_resumes_after_disconnect_and_daemon_restart`, conversation history paging (D-058), CLI `events_follow_resyncs_when_the_hosts_log_starts_over`. | Pending (plan M6 #6) |
| Resume after a restart: conversation identity and cursor stay stable | Passed — `runs_survive_a_restart_and_follow_a_changing_goal` (same conversation, generation 2, same native session, run `interrupted`), `changes_are_journaled_and_replayed_after_a_crash`. Live: `live_sdk_restart` (graceful stop, 2026-10-10). | Partial (no `SIGKILL` yet) |

Release gate for this change: `cargo fmt --all -- --check`, `cargo clippy
--all-targets` and `cargo test` passed on macOS (2026-10-13, and again on
2026-10-14 after rebasing onto M2–M6); the desktop app's sources typecheck
(only type changes there).

## The task-to-PR workflow (plan M2)

What M2's steps need a live Claude run to show; the offline e2e tests (fake
agent) cover the mechanics. Re-run with the audit's harness:
`docs/audits/autonomy/scripts/bench.sh` (Modes B and C on t1/t2) and
`task_metrics.py` on real tasks.

| # | Scenario | Status | Evidence |
|---|---|---|---|
| M2-1 | Benchmark t1/t2, Mode C: reach Review with the independent check passing | Pending | Offline: `verification_stops_when_a_fix_changes_nothing`, `a_check_that_cant_run_blocks_on_the_developer_once` (D-062). |
| M2-2 | One session per task (t1/t2 Mode C: 1 session each; real tasks ≤ 2) | Pending | Offline: a fix resumes the session (`--resume`), the planning turn and steps share it (D-064). |
| M2-3 | An approval answered after the run's limit: the approved command completes | Pending | Offline: `waiting_on_an_approval_doesnt_use_up_the_runs_time` (audit E1, D-063). |
| M2-4 | The planning turn with a real model: a JSON proposal, reviewed by the brain | Pending | Offline: `the_coding_agent_plans_first_in_the_tasks_conversation`, `a_plan_the_brain_faults_goes_back_once_then_to_the_developer` (D-064, D-084). |
| M2-8 | A question with real Claude ("how does X work?" on a real repository): the agent's answer with `path:line`, the task done with no steps; then "then change it": a plan in the same session | Pending | Offline: `a_question_is_answered_and_a_change_after_it_is_planned`, `a_failed_look_at_the_code_stops_planning` (D-084). |
| M2-5 | A run that starts `sleep 20 && echo done` in the background and says it will wait ends only after `done` (both backends) | Pending | Offline: `a_run_waits_for_the_agents_background_work` (stream-json shapes), actor unit tests, worker mapping tests (D-065). |
| M2-6 | The golden path with real Claude and real GitHub: a task from `otter task new` to an accepted pull request in a disposable repository | Pending | Offline: `a_task_goes_from_the_terminal_to_an_accepted_pull_request` (CLI e2e: a fake agent, a real Git remote, a fake `gh`) (D-068). Publishing to GitHub needs an authorized disposable repository. |
| M2-7 | Dogfooding with deliberate failures: a failing test, failing CI (then the same failure again), otterd stopped at each checkpoint, a cancel midway | Pending | Offline: `the_same_ci_failure_after_a_fix_goes_to_the_developer`, `delivery_resumes_at_each_checkpoint_without_repeating_itself`, `an_open_pull_request_is_adopted_and_cancelling_keeps_the_work`, `runs_survive_a_restart_and_follow_a_changing_goal`, `a_check_that_cant_run_blocks_on_the_developer_once` (D-062, D-068). |

## The agent manager (plan M3)

| # | Scenario | Status | Evidence |
|---|---|---|---|
| M3-1 | Several tasks and workspaces at once on two hosts, handled from Chat and the sidebar (the Inbox until D-178): decisions answered on the task's page, a review accepted, a stopped task retried, notifications once per need | Pending | Offline: `TasksView.test.tsx` (an approval answered, Accept), `taskNeeds.test.ts` (a need told once), `what_a_task_needs_is_one_rule` (core), `tasks_that_need_you_are_listed` (`otter ls`) (D-071…D-073, D-178). |

## Diagnostics (plan M4)

| # | Scenario | Status | Evidence |
|---|---|---|---|
| M4-1 | Real failures diagnosed without a terminal and without a secret: Claude Code signed out (auth), a rate limit, a crashing agent, a direnv error printing a token from `.envrc`, a command failing with a reason; `otter doctor` on a host missing tmux | Pending | Offline: `credentials_in_failures_stay_out_of_what_otter_keeps`, `a_failed_process_says_why_without_its_terminal`, `a_host_says_whats_wrong_with_it`, `doctor_says_whats_wrong_and_what_to_do`, the `redact` unit tests (D-075…D-077). |

## Parallel agents (plan M5)

| # | Scenario | Status | Evidence |
|---|---|---|---|
| M5-1 | Three tasks on one real repository with live Claude: two in parallel worktrees, one waiting for the busy workspace and going on by itself; all paused at once and resumed; each PR with only its own work | Pending | Offline: `a_second_task_waits_for_the_workspace`, `a_waiting_task_gets_a_worktree_of_its_own`, `pause --all` in the CLI e2e, `TasksView.test.tsx`'s Pause all (D-079…D-081, D-178). |

## Unattended work (plan M6)

| # | Scenario | Status | Evidence |
|---|---|---|---|
| M6-1 | A daily trigger with live Claude on a real repository, overnight: fires once at its time (and once after otterd was down through it), publishes when allowed, pauses on a decision past its wait limit and waits in the Inbox | Pending | Offline: the four trigger e2e tests and `otter trigger` in the CLI e2e (D-083). |

## Otter answers with tools (plan `brain-runtime-v2`, M0)

| # | Scenario | Status | Evidence |
|---|---|---|---|
| V2-0a | The Agent SDK runs Otter's profile — none of Claude Code's tools, one tool the worker defines — under the host's sign-in: only that tool offered, called once with the task, its result in the answer, nothing else run or asked for | Passed 2026-10-14, Ubuntu x86_64, `haiku`, SDK 0.3.285 / Claude Code 2.1.285, auth source `none` (the sign-in) | `node packages/claude-runtime/probes/otter-tools.mjs`: 5/5. Options: `tools: []`, an in-process `createSdkMcpServer` tool, `strictMcpConfig`, `settingSources: []`. The SDK warns that a tool named in `allowedTools` skips `canUseTool`: Otter's gate is otterd's gateway, which every call reaches. |
| V2-0b | The same under an API key passed through otterd's env file | Pending | No key on the host the spike ran on. |
| V2-0c | 5–10 dogfooding transcripts (a code question mid-task, a blocked approval, guidance and a redirect while running, more on a done task, a question-only request, "ship it" in review, "go on" on a plan), and the turn's limits calibrated against them | Pending | Needs real use; until then the plan's 60 s / 12 calls / 6 round trips stand. |
| V2-1a | Otter's turn through the built worker's Otter profile, real Claude: asked "why is this task blocked?" with a pending `rm -rf /opt/shared` decision behind its tools, it looks (`get_task`, `get_pending_decisions`) and names the decision as the developer's to make; no `INTENT:`, nothing written | Passed 2026-10-15, Ubuntu x86_64, `haiku`, the sign-in | `live_otter_turn` (`otterd` unit, ignored): real worker and Claude, the tools' answers stubbed. |
| V2-1b | The same through the whole daemon: `OTTER_CONTROLLER=claude`, `OTTER_BRAIN_V2=1`, a task blocked before planning ("Choose a workspace"), asked why | Passed 2026-10-15, as above: the last 3 runs in a row | `live_otter_says_why_a_task_is_blocked` (e2e, ignored). Earlier runs found two faults, both fixed: "workspace" read as a part of the code (Otter's instructions now say what its words mean), and the directory the worker ran in offered as the workspace (it now runs in an empty directory of otterd's, and is told it's nobody's). Whether it looks with `get_task` or answers from its context varies; the reason it gives was right every time, though haiku sometimes calls the blocked task "still a draft" (it has no steps yet). |
| V2-1c | A task at work with live Claude as the coding agent too: guidance, a redirect, "ship it" in review, through Otter's turn | Pending | Offline: `converse::*::tools`, `otter_turn::*` (fake worker for the Claude turn). |
| V2-2 | A question while real Claude works (SDK runtime, haiku): asked to create a file and to list the directory, it answers after the work, listing the files and saying the file needs a turn of its own; the file isn't written; the run's result stays the work's | Passed 2026-10-15, twice in a row | `live_a_question_is_answered_reading_only` (e2e, ignored). Earlier runs: enforcement held, but the agent couldn't list files — the native Claude Code had no Glob/Grep (now asked for in coding runs, D-091), and the question's prompt now says which tools work. |
| V2-3 | An answer-only task with real Claude as the coding agent (SDK runtime, haiku), asked what `make test` does and to write the answer into a file: it answers, pointing to `Makefile:1-2`, the task is done, and no file is written | Passed 2026-10-15 | `live_an_answer_only_task_writes_nothing` (e2e, ignored). |
| V2-4 | Otter on the host with real Claude (haiku, through the worker): asked to make a task in a named workspace and not start it, it makes a draft there with the developer's message, word for word, as its request; asked which tasks there are, it names it | Passed 2026-10-15 | `live_otter_on_the_host_makes_a_task` (e2e, ignored). |
| V2-5 | Real Claude all the way (Otter and the coding agent, haiku): asked on the host to make and start a task that creates a file, Otter makes it, the agent does it, and Otter follows up once when it reaches review | Passed 2026-10-15 | `live_otter_follows_up_a_task_it_made` (e2e, ignored). The follow-up reported what the task said: verification judged the file missing — the workspace wasn't a Git repository, so there was no diff to see. That is the verification's limit (D-062), not the follow-up's. |
| V2-6 | A provider's API with Otter's tools (OpenRouter, `openai/gpt-4o-mini`, the key in this host's environment): asked about a draft task's history, it calls `get_task_history` and answers that it is a draft | Passed 2026-10-15 | `live_an_api_provider_answers_with_otters_tools` (e2e, ignored). Offline, both API styles against a stand-in: `provider_turn::*`. Anthropic's API live: pending (no key here). |
| V2-0d | Today's conversation, pinned: each route through `converse` (guide, redirect, go on from draft, paused and failed, pause, revise, finish in review and at work, done + guidance, the plan question, a question, another developer-only decision, no model) | Passed (offline) | `crates/daemon/tests/e2e/converse.rs`, 14 tests on the `scripted` brain; forcing every intent to none fails 11 of them, the other three expect nothing to happen. |
| W-1 | Watches with real Claude and a real pull request (D-183): asked in a task "tell me when CI is done", Otter sets a `ci` watch and says for how long; when CI finishes it says once, in the developer's language, what it came to and when it was read; a conflicting pull request is said at once | Passed 2026-10-22 | v0.7.78, `claude-opus-5-5`, the scratch repository on a base with a 4-minute check: the watch set and said in Chinese; "all 4 checks passed" reported 3 s after the probe, once; `time` fired 11 s late and said what changed; a conflicting pull request said at once. Found: the reply as a watch was set told a stale gate — fixed by D-188 (`watch` reads first). Not seen in the Mac app. Offline: `watches::*`. |

## Known limits

- A daemon stop ends the managed run. The next run resumes the native
  session, so what was said survives but the process doesn't (invariant 4's
  exception).
- No exactly-once guarantee for tool effects: a tool cut off by a crash is
  `result_unavailable`.
- Linux needs glibc; musl hosts aren't supported.
- Subagents (`Task`, `Agent`) are disallowed.
- Structured attachments are off; screenshot paste works in interactive
  sessions.
