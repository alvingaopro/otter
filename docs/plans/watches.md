# Watches — Otter looks again later: design

**Status:** proposed (2026-10-22), nothing built. The decision is recorded
(the next free D-number) when slice 1 is built, with the developer's
answers to §8. **Baseline:** `main` at `e456132` (v0.7.70).

> **Core decision:** when the developer asks Otter to keep an eye on
> something, Otter sets a **watch** in that conversation. otterd keeps it,
> looks for what it waits on with deterministic code and no model, and only
> when that happens — or its deadline comes — runs one turn of Otter's
> that says so, in the conversation that asked. A watch is one-shot and
> bounded, looks and says, and never does anything on the developer's word
> but what they gave it when they asked. Recurring *work* stays a trigger
> (D-083).

## 1. Why (dogfooding, 2026-10-22)

In a task's conversation, the task accepted and its pull request (#171)
open, the developer asked “可以帮我盯一下吗?” — can you keep an eye on it
for me? Otter answered that it can't: it only looks when they send a
message. It also said “I just looked again, CI is still running (pending)”.

Two gaps, both verified:

1. **Nothing makes Otter come back.** The engine steps active, review and
   blocked tasks only (`run_controller`); Otter's turn runs on a message
   (D-089), or on the host for a status change of a task Otter made itself
   (D-094). Triggers (D-083) start a new coding task on a schedule — the
   wrong weight for “look at CI in ten minutes”.
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
| Everything is bounded | Claude Code: recurring cron expires after 7 days, a wakeup is clamped to 60 s–1 h; ChatGPT Tasks: a cap on active tasks by plan (3–15), at most hourly, unattended ones may pause | Every watch has a deadline; caps per conversation and host; a minimum delay |
| Overlap and missed runs have a stated policy | Temporal Schedules: overlap *Skip* by default, a catch-up window | Missed while otterd was down: once, as now (D-083's rule); one Otter turn per conversation at a time, the developer's messages first (D-094) |
| Poll with backoff and jitter where there is no push | AWS, “Exponential backoff and jitter”; otterd has no public listener for webhooks (D-083, Not done) | Probe intervals that lengthen, with jitter; webhooks later, through a relay |
| Let the platform do what it does itself | GitHub auto-merge waits on required checks itself | If merging ever is on the developer's word, prefer `gh pr merge --auto` where the repository allows it (§6) |
| Visible and cancellable | ChatGPT Tasks' list; Claude Code's `CronList`/`CronDelete` | A line on the task's page, `otter watch ls/rm`, and `cancel_watch` |
| Authority is given when it's set up, not when it fires | D-083's policy decided in advance; GitHub records who turned auto-merge on | The developer's message that asked is the authority; the turn a watch starts has none beyond it |

## 3. Words

- A **watch**: something Otter looks at again later, for the developer, in
  the conversation where they asked, until what they wait for happens or
  its deadline comes. It fires once.
- A **trigger** (D-083) stays scheduled *work*: a new task each
  occurrence. A watch never starts work; it looks and says.
- D-094's follow-ups are standing watches Otter sets by itself on the tasks
  it makes; slice 2 folds them into this mechanism.

## 4. What can be watched

A closed set, each probed by otterd without a model:

| Kind | Probe | Fires when | Deadline (default, max) |
|---|---|---|---|
| `time` | none | the time comes: “look at X in 30 minutes” | — (1 min to 7 days ahead) |
| `ci` on a PR | `gh pr checks` and `gh pr view --json state,mergeable,headRefOid` | every check finished (passed or failed); or CI can't run (the PR conflicts); or no check started 10 min after the last push; or the PR was merged or closed | 2 h, 7 days |
| `pr` | `gh pr view --json state,reviewDecision` | merged, closed, or reviewed | 24 h, 7 days |
| `task` (host conversation) | otterd's own state | reaches review, done or failed, or waits on the developer | 24 h, 7 days |

- `ci`: a new commit on the PR keeps the watch going on the new head, and
  the report says so. A PR in another repository than the workspace's is
  refused: `gh` runs in the task's workspace with Otter's GitHub sign-in
  where it has one (D-133).
- Not a kind: an arbitrary command as the condition. A command run
  unattended is the policy's business (D-066, D-173); `time` and the
  fire turn's read-only look (§5.3) cover “go and check X later”.

## 5. How it works

### 5.1 Lifecycle and storage

```
developer's message ──watch──► waiting ──probe: not yet──► waiting (probe again later)
                                  │
                                  ├─ it happened / deadline / the time came ──► reported   (final)
                                  └─ cancelled: the developer, or the task cancelled ──► cancelled (final)
```

- Kept on the conversation's own document: `Task.watches` (`task.json`,
  with `WatchSet` / `WatchReported` / `WatchCancelled` in its history) or
  `Chat.watches` for the host's conversation. Each: id, `what` (the
  developer's ask as Otter put it), `asked_in` (their message id), kind
  and target, `next_at`, `deadline`, the last state probed (a short key,
  to tell a change), status.
- Made under `<message id>:watch:<n>`, like every change on the developer's
  word (D-089): a message replayed makes it once.

### 5.2 One loop, cheap probes

- `run_watches`, beside `run_triggers`: every 15 s it probes the watches
  whose `next_at` passed; a task change wakes it early for `task` watches.
- `ci` probes every 30 s for the first 10 min, then every 2 min, then at
  most every 5 min, ±10 % jitter. A probe that fails (network, `gh`) isn't
  a firing: it's tried again later; three failures in a row fire as “can't
  read CI: why”.
- No model on a probe that found nothing.

### 5.3 Firing: one turn, said once

- A turn no message started — `Origin::Watch { watch, asked_in }`, next to
  D-094's `Origin::Event`: the conversation's observe tools and pausing,
  nothing on the developer's word. One exception, for a `time` watch in a
  task: `ask_agent` once, with the watch's own words, because that look is
  exactly what the developer asked for; it only reads (D-091).
- It's told what was asked, when, and what the probe found, with the time
  it was read. Its reply is posted in the conversation once, in the same
  write that marks the watch reported — a crash before posts it after the
  restart, never twice (as D-094). Without Otter's tools, a line: “CI on
  #171 passed: 12 checks.”
- The developer's messages come first; one Otter turn per conversation at
  a time.
- A report that needs the developer — CI failed, the PR conflicts, the
  deadline passed with nothing — is in the Inbox (D-072) until they open
  the task: `Task::needs()` gains a kind for it.

### 5.4 Tools (`gateway.rs`)

| Tool | Class | What |
|---|---|---|
| `watch` | on the developer's word; task and host | `{what, kind: time\|ci\|pr\|task, pr?, task_id?, in_minutes?, for_minutes?}`; a task's `ci` defaults to its own PR |
| `list_watches` | observe | this conversation's watches, waiting and lately reported |
| `cancel_watch` | routine (the safe direction, as pausing) | `{watch_id}` |

- `watch` only in a turn the developer's message started. A watch's own
  turn can't set another: no loop a model keeps alive by itself.
  (Claude Code's self-paced `/loop` lets the model re-arm; Otter
  deliberately doesn't.) “Keep watching” after a deadline is a new watch,
  on a new message.
- Two lines for `INSTRUCTIONS` and `HOST_INSTRUCTIONS`: when the developer
  asks to keep an eye on something or come back later, `watch`, and say
  what for and until when; and say when what you report was read — never
  “I just looked” about something you didn't read in this turn.

### 5.5 Bounds, deterministic

- At most 3 waiting watches per conversation and 20 per host; past that the
  tool refuses and says which are waiting.
- `time`: 1 min to 7 days ahead. Other kinds: deadline at most 7 days.
- Missed while otterd was down or the host slept (`skip_sleep`): probed
  once on start; an overdue `time` watch fires once and says how late.
- A cancelled task's watches are cancelled. A paused task's keep looking:
  they change nothing.
- What's kept and shown is redacted (D-075).

### 5.6 What the developer sees

- In the task's conversation, above the message box: “Watching CI on #171
  · until 17:40 · Cancel”. Times in the developer's local time; the tool
  takes minutes, so the model needs no time zone.
- `otter watch ls [--task <id>]`, `otter watch rm <id>`.
- Protocol: `watches` on the task and the chat (additive, `serde(default)`)
  and `watch.list` / `watch.cancel`. Whether that needs a
  `PROTOCOL_VERSION` bump is checked against D-107 when built.

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
   - a test with a fake `gh` reporting `mergeable: CONFLICTING` and no
     checks.
1. **Watches in a task's conversation:** `time` and `ci`; the three tools;
   the loop; the report and its Inbox item; the line above the message
   box; `otter watch`. Accepting a task whose CI hasn't finished sets a
   `ci` watch, said in Otter's reply and cancellable (a default to confirm,
   §9).
2. **The host's conversation:** `pr` and `task` kinds; D-094's follow-ups
   become `task` watches Otter sets on the tasks it makes.
3. **If decided (§6):** merging on green.

Later, each its own decision: recurring *looks* (a trigger whose
occurrence is a look, not a task), notifications outside the app (macOS),
webhooks through a relay, a command as a condition under policy.

## 8. Acceptance (slice 1)

End to end, with the fake `gh` delivery's tests use and the `scripted`
turn backend:

- A `ci` watch set on a message fires once when the checks pass; the
  message replayed sets no second watch; otterd killed mid-watch fires it
  once after the restart.
- A conflicting PR fires “can't run” on the first probe, not at the
  deadline.
- The deadline passed: one message, the watch final, an Inbox item.
- No turn on a probe that found nothing (the backend counts turns).
- A turn a watch started can't call `watch`; a fourth waiting watch is
  refused.
- An overdue `time` watch fires once after a restart and says how late.
- Cancelling the task cancels its watches.

## 9. For the developer to decide

1. Merging on green: (a), (b) or (c) (§6)?
2. Accepting with CI unfinished sets a watch by itself: yes by default?
3. Defaults: CI deadline 2 h; 3 waiting per conversation, 20 per host.
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
