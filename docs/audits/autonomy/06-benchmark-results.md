# 06 — Experiments and benchmark

Part of the [autonomy audit](README.md). Everything here was run on
2026-10-13 on the developer's Mac (darwin, arm64), in disposable git worktrees
of `5ebcf4f`, never against the developer's working tree, `~/.otter` or
`~/.config/otter`. Nothing was pushed.

## 1. Deterministic experiments (E1–E3)

Real `otterd` + tmux + the repo's fake `claude -p`
(`crates/daemon/tests/fake_claude_stream.sh`) and the deterministic `rules`
brain, via the e2e harness. Source:
[experiments/audit_experiments.rs](experiments/audit_experiments.rs) (append
to `crates/daemon/tests/e2e.rs` in a scratch worktree), plus
[experiments/fake_allow_delay.patch](experiments/fake_allow_delay.patch) (the
fake agent takes 2 s to run an approved command, as a real one would — without
it the fake finishes in microseconds and beats the controller's step). Output:
[experiments/audit_experiments.out](experiments/audit_experiments.out).

```sh
cargo test -p otterd --test e2e audit_ -- --nocapture --test-threads=1
```

| # | Setup | Result | Shows |
|---|---|---|---|
| E1 | `OTTER_RUN_TIMEOUT_MS=3000`; agent asks a developer-only `rm -rf build`; developer approves after 5 s | `runs=[(Failed, "Timed out after 1 min"), (Waiting, None)]`; history: *approved by user → Implementing → Run failed → … A claude run started → decision needed (high) → Blocked — Needs your approval: Run `rm -rf build`* | RC-3: the approval kills the run; a fresh run asks the same thing. (First attempt without the 2 s delay completed normally — the race is real but needs a realistic agent.) |
| E2 | Check `make test` can't run (command missing); agent reports success every time | 6 runs, 6 × `` `make test` (exit 2) ``, `Failed — Stopped: all 6 attempts are used up`; no loop detection | RC-1/RC-4: an unrunnable check burns the whole budget |
| E3 | Agent makes one routine `Edit` (legacy_cli) | `decisions: [(ToolPermission, Low, Approved, Policy, "Edit src/a.rs")]`; history "decision needed (toolpermission, low risk)", "decision approved by policy" | RC-10: policy allows recorded as decisions on legacy_cli |

## 2. Live benchmark: three modes (plan revision 2)

### Modes

| Mode | What runs | Controller | Permission policy |
|---|---|---|---|
| **A** Direct SDK | [scripts/direct_sdk.mjs](scripts/direct_sdk.mjs): `query()` from the **same** SDK package the worker ships (`@anthropic-ai/claude-agent-sdk` 0.3.285, `~/.local/lib/otter/claude-runtime/0.6.19/node_modules`), the worker's options (`settingSources: ["project"]`, `claude_code` preset, `permissionMode: "default"`, `disallowedTools: Task, Agent`) | none | a `PreToolUse` hook and `canUseTool` that allow everything and count calls |
| **B** Otter SDK runtime, no model controller | dev `otterd` (HEAD `5ebcf4f`) with `coding_backend: sdk` and the 0.6.19 worker; `controller: rules` | deterministic engine: 1 task = the request, check = detected project command, no model calls; asks the developer for anything policy leaves open | Otter's policy |
| **C** Otter SDK runtime + Control Agent | as B, with the developer's controller (`openrouter`, `anthropic/claude-haiku-5.5`, their key) | model plan / decide / judge | Otter's policy |

`OTTER_CONTROLLER=off` starts no runs, and no protocol request starts a run
outside a feature, so `rules` is the closest available "runtime without
Controller". Harness: [scripts/bench.sh](scripts/bench.sh). The "developer" in
B/C is a loop that answers every pending decision within ~2 s (approve; deny
`rm -rf`/`git push`/`sudo`; answer questions "your call"), so **human wait time
is excluded** and approvals are reported as counts.

**Controlled:** SDK version, model (Claude Code's default — neither Otter nor
Mode A sets one), repository revision, task prompt, toolchain (each worktree's
`.envrc` is replaced by `PATH_add ~/.cargo/bin` — the repo's `use flake` can't
resolve on this host, R-3), warm build cache (each worktree pre-built
`otter-core` tests), browser mocked (after the first B run, §4).
**Not controlled / differs:** Otter appends its system-prompt instructions and
task prompt (that *is* the treatment); Mode A's policy allows everything
(Otter's allows routine work and asks for the rest); single run per cell;
network and API latency.

### Tasks

| Task | Category | Prompt (verbatim in `bench.sh`) | Independent acceptance |
|---|---|---|---|
| t1 | Bug fix | Fix `normalized_brief_drops_blank_text` (injected bug: list items no longer trimmed) without changing the test; check with `cargo test -p otter-core`; don't commit | test passes **and** the test body unchanged; negative control: fails at baseline ✓ |
| t2 | Small feature | Add `Brief::item_count()` with defined semantics and a unit test; check with `cargo test -p otter-core`; don't commit | a hidden test (`bench_item_count`) appended only for the check passes; negative control: doesn't compile at baseline ✓ |

The plan's other three categories (refactor, build-failure diagnosis,
multi-file feature) and repetitions were **not run** — see §6.

### Results — raw

Independent acceptance = the `verify` step of `bench.sh`, run after each mode
finished, outside Otter. "Otter verdict" = the feature's final status.

| Run | Mode | Task | Indep. acceptance | Otter verdict | Wall s | SDK sessions / runs | SDK turns | Model calls (controller) | Policy checks → developer approvals | Interrupts / restarts | Fix tasks | Time to first edit s | Cost $ |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| A1 | A | t1 | **pass** | — | 20.7 | 1 / 1 | 1 (6 round trips) | 0 | 5 hook → 0 (`canUseTool` 0) | 0 | — | 13.5 | 0.26 |
| A2 | A | t1 | **pass** | — | 13.0 | 1 / 1 | 1 (6) | 0 | 5 → 0 | 0 | — | 8.3 | 0.16 |
| A3 | A | t1 | **pass** | — | 13.1 | 1 / 1 | 1 (6) | 0 | 5 → 0 | 0 | — | 8.9 | 0.16 |
| A4 | A | t2 | **pass** | — | 17.6 | 1 / 1 | 1 (7) | 0 | 6 → 0 | 0 | — | 9.5 | 0.18 |
| A5 | A | t2 | **pass** | — | 16.7 | 1 / 1 | 1 (7) | 0 | 6 → 0 | 0 | — | 9.0 | 0.19 |
| A6 | A | t2 | **pass** | — | 20.6 | 1 / 1 | 1 (7) | 0 | 6 → 0 | 0 | — | 10.1 | 0.19 |
| B1 † | B | t1 | **pass** | (stopped by auditor at 4 runs) | 791 | 4 / 4 | 4 | 0 | unavailable → **6** | 1 (auditor pause) | 3 | 9.1 | 1.18 |
| B2 ‡ | B | t1 | pass, **plus an out-of-scope edit to `crates/daemon/tests/e2e.rs`** | Review | 823 | 4 / 4 | 4 | 0 | unavailable → **4** | 0 | 3 | 9.7 | 1.13 |
| C1 | C | t1 | **pass** | **Failed** — "all 6 attempts are used up" | 98 | 6 / 6 | 6 | 4 (1 plan + 3 judge) § | unavailable → 0 | 0 | 2 (+1 refused by the limit) | 41.9 | 0.89 |
| C2 | C | t2 | **pass** | **Failed** — "all 6 attempts are used up" | 107 | 6 / 6 | 6 | 4 (1 plan + 3 judge) § | unavailable → 0 | 0 | 2 (+1 refused by the limit) | 27.2 | 0.95 |

† B1 ran with the environment inherited from the Otter terminal this audit
ran in (R-6) and with `OTTER_*` configuration exported; one nested e2e test
opened GitHub's device-login page in the developer's real Otter app. ‡ B2 ran
with configuration in `settings.json` and a mocked browser, but the mock's
`BROWSER` export leaked into nested test daemons and broke the same e2e test
(diagnosed correctly by the agent; the export was removed before C2). §
inferred from the code path (one `plan`, one `judge` per verification with
evidence); Otter doesn't count model calls. "Policy checks" for B/C:
**unavailable** — on `sdk`, allowed checks leave no record (only tool records).
Per-run SDK round trips and tokens: **unavailable** in Otter's state (09 §7).
Worker start-up (claim → turn delivered) in B/C: 1.2–2.0 s per run
([scripts/run_startup.py](scripts/run_startup.py)).

### What the numbers show

1. **Completion.** The agent's work was correct in **10 of 10** runs by the
   independent check. Otter's verdict agreed in **1 of 4** Otter runs (B2,
   and only after an out-of-scope edit); both Controller runs (C1, C2) ended
   **Failed with correct work**.
2. **Why C failed.** The model planner wrote 4 tasks for each 1-line/10-line
   change and 4–5 acceptance criteria per feature, most of which the declared
   check can't prove: "No git commit is created", "the test passes without any
   modification to the test code", "`Brief` has `pub fn item_count`", "a unit
   test … covers an empty, full and partial brief". The check
   (`cargo test -p otter-core`, correctly scoped) **passed every time
   (6 × exit 0)**; the judge, who sees only the check output and the agent's
   summaries ("a claim, not evidence"), marked the rest ✗ and asked for
   "deterministic evidence" (`git diff`, `git log`) that nothing collects.
   Fix tasks then re-ran the same checks in fresh sessions until the 6-run
   budget ended. → **RC-1, RC-2 confirmed on the SDK path.**
3. **Why B was slow.** The deterministic engine ignores the request's check
   (`cargo test -p otter-core`) and uses the detected project command
   (`cargo test`, whole workspace; `brain.rs:268, 304-307`); Otter's prompts
   then tell the agent to run it too ("The work is checked with `cargo test`;
   run it before you finish"), so the agent ran the full suite (≈2–3 min each)
   in every run, chased a failure unrelated to its change, and needed the
   developer for `/tmp` logs and `git checkout`/`git stash` probes (High
   risk). In B2 it finally edited an unrelated test to get the check green.
   → unscoped checks + injected test instructions turn a 13-second task into
   13–14 minutes and push the agent out of scope.
4. **Overhead that is *not* there.** Worker start-up ≈1.5 s; 0 `canUseTool`
   calls when the hook allows; 0 interrupts or restarts by the Controller in
   B/C; 0 developer approvals in C (routine work stayed automatic). A
   Controller run's per-session work is fast (8–17 s); the cost is the
   *number* of sessions and the verdict.
5. **Time to first edit**: A 8–14 s; B 9–10 s; C 27–42 s (planning plus
   "inspect"/"reproduce" tasks before the edit task).

### Comparison summary (medians)

| | A direct SDK | B runtime, rules | C runtime + Controller |
|---|---|---|---|
| Correct work (independent) | 6/6 | 2/2 | 2/2 |
| Feature reported complete | n/a | 1/2 (after out-of-scope edit) | **0/2** |
| Wall s | 17 | ~800 | ~100 |
| Sessions per task | 1 | 4 | 6 |
| Developer approvals | 0 | 4–6 | 0 |
| Cost $ | 0.18 | ~1.15 | ~0.92 |

## 3. The real dogfooding feature as a Mode C data point (legacy_cli)

Not a controlled benchmark — the request was empty and the developer
answered in real time — but it is the longest real observation:
3 h 48 min; 10 runs / 10 sessions; 1 plan, 3 judges; 83 auto-approvals,
12 developer approvals, 3 controller denials; 2 runs killed by the run
timeout right after approvals; 3 checks all `exit 127`; feature stopped at
"all 6 attempts are used up", then paused by the developer; the requested
milestone was finished in the first 26 minutes ([01](01-execution-architecture.md) §6;
[scripts/feature_metrics.py](scripts/feature_metrics.py)).

## 4. Fairness and confounds

- **Same:** SDK 0.3.285 (A imports the worker's own copy), model (default),
  revision, prompt, toolchain, warm cache, machine.
- **Different by design:** Otter's appended system prompt and task prompt;
  Otter's policy vs Mode A's allow-all.
- **Confounds found and handled:** the nested Otter-terminal environment (B1),
  the `BROWSER` mock export (B2). C1/C2 used the corrected harness. The B runs
  still show the mechanism (unscoped check, injected test instruction,
  approvals for probes) but their durations include chasing a failure the
  benchmark environment caused; a clean B duration is **unavailable**.
- **Human latency excluded** (instant responder). In real use every developer
  approval in B adds the developer's response time — and, past 30 minutes,
  the run (RC-3).

## 5. Reproduce

```sh
# a built dev otterd + otter (any checkout of 5ebcf4f)
cargo build -p otterd --bin otter
export BENCH_OUT=/path/to/out BENCH_OTTER_BIN=$PWD/target/debug
S=docs/audits/autonomy/scripts/bench.sh
for m in A B C; do $S setup t1 /tmp/wt-t1-$m; done   # disposable worktrees
$S direct t1 /tmp/wt-t1-A
$S otter  t1 /tmp/wt-t1-B rules
$S otter  t1 /tmp/wt-t1-C model     # uses ~/.otter/state settings + key (read-only)
for m in A B C; do $S verify t1 /tmp/wt-t1-$m; done
python3 -I docs/audits/autonomy/scripts/feature_metrics.py "$(cat $BENCH_OUT/t1-C.home)/state" <feature id>
python3 -I docs/audits/autonomy/scripts/run_startup.py "$(cat $BENCH_OUT/t1-C.home)/state"
git worktree remove --force /tmp/wt-t1-A   # etc.
```

Pre-build `cargo test -p otter-core --no-run` in each worktree first for a
warm cache. Mode C needs the developer's Control Agent key in
`~/.otter/state/secrets.json`; Modes A–C need a signed-in Claude.

## 6. Not done, and why

- Tasks 3–5 of the plan (refactor, build-failure diagnosis, multi-file
  feature) and repetitions of B/C: each B run costs ~14 minutes of full test
  suites; C's outcome was deterministic in both runs (the same verification
  loop), so more repetitions would add cost more than information. The
  harness takes new tasks in `prompt()`/`setup()`/`verify()`.
- Mode B on t2 was started and stopped early (discarded) once B's behaviour
  had been seen twice.
- No measurement of permission round-trip latency, SDK tokens, or Claude
  Code's own round trips under Otter (needs production instrumentation).
- No run exercised background tasks, unrecognised built-in tools, or a run
  longer than 30 minutes on `sdk`; those findings rest on code and the
  legacy_cli record.
