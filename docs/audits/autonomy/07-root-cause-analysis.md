# 07 — Root cause analysis

Part of the [autonomy audit](README.md). Confidence levels as the plan defines
them: **Confirmed** (reproduced or directly demonstrated), **Strong evidence**
(multiple independent observations), **Hypothesis** (plausible, not
validated). Labels for evidence: [C] code · [R] reproduced · [D] dogfooding
record (legacy_cli) · [I] inference.

## 1. The central hypothesis

> *Is Otter's orchestration layer reducing the native autonomy of Claude Code?*

**Answer: 2 — Yes, but in specific, identifiable mechanisms, and not where the
symptoms suggested.**

- **Not** through how often the Controller calls a model: ~4 model calls per
  small feature (1 plan + 1 judge per verification) and ~10–20 over the 3 h 48 min
  real feature. Not through SDK worker start-up (~1.5 s/run). Not through the
  SDK permission hook in itself (hook `allow` doesn't reach `canUseTool`; 0
  developer approvals on a routine task in Mode C).
- **Yes** through the *shape* the Controller imposes on the work: a model with
  no repository access writes a plan of 1–5 tasks and acceptance criteria; each
  task is a separate, one-turn, cold SDK session; the run ends at the first
  `result`; any failure restarts cold; and completion is judged against the
  plan's criteria using a single shell command plus the rule that the agent's
  own report is "a claim, not evidence". When those criteria can't be proven by
  that command, the feature cannot complete — however good the agent's work.
- **And** through two mechanical defects that convert developer attention into
  wasted runs: the 30-minute run timeout that counts time spent waiting for the
  developer, and lexical policy rules that send routine commands to the
  developer.
- A large part of the *observed* slowness had a cause outside orchestration:
  the workspace environment lacked the Rust toolchain (direnv `use flake`
  failing silently without `nix`), which made every independent check fail.
  Orchestration's contribution was to have no way to recognise that.

Evidence summary for this answer: the real feature (legacy_cli) finished its
actual work in 26 min and then spent 3 h 20 min in orchestration and
environment friction [D]; benchmark task 1 — the same one-line fix — took 13–21 s
and one session natively (Mode A, 3 runs) and ended **Failed after 6 sessions** with
the Controller (Mode C), although the independent acceptance check passed on
its result [R]. See [06](06-benchmark-results.md) for all runs.

## 2. Ranked issues

### RC-1 · Unprovable acceptance criteria make verification loop until the budget is gone

- **Observed symptom:** "Not verified yet" repeating; new "Fix what
  verification found" tasks; "Stopped: all 6 attempts are used up" on work
  that is actually correct; repeated test runs.
- **Root cause:** `plan` (no repo access) writes criteria the deterministic
  check can't prove — process rules ("confirmed with the requester before
  implementation begins", "no git commit is created", "no test files are
  edited"). `judge` must mark them met only on "deterministic" evidence and
  treats the agent's report as a claim (`brain.rs:633-636`). Each unmet
  criterion becomes a new task in a new session (`controller.rs:900-937`); the
  loop detector can't see it (it only counts *failed runs* of one task,
  `controller.rs:1565-1581`).
- **Evidence:** [D] ac_3 never met across 3 verifications; [R] Mode C task 1:
  check `cargo test -p otter-core` passed all 3 times (exit 0), criteria 1/3/4 judged
  ✗, 6 runs, feature Failed; independent verification passes. Mode C task 2
  the same — even "`Brief` has `pub fn item_count`" was judged ✗, because the
  judge never sees the code or the diff.
- **Affected:** `brain.rs` (plan, judge prompts), `controller.rs` verify_step.
- **Impact:** **Highest.** Turns successful work into failure; drives repeated
  runs, repeated test executions and "not verified" noise.
- **Confidence:** **Confirmed.**
- **Fix:** criteria must be checkable by the declared check, or explicitly
  marked as judged from the diff/agent report; give the judge the diff and git
  state as evidence; a check that passed twice with no code change since ends
  verification; detect "same verdict, no new evidence" as a loop.
- **Complexity:** Medium.

### RC-2 · One task = one turn = one cold session

- **Symptom:** slow completion; repeated surveying/re-reading; the agent
  "stops" after each step; "the Controller supervises routine details".
- **Root cause:** the plan splits work into 1–5 tasks; each starts
  `Conversation::new` (new SDK session); a run ends at its first `result`
  (`actor.rs:617-626`); fix tasks and failed runs never resume
  (`controller.rs:757-764`).
- **Evidence:** [D] 10 runs = 10 sessions; M1 fully done in task 3 of 5,
  tasks 2/4/5 redundant; [R] Mode C t1: 4 planned tasks + 2 fixes = 6 sessions
  for a one-line change (Mode A: 1 session).
- **Affected:** `controller.rs` (plan → tasks → runs), `runs.rs`, `actor.rs`.
- **Impact:** High — multiplies cost and latency; loses the agent's context;
  splits a coherent change across sessions.
- **Confidence:** **Confirmed.**
- **Fix:** one feature = one SDK session by default; tasks become a checklist
  inside the prompt (or are omitted for small requests); fixes resume the
  session that wrote the code with the failing output as the next turn.
- **Complexity:** Medium.

### RC-3 · The run timeout counts time blocked on the developer

- **Symptom:** "Timed out after 30 min" right after approving; the same
  approval asked again; repeated 30-minute timeouts in the screenshot.
- **Root cause:** `started_at + 30 min < now` (`controller.rs:674`), not
  checked while Blocked, fires on the first step after the answer; stop as
  Failed → fresh run.
- **Evidence:** [D] two runs killed 0.7 s and ~1 s after approvals that came
  after 66 and 91 minutes; [R] E1 reproduces it (approve → run Failed → new run
  → same approval asked).
- **Impact:** High per occurrence — destroys the run's context and the
  developer's approval; burns an attempt.
- **Confidence:** **Confirmed.**
- **Fix:** measure active time only (exclude Waiting/Blocked); interrupt
  before stopping; resume the session after a timeout.
- **Complexity:** Low.

### RC-4 · Verification runs in an environment that can't build the project, and can't tell

- **Symptom:** `cargo test (exit 127)`; "Make cargo available on the
  evidence-run PATH"; fix tasks that can't fix anything.
- **Root cause:** environment resolution: `direnv export json` under the
  non-interactive login shell; `use flake` fails without `nix` but direnv exits
  0 (`environment.rs:51-80`, `env.rs:60-110`). The workspace reports
  `direnv: ready`. The check's failure is fed to the judge like a test failure.
- **Evidence:** [D] 3/3 checks exit 127 `cargo: command not found`; [R]
  reproduced the exact env capture and direnv result; E2 shows any unrunnable
  check burns the budget.
- **Affected:** `environment.rs`, `env.rs`, `controller.rs` verify_step.
- **Impact:** High on this host (it's what exhausted the budget in the real
  feature); environment-specific.
- **Confidence:** **Confirmed** (environment); the general "unrunnable check
  is treated as a code failure" — **Confirmed** [R].
- **Fix:** surface direnv stderr / missing toolchain as an environment error
  at workspace preparation; classify exit 126/127 and "command not found" as
  "check can't run here" → one developer-facing message, not a fix task.
- **Complexity:** Low–Medium.

### RC-5 · Lexical policy rules send routine commands to the developer

- **Symptom:** many approvals; low-risk commands (`ls`, `cat .envrc`, logs in
  `/tmp`) needing the developer.
- **Root cause:** `.env` matched as a substring (`.envrc`); any `>` outside
  the workspace (incl. `/tmp`); whole-word `rm` (v0.6.19; fixed by uncommitted
  D-062); no approval caching.
- **Evidence:** [C] re-classification of every developer-only ask in the
  record against v0.6.19's policy; [D] ≥3 `.envrc`, 5 `/tmp`, 1 `rm` asks.
- **Impact:** Medium–High — each one is a developer interruption and, with
  RC-3, can cost the run.
- **Confidence:** **Confirmed.**
- **Fix:** policy precision (§6 of [03](03-approval-analysis.md)) + per-run
  caching of identical approvals.
- **Complexity:** Low.

### RC-6 · The developer's intent was discarded; the plan instructed the agent to ask

- **Symptom:** "The feature request for Otter came through empty. What should
  this feature do?" after execution started; a second scope question later.
- **Root cause:** empty request accepted at Start; the developer's first
  message is skipped as "the request" by `brief`, `unanswered` and
  `developer_notes`; the plan turned the gap into a "confirm with the
  requester" task and criterion.
- **Evidence:** [C] `features.rs:664-689`, `brain.rs:491-506`,
  `controller.rs:1522, 1637`; [D] full chain in [05](05-conversation-attention.md) §5.
- **Impact:** Medium (one-time per feature, but it seeds RC-1).
- **Confidence:** **Confirmed.**
- **Fix:** use the first message as the request; validate at Start; Otter
  asks before planning, not the agent mid-run.
- **Complexity:** Low.

### RC-7 · The Controller can deny developer-only requests, in the developer's name

- **Evidence:** [C] `policy.rs:579-581`, `runs.rs:943-945`; [D] 3 denials incl.
  an empty untracked file "a core source file" and the run's own test command.
- **Impact:** Medium — the agent believes the developer said no.
- **Confidence:** **Confirmed.**
- **Fix:** developer-only means developer-only: skip the brain; attribute and
  explain every denial.
- **Complexity:** Low.

### RC-8 · Background work is cut off at the first `result`

- **Evidence:** [C] `actor.rs:617-626`, `map.ts:103-113`, SDK typings; [D]
  `run_5e6e0ssfr0` ("I'll wait for it") → Done.
- **Impact:** Medium — tasks marked Done early; long test runs lost.
- **Confidence:** **Strong evidence** (code + one observation on legacy_cli;
  not reproduced on sdk).
- **Fix:** map the SDK's background-task and `session_state_changed` messages;
  end the run on `idle` with no background tasks.
- **Complexity:** Medium.

### RC-9 · Unrecognised SDK built-in tools need a model decision

- **Evidence:** [C] 18 mapped names vs the SDK tool list; `Other` → Ask.
- **Impact:** Potentially high on long runs (background polling, task lists);
  not observed in the benchmark.
- **Confidence:** **Hypothesis** for impact (code-confirmed mechanism).
- **Fix:** classify the SDK's built-in tools explicitly; test against the
  pinned SDK's tool list.
- **Complexity:** Low.

### RC-10 · Presentation noise (separate from execution)

- **Symptoms:** "too many low-value progress messages", "Read: read" records.
- **Root causes:** every agent text block → a conversation message
  (`runs.rs:388`); legacy_cli records policy allows as decisions; activity
  lines in the main column; duplicate summaries; the escalate/unblock flicker.
- **Evidence:** [C][D][R] ([05](05-conversation-attention.md)).
- **Execution impact:** small (feature-store writes on the bridge's path;
  unmeasured). **User impact:** high — it's most of what the developer sees.
- **Confidence:** **Confirmed.**
- **Complexity:** Low.

## 3. Symptoms → causes

| Symptom (from the plan) | Root causes | Not the cause |
|---|---|---|
| Too many low-value progress messages | RC-10 | model call frequency |
| Too many approvals and decisions | RC-5, RC-7, RC-6 (questions), RC-9 (potential) | SDK hook design itself |
| Slow task completion | RC-1, RC-2, RC-3, RC-4 | worker start-up (≈1.5 s), Controller tick |
| Repeated tool execution and timeout loops | RC-1, RC-4 (repeated checks); RC-3 (timeouts); prompts demanding full tests on every task | — |
| Agents stop or wait instead of recovering | RC-2 (run ends at first result), RC-8, RC-6 (plan tells it to ask) | the agent: it recovered on its own when allowed (R-7 in [04](04-recovery-analysis.md)) |
| Controller supervises routine details | RC-2 (planning decomposition), RC-1 (judging process criteria) | — |
| User must manage instead of delegate | all of the above; RC-3 + RC-5 especially | — |

## 4. What was *not* found

- No evidence that the SDK integration itself (worker, hook, `canUseTool`,
  session persistence) reduces autonomy beyond RC-8/RC-9 and an unmeasured
  per-call round trip. The structured runtime is sound; the problems sit in how
  the Controller uses it.
- No evidence of excessive model-call *frequency*.
- No evidence that Claude Code's own permission system added prompts on the
  SDK path (0 `canUseTool` calls with an allowing hook).
