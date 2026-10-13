# 01 — Execution architecture: what actually happens

Part of the [autonomy audit](README.md). Code references are to the working
tree on `roadmap-m1` (HEAD `5ebcf4f`). Where the production daemon that produced
the dogfooding evidence (**otterd 0.6.19**, tag `v0.6.19`) differs, both are
given. `brain.rs`, `policy.rs` (apart from the uncommitted D-062 change),
`actor.rs`, `runtime/mod.rs`, `claude_sdk.rs` and the worker
(`packages/claude-runtime`) are identical in both.

**Evidence labels**, used in every document:
**[C]** confirmed from code · **[R]** reproduced at runtime (experiment or
benchmark in [06](06-benchmark-results.md)) · **[D]** observed in the real
dogfooding record (`~/.otter/state`, feature `ft_ya5zm0s22e`) · **[I]**
inference, not yet validated.

## 1. Two backends, one engine

A feature's coding work runs through one of two backends behind the same
runtime contract (`runtime/mod.rs`, D-044/D-057):

| | `sdk` (primary, D-057) | `legacy_cli` |
|---|---|---|
| Process | Node worker (`packages/claude-runtime`) driving the Claude Agent SDK 0.3.285, which runs its bundled Claude Code | `claude -p --input-format stream-json …` |
| Every tool call | `PreToolUse` hook → otterd policy (`sdk.ts:99,145-168`) | Only calls Claude Code itself asks about (`--permission-prompt-tool stdio`, `claude_stream.rs:35-36`) |
| Settings | `settingSources: ["project"]` (`claude_sdk.rs:401`) | `--setting-sources ""` (`claude_stream.rs:81-86`) |
| Default? | **No** | **Yes** — `coding_backend()` falls back to `legacy_cli` (`settings.rs:153-159`) |

**[C][D] The real dogfooding feature ran on `legacy_cli`**, not the SDK: every
conversation manifest under `~/.otter/state/conversations/` says
`"backend": "legacy_cli"` and `settings.json` sets no `coding_backend`. The
primary backend is therefore not what ships by default. Everything in this
audit that comes from that record is labelled as `legacy_cli` evidence; SDK
conclusions rest on code and on the SDK benchmark ([06](06-benchmark-results.md)).

## 2. The actual sequence

```
Developer      Daemon: features + Controller          Brain (model)        Runtime / worker          Claude (SDK)
   |  create(title, request)  -> Draft            (empty request accepted; no message stored)
   |  start                   -> Planning
   |                              |-- plan(brief) ------------>|  model call, NO repo access
   |                              |<- reqs, criteria, 1-5 tasks, verify_command
   |                              |   policy.classify(verify_command)  (Ask -> developer decision)
   |                              |   Implementing
   |                              |-- start_run(task 1 prompt) --------------------->| spawn worker (cold)
   |                              |                                                  |-- query(prompt) --->| new session S1
   |                              |   every tool call:  PreToolUse -> policy_check ->|<--------------------|
   |                              |<----------------------------- ToolCheck ---------|
   |                              |   classify: Allow | Deny | Ask
   |                              |   Ask: canUseTool -> DecisionNeeded -> brain.decide (model) / developer
   |                              |<----------------------------- TurnFinished ------| first `result`
   |                              |   run ends: worker shut down, process group SIGKILLed
   |                              |   task 1 Done
   |                              |-- start_run(task 2 prompt) --------------------->| NEW worker, NEW session S2
   |                              |   ... one run (one turn) per task ...
   |                              |   all tasks Done -> Verifying
   |                              |   sh -c verify_command (workspace env, 10 min)
   |                              |-- judge(criteria, evidence) ->|  model call
   |                              |   not met -> new task "Fix what verification found"
   |                              |-- start_run(fix prompt) ------------------------>| NEW worker, NEW session S3
   |                              |   met -> Review (push asks developer, PR, CI, gates)
   |  accept                  -> Done
```

Cross-cutting, at any point: a 5 s controller tick plus wake-on-change
(`controller.rs:56-58, 84-128`); a **30-minute wall-clock run timeout**
(`controller.rs:61-63, 674-682`); a 120-minute active-time budget, 6 fresh
runs per feature, 3 attempts per task, and a 3-in-a-row loop detector
(`feature.rs:384-385`, `controller.rs:40-43`).

## 3. Transition-by-transition

| # | Transition | Owner | Model call? | Starts / stops the agent? | Can block? | Needs the developer? | Persisted | After a daemon restart |
|---|---|---|---|---|---|---|---|---|
| 1 | Create feature | developer (`features.rs:664-697`) | no | no | no | — | `feature.json` (no message if request empty) | durable |
| 2 | Draft → Planning | developer `start` (`features.rs:424`) — **no validation of the request** | no | no | no | yes (start) | status | durable |
| 3 | Planning → Implementing | `brain.plan` (`controller.rs:442-449`), stored by code (455-575) | **yes** | no | if `verify_command` needs approval: a developer decision (519-542) | sometimes | plan, criteria, tasks, verify command | durable; planning error fails the feature (449, 117-123) |
| 4 | Next task → run | code (`controller.rs:705-789`, `runs.rs:122-276`) | no | **starts a new worker + new session** unless resuming a Cancelled/HandedOff/Interrupted run of the same task | no | no | run, conversation journal | journal replay; run marked interrupted and resumed (D-061) |
| 5 | Tool call | policy (`policy.rs:classify`), deterministic | no (yes if Ask and not user-only: `brain.decide`) | no | **yes**: Ask blocks the tool call; user-only blocks the feature (`runs.rs:665-670`) | for High risk | sdk: only Deny/Ask recorded; legacy: every ask recorded | pending decisions survive |
| 6 | Agent turn ends | actor (`actor.rs:617-626`) | no | **stops the worker** (5 s grace, then group SIGKILL) | no | no | `Finished`, `RunEnded` | — |
| 7 | Run ended → task state | code (`runs.rs:741-762`) | no | no | no | no | Completed→task Done, Failed→Failed | durable |
| 8 | All done → Verifying → judge | code runs the check (`controller.rs:801-835`), `brain.judge` (858-865) | **yes** | a failed criterion creates a new task → a new fresh run (900-937) | no | no | evidence, criteria, new task | durable; judging error fails the feature (865) |
| 9 | Developer message | `brain.reply` classifies intent (`controller.rs:179-293`) | **yes** | redirect interrupts; pause/finish/revise **stop** the run (309-316) | no | — | message | durable |
| 10 | Run older than 30 min | code (`controller.rs:674-682`) | no | **kills the run as Failed**; next run is fresh | — | — | run Failed, attempt spent | — |
| 11 | Limits | code (`controller.rs:689-773`) | no | no | feature Failed | yes (retry) | `LimitReached` | durable |

## 4. Component responsibility matrix

| Component | Owns | Model? | Notes |
|---|---|---|---|
| `features.rs` | Feature store, commands, history, decisions | no | Every `apply` rewrites `feature.json` and wakes the controller (`features.rs:952-962`) |
| `controller.rs` | Stepping features: plan → implement → verify → review; limits; timeouts; routing developer messages | calls the brain | 5 s tick + wake-on-change; one step per feature at a time (`inflight`) |
| `brain.rs` | `plan`, `decide`, `judge`, `review`, `reply` | yes (`openrouter`/Haiku on this host) or `rules` | No tools, no repository access (`claude_stream.rs:431-432`; plain API call) |
| `runs.rs` | Starting runs, the bridge from runtime events to the feature, recording decisions, forwarding answers | no | Every text block becomes a feature message (`runs.rs:387-388`) |
| `runtime/actor.rs` | One conversation: turns one at a time, journal-before-act, interrupts, ending the run at turn end | no | D-055/D-056/D-058 |
| `runtime/policy.rs` | Allow / Deny / Ask, by word lists and paths | no | Deterministic; no memory between calls |
| `agents/claude_sdk.rs` + worker | Worker process, init, PreToolUse/canUseTool bridging | no | One worker per run (`claude_sdk.rs:3-4`) |
| `attention.rs` | Workspace/session attention | no | Not used by managed runs at all (only `sessions.rs`, `reconcile.rs`, `workspaces.rs` call it) |

## 5. Controller intervention inventory

Each intervention point, classified per the plan's categories. Detailed
evidence in [02](02-controller-analysis.md).

| Intervention | Category | Why |
|---|---|---|
| Policy gate; `resolve` refusing a model's allow of a developer-only request | **Essential** | Safety boundary the SDK's own modes don't express (workspace, credentials, production) |
| Durable state, journal, resume-by-session after pause/interrupt/crash (D-055–D-061) | **Essential** | Product requirement: persistent, recoverable sessions |
| No automatic merge/deploy; developer accepts | **Essential** | User intent |
| Deterministic check after the agent finishes | **Useful** | Independent evidence — *when the check can run in the workspace environment and matches the task's scope* (it often doesn't: [04](04-recovery-analysis.md) R-2, R-3) |
| Loop detector, attempt/time budgets | **Useful** in principle | Bounded cost. In practice the time budget restarts on every unblock ([02](02-controller-analysis.md) §5) |
| Routing a developer message (guide / redirect / pause …) | **Useful** | But a model mis-reading "correction" as `revise` stops the run |
| Planning by a model with no repository access, into 1–5 tasks | **Harmful** | Tasks are generic ("Clarify scope / Survey / Implement / Add tests / Verify") and each becomes a separate cold session; observed [D] |
| One run = one turn = one task; new session per task and per fix | **Harmful** | Breaks execution continuity; the agent re-reads the repo each time and can't carry its own plan across Otter's task boundaries [C][D] |
| 30-minute wall-clock run timeout that includes time waiting for the developer | **Harmful** | Kills the run the moment the developer approves; the next run starts fresh and asks again [C][D][R] |
| Controller (model) denying developer-only requests, reported as "The developer declined this." | **Harmful** | A small model overrides work in the developer's name [C][D] |
| `judge` against plan-invented, unverifiable criteria | **Harmful** | ac_3 "confirmed with the requester before implementation begins" could never be met; it drove two extra fix runs and a second scope question [D] |
| Ending the run at the first `result` (background work killed) | **Harmful** | Contradicts the SDK's documented background-task model [C][D] |
| "Run the project's tests before you finish" + "The work is checked with `cmd`; run it before you finish" on *every* task | **Redundant** | Full test runs per task, then the controller's own run [C][D][R] |
| Restating every acceptance criterion and every developer message in every fresh task prompt | **Redundant** | — |
| Asking the brain about unrecognised built-in tools (`BashOutput`, `TaskOutput`, `Monitor`, task-list tools, `Skill`) | **Harmful** on `sdk` | A model call (or, with `rules`, a block on the developer) per call to a routine tool [C] |

## 6. Bottlenecks, ranked by measured impact

From the real feature (legacy_cli), 3 h 48 min wall clock
([scripts/feature_metrics.py](scripts/feature_metrics.py)) [D]:

| Where time went | Minutes | Share |
|---|---|---|
| Feature **Blocked** on the developer (20 blocks: 16 approvals, 4 decisions) | 193.8 | 85 % |
| Implementing | 26.5 | 12 % |
| Failed (waiting for a retry) | 7.1 | 3 % |
| Verifying + planning | 0.3 | 0 % |

The requested work (roadmap milestone M1) was committed by the third run at
18:56, **26 minutes after start** [D]. Everything after it (3 h 20 min, 7 more
runs, 3 verification failures, 12 developer approvals, the attempt budget
exhausted once) was orchestration and environment friction, not development.
