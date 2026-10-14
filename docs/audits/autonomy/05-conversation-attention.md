# 05 — Conversation, attention and noise

Part of the [autonomy audit](README.md). Labels: [C] code · [R] reproduced ·
[D] dogfooding record (legacy_cli) · [I] inference.

This document is about **presentation**, kept separate from execution (plan
rule 2): a noisy view doesn't by itself mean the agent was slowed down. Where
noise *does* cost execution time it is said so explicitly.

## 1. Message taxonomy

What a feature's view shows (`FeaturesView.tsx`: main `Conversation` column,
side tabs Plan/Work/Tasks/Approvals/Evidence/Delivery/Timeline):

| Item | Emitter | Persisted | Where | Class |
|---|---|---|---|---|
| Developer messages | `features.rs:689-697, 722` | yes | main | user input |
| Otter's reply | `controller.rs:209-215` | yes | main | important progress / answer |
| "Plan: N task(s) …" | `controller.rs:547-568` | yes | main | important progress |
| "Verified." / "Not verified yet … Next: …" | `controller.rs:885-925` | yes | main | final result / recoverable issue |
| "Stopped: …" | `controller.rs:403-405` | yes | main + banner | recoverable issue |
| **Every text block the agent writes** ("I'll look at…", "Now the core types.") | `runs.rs:387-388 → agent_said 843-852` | yes, + history "The coding agent reported" | **main** | **execution detail / internal reasoning** |
| Agent end-of-run summary | `runs.rs:741-745` | yes | main | final result |
| Activity lines (`$ cmd`, `✎ path`, `Read`, `? question`) | `runs.rs:398-400, 874-887` | yes (last 30 per run) | **main** (last 6 while live) | execution detail |
| Decision cards | `runs.rs:571-683`, `controller.rs:519-542, 989-1020` | yes | Approvals tab, **all statuses** | user decision when pending; **debug event when decided by policy** |
| Status changes | `feature.rs:530-551` | yes | header, banner, Timeline | varies |
| Evidence | `controller.rs:842-847` | yes | Evidence tab, Timeline | progress / recoverable issue |
| Rationale ("Plan (…)", "Needs your decision …") | many | yes | Plan tab | internal reasoning summary |

**[C]** The doc comment on `agent_said` says "What the coding agent said at the
end of a turn that isn't the last" (`runs.rs:842`), but its call site fires on
**every** text block (`runs.rs:388`).

## 2. Measured noise in the real feature [D]

| | Count |
|---|---|
| History events | 409 |
| — of which decision events (`DecisionRequested` + `DecisionResolved`) | 203 (50 %) |
| — of which policy auto-approvals | 83 decisions → 166 events |
| Conversation messages | 69: 58 agent, 8 controller, 3 developer |
| — agent messages that are run summaries | 9 |
| — agent messages that are running commentary | ~45 |
| — agent summaries posted twice (whitespace-different duplicate: the streamed text vs the turn's `result`, compared with `==` at `runs.rs:466-470`) | 4 |
| Times the feature was Blocked | 20 |
| Blocked → "Needs your decision: <same command>" → "Nothing is waiting on you any more" within 1 s, right after the developer approved | 2 (20:33:41, 22:13:52) |

**[R]** E3 reproduces the decision noise on legacy_cli: one routine `Edit`
produces a Decision (`ToolPermission, Low, Approved, Policy`) and two history
lines, "decision needed (toolpermission, low risk)" and "decision approved by
policy".

On the **sdk** backend policy allows are *not* recorded as decisions
(`runs.rs:432`), so the 83-record noise is a legacy_cli artefact — but the
agent-commentary-as-messages and activity-in-main-column noise are
backend-independent. [C]

## 3. Does the noise cost execution time?

- Every `agent_said` and `run_activity` rewrites `feature.json` with an fsync
  under the global features mutex and wakes the controller
  (`features.rs:298-310, 952-962`). The bridge handles runtime events one at a
  time (`runs.rs:370`), so on the SDK path a `policy_check` can queue behind
  these writes. [C] The size of that delay was **not measured** [I]. It is the
  only way presentation can slow execution; everything else in this section
  is display-only.
- The desktop app reloads the host's whole feature list on each
  `FeatureChanged` (`featureSource.ts:410`). [C] Display cost only.

## 4. Attention semantics

| State | How it's raised | Correct? |
|---|---|---|
| Working | status Implementing/Verifying/Planning | yes |
| Needs user | status Blocked or Review, or any pending decision (`features.ts:243-250`) | mostly — see below |
| Blocked | every developer-only ask blocks the feature immediately (`runs.rs:665-670`); verify-command and `allowance` decisions block later via `escalate` | inconsistent timing |
| Completed | Review → Done (developer accepts) | yes |
| Failed | limits; **not** counted as needing the developer (`features.ts:249`) | arguably wrong: a Failed feature waits on a retry |

Specific checks the plan asked for:

- **Auto-approved permissions generating attention:** no — badges use pending
  decisions only. But on legacy_cli they're listed in the Approvals tab with
  the pending ones, and the Timeline says "Decision needed…". [C][D]
- **Agent uncertainty generating attention:** yes, through the plan:
  a plan that says "confirm with the requester" makes the agent ask
  (§5). [D]
- **Routine errors before recovery:** no — a failed check goes straight to a
  fix task without raising attention (`controller.rs:900-937`). [C] (The
  problem there is the opposite: it never escalates "the check can't run".)
- **Informational messages treated as decisions:** policy-approved records on
  legacy_cli. [C][D]
- **Resolved approvals shown as pending work:** the flicker below. [C][D]

**The flicker** [C, race inferred from code and matching [D]]: `decide_open`
asks the brain about a decision (even developer-only ones, `controller.rs:580-612`);
meanwhile the developer approves (feature → Implementing); the brain returns
"escalate" and `escalate` (`controller.rs:653-669`) re-blocks the feature
without checking that the decision is still pending; the next step finds
nothing pending and `unblock`s. The `Err` branch of `decide_open` *does*
re-check (636-647); `escalate` doesn't.

**Workspace attention** (`attention.rs`) is not involved in managed runs at
all: only `sessions.rs`, `reconcile.rs` and `workspaces.rs` raise it. [C]

## 5. The "came through empty" question

Chain, all from the record and code [C][D]:

1. The developer created feature "Otter" with an **empty request**
   (allowed: only a title is required, `features.rs:664-669`; the desktop
   dialog sends `request.trim()`, `FeaturesView.tsx:998-1053`).
2. With an empty request no message is stored (`features.rs:689`). The
   developer then wrote *"can you self improve the repo, using claude code"*
   (18:30:02) and pressed Start (18:30:04).
3. That message became `messages[0]`, which every consumer skips as "the
   request": `brief()` adds messages only `if user.len() > 1` from index 1
   (`brain.rs:491-506`); `unanswered()` skips index 0 (`controller.rs:1522`);
   `developer_notes` does `.skip(1)` (`controller.rs:1637`). **The developer's
   actual intent never reached the planner, the reply model, or the agent.**
4. The planner, given `Request:` (empty), planned "Clarify scope: confirm …
   with the requester" and criterion ac_3 "confirmed with the requester before
   implementation begins".
5. The run prompt said `Request:\nOtter` (`task_prompt` falls back to the
   title, `controller.rs:1659-1663`), and task 1 told the agent to confirm with
   the requester — so the agent asked, via `AskUserQuestion`: *"The feature
   request for Otter came through empty. What should this feature do?"*,
   **after** execution had started.
6. The developer answered (5 min later). Later, ac_3 made the judge demand
   proof of confirmation; the agent asked a **second** scope question (19:03).

So: the missing requirement *was* resolvable from context (the developer's
own first message); Otter discarded it; there was no validation before Start;
and the question was asked by the coding agent mid-run because the plan
told it to. Distinguish:

- **Missing essential requirement** (empty request, nothing else said): Otter
  should ask *before* planning, once, itself.
- **Ordinary implementation ambiguity** (branch naming, commit style): the
  agent should decide. In the record the agent's second question ("how should
  each milestone be committed and pushed?") was answered by the controller
  from repo convention — reasonable, and an example of what not to escalate.

## 6. Unnecessary user-visible events — examples

| Event | Count [D] | Should be |
|---|---|---|
| "Decision needed (tool_permission, low risk)" + "approved by policy" | 83 pairs | not a decision; an activity line |
| Agent commentary as conversation messages | ~45 | Work tab / transient stream |
| Duplicate agent summaries | 4 | once |
| Blocked/unblocked flicker after approval | 2 | none |
| "Not verified yet" for environment/unverifiable reasons | 3 | one "the check can't run here" message, or none |
| Developer approvals for `.envrc` reads and `/tmp` logs | ≥8 | none (policy precision, [03](03-approval-analysis.md)) |
| A run still shown "waiting" after its decision was answered — `runs.rs:656-661` sets `Waiting`, nothing sets it back to `Running` [C]; seen in benchmark Mode B [R] | every answered run | Running again once the answer is delivered |

## 7. Recommendations: separate execution logs from conversation

1. **Conversation = developer, Otter, and the agent's *final* result per
   run.** Stop calling `agent_said` for every text block (`runs.rs:388`);
   stream intermediate text transiently (D-051 already exists) and keep it in
   the conversation journal / Work tab.
2. **Activity out of the main column**, and out of `feature.json`: emit a
   transient event instead of rewriting the feature per tool call
   (`runs.rs:874-887`). This also removes the only presentation path that can
   delay a policy reply (§3).
3. **Policy allows are not decisions.** Make legacy_cli match sdk: record a
   tool record, not a Decision (`runs.rs:580-653`).
4. **Fix `escalate`** to re-check the decision is still pending, and skip the
   brain for developer-only decisions (as `review_step` already does,
   `controller.rs:1150`).
5. **Validate the request at Start**, and treat a message sent before Start as
   the request when the request is empty.
6. **Deduplicate summaries** by normalised text.
