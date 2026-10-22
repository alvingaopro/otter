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
        self.assertEqual(decisions.claim("First", a), 198)
        self.assertEqual(decisions.claim("Second", a), 199)
        self.assertEqual(decisions.claimed(a), {198, 199})
        # The claim says what for, and leaves no tag behind here.
        msg = sh(a, "git", "ls-remote", "--tags", "origin")
        self.assertIn("refs/tags/decision/D-198", msg)
        self.assertEqual(sh(a, "git", "tag", "-l", "decision/*"), "")

    def test_a_number_taken_meanwhile_is_skipped(self) -> None:
        a, b = self.repo.dir, self.repo.clone("b")
        # Both see D-197 as the last; b pushes first.
        self.assertEqual(decisions.claim("b's", b), 198)
        # a's first try is refused; it takes the next.
        real = decisions.claimed
        decisions.claimed = lambda cwd=None: set()
        try:
            self.assertEqual(decisions.claim("a's", a), 199)
        finally:
            decisions.claimed = real
        self.assertEqual(decisions.claimed(a), {198, 199})

    def test_new_writes_the_file_and_check_takes_it(self) -> None:
        a = self.repo.dir
        path = decisions.new("Decision numbers are claimed, one file each", a)
        self.assertEqual(path.name, "D-198-decision-numbers-are-claimed-one-file-each.md")
        self.assertTrue(path.read_text().startswith("# D-198 — Decision numbers are claimed, one file each ("))
        self.assertEqual(decisions.slug("聊天框"), "decision")
        self.assertEqual(decisions.check(a), [])

    def test_check_says_what_is_wrong(self) -> None:
        a = self.repo.dir
        decisions.new("Claimed", a)
        d = a / "docs/decisions"
        # Appended to the archive past its last; a file unclaimed; a
        # duplicate; a bad name and title; a number mentioned that none has.
        (a / "docs/decisions.md").write_text(ARCHIVE + "\n## D-198 — Late (2026-10-23)\n")
        (d / "D-205-unclaimed.md").write_text("# D-205 — Unclaimed (2026-10-23)\n")
        (d / "notes.md").write_text("# Notes\n")
        (d / "D-206-wrong-title.md").write_text("# D-207 — Wrong (2026-10-23)\n")
        (a / "code.rs").write_text("// as D-300 says\n")
        sh(a, "git", "add", "-A")
        problems = "\n".join(decisions.check(a))
        for want in [
            "D-198 is in docs/decisions.md: from D-198 on a decision is a file of its own",
            "D-198 is in docs/decisions.md and in docs/decisions/D-198-claimed.md",
            "docs/decisions/D-205-unclaimed.md: D-205 wasn't claimed",
            "docs/decisions/notes.md: name it D-NNN-<slug>.md",
            "docs/decisions/D-206-wrong-title.md: its first line is `# D-206",
            "D-300 is mentioned but no decision has that number",
        ]:
            self.assertIn(want, problems)
        # Offline, only what the files themselves say.
        self.assertNotIn("wasn't claimed", "\n".join(decisions.check(a, offline=True)))


if __name__ == "__main__":
    unittest.main()
