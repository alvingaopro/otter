#!/usr/bin/env python3
"""The desktop app's update manifest (D-125).

    scripts/update_manifest.py X.Y.Z DIR    print latest.json

Installed apps read `latest.json` from the latest release. It names, per
architecture, that release's `Otter_X.Y.Z_macos_<arch>.app.tar.gz` and gives
its signature: the text of the `.sig` beside it in DIR. A missing signature
is an error, so no release goes out offering an update one architecture
can't verify.
"""

import json
import sys
from datetime import datetime, timezone
from pathlib import Path

RELEASES = "https://github.com/alvingaopro/otter/releases/download"
# The updater's name for each architecture, and the release's.
ARCHES = {"darwin-aarch64": "arm64"}


def manifest(version: str, sigs: Path, now: datetime) -> dict:
    platforms = {}
    for target, arch in ARCHES.items():
        name = f"Otter_{version}_macos_{arch}.app.tar.gz"
        platforms[target] = {
            "signature": (sigs / f"{name}.sig").read_text().strip(),
            "url": f"{RELEASES}/v{version}/{name}",
        }
    return {
        "version": version,
        "pub_date": now.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }


def main(args: list[str]) -> int:
    if len(args) != 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    version, sigs = args
    print(json.dumps(manifest(version, Path(sigs), datetime.now(timezone.utc)), indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
