#!/usr/bin/env python3
"""Tests for the docstring check's scoping.

Every bug this script has had was in deciding which declarations a change
touches -- never in finding undocumented ones, which is clippy's job. So
that is what these pin, and they run without cargo.
"""

from __future__ import annotations

import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("check", HERE / "check-docstrings.py")
check = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check)


class Attribution(unittest.TestCase):
    """Which declaration a changed line belongs to."""

    def covers(self, source: str, declaration: int, limit: int, touched: set[int]) -> bool:
        """Runs `covers` against `source` written to a real file."""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "a.rs"
            path.write_text(source, encoding="utf-8")
            return check.covers(str(path), declaration, limit, touched)

    # Two undocumented functions in a row, and only the second one edited.
    # Without an upper bound the first swallowed the second's lines and the
    # check failed a change for a declaration it never went near.
    ADJACENT = "fn a() {\n    x\n}\n\nfn b() {\n    y\n}\n"

    def test_an_edit_in_the_second_does_not_reach_the_first(self):
        self.assertFalse(self.covers(self.ADJACENT, 1, limit=5, touched={6}))

    def test_an_edit_in_the_second_reaches_the_second(self):
        self.assertTrue(self.covers(self.ADJACENT, 5, limit=99, touched={6}))

    def test_a_body_edit_reaches_its_own_declaration(self):
        self.assertTrue(self.covers(self.ADJACENT, 1, limit=5, touched={2}))

    def test_a_documented_declaration_ends_attribution(self):
        source = "fn a() {\n    x\n}\n\n/// Documented.\nfn b() {\n    y\n}\n"
        # Line 7 is inside b, which has its own doc comment on line 5.
        self.assertFalse(self.covers(source, 1, limit=99, touched={7}))

    def test_the_declaration_line_itself_counts(self):
        self.assertTrue(self.covers(self.ADJACENT, 1, limit=5, touched={1}))

    def test_an_untouched_declaration_is_left_alone(self):
        self.assertFalse(self.covers(self.ADJACENT, 1, limit=5, touched={6, 7}))


class TouchedLines(unittest.TestCase):
    """Which lines a diff counts as changed."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.repo = Path(self.directory.name)
        self.previous = os.getcwd()
        self.addCleanup(os.chdir, self.previous)
        os.chdir(self.repo)
        for command in (
            ["git", "init", "-q", "-b", "main"],
            ["git", "config", "user.email", "t@example.com"],
            ["git", "config", "user.name", "t"],
        ):
            subprocess.run(command, check=True, capture_output=True)

    def commit(self, text: str) -> None:
        (self.repo / "a.rs").write_text(text, encoding="utf-8")
        subprocess.run(["git", "add", "-A"], check=True, capture_output=True)
        subprocess.run(["git", "commit", "-qm", "x"], check=True, capture_output=True)

    def test_a_deleted_doc_comment_counts_as_a_change(self):
        # The edit that makes a surviving declaration undocumented. Its hunk
        # has nothing on the new side, so a naive range is empty and the
        # change looks like it touched nothing at all.
        self.commit("fn main() {}\n\n/// Documented.\nfn helper() {}\n")
        (self.repo / "a.rs").write_text(
            "fn main() {}\n\nfn helper() {}\n", encoding="utf-8"
        )
        touched = check.touched_lines("main")
        self.assertIn(3, touched["a.rs"], "the surviving declaration was missed")

    def test_uncommitted_work_counts(self):
        # Checking only committed state would report a clean run right up
        # until the moment it is too late to act on.
        self.commit("fn main() {}\n")
        (self.repo / "a.rs").write_text("fn main() {}\nfn added() {}\n", encoding="utf-8")
        self.assertIn(2, check.touched_lines("main")["a.rs"])

    def test_an_unchanged_tree_touches_nothing(self):
        self.commit("fn main() {}\n")
        self.assertEqual(check.touched_lines("main"), {})


if __name__ == "__main__":
    unittest.main(verbosity=2)
