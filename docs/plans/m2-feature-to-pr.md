# M2 — Feature-to-PR workflow: implementation plan

**Milestone:** M2 (P0) of [`docs/roadmap.md`](../roadmap.md): *one complete
feature workflow, plan → implement → test → review → PR → CI → done, with
durable checkpoints and human control.*
**Status:** in progress — step 1 done (D-062, PR #53), step 2 done
(D-063, PR #54), step 3 done (D-064, PR #55), step 4 done (D-065, PR #56), step 5 done (D-066). **Branch:** `roadmap-m2`, started from `main`: M1's runtime changes
(`roadmap-m1` @ `5ebcf4f`, which the line numbers below refer to) aren't
published yet, so these steps are built without them.
**Inputs:** the M2 spec (scope, acceptance, deliverables, release gate — kept
verbatim in §8), and the autonomy audit
[`docs/audits/autonomy/`](../audits/autonomy/README.md) (PR #51), whose
root causes RC-1…RC-10 and remediation items P0-1…P2-5 this plan schedules.

## 1. Why M2 starts with the autonomy fixes

The pieces of the golden path exist: features and their store (D-043), the
Controller and brain (D-045), managed runs (D-044/D-055–D-057), and delivery —
push, pull request, CI follow-up, gates (D-047, `delivery.rs`). What doesn't
exist is a path that **finishes**:

- Benchmark Mode C: correct work, check exit 0 every time, independent
  acceptance test passing — and the feature ended *Failed, all 6 attempts are
  used up* on both tasks (audit 06).
- The real feature: requested work done after 26 min, then 3 h 20 min of
  verification loops, killed-after-approval runs and an unrunnable check
  (audit 01 §6).

M2's acceptance ("complete a PR in a disposable repository", "fail tests
deliberately; show failure, evidence and next action") can't pass while
verification can't conclude. So the first half of M2 is the audit's P0 items;
the second half is the checkpoint contract the spec asks for.

## 2. Design principles

Each is a decision to record in `docs/decisions.md` when implemented.

1. **The agent that can see the repository plans.** Planning becomes the
   first turn of the coding session: the agent reads the code and returns a
   checklist, the criteria, and the check it proposes. The brain reviews and
   may tighten it; it no longer plans blind from a root path and the brief
   (`brain.rs:537-549`).
2. **One warm session per feature.** Tasks are a checklist inside one
   conversation; the next task, a fix, a CI failure and a timeout are new
   **turns** in it (`resume` by the bound session id). A fresh session only
   when the SDK reports the session is gone, or on the developer's request.
3. **The supervisor verifies with deterministic evidence.** The judge gets
   HEAD, `git status`, `git diff --stat` and a bounded diff (uncommitted
   included), plus the check's result. The agent's in-session test output is
   admissible when Otter's own re-run of the check agrees. A failing
   deterministic check is never judged away (`brain.rs:651-664` stays).
4. **Stop on evidence, not on attempt counts.** Same state and same verdict
   after a fix → stop and hand to the developer. An unrunnable check is an
   environment problem, not a fix task.
5. **Time is counted only while the agent is working.** Waiting on the
   developer, or on background tasks, doesn't consume the run's budget;
   running out interrupts and resumes, it doesn't fail and restart.
6. **Developer-only stays developer-only.** No model sees or decides a
   `user_only` request; nothing is denied in the developer's name.
7. **Report once.** The conversation holds the developer, Otter, and one
   result per turn. Intermediate text and tool activity are detail
   (journal / Work tab), shown live as "latest N", not persisted per event.

## 3. Work, in order

Each step lands as its own commit with tests; the release gate (§8) runs on
each. Line numbers refer to `roadmap-m1` @ `5ebcf4f`.

### Step 1 — Verification that can finish (audit P0-1; RC-1, RC-4)

1. **Unrunnable check.** In `run_check` / `verify_step`
   (`controller.rs:1718-1764, 827`): exit 126/127, or an output tail matching
   `command not found|No such file or directory|Error 127`, is recorded as
   evidence with `ok: None` titled "`<cmd>` can't run here". One Controller
   message; the feature goes to Blocked with that as the reason; no fix task.
   (Exit code alone is not enough: `make` wraps a missing command as exit 2,
   audit E2.)
2. **Loop stop.** Before judging, fingerprint the state: HEAD + hash of
   `git diff HEAD` + `git status --porcelain` + the check's `ok`. If the
   previous verification had the same fingerprint and the same met/unmet
   pattern, don't create a fix task: go to Review with "Not verified: …;
   nothing changed after a fix". This lives in `verify_step`, because
   `repeating_failure` (`controller.rs:1565-1581`) keys on a failed run of the
   same task and fix tasks never match it.
3. **Diff evidence.** `verify_step` pushes an `EvidenceKind::Diff` item
   (`ok: None`) with HEAD, status, `--stat` and a diff bounded like
   `CHECK_OUTPUT`; untracked files via `git diff --no-index /dev/null <path>`,
   capped — never `git add -N` (it changes the agent's index). Not
   `delivery.rs`'s diffstat, which is `base...HEAD` (committed only). The
   judge prompt (`brain.rs:611-640`) says: "the diff and status are
   deterministic evidence; the check output proves only what it ran".
4. **Typed criteria.** `Criterion` gains `#[serde(default)] how: Check | Diff`
   (`core/feature.rs`). Check-typed criteria take `met` from the check in
   code; the model judges only diff-typed ones. The plan prompt forbids
   criteria about the conversation or the process ("confirmed with the
   requester", "no commit was made" as a criterion rather than a git fact).

**Decision to record:** a loop stop lands in **Review**, not Failed — the
developer sees exactly which criteria are unproven and can Accept or send it
back. Trades a little safety for completion; the deterministic check and the
developer's acceptance gate still hold.

**Tests:** audit E2 becomes an e2e assertion (1 run, no "Fix what
verification found" task, `iterations_used == 1`, one message); a test brain
that always answers ✗ with a passing check → at most 2 verifications and no
third run; a brain unit test on a temp repo with an uncommitted edit and a new
file → the judge prompt contains the hunk and the `??` line.

### Step 2 — Run time counts active work only (audit P0-3; RC-3)

- Replace the wall-clock limit (`controller.rs:61-63, 671-682`) with
  accumulated active time: paused while a decision is pending, while the run
  is Waiting, and while background tasks run (Step 4).
- A no-progress watchdog (no runtime event for N minutes while the model is
  generating) interrupts the turn; the feature resumes the same session with
  "you ran out of time on X" instead of a new run.
- Sleep of the host is not progress and not elapsed active time (count in
  slices; drop a slice whose wall time far exceeds its length).
- Set the run back to Running when a decision's answer is delivered
  (`runs.rs:656-661` sets Waiting and nothing resets it).

**Tests:** audit E1 as an e2e test — 3 s limit, 5 s wait on an approval →
the approved command completes.

### Step 3 — One session per feature (audit P0-2; RC-2)

- `implement_step` resumes the feature's bound session for the next task, a
  fix, a CI failure and a timeout — not only for Cancelled/HandedOff/
  Interrupted runs (`controller.rs:756-764`). Today a fix is a new task, so
  there is nothing to resume (`controller.rs:906-915`).
- Planning turn first (principle 1): the agent's first turn returns the
  checklist, criteria (typed, Step 1) and proposed check; `plan` in the brain
  becomes a review of that proposal. Small requests skip the checklist.
- Fresh session only on the SDK's "no conversation found" for that id, or on
  developer request; keep D-056 (one turn at a time) and D-061 (cut-off
  notice).
- Attempt accounting moves from per task to per feature (consecutive turns
  without a state change), replacing `MAX_TASK_ATTEMPTS` per task id.

**Tests:** e2e with the fake agent: a fix after a failed check reuses the
session id; benchmark t1/t2 in Mode C use 1 session.

### Step 4 — Runs survive background work (audit P1-4; RC-8)

- The worker (`packages/claude-runtime/src/map.ts`, which today handles only
  `system/init`) maps `task_started` / `task_progress` /
  `task_notification` into runtime events; the actor keeps a set of running
  background tasks.
- At the turn's `result`, `actor.rs:617-626` stops the worker only when that
  set is empty; otherwise it waits for a `task_notification` (which starts a
  turn by itself) with a grace limit, then stops.
- Long-running shells the agent started on purpose (dev servers) are not
  swept by an interrupt.
- Consider bumping the pinned SDK (0.3.285) for the task-event surface;
  check its changelog first.

**Tests:** a probe run that starts `sleep 20 && echo done` in the background
and says "I'll wait" ends only after `done`.

### Step 5 — Approvals without noise (audit P1-1, P1-2, P1-5; RC-5, RC-7, RC-9)

- **Path-aware sensitive files:** `.env` matches a path component
  `(^|/)\.env(\.[^/]+)?$`, tested against each path operand and the edited
  path, not the whole command line (`policy.rs:128, 175, 527-528`). `.envrc`
  is not a credential.
- **Write boundary:** `$TMPDIR`, `/tmp`, `/private/tmp` and the workspace's
  build/cache dirs are writable roots; `2>/dev/null` and other pseudo-devices
  are ignored (`policy.rs:405-408`). System paths and `$HOME` stay asks.
- **Approval cache:** when the developer approves, record a grant for the
  normalised command for the rest of the feature's session and check it
  before `record_ask` (`runs.rs:571-683`); optionally return
  `updatedPermissions` from `canUseTool` (`sdk.ts:180-187`). Destructive,
  credential and push actions are never granted — asked every time.
- **Developer-only:** `decide_open` / `escalate` skip the brain for
  `user_only` decisions (`controller.rs:580-669`); `escalate` re-checks the
  decision is still pending (removes the Blocked/unblocked flicker); a denial
  says who denied it and why (`runs.rs:943-945`).
- **Built-in tools:** classify `BashOutput`, `KillShell`/`KillBash`,
  `TodoWrite`, task-list and plan-mode tools explicitly; a unit test fails
  when the pinned SDK exposes a tool that lands in `ToolCall::Other`
  (`claude_stream.rs:621`, `policy.rs:559`).

**Tests:** replay every developer-only ask from the dogfooding record through
the new policy → only the Claude-memory write and `git push` still ask.

### Step 6 — Request and questions (audit P1-3; RC-6)

- An empty request takes the first developer message
  (`brain.rs:491-506`, `controller.rs:1522, 1637` stop skipping it); with
  none, Start asks one question itself before planning.
- The plan never instructs the agent to ask the developer.
- Optional, behind a decision: a non-blocking question tool — the agent asks,
  gets "pending" immediately and keeps working; the answer arrives as input
  to the session. Blocking `AskUserQuestion` stays for real blockers.

### Step 7 — Checkpoint contract and the golden path (the M2 spec)

- **State machine:** document the canonical `FeatureStatus` transitions
  (Draft → Planning → Implementing → Verifying → Review → delivery gates →
  Done; Blocked / Paused / Failed / Cancelled as side states) with the owner
  of each transition (developer, deterministic engine, brain) in
  `docs/feature-states.md`, mirroring `runtime-states.md` from M1. Brain
  decisions are only ever *inputs* to an engine transition.
- **Checkpoints and idempotency:** each step records a durable checkpoint
  with its evidence — plan accepted, turn finished (session id), check result
  (command, exit, tail), diff fingerprint, commit SHA, push (branch, SHA), PR
  (number, URL), CI (run ids, conclusion). Resuming re-reads the checkpoint
  and never repeats an external action: an existing branch/PR for the feature
  is adopted, not recreated.
- **Bounded retries:** CI failures return as a turn in the same session with
  the failed log (D-047's flaky rerun budget stays); repeated identical CI
  failure → developer.
- **Controls:** pause, resume, redirect (a developer message steered into the
  live turn), cancel — deterministic at every checkpoint. Cancel keeps the
  worktree and branch and reports the cleanup policy.

### Step 8 — Presentation (audit P2-1…P2-5; RC-10)

- One agent result per turn in the conversation; stop calling `agent_said`
  per text block (`runs.rs:387-388`).
- Activity as transient events, not `feature.json` rewrites
  (`runs.rs:874-887`) — also removes fsyncs from the policy-reply path.
- legacy_cli: policy allows are tool records, not decisions
  (`runs.rs:580-653`). Deduplicate summaries by normalised text.

### Also decide in M2 (audit P1-6)

`coding_backend()` defaults to `legacy_cli` (`settings.rs:153-159`). Steps
3–5 are SDK-first; either make `sdk` the default as part of M2 or record why
not and which steps also apply to `legacy_cli`.

## 4. What not to do

- Don't let a model approve what policy marks developer-only, and don't add a
  "bypass" mode for unattended work (that's M6, with its own policy).
- Don't hand the verdict to the agent alone: its report is admissible only
  alongside a deterministic re-run.
- Don't give the judge tools: one structured call with deterministic evidence
  keeps it cheap, reproducible and provider-neutral (`providers.rs`
  `structured` has no tool loop).
- Don't grow the policy into a full shell interpreter: a small tokenizer
  with path normalisation for operands and redirects is enough.
- Don't run parallel tasks in a shared directory (M5 uses a worktree per
  task, `git.rs` already supports it).

## 5. Measuring it

Rerun `docs/audits/autonomy/scripts/bench.sh` Modes B and C on t1/t2 after
each step, and `feature_metrics.py` on real features. Targets for M2:

| Metric | Today (audit) | M2 target |
|---|---|---|
| Mode C t1/t2 outcome | Failed, 6 sessions | Review, independent check passing |
| Sessions per feature | 6 (bench), 10 (real) | 1 (bench), ≤ 2 (real) |
| Fix tasks on correct work | 5 | 0 |
| Unrunnable check | 6 runs burned (E2) | 1 verification, 1 message |
| Approval followed by a timeout kill | 2 (real) | 0 (E1 test) |
| Main-column items, dogfooding replay | 69 | ≤ 15 |

## 6. Risks

- **Accepting bad work too easily** (Steps 1, 3): mitigated by keeping
  "failing check ⇒ not met", diff/status visible to the judge (catches
  out-of-scope edits, as in benchmark B2), and the developer's acceptance gate.
- **Long sessions hit context limits** (Step 3): the SDK compacts; allow a
  fresh session after N turns or on request.
- **Runs that never go idle** (Step 4): bounded by active time and the grace
  limit.
- **Loosened write boundary** (Step 5): only temp and build/cache roots;
  `$HOME`, credentials, system paths, destructive Git and push stay asks.

## 7. Deliverables

- `docs/feature-states.md` — state diagram, transition owners, checkpoint
  contract.
- A golden-path scenario script (disposable repository, fake agent for CI;
  live-Claude variant recorded as Pending in `runtime-validation.md`).
- A dogfooding report with failure cases (deliberate test and CI failures,
  interrupt at each checkpoint, cancel midway).
- Decisions for principles 1–7 and the loop-stop → Review choice.

## 8. M2 spec (from the roadmap) and acceptance

Scope: trace features/controller/brain/run bridge/Git/PR/CI; a canonical
state machine with an owner per transition, controller decisions separate
from deterministic execution; plan → implement → test → review → PR → CI →
done with durable checkpoints and resumable commands; evidence per step
(commit SHA, test command/results, browser artifacts, PR URL, CI status);
blocking questions and approvals routed to the attention inbox; pause,
resume, redirect and cancel deterministic; failing tests/CI handled with
bounded budgets and escalation rather than loops.

Acceptance:

- [ ] Create a feature from the CLI and complete a PR in a disposable test
  repository.
- [ ] Interrupt at each checkpoint; resume without duplicate branches, PRs or
  tool actions.
- [ ] Fail tests and CI deliberately; UI/CLI show the failure, evidence and
  next action.
- [ ] Cancel midway; preserve work and report the cleanup policy.
- [ ] (from the audit) Benchmark t1/t2 Mode C reach Review with the
  independent check passing, in one session each.

Release gate: `cargo fmt --all -- --check`, `cargo clippy --all-targets`,
`cargo test`; desktop build and smoke test when UI changes; regression tests
for the Claude and Codex paths; each acceptance scenario with reproducible
steps and captured results.
