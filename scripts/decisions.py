#!/usr/bin/env python3
"""Decision numbers that two agents can't both take (D-198).

    scripts/decisions.py new "Title"     claim the next number and write
                                         docs/decisions/D-NNN-<slug>.md
    scripts/decisions.py check           what CI runs (--offline: not the claims)

A number is claimed by pushing the tag `decision/D-NNN` to `origin`: a push
of a tag that is already there is refused, so of two agents that pick the
same number, one gets it and the other takes the next. The file says which
claim is its own (`<!-- claim: <tag object> -->`), and CI checks it against
`origin`. Make the decision when the work starts, before its number is
written into code, tests or docs. A claim whose work is dropped leaves a
gap; that's fine.

D-001–D-197 stay in docs/decisions.md. From D-198 on, a decision is a file
of its own in docs/decisions/, so two branches never write the same place.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import time
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ARCHIVE = Path("docs/decisions.md")
DIR = Path("docs/decisions")
# The last decision kept in the archive; every later one is a file.
ARCHIVE_LAST = 197
TAG = "refs/tags/decision/D-"
HEADING = re.compile(r"^## D-(\d{3,}) — ", re.M)
FILE = re.compile(r"^D-(\d{3,})-[a-z0-9]+(?:-[a-z0-9]+)*\.md$")
TITLE = re.compile(r"^# D-(\d{3,}) — \S")
MARK = re.compile(r"^<!-- claim: ([0-9a-f]{40,64}) -->$", re.M)
MENTION = re.compile(r"\bD-(\d{3,})\b")


def git(*args: str, cwd: Path = ROOT, input: str | None = None) -> str:
    done = subprocess.run(["git", *args], cwd=cwd, text=True, input=input, capture_output=True)
    if done.returncode != 0:
        sys.exit(f"git {' '.join(args[:2])}: {done.stderr.strip() or done.returncode}")
    return done.stdout


def remote_claims(cwd: Path = ROOT) -> dict[int, str]:
    """The numbers claimed on `origin`, each with its tag object."""
    out = git("ls-remote", "--tags", "origin", f"{TAG}*", cwd=cwd)
    return {int(m.group(2)): m.group(1) for m in re.finditer(rf"^(\w+)\t{re.escape(TAG)}(\d+)$", out, re.M)}


def numbers(cwd: Path, rev: str) -> list[int]:
    """The decisions written at `rev`: the archive's headings, then the files."""
    archive = subprocess.run(["git", "show", f"{rev}:{ARCHIVE}"], cwd=cwd, text=True, capture_output=True).stdout
    files = subprocess.run(
        ["git", "ls-tree", "--name-only", f"{rev}:{DIR}"], cwd=cwd, text=True, capture_output=True
    ).stdout.split()
    return [int(n) for n in HEADING.findall(archive)] + [int(m.group(1)) for f in files if (m := FILE.match(f))]


def claim(title: str, cwd: Path = ROOT) -> tuple[int, str]:
    """Push the next free `decision/D-NNN`: its number and tag object.

    The tag object is made here and pushed by id; no local ref is written,
    since other worktrees of this repository, other agents, share them. A
    nonce makes it this claim's alone, and `origin` is asked afterwards
    whose it holds."""
    git("fetch", "--quiet", "--no-tags", "origin", "main", cwd=cwd)
    base = git("rev-parse", "origin/main^{commit}", cwd=cwd).strip()
    who = git("var", "GIT_COMMITTER_IDENT", cwd=cwd).strip()
    name = who.split(" <")[0]
    branch = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"], cwd=cwd, text=True, capture_output=True
    ).stdout.strip()
    nonce = f"{os.getpid()}.{time.time_ns()}.{os.urandom(4).hex()}"
    n = max(set(remote_claims(cwd)) | set(numbers(cwd, "origin/main")), default=0) + 1
    for _ in range(50):
        tag = f"decision/D-{n:03d}"
        body = (
            f"object {base}\ntype commit\ntag {tag}\ntagger {who}\n\n"
            f"D-{n:03d} — {title}\n\nClaimed by {name} on {branch} ({nonce}).\n"
        )
        obj = git("mktag", cwd=cwd, input=body).strip()
        pushed = subprocess.run(
            ["git", "push", "--porcelain", "origin", f"{obj}:{TAG}{n:03d}"],
            cwd=cwd,
            text=True,
            capture_output=True,
        )
        status = next((line for line in pushed.stdout.splitlines() if f"{TAG}{n:03d}" in line), "")
        if pushed.returncode == 0 and status.startswith("*"):
            if remote_claims(cwd).get(n) != obj:
                sys.exit(f"claimed D-{n:03d}, but origin holds another claim for it: try again")
            return n, obj
        if "already exists" in status or "cannot lock ref" in status:
            n += 1
            continue
        sys.exit(f"couldn't claim D-{n:03d}: {(status or pushed.stderr).strip()}")
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
    n, obj = claim(title, cwd)
    path = cwd / DIR / f"D-{n:03d}-{slug(title)}.md"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"# D-{n:03d} — {title} ({date.today().isoformat()})\n"
        f"<!-- claim: {obj} -->\n\n"
        "What was decided, why, and what would make us revisit it.\n"
    )
    return path


def check(cwd: Path = ROOT, offline: bool = False) -> list[str]:
    """What is wrong with the decisions in the working tree, a line each."""
    problems = []
    archive = cwd / ARCHIVE
    text = archive.read_text() if archive.exists() else ""
    seen: dict[int, str] = {}
    marks: dict[int, str | None] = {}

    def once(n: int, where: str) -> None:
        if n in seen:
            problems.append(f"D-{n:03d} is in {seen[n]} and in {where}: make yours with scripts/decisions.py new")
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
        content = f.read_text()
        t = TITLE.match(content.splitlines()[0]) if content else None
        if not t or int(t.group(1)) != n:
            problems.append(f"{where}: its first line is `# D-{n:03d} — <title> (<date>)`")
        if n <= ARCHIVE_LAST:
            problems.append(f"{where}: D-{n:03d} belongs to {ARCHIVE}'s numbers")
        mark = MARK.search(content)
        marks[n] = mark.group(1) if mark else None
        once(n, where)
    if offline:
        return problems
    claims = remote_claims(cwd)
    for n, where in seen.items():
        if n <= ARCHIVE_LAST:
            continue
        if n not in claims:
            problems.append(f"{where}: D-{n:03d} wasn't claimed; make the decision with scripts/decisions.py new")
        elif marks.get(n) != claims[n]:
            problems.append(
                f"{where}: D-{n:03d} was claimed by another (the file's `<!-- claim: … -->` isn't origin's);"
                " make yours with scripts/decisions.py new"
            )
    # A number written anywhere is one that exists, or one claimed. (This
    # script's tests write made-up numbers on purpose.)
    known = set(seen) | set(claims)
    top = max(known, default=0)
    mentioned = subprocess.run(
        ["git", "grep", "-I", "-o", "-h", "-E", r"\bD-[0-9]{3,}\b", "--", ".", ":!*.lock", ":!scripts/test_decisions.py"],
        cwd=cwd,
        text=True,
        capture_output=True,
    ).stdout
    for n in sorted({int(m) for m in MENTION.findall(mentioned)}):
        if n > top:
            problems.append(f"D-{n:03d} is mentioned but no decision has that number: make it with scripts/decisions.py new")
    return problems


def main(argv: list[str]) -> None:
    if len(argv) == 2 and argv[0] == "new":
        print(new(argv[1]).relative_to(ROOT))
    elif argv and argv[0] == "check" and argv[1:] in ([], ["--offline"]):
        problems = check(offline=argv[1:] == ["--offline"])
        for p in problems:
            print(p, file=sys.stderr)
        if problems:
            sys.exit(1)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
