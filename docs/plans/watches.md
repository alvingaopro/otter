# Watches — Otter looks again later: design

**Status:** proposed (2026-10-22), nothing built. Reviewed once against
the code (Fable); its corrections are folded in. The decision is recorded
(the next free D-number) when slice 1 is built, with the developer's
answers to §9. **Baseline:** `main` at `e456132` (v0.7.70).
The design as a page: https://claude.ai/artifact/2cMjnbdYyXCo4bWhbQbuTW.

> **Core decision:** when the developer asks Otter to keep an eye on
> something, Otter sets a **watch** in that conversation. otterd keeps it,
> looks for what it waits on with deterministic code and no model, and only
> when that happens — or its deadline comes — runs one turn of Otter's
> that says so, in the conversation that asked. A watch is one-shot and
> bounded, and it only looks and says. It is D-094's follow-up, made for
> what the developer asks to be told about. Recurring *work* stays a
> trigger (D-083).

## 1. Why (dogfooding, 2026-10-22)

In a task's conversation, the task accepted and its pull request (#171)
open, the developer asked “可以帮我盯一下吗?” — can you keep an eye on it
for me? Otter answered that it can't: it only looks when they send a
message. It also said “I just looked again, CI is still running (pending)”.

Two gaps, both verified:

1. **Nothing makes Otter come back.** The engine steps a task that is
   active or in review, blocked with a workspace, or has a message to
   answer (`run_controller`); Otter's turn runs on a message (D-089), or on
   the host for a status change of a task Otter made itself (D-094). Once
   its last message is answered, a done task is never looked at again. Triggers
   (D-083) start a new coding task on a schedule — the wrong weight for
   “look at CI in ten minutes”.
2. **What Otter reported wasn't true.** #171 is `CONFLICTING` with no
   checks at all, and GitHub doesn't start `pull_request` workflows on a
   pull request that conflicts: CI was never going to report. Otter read
   CI only in `review_step`, so the accepted task's `delivery.ci` was
   frozen at acceptance; `delivery::gates` reads “no checks, and the
   repository has workflows” as `CI: pending — running`; and `get_task`
   returns that gate without saying when it was read. “I just looked” was
   a stale snapshot. A watch built on that gate would have waited out its
   deadline for nothing — so slice 0 (§7) comes first.

## 2. What others do, and what Otter takes from it

| Practice | Where | What Otter takes |
|---|---|---|
| Timers are durable state, not a sleeping process; they survive restarts | Temporal's timers and Schedules | A watch is written in its conversation's document; a restarted otterd goes on with it |
| Wait on a condition, not on a clock; don't spend a model call per poll | Claude Code: *Monitor* streams until a condition, *cron* re-runs a prompt on a fixed clock, and work the harness tracks isn't polled; Kubernetes controllers requeue after a delay and act on observed state | otterd probes with plain code; a model turn only when the state reached what was waited for |
| One-shot and recurring are different things | Claude Code: `ScheduleWakeup` / one-shot cron vs recurring cron and routines; ChatGPT Tasks: one-time vs recurring | A watch is one-shot and continues a conversation; recurring work is a trigger and starts a task |
| Everything is bounded | Claude Code: recurring cron expires after 7 days, a wakeup is clamped to 60 s–1 h; ChatGPT Tasks: a cap on active tasks by plan (3–15), at most hourly, unattended ones may pause | Every watch has a deadline; a cap per conversation; a minimum delay |
| Overlap and missed runs have a stated policy | Temporal Schedules: overlap *Skip* by default, a catch-up window | Missed while otterd was down: once, as now (D-083's rule); one Otter turn per conversation at a time, the developer's messages first (the controller's own slots) |
| Poll gently where there is no push | AWS, “Exponential backoff and jitter”; otterd has no public listener for webhooks (D-083, Not done) | One modest interval with jitter for slice 1; backoff if rate limits ever matter; webhooks later, through a relay |
| Let the platform do what it does itself | GitHub auto-merge waits on required checks itself | If merging ever is on the developer's word, prefer `gh pr merge --auto` where the repository allows it (§6) |
| Visible and cancellable | ChatGPT Tasks' list; Claude Code's `CronList`/`CronDelete` | `get_task` lists the task's watches, `cancel_watch`; a line on the task's page and `otter watch` in slice 1b |
| Authority is given when it's set up, not when it fires | D-083's policy decided in advance; GitHub records who turned auto-merge on | The developer's message that asked is the authority; the turn a watch starts has none |

## 3. Words

- A **watch**: something Otter looks at again later, for the developer, in
  the conversation where they asked, until what they wait for happens or
  its deadline comes. It fires once.
- A **trigger** (D-083) stays scheduled *work*: a new task each
  occurrence. A watch never starts work; it looks and says.
- D-094's follow-ups are standing watches Otter sets by itself on the tasks
  it makes. Slice 1 is built in their shape, for a task's conversation;
  slice 2 makes them one thing.

## 4. What can be watched

A closed set, each decided by otterd without a model:

| Kind | Slice | Probe | Fires when | Deadline (default, max) |
|---|---|---|---|---|
| `time` | 1 | none | the time comes: “look at where this stands in 30 minutes” | — (1 min to 7 days ahead) |
| `ci` on the task's PR | 1 | `gh pr checks` and `gh pr view --json state,mergeable,headRefOid` | every check finished (passed or failed); or CI can't run (the PR conflicts); or no check started 10 min after the last push; or the PR was merged or closed | 2 h, 7 days |
| `pr` | 2 | `gh pr view --json state,reviewDecision` | merged, closed, or reviewed | 24 h, 7 days |
| `task` (host) | 2 | otterd's own state | reaches review, done or failed, or waits on the developer | 24 h, 7 days |

- `gh` runs with `--repo owner/name` taken from the task's
  `delivery.pr_url`, in Otter's own directory (`otter_dir`), with Otter's
  GitHub sign-in where it has one (`git_account.gh_env`, D-133) — not in
  the task's workspace, which is put away once the task is done and may
  be removed during a long watch. A task's `ci` watch is pinned to its own
  pull request.
- `ci`: a new commit on the PR keeps the watch going on the new head, and
  the report says so. `GitHub::checks` already tells “no checks” from an
  error.
- Not a kind: an arbitrary command as the condition. A command run
  unattended is the policy's business (D-066, D-173).

## 5. How it works

### 5.1 Lifecycle and storage

```
developer's message ──watch──► waiting ──probe: not yet──► waiting (probe again later)
                                  │
                                  ├─ probe: it happened / deadline / the time came
                                  ▼
                                firing  (the finding stored on the watch)
                                  │  Otter's turn, in the controller's slot
                                  ▼
                                reported (final; the message and this status in one write)

  waiting or firing ── cancel_watch, the task cancelled ──► cancelled (final)
```

- Kept on the task document: `Task.watches` (`task.json`), with
  `WatchSet` / `WatchReported` / `WatchCancelled` in its history. Each: id,
  `what` (the developer's ask as Otter put it), `asked_in` (their message
  id), kind and target, `next_at`, `deadline`, status, and once firing the
  finding (what the probe read, and when).
- **Firing before the turn.** The finding is written, with the status
  `firing`, before any turn runs: a restart re-runs the turn on the stored
  finding instead of probing again (the state may have moved by then, and
  a `ci` finding can't be re-derived the way D-094's `follow_key` can).
- Made under `<message id>:watch:<kind>:<target>` — by content, not by the
  order of tool calls, which a replayed turn may change (as the memory
  tools dedupe by the note). The target is the PR number for `ci`, and
  `in_minutes` for `time`. The same key answers “already watching #171”.

### 5.2 Probes and turns: who runs what

- **Probing** is its own loop, `run_watches`, beside `run_triggers`: every
  15 s it probes the `ci` watches whose `next_at` passed, and writes
  `firing` with the finding when one is due. A `time` watch whose time
  came is written `firing` the same way (the finding: it's time, and how
  late), so everything goes through one state. `ci` is probed every 60 s
  (`OTTER_WATCH_POLL_MS`), ±10 % jitter. A probe that fails (network,
  `gh`) isn't a firing: it's tried again; three failures in a row fire as
  “can't read CI: why”. No model on a probe.
- **The turn** is the controller's, not the loop's: the one-turn-per-task
  rule is `ControllerState.inflight` and lives in `run_controller`. Its
  filter gains “has a watch firing”, which also brings a done task in;
  `step()` runs the watch's turn after the
  unanswered-messages branch, so the developer's messages come first.
  The host's watches (slice 2) go behind `host_turn`, exactly as
  `due_follow_up` does.

### 5.3 Firing: one turn, said once

- A turn no message started: `Origin::Event { event_id:
  "watch:<id>:<n>" }` (`n` counts the watch's findings; the finding's text
  stays on the watch, out of command and correlation ids), as D-094's
  follow-ups — the task's observe
  tools and pausing, nothing on the developer's word. `watch` is refused
  there already (`DeveloperWord` without an authority, `gateway.rs`), so a
  watch can't set another. The event prompt and the `scripted` backend's
  event branch get a task-scope variant (both are worded for the host
  today).
- It's told what was asked, when, and what the probe found, with the time
  it was read. Its reply is posted in the task's conversation in the same
  write that marks the watch reported (as `record_follow_up`), and only
  if the watch is still `firing`: a `cancel_watch` that landed meanwhile
  wins, and nothing is said. Without Otter's tools, a line: “CI on #171
  passed: 12 checks.”
- **A watch's turn never fails the task, and never stays `firing`.** It
  runs `bounded`, as `follow_up` does: no answer, an error, or a panic
  becomes a plain line about the finding, written with `reported` (as
  `follow_up`'s PANICKED fallback). An error out of `step()` would
  otherwise reach `run_controller`, which fails the task — a task in
  review failed by a report — and a watch left `firing` would start a
  model turn on every tick.
- **The reply's correlation id is the watch's** (`watch:<id>:<n>`),
  never the developer's `msg_…`: `unanswered()` takes the last Otter
  message correlated to a `msg_` id as the point everything before is
  answered, so a report tied to `asked_in` would swallow every message the
  developer sent while the watch waited.
- No `ask_agent` from a watch's turn. It needs a live run (`ask` refuses
  `not_ready` without one, and D-091 left a run started for a question
  alone unbuilt), it takes the turn's message ids (an event turn has
  none), and letting it through means giving the event an authority —
  which would let `watch` through too. A look with the coding agent from a
  watch is a later decision: an explicit tool list for the origin, only
  while a run is live, asked under the watch's own command id.
- **What draws the developer's eye in slice 1 is the message itself**, as
  any message of Otter's. Chat's NEEDS YOU (D-178; the Inbox is gone)
  comes from `Task::needs()`, and every need there is cleared by a state
  change; a report has none, so a `needs()` kind for it — and what clears
  it — is a later decision with its own protocol and app work.

### 5.4 Tools (`gateway.rs`, task scope in slice 1)

| Tool | Class | What |
|---|---|---|
| `watch` | on the developer's word | `{what, kind: time\|ci, in_minutes?, for_minutes?}`; `ci` watches the task's own PR |
| `cancel_watch` | routine (the safe direction, as pausing) | `{watch_id}` |

- The task's watches, waiting and lately reported, are in `get_task`'s
  answer; no tool of their own.
- Two lines for `INSTRUCTIONS`: when the developer asks to keep an eye on
  something or come back later, `watch`, and say what for and until when;
  and say when what you report was read — never “I just looked” about
  something you didn't read in this turn.

### 5.5 Bounds, deterministic

- At most 3 waiting watches per task; past that the tool refuses and says
  which are waiting.
- `time`: 1 min to 7 days ahead. `ci`: deadline at most 7 days.
- Missed while otterd was down: probed once on start; an overdue `time`
  watch fires once and says how late. (`next_at` and the deadline are wall
  clock; nothing from `skip_sleep` is needed.)
- A cancelled task's watches are cancelled. A paused task's keep looking:
  they change nothing.
- What's kept and shown is redacted (D-075).

### 5.6 What the developer sees, and the protocol

- Slice 1: the reports in the conversation, and the watches in the task's
  details as `get_task` has them.
- Slice 1b: a line above the message box — “Watching CI on #171 · until
  17:40 · Cancel” — in the developer's local time (the tool takes minutes,
  so the model needs no time zone); `otter watch ls [--task]`,
  `otter watch rm`.
- `watches` on the task is an optional field and `watch.cancel` a new
  method: compatible, no `PROTOCOL_VERSION` bump (`protocol.md`,
  Compatibility).

## 6. Merging on green: the developer's call, not in slices 0–2

D-047: Otter never merges. The ask in §1 came right after “you can merge
it on GitHub when CI is green”, so it may well mean “merge it when it's
green”. Three answers:

- **(a) The watch reports; the developer merges.** D-047 stands. Slices
  0–2.
- **(b) On their word, Otter turns on GitHub's auto-merge**
  (`gh pr merge N --auto --merge`) and GitHub merges by its own rules.
  Needs the repository's *Allow auto-merge* and required checks on the
  base branch; `alvingaopro/otter` has neither (`allow_auto_merge: false`,
  `main` unprotected).
- **(c) A one-off grant on the watch.** The developer's message says to
  merge this PR when it's green; the `ci` watch carries that grant, and on
  every check passed, deterministic code runs `gh pr merge N --merge
  --match-head-commit <the sha CI passed on>` — never a model's choice at
  firing. Refused, and reported instead, if the head moved, the PR
  conflicts, or any check didn't pass. Revises D-047.

Recommended: (a) now; (c) as its own decision if wanted, with (b) used
instead wherever the repository allows auto-merge.

## 7. Slices

0. **CI tells the truth** (a fix on its own, before watches):
   - a PR that conflicts reads `CI: can't run — the branch conflicts with
     <base>`, not “running”; in review that's a fix step (bring the branch
     up to date) or a question, not a wait;
   - no check reported 10 min after a push: said, not “running”;
   - the CI gate says when it was read, and `get_task` returns that;
   - `fake_gh.sh`'s `pr view` learns `mergeable` and `headRefOid`; a test
     with a conflicting PR and no checks.
1. **Watches in a task's conversation:** `time` and `ci`; `watch`,
   `cancel_watch`, the watches in `get_task`; `run_watches`, the
   controller's filter and `step()`; the report. An *Accept anyway* while
   CI hasn't finished sets a `ci` watch (a default to confirm, §9): Otter's
   own `accept_task` never overrides the gates, so this is the app's and
   the CLI's override (`task.act`), placed in `apply_action`'s Accept and
   keyed by that act's command id.
   1b. The line above the message box and `otter watch`.
2. **The host's conversation:** `pr` and `task` kinds; D-094's follow-ups
   become the `task` watches Otter sets on the tasks it makes.
3. **If decided (§6):** merging on green.

Later, each its own decision: a `needs()` kind for a report and what
clears it; a look with the coding agent from a watch; recurring *looks* (a
trigger whose occurrence is a look, not a task); backoff; notifications
outside the app; webhooks through a relay; a command as a condition under
policy.

## 8. Acceptance (slice 1)

End to end, with the fake `gh` delivery's tests use and the `scripted`
turn backend (which gains a count of its turns):

- A `ci` watch set on a message fires once when the checks pass; the
  message replayed sets no second watch; otterd killed while the watch is
  `firing` reports once after the restart, on the stored finding.
- A conflicting PR fires “can't run” on the first probe, not at the
  deadline.
- The deadline passed: one message, the watch final.
- No turn on a probe that found nothing.
- A message the developer sends while a watch waits is still answered
  after the report (the report doesn't count as answering it).
- A watch's turn that errors or panics posts a plain line and ends the
  watch; the task's status is untouched. A watch cancelled while firing
  says nothing.
- A turn a watch started can't call `watch`; a fourth waiting watch is
  refused.
- A done task's watch fires (the controller steps it for the watch).
- An overdue `time` watch fires once after a restart and says how late.
- Cancelling the task cancels its watches.

## 9. For the developer to decide

1. Merging on green: (a), (b) or (c) (§6)?
2. *Accept anyway* while CI hasn't finished sets a watch by itself: yes by
   default?
3. Defaults: CI deadline 2 h; probe every 60 s; 3 waiting per task.
4. The word: *watch* (盯) — or *check back*, *reminder*?

## Sources

- Claude Code's own scheduling tools (`ScheduleWakeup`, `CronCreate`,
  `Monitor`): their tool contracts in this session.
- Temporal, [Schedules](https://docs.temporal.io/schedules): overlap
  policies, catch-up window, pause.
- OpenAI, [Tasks in ChatGPT](https://help.openai.com/en/articles/10291617-scheduled-tasks-in-chatgpt):
  active-task caps, at most hourly, unattended tasks pausing.
- GitHub Community, [Does GitHub Actions run on a non-mergeable pull request](https://github.community/t/does-github-actions-run-on-non-mergable-pull-request/126990):
  `pull_request` workflows don't start on a conflicting PR (community and
  staff replies, not the docs; matches #171).
- AWS Architecture Blog, [Exponential backoff and jitter](https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/).
- `gh pr merge --help` (`--auto`, `--match-head-commit`), and
  `gh api repos/alvingaopro/otter` for the repository's settings.
