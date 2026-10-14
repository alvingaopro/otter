# M3 — Agent manager UX: implementation plan

**Milestone:** M3 (P1) of [`docs/roadmap.md`](../roadmap.md): *parallel work
is understandable and actionable from the desktop inbox.*
**Written without the milestone's spec.** The roadmap takes each
milestone's scope and acceptance from `03-agent-manager-ux.md`, which isn't
on this machine; the requester asked to go on regardless (2026-10-14). The
scope below is derived from the exit outcome, design §21 and §26, the
autonomy audit's attention findings ([05](../audits/autonomy/05-conversation-attention.md)),
and an audit of the code (§2) — each item says where it comes from. When
the spec arrives, it wins: this plan is redone against it (D-070).
**Status:** in progress — step 1 (D-071). **Branch:** `roadmap-m3`, from `main` after M2.

## 1. What "the desktop inbox" means here

Design §26 draws one list — NEEDS YOU, WORKING, COMPLETED — of everything
the developer has going on, closer to "browser tabs + an inbox" than to a
terminal multiplexer. With features (D-043) there are two kinds of work in
flight: workspaces (sessions an agent or the developer drives) and features
(work Otter drives). *Understandable*: from the list alone, the developer
can tell what each is doing, what waits on them, and since when.
*Actionable*: what waits on them can be done from the list — approve, deny,
answer, accept, retry, open — without opening each item and finding the
right tab.

## 2. The code against that (audit)

Already met, kept as is:

| What | Where | Proof |
|---|---|---|
| Workspaces grouped NEEDS YOU / WORKING / COMPLETED / IDLE / ARCHIVED, longest-waiting first | `apps/desktop/src/model.ts` `groups`; activity from `otter-core` (`Workspace::activity`) via `src-tauri/src/view.rs` | `model.test.ts` |
| A notification for each new workspace attention, and a jump to it | `App.tsx` (notification effect) | `App.test.tsx` |
| The menu-bar item lists workspaces that need you | `App.tsx` `tray_update` | — |
| Features: a "Needs you" filter, needs-first order, a badge count | `FeaturesView.tsx`, `features.ts` `needsYou`, `sortFeatures` | `FeaturesView.test.tsx` |
| A decision is answerable in the feature's Approvals tab, forms included | `FeaturesView.tsx` `Decision`, `QuestionForm` | `FeaturesView.test.tsx` |
| One agent message per turn; activity live, not stored per tool | D-069 | daemon e2e |

The gaps:

1. **Two inboxes.** Features and workspaces are separate views with
   separate lists and badges (`FeaturesView` vs `Sidebar`); nothing shows
   all that needs the developer in one place (design §26).
2. **Two rules for "needs you".** A workspace's activity is computed by
   `otter-core` and served to the app (`view.rs`: "the frontend never
   re-implements them"); a feature's is `needsYou()` in TypeScript, and it
   leaves out a failed feature, which waits on the developer's retry
   (audit 05 §4).
3. **Not actionable from the list.** A feature row says "2 decisions
   waiting"; acting means opening it, then its Approvals tab.
4. **Waiting time isn't the order.** Workspaces sort longest-waiting first;
   features by `updated_at`, which every tool line used to bump.
5. **Features don't notify.** A feature needing the developer changes a
   badge count only: no notification, nothing in the menu-bar item.
6. **`otter ls` leaves features out** (`crates/cli/src/dashboard.rs`).

## 3. Steps

Each lands as its own PR (`release:skip`), with tests; the release gate runs
on each — the desktop build through CI's Desktop job, as the Tauri crate
doesn't build on the machine this is written on.

### Step 1 — One rule for what a feature needs (gaps 2, 4)

`otter-core`: `Feature::needs() -> Option<Need>` — what waits on the
developer, as `kind` (a decision, a question, review, blocked, failed),
`summary` (the decision's summary, the status reason), `since` (the
decision's `created_at`, else when the status last changed) and the
decision's id when there is one; and `Feature::activity() -> Activity` on
the workspaces' scale (failed counts as needing them, as for workspaces).
The app gets both computed, the way `view.rs` serves a workspace's activity
(`src-tauri/src/features.rs`), and `features.ts` stops computing its own.
Features needing the developer sort longest-waiting first.

### Step 2 — The inbox (gaps 1, 3)

A view of its own, first in the activity bar: NEEDS YOU, WORKING, DONE,
across features and workspaces, on every connected host. A row says what it
is, what it's doing now (`statusDetail`; a workspace's `headline`), what
waits and for how long. A feature's pending decision is answered in place
(the same `Decision` card the Approvals tab uses); review offers Accept and
"Open"; failed offers Retry; a workspace that needs the developer opens to
its session. The Features and Workspaces views stay.

### Step 3 — Told when it matters (gaps 5, 6)

A notification for each new thing a feature needs (a decision, review,
blocked, failed), once, unless that feature is in front of the developer;
the menu-bar item lists features with workspaces. `otter ls` lists features
that need the developer, with what and since when, under NEEDS YOU.

### Step 4 — Record

Decisions (D-071 onward), design §26 brought up to date, and the live
scenario — several features and workspaces in parallel on two hosts,
handled from the inbox — recorded as pending in `runtime-validation.md`.

## 4. Not in this plan

Without the spec, nothing that adds daemon state or changes how work runs:
no changes to `attention.rs`, the controller or policy, no new requests
where a value derived from what is already served does. Candidates for when
the spec arrives: unread markers ("what changed since you looked"),
grouping by project or repository, bulk actions, keyboard triage.

## 5. Acceptance (derived)

- [ ] Everything that waits on the developer — feature decisions, reviews,
  blocked and failed features, workspaces that need them — is in one list,
  longest-waiting first, across hosts.
- [ ] Each can be acted on from that list: a decision answered, a review
  accepted, a failed feature retried, a workspace opened.
- [ ] A feature's "needs you" is computed once, in `otter-core`, and the app,
  the notifications and `otter ls` agree on it.
- [ ] A new need notifies once.
