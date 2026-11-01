#!/usr/bin/env python3
"""Protect the Markdown link gate against unpublished local files."""
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

CHECKER = Path(__file__).with_name("check-links.py")
TEMP_ROOT = Path.home() / "Gits" / ".tmp"


class LinkCheckTests(unittest.TestCase):
    def setUp(self):
        TEMP_ROOT.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="ego-check-links-", dir=TEMP_ROOT)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "--quiet")
        (self.root / "tools/docs").mkdir(parents=True)
        shutil.copyfile(CHECKER, self.root / "tools/docs/check-links.py")
        (self.root / "docs").mkdir()
        for name in ("README.md", "ARCHITECTURE.md", "CLI.md", "CONTRIBUTING.md"):
            (self.root / name).write_text("# Start\n")
            self.git("add", name)
        (self.root / ".gitignore").write_text("/docs/metrics/\n/docs/book/\n")
        self.git("add", ".gitignore")

    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.root, check=True, capture_output=True, text=True,
        )

    def check(self):
        return subprocess.run(
            [sys.executable, "-B", "tools/docs/check-links.py"],
            cwd=self.root, capture_output=True, text=True,
        )

    # Catches: ignored local Markdown with a broken link makes the gate fail.
    def test_ignored_markdown_with_broken_link_is_skipped(self):
        path = self.root / "docs/metrics/observation.md"
        path.parent.mkdir()
        path.write_text("[Missing](../../missing-guide.md)\n")
        self.git("check-ignore", "docs/metrics/observation.md")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")

    # Catches: ignoring real tracked failures, including force-added ignored paths.
    def test_tracked_markdown_with_broken_link_still_fails(self):
        for name in ("docs/guide.md", "docs/metrics/tracked.md", "README.md"):
            with self.subTest(path=name):
                path = self.root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("[Missing](missing.md)\n")
                self.git("add", "--force", name)
                result = self.check()
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(f"{name}: missing.md (missing path)", result.stderr)
                path.write_text("# Start\n")

    # Catches: untracked drafts leaking into a tracked-only publication check.
    def test_untracked_markdown_with_broken_link_is_skipped(self):
        (self.root / "docs/draft.md").write_text("[Missing](missing.md)\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")

    # Catches: splitting Git paths on whitespace or scanning generated book files.
    def test_tracked_path_with_spaces_and_valid_fragment_passes(self):
        (self.root / "docs/a guide.md").write_text("# Topic\n[Here](#topic)\n")
        self.git("add", "docs/a guide.md")
        (self.root / "docs/book").mkdir()
        (self.root / "docs/book/output.md").write_text("[Missing](missing.md)\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        self.assertIn("Checked 1 local links across 5 Markdown files; 0 errors.", result.stdout)


if __name__ == "__main__":
    unittest.main()
