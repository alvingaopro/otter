# M6 — Automation & unattended runs: implementation plan

**Milestone:** M6 (P2) of [`docs/roadmap.md`](../roadmap.md): *controlled
unattended execution: triggers, policies, idempotency.*
**Written without the milestone's spec** (`06-automation.md` isn't on this
machine), on the requester's instruction to go on, as M3–M5 were: the scope
is derived from the exit outcome, the M2 plan ("don't add a 'bypass' mode
for unattended work — that's M6, with its own policy"), D-066 (what may be
granted) and an audit of the code (§2). When the spec arrives, it wins
(D-082).
**Status:** done (D-082, D-083). The live scenario is pending (`runtime-validation.md` M6-1). **Branch:** `roadmap-m6`, from `main` after M5.

## 1. The outcome, read from the three words

- **Triggers:** work starts without a person at the keyboard — on a
  schedule kept by the host, or fired by hand from a script.
- **Policies:** what such work may do without the developer is decided by
  the developer *in advance*, on the trigger — never by a model in the
  moment (D-044, D-066), never a bypass. What wasn't decided in advance
  still waits for them; unattended work just doesn't wait for ever.
- **Idempotency:** a trigger occurrence starts exactly one feature,
  whatever restarts or repeats happen; external actions already are
  idempotent (D-068).

## 2. The code against that (audit)

Already met: creating a feature is idempotent by command id
(`FeatureStore::applied`); delivery's push and pull request are idempotent
and resumable (D-068); budgets bound a feature's work (attempts, time,
D-063); policy decides what runs without asking, and an approval can be
remembered for a feature (D-066); a publish can be allowed for a feature
(`allowed_commands`, checked by `allowance`); `otter feature new` / `start`
script a feature by hand.

The gaps:

1. **No trigger:** nothing starts work by itself; a cron job calling the CLI
   would have no idempotency across retries and no overlap rule.
2. **No unattended policy:** a feature started for unattended work blocks
   on its first developer-only ask (publishing included) and waits for
   ever, its agent process idling.
3. **Nothing records that work was unattended**, nor which trigger started
   it.

## 3. Steps

One PR (`release:skip`), with tests.

- **Triggers on the host** (`state/triggers.json`): a name, a workspace, a
  request, a schedule (every N minutes/hours, or daily at HH:MM UTC), and
  its policy. otterd fires a due trigger by creating and starting a
  feature with the command id `trigger:<id>:<occurrence>` — one feature per
  occurrence, however often it's fired. Missed occurrences (otterd was
  down) fire once, the latest, not each; an occurrence whose previous
  feature is still at work is skipped and said. `otter trigger add / ls /
  rm / run` (run: now, by hand).
- **The trigger's policy, decided in advance:** budget caps (attempts,
  minutes of work) for its features; `--publish` grants publishing to the
  workspace's branch (the push and pull request D-047 otherwise asks
  for) — the developer's approval, given ahead and on record; a wait
  limit: a developer-only decision that waits longer than it pauses the
  feature (its agent stops; it says why; resuming asks again) instead of
  idling. Nothing else changes: what policy asks the developer, it still
  asks them; no model approves on their behalf.
- **On record:** a triggered feature says which trigger and occurrence
  started it (`Feature.trigger`), and that it's unattended.

## 4. Not in this plan

Webhooks and CI events as triggers (no listener in otterd; a script can
`otter trigger run`), cron expressions beyond "every" and "daily",
cross-host triggers, notifications by mail or chat — for the spec.

## 5. Acceptance (derived)

- [x] A trigger fired twice for one occurrence starts one feature; one
  whose last feature is still at work is skipped
  (`a_trigger_starts_one_feature_per_occurrence_and_doesnt_overlap`;
  missed ones fire once: `missed_occurrences_fire_once_after_a_restart`).
- [x] A triggered feature with `--publish` delivers without asking; without
  it, the publish waits, and past the wait limit the feature pauses
  (`an_unattended_feature_publishes_when_its_trigger_allows_it`,
  `an_unattended_feature_pauses_when_it_waits_too_long`).
- [x] A triggered feature records its trigger and occurrence, and its
  budget caps hold (the first test).
- [ ] With live Claude, overnight — pending (M6-1).
