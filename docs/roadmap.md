# Roadmap — agreed scope

The "Otter" task request arrived with no text. Asked what it should do, the
requester (2026-10-13) answered: work through the roadmap in
`~/Downloads/otter-roadmap`, milestone by milestone, committing, pushing and
validating each until all are done. That roadmap calls itself *proposed*
milestones, not claims of completion: audit the code before each one.

## Milestones, in order

| # | Milestone | Priority | Exit outcome |
|---|---|---|---|
| M1 | Runtime reliability | P0 | Managed (Claude SDK) runs are durable, recoverable and safe under interruption. |
| M2 | Task-to-PR workflow | P0 | One complete task workflow, plan → implement → test → review → PR → CI → done, with durable checkpoints and human control. |
| M3 | Agent manager UX | P1 | Parallel work is understandable and actionable from the desktop inbox. |
| M4 | Observability & diagnostics | P1 | Failures can be diagnosed without exposing secrets or reading raw terminal output. |
| M5 | Multi-agent orchestration | P2 | Several coding agents coordinate without corrupting shared work or losing human control. |
| M6 | Automation & unattended runs | P2 | Controlled unattended execution: triggers, policies, idempotency. |

Each milestone's scope and acceptance criteria are those in the roadmap files
`01-runtime-reliability.md` … `06-automation.md`. Later milestones don't ship
ahead of the P0 release gates.

**Inputs:** the existing daemon, CLI, worker and desktop code, and these
milestone specs. **Outputs, per milestone:** code and tests that meet its
acceptance criteria, its deliverables (state tables, contracts, runbooks)
written into `docs/`, new decisions in `docs/decisions.md`, protocol changes
in `docs/protocol.md`, and evidence in `docs/runtime-validation.md`.

## How each milestone is done

Unless the requester says otherwise (these defaults were proposed and got no
reply, so they stand until changed):

- **Before editing:** audit the code against the milestone. Mark tasks that are
  already met with the code and tests that prove it, rather than redoing them.
- **Git:** one `roadmap-mN` branch per milestone, pushed to `origin`, with a PR
  to `main`, as with earlier runtime milestones. Since 2026-10-14 the
  requester has asked for each PR to be opened and merged once its checks
  pass, and for the work to go on without asking: a PR per plan step. Every
  merge to `main` cuts a release unless the PR is labelled `release:skip`,
  so these PRs carry that label (PRs #53 and #54 didn't, and shipped
  v0.6.21 and v0.6.22).
- **Releases:** none are cut as part of this work.
- **Validation (release gate):** `cargo fmt --all -- --check`,
  `cargo clippy --all-targets`, `cargo test`, the desktop build when UI changes,
  and failure-injection tests for the crash/interrupt scenarios. Scenarios that
  need a live Claude run are recorded as *Pending* in
  `docs/runtime-validation.md`, not run here.
- **Constraints:** changes stay inside this repository. Existing tests stay
  green. `docs/invariants.md` and `AGENTS.md` apply. Domain types stay
  provider-neutral, and secrets are never logged.

## Status (2026-10-14)

| # | State |
|---|---|
| M1 | Done (D-061): explicit run states with legal transitions (`interrupted`, `recovery_required`), a lost agent process resuming instead of failing and starting over, a developer-only `recovery` decision where continuing isn't safe. Built on `roadmap-m1` before M2 and rebased onto it afterwards (a run that needed recovery starts over in a new session, not the task's). State table, crash matrix, operator guide: `runtime-states.md`; evidence: `runtime-validation.md` (live-Claude runs pending). |
| M2 | Plan steps 1–8 merged (PRs #53–#60; D-062…D-069; `docs/plans/m2-feature-to-pr.md`). Offline acceptance met; the live-Claude and real-GitHub scenarios are pending (`runtime-validation.md`, M2-1…M2-7). |
| M3 | Built 2026-10-14 on the requester's instruction, without its spec (`03-agent-manager-ux.md` isn't on this machine) and ahead of the P0 gates, from a scope derived in `docs/plans/m3-agent-manager-ux.md` (D-070): the Inbox and one rule for what a task needs (PRs #61–#63, D-071…D-073). Live scenario pending (M3-1); to be redone against the spec when it arrives. |
| M4 | Built 2026-10-14 on the requester's instruction, without its spec, from a scope derived in `docs/plans/m4-observability.md` (D-074): secrets kept out of what Otter keeps (invariant 13), failures that say why and what next, `otter doctor` (PRs #64–#66, D-075…D-077). Live scenarios pending (M4-1); to be redone against the spec when it arrives. |
| M5 | Built 2026-10-14 on the requester's instruction, without its spec, from a scope derived in `docs/plans/m5-orchestration.md` (D-078): one driver per workspace, a worktree of its own for a waiting task, pause everything (PRs #67–#68, D-079…D-081). Parallel tasks inside a task wait for the spec. Live scenario pending (M5-1). |
| M6 | Built 2026-10-14 on the requester's instruction, without its spec, from a scope derived in `docs/plans/m6-automation.md` (D-082): triggers with one task per occurrence, an unattended policy set in advance (publish, caps, a wait limit) (D-083). Live scenario pending (M6-1). |

## Not in scope

Code signing and notarization, a hosted telemetry backend, and any new
provider beyond Claude and Codex.
