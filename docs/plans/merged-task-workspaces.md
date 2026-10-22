# A finished task's workspace goes once its work is merged: design

**Status:** built in D-189 (2026-10-22), with the recommended answers to
§7: built now, 24 hours, on by default, a pull request closed without
merging keeps its workspace. Where the build differs from this text,
`docs/decisions.md` D-189 is right. It differs here: the branch a new
workspace gets is `otter/<title>-<id>-<n>`, a name never pushed. Condition
5 (§4.1) only keeps a workspace when the pull request's head is known
locally; D-108's guard is the one that keeps what is on no remote. §4.4's
"why" is `merged`, and a new workspace is made only when Otter deleted the
old one once it was merged: deleted by the developer, the task waits for a
workspace.
**Baseline:** `main` at `f85208c` (v0.7.80).

What it would change, so it is written down once:

- D-105's "files, branch and conversations kept" for a task's own
  workspace: kept while the work isn't merged; after the task's pull request
  is merged (and a grace period, §4.5), the workspace is deleted the way
  `workspace delete` deletes it (D-108).
- D-105's line "Unarchive it to look at the work" holds until then.

Not changed: D-036 (archiving isn't deleting: the developer's archive stays
an archive), D-108's guards (uncommitted changes and commits on no remote
stop a delete without force), design §29 (an attached folder is never
deleted), and the rule that cleanup only touches what Otter made under
`~/.otter`.

> **Core decision:** otterd looks, now and then, at each archived workspace
> made for a task. When the task's pull request is merged on GitHub, the
> workspace holds nothing that isn't there, and the grace period has
> passed, otterd deletes it with the ordinary delete, without force, and
> the task's conversation says so. Anything else, and it stays archived as
> today. Plain code, no model, nothing asked of the developer.

## 1. Why

On 2026-10-22 the dev VM's disk was 81% full: 230G in 33 git worktrees
under `~/Project/otter-wt`, nearly all of it `target/` (5–23G each). Those
were made by hand by Claude Code sessions working on Otter, not by Otter.
They are fixed apart (the sessions now remove a worktree once its pull
request merges). The developer then asked whether Otter does the same:
deletes a workspace's worktree when the workspace is deleted, and when a
task ends.

The first it does (D-108, D-135). The second it doesn't, on purpose: a
task's own workspace is archived when the task ends (D-105) and kept
forever, worktree, branch and build output included. It is the same shape
as the 230G, one worktree per piece of work, never collected.

**Evidence, honestly:** none yet. This host's otterd (`~/.workd`) has four
workspaces, none archived and none made for a task. The task bench
(`.dev/bench`) has 2 archived task workspaces among 61, 13M in all. No
other host was looked at. This design is for a backlog that is expected,
not one that has been seen, so it is kept small (§6) and the developer
decides whether to build it now (§7).

## 2. Current behaviour

- **A task ends:** `put_away_workspace` (`crates/daemon/src/tasks.rs:1339`,
  called from the engine's loop at `controller.rs:244`) archives the
  workspace made for the task (`ws.task_id`), unless the task still asks
  something or a session there is in use. The conversation says "its files
  and branch are kept. Unarchive it to look at the work."
- **Then nothing looks at it again.** Once its last message is answered, a
  done task is never stepped (watches.md §1). An archived workspace stays
  until the developer deletes it.
- **Deleting** (`workspace_delete`, `crates/daemon/src/workspaces.rs:349`;
  `delete_stopped`, `:416`): without force, uncommitted changes stop it,
  and so do commits on the branch that are on no remote (`git.rs:395`,
  `unpushed`). Commits re-applied by a rebase merge or squashed into one
  count as landed (`git.rs:419`, compared by patch id). A branch with a
  merge commit of its own isn't landed that way. Then the worktree, the
  local branch (`git.rs:477`, never a remote one) and, when nothing else
  uses it, the repository's copy go. An attached folder is never touched.
- **What GitHub says of a pull request** is read by `GitHub::pr_state`
  (`delivery.rs:987`): `OPEN`, `MERGED` or `CLOSED`, its head and base.
  The CI watch reads it with Otter's own GitHub sign-in (D-133:
  `git_account.gh_env`) from Otter's directory, naming the repository taken
  from the pull request's URL (`watches/mod.rs:454`, `watches/ci.rs:73`).
- **A task asked for more after done** (D-050) brings its own archived
  workspace back: `workspace_taken` unarchives it and waits while it
  prepares (`controller.rs:1692`). A workspace that is gone isn't handled:
  `store.workspace(..)?` finds nothing, so the step goes on, and
  `task_workspace` (`controller.rs:635`) fails with "no workspace". That
  is already so when the developer deletes a task's workspace by hand.
- **Surfaces:** the task's Changes section is left out without a workspace
  (D-147). The "Task ↗" chip is on the workspace and goes with it (D-146).

## 3. Words

- A task's **own workspace**: one made for it (D-080), `ws.task_id` the
  task. A workspace the developer chose for a task is never touched
  (D-105), and neither is this design.
- **Merged**: GitHub says the task's pull request is `MERGED`. Not "pushed",
  not "on a remote": a branch pushed and still in review is on a remote.

## 4. Design

### 4.1 What qualifies

All of:

1. The workspace is the task's own, archived, and from Git.
2. The task is done or cancelled, and asks nothing.
3. The task has a pull request on GitHub (`delivery.pr_number`, and a
   `pr_url` that `repo_of` reads).
4. GitHub says it is `MERGED`, at least the grace period ago (`mergedAt`).
5. The workspace's `HEAD` is the pull request's head (`headRefOid`) or
   behind it: nothing was committed there after the pull request's last
   push.
6. The host setting is on (§4.6).

Never: a workspace that is ready (the developer unarchived it: it is
theirs, as in D-105), a task with no pull request or that declined
publishing (`delivery.declined`), a pull request closed without merging,
an attached folder or empty workspace, a host without `gh`. Each stays
archived, as now.

### 4.2 The sweep

A loop of its own, beside `run_watches`: every 30 minutes with jitter, the
first a minute after otterd starts (`OTTER_MERGED_SWEEP_MS` for tests). Each
pass reads which workspaces qualify by 1–3 and 6 from the store, then asks
GitHub about each once: `gh pr view <n> --repo <owner/name> --json
state,mergedAt,headRefOid`, with the CI watch's environment and limit. What
it learns is kept in memory only: a pull request closed without merging is
asked about again after a day (it can be reopened), one still open on the
next pass. A restart asks again once. One call per archived own workspace
per pass; GitHub allows 5000 an hour.

**Not a watch.** A watch only looks and says, fires once, and ends in a
model's turn (D-183). This acts, needs no model, and has no deadline. It
reuses only `GitHub::pr_state`'s read (extended with `mergedAt`).

**Not git alone.** "Its commits are on a remote" is true of a branch in
review. Telling merged from pushed by git needs a fetch of each repository
and the patch-id check, and is still wrong for a branch with its own merge
commit. The pull request is what the task delivered (D-047), so GitHub's
word on it is the signal. Hosts without `gh` keep today's behaviour.

### 4.3 The delete

The ordinary `workspace_delete`, `force: false`, so D-108's guards decide
what is lost: nothing that isn't elsewhere. When it refuses (uncommitted
changes, or commits on no remote, such as a squash-merged branch that
carried its own `Merge origin/main`), the workspace stays archived. The
conversation says why, once, and the sweep leaves that workspace alone
from then on: an additive `Workspace.kept` with the reason, cleared when
the workspace is unarchived. It never retries with force.

"Uncommitted changes" includes untracked files that aren't ignored
(`git.rs:614`, `status --porcelain --untracked-files=normal`). A scratch
script or a log a coding agent left in the tree is enough to keep the
workspace. That is the safe way to fail, but it may well be the usual
one. It is the first thing to watch in dogfooding, and `kept`'s reason
says which it was: untracked files, changed files, or commits on no
remote.

### 4.4 The task afterwards

On success the task's `workspace_id` is cleared, and an additive
`Task.workspace_gone` keeps what is needed later: the workspace's name, its
repository (as stored, D-075), the pull request's base, when, and why. The
conversation says, as D-105's line does:

> Its workspace *name* is deleted: pull request #*n* was merged, and
> everything in it is on *base*. Asked for more, the task starts again from
> *base*.

Asked for more after that (D-050), the task gets a new workspace of its own
from the same repository, branched from the base, which already has the
merged work. If that can't be made, it is blocked with no workspace, and
D-186's placement takes over, as for a new task. The old branch isn't
brought back: its work is on the base.

This also fixes the gap in §2 for a workspace the developer deletes: the
delete writes the same record on the task whose own workspace it was, so
that task gets the same path, not a "no workspace" error.

### 4.5 Grace period

D-105 tells the developer they can unarchive the workspace to look at the
work. Once the pull request is merged, the work is on the base branch and
the pull request keeps the diff and the review, so little is lost. A short
wait still covers looking right after merging. Proposed: 24 hours after
`mergedAt`. During it nothing changes. An unarchived workspace is out of
the sweep while it is unarchived. Archived again, it qualifies again.

### 4.6 Setting and surfaces

- A host setting, *Delete a task's workspace once its work is merged*,
  next to *Otter's tools* in the app's Settings (`settings.rs`,
  `Option<bool>` like `otter_tools`). On by default (§7).
- The protocol gains only additive fields (`Settings`, `Task.workspace_gone`,
  `Workspace.kept`): no protocol bump (`docs/protocol.md`).
- The app needs nothing new to show it: `WorkspaceDeleted` already takes the
  workspace out of the list, the line is in the conversation, the Changes
  section is left out without a workspace (D-147), and "Open workspace" is
  hidden without `workspace_id` (`apps/desktop/src/TasksView.tsx:392`).

## 5. Tests

End-to-end, with the fake `gh` the e2e tests already have
(`crates/daemon/tests/e2e.rs:2366`) answering `pr view`:

- merged past the grace period, clean: deleted (worktree, branch, record),
  the line said, `workspace_id` cleared, `workspace_gone` written;
- merged inside the grace period, open, closed without merging: kept;
- merged but with uncommitted changes, or a commit after the pull
  request's head: kept, one line, and not tried again;
- unarchived, chosen by the developer rather than made for the task, no
  pull request, setting off: never asked about;
- asked for more after deletion: a new workspace from the base, and the
  task goes on;
- the developer deletes a task's workspace by hand, then asks for more:
  the same.

A unit test for what qualifies (§4.1) as a pure function of the task, the
workspace and what GitHub said.

## 6. Size

One slice: the sweep (one file, about the size of `watches/time.rs` plus
`watches/ci.rs`), two additive fields, the reopen path, the setting, the
tests above. No app work beyond the setting's switch.

## 7. Open questions for the developer

1. **Build it now, or wait for a backlog to show?** No host has one yet
   (§1). Recommended: build it. It is small, the reopen fix (§4.4) is
   needed anyway, and a task workspace's `target/` is gigabytes.
2. **Grace period:** 24 hours after merge (recommended), at once, or 7 days?
3. **On by default?** Recommended yes: what goes is only what is already
   on GitHub, and the switch turns it off.
4. **A pull request closed without merging:** keep its workspace forever
   (recommended for now, it may hold work the developer still wants), or
   delete it after a while?
