#!/usr/bin/env python3
"""The product version, kept identical everywhere it is written.

    scripts/version.py current              print the version
    scripts/version.py next patch|minor|major
                                            print the version after a bump
    scripts/version.py set X.Y.Z            write X.Y.Z everywhere

Sources of truth it keeps in step: the workspace `Cargo.toml` (otterd, otter
and the crates), the desktop app's `Cargo.toml` and `package.json`, and the
local entries of the lockfiles (so `--locked` builds keep working). The Tauri
config has no version of its own; Tauri reads the desktop crate's.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKSPACE = ROOT / "Cargo.toml"
DESKTOP_CARGO = ROOT / "apps/desktop/src-tauri/Cargo.toml"
PACKAGE_JSON = ROOT / "apps/desktop/package.json"
PACKAGE_LOCK = ROOT / "apps/desktop/package-lock.json"
LOCKFILES = [ROOT / "Cargo.lock", ROOT / "apps/desktop/src-tauri/Cargo.lock"]

SEMVER = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


def section_version(path: Path, section: str) -> re.Match:
    """The `version = "…"` line inside `[section]`."""
    text = path.read_text()
    m = re.search(
        rf"^\[{re.escape(section)}\]\n(?:(?!^\[).*\n)*?^version = \"([^\"]+)\"",
        text,
        re.M,
    )
    if not m:
        sys.exit(f"no version in [{section}] of {path}")
    return m


def current() -> str:
    return section_version(WORKSPACE, "workspace.package").group(1)


def bump(version: str, kind: str) -> str:
    m = SEMVER.match(version)
    if not m:
        sys.exit(f"not a plain X.Y.Z version: {version}")
    major, minor, patch = map(int, m.groups())
    if kind == "major":
        return f"{major + 1}.0.0"
    if kind == "minor":
        return f"{major}.{minor + 1}.0"
    if kind == "patch":
        return f"{major}.{minor}.{patch + 1}"
    sys.exit(f"unknown bump: {kind} (patch, minor or major)")


def set_section(path: Path, section: str, version: str) -> None:
    text = path.read_text()
    m = section_version(path, section)
    start, end = m.span(1)
    path.write_text(text[:start] + version + text[end:])


def set_lockfile(path: Path, version: str) -> None:
    """Local packages (no `source`) carry the workspace version."""
    blocks = path.read_text().split("\n[[package]]\n")
    for i, block in enumerate(blocks[1:], start=1):
        if "\nsource = " not in block:
            blocks[i] = re.sub(r'^version = "[^"]+"', f'version = "{version}"', block, count=1, flags=re.M)
    path.write_text("\n[[package]]\n".join(blocks))


def set_json(path: Path, version: str, lock: bool) -> None:
    data = json.loads(path.read_text())
    data["version"] = version
    if lock and "" in data.get("packages", {}):
        data["packages"][""]["version"] = version
    path.write_text(json.dumps(data, indent=2) + "\n")


def set_all(version: str) -> None:
    if not SEMVER.match(version):
        sys.exit(f"not a plain X.Y.Z version: {version}")
    set_section(WORKSPACE, "workspace.package", version)
    set_section(DESKTOP_CARGO, "package", version)
    set_json(PACKAGE_JSON, version, lock=False)
    set_json(PACKAGE_LOCK, version, lock=True)
    for lockfile in LOCKFILES:
        set_lockfile(lockfile, version)


def main(argv: list[str]) -> None:
    match argv:
        case ["current"]:
            print(current())
        case ["next", kind]:
            print(bump(current(), kind))
        case ["set", version]:
            set_all(version)
        case _:
            sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])
