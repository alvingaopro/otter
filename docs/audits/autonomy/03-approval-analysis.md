# 03 — Approvals and permissions

Part of the [autonomy audit](README.md). Labels: [C] code · [R] reproduced ·
[D] dogfooding record (legacy_cli) · [I] inference. `policy.rs` lines are the
same in v0.6.19 and HEAD except where D-062 (uncommitted: `rm` inside the
workspace is routine) is noted.

## 1. The pipeline on the SDK backend

```
Claude (SDK) wants a tool
   │
   ├─ Task / Agent ──────────────────────────────► removed from the model's tools (disallowedTools)
   │
   ▼
PreToolUse hook (worker, sdk.ts:145-168)               ── blocks the tool call
   │  policy_check  ─► otterd ToolCheck (claude_sdk.rs:826-831)
   │                   ─► actor ─► bridge (runs.rs:424-458), one event at a time
   │                   ─► policy::classify (deterministic, no memory)
   │  policy_reply  ◄─ allow | deny(reason) | ask
   │
   ├─ allow ─► tool runs. Nothing recorded as a decision (only the tool record).
   ├─ deny  ─► tool refused, reason shown to Claude. Recorded as a Denied decision.
   └─ ask   ─► canUseTool (sdk.ts:171-188)              ── blocks, no deadline
                 permission_request ─► DecisionNeeded ─► classify AGAIN (runs.rs:403)
                 ─► record_ask: Pending decision; run Waiting
                    ├─ user_only (High) ─► feature Blocked: "Needs your approval: …"
                    └─ not user_only (Medium: questions, plans, unknown tools)
                         ─► controller decide_open ─► brain.decide (model call)
                              allow / deny / answer / escalate ─► developer
                 resolve ─► forward_decision ─► canUseTool returns
```

On `legacy_cli` the shape differs: Otter sees only what Claude Code itself
asks about (`--permission-prompt-tool stdio`), but every one of those asks —
including the ones policy allows — is recorded as a Decision with two history
events (`runs.rs:571-653`). [C]

**Who classifies risk:** only Otter's `policy.rs`. Claude Code's own permission
system is layered underneath: `permissionMode: "default"`; on the SDK path the
hook answers first and a hook `allow` does not reach `canUseTool` (observed by
the worker's README, `packages/claude-runtime/README.md:41-42`; **[R]** Mode A
of the benchmark: 5 hook calls, **0** `canUseTool` calls). [C/R]

**Inherited from Claude Code:** the `project` settings source only. The
developer's `~/.claude` allow-rules are not loaded on either backend (`[]` on
legacy, `["project"]` on sdk), deliberately (D-044). [C]

## 2. Duplicate or redundant checks

| Check | Redundant? | Evidence |
|---|---|---|
| Hook round trip for every tool call, including Read/Glob/Grep/TodoWrite | Partly: classify is microseconds, but the reply passes two channel hops and a one-at-a-time bridge that also rewrites `feature.json` and fsyncs the journal for the tool-start event | `runs.rs:370, 398-400, 874-887`; `actor.rs:470-473` [C]; latency **not measured** (§6) |
| `classify` twice on the ask path | Yes (harmless) | `runs.rs:403, 430` [C] |
| Hook *and* `canUseTool` for the same call | No — sequential halves of one decision | [C][R] |
| Approval caching ("always allow this") | **None.** `canUseTool` ignores the SDK's `suggestions`/`updatedPermissions`; identical asks reach the brain or the developer again | `sdk.ts:180-187` [C]; **[D]** the same `> /tmp/otter-build.log` pattern asked at 20:34:16 and again at 20:34:25 |
| Auto-approved calls shown as decisions | legacy_cli: **yes**, in the Approvals tab and the timeline as "Decision needed (tool_permission, low risk)" + "approved by policy"; sdk: no | `runs.rs:580-653`, `FeaturesView.tsx:758-837` [C]; **[D]** 83 such records; **[R]** E3 |

## 3. Why routine commands reached the developer

From the real feature [D]: **83** policy auto-approvals, **12** developer
approvals, **3** controller denials, **1** pending at interrupt — all 16
developer-only asks were "high risk". Re-classified against the v0.6.19
policy (full command lines from the journals):

| Rule that fired | Example command (abridged) | Was it risky? |
|---|---|---|
| `.env` in **CREDENTIALS**, matched as a *substring* (`mentions_any`, `policy.rs:160-178`) | `ls ~/.cargo/bin …; cat .envrc; git status --short` | No — `.envrc` is direnv config, not secrets. Fired **at least 3 times** |
| `>` redirect to a path outside the workspace | `cargo build > /tmp/otter-build.log 2>&1` (3×), `cargo test --workspace > /tmp/otter-test.log` | No — scratch logs in `/tmp` |
| whole-word `rm` (v0.6.19) | `rm crates/daemon/src/lib.rs` (empty, untracked) | No — inside the workspace; D-062 (uncommitted) fixes this |
| whole-word `deploy` somewhere in a long line | `cargo test -p otter-core …; sed -i '' …` (truncated at 200 chars) | Unclear |
| Write outside the workspace | memory file under `~/.claude/projects/…` | Reasonable to ask |

None of `export`, `;`, `&&`, `|`, `2>&1`, `cd`, `timeout` or reading from `~`
triggers on its own. [C] The word lists also catch e.g. `cargo test -- token`
or `cargo test delete` (whole words `token`, `delete`). [C]

**These are policy-precision problems, not too-strict defaults.** The rules'
intent (credentials, outside-workspace writes, destruction) is right; the
matching is lexical.

## 4. Can a low-risk request pause execution?

- **Auto-approved:** no pause beyond the round trip (§2). Not counted as
  attention: badges and the "needs you" dot use pending decisions only
  (`features.ts:243-250`). [C]
- **Medium (question, plan approval, unknown tool):** the run waits for a
  brain call (up to 300 s) — or, with the `rules` brain, the feature blocks on
  the developer (`brain.rs:387-391`). [C]
- **Unknown built-in tools are Medium.** `tool_call` recognises 18 names
  (`claude_stream.rs:583-625`); everything else is `Other` → Ask
  (`policy.rs:559-564`). On the SDK path *every* call goes through the hook, so
  `BashOutput`, `KillShell`, `TaskOutput`, `TaskCreate`/`TaskUpdate`,
  `Monitor`, `Skill`, `EnterPlanMode` and every MCP tool each cost a model
  decision, or a developer block with `rules`. [C] Whether the bundled Claude
  Code (2.1.285) uses these in ordinary runs was not observed in the benchmark
  runs (§6 of [06](06-benchmark-results.md)). [I]
- **High:** blocks the feature until the developer answers, and then **the run
  can be killed by the 30-minute timeout the moment they do**
  ([04](04-recovery-analysis.md) R-1).

## 5. Approval decision matrix (current behaviour)

| Operation | Example | Policy today | Who decides | Blocks? | Recorded |
|---|---|---|---|---|---|
| Read anywhere | `Read ~/Downloads/x` | Allow | — | no | sdk: tool record; legacy: decision |
| Edit inside workspace | `Edit src/a.rs` | Allow | — | no | same |
| Build / test inside workspace | `cargo test -p x` | Allow | — | no | same |
| Redirect to `/tmp` | `cargo build > /tmp/b.log` | Ask, High, user-only | developer (model may deny) | **feature** | decision |
| Anything mentioning `.envrc`/`.env*` | `cat .envrc` | Ask, High, user-only | developer | **feature** | decision |
| `rm` inside workspace | `rm target/x` | v0.6.19: Ask High · HEAD: Allow (D-062) | developer / — | v0.6.19: feature | decision |
| `git push`, `deploy`, `prod` (whole word) | `git push origin b` | Ask, High, user-only | developer | feature | decision |
| `rm -rf /`, `| sh` | — | Deny | policy | — | decision |
| Question / plan approval | `AskUserQuestion` | Ask, Medium | brain → developer | run | decision |
| Unknown tool | `BashOutput`, `mcp__x` | Ask, Medium | brain → developer | run | decision |
| Subagent | `Task` | removed (sdk) / Deny (legacy) | — | — | — |

## 6. Can Otter support Automatic / Conditional / Interactive?

Yes, with the existing pieces; the gap is precision, not architecture.

| Level | Today's equivalent | What's missing |
|---|---|---|
| **Automatic** — inside the sandbox boundary | `Verdict::Allow` for reads, workspace edits, builds/tests | Treat scratch writes (`/tmp`, `$TMPDIR`, `target/`) and workspace config (`.envrc`, `.env.example`) as inside the boundary; classify the SDK's built-in tools (BashOutput, TaskOutput, task lists, Monitor, Skill) as Automatic |
| **Conditional** — allowed by workspace policy | none | A per-workspace allow list (e.g. `git push` to the feature branch, `npm install`), stored with the workspace, never with the repository (D-044's reason for ignoring project settings still holds) |
| **Interactive** — sensitive operations | `Ask { user_only: true }` | Keep for credentials (real secret files, `printenv`, auth commands), production, destructive Git, writes to `$HOME` outside the workspace. Approval caching per (run, normalised command) so the same thing isn't asked twice |

Must stay Interactive regardless of speed: credential access, production
infrastructure, force-push/history rewrite, writes outside the workspace and
scratch dirs, network installs from unpinned sources when the workspace says so.

**Latency of the pipeline: unavailable.** The history writes `DecisionRequested`
and `DecisionResolved` in one `apply`, so their timestamps can't measure a
round trip. Journal gaps on legacy_cli suggest ~140–380 ms per recorded ask
[D/I]. Neither backend times spawn → ready or hook round trips; instrumenting
that would require production code changes and was not done.

## 7. Unnecessary interruptions found

1. `.env` substring rule catching `.envrc` — ≥3 developer blocks [D].
2. `/tmp` log redirects — 5 developer blocks or controller denials [D].
3. `rm` inside the workspace — fixed in HEAD (D-062), unreleased [C].
4. Re-asking identical commands — no caching [C][D].
5. Auto-approvals rendered as decisions on legacy_cli — 83 noise records [D][R].
6. Developer approvals that the run then can't use (killed on approval) — 2 [D][R].
7. Unknown built-in tools needing a model decision on the SDK path [C].
