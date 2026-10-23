"""Tests for scripts/decisions.py (D-198), each in a repository of its own
with a bare one as `origin`: run with `python3 -m unittest scripts/test_decisions.py`."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import decisions  # noqa: E402

ARCHIVE = "# Decisions\n\n## D-196 — Typing (2026-10-22)\n\nx\n\n## D-197 — Names (2026-10-22)\n\ny\n"


def sh(cwd: Path, *args: str) -> str:
    return subprocess.run(args, cwd=cwd, text=True, capture_output=True, check=True).stdout


class Repo:
    """A clone with `origin`, its main holding the archive."""

    def __init__(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        base = Path(self.tmp.name)
        self.origin = base / "origin.git"
        sh(base, "git", "init", "-q", "--bare", "-b", "main", str(self.origin))
        self.dir = self.clone("a")
        (self.dir / "docs").mkdir()
        (self.dir / "docs/decisions.md").write_text(ARCHIVE)
        sh(self.dir, "git", "add", "-A")
        sh(self.dir, "git", "commit", "-q", "-m", "archive")
        sh(self.dir, "git", "push", "-q", "origin", "main")

    def clone(self, name: str) -> Path:
        d = Path(self.tmp.name) / name
        sh(Path(self.tmp.name), "git", "clone", "-q", str(self.origin), str(d))
        sh(d, "git", "config", "user.name", name)
        sh(d, "git", "config", "user.email", f"{name}@example.com")
        sh(d, "git", "checkout", "-q", "-B", "main")
        return d


class Claims(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = Repo()

    def tearDown(self) -> None:
        self.repo.tmp.cleanup()

    def test_numbers_follow_the_archive_and_each_other(self) -> None:
        a = self.repo.dir
        n, obj = decisions.claim("First", a)
        self.assertEqual(n, 198)
        self.assertEqual(decisions.claim("Second", a)[0], 199)
        claims = decisions.remote_claims(a)
        self.assertEqual(set(claims), {198, 199})
        self.assertEqual(claims[198], obj)
        # The claim says what for, and no local ref was written.
        self.assertIn("D-198 — First", sh(a, "git", "cat-file", "-p", obj))
        self.assertEqual(sh(a, "git", "for-each-ref", "refs/tags"), "")

    def test_a_number_taken_meanwhile_is_skipped(self) -> None:
        a, b = self.repo.dir, self.repo.clone("b")
        # Both see D-197 as the last; b pushes first.
        self.assertEqual(decisions.claim("b's", b)[0], 198)
        # a looked before b's push: its first try is refused, it takes the next.
        real = decisions.remote_claims
        calls = []

        def stale(cwd: Path = decisions.ROOT) -> dict:
            calls.append(cwd)
            return {} if len(calls) == 1 else real(cwd)

        decisions.remote_claims = stale
        try:
            self.assertEqual(decisions.claim("a's", a)[0], 199)
        finally:
            decisions.remote_claims = real
        self.assertEqual(set(decisions.remote_claims(a)), {198, 199})

    def test_two_worktrees_of_one_clone_get_two_numbers(self) -> None:
        a = self.repo.dir
        w = Path(self.repo.tmp.name) / "w"
        sh(a, "git", "worktree", "add", "-q", "-b", "other", str(w), "origin/main")
        # Worktrees share refs: a claim writes none, or two at once could
        # push each other's (as `git tag` then `push` did).
        ran = []
        real = subprocess.run

        def run(args, *rest, **kw):
            ran.append(list(args))
            return real(args, *rest, **kw)

        decisions.subprocess.run = run
        try:
            n1, o1 = decisions.claim("Here", a)
            n2, o2 = decisions.claim("There", w)
        finally:
            decisions.subprocess.run = real
        self.assertEqual((n1, n2), (198, 199))
        self.assertNotEqual(o1, o2)
        writes = [r for r in ran if r[:2] in (["git", "tag"], ["git", "update-ref"])]
        self.assertEqual(writes, [])
        self.assertEqual(sh(a, "git", "for-each-ref", "refs/tags"), "")

    def test_a_refused_push_says_why(self) -> None:
        hook = self.repo.origin / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\necho no claims today >&2\nexit 1\n")
        hook.chmod(0o755)
        with self.assertRaises(SystemExit) as stop:
            decisions.claim("Refused", self.repo.dir)
        self.assertIn("couldn't claim D-198", str(stop.exception))
        self.assertEqual(decisions.remote_claims(self.repo.dir), {})

    def test_new_writes_the_file_and_check_takes_it(self) -> None:
        a = self.repo.dir
        path = decisions.new("Decision numbers are claimed, one file each", a)
        self.assertEqual(path.name, "D-198-decision-numbers-are-claimed-one-file-each.md")
        lines = path.read_text().splitlines()
        self.assertTrue(lines[0].startswith("# D-198 — Decision numbers are claimed, one file each ("))
        self.assertEqual(lines[1], f"<!-- claim: {decisions.remote_claims(a)[198]} -->")
        self.assertEqual(decisions.slug("聊天框"), "decision")
        self.assertEqual(decisions.check(a), [])

    def test_check_says_what_is_wrong(self) -> None:
        a, b = self.repo.dir, self.repo.clone("b")
        decisions.new("Claimed", a)
        d = a / "docs/decisions"
        # b claimed D-199; a writes it as its own.
        decisions.claim("b's", b)
        (d / "D-199-not-ours.md").write_text("# D-199 — Not ours (2026-10-23)\n<!-- claim: " + "0" * 40 + " -->\n")
        # Appended to the archive past its last; a file unclaimed; a
        # duplicate; a bad name and title; a number mentioned that none has.
        (a / "docs/decisions.md").write_text(ARCHIVE + "\n## D-198 — Late (2026-10-23)\n")
        (d / "D-205-unclaimed.md").write_text("# D-205 — Unclaimed (2026-10-23)\n")
        # Claimed by b, written without its claim line (before there was one).
        _, b_obj = decisions.claim("b's other", b)
        (d / "D-200-no-mark.md").write_text("# D-200 — No mark (2026-10-23)\n")
        (d / "notes.md").write_text("# Notes\n")
        (d / "D-206-wrong-title.md").write_text("# D-207 — Wrong (2026-10-23)\n")
        (a / "code.rs").write_text("// as D-300 says\n")
        sh(a, "git", "add", "-A")
        problems = "\n".join(decisions.check(a))
        for want in [
            "D-198 is in docs/decisions.md: from D-198 on a decision is a file of its own",
            "D-198 is in docs/decisions.md and in docs/decisions/D-198-claimed.md",
            "docs/decisions/D-199-not-ours.md: D-199 was claimed by another",
            "docs/decisions/D-205-unclaimed.md: D-205 wasn't claimed",
            f"docs/decisions/D-200-no-mark.md: say whose claim it is — if D-200 is yours, make its second line `<!-- claim: {b_obj} -->`",
            "docs/decisions/notes.md: name it D-NNN-<slug>.md",
            "docs/decisions/D-206-wrong-title.md: its first line is `# D-206",
            "D-300 is mentioned but no decision has that number",
        ]:
            self.assertIn(want, problems)
        # Offline, only what the files themselves say.
        offline = "\n".join(decisions.check(a, offline=True))
        self.assertNotIn("wasn't claimed", offline)
        self.assertNotIn("claimed by another", offline)


if __name__ == "__main__":
    unittest.main()
