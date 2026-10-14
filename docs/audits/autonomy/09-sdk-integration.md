# 09 — The Claude Agent SDK integration

Part of the [autonomy audit](README.md) (added by plan revision 2). Scope:
`packages/claude-runtime/` and `crates/daemon/src/agents/claude_sdk.rs`, plus
the parts of the runtime the `sdk` backend goes through. The SDK integration is
deliberate and is **not** proposed for replacement; the question is where Otter
adds overhead or removes autonomy the SDK would otherwise give Claude.

Labels: [C] code or SDK typings · [R] reproduced (benchmark/experiment) ·
[D] dogfooding record · [I] inference. All files in scope are byte-identical
between v0.6.19 and HEAD; the installed worker
(`~/.local/lib/otter/claude-runtime/0.6.19`) matches the source. SDK:
`@anthropic-ai/claude-agent-sdk` 0.3.285 (`sdk.d.ts`, `sdk-tools.d.ts`).

## 0. Is the SDK backend actually in use?

**Not by default.** `Settings::coding_backend()` falls back to `legacy_cli`
(`settings.rs:153-159`), and the developer's host has no `coding_backend`
setting, so the real dogfooding feature ran on `legacy_cli` [C][D]. The
"primary structured backend" is opt-in today. This matters for reading every
observed symptom: the 83 policy-approval records, the `--setting-sources ""`
behaviour and the "only what Claude Code asks about" pipeline are legacy_cli
traits; the structural findings (one-turn runs, fresh sessions, run timeout,
verification loop) are shared and were reproduced on `sdk` [R].

## 1. Worker lifecycle

- **One Node worker per run** (`claude_sdk.rs:3-4`), spawned in its own
  process group with `kill_on_drop` (`claude_sdk.rs:297-310`). [C]
- Start-up: spawn `node dist/main.js` → `initialize` (`claude_sdk.rs:391-403`)
  → `sdkQuery({prompt: InputQueue, options})` (`sdk.ts:110`) → the SDK spawns
  its bundled Claude Code → `initializationResult()` (`sdk.ts:133`) → `ready`
  (`main.ts:119-126`) → the actor sends the first turn (`actor.rs:247`). [C]
- **Measured cost [R]:** claim → turn delivered **1.2–1.6 s**, claim → first
  tool call **2.3–2.9 s**, across 10 SDK runs in the benchmark
  ([scripts/run_startup.py](scripts/run_startup.py)). legacy_cli is the same
  order (1.1–1.3 s / 2.2–3.4 s) [D]. **Worker start-up is not a meaningful
  overhead**; what costs is how many runs there are and that each is a new
  session (§2).
- Init timeouts disagree: worker 60 s (`main.ts:16`) vs otterd 90 s
  (`claude_sdk.rs:59-67`) and D-057's 90 s. Low impact. [C]

## 2. Runs and turns

- A turn ends at the SDK's `result` (`map.ts:179-216`). A run ends at the
  first `TurnFinished` with nothing queued: the actor stops the worker
  (`actor.rs:617-626`), the worker ends input, waits 5 s, aborts
  (`sdk.ts:220-229`), and otterd SIGKILLs the group (`claude_sdk.rs:689-708`). [C]
- The controller starts each run with exactly one prompt (`runs.rs:169-187`);
  further turns only come from developer messages. **So in practice
  run = turn = task**, and a feature with N tasks costs N fresh SDK sessions.
  [C] Benchmark Mode C, task 1: **6 runs, 6 sessions, 6 turns** for a one-line
  fix [R]; Mode A did it in **1 session, 1 `query`** (6 model round trips
  inside it) [R].
- Resume is by explicit session id and only for the same task after
  Cancelled/HandedOff/Interrupted (`controller.rs:757-764`). [C] **Resume
  frequency in all measured runs: 0** (no run was paused or interrupted) [R][D].

### Background work after `result`

The SDK typings describe work that continues after a turn's `result`:
`task_notification` (`sdk.d.ts:5927-5945`), `session_state_changed: 'idle'` as
the "authoritative turn-over signal" (`sdk.d.ts:5793-5799`), results "held
back while background work finishes" (5657), turns the CLI starts itself on a
"background task notification or auto-continuation" (5381), and Stop-hook
`background_tasks` to tell "done" from "paused waiting for background work"
(9482). [C]

The worker's mapper handles only `system/init` (`map.ts:103-113`); a `result`
with no Otter turn running becomes a debug-level notice (`map.ts:180-182`,
`claude_sdk.rs:793-796`); text/tool events with no current turn are ignored
(`actor.rs:389-391, 425-427, 452-454`). Combined with stopping the worker at
the first `result`, **anything Claude planned to do after a background task
finishes is cut off, and the task is recorded Done**. [C] The record shows the
pattern on legacy_cli (`run_5e6e0ssfr0`) [D]. Not reproduced on `sdk` in the
benchmark (no run used background work) [I].

## 3. Permission pipeline: `PreToolUse → ToolCheck → policy → canUseTool → DecisionNeeded`

| # | Hop | Blocks the tool call? |
|---|---|---|
| 1 | SDK fires the `PreToolUse` callback for every tool (`sdk.ts:99`) | yes |
| 2 | worker → `policy_check` → otterd `ToolCheck` (`claude_sdk.rs:826-831`) | yes |
| 3 | reader → channel → actor → `ActorEvent::Check` → bridge, which handles events **one at a time** (`runs.rs:370`) | yes |
| 4 | `policy::classify` (`runs.rs:430`) | yes (µs) |
| 5 | `policy_reply` allow / deny / ask (`claude_sdk.rs:668-678`) | yes |
| 6 | ask → Claude Code calls `canUseTool` → `permission_request` → `DecisionNeeded` (`sdk.ts:171-173`, `claude_sdk.rs:832-855`) | yes |
| 7 | actor journals the interaction (fsync); bridge **classifies again** (`runs.rs:403`); `record_ask` writes a Pending decision | yes |
| 8 | controller `decide_open` → `brain.decide` (model, ≤300 s) or developer | yes, unbounded |
| 9 | `resolve` → `forward_decision` → `canUseTool` returns | — |

Findings:

- **Hook `allow` skips `canUseTool`** — no double prompt. [R] Mode A: 5 and 6
  hook calls, 0 `canUseTool` calls; the worker README reports the same
  (`README.md:41-42`).
- **Policy allows are not recorded** as decisions on `sdk` (`runs.rs:432`) —
  unlike legacy_cli. [C][R] (Mode C: 0 decision records.)
- **Every tool call — Read, Grep, TodoWrite included — waits for an otterd
  round trip**; the bridge may be busy rewriting `feature.json` (with fsync,
  under the global features mutex) for the preceding tool-start or text event
  (`runs.rs:387-400, 843-887`, `features.rs:298-310`). [C] Round-trip latency
  is **not instrumented**; per-run turn times in Mode C (6–13 s for 2–5 tool
  calls) leave little room for a large per-call cost [R/I].
- **No caching**: `canUseTool` ignores the SDK's `suggestions` /
  `updatedPermissions` (`sdk.ts:180-187`; `sdk.d.ts:216-224`). [C]
- **`classify` runs twice** on the ask path. Harmless. [C]
- **Unrecognised built-in tools need a decision.** Otter maps 18 tool names
  (`claude_stream.rs:583-625`); the SDK's own tool list (`sdk-tools.d.ts:11-55`)
  includes `TaskCreate/Get/Update/List`, `TaskStop`, `Monitor`, `CronCreate/…`,
  `ScheduleWakeup`, `EnterPlanMode`, `Workflow`, `EnterWorktree`, MCP resource
  tools; plus `BashOutput`/`KillShell`/`TaskOutput`, `Skill` and `mcp__*`. All
  become `Other` → Ask Medium (`policy.rs:559-564`) → a model call, or with
  the `rules` brain a developer block. [C] Not triggered in the benchmark
  runs [R], so the frequency in real work is unknown [I] — but background-task
  polling (`BashOutput`/`TaskOutput`) is exactly what long test runs use.
- **Safety (out of scope, reported):** if `record_ask` fails to persist, the
  bridge answers Allow (`runs.rs:407-411, 678-681`) — an Ask, even a
  developer-only one, becomes an Allow on a store error. [C]

## 4. What Otter injects

| Text | Where |
|---|---|
| System-prompt append: "You are working on one task of a feature, managed by Otter. Stay inside the working directory. Run the project's tests before you finish. End with a short summary…" | `runs.rs:1334-1336` → `sdk.ts:93` |
| Task prompt: title, request, **all** requirements and criteria, "Your task (n of N)", previous failure, **all** developer notes, "The work is checked with `cmd`; run it before you finish." | `controller.rs:1650-1695` |
| Resume: "Continue the task where you left off." (+ `CUT_OFF`) | `controller.rs:1698-1716` |
| Every denial not from policy: "The developer declined this." (also when the controller denied) | `runs.rs:943-945` |
| Hook deny reason; "Otter couldn't check this tool call (the turn ended)."; "The request was cancelled before anyone answered." | `sdk.ts:157, 165, 178` |

Issues: "Stay inside the working directory" is stricter than policy (reads
outside are allowed); "run the tests" appears twice; misattributed denials
[C]. `disallowedTools: ["Task","Agent"]` removes subagents (D-059, temporary)
— no parallel exploration, all search output in the main context [C].
`settingSources: ["project"]` loads the repo's CLAUDE.md but not the
developer's user settings, skills or MCP servers [C] — deliberate isolation.

## 5. Interruptions of an SDK turn

| Path | Mechanism | Session afterwards |
|---|---|---|
| End of turn | `handle.stop()` → shutdown → group SIGKILL | resumable, but the controller starts the *next task* fresh |
| Interrupt / pause / cancel | `q.interrupt()` with a 10 s deadline (`actor.rs:72-81`) | Cancelled → resumed later |
| Redirect | interrupt, next turn in the **same** worker | the only path that keeps the worker |
| Run timeout (30 min wall clock, incl. waiting) | `stop_runs(Failed)`, **no interrupt first** | **never resumed** |
| Controller reads a message as pause/finish/revise | `hold_runs` | Cancelled |

Measured: **0 controller-induced interrupts** in the SDK benchmark runs [R];
the run timeout killed 2 runs in the dogfooding record [D] and 1 in E1 [R].

## 6. Limits passed to the SDK

None: `RunLimits::default()` (`runs.rs:216`) → `maxTurns`/`maxBudgetUsd`
stripped (`claude_sdk.rs:424-429`). **Keep it that way** — Claude's own loop
inside a turn is uncapped. Otter's limits act *between* turns (§5). No limit
looks at cost (`cost_usd` has no consumer in the daemon). [C]

## 7. Metrics available from Otter's state

Kept: `total_cost_usd` (session-cumulative) in `Op::Finished`; per-turn
`queued_at`/`started_at`/`ended_at`; tool records; interactions; worker, SDK
and Node versions in the run's `Claim`. Dropped: `usage`, `modelUsage`,
`num_turns`, `duration_ms`, `duration_api_ms`, `permission_denials`,
`stop_reason` and every non-`init` system message (`map.ts:203-210`,
`claude_sdk.rs:862-867`). [C] So Otter can't report SDK-internal round trips
or tokens; the benchmark reads them from Mode A's SDK messages directly.

## 8. Findings

| # | Finding | Severity | Status |
|---|---|---|---|
| S-1 | run = turn = task: each Otter task is a cold SDK session; the agent's own multi-step plan can't cross Otter's task boundaries | **High** | Confirmed [C][R] |
| S-2 | Worker stopped at the first `result`; background-task follow-ups dropped; task recorded Done | **High** | Confirmed in code [C]; observed on legacy [D]; not reproduced on sdk [I] |
| S-3 | 30-min wall-clock run timeout incl. time waiting for decisions; fresh restart | **High** | Confirmed [C][D][R] |
| S-4 | Unrecognised SDK built-in tools → model decision / developer block | **High** potential | Confirmed in code [C]; frequency unknown [I] |
| S-5 | Every tool call round-trips to otterd, possibly behind feature-store fsyncs | Medium | Code [C]; latency unmeasured |
| S-6 | No approval caching | Medium | [C] |
| S-7 | Denials attributed to the developer | Medium | [C][D] |
| S-8 | Subagents off | Medium (deliberate) | [C] |
| S-9 | Failed `record_ask` write turns Ask into Allow | Medium (safety) | [C] |
| S-10 | Per-turn metrics dropped | Low (observability) | [C] |
| S-11 | `sdk` is not the default backend | Medium (product) | [C][D] |
| S-12 | Worker start-up | **Not an issue** (≈1.5 s) | Measured [R] |

## 9. Keep

- The documented SDK surface only: `query()` with streaming input, the
  `PreToolUse` hook + `canUseTool`, `resume` by explicit id, the bundled Claude
  Code pinned, `settingSources: ["project"]`.
- No SDK turn/budget caps.
- Deterministic policy that allows routine work without a model or a human.
- Fail-closed waits (a cancelled wait is a denial).
- One turn at a time, journal-before-act, idempotent receipts (D-056, D-058).
- Resume by native session id after pause, restart or crash, with the
  cut-off notice (D-061).
- Redirect in the same worker.
- Bounded frames, drained stderr, process-group cleanup.
