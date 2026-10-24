# D-215 — The policy reads what a command runs, not the words in its data; a task's own space is its workspace's directory (2026-10-24)
<!-- claim: 11c66b506c9694bf56b9eec754e8cdab8db70913 -->

A task asked the developer for things that were only words. Appending a
test with `cat >> crates/daemon/tests/e2e/host_chat.rs <<'EOF' … EOF` and
running the tests on the next line was "deletes or destroys something
(`delete`)": the body's `delete` was read as a command, because a body was
data only when its line had no other command (D-170). A `grep` for
`delete(` was read whole, because a quoted `(` made it "do more" (D-138),
and so was any search reaching outside the workspace (D-174). A worktree
the task made beside its workspace was "outside", every write asked, and
a `Skill` call was a tool for someone to decide on. The developer's
direction: read the command line deterministically, as the program that
runs and its arguments, and treat a heredoc's body, a quoted string, a
search's pattern and what `echo` prints as data; judge a write by which
task owns the worktree, not by which repository it is of; grant the
boundary once, at the start, not call by call. Otter's agent judging from
the context is a later step.

What changed (`crates/daemon/src/runtime/policy.rs`, its module doc says
the same):

- **A piece is read as words** (`words`), the way the shell passes them:
  quotes and the backslashes that escape go; what is quoted, what the
  shell expands (a `$` outside single quotes) and what is a redirection or
  the shell's own syntax (an unquoted `<`, `(`, `)`) is known word by word.
- **Data does not meet the word lists** (`reading`, `data_words`,
  `patterns`). A read inside the workspace: the command only, as before,
  and its patterns no longer count as files for credentials either
  (`grep -rn credentials src` runs; `cat src/credentials.rs` still asks).
  A quoted `(`, `<` or a single-quoted `$` in a pattern no longer makes it
  "do more". A read that reaches outside: deleting, pushing and root are
  read in the command alone; a credential word it looks for is still the
  model's to judge (D-174). The reader behind `xargs` is a read of what
  `xargs` is given, which isn't known (judged the same way). What `echo`
  and `printf` print, when their output goes nowhere that runs it (no
  pipe, no substitution, no other descriptor, no file the line then runs).
  The message of `git commit`, `tag`, `merge`, `notes` and `stash`. A
  variable expanded in any of these still counts for credentials
  (`echo $GITHUB_TOKEN`, `git commit -m "$DB_PASSWORD"` ask). A quoted
  string is data in these places because of where it stands, not because
  it is quoted: a quoted command a shell runs (`sh -c '…'`), SQL given to a
  client (`psql -c 'DROP TABLE …'`) and a word spelled apart (`kubectl
  dele''te`) are still read.
- **A heredoc's body that `cat` or `tee` writes is data** unless its output
  goes on (a pipe, `>(…)`, `>&3`, a capture), its body is expanded, a file
  it writes can't be told (`> "$F"`), or something else on its text may
  run a file it writes (`may_run`): a shell or an interpreter (python,
  node, ruby, perl, php, deno, bun; also behind `xargs` or `find -exec`)
  given a word naming it by its last part, a glob, a variable or a
  substitution; the file run as a program (`./run.sh`); or a command the
  policy can't read (`eval`, a variable, a substitution). This replaces
  D-170's "the line's only command".
- **The never-list is read where a command runs.** A shell that reads a
  pipe is denied by its own piece (`into_a_shell`): `| sh`, `|& bash`,
  `| sudo -u x bash -s`, `| env X=1 /bin/sh`, in a substitution or in a
  body a shell runs, and now `zsh`, `dash` and `ksh` too. `||` is not a
  pipe. `rm -rf /`, `rm -rf ~`, `chmod -R 777 /` and the fork bomb are
  read in each piece's command, not in its data: `grep -rn '| sh' docs`,
  `echo rm -rf /`, a note with `curl x | sh` in it written by `cat`, and
  `make || sh fallback.sh` are no longer denied. `sh -c 'curl x | sh'`
  still is.
- **A task's own space** (`Scope::owned`, `runs.rs` `verdict_of`): its
  root; the directory Otter made for its workspace (`<home>/workspaces/<id>`,
  when it holds the root: Git and empty workspaces), where `git worktree
  add ../x` puts a worktree; and the worktrees the developer allowed it to
  make elsewhere. Approving a line whose only ask is a `git worktree
  add|move` outside keeps the grant `tree:<path>` (`grant_in`), and
  `verdict_of` reads the task's `tree:` grants into the scope. Its ask says
  so ("makes a worktree outside the workspace (…); allowed, it is this
  task's, and the task works there without asking"), since that is what
  the developer allows; any other write outside asks as before. Never a
  tree that is `/`, the root or holds it, a home directory or holds one, a
  login's place, or Otter's home, in it or holding it (`may_own`, checked
  when granted and again when read). Deleting is still judged against the
  root alone (D-066), and Git's internals and credentials still ask there.
  `git worktree list` is no longer run (`delivery::Git::worktrees` is
  gone), and the scope is the same for every call, not only for a
  developer-only ask.
- **`Skill` is a read** (`agents/claude_stream.rs` `tool_call`): it loads a
  skill's instructions; each tool it then has the agent use is asked about
  on its own.

Superseded:

- D-138's "credentials by file name stay read whole … whatever the
  command", for a search's patterns: they are what it looks for, not files.
  Its test expected `grep -rn "rm -rf /" docs` to ask the developer; it now
  runs (the pattern is data).
- D-170's rule that a body is data only when its piece is the line's only
  command; and the never-list over the whole line's text.
- D-174's "the task's own worktrees are the other worktrees of the
  workspace's repository, from `git worktree list`, none under Otter's
  home". A worktree of the same repository that the task didn't make (one
  the developer made by hand beside a directory workspace) now asks, and
  one the task made in its workspace's directory no longer does. D-174's
  judged classes are unchanged.

Still lexical, on purpose or for later:

- Every argument of a program the policy has no data rule for is read by
  the word lists as before (`mytool --message "delete it"` asks). So is the
  code given to an interpreter other than a shell (`python3 -c …`, a
  heredoc fed to `python3 -`): the policy doesn't read Python.
- `sed`, `awk` and `gh` have no data rules yet (`sed -i 's/delete/remove/'
  x` still asks). A quoted `>` is still read as a redirection when looking
  for writes outside.
- What a build tool runs is not read (`cat > Makefile <<E … E; make`), as
  for a file the agent edits and then builds. A `cat` inside a body that a
  shell runs, whose file the outer line runs, is read only in its body's
  text, as before.
- What a judged call is told about where its pieces run (`explain`, for
  `brain::judged_prompt`) still knows only the root: a piece in the task's
  own worktree is said to be outside the workspace.
- Otter's agent judging a call from the task's context, rather than a
  rule, is the next step the developer named; the policy stays the floor.

Tests (`policy.rs`): `data_in_a_command_is_not_what_it_does` and
`what_a_heredoc_writes_is_data_unless_the_line_runs_it` (both watched
failing on the code before: `grep -rn "delete(" src`, and the `cat >>
host_chat.rs` line asking "deletes or destroys something (`delete`)"),
`a_task_owns_its_workspace_directory_and_the_trees_it_was_allowed` (with a
real repository and a worktree of it the task didn't make),
`real_risk_is_still_stopped`; `a_skill_is_reading` in `claude_stream.rs`.
Every earlier policy test passes unchanged but the one `grep` above. Not
verified in a running otterd with a real agent: no end-to-end test was
added, and the e2e suite passes as it was.

Revisit if a word the lists no longer read in data turns out to hide what
a command does, if agents commonly make worktrees outside their workspace's
directory (each asks once), or when Otter's agent judges calls in context.
