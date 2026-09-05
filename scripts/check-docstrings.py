#!/usr/bin/env python3
"""Fail a change that adds or edits a declaration without documenting it.

Detection is clippy's, not ours: `clippy::missing_docs_in_private_items`
already knows what an undocumented item is, in every form the language has
-- functions, structs, fields, constants, traits, impls -- and it knows it
from the compiler's own view of the code rather than from a regex over the
text. All this adds is the scoping, which clippy has no opinion about.

The scoping is the point. The lint fires 246 times on this repo, so turning
it on wholesale would mean documenting the codebase before landing anything
else. Instead a run fails only for declarations the change itself touched,
so the rule applies to what you are writing.

    scripts/check-docstrings.py                 # against origin/main
    scripts/check-docstrings.py <base-ref>

Editing only a body counts too. Clippy reports a diagnostic at the
declaration line, so matching changed lines against it directly would let a
body-only edit to an undocumented function through. A changed line is
therefore attributed to the declaration above it, and only the nearest one:
attribution stops at the next declaration of either kind. A doc comment
means a documented declaration starts there, and clippy has already told us
where the undocumented ones are, so both ends of a declaration's territory
are known without parsing anything.

That is a heuristic, and worth knowing which way it fails: a `///` inside a
string literal ends attribution early, so a line is missed rather than
wrongly blamed. Erring towards silence is the right direction for a rule
that makes people write prose.

It is zero-tolerance rather than a percentage, so passing this leaves an
80% threshold well clear.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys

HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", re.M)
LINT = "clippy::missing_docs_in_private_items"


def git(*args: str) -> str:
    return subprocess.run(
        ("git",) + args, capture_output=True, text=True, check=True
    ).stdout


def touched_lines(base: str) -> dict[str, set[int]]:
    """Line numbers the change touched, per file, in the working tree.

    Diffing the merge base rather than the tip of `base` keeps the result
    about this change; leaving off `...HEAD` includes work that is not
    committed yet, so the answer arrives before the push rather than after.
    """
    merge_base = git("merge-base", base, "HEAD").strip()
    files = git("diff", "--name-only", merge_base).split()
    touched: dict[str, set[int]] = {}
    for path in files:
        lines: set[int] = set()
        diff = git("diff", "-U0", merge_base, "--", path)
        for hunk in HUNK.finditer(diff):
            start = int(hunk.group(1))
            count = int(hunk.group(2) or 1)
            if count:
                lines.update(range(start, start + count))
            else:
                # A pure deletion: nothing on the new side, so the range would
                # be empty and the change would count as touching nothing.
                # Deleting a doc comment is exactly that shape, and it is the
                # one edit that makes a surviving declaration undocumented, so
                # take the lines either side of where the removal happened.
                lines.update((start, start + 1))
        if lines:
            touched[path] = lines
    return touched


def undocumented() -> list[tuple[str, int, str]]:
    """Every undocumented item clippy can see, as (file, line, what)."""
    result = subprocess.run(
        [
            "cargo", "clippy", "--all-targets", "--message-format=json",
            "--quiet", "--", "-W", LINT,
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        # Unconditionally, not only when stdout is empty: a build failure
        # still emits JSON, so a run that found no missing-docs diagnostic
        # because it never got as far as checking would otherwise report
        # success.
        print(result.stderr, file=sys.stderr)
        raise SystemExit("cargo clippy failed; fix the build first")

    items = []
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line).get("message") or {}
        except json.JSONDecodeError:
            continue
        text = message.get("message") or ""
        if "missing documentation" not in text:
            continue
        for span in message.get("spans", []):
            if span.get("is_primary"):
                items.append((span["file_name"], span["line_start"], text))
    return items


DOC_LINE = re.compile(r"^\s*(?:///|/\*\*[^*])")


def covers(path: str, declaration: int, limit: int, touched: set[int]) -> bool:
    """Whether `touched` includes the declaration or anything belonging to it.

    Its territory runs from the declaration to whichever comes first: `limit`,
    the next undocumented declaration clippy reported, or a doc comment, where
    a documented one begins. Without both ends, an edit in one declaration
    reads as an edit in every undocumented declaration above it, and the check
    fails a change for something it did not touch.
    """
    if declaration in touched:
        return True
    try:
        with open(path, encoding="utf-8") as handle:
            lines = handle.read().split("\n")
    except OSError:
        return False
    for number in range(declaration + 1, min(limit, len(lines) + 1)):
        if DOC_LINE.match(lines[number - 1]):
            return False
        if number in touched:
            return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", nargs="?", default="origin/main")
    args = parser.parse_args()

    touched = touched_lines(args.base)
    if not touched:
        print("Nothing changed; nothing to check.")
        return 0

    # Each declaration's territory ends where the next one starts, so they are
    # grouped by file and walked in order.
    by_file: dict[str, list[tuple[int, str]]] = {}
    for path, line, what in undocumented():
        by_file.setdefault(path, []).append((line, what))

    offenders = []
    for path, items in by_file.items():
        items.sort()
        lines = touched.get(path, set())
        for index, (line, what) in enumerate(items):
            limit = items[index + 1][0] if index + 1 < len(items) else sys.maxsize
            if covers(path, line, limit, lines):
                offenders.append((path, line, what))

    if not offenders:
        print(f"Every declaration touched since {args.base} is documented.")
        return 0

    print(f"Undocumented declarations touched since {args.base}:\n")
    for path, line, what in sorted(set(offenders)):
        print(f"  {path}:{line}  {what}")
    print(
        "\nSay why each one exists, not what it does -- and only what is true "
        "of it. A stale comment is treated as a defect here, so an accurate "
        "short line beats a thorough wrong one."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
