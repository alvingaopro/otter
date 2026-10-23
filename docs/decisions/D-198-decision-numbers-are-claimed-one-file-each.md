# D-198 — Decision numbers are claimed, one file each (2026-10-22)
<!-- claim: 473156ac64b26c0b8f60c265efcd539a97599f68 -->

Several coding agents work on Otter at once, and decision numbers kept
colliding. An agent took the last number on `main` plus one when it began,
and wrote it into code comments, test names and docs as it went (D-190–D-197
are mentioned 236 times in `crates` and `apps`). Another agent did the same,
and whichever merged second renumbered everything; on 2026-10-22 one change
was renumbered twice (D-191 → D-195 → D-196) while #182 and #184 merged. And
every decision was appended to the end of one 7 500-line file, so any two
branches that each added one conflicted there.

- **A number is claimed, on `origin`, before it is used.**
  `scripts/decisions.py new "Title"` pushes the tag `decision/D-NNN`
  (annotated: the title, who, which branch) and writes
  `docs/decisions/D-NNN-<slug>.md`, whose second line names its claim
  (`<!-- claim: <tag object> -->`). A push of a tag that already exists is
  refused, so of two agents that pick the same number one gets it and the
  other takes the next — GitHub decides, no lock, no service. The tag
  object is made with `git mktag`, with a nonce, and pushed by its id: no
  local ref is written, since every worktree of a repository shares them
  (two agents' worktrees of one clone would otherwise push each other's
  tag), and `origin` is asked afterwards that it holds this one. Only
  "already exists" moves on to the next number; any other refusal stops
  with what git said. The next number is one past the
  highest claimed or written on `main`. A claim whose work is dropped leaves
  a gap; that's fine. Tags don't release anything: releases run on pushes to
  `main`.
- **One file per decision**, from D-198 on: `docs/decisions/D-NNN-<slug>.md`,
  its first line `# D-NNN — Title (date)`. Two branches never write the same
  place. `docs/decisions.md` keeps D-001–D-197 as they are, and says where
  the rest are.
- **CI checks it** (`python3 scripts/decisions.py check`, the *Decisions*
  job): no number twice; nothing after D-197 in `docs/decisions.md`; each
  file named and titled with its number; each number from D-198 on claimed,
  by this file (its `claim` is the tag `origin` holds, so a branch that
  wrote a number someone else claimed fails); no `D-NNN` written anywhere
  in the repository past the highest that exists or is claimed. Python 3.9
  is enough (the Mac's own `python3`). A branch that took a number without claiming it, or
  appended to the old file, fails with what to do.
- **Not chosen:** the pull request's number (they overlap D-186–D-197
  already, and one pull request often holds several decisions); numbering
  at merge time (CI takes minutes, and two merges still race in them); names
  instead of numbers (longer to cite, and every habit and document uses
  numbers).

Tests: `scripts/test_decisions.py` (a bare repository as `origin`):
numbers follow the archive and each other; a number another clone pushed
first is skipped; two worktrees of one clone get two numbers and leave no
local tag; a push the server refuses stops with its reason; `new` writes
the file and `check` accepts it; `check` names each mistake (appended to
the archive, a duplicate, unclaimed, claimed by another, a bad name, a
wrong title, a number mentioned that no decision has). This decision's own
number was claimed with the script.

Reviewed against the code (Fable) before merging. The review found that
two worktrees of one repository could push the same claim (the script made
and deleted a local tag, which worktrees share), that a refusal for any
reason was taken for a number already taken, and that `check` didn't ask
whose claim a number was; all three are fixed above.
