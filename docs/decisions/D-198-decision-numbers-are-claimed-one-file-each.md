# D-198 — Decision numbers are claimed, one file each (2026-10-22)

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
  `docs/decisions/D-NNN-<slug>.md`; `claim "Title"` only claims and prints
  the number. A push of a tag that already exists is refused, so of two
  agents that pick the same number one gets it and the other takes the next
  — GitHub decides, no lock, no service. The next number is one past the
  highest claimed or written on `main`. A claim whose work is dropped leaves
  a gap; that's fine. Tags don't release anything: releases run on pushes to
  `main`.
- **One file per decision**, from D-198 on: `docs/decisions/D-NNN-<slug>.md`,
  its first line `# D-NNN — Title (date)`. Two branches never write the same
  place. `docs/decisions.md` keeps D-001–D-197 as they are, and says where
  the rest are.
- **CI checks it** (`python3 scripts/decisions.py check`, the *Decisions*
  job): no number twice; nothing after D-197 in `docs/decisions.md`; each
  file named and titled with its number; each number from D-198 on claimed;
  no `D-NNN` written anywhere in the repository past the highest that
  exists or is claimed. A branch that took a number without claiming it, or
  appended to the old file, fails with what to do.
- **Not chosen:** the pull request's number (they overlap D-186–D-197
  already, and one pull request often holds several decisions); numbering
  at merge time (CI takes minutes, and two merges still race in them); names
  instead of numbers (longer to cite, and every habit and document uses
  numbers).

Tests: `scripts/test_decisions.py` (a bare repository as `origin`):
numbers follow the archive and each other; a number another clone pushed
first is skipped; `new` writes the file and `check` accepts it; `check`
names each mistake (appended to the archive, a duplicate, unclaimed, a bad
name, a wrong title, a number mentioned that no decision has). This
decision's own number was claimed with the script.
