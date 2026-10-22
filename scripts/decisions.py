#!/usr/bin/env python3
"""Decision numbers that two agents can't both take (D-198).

    scripts/decisions.py new "Title"     claim the next number and write
                                         docs/decisions/D-NNN-<slug>.md
    scripts/decisions.py claim "Title"   claim the next number only; print it
    scripts/decisions.py check           what CI runs (--offline: no claims)

A number is claimed by pushing the tag `decision/D-NNN` to `origin`: a push
of a tag that is already there is refused, so of two agents that pick the
same number, one gets it and the other takes the next. Claim the number when
the work starts, before it is written into code, tests or docs. A claim
whose work is dropped leaves a gap; that's fine.

D-001–D-197 stay in docs/decisions.md. From D-198 on, a decision is a file
of its own in docs/decisions/, so two branches never write the same place.
"""

import re
import subprocess
import sys
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ARCHIVE = Path("docs/decisions.md")
DIR = Path("docs/decisions")
# The last decision kept in the archive; every later one is a file.
ARCHIVE_LAST = 197
TAG = "decision/D-"
HEADING = re.compile(r"^## D-(\d{3,}) — ", re.M)
FILE = re.compile(r"^D-(\d{3,})-[a-z0-9]+(?:-[a-z0-9]+)*\.md$")
TITLE = re.compile(r"^# D-(\d{3,}) — \S")
MENTION = re.compile(r"\bD-(\d{3,})\b")


def git(*args: str, cwd: Path = ROOT, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=cwd, text=True, capture_output=True, check=check)


def claimed(cwd: Path = ROOT) -> set[int]:
    """The numbers claimed on `origin`."""
    out = git("ls-remote", "--tags", "origin", f"refs/tags/{TAG}*", cwd=cwd).stdout
    return {int(m.group(1)) for m in re.finditer(rf"refs/tags/{re.escape(TAG)}(\d+)$", out, re.M)}


def numbers(cwd: Path, rev: str | None = None) -> list[int]:
    """The decisions written: the archive's headings, then the files, in the
    working tree or at `rev`."""
    if rev:
        archive = git("show", f"{rev}:{ARCHIVE}", cwd=cwd, check=False).stdout
        files = git("ls-tree", "--name-only", f"{rev}:{DIR}", cwd=cwd, check=False).stdout.split()
    else:
        path = cwd / ARCHIVE
        archive = path.read_text() if path.exists() else ""
        files = [p.name for p in (cwd / DIR).glob("*.md")] if (cwd / DIR).is_dir() else []
    found = [int(n) for n in HEADING.findall(archive)]
    found += [int(m.group(1)) for f in files if (m := FILE.match(f))]
    return found


def claim(title: str, cwd: Path = ROOT) -> int:
    """Push the next free `decision/D-NNN`; the number it took."""
    git("fetch", "--quiet", "origin", "main", cwd=cwd)
    taken = claimed(cwd) | set(numbers(cwd, "origin/main"))
    n = max(taken, default=0) + 1
    who = git("config", "user.name", cwd=cwd, check=False).stdout.strip() or "someone"
    branch = git("rev-parse", "--abbrev-ref", "HEAD", cwd=cwd, check=False).stdout.strip()
    for _ in range(50):
        tag = f"{TAG}{n:03d}"
        git("tag", "-d", tag, cwd=cwd, check=False)
        git("tag", "-a", tag, "-m", f"D-{n:03d} — {title}\n\nClaimed by {who} on {branch}.", "origin/main", cwd=cwd)
        pushed = git("push", "--quiet", "origin", f"refs/tags/{tag}", cwd=cwd, check=False)
        git("tag", "-d", tag, cwd=cwd, check=False)
        if pushed.returncode == 0:
            return n
        if "already exists" not in pushed.stderr and "rejected" not in pushed.stderr:
            sys.exit(f"couldn't claim D-{n:03d}: {pushed.stderr.strip()}")
        n += 1
    sys.exit("couldn't claim a decision number: 50 were taken in a row")


def slug(title: str) -> str:
    words = re.findall(r"[a-z0-9]+", title.lower())
    out = ""
    for w in words:
        if len(out) + len(w) + 1 > 48:
            break
        out = f"{out}-{w}" if out else w
    return out or "decision"


def new(title: str, cwd: Path = ROOT) -> Path:
    n = claim(title, cwd)
    path = cwd / DIR / f"D-{n:03d}-{slug(title)}.md"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"# D-{n:03d} — {title} ({date.today().isoformat()})\n\n"
        "What was decided, why, and what would make us revisit it.\n"
    )
    return path


def check(cwd: Path = ROOT, offline: bool = False) -> list[str]:
    """What is wrong with the decisions in the working tree, a line each."""
    problems = []
    archive = cwd / ARCHIVE
    text = archive.read_text() if archive.exists() else ""
    seen: dict[int, str] = {}

    def once(n: int, where: str) -> None:
        if n in seen:
            problems.append(f"D-{n:03d} is in {seen[n]} and in {where}: claim another with scripts/decisions.py claim")
        seen[n] = where

    for n in (int(n) for n in HEADING.findall(text)):
        if n > ARCHIVE_LAST:
            problems.append(
                f"D-{n:03d} is in {ARCHIVE}: from D-{ARCHIVE_LAST + 1:03d} on a decision is a file of its own,"
                " made with scripts/decisions.py new"
            )
        once(n, str(ARCHIVE))
    files = sorted((cwd / DIR).glob("*.md")) if (cwd / DIR).is_dir() else []
    for f in files:
        if f.name == "README.md":
            continue
        where = str(DIR / f.name)
        m = FILE.match(f.name)
        if not m:
            problems.append(f"{where}: name it D-NNN-<slug>.md")
            continue
        n = int(m.group(1))
        first = f.read_text().splitlines()[:1]
        t = TITLE.match(first[0]) if first else None
        if not t or int(t.group(1)) != n:
            problems.append(f"{where}: its first line is `# D-{n:03d} — <title> (<date>)`")
        if n <= ARCHIVE_LAST:
            problems.append(f"{where}: D-{n:03d} belongs to {ARCHIVE}'s numbers")
        once(n, where)
    if offline:
        return problems
    claims = claimed(cwd)
    for n, where in seen.items():
        if n > ARCHIVE_LAST and n not in claims:
            problems.append(f"{where}: D-{n:03d} wasn't claimed; make the decision with scripts/decisions.py new")
    # A number written anywhere is one that exists, or one claimed.
    known = set(seen) | claims
    top = max(known, default=0)
    # (This script's tests write made-up numbers on purpose.)
    mentioned = git(
        "grep", "-I", "-o", "-h", "-E", r"\bD-[0-9]{3,}\b", "--", ".", ":!*.lock", ":!scripts/test_decisions.py",
        cwd=cwd, check=False,
    ).stdout
    for n in sorted({int(m) for m in MENTION.findall(mentioned)}):
        if n > top:
            problems.append(f"D-{n:03d} is mentioned but no decision has that number: claim it with scripts/decisions.py")
    return problems


def main(argv: list[str]) -> None:
    match argv:
        case ["new", title]:
            print(new(title).relative_to(ROOT))
        case ["claim", title]:
            print(f"D-{claim(title):03d}")
        case ["check", *rest] if rest in ([], ["--offline"]):
            problems = check(offline=rest == ["--offline"])
            for p in problems:
                print(p, file=sys.stderr)
            if problems:
                sys.exit(1)
        case _:
            sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
