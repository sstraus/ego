#!/usr/bin/env python3
"""Behavioral checks for check-changelog.py against a real temporary Git history.

python3 -B scripts/test-check-changelog.py
"""
from pathlib import Path
import subprocess
import tempfile
import unittest

CHECKER = Path(__file__).with_name("check-changelog.py")
CARGO = '[workspace]\n\n[workspace.package]\nversion = "{}"\nedition = "2024"\n'


class ChangelogCheck(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "user.name", "Test")
        self.commit("chore(release): prepare 0.1.0", version="0.1.0")
        self.commit("feat(cli): add the run command")
        self.commit("chore(deps): bump a dependency")
        self.commit("fix(cli)!: refuse empty prompts")
        self.commit("chore(release): prepare 0.2.0", version="0.2.0")

    def tearDown(self):
        self.temporary.cleanup()

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

    def commit(self, subject, version=None):
        if version:
            (self.root / "Cargo.toml").write_text(CARGO.format(version), encoding="utf-8")
            self.git("add", "Cargo.toml")
        self.git("commit", "-q", "--allow-empty", "-m", subject)

    def check(self, changelog, *args):
        (self.root / "CHANGELOG.md").write_text(changelog, encoding="utf-8")
        return subprocess.run(
            ["python3", "-B", str(CHECKER), *args, "--root", str(self.root)],
            text=True, capture_output=True,
        )

    def changelog(self, unreleased="", release="- Add `ego run`.\n- Refuse empty prompts.\n", heading="## [0.2.0] - 2026-11-02"):
        return f"# Changelog\n\n## [Unreleased]\n\n{unreleased}\n{heading}\n\n{release}\n## [0.1.0] - 2026-11-01\n\n- First release.\n"

    def test_complete_changelog_passes_with_history(self):
        result = self.check(self.changelog(), "release", "--history")
        self.assertEqual(result.returncode, 0, result.stderr)

    # Catches: a release ships while changes still wait under Unreleased.
    def test_unreleased_entries_block_the_release(self):
        result = self.check(self.changelog(unreleased="- Pending change.\n"), "release")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unreleased", result.stderr)

    # Catches: the Cargo version is released without its own dated, non-empty section.
    def test_missing_undated_or_empty_section_blocks_the_release(self):
        cases = {
            "missing": self.changelog(heading="## [0.1.9] - 2026-11-02"),
            "undated": self.changelog(heading="## [0.2.0]"),
            "empty": self.changelog(release="<!-- internal: regression case regression case -->\n"),
        }
        for name, text in cases.items():
            with self.subTest(name):
                result = self.check(text, "release")
                self.assertNotEqual(result.returncode, 0, name)
                self.assertIn("0.2.0", result.stderr)

    # Catches: a feat or fix (including a breaking `!` one) is silently left out.
    def test_unlisted_feature_or_fix_story_blocks_the_release(self):
        result = self.check(self.changelog(release="- Add `ego run`.\n"), "release", "--history")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("regression case", result.stderr)
        self.assertNotIn("regression case", result.stderr)

    # Catches: an explicitly internal change still forces a public entry.
    def test_internal_marker_accounts_for_a_story(self):
        release = "- Add `ego run`.\n<!-- internal: regression case -->\n"
        result = self.check(self.changelog(release=release), "release", "--history")
        self.assertEqual(result.returncode, 0, result.stderr)

    # Catches: --since is ignored and earlier, already released stories are demanded.
    def test_since_moves_the_previous_release_point(self):
        since = subprocess.run(["git", "rev-parse", "HEAD~2"], cwd=self.root, check=True, capture_output=True, text=True).stdout.strip()
        release = "- Refuse empty prompts.\n"
        self.assertNotEqual(self.check(self.changelog(release=release), "release", "--history").returncode, 0)
        result = self.check(self.changelog(release=release), "release", "--history", "--since", since)
        self.assertEqual(result.returncode, 0, result.stderr)

    # Catches: release notes publish the private internal marker.
    def test_extract_prints_the_section_without_internal_markers(self):
        release = "- Add `ego run`.\n<!-- internal: regression case -->\n"
        result = self.check(self.changelog(release=release), "extract", "--version", "0.2.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "- Add `ego run`.\n")


if __name__ == "__main__":
    unittest.main()
