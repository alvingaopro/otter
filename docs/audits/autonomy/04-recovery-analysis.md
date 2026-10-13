# 04 — Timeouts, recovery and loops

Part of the [autonomy audit](README.md). Labels: [C] code · [R] reproduced ·
[D] dogfooding record (legacy_cli) · [I] inference.

## 1. Every timeout and limit

| Mechanism | Owner / where | Default · knob | Trigger | On expiry | Cleanup | Recovery |
|---|---|---|---|---|---|---|
| **Run timeout** | controller `controller.rs:61-63, 674-682` | **30 min** · `OTTER_RUN_TIMEOUT_MS` | wall clock since `run.started_at`, **including time blocked on decisions**; only checked in `implement_step`, i.e. not while Blocked | `stop_runs(Failed, "Timed out after 30 min")`, no interrupt first | worker shut down, process group SIGKILLed; pending decisions cancelled | task Failed, attempt spent, **next run fresh** (Failed runs are never resumed, `controller.rs:757-764`) |
| Active-time budget | `controller.rs:689-697`, `feature.rs:547-549` | 120 min · `budget.max_minutes` | `active_since` + 120 min, checked only with no live run; **restarts on every unblock** | feature Failed `LimitReached time` | none | developer retry |
| Fresh-run budget | `feature.rs:384`, `runs.rs:244-253` | 6 per feature | each non-resume run | feature Failed `attempts` | — | retry resets to 0 |
| Task attempts | `controller.rs:41` | 3 | Failed task re-picked | task stays Failed | — | — |
| Loop detector | `controller.rs:43, 1565-1581` | 3 | last 3 runs **Failed**, same task, same summary | feature Failed `loop` | — | — (never fires on check failures: each fix is a new task) |
| Verify check | `controller.rs:66-68, 1718-1764` | 10 min · `OTTER_CHECK_TIMEOUT_MS` | `sh -c cmd` | failed evidence "timed out" | `kill_on_drop` on `sh` only — **no process group**, grandchildren (`cargo`, test binaries) may survive [I] | judge → fix task |
| Interrupt deadline | `actor.rs:72-81, 262-270` | 10 s · `OTTER_INTERRUPT_DEADLINE_MS` | interrupt not settled | run treated as lost (`outcome_unknown`) | stop | D-061: Interrupted → resumed, or RecoveryRequired |
| Worker init | `main.ts:16` (60 s) vs `claude_sdk.rs:59-67` (90 s) | `OTTER_WORKER_INIT_TIMEOUT_MS` | SDK `initializationResult` | `fatal` "Claude didn't start" | group kill | run failed |
| Worker shutdown | `sdk.ts:220-229`, `main.ts:17`, `claude_sdk.rs:689-708` | 5 s / 8 s | end of every run | abort, then SIGKILL group | **kills background jobs** | — |
| Brain calls | `claude_stream.rs:451-456`, `providers.rs:444-448` | 300 s | model call | planning/judging failure **fails the feature**; decide escalates; reply drops intent | — | developer retry |
| Permission wait (`canUseTool`) | none (`interactions.ts`) | ∞ | — | — | — | **[D]** Claude Code waited 66 and 91 min and delivered the answer fine |
| Tool / shell / test commands | Claude Code's own Bash timeout | Claude Code default | — | — | — | the agent's |
| Session reconciliation | `reconcile.rs` | 500 ms | terminal sessions only; managed runs are not reconciled there | — | — | `recover_runs` at daemon start |

## 2. Findings

### R-1 · The run timeout kills runs at the moment the developer approves — **Confirmed** [C][D][R]

- Code: the check `run.started_at + run_timeout() < now` (`controller.rs:674`)
  has no notion of waiting; `step` doesn't reach it while Blocked (142-148).
- Dogfooding: `run_ctcjjwmbat` blocked 19:27:36 → approved 20:33:33.935 →
  `run_ended limit_reached` 20:33:34.605 with no `tool_finished` for the
  approved command; `run_a8743rrtac` the same at 22:13:45. Each restart was a
  fresh session told "The previous attempt failed: Timed out after 30 min",
  and its first command was again classified High and blocked.
- Experiment **E1** (fake agent, timeout 3 s, developer answers after 5 s):
  `after approval: runs=[(Failed, "Timed out after 1 min"), (Waiting, None)]`
  and the history ends `decision approved by user → Implementing → Run failed
  → … A claude run started → decision needed (toolpermission, high risk) →
  Blocked — Needs your approval: Run rm -rf build`. The developer's approval
  is consumed by killing the run, and the new run asks the same question.
- Claude Code itself had no problem waiting (no permission timeout).

### R-2 · Verification measures the environment, not the work — **Confirmed** [D][R]

Two variants, same effect: the deterministic check fails for reasons the
coding agent can't fix by changing code, the judge turns that into a "fix"
task, and each fix is a fresh run against the attempt budget.

- **(a) Toolchain missing from the workspace environment** [D]: all three
  checks in the real feature were `` `cargo test` (exit 127) `` with
  `sh: cargo: command not found` — while the agent, in the same feature,
  reported build and tests green (it used `~/.cargo/bin` explicitly). Result:
  3 "Fix what verification found" runs, then "Stopped: all 6 attempts are used
  up". See R-3 for why cargo was missing.
- **(b) Unscoped, environment-sensitive check** [R]: in benchmark Mode B
  ([06](06-benchmark-results.md)) the deterministic engine chose the
  whole-workspace `cargo test` for a task whose acceptance was
  `cargo test -p otter-core`. The task was fixed in run 1, but the full suite
  — which includes e2e tests that misbehave inside a managed run's environment
  — failed three times (exit 101), producing 3 more runs and 6 developer
  approvals while the agent chased unrelated test failures.
- Experiment **E2**: a check that can't run + an agent that succeeds every time
  → 6 runs, `Failed — Stopped: all 6 attempts are used up`. The loop detector
  never fires (it only looks at *Failed runs*, and each fix is a new task).
- The judge isn't told *why* the check failed in a way that separates
  "your change broke it" from "this command can't run here", and the
  remediation text is a model's paraphrase (the real one said "Make cargo
  available on the evidence-run PATH" — a task the coding agent cannot do for
  the controller's environment).

### R-3 · The workspace environment silently lacks the toolchain — **Confirmed** [C][R]

- The repo's `.envrc` is `use flake`. Otter resolves it with
  `direnv export json` under the **login** environment (`environment.rs:51-80`,
  `env.rs:60-110`: `$SHELL -l -c`, non-interactive).
- On this host `nix` is put on PATH only in `~/.zshrc` (interactive), so in
  the login environment `use flake` prints `nix: command not found` — and
  **direnv still exits 0**. Reproduced exactly:
  `env -i HOME=… zsh -l -c env` → PATH without nix/cargo; `direnv export json`
  under it → exit 0, stderr `nix: command not found`, an export with no
  toolchain.
- Otter reports the environment as `{"kind":"direnv","status":"ready"}`
  (`~/.otter/state/state.json`).
- This is a **development-environment problem**, not orchestration. The
  native agent recovered from it by itself (found `~/.cargo/bin`); the
  controller's deterministic check could not, and nothing surfaced "the
  environment is broken" as such — it surfaced as "tests failed".
- Related: the first start failed "claude is not on PATH" in the same
  environment-capture family [D] (cause not determined; `~/.local/bin` *is* on
  the current login PATH) [I].

### R-4 · A run ends at the first `result`; background work is killed — **Confirmed** [C][D]

- `actor.rs:617-626`: on `TurnFinished` with nothing queued, stop the worker
  and end the run. The worker's shutdown aborts the SDK and otterd SIGKILLs the
  process group (`sdk.ts:220-229`, `claude_sdk.rs:689-708`).
- The SDK documents work that continues after `result` (task notifications,
  `session_state_changed: idle` as "authoritative turn-over signal",
  background-task wake-ups; `sdk.d.ts:5381, 5657, 5793-5799, 5927-5945, 9482`).
  The worker's mapper drops those messages (`map.ts:103-113, 180-182`).
- [D] `run_5e6e0ssfr0`, task "Survey existing code": the agent's last words
  were *"The `cargo test` run is still going in the background, and I'll get a
  notification when it finishes. I'll wait for it before writing the
  summary."*; turn finished 18:38:58.019, run ended .569, task marked **done**.
  (legacy_cli; the SDK path ends runs by the same actor code.)
- Whether a backgrounded Bash survives the group kill depends on whether
  Claude Code detaches it [I]. Either way the notification can never arrive.

### R-5 · Every failure restarts cold — **Confirmed** [C][D]

A Failed run (timeout, crash in v0.6.19, or error) is never resumed; a failed
check spawns a new *task*, which is always a new session. 10 runs in the real
feature = 10 sessions. The agent that understood the code best (the one that
wrote it) is never the one asked to fix it.

### R-6 · A daemon started inside an Otter terminal inherits that terminal's identity — **Confirmed** [C][R] (low product impact)

Found while benchmarking, and caused by how the benchmark was launched. This
audit ran in an Otter terminal session, whose environment carries
`OTTER_SESSION_ID`/`OTTER_WORKSPACE_ID`…, `BROWSER=otter-open` and the
production daemon's browser stand-ins first on `PATH` (`sessions.rs:217-236`,
`files.rs:74-102`). A scratch `otterd` started from there captures its login
environment by running `$SHELL -l`, which inherits all of it; `env.rs`'s
`STRIP` list (`env.rs:24-40`) removes terminal variables but none of Otter's own.
The benchmark agent then ran the repo's
`browser_login_opens_trusted_pages_through_a_connected_app` test, which
executes `xdg-open https://github.com/login/device` and lets "a real xdg-open
on the host have its turn" (`e2e.rs:2167`); the fallback found the
**production** daemon's stand-in, and the page reached the developer's Otter
app (the developer saw a GitHub login appear and asked about it).

Managed runs on a normally started daemon don't get the stand-ins (only
terminal sessions do), so ordinary use is not affected. It matters for
dogfooding Otter on Otter, and the test that falls through to a real
`xdg-open` is a test-hygiene defect. The later benchmark runs drop every
stand-in from `PATH` and mock the browser ([scripts/bench.sh](scripts/bench.sh)).
Benchmark Mode B's run 1–4 numbers are affected by this inherited environment
([06](06-benchmark-results.md) §4).

### R-7 · Recovery the native agent did on its own — observed [D]

- Toolchain missing → found `~/.cargo/bin`, then the flake dev shell.
- A unit test broken by its own earlier change → found and fixed it.
- Build hang suspicion → moved test runs to the background with a log.
These are exactly the recoveries the plan wants the agent to own. They
happened; Otter's verification then failed anyway (R-2), so they didn't count.

## 3. The plan's scenarios

| Scenario | What happens today | Owner today | Should be |
|---|---|---|---|
| A. Compilation failure | Agent sees it in its own run and fixes it (R-7). If only the controller's check sees it: new fresh-session fix task | agent / controller | **Agent**, in the same session |
| B. Test failure | Same as A; controller's check → judge → fix task (new session) | controller | **Agent** first; controller only verifies the end state and resumes *the same session* with the failure output |
| C. Test hangs | No signal: Claude Code's Bash timeout inside the run; the controller's check has a 10-min timeout without a process group; the 30-min run timeout can't tell slow from stuck | — | **Runtime**: expose elapsed time / last output of running tools; kill process groups |
| D. Missing dependency / toolchain | Agent works around it; controller's check fails with exit 127 and burns attempts (R-2, R-3) | — | **Runtime** detects "command not found" / environment errors and reports *environment* failure to the developer once, instead of a fix task |
| E. Repeated identical failure | Loop detector only sees Failed runs of one task; check failures across fix tasks are invisible to it | controller | **Controller**: detect the same check failure (same command, same exit code, same tail) twice → stop and say so |
| F. Agent process crashes | HEAD (D-061): Interrupted → resume same session with `CUT_OFF`; v0.6.19: Failed → fresh run | runtime | Keep HEAD's behaviour (good) |

## 4. Recovery responsibility matrix

| Recovery | Coding agent | Runtime | Controller | Developer |
|---|---|---|---|---|
| Fix compile/test errors from its own changes | **owns** | — | — | — |
| Retry a flaky command, inspect logs, find a tool | **owns** | — | — | — |
| Background work and waiting for it | **owns** | must not kill it (R-4) | — | — |
| Process crash, daemon restart | — | **owns** (resume by session, D-061) | resumes | only if unsafe |
| Environment broken (toolchain, PATH, direnv) | may work around | **detects** and reports | stops fix loops | **fixes** |
| Run limits (time, cost) | — | measures *active* time | enforces, then resumes the same session | raises a limit |
| Verified completion | runs the checks it was told | — | runs the check once, independently; on failure **resumes the same session** with the output | accepts |
| Risky operations | asks | policy | — | **decides** |

The controller should not become a second coding agent: it should not plan
implementation steps, decompose a coherent change into sessions, or paraphrase
failures into new tasks.

## 5. Failure-state diagram (today)

```
            ┌────────────── developer approves after >30 min ──────────────┐
            │                                                              ▼
Implementing ──ask(High)──► Blocked ──approve──► Implementing ──(run age>30m)──► run Failed ─┐
     │                                                                                       │
     │ run Completed (first `result`; background work killed)                                │
     ▼                                                                                       ▼
 next task ──► … ──► Verifying ──check fails (incl. exit 127)──► judge ──► NEW fix task ──► NEW session
     ▲                                                                                       │
     └───────────────────────────────────────────────────────────────────────────────────────┘
                         each fresh run spends 1 of 6 → Failed "all 6 attempts are used up"
```
