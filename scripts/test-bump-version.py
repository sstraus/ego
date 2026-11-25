#!/usr/bin/env python3
"""Check the local version bump without modifying the checkout."""

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "bump_version", Path(__file__).with_name("bump-version.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class BumpVersionTest(unittest.TestCase):
    def test_only_workspace_patch_changes(self):
        source = '[workspace.package]\nversion = "0.4.7"\n\n[dependencies]\nother = "0.4.7"\n'
        result, version = module.bumped_manifest(source)
        self.assertEqual(version, "0.4.8")
        self.assertEqual(
            result,
            '[workspace.package]\nversion = "0.4.8"\n\n[dependencies]\nother = "0.4.7"\n',
        )

    def test_refuses_prerelease_without_a_partial_edit(self):
        source = '[workspace.package]\nversion = "0.4.7-rc.1"\n'
        with self.assertRaisesRegex(ValueError, "stable workspace package version"):
            module.bumped_manifest(source)


if __name__ == "__main__":
    unittest.main()
