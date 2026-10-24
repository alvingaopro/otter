# Who decides what is risky: Otter's agent, not a regex

Status: built in D-174 (milestone 1, the policy's own fixes, is D-173). The
open questions below are answered there. Where the build differs from this
text, `docs/decisions.md` D-174 is right. The task's own worktrees are no
longer found from the repository's worktree list: D-215 counts the
workspace's directory and the worktrees the developer allowed the task to
make.

What the new decision would supersede, so it is written down once:

- D-049's rule that deleting, destroying and any word-listed command asks the
  developer (`docs/decisions.md:1412`), for the judged classes below;
- the rule (in `decide_open`, `crates/daemon/src/controller.rs:1358`) that a
  developer-only ask is never shown to a model, for the judged classes (the
  floor keeps it);
- D-044's "a denial can't be turned around by anyone"
  (`docs/decisions.md:1219`, `crates/daemon/src/runtime/policy.rs:27`), for a
  denial **by the model** (a denial by the developer or by policy stays final);
- D-066's "`$HOME`, system paths and other directories still ask"
  (`docs/decisions.md:2142`), for the task's own worktrees only.

D-066's "credentials are never granted" (`docs/decisions.md:2155`) is not
changed.

## Problem

A task stops and waits for the developer far more often than it should. Each
stop is a "high risk" decision that is only a word in a command line.

What the developer has hit:

- **Searches that ask.** A `grep` whose pattern holds a second alternative
  with a risky word (`grep -E 'a|token' x`, or `grep -rn 'delete\|token' src`)
  is developer-only, "touches credentials (`token`)". The policy splits the
  line at every `|`, quoted or not (`crates/daemon/src/runtime/policy.rs:23`
  says quotes are not understood), so the text after the `|` is read as a
  command of its own, and `token` in it counts. The same search with one
  alternative, `grep -rn token src`, is fine since D-138
  (`docs/decisions.md:4734`). I cannot see the developer's exact lines; this
  is the class of search I can reproduce.
- **Edits that ask, directory by directory.** Editing files in a worktree
  that sits next to the workspace (the usual way to keep a branch apart) is
  "edits outside the workspace", developer-only
  (`crates/daemon/src/runtime/policy.rs:1266`). Approving one is remembered
  only for that exact directory (`crates/daemon/src/runtime/policy.rs:1350`,
  the grant key is `edit:<dir>`), so the next directory asks again.
- **A skill call is "medium risk".** Any tool policy does not know, a skill
  included, is `Ask` with `Risk::Medium`
  (`crates/daemon/src/runtime/policy.rs:1301`). That one is not
  developer-only, so the model may decide it, but a host without a model
  always escalates it (`crates/daemon/src/brain.rs:639`), and the task waits.

Why it keeps happening: the policy is a lexical reader. It cannot tell a word
in a pattern from a word in a command, so each case got its own exception:
reading is not doing (D-138), `git rm` of the work's own files is routine
(D-163), a delete is routine only where the policy can tell what it deletes
(D-164), what a line only prints is not what it does (D-170). Each fix is
right, and the next false alarm is a different shape. The developer asked for
the judgment to be made by Otter's agent, which can read the whole call.

## Current behaviour

1. The coding agent asks to run a tool. The run asks the policy:
   `policy::classify` (`crates/daemon/src/runtime/policy.rs:1260`) at
   `crates/daemon/src/runs.rs:548` (an interaction) and
   `crates/daemon/src/runs.rs:583` (a hook check). The run's directory is the
   workspace root (`crates/daemon/src/runs.rs:296`); where a command really
   works is worked out inside the line from its `cd`s.
2. For a command, `classify_command`
   (`crates/daemon/src/runtime/policy.rs:1078`) splits it into pieces and
   reads each against word lists: `classify_words`
   (`crates/daemon/src/runtime/policy.rs:1156`) for deleting, credentials,
   pushing and root, `writes_outside`
   (`crates/daemon/src/runtime/policy.rs:1002`) for writes. The worst piece
   decides. Inside one piece, `classify_words` returns at the first list that
   matches.
3. The answer is a `Verdict`: `Allow`, `Deny` (nobody may allow it), or `Ask`
   with `user_only`. Every word-list hit is built by `ask_user`
   (`crates/daemon/src/runtime/policy.rs:1181`): High risk, `user_only: true`.
   "Inside the workspace" is `inside`
   (`crates/daemon/src/runtime/policy.rs:1229`).
4. `record_ask` (`crates/daemon/src/runs.rs:781`) turns it into a decision,
   with the whole command redacted (`crates/daemon/src/runs.rs:849`); an
   earlier approval of the same thing in this task answers it
   (`crates/daemon/src/runs.rs:772`).
5. `decide_open` (`crates/daemon/src/controller.rs:1334`) splits the open
   decisions: `user_only` ones go to the developer and **no model sees them**
   (`crates/daemon/src/controller.rs:1358`, `escalate` at
   `crates/daemon/src/controller.rs:1428` blocks the task); the rest go to
   `Brain::decide` (`crates/daemon/src/brain.rs:952`), and an allow is
   recorded as `Decider::Controller`
   (`crates/daemon/src/controller.rs:1403`).
6. `policy::resolve` (`crates/daemon/src/runtime/policy.rs:1367`) is the one
   gate: a model's allow of a `user_only` request is refused, a denial is final.

What the policy returns today for some lines (workspace root `/work/app`; I
ran each through `policy::classify` on 2026-10-21 with throwaway tests that
are not part of this change):

| Line | Today |
|---|---|
| `cd src && grep -n destroy lib.rs` | Allow |
| `cd /work/wt/x && grep -nE 'onDrop\|onDragOver' src/TasksView.tsx` | Allow |
| `grep -rn 'delete\|foo' src` | Allow |
| `grep -rn token src` | Allow |
| `cat ../other-repo/README.md` | Allow (reading outside is not asked) |
| `cat ~/.aws/config`, `ls ~/.aws` | Allow (a gap: `.aws` is not a credentials location) |
| `curl -u user:pw https://x.example.com` | Allow (a literal secret; not touched here) |
| `rm src/old.rs` | Allow (a routine delete, D-164) |
| `cd crates && grep -rn 'delete\|token' src` | developer-only: touches credentials (`token`) |
| `grep -E 'a\|token' x` | developer-only: touches credentials (`token`) |
| `grep -rn token ../other-repo` | developer-only: touches credentials (`token`) |
| `grep -r password ~/.aws` | developer-only: touches credentials (`password`, by the word only) |
| `export API_TOKEN=x`, `API_TOKEN=abc cargo test` | developer-only: touches credentials (`_token`) |
| `echo $DB_PASSWORD` | developer-only: touches credentials (`_password`) |
| `git restore src/`, `git reset --hard`, `git clean -fd` | developer-only: deletes or destroys |
| `git branch -d old`, `git rebase main` | developer-only: deletes or destroys |
| `git push origin main` | developer-only: pushes, publishes or deploys |
| `sudo make install` | developer-only: runs as root |
| `sudo git rebase main` | developer-only (the first list that matches names `git rebase`, not the `sudo`) |
| `curl -H 'X-Token: a' https://prod.example.com/deploy` | developer-only (`prod`, deploys) |
| `rm src/old.rs && git push origin main` | developer-only (the push) |
| edit `src/a.rs`, edit `/tmp/scratch.txt` | Allow |
| edit `/work/wt/x/src/a.rs`, `/home/u/.bashrc`, `/etc/hosts`, `/home/u/.ssh/config` | developer-only: edits outside the workspace |
| the `Skill` tool | `Ask`, medium, not developer-only: the model decides |

The second line is worth a note: `grep -nE 'onDrop|onDragOver'` is allowed
only because neither `onDrop` nor `onDragOver` is a risky word. Put `token`
in the second alternative and it is developer-only.

## Proposal

Four changes, in the order they matter.

1. **Deterministic code stays as the floor**, and the floor is **strictest
   wins, inside a piece too**: `classify_words` collects every hit and takes
   the worst, instead of returning at the first list that matches (today every
   hit is developer-only, so the order did not matter; once some are judged,
   `sudo git rebase main` must not come out as the judged `git rebase`, which
   is what `classify_words` would return before it reaches the `sudo` check).
2. **A judged class** is an `Ask` with `user_only: false`, built by a second
   constructor next to `ask_user` (`crates/daemon/src/runtime/policy.rs:1181`).
   `decide_open` (`crates/daemon/src/controller.rs:1358`) already sends such
   an ask to `Brain::decide` (`crates/daemon/src/brain.rs:952`). The judged
   classes are only what is **recoverable or only reads**; see the two tables.
3. **The task's own worktrees are inside the workspace.** A worktree of the
   workspace's own repository is the same project, so an edit there is routine
   (`Allow`), not a judged call and not a model call per edit. The daemon
   works out those roots and gives them to `inside`
   (`crates/daemon/src/runtime/policy.rs:1229`); every other place outside
   (`~/.bashrc`, `/etc/hosts`, another repository) stays the floor.
4. **The floor's credential list grows**: `.aws`, `.kube`,
   `.config/gcloud`, `.netrc`, `.docker/config.json`, `.npmrc` and `.pypirc`
   join `CREDENTIAL_FILES` (`crates/daemon/src/runtime/policy.rs:129`), because
   today only the word, not the place, keeps `grep -r password ~/.aws` on the
   floor, and `cat ~/.aws/config` is allowed.

```mermaid
flowchart TD
    A[Agent asks to run a tool] --> B[policy::classify]
    B -->|never-list or Deny| X1[Refused. Final]
    B -->|Allow: routine, own worktrees included| R1[Runs, recorded as routine]
    B -->|Ask, developer-only: the floor| D[The developer decides]
    B -->|Ask, judged| M[Otter's model: Brain::decide, with a deadline]
    M -->|allow, with a rationale| R2[Runs. Decider::Controller, rationale on the decision]
    M -->|deny, with a rationale| X2[Refused this time. A repeat goes to the developer]
    M -->|escalate, error, deadline or no model| D
    D -->|approve| R3[Runs]
    D -->|deny| X3[Refused. Final]
    R2 -.->|policy::resolve| G[A developer-only ask is never allowed by the model]
```

```
tool call -> classify -+-> Allow ........................ runs
                       +-> Deny (never) ................. refused, final
                       +-> developer-only (floor) ....... the developer
                       +-> judged ---> Otter's model --+-> allow ... runs, why recorded
                                                       +-> deny .... refused once; a repeat
                                                       |            goes to the developer
                                                       +-> escalate / error / deadline /
                                                           no model --> the developer
```

This is not a small change in the code. The model needs to see the call, and
`Brain::decide` receives only a `Context` and a `DecisionRequest`
(`crates/daemon/src/brain.rs:952`), which has no directory, no inside-or-outside
and no edit path. New fields on `DecisionRequest` are a change to what Otter
keeps and sends (the store, `docs/protocol.md`, invariant 12 in
`docs/invariants.md:136`, as D-157 was), and what the model is shown is
redacted, as the decision's command already is
(`crates/daemon/src/runs.rs:849`, invariant 13 in `docs/invariants.md:150`).

## What stays developer-only

The floor. These are decided by the developer or by nobody:

| Class | Example | Today | After |
|---|---|---|---|
| The never-list (`rm -rf /`, a download piped into a shell) | `rm -rf /` | refused | refused |
| Pushing, publishing, deploying | `git push origin main` | developer | developer |
| Running as root | `sudo make install` | developer | developer |
| Throwing away work Git cannot bring back | `git restore src/`, `git reset --hard`, `git clean -fd`, `git checkout -- .`, `git stash drop` | developer | developer |
| A delete the policy cannot tell is the work's own | `rm` outside the routine case (D-164) | developer | developer |
| A credentials file or place (the list grows, change 4), a secret as an assignment or a variable, a credential command | `grep -r password ~/.aws`, `export API_TOKEN=x`, `echo $DB_PASSWORD`, `printenv`, `gh auth` | developer | developer |
| Infrastructure and data destruction | `kubectl delete pod web`, `drop table` | developer | developer |
| The broad destructive words, until they are measured | `docker system prune --volumes`, `helm uninstall`, a script named `delete-db` (the words `prune`, `uninstall`, `delete`, `destroy`, `terminate`) | developer | developer |
| An edit outside the workspace and its worktrees | edit `/home/u/.bashrc`, `/etc/hosts`, `/home/u/.ssh/config` | developer | developer |
| A line with a floor piece and any other piece | `rm src/old.rs && git push origin main` | developer | developer |
| A floor hit and a judged hit in one piece | `sudo git rebase main` (root, and a rebase) | developer | developer |

A line takes the strictest class of its pieces
(`crates/daemon/src/runtime/policy.rs:180` ranks them), and, with change 1,
of the hits inside one piece. A judged hit can never lower a floor hit.

## What the agent judges

What the word lists flag by a word, where the damage can be undone or there is
none:

| Class | Example | Today | After |
|---|---|---|---|
| A credential *word* as a search or test argument of a reader, including a word that a quoted `\|` cut into a piece | `cd crates && grep -rn 'delete\|token' src`, `grep -E 'a\|token' x`, `grep -rn token ../other-repo` | developer | Otter's model |
| Local history that comes back from the reflog | `git branch -d old`, `git rebase main` | developer | Otter's model |
| A tool the policy does not know | the `Skill` tool | the model, if there is one | Otter's model (unchanged), see the open questions |

What the agent does not need to judge, because the policy already gets it right
or change 3 makes it routine:

| Line | Today | After |
|---|---|---|
| `cd src && grep -n destroy lib.rs` | Allow | Allow |
| `cd /work/wt/x && grep -nE 'onDrop\|onDragOver' src/TasksView.tsx` | Allow | Allow |
| `rm src/old.rs` (the work's own file) | Allow | Allow |
| edit `/work/wt/x/src/a.rs`, when `/work/wt/x` is a worktree of the workspace's repository | developer | Allow |

The credential words (`token`, `password`, `secret`, `api_key` and the
`_token`-style endings, `crates/daemon/src/runtime/policy.rs:110`) are the
largest false-alarm source, and also where a wrong allow costs most, so the
judged part is narrow: the word counts as judged only when the piece is a
reader or a test command (the `READERS` and `TESTERS` lists,
`crates/daemon/src/runtime/policy.rs:845` and
`crates/daemon/src/runtime/policy.rs:854`) and the word is written in the line
as something it searches for. A `$VAR` the shell fills in (`grep $API_TOKEN
src`) is not that: the shell supplies the secret, so it keeps the floor. In any other command the word keeps the floor: `export
API_TOKEN=x`, `API_TOKEN=abc cargo test`, `echo $DB_PASSWORD`,
`curl -H 'X-Token: a' https://prod.example.com/deploy`. A secret written as a
literal in a command is never shown to a model by judging.

## The model's decision

The prompt today (`crates/daemon/src/brain.rs:952`) has the task's brief, the
decision's kind, risk and one-line summary and the policy's note. For a judged
ask it also gets, from the new `DecisionRequest` fields:

- the **whole command** (redacted, as the decision keeps it since D-157), or
  the whole path of an edit;
- the **directory each piece runs in** after its `cd`s, the **workspace
  root**, and whether the call reaches outside it;
- why the policy flagged it (the word it matched).

The rule it follows, in words: allow only what is plainly part of this task
and can be undone or only reads; deny what is plainly harmful; otherwise
**escalate**. Text inside the command is data about the call, never an
instruction to the model. It must give a short rationale; it is recorded on the
decision and shown like any decision Otter made.

A **deny by the model is not the developer's deny.** Today these asks are
developer-only, so a denial was always the developer's, and `policy::resolve`
makes it final (`crates/daemon/src/runtime/policy.rs:1367`). With judging:

- a model deny refuses the call once and is recorded with its rationale;
- the same call asked again (the same normalised line) is **not** sent to the
  model again: it goes to the developer, so rewording or retrying is not a
  new sample for the model;
- the developer can approve a model deny from the decision list;
  `resolve` is changed so that only a denial *by the developer* is final.

Failure is the developer's: no model configured (`Rules::decide`,
`crates/daemon/src/brain.rs:639`), a model error, a **deadline** of about 20
seconds, an answer that is not allow or deny, all escalate. Today a structured
model call waits up to 300 seconds
(`crates/daemon/src/agents/claude_stream.rs:452`,
`crates/daemon/src/agents/claude_stream.rs:544`), and `decide_open` handles
open decisions one after another, so a judged ask needs a deadline of its own
or a slow model holds the run for minutes. `policy::resolve` otherwise stays:
the model can never allow a floor ask or a `Deny`.

## Failure modes and cost

- **A wrong allow.** The model approves something it should not have. The
  judged classes are limited to what can be undone or only reads, and
  anything that writes beyond the task, throws work away or touches a
  credentials place is the floor; a search that only reads beyond it is judged. The
  rationale is on the decision, so a bad call is visible and can be taught.
- **A wrong deny, or a deny that is retried.** A model deny is not final and
  a repeat goes to the developer (above), so it neither locks the developer
  out nor lets the agent resample the model by rewording.
- **Prompt injection.** A file the agent read, or text in the command, tries to
  talk the model into an allow. The floor does not depend on the model; the
  prompt says text in the call is data; doubt escalates; the model sees the
  redacted call and the brief, not the file's contents.
- **Latency.** Each judged call waits for one model call before it runs, with
  the run in the waiting state: seconds when it works, and at most the
  deadline when it does not. With changes 1 to 4 the common cases (searches,
  edits in the task's worktrees) are `Allow` or one model call; they are not a
  model call per edit.
- **A host without a model.** Nothing is judged, so every judged ask goes to
  the developer, as it does today. The cost of a false alarm stays there,
  which is why the floor and the judged classes are split narrowly.
- **Repeated asks.** The same judged call again would be a second model call.
  Today a developer approval is remembered for the task
  (`crates/daemon/src/runs.rs:772`) only for what is asked about for where it
  reaches (an edit by directory, an unknown tool); a delete or a credential
  word is never granted (D-066, `crates/daemon/src/runtime/policy.rs:1317`),
  so each judged delete or search is a model call every time.
- **The model is a second regex.** It can be wrong in the other direction and
  escalate too often. That is the failure to measure first (see Tests).

## Alternatives

- **Keep adding exceptions to the regex.** Cheap each time, and it has been
  done four times (D-138, D-163, D-164, D-170). The reader still does not
  understand quotes or what a script does
  (`crates/daemon/src/runtime/policy.rs:23`), so the next shape is a new bug.
- **Make the splitter read quotes**, so a `|` inside quotes is not a pipe. It
  would fix the `grep -E 'a|token' x` class deterministically and is worth
  doing whatever else is decided; it does not help the other judged classes,
  and the module has said since D-059 that it is not a parser.
- **Grant a whole directory tree**, instead of treating the task's own
  worktrees as inside (change 3). A grant is asked once and then remembered;
  change 3 never asks. It can be had without a model.
- **A trust level in Settings** (strict, normal, trusting). It moves the
  decision to the developer once instead of every time, and gives up the
  judgment: a trusting level approves a delete the agent should have been asked
  about. It can sit on top of this design, not replace it.
- **Why this one:** the developer asked for it, the machinery is there
  (`Brain::decide`), and the floor keeps the hard guarantees deterministic.

## Tests

In `crates/daemon/src/runtime/policy.rs` (next to
`looking_for_a_risky_word_is_not_doing_it`,
`crates/daemon/src/runtime/policy.rs:1948`), with workspace root `/work/app`:

- Allow, as today: `cd src && grep -n destroy lib.rs`,
  `cd /work/wt/x && grep -nE 'onDrop|onDragOver' src/TasksView.tsx`,
  `grep -rn 'delete\|foo' src`, `grep -rn token src`, `rm src/old.rs`,
  `cat ../other-repo/README.md`.
- Judged (`Ask` with `user_only: false`; developer-only today):
  `cd crates && grep -rn 'delete\|token' src`, `grep -E 'a|token' x`,
  `grep -rn token ../other-repo`, `git branch -d old`, `git rebase main`.
- Developer-only or refused, unchanged: `grep -r password ~/.aws` (now by the
  place as well as the word), `cat ~/.aws/config` (new: today it is Allow),
  `cat ~/.ssh/id_ed25519`, `grep -n password .env`, `grep $API_TOKEN src`,
  `echo $DB_PASSWORD`, `export API_TOKEN=x`, `API_TOKEN=abc cargo test`,
  `git push origin main`, `git restore src/`, `git reset --hard`,
  `git clean -fd`, `kubectl delete pod web`, `sudo make install`,
  `rm -rf /`, an edit of `/home/u/.bashrc`, `/etc/hosts` or
  `/home/u/.ssh/config`.
- Strictest wins: `sudo git rebase main` (one piece, a floor hit and a judged
  hit),
  `curl -H 'X-Token: a' https://prod.example.com/deploy` and
  `rm src/old.rs && git push origin main` are developer-only.
- Own worktrees: an edit of `/work/wt/x/src/a.rs` is Allow when `/work/wt/x`
  is a worktree of the workspace's repository, and developer-only when it is
  an unrelated directory.

In `crates/daemon/tests/e2e.rs`, with the scripted and yes-man brains: a judged
ask is decided by `Decider::Controller` with a rationale and the run goes on;
a model that escalates, the rules brain, a failing model and a model past its
deadline leave it waiting for the developer; a model deny refuses once, the
same call again goes to the developer, and the developer can approve it; a
developer's denial stays final; credentials and push still wait. A unit test
that the prompt carries the whole call, the directories and the root, redacted.

Measure before shipping: run the judged classes over the bench's recorded calls
(`.dev/bench`, if its records are kept) and count allow, deny and escalate; a
model that escalates almost everything has not removed the problem.

## Open questions

- **Should a model's allow be remembered for the task**, as the developer's
  approval is for an unknown tool (D-066, `docs/decisions.md:2150`)? It
  saves a model call; it also makes one wrong allow last the whole task.
  Judged deletes and credential words stay un-granted either way, which
  `policy::grant` already ensures, so those are a model call each time.
- **Can the developer switch judging off** in Settings (everything judged goes
  to them), or on a task? The no-model host already behaves that way.
- **What goes on the infrastructure-destroy list** (the floor):
  `kubectl delete`, `terraform destroy`, `helm uninstall`, `docker rm*`,
  `drop table`, `truncate`, `delete from`, `flushall`? Some of these are
  routine in a scratch database; the list decides how much stays developer-only.
- **Should the broad destructive words ever be judged** (`prune`,
  `uninstall`, `delete`, `destroy`, `terminate`)? They match too much that
  cannot be undone (`docker system prune --volumes`), so this design leaves
  them on the floor until the measurement above says a model can tell them apart.
- **How are the task's own worktrees found?** From the Git repository's
  worktree list, once per run, or from where Otter made them. A worktree the
  developer made by hand next to the workspace would be inside only in the
  first case.
- **A secret written as a literal** (`curl -u user:pw https://x.example.com` is
  Allow today) is a gap in the floor, separate from this design; it should be
  looked at when the credential list grows.
- **The skill and unknown-tool class** is already model-decided; is "medium"
  the right label, or should a skill the developer installed be allowed as
  routine?
