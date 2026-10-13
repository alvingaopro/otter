# Agent autonomy audit — executive summary

**Question:** does Otter's orchestration (the Controller, the managed runtime
around the Claude Agent SDK, approvals, attention) reduce the native autonomy
of the coding agent, and what should change first?
**Plan:** [`docs/plans/agent-autonomy-audit.md`](../../plans/agent-autonomy-audit.md)
(revision 2: the Claude Agent SDK is the primary backend and stays).
**Date:** 2026-10-13. **Code:** `roadmap-m1` @ `5ebcf4f` and production
otterd **0.6.19**. **No production code was changed.**

## Evidence base

| Source | What it gives | Label |
|---|---|---|
| Code reading (controller, brain, runs, runtime, policy, worker, SDK typings, desktop app) | mechanisms, with file:line | [C] |
| The real dogfooding feature on this host (`~/.otter/state`, feature "Otter", 3 h 48 min, 10 runs, 409 history events) — **ran on `legacy_cli`**, the default backend | what actually happened | [D] |
| Three deterministic experiments with the e2e harness (fake agent) | E1 run-timeout, E2 unrunnable check, E3 decision noise | [R] |
| A small live benchmark: direct SDK (A) vs Otter SDK runtime without a model controller (B) vs with it (C), same SDK 0.3.285, same tasks | measured overhead | [R] |

Single runs per mode: **diagnostic, not statistically strong**. See
[06](06-benchmark-results.md) for what was and wasn't measured.

## The five questions

### 1. Why does Otter feel less autonomous than native Claude Code?

Because the Controller reshapes the work, not because it talks too much.
A model with no access to the repository plans 1–5 tasks and acceptance
criteria; **each task runs as a separate, one-turn, cold SDK session**; the
run ends at the agent's first `result` (background work is cut off); any
failure restarts from scratch; and completion is judged against the plan's
criteria with a single shell command, the agent's own report counting as "a
claim, not evidence". When a criterion can't be proven by that command
("no commit was made", "confirmed with the requester"), verification loops
until the attempt budget is gone — **even when the work is correct**.

Measured: the same one-line fix took **13–21 s and 1 session** through the SDK
directly (3 runs), and ended **Failed after 6 sessions** under the Controller while the
independent acceptance test passed on its result ([06](06-benchmark-results.md)).
In the real feature the requested work was done **26 minutes** in; the next
**3 h 20 min** were orchestration and environment friction, 85 % of it blocked
on the developer ([01](01-execution-architecture.md) §6).

### 2. Which components introduce the most overhead?

| Component | Overhead | Not overhead |
|---|---|---|
| Controller: plan → tasks → sessions; `judge` against plan criteria | **most** — sessions × N, verification loops, failures on correct work (RC-1, RC-2) | model-call *frequency* (~4 per small feature) |
| Controller: 30-min run timeout incl. waiting for the developer | kills the run the moment the developer approves (RC-3) | — |
| Policy (`runtime/policy.rs`) | lexical rules send routine commands to the developer (RC-5) | the deterministic allow path |
| Workspace environment (direnv/`nix`) | toolchain silently missing → every check exit 127 (RC-4) | — |
| SDK worker / permission hook | small: ≈1.5 s start-up per run; one otterd round trip per tool call (unmeasured, likely small) | hook `allow` never reaches `canUseTool` |

### 3. Why does Otter request so much user attention?

- **Real interruptions** (the feature Blocked 20 times): policy precision —
  `.env` matching `.envrc`, any redirect to `/tmp`, `rm` inside the workspace
  (fixed in HEAD, unreleased) — and no approval caching (RC-5).
- **Questions the system caused:** the developer's first message was discarded
  for an empty request; the plan then told the agent to "confirm with the
  requester" — twice (RC-6).
- **Approvals that bought nothing:** two approvals given after long waits were
  followed by the run being killed for exceeding 30 minutes, and the next run
  asked again (RC-3).
- **Noise that looks like attention** (no execution cost): 83 policy
  auto-approvals shown as decisions on legacy_cli, every agent text block as a
  conversation message, duplicate summaries, a Blocked/unblocked flicker
  ([05](05-conversation-attention.md)).

### 4. The three highest-impact improvements

1. **Verification that can finish** (P0-1): criteria checkable by the declared
   check or judged from the diff; the judge sees the diff and git state;
   "command not found" is an environment problem, not a fix task; stop when
   the same evidence repeats.
2. **One SDK session per feature; fixes and continuations resume it**
   (P0-2): tasks become a checklist in the prompt, not separate cold sessions.
3. **A run timeout that counts only active time, and resumes instead of
   restarting** (P0-3) — together with **policy precision + approval caching**
   (P1-1) it removes almost every wasted developer interaction in the record.

Full roadmap with acceptance criteria: [08](08-remediation-roadmap.md).

### 5. What should be simplified or removed?

- **Remove** task-to-session decomposition and model-planned process criteria
  from the Controller; keep it as a supervisor that plans once (optionally),
  verifies, and enforces limits.
- **Remove** model decisions on developer-only requests (the model can only
  deny, and does so in the developer's name).
- **Remove** per-text-block and per-tool feature writes from the conversation
  (keep them in the journal / Work tab).
- **Simplify** the policy's word lists into precise rules plus a per-run
  approval cache.
- **Keep** the SDK integration as is: the hook + `canUseTool` pipeline,
  journaling, resume-by-session, fail-closed waits, no SDK turn caps
  ([09](09-sdk-integration.md) §9).

## The central hypothesis

**"Otter's orchestration layer reduces the native autonomy of Claude Code":
Yes, but only in specific scenarios** (plan option 2) — task decomposition
into sessions, unprovable verification, the run timeout, and policy
precision. The SDK runtime itself is not the problem; worker start-up and
model-call frequency are not material. A significant share of the observed
slowness came from the development environment (missing toolchain), which the
Controller could neither detect nor recover from.

## Documents

| # | Document | Contents |
|---|---|---|
| 01 | [Execution architecture](01-execution-architecture.md) | actual sequence, responsibility matrix, intervention inventory, where time went |
| 02 | [Controller analysis](02-controller-analysis.md) | wake-ups, every model call, session vs execution continuity, prompts |
| 03 | [Approval analysis](03-approval-analysis.md) | pipeline, why commands reached the developer, decision matrix, levels |
| 04 | [Recovery analysis](04-recovery-analysis.md) | every timeout, findings R-1–R-7, scenarios, responsibility matrix |
| 05 | [Conversation & attention](05-conversation-attention.md) | message taxonomy, noise counts, attention semantics, the empty request |
| 06 | [Benchmark results](06-benchmark-results.md) | experiments E1–E3, Modes A/B/C, raw numbers, reproduction |
| 07 | [Root cause analysis](07-root-cause-analysis.md) | RC-1–RC-10 ranked, symptoms → causes, the hypothesis |
| 08 | [Remediation roadmap](08-remediation-roadmap.md) | P0–P3 with benefit, evidence, files, risk, acceptance |
| 09 | [SDK integration](09-sdk-integration.md) | worker lifecycle, runs/turns, permission pipeline, injections, keep-list |
| — | [scripts/](scripts/) · [experiments/](experiments/) | metrics extractors, benchmark harness, experiment source and output |

## Limitations

- One run per task per mode; two benchmark tasks (the plan asks for ≥5 × several).
- The real feature ran on `legacy_cli`; SDK conclusions rest on code and the benchmark.
- Permission round-trip latency and SDK-internal tokens were not instrumented
  (would need production changes).
- Mode B's whole-workspace check is sensitive to the benchmark's own nested
  environment (see [06](06-benchmark-results.md) §4); Mode B numbers show the
  mechanism, not a clean duration.
- During the first Mode B run, a nested e2e test opened GitHub's device-login
  page in the developer's real Otter app through an inherited browser
  stand-in ([04](04-recovery-analysis.md) R-6); later runs mocked the browser.
