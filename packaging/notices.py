#!/usr/bin/env python3
"""Print the license notices of every crate linked into exnode for a target.

    python3 -I packaging/notices.py x86_64-unknown-linux-musl > THIRD-PARTY-NOTICES.txt

A static exnode binary carries the code of its dependencies, and their
licenses (MIT, Apache-2.0, ISC and others) ask that their notices travel with
it.  This walks `cargo metadata` from exnode through its normal dependencies
(not dev or build-only ones) for that target, and prints each crate's name,
version and license with the text of its LICENSE, COPYING and NOTICE files
from the local cargo registry, which a build has already filled.  It reads
only local files and runs nothing but cargo.  Python because the release
runner and a dev machine both have it and neither need have fish.
"""

import json
import os
import subprocess
import sys

NOTICE_PREFIXES = ("license", "licence", "copying", "notice", "copyright", "unlicense")


def main() -> int:
    if len(sys.argv) != 2 or not sys.argv[1].replace("-", "").replace("_", "").isalnum():
        print("usage: notices.py <target-triple>", file=sys.stderr)
        return 2
    target = sys.argv[1]
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target,
         "--manifest-path", os.path.join(root, "Cargo.toml")],
        check=True, capture_output=True, text=True).stdout)

    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    start = next(p["id"] for p in meta["packages"] if p["name"] == "exnode" and p["source"] is None)

    # normal dependencies only: a dep_kind of null is a normal one
    seen, todo = set(), [start]
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                todo.append(dep["pkg"])

    out = sys.stdout
    out.write("Third-party notices for exnode (%s)\n\n" % target)
    out.write("exnode links the crates below.  Each one's license and notice files follow.\n")
    crates = sorted((packages[p] for p in seen if packages[p]["source"] is not None),
                    key=lambda p: (p["name"], p["version"]))
    for p in crates:
        out.write("\n%s\n%s %s  (license: %s)\n" % ("=" * 78, p["name"], p["version"], p.get("license") or "not stated"))
        crate_dir = os.path.dirname(p["manifest_path"])
        files = sorted(f for f in os.listdir(crate_dir)
                       if f.lower().startswith(NOTICE_PREFIXES) and os.path.isfile(os.path.join(crate_dir, f)))
        if not files:
            out.write("(the crate ships no license file; its license is %s)\n" % (p.get("license") or "not stated"))
        for f in files:
            with open(os.path.join(crate_dir, f), encoding="utf-8", errors="replace") as fh:
                out.write("\n--- %s ---\n%s\n" % (f, fh.read().rstrip()))
    print("%d crates" % len(crates), file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
