# Brain Runtime V2 — Otter answers with tools: implementation plan

**Decision:** D-089. **Baseline:** `main` at `efb45b2` (v0.6.27).
**Reviewed** twice on 2026-10-14 (a first draft, then a revision); the
corrections are folded in — among them, that what Otter does on the
developer's word draws its authority from the developer's own message.
**Status:** Every milestone done (D-089–D-096). M0 done but for what needs real use (`runtime-validation.md`
V2-0b, V2-0c). M1 done: the gateway, the turn, the `scripted` backend,
and Claude's turn through the worker's Otter profile (live: V2-1a, V2-1b;
`otter doctor` says which path a host has). M2 reordered (D-090): the
default stays off until the transcripts exist. Done: a host setting
(*Otter's tools (preview)* in Settings) and streaming (Claude's words as it
writes them, a new paragraph per message, the reply replacing them at the
end), and the crash and replay tests of §8 (a crash before a change: made
once after; after a forward: delivered once; a message sent twice:
answered once). M2 is done but for the default, which waits on V2-0c.
M2.5: a question for the agent at work is a turn that only reads (D-091;
live: V2-2), and answer-only tasks (D-092; live: V2-3). The developer asked
for every milestone before the dogfooding; what is decided without it says
so in its decision. M3 done: Otter's conversation on the host (D-093;
`otter chat`; the app's Otter view; live: V2-4). M4 done: follow-ups on the
tasks Otter made (D-094; live: V2-5). M5 done: the API providers answer with
Otter's tools (D-095; live with OpenRouter: V2-6; §6 has the matrix).
`INTENT:` stays for what has no turn (`claude` without the worker, a
provider without a key, `rules`) until a decision of its own retires it.
And the turn is the default (D-096), at the developer's direction before
the transcripts: every milestone of the plan is done. Still to be had from
real use: V2-0b (the worker under an API key), V2-0c (the transcripts), the
API providers other than OpenRouter live, and V2-1c. **To make the default
possible, the developer:** turns the setting on for a host whose Otter is
Claude Code, works real tasks with it, and records 5–10 transcripts
(V2-0c); and, on a host with an API key, runs the spike with it (V2-0b).
**Branch:** one per step, from `main`.

> **Core decision:** Otter's replies become a bounded, tool-using turn. Every
> change the turn makes goes through the controller's existing in-process
> functions. A tool can act on the developer's behalf only when the developer's
> own message started the turn, and only for that message. The coding agent
> stays the only thing that reads or changes code. A question about code is a
> read-only turn of the coding agent, enforced by policy (M2.5), not a
> capability of Otter.

Architecture questions are settled. This plan fixes the five things the
reviews left open: the turn model, the tool contract, conversation storage,
backend compatibility, and each milestone's acceptance criteria.

---

## 1. Decisions that stand

**Yes:**

- a tool-using turn for Otter;
- a tool gateway written in Rust that doesn't depend on any model backend;
- the Claude Agent SDK worker as the first backend, with the legacy path kept
  for every other backend;
- the task conversation first, then a conversation per host;
- reuse of the controller, policy, `command_id`, `FEATURE_SCHEMA`/`migrate()`,
  `otter_core::redact` and D-058's file mechanics.

**No:**

- renaming `Feature` → `Task` internally or on the wire (D-088 stands);
- a new scheduler, store or database;
- cross-host tools or orchestration;
- running several tasks in parallel to compare them;
- `cancel_task` or `delete_task` as model tools;
- the model approving anything on its own word;
- general shell or file access for Otter.

**Vocabulary (D-053, D-088):**

| The UI says | The code says |
|---|---|
| Task | `Feature` |
| Step | `otter_core::feature::Task` |
| Run | run |
| Otter | brain + controller |

"Brain" never appears in the UI. Config keeps its names (`OTTER_CONTROLLER`).

---

## 2. Where a turn runs

A V2 turn replaces `Brain::reply` + `converse()` at the same place:
**inside `step()`** (`controller.rs:221`), under the feature's `inflight`
guard (`controller.rs:200`). This keeps the turn ordered with the lifecycle,
so M1/M2 need no revision checks.

Consequences:

1. **Hard deadline.** While the turn runs, nothing else steps that feature: no
   `decide_open`, `implement_step` or `review_step`. Proposed defaults, to be
   calibrated in M0 against the transcripts: **60 s** for each turn, **12** tool
   calls per turn, **6** model round trips per turn. When any of them runs
   out, the turn ends with what it has. It never continues in the background.
2. **Tools call in-process functions**, never RPC: `features::apply_action`,
   `features::replan`, `runs::tell_runs`, `runs::redirect_runs`,
   `runs::hold_runs`, and the controller's `decide`. These are the same
   functions `converse()` calls today.
3. **Exactly one reply per developer message.** `unanswered()`
   (`controller.rs:2443`) treats every developer message after the last
   controller message whose `correlation_id` is a `msg_` id as still open.
   A V2 turn **always** ends by posting exactly one `MessageRole::Controller`
   message correlated with the last open developer message. This also
   applies on deadline, model failure or tool failure; the fallback text is
   `status_text(f)`, as today. Without that message, `step()` would run the
   turn again on every wake.
4. **The turn starts with the same context `reply()` builds today**
   (`brain.rs:894-905`): the brief, `status_text`, the steps, and the last
   12 messages at 600 characters each. To that it adds the tool descriptions.
   Otter fetches details with tools and doesn't paste in whole transcripts.
   (D-113 later raised this to the last 20 messages at 2 000 characters each,
   plus Otter's memories. `get_messages` reads the rest.)
5. Moving the turn outside `step()` later needs `expected_revision` checks
   and its own decision. That is not in V2.

---

## 3. Turn model

### 3.1 Turn kinds

```rust
// Conceptual.
enum TurnOrigin {
    /// Started by unanswered developer messages. `authority` is the id of the
    /// last one: the message the developer actually wrote.
    Developer { authority: MessageId, messages: Vec<MessageId> },
    /// Started by a lifecycle event (M4). No developer authority.
    Event { event_id: String },
}
```

The **origin is set by the code that starts the turn**. The model never
produces or edits it. The gateway checks it on every write.

### 3.2 Tool classes

| Class | Allowed in | Examples | Recorded as |
|---|---|---|---|
| **Observe** | any turn | `get_task`, `get_run_events`, `get_pending_decisions` | (no change) |
| **Routine** | any turn | `pause_task` (the safe direction; D-083 itself pauses on a wait limit) | `Decider::Controller`, with Otter's reason |
| **On the developer's word** | `Developer` turns only | `forward_to_agent`, `redirect_agent`, `start_task`, `resume_task`, `approve_plan`, `accept_task`, `retry_task`, `revise_task`, `ask_agent` (M2.5), `create_task` (M3), `remember`, `edit_memory` (D-113) | `Decider::User`, with "You said …" (the message text); `command_id` from the developer's message (rule 3) |

Rules:

1. **Authority is the developer's message, read by the model.** The model
   decides *what* the message asks for, and the tool records *whose* word it
   is. This matches today's trust model: `converse()` already accepts on
   "ship it" (`controller.rs:423`) and approves a plan on "go on"
   (`controller.rs:480`) from the model's reading of a message. V2 makes that
   explicit and auditable. It doesn't widen it.
2. **Same scope as today.** The only developer-only decision a message may
   resolve is the **plan approval** (D-084). Every other `user_only` decision
   is still resolved only through `feature.act(Decide)` from a button: delete,
   publish, credentials and recovery (D-061). "Yes, delete it" in chat
   doesn't resolve a deletion prompt. Otter points to the prompt instead.
   (Since D-205, the host conversation's `answer_decision` also settles a
   question to the developer, or an approval that isn't `user_only`, on
   their message. Everything `user_only` still takes the button.)
3. **One message, one effect.** A repeated turn or a retried call can't
   apply the same thing twice:
   - `forward_to_agent` and `redirect_agent` keep **each forwarded message's
     own id** as its command id, exactly as today (D-059), so `carried()`
     still recognizes what was delivered.
   - Every other write on the developer's word uses
     `<authority message id>:<tool>`, because one message may cause two
     different writes (accept, then pause).
4. **Event turns** (M4) can only observe and pause. They can't start or
   resume work: paused means the developer stopped it. They can't accept,
   approve or revise. They can't retry, which would reset the budget D-045
   relies on as a backstop. They can't create tasks. Unattended work stays
   with D-083's triggers, "under a policy set in advance — never a model's
   in the moment".
5. **Model claims aren't authority.** If tool output or worker text says "the
   developer approved", that is data, not a reason for anything.

M1 implements `TurnOrigin` and the classes, and tests `Event` turns through
the scripted backend (§9.3). Real event turns start in M4.

---

## 4. Tool contract

### 4.1 Envelope

Trusted code supplies the following; the model never writes them:

- `host_id`, `feature_id`;
- `turn_id`, `origin`;
- the `command_id` derived for each write.

Every result is one of:

- `{ ok: true, data }`, typed and bounded;
- `{ ok: false, error: { code, message, next } }`.

The error codes are `not_found`, `not_ready`, `not_allowed_in_this_turn`,
`needs_developer`, `conflict`, `retryable` and `deadline`. `next` says what
would work instead, such as "press Approve" or "say so yourself". All output
passes `otter_core::redact` before the model sees it.

### 4.2 Observe tools (M1)

| Tool | Backed by | Bounds |
|---|---|---|
| `get_task` | feature document | status, steps, criteria, `status_reason`; no messages |
| `get_task_history` | `history.jsonl` | paginated, newest first, redacted |
| `get_run_events` | the run's events and activity | last N (default 30), redacted |
| `get_pending_decisions` | `f.pending_decisions()` | id, summary, kind, risk, `user_only` |
| `get_task_evidence` | evidence and the latest check | bounded excerpts |
| `get_messages` (D-113) | the conversation's messages (a task's, or the host's) | newest first, whole, redacted; 30 a page, 8 000 characters each, 40 000 a page; `before`, `query` |

`list_tasks` and `list_workspaces` arrive with the host conversation (M3).

### 4.3 Write tools: what `converse()` does today and what V2 calls

| Intent today | `converse()` today | V2 tool | Class | Calls |
|---|---|---|---|---|
| Guide | `tell_runs` for each developer message not yet carried | `forward_to_agent(message_ids)` | developer's word | `runs::tell_runs(f, id, text)` for each id |
| Redirect | `redirect_runs(last.id, text)` | `redirect_agent(message_ids)` | developer's word | `runs::redirect_runs` |
| Continue (draft) | → planning | `start_task` | developer's word | transition |
| Continue (paused) | `resume_status` | `resume_task` | developer's word | transition |
| Continue (blocked, nothing pending) | `resume_status` | — (no tool) | — | `unblock()` (`controller.rs:489`) already does this on the next step |
| Continue (plan question pending) | `decide(Decider::User, "You said go on")` | `approve_plan` | developer's word | `decide` |
| Continue (failed) | reset budget + failed steps, resume | `retry_task` | developer's word | same reset |
| Pause | active/blocked/review → paused + `hold_runs` | `pause_task` | routine | transition + `hold_runs` |
| Revise | `features::replan`, stop runs | `revise_task` | developer's word | `features::replan` + `hold_runs` (**no note**) |
| Finish (review) | `apply_action(Accept{override_gates:false})` | `accept_task` | developer's word | `apply_action`; D-047 gates apply |
| Finish (active/blocked) | → paused | `pause_task` | routine | as above |
| None | reply only | — | — | — |

Rules carried over from `converse()`. M0 freezes them as characterization
tests:

- Guide, Redirect or Continue on a **done** task means revise (D-050).
- Guide or Redirect while the **plan question** is pending means revise
  (D-084).
- `forward_to_agent` and `redirect_agent` take **ids of developer messages
  only**. The text sent is the developer's, unchanged. "Already carried" is
  checked with `carried()` (D-059). M1 never sends text written by Otter to
  the agent, and Otter's own words go only into its reply.
- **Never call** `feature.send` (it would add a `User` message and run Otter
  again) or `RequestChanges { note }` (it records `note` as a `User` message,
  `features.rs:654`).

**Excluded tools:**

- `cancel_task`: today's chat never cancels, and cancelled is the one state
  that can't come back (D-050). It stays a button.
- `delete_task`.
- Any decision other than plan approval (§3.2 rule 2).
- `override_gates`.
- Publish, merge or deploy.
- Shell, files, raw RPC.

---

## 5. Conversation storage

| Scope | Messages | Tool records | Mechanism |
|---|---|---|---|
| Task | `Feature.messages` (unchanged) | `history.jsonl` of the feature: one record per tool call (request, origin, `command_id`, outcome, redacted) | existing feature store, `FEATURE_SCHEMA`/`migrate()` |
| Host (M3) | its own journal | same journal | D-058's **file mechanics** (commit order, fsync, snapshot, read-only on corruption) with **its own record kinds**, not the runtime `Op` enum |

- **Otter's memory (D-113)** is kept beside them, as Markdown each turn is
  given: `state/tasks/<id>/memory.md` (the task's conversation),
  `state/otter/chat-memory.md` (the host's conversation) and
  `state/otter/memory.md` (across every conversation on the host). It is
  written whole and atomically, redacted and bounded. The history records
  that Otter changed it, not what it says.
- The coding agent's runtime conversation (D-055, D-058, D-064) is separate
  and unchanged. Otter's calls never go into it.
- **Write intent is a history record.** For every write, a
  `tool_intent { command_id, tool, args }` record is appended **before** the
  in-process call. A `tool_outcome` record follows after it. A crash between
  the two leaves an intent without an outcome, which §8 handles.
- M1 checks that history readers (`feature.events`, the app's history view
  and older clients) skip record kinds they don't know. If they don't, the
  new kinds wait for a schema step.

---

## 6. Backend matrix

| `OTTER_CONTROLLER` | V2 turn | Fallback | Notes |
|---|---|---|---|
| `claude` (default with Claude Code) | yes, through the Claude worker with an Otter-only tool profile (spike V2-0a passed: `tools: []` and an in-process tool server) | legacy `claude -p` reply + `INTENT:` when the worker or the host's Node is missing (D-060) | sign-in auth (D-057) |
| `anthropic` (API key) | yes, since M5: the tool loop in otterd over the Messages API (D-095); the worker under a key (V2-0b) is still to be tried | legacy `INTENT:` without a key | offline (a stand-in API); live pending: no key here |
| `openrouter` (API key) | yes, since M5: the tool loop in otterd over chat completions with tools (D-095) | legacy `INTENT:` without a key | **live: V2-6** |
| `openai`, `gemini`, `deepseek`, `xai`, `mistral`, `groq` (D-052) | yes, since M5: the same OpenAI-style loop | legacy `INTENT:` without a key | offline only (same code as `openrouter`); each provider's tool-calling quirks untried |
| `rules` | no | status text **and the message forwarded as guidance** (`Intent::Guide`, `brain.rs:160-171`; D-084) | unchanged |
| `off` | — | the controller doesn't step, so nothing answers (`controller.rs:180`) | unchanged |
| `yes`, `skeptic` | no | unchanged | tests |
| `scripted` (new, tests only) | yes, replaying a fixed tool sequence | — | §9.3 |

- `otter doctor` (D-077) reports which path a host is on, and why: "V2", or
  "legacy: no Node", "legacy: provider", and so on.
- The UI doesn't promise V2 behaviour on a legacy path.
- The Otter tool profile has no built-in tools. Only the gateway's tools are
  available, and subagents stay off as they are for coding runs (`sdk.ts`
  `DISALLOWED_TOOLS`).
- The Otter worker runs as its own process and session, separate from coding
  runs.

---

## 7. Read-only agent turn (M2.5)

This answers golden scenario 1, a **code** question in the middle of a task.
"Why is this task blocked?" is **not** in this category: the Observe tools
answer it in M1.

Built in M2.5 as D-091 records. Two changes from this section: the question
is asked only while a run is at work (with none, it is a task of its own,
D-084), and coding runs ask for Glob and Grep, which the native Claude Code
leaves out unless asked — without them a question's turn couldn't search.

### 7.1 Shape

- Tool: `ask_agent(message_id)`, which is on the developer's word. It queues
  the developer's question as a **read-only turn** in the task's existing
  coding-agent conversation. That conversation already has the code in
  context (D-064). The turn waits behind the current one (D-056: turns aren't
  merged).
- Otter's turn ends at once ("asked the agent; it answers after its current
  turn"). The answer arrives as the agent's own message (D-086). Task scope
  needs no event wakeup.

### 7.2 Enforcement

Today the policy check sees the call and the workspace root, **not the turn**
(`policy::classify(&call, &root)`, `runs.rs:491`, `:522`; the actor's `Check`
event has no turn). M2.5 adds:

1. A **mode** on the queued turn (`normal | read_only`). It is stored with the
   turn, so a restart keeps it. This is a journal schema step in which old
   records read as `normal`.
2. In the `Check` handler, when the turn is read-only:

   | Call | Verdict |
   |---|---|
   | `Read` | allow |
   | `Fetch` | allow |
   | `Edit`, `Command`, `Delegate`, `Outside`, `Other`, `PlanApproval` | **deny** |
   | `Question` | asks the developer as usual |

3. **Bash is denied outright** in read-only turns. `policy.rs` says its
   command reading is "lexical … not a sandbox", so a read-only allowlist for
   commands would be best-effort, not a guarantee. An allowlist can come later
   as a separate decision, and is never described as a guarantee.

**Backends:**

| Backend | Read-only turns | Why |
|---|---|---|
| `sdk` | yes | Every tool call passes otterd's policy through the mandatory `PreToolUse` hook (`sdk.ts:5-7`) |
| `legacy_cli` | not supported | Claude Code runs the commands it considers read-only without asking otterd (`claude_stream.rs:35-36`) |
| Codex | not supported | Approvals aren't observable (`codex.rs:17`) |

On an unsupported backend, `ask_agent` returns `not_allowed_in_this_turn` with
"code questions need the SDK runtime". Otter says so; it never falls back to
an unenforced turn.

### 7.3 Delivery mode

`DeliveryMode { Auto (default; D-084 unchanged), AnswerOnly, PullRequest }`
on the feature, added through `migrate()`.

- `AnswerOnly` means **every** turn of the task is read-only, using the same
  enforcement as §7.2.
- It is set explicitly by the developer, or by Otter on their word ("don't
  change code"). It is never inferred.
- It ships with M2.5 and isn't exposed before then.
- `PullRequest` keeps D-047's gates, and choosing it never grants publishing.

---

## 8. Recovery and limits

- **Crash during a turn, before any write:** the developer message is still
  open, so the next `step()` runs a fresh turn.
- **Crash after `tool_intent`, without `tool_outcome`:**
  - Idempotent writes (transitions, `tell_runs` keyed by message id) are
    reconciled by reading the feature: if the effect is there, the outcome is
    recorded; if not, the call runs again under the same `command_id`.
  - Anything whose outcome can't be read back follows D-061: a developer
    decision, never an automatic retry.
- **Budgets:** D-045's limits (attempts, minutes, per-run timeout, loop
  detection) and D-083's trigger policy are unchanged. V2 adds only the
  per-turn limits in §2. Under sign-in auth, cost is reported but isn't a
  hard stop; time, calls and round trips are.
- **Model failure or deadline:** the fallback reply (§2 item 3). Writes
  already done stay done and are listed in the reply.

---

## 9. Protocol, compatibility, testing

### 9.1 Protocol

- `feature.*`, `Feature`, `state/features/<id>/`: unchanged. There is no
  `PROTOCOL_VERSION` bump in M1–M2.5.
- **New events are safe:** `Event` has `#[serde(other)] Unknown`
  (`protocol/src/events.rs:176`, tested at `:296`).
- **New requests to an older daemon** get `ErrorCode::InvalidRequest`, and
  the connection stays open (`daemon/src/server.rs:54-64`). M3 maps that to
  "this host's otterd is too old for Otter conversations".

### 9.2 Entry point

M1 runs behind `OTTER_BRAIN_V2=1`; since M2a, also the host setting
*Otter's tools (preview)* (`otter_tools`; the environment overrides it, `1`
on, `0` off). When it is on and the backend is V2-capable (§6), the task
conversation's turn is V2. The default stays off until the reply streams
and the M0 transcripts exist (D-090); making it the default is a decision
of its own.

### 9.3 Test backend

`OTTER_CONTROLLER=scripted` (tests only, like `yes` and `skeptic`). It plays
a fixed list of tool calls and a final reply, set by the test. It runs on the
existing e2e `TestHost` with fake coding agents. It can start `Event` turns
directly, so the turn classes are tested before M4.

---

## 10. Milestones and acceptance

Each exit names how it is proved. "e2e" means `crates/daemon/tests/e2e.rs`
with `scripted` and the fake agents unless a real backend is stated.

### M0: Baseline

- Check every symbol and line cited here against `main` and report drift.
- **Characterization tests for `converse()` as it is today**, one per row of
  §4.3 plus the two rewrite rules, on `rules` and on a scripted legacy
  intent. They must pass before and after V2.
- Spike: the Claude worker with an Otter-only tool profile under sign-in and
  under an API key from the env file. Record the result in the backend matrix.
- 5–10 dogfooding transcripts: a mid-task code question, a blocked approval,
  guidance while running, redirect while running, follow-up on a done task,
  a question-only request, "ship it" in review, "go on" on a plan.
- Calibrate the §2 limits against the transcripts.
- ~~Move this document to `docs/plans/brain-runtime-v2.md` and add D-089.~~
  Done.

**Exit:** the characterization tests are green; the backend matrix is
confirmed; the transcripts are recorded; D-089 is merged.

### M1: Gateway, Claude turn, turn model (behind the flag)

- `TurnOrigin`, tool classes, envelope, redaction, limits, and the
  one-reply invariant.
- Observe tools (§4.2) and write tools (§4.3).
- `tool_intent`/`tool_outcome` history records.
- The Otter tool profile for the Claude worker and the tool bridge into the
  Rust gateway.
- `scripted` backend; `otter doctor` path reporting.

**Exit:**

- Real Claude, by hand: "Why is this task blocked?" gets the exact pending
  decision and the last run evidence, with no `INTENT:` (scenario 2).
- e2e:
  - guidance and redirect forward the developer's message ids, and nothing
    written by Otter reaches the agent;
  - "ship it" in review accepts (gates apply), and the same request in an
    `Event` turn returns `not_allowed_in_this_turn`;
  - an `Event` turn can pause, but `start_task`, `resume_task`,
    `retry_task` and `accept_task` all return `not_allowed_in_this_turn`;
  - "go on" approves the plan, while "approve the delete" in chat doesn't;
  - every developer message gets exactly one reply, including on deadline;
  - nothing calls `feature.send`.
- The characterization tests are still green on `rules` and a legacy
  provider.

### M2: Task conversation by default, hardened

- V2 is the default for V2-capable backends; the legacy paths are unchanged.
- Crash and replay through the history records, per §8.
- Latency and confirmation rate measured against the M0 transcripts.

**Exit:**

- e2e: a crash after `tool_intent` and before the call, a crash after the
  call and before `tool_outcome`, a duplicate developer message, the
  deadline, the worker missing (falls back to legacy), and redaction of a
  secret in run output.
- No unauthorized transition in the characterization tests.
- Golden scenarios 2–5 and 8–12 pass.

### M2.5: Read-only agent turn

- Turn mode, the `Check` handler rule and Bash denial (§7.2).
- `ask_agent`; `DeliveryMode::AnswerOnly`.

**Exit:**

- e2e on the `sdk` runtime with a fake agent that tries `Edit` and `Bash`
  inside a read-only turn: both are denied, and nothing changes in the
  workspace (checked with `git status` and the file hashes).
- A restart keeps the turn read-only.
- `legacy_cli` and Codex give a clear refusal.
- By hand on real Claude: a code question during a task gets an answer that
  cites `path:line`, and the workspace is unchanged (scenario 1).

### M3: Conversation per host

- The host journal (§5); `list_tasks`, `list_workspaces`, `create_task`, and
  the task tools scoped to that host.
- An **Otter** entry in the UI with a visible host selector and links to task
  conversations.
- The "too old" message (§9.1).

**Exit:** from the host conversation, list the tasks, create a bugfix task
and open it. No other host is touched. The conversation survives an app
restart and a daemon restart. An older daemon shows the "too old" message.

### M4: Event follow-up

- `Event` turns from controller lifecycle events on a host conversation,
  with subscriptions and dedupe kept in the host journal.
- No duplicate completion message: the controller's message stays the one
  that announces it (D-069).

**Exit:** e2e: one completion event leads to at most one follow-up. A
duplicate event leads to nothing. After a restart, nothing is missed or
repeated. No worker is kept idle while it waits.

### M5: Tool calling for other providers

- Native tool calling in `providers.rs` for at least one provider, using the
  same gateway, tool schemas and errors.
- Remove `INTENT:` only after an approved deprecation decision.

**Exit:** that provider passes the M1/M2 golden scenarios, and there is a
support table for the rest.

---

## 11. Golden scenarios

| # | Scenario | Pass condition | First milestone |
|---|---|---|---|
| 1 | Code question in the middle of a task | Answer from the agent cites code; workspace unchanged | M2.5 |
| 2 | "Why is this blocked?" | Names the exact pending decision or failure from state | M1 |
| 3 | Guidance while running | Developer's message forwarded by id, once | M1 |
| 4 | Redirect while running | Current turn stops; developer's text goes next | M1 |
| 5 | Developer-only approval | Chat can't resolve it, except plan approval in a developer turn | M1 |
| 6 | `AnswerOnly` task | Every write denied by policy | M2.5 |
| 7 | Host conversation | Lists and creates tasks on the selected host only | M3 |
| 8 | Crash after a write | Effect once; uncertain → developer decision | M2 |
| 9 | Legacy providers and `rules` | Characterization tests unchanged | M1 |
| 10 | Secret in run output, injection in tool output | Redacted; no tool runs on the strength of tool text | M1 |
| 11 | "Ship it" in review | Accepts through the gates in a developer turn; refused in an event turn | M1 |
| 12 | Every developer message | Exactly one reply, also on failure | M1 |

**Metrics** (V2 against legacy on the M0 transcripts):

- correct action rate;
- unnecessary confirmations;
- unsupported claims (statements with no matching tool evidence, annotated
  by hand);
- time to a useful answer;
- safety regressions (target zero).

---

## 12. To verify in M0 (not assumed here)

- The Claude worker can run a profile with no built-in tools and only
  external tools, under sign-in and under an API key from the env file.
- ~~History readers skip record kinds they don't know (§5).~~ They do:
  `FeatureEvent` has `#[serde(other)] Unknown`, every record carries its
  `text`, and the desktop app reads `type` as a string.
- Where a queued turn's mode is best stored in the D-058 records (§7.2).
- How the controller reads the end of a forwarded turn today. A read-only
  question turn must not be taken as the result of the current step, and it
  must cost no attempt (D-064: only a turn that starts new work costs one).
  This hasn't been checked yet (§7).
- The §2 limits are right for real transcripts.
- That Otter's worker never sees the project's `.envrc` (it gets the
  host's base environment): no test shows it, since the fake host has no
  direnv.
