# Managed runs — states, crashes and recovery

What a managed coding-agent run (D-044) can be, how it moves, what each
crash leaves behind, and what to do about it. Roadmap M1 (`roadmap.md`);
the decision is D-061. Two records are involved, and they are kept apart:

- the **feature's run** (`otter_core::feature::Run`, in `feature.json`):
  what the feature, the controller and the developer see;
- the **conversation** (`otter_core::conversation`, D-055/D-058): the
  journaled authority on what was sent to the agent and what came back —
  turns, tools, interactions, generations.

## Run states

| State | Meaning | Over? |
|---|---|---|
| `starting` | Reserved; runs are recorded `running` once their process is up. | no |
| `running` | Its agent is working on a turn. | no |
| `waiting` | Its agent waits on a decision (policy left it open). | no |
| `completed` | Its last turn finished successfully (not: the work is accepted). | yes |
| `failed` | The agent reported a failure, or the run hit a limit (timeout). The task's attempt is spent; the next run starts over with what went wrong. | yes |
| `cancelled` | Stopped on purpose: pause, interrupt, a changed goal, cancel. The next run resumes its conversation. | yes |
| `handed_off` | The developer took the conversation over in an interactive session. | yes |
| `interrupted` | Ended with its process — otterd restarted, or the agent's process was lost — while its turn was unsettled. The next run resumes the same conversation, told that the cut-off step's result is unknown. Not an attempt. | yes |
| `recovery_required` | Ended so that Otter can't safely continue on its own. The task is `blocked`, the feature `blocked`, and a `recovery` decision (the developer's only) asks whether to start the task over. | yes |

The roadmap's other names map onto existing records: *queued* input is a
`queued` turn of the conversation (it waits there for the next run, D-059);
*awaiting approval* is `waiting`.

**Transitions** (`RunState::can_become`): `starting → running ⇄ waiting`,
and any live state → any ended state, once. An ended run never changes
again; continuing its work is a new run (a new generation of the same
conversation). The daemon refuses any other move: a late `waiting` after the
run ended, a second ending, an ending recorded by a restarted daemon for a
run that already ended.

### How an unsettled ending is classified

When a run ends without its turn settling — otterd restarted
(`recover_runs`) or the agent's process went away mid-turn (the actor's
`OutcomeUnknown`) — `runs::unsettled` decides, in this order:

1. its conversation's journal can't be trusted (read-only) → `recovery_required`;
2. it is the third run of the task in a row to be cut off → `recovery_required`
   (no endless resume loop);
3. the agent said its session id → `interrupted` (resume it);
4. no turn of the run was ever sent → `interrupted` (starting again
   repeats nothing; a fresh run, which is an attempt);
5. otherwise the agent had the task and there is nothing to resume →
   `recovery_required`: starting over could repeat what it did.

A run whose journal shows it completed before the daemon died is recorded
`completed` (D-058) and never reaches this.

## Crash matrix

"Turn" is the conversation's record; "run" the feature's.

| What happens | When | Turn | Run | Then |
|---|---|---|---|---|
| otterd stops or is killed | before the turn is sent | never sent | `interrupted` | A new otterd starts the task again; nothing had reached the agent. |
| otterd stops or is killed | turn sent, not confirmed | `outcome_unknown`, `delivery: unknown` | `interrupted` (with a session id) | Resumed; the turn is never resent; the continuation says the result is unknown. |
| otterd stops or is killed | mid-tool | `outcome_unknown`, tool `result_unavailable` | `interrupted` | Same. Leftover processes of the old run are ended by pid *and* start time (D-058). |
| otterd stops or is killed | after the turn completed, before the feature heard | `completed` | `completed` (from the journal) | Nothing runs again. |
| The agent's process dies (otterd lives) | mid-turn or mid-tool | `outcome_unknown`, error `worker_crashed`, tool `result_unavailable` | `interrupted` | Resumed as above; queued guidance waits for it. |
| The agent's process dies | before it said its session id, turn sent | `outcome_unknown` | `recovery_required` | Blocked on a `recovery` decision. |
| Either, a third time in a row | — | `outcome_unknown` | `recovery_required` | Blocked on a `recovery` decision. |
| A conversation record can't be written | any | the conversation turns read-only | `recovery_required` | Blocked; see troubleshooting. |
| otterd stops or is killed | a decision pending | — | `interrupted` | The decision is `cancelled` (its run is gone); the resumed agent asks again if it still wants to. A decision is never carried into a run that didn't ask it. |
| otterd killed | while a decision is being recorded | — | — | `feature.act` is idempotent by `command_id`: before it was persisted, retrying applies it once; after, retrying returns the result. It is never applied twice. |
| otterd killed | with a `recovery` decision open | — | stays `recovery_required` | The decision survives restarts (it isn't tied to a live run); nothing starts by itself. |
| A client disconnects (SSH, app) | any | unaffected | unaffected | The run goes on in otterd. A reconnecting client resumes events from its cursor and reads `feature.events` / `conversation.history` in order (invariant 12). |
| The developer pauses / interrupts / cancels | any | `interrupted` / `cancelled` | `cancelled` | Pause and interrupt resume later; cancel closes the conversation (D-059). |
| The run outlives its time limit | any | `limit_reached` | `failed` | An attempt spent; the next run starts over. |

Covered by tests: `a_run_lost_mid_tool_resumes_without_claiming_the_step`,
`a_run_that_keeps_getting_lost_waits_for_the_developer`,
`a_run_lost_before_its_conversation_was_kept_starts_over_only_when_allowed`,
`runs_survive_a_restart_and_follow_a_changing_goal`,
`guidance_reaches_the_agent_once_across_a_pause_and_a_restart`
(daemon e2e), and the unit tests `a_run_ends_once` (core),
`a_run_cut_off_is_resumed_only_when_that_is_safe` and
`changes_are_journaled_and_replayed_after_a_crash` (daemon).

What isn't promised: a tool cut off mid-way may or may not have taken
effect, and nothing can tell from outside the agent. Otter's part is to
never call it a success, never resend the turn on its own, and tell the
resumed agent to check before repeating it.

## Troubleshooting (operators)

Everything below is visible from the CLI (`otter feature show <id>`,
`otter conversation show <id>`, `otter conversation history <id>`) and
the desktop app; no secrets are in any of it.

**A feature is blocked with "Needs you: a run can't safely continue".**
Its last run is `recovery_required` and a `recovery` decision is open
("Start “<task>” over?"); the decision's detail says why.

1. Look at the workspace: what the cut-off run was doing (the run's
   activity lines, the conversation's last tool) may already have happened —
   a file written, a command run, something published.
2. Undo or finish it by hand if needed, or take the conversation over.
3. Approve to start the task over (a fresh run, counted as an attempt):
   `otter feature decide <feature> <decision>`; or decline (`--deny`): the
   feature pauses with the task pending, and starts over when you resume it
   (`otter feature resume <feature>`).

Nobody else can answer it: the Control Agent escalates it, and a restart
keeps it open.

**"…its conversation's record can't be trusted (…)".** The conversation's
journal (`~/.otter/state/conversations/<id>/journal.jsonl`) failed to load
or to be written. It is read-only; nothing is skipped or rewritten. Check the
disk (space, permissions — the files are 0600, this user's), then decide as
above; a fresh run starts a new conversation and leaves the broken one for
inspection.

**"…runs in a row were cut off".** The agent's process keeps dying
(`worker_crashed`) or otterd keeps restarting mid-run. Check
`otter runtime status` (Node, worker, Claude Code sign-in), the host's
memory, and otterd's log (`~/.otter/logs/workd.log`) before starting over.

**A run is `interrupted`.** Nothing to do: the controller resumes it. If it
doesn't, the feature is paused or blocked on something else — its status
reason says what.
