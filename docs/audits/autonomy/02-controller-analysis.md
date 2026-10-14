# 02 — The Controller and the Brain

Part of the [autonomy audit](README.md). Labels: [C] code · [R] reproduced ·
[D] dogfooding record · [I] inference. `brain.rs` is byte-identical between
v0.6.19 and HEAD; `controller.rs` changed only in the M1 commit (resume of
`Interrupted` runs, the `CUT_OFF` notice, recovery escalation).

## 1. When the controller runs

- **Loop:** `run_controller` (`controller.rs:84-128`) wakes on `feature_wake`
  or every `OTTER_CONTROLLER_TICK_MS` (default 5 s, 56-58). `feature_wake` is
  notified by **every** `feature_changed` (`features.rs:961`) and by every run
  ending (`runs.rs:322`). [C]
- **Which features:** active ones, ones in Review, Blocked ones with a
  workspace, and any with an unanswered developer message (102-109). One step
  per feature at a time (`inflight`, 113-115). [C]
- **Consequence:** because `run_activity` and `agent_said` rewrite the feature
  on every tool call and every text block (`runs.rs:843-887`), the controller
  is woken for each of them. Each wake runs `step`, which for a feature with a
  live run is cheap (`implement_step` → timeout check → `decide_open`), and
  makes **no model call unless a decision is pending**. [C] So the wake
  frequency costs CPU and disk, not model calls or agent time. [I: not timed]

`OTTER_CONTROLLER=off` freezes every feature: no run is ever started
(`controller.rs:93-95`). There is **no protocol request that starts a run
outside a feature** (`crates/protocol/src/lib.rs:205-218` has
`runtime.capabilities` and read-only `conversation.*` only). [C]

## 2. Every model call

| Entry point | Call site | Trigger | On failure |
|---|---|---|---|
| `plan` | `controller.rs:442` | Planning with no tasks, or `replan` | step error → **feature Failed** "planning failed" (449, 117-123) |
| `decide` | `controller.rs:600` | each pending decision except Recovery, once per decision id (`asked`, in memory) | escalate to the developer (610-612) |
| `judge` | `controller.rs:858` | every Verifying pass | **feature Failed** "judging failed" (865) |
| `review` | `controller.rs:1116` | each browser-check screenshot | ignored |
| `reply` | `controller.rs:179` | any unanswered developer message | falls back to status text, intent `None` — **the message is not forwarded** (193-199) |

All model calls have a 300 s timeout and **no retry** (`claude_stream.rs:451-456,
543-545`; `providers.rs:444-448`). On this host the brain is `openrouter` with
`anthropic/claude-haiku-5.5` (`~/.otter/state/settings.json`). [C]

**Measured frequency.** In the real feature [D]: 1 `plan`, 3 `judge`
(one per verification), 4 `decide` that produced an outcome (1 question
answered, 3 denials) plus escalations that the history doesn't separate,
and 2 `reply` (the developer's two messages after the failure). That is
roughly **10–20 model calls over 3 h 48 min** — not the bottleneck by count.
The damage is in *what* those few calls decide (§4–§6), not in how often they
happen.

## 3. Session continuity vs execution continuity

The plan asks to separate these. They come apart sharply in Otter.

- **Session continuity (same provider conversation):** preserved **only**
  when a task's last run ended `Cancelled`, `HandedOff` or (HEAD only)
  `Interrupted` and has a session id (`controller.rs:757-764`; v0.6.19:751
  without `Interrupted`). Every new task — including every "Fix what
  verification found" task — gets `Conversation::new` and a fresh SDK session
  (`runs.rs:122-137`). A `Failed` run (including a timeout) is never resumed.
  [C] **Real feature: 10 runs, 10 different Claude session ids.** [D]
- **Execution continuity (working without orchestration pauses):** a run is
  one turn. The actor stops the worker at the first `TurnFinished` with
  nothing queued (`actor.rs:617-626`); more turns happen only if the developer
  writes while it works (`runs.rs:892-926`). [C] So the agent's own plan can
  never span Otter's task boundary: when it finishes "its" task, the process
  is killed and the next task starts from zero.

**What the agent is told** (all injected by Otter, [C]):

- System prompt append, every run (`runs.rs:1334-1336`): *"You are working on
  one task of a feature, managed by Otter. Stay inside the working directory.
  Run the project's tests before you finish. End with a short summary of what
  you changed and how you checked it."*
- Fresh task prompt (`controller.rs:1650-1695`): feature title, request (or the
  title if the request is empty), all requirements, **all** acceptance
  criteria, "Your task (n of N): …", "The previous attempt failed: …", every
  developer message, "The work is checked with `cmd`; run it before you
  finish."
- Resume: "Continue the task where you left off." (+ `CUT_OFF` in HEAD).
- Any denial by the developer **or the controller**: *"The developer declined
  this."* (`runs.rs:943-945`).

Nothing tells the agent it is unattended. A question asked in plain text ends
the turn, the task is marked Done (`runs.rs:754-755`) and nobody answers it. [C]

## 4. The plan: generic tasks from a model that can't see the repo

`plan` gets only the brief (title, request, later messages) and no tools
(`brain.rs:538-542`; `--tools ""` at `claude_stream.rs:431-432`). It must emit
"1-5 tasks in order" and checkable criteria. [C]

Real feature [D]: an empty request and a one-word title produced
`Clarify scope → Survey existing code → Implement feature → Add tests → Verify
and fix`, three requirements including "Clarify the actual Otter feature
request, since the request text was empty", and criterion ac_3 *"The request
is confirmed with the requester before implementation begins"*.

Consequences, each in the record:

1. Task 1 told the agent to "Confirm … with the requester" — the source of the
   "came through empty" question (the agent's `AskUserQuestion`, worded from
   the plan) ([05](05-conversation-attention.md) §5).
2. Task 3 ("Implement feature") did **all** the work: M1 committed at 18:56
   (17.8 min, 97 tool calls, $4.12). Tasks 2, 4 and 5 each cold-started a new
   session to survey, add tests and re-verify what task 3 already had.
3. ac_3 is a *process* criterion that no evidence can satisfy after the fact.
   The judge marked it ✗ every time; remediation asked for "the requester's
   timestamped confirmation that predates the first implementation commit";
   the agent asked the developer to confirm scope a second time (19:03).

## 5. Timeouts and budgets the controller enforces

- **Run timeout** — `started_at + 30 min < now` (`controller.rs:674`), no
  deduction for time waiting on decisions; not checked while Blocked
  (`step` only calls `decide_open`/`unblock`, 142-148), so it fires on the
  first step *after* the developer answers. The run is stopped as Failed
  without an interrupt first; the attempt is spent; the next run is fresh.
  [C] **[R]** experiment E1; **[D]** runs `run_ctcjjwmbat` (blocked 66 min,
  killed 0.7 s after approval) and `run_a8743rrtac` (blocked 91 min, same).
- **Active-time budget (120 min)** — `active_since` is reset every time the
  feature goes from non-active to active (`feature.rs:547-549`), i.e. after
  every unblock. [C] It never bounded the 3 h 48 min feature.
- **Attempts** — 6 fresh runs per feature (`Budget::default`, `feature.rs:384`),
  3 per task (`controller.rs:41`). A 5-task plan leaves one fix cycle. **[R]**
  E2: a check that can't run (exit 2/127) with an agent that succeeds every
  time burns all 6 runs, then the feature fails "all 6 attempts are used up".
  The loop detector does not fire because it compares run failures, and every
  *run* succeeded — the failures are the check's. [C][R]

## 6. The controller deciding for the developer

- `decide_open` asks the brain about every pending decision except Recovery —
  including developer-only ones (`controller.rs:595-612`). [C]
- `policy::resolve` lets anyone deny (`policy.rs:579-581`), so the model's
  *deny* of a developer-only request takes effect, while its *allow* is
  refused and escalated. [C]
- The agent is told "The developer declined this." either way
  (`runs.rs:943-945`). [C]
- **[D]** three controller denials of developer-only requests:
  - a write to Claude Code's memory directory (reasonable);
  - `rm crates/daemon/src/lib.rs` — an **empty, untracked** file the agent had
    just asked the developer about and been told to delete; Haiku called it "a
    core source file". The agent's next summary: *"you approved deleting the
    empty lib.rs, but the delete command was declined"*;
  - `cargo test --workspace > /tmp/otter-test.log` — the run's whole purpose,
    denied because the log path was outside the workspace.
- The model sees only `questions[0]` of a form, but its one answer is copied to
  every question (`claude_stream.rs:608-619`, `claude_sdk.rs:739-747`). [C]

## 7. Prompt assessment

| Encourages… | Where | Verdict |
|---|---|---|
| Excessive progress reporting | "End with a short summary…" | Mild; the summary is used as `Run.summary` |
| Asking permission / questions | nothing tells the agent it runs unattended | Gap, not encouragement — but the *plan* can instruct it to ask (§4) |
| Repeated planning | `revise` intent replans and stops the run | Structural, model-triggered |
| Stopping after milestones | one task = one turn = one session | **Structural; the strongest effect** |
| Questions answerable from the repo | `decide`/`plan` have no repository access | The model answers blind ("Be conservative") |
| Repeated test runs | two prompts each demand a full test run per task; the controller runs it again | Redundant |

## 8. Answer to the phase question

**Does controller intervention reduce native agent autonomy? Yes — but not by
calling the model too often.** It reduces autonomy by *structure*: splitting
work into model-planned tasks run as separate cold sessions, ending each at
its first turn, re-running fresh after any failure or timeout, verifying
against criteria it invented, and letting a small model deny work in the
developer's name. The per-call overhead (a handful of Haiku calls) is minor
next to these. Whether the planning split is net-harmful on *well-specified*
requests is measured, on a small scale, in [06](06-benchmark-results.md).
