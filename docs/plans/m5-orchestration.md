# M5 — Multi-agent orchestration: implementation plan

**Milestone:** M5 (P2) of [`docs/roadmap.md`](../roadmap.md): *several coding
agents coordinate without corrupting shared work or losing human control.*
**Written without the milestone's spec** (`05-…` isn't on this machine), on
the requester's instruction to go on, as M3 and M4 were (D-070, D-074): the
scope is derived from the exit outcome, the M2 plan's "don't run parallel
tasks in a shared directory (a worktree per task, `git.rs` already supports
it)", and an audit of the code (§2). When the spec arrives, it wins (D-078).
**Status:** steps 1–4 done (D-079…D-081). The live scenario is pending (`runtime-validation.md` M5-1). **Branch:** `roadmap-m5`, from `main` after M4.

## 1. The outcome, read narrowly

Several coding agents already run at once: one per feature, each feature on
a workspace, plus whatever agents the developer starts in sessions. What
the outcome asks is that this is **safe** and **controlled**:

- *Without corrupting shared work:* two agents never write the same
  directory at the same time, and one's work isn't published as another's.
- *Without losing human control:* the developer sees what runs where, and
  can hold or stop it — all of it at once included.

Parallel tasks *inside* one feature — several agents splitting a plan and
merging back — is the larger reading. It runs against D-064 (one
conversation per feature) and needs merge policy the code has no evidence
for; it's left for the spec (§4).

## 2. The code against that (audit)

Already met, kept as is: one run at a time per feature (`start_run`: "a run
is already working on this feature"); a feature's run and an interactive
session of the same feature's *task* don't drive one conversation at once
(D-064's check); Git workspaces are worktrees of one shared repository, each
on its own branch (`git.rs`), so a workspace per line of work is cheap; the
Inbox shows every feature and workspace that's working (D-072).

The gaps:

1. **Two features can run agents in one directory at once.** Nothing stops
   a second feature's run in a workspace another feature's run is using;
   or a feature's run alongside an agent the developer started in that
   workspace. They edit the same files, and delivery pushes one branch with
   both features' work.
2. **No way out but waiting by hand:** the developer has to notice, and
   make another workspace, to run two features side by side.
3. **No stop-everything.** Pause and cancel are per feature; with several
   agents at work on a host there's no single control to hold them all.

## 3. Steps

Each lands as its own PR (`release:skip`) with tests.

### Step 1 — One driver per workspace (gap 1)

A workspace is driven by at most one managed run at a time; a feature whose
next turn needs a workspace another feature's run holds waits — visibly, on
the Inbox ("waiting for <workspace>: <other feature> is working there") —
and goes on when it's free (whichever steps first, D-079). A run doesn't start in
a workspace where an agent session the developer started is working;
a session started there while a run works is told so.

### Step 2 — A feature of its own (gap 2)

Starting a feature on a Git workspace another feature holds offers its own
workspace: a worktree of the same repository on a branch for the feature
(`otter/<feature>`), made like any workspace (D-010). The CLI (`--own-
workspace`), the app's New feature dialog, and the busy notice offer it.
Directory workspaces can't be split this way; they wait.

### Step 3 — Stop everything (gap 3)

`otter feature pause --all [--host]` and an Inbox action hold every
feature's run (each pauses as a pause does: the turn interrupted, the
conversation held); resume one by one, or all.

### Step 4 — Record

Decisions (D-079 onward), the live scenario — three features on one
repository, two in parallel worktrees, one waiting, all paused and resumed
— pending in `runtime-validation.md`.

## 4. Not in this plan

Parallel tasks inside a feature with merging, agent-to-agent messaging, a
planner that splits work across agents, cross-host scheduling — for when
the spec arrives, with evidence from dogfooding (architecture-lessons
§24–25).

## 5. Acceptance (derived)

- [x] Two features never run agents in one directory at once; the second
  waits, says why, and goes on by itself (`a_second_feature_waits_for_the_workspace`).
- [x] A feature can be given its own worktree of a busy repository, and
  both run in parallel, each delivering only its own work
  (`a_waiting_feature_gets_a_worktree_of_its_own`).
- [x] Every run on a host can be held at once, and resumed (CLI e2e
  `features_and_the_coding_runtime_from_the_terminal`, `Inbox.test.tsx`).
- [ ] With live agents on a real repository — pending (M5-1).
