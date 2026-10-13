# 08 — Remediation roadmap

Part of the [autonomy audit](README.md). Nothing here is implemented by the
audit. Every item keeps the SDK integration, structured events, approval
interception and session persistence (plan revision 2), and prefers
simplifying existing components over adding new ones.

**How improvement is measured, for every item:** rerun
[scripts/bench.sh](scripts/bench.sh) Modes B and C on the benchmark tasks and
[scripts/feature_metrics.py](scripts/feature_metrics.py) on real features.
Headline metrics: completion (Otter's verdict *and* the independent check),
SDK sessions per feature, wall time, developer approvals, fix tasks created,
share of time Blocked.

## Target shape (simplify, don't add)

```
Developer
   │
   ▼
Feature (request validated)  ── Attention: only pending decisions, failures, results
   │
   ▼
Controller = Task Supervisor:  plan once (optional checklist), verify, enforce limits
   │        no task decomposition into sessions; no model decisions on developer-only asks
   ▼
ONE SDK session per feature  ◄── fixes and timeouts resume it with the evidence as a new turn
   │   PreToolUse → policy (precise) → allow | deny | ask (cached per run)
   ▼
Tools / repository (workspace environment checked at preparation)
```

This is the plan's proposed "Task Manager / Supervisor / Coding Agent /
Native runtime" shape, reached by **removing** behaviour from the existing
Controller (decomposition into sessions, process criteria, model vetoes) rather
than adding a layer.

## P0 — Autonomy and execution reliability

### P0-1 · Verification that can finish (RC-1, RC-4)

- **Change:** (a) `plan` must express criteria as either *checked by the
  declared command* or *judged from the diff*; drop process criteria about
  conversation history. (b) `judge` receives `git diff --stat`, the diff
  (bounded), `git status` and HEAD as deterministic evidence. (c) A check
  exiting 126/127 or printing "command not found" is recorded as *the check
  can't run here* → one developer-facing message, no fix task. (d) Same check
  outcome + same verdict with no new commits since the last pass → stop
  verifying (loop).
- **Benefit:** correct work completes; no repeated test runs for nothing.
- **Evidence:** [D] ac_3; [R] Mode C t1 Failed with a passing independent
  check; E2.
- **Files:** `brain.rs` (plan/judge prompts, `rules_judge`),
  `controller.rs` (`verify_step`, `run_check`, `repeating_failure`).
- **Complexity:** Medium. **Regression risk:** accepting work too easily —
  keep "deterministic check failed ⇒ not met"; keep the developer's Accept.
- **Acceptance:** benchmark t1/t2 Mode C reach Review with the independent
  check passing; E2 stops after 1 verification with "the check can't run".

### P0-2 · One SDK session per feature; fixes resume it (RC-2)

- **Change:** start one conversation per feature; plan tasks become a
  checklist in the first prompt (and are skipped for small requests); the next
  task, a fix, or a continuation is a **new turn in the same session**
  (`resume` by the bound session id). Keep D-056 (one turn at a time) and D-061
  (cut-off notice).
- **Benefit:** the agent keeps its context and its own plan; fewer cold starts
  and re-reads.
- **Evidence:** [D] 10 sessions; [R] 6 sessions vs 1.
- **Files:** `controller.rs` (`implement_step`, `task_prompt`,
  `continue_prompt`), `runs.rs` (`start_run`), `core/feature.rs` (task ↔ run).
- **Complexity:** Medium. **Risk:** very long sessions hit context limits — the
  SDK compacts; keep the option to start fresh after N turns or on developer
  request.
- **Acceptance:** Mode C t1/t2 use 1 session; sessions per feature ≤ 2 on real
  features.

### P0-3 · Run timeout counts active time only (RC-3)

- **Change:** subtract time in Waiting/Blocked; interrupt before stopping;
  after a timeout, resume the same session ("you ran out of time on X") instead
  of a fresh run; check the timeout while Blocked only for *active* time.
- **Files:** `controller.rs:61-63, 671-682`, `runs.rs` stop paths, `core/feature.rs`
  (accumulated active time).
- **Complexity:** Low. **Risk:** none known.
- **Acceptance:** E1 with a 3 s timeout and a 5 s wait completes the approved
  command; a new e2e test asserts it.

### P0-4 · Workspace environment health (RC-4)

- **Change:** at workspace preparation, keep direnv's stderr; when it
  reports `command not found` (or the export lacks what the `.envrc` should
  add), mark the environment `degraded` with the message, visible in the app
  and the feature. Optionally resolve with `$SHELL -l -i` as a fallback.
- **Files:** `environment.rs`, `env.rs`, `workspaces.rs`.
- **Complexity:** Low–Medium. **Risk:** interactive shells printing noise
  (already handled by markers in `capture_login_env`).
- **Acceptance:** on this host, the `otter` workspace shows "nix not found"
  instead of `ready`.

## P1 — Approval and recovery

### P1-1 · Policy precision + approval caching (RC-5)

- **Change:** `.env` matches only `.env` and `.env.*` file names, not `.envrc`
  (and reading a `.envrc` is not a credentials access); writes under `$TMPDIR`,
  `/tmp` and the workspace's `target/` are inside the boundary; ship D-062.
  Cache an approval per (run, normalised command) — or return the SDK's
  `updatedPermissions` for the session — so the same thing isn't asked twice.
- **Files:** `runtime/policy.rs`, `sdk.ts` (`canUseTool`), `runs.rs`.
- **Complexity:** Low. **Risk:** loosening the boundary — keep `$HOME` writes,
  credentials files, production and destructive Git interactive.
- **Acceptance:** replaying every developer-only ask from the dogfooding record
  through the new policy leaves only the Claude-memory write and `git push`
  asking.

### P1-2 · Developer-only means developer-only (RC-7)

- **Change:** skip `brain.decide` for `user_only` decisions (as `review_step`
  already does); every denial tells the agent who denied it and why.
- **Files:** `controller.rs` (`decide_open`, `escalate`), `runs.rs:943-945`.
- **Complexity:** Low. **Acceptance:** a test where a model brain is asked
  about a developer-only request asserts it never sees it.

### P1-3 · Request validation and intent (RC-6)

- **Change:** an empty request takes the first developer message; if there is
  none, Otter asks one question *before planning*. The plan never instructs
  the agent to ask the developer.
- **Files:** `features.rs` (Start), `brain.rs` (`brief`), `controller.rs`
  (`unanswered`, `developer_notes`).
- **Complexity:** Low.

### P1-4 · Keep the session alive for background work (RC-8)

- **Change:** map `task_started`/`task_notification`/`session_state_changed`
  in the worker; end the run on `idle` with no background tasks (with a grace
  limit); declare `perTaskStopAffordance` so an interrupt doesn't kill
  background tasks.
- **Files:** `packages/claude-runtime/src/map.ts`, `sdk.ts`, `actor.rs`.
- **Complexity:** Medium. **Risk:** runs that never go idle — bound by the
  active-time limit.
- **Acceptance:** a probe run that starts `sleep 20 && echo done` in the
  background and says "I'll wait" ends only after `done`.

### P1-5 · Classify the SDK's built-in tools (RC-9)

- **Change:** map `BashOutput`, `KillShell`, `TaskOutput`, task-list tools,
  `Monitor`, `Skill`, `EnterPlanMode`, … explicitly; a unit test fails when the
  pinned SDK's tool list has an unclassified tool.
- **Files:** `agents/claude_stream.rs` (`tool_call`), `runtime/policy.rs`.
- **Complexity:** Low.

### P1-6 · Make `sdk` the default backend, or say why not (S-11)

- The audit's SDK findings only matter if the SDK runs. Decide and record it
  (decisions.md).

## P2 — Conversation and progress presentation (RC-10)

- **P2-1** Conversation = developer, Otter, and one agent result per run;
  intermediate agent text stays transient (D-051) and in the Work tab
  (`runs.rs:388`).
- **P2-2** Activity lines as transient events, not `feature.json` rewrites
  (`runs.rs:874-887`) — also removes fsyncs from the policy-reply path.
- **P2-3** legacy_cli: policy allows are tool records, not decisions
  (`runs.rs:580-653`).
- **P2-4** Fix `escalate` (re-check pending) — removes the flicker.
- **P2-5** Deduplicate run summaries by normalised text.
- **Acceptance:** on a replay of the dogfooding feature, the main column holds
  ≤ 15 items instead of 69; the Timeline has no "Decision needed … low risk".
- **Complexity:** Low each. **Risk:** losing detail developers want — it stays
  in the Work tab and the conversation journal.

## P3 — Advanced supervision (only where benchmarks show a need)

- Permission round-trip and worker timing instrumentation (keep `num_turns`,
  `duration_ms`, `usage` in `Op::Finished`) — needed before optimising S-5.
- Cost-aware limits (no limit reads `cost_usd` today).
- Subagents (D-059) once lineage and policy are handled.
- Re-run the benchmark with ≥5 tasks × ≥3 repetitions per mode before adding
  any new supervision.

## Order of work

| Step | Items | Why first |
|---|---|---|
| 1 | P0-3, P1-2, P1-1 | Low complexity, directly stop wasted runs and false denials |
| 2 | P0-1, P0-4 | Make completion possible and environment failures visible |
| 3 | P0-2 | The biggest structural change; needs P0-1's verification to resume into |
| 4 | P1-3, P1-4, P1-5, P1-6 | Remove the remaining autonomy breaks |
| 5 | P2-* | Presentation, once execution is right |
| 6 | P3 | Only with benchmark evidence |
