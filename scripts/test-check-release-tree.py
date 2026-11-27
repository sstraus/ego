#!/usr/bin/env python3
"""Exercise drift detection through real local Git trees, without a filter export."""

import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("release_check", Path(__file__).with_name("check-release-tree.py"))
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)
FILTER_REPO = None


class ReleaseTreeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Load the pinned filter once: its modules stay cached in sys.modules and
        # read sibling fixtures from the directory they were loaded from.
        cls.filters = tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent)
        cls.rules = CHECK.load_filter(FILTER_REPO, Path(cls.filters.name))

    @classmethod
    def tearDownClass(cls):
        sys.path.remove(cls.filters.name)
        cls.filters.cleanup()

    def test_renamed_file_cannot_keep_a_stale_body_and_modes_or_paths_cannot_drift(self):
        # Catches: accepting an old body just because its renamed file was approved;
        # also catches file-level exemptions hiding code, executable-mode, or set drift.
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent) as directory:
            root = Path(directory)

            def git(*args, data=None):
                return subprocess.check_output(["git", "-C", str(root), *args], input=data, stderr=subprocess.DEVNULL)

            def commit():
                git("add", "-A")
                git("-c", "user.name=Release check test", "-c", "user.email=release-check@example.invalid", "commit", "-qm", "fixture")
                return git("rev-parse", "HEAD").decode().strip()

            git("init", "-q")
            source_path = root / "crates/private.rs"
            public_path = root / "crates/public.rs"
            source_path.parent.mkdir()
            source_path.write_text("// private provenance\n\n// stable context\n// separates independent edits\n\nfn value() -> u8 { 1 }\n")
            base_source = commit()
            source_path.unlink()
            public_path.write_text("// public description\n\n// stable context\n// separates independent edits\n\nfn value() -> u8 { 1 }\n")
            base_public = commit()
            public_path.unlink()
            source_path.write_text("// private provenance\n\n// stable context\n// separates independent edits\n\nfn value() -> u8 { 2 }\n")
            source = commit()
            source_path.unlink()
            public_path.write_text("// public description\n\n// stable context\n// separates independent edits\n\nfn value() -> u8 { 2 }\n")
            candidate = commit()
            real_git = CHECK.git
            with patch.object(CHECK, "BASE_SOURCE", base_source), patch.object(CHECK, "BASE_PUBLIC", base_public), patch.dict(CHECK.RENAMES, {"crates/private.rs": "crates/public.rs"}), patch.object(CHECK, "git", lambda *args, **kwargs: real_git(*args, root=root, **kwargs)):
                scratch = root / "scratch"
                scratch.mkdir()
                for name, ref, failure in (("green", candidate, False), ("stale", base_public, True)):
                    with self.subTest(name=name):
                        output = root / name
                        output.mkdir()
                        self.assertEqual(CHECK.compare(source, ref, lambda data: data, lambda path: path, scratch, output), failure)
                for name in ("code", "mode", "extra", "missing"):
                    # Restore the corrected candidate, then create one actual Git defect.
                    git("restore", "--source=" + candidate, "--staged", "--worktree", "--", "crates/")
                    if name == "code":
                        public_path.write_text("// public description\n\n// stable context\n// separates independent edits\n\nfn value() -> u8 { 9 }\n")
                    elif name == "mode":
                        public_path.chmod(0o755)
                    elif name == "extra":
                        (root / "crates/extra.rs").write_text("fn extra() {}\n")
                    else:
                        public_path.unlink()
                    ref = commit()
                    with self.subTest(name=name):
                        output = root / name
                        output.mkdir()
                        self.assertTrue(CHECK.compare(source, ref, lambda data: data, lambda path: path, scratch, output))
                    extra = root / "crates/extra.rs"
                    if extra.exists():
                        extra.unlink()

    def test_curated_test_file_keeps_its_curation_after_moving_into_tests_it(self):
        # Catches: moving crates/<c>/tests/<f>.rs into tests/it/ (one test binary
        # per crate) drops the approved historical curation, because the replay
        # looks the moved path up in the base trees by its new name only.
        # The renamed case also catches a moved RENAMES entry whose old public
        # path is then reported as a missing public-only addition.
        cases = (("a.rs", "a.rs"), ("a_058.rs", "a.rs"))
        for source_name, public_name in cases:
            with self.subTest(source=source_name), tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent) as directory:
                root = Path(directory)

                def git(*args):
                    return subprocess.check_output(["git", "-C", str(root), *args], stderr=subprocess.DEVNULL)

                def commit(path, text):
                    for old in (root / "crates").rglob("*.rs"):
                        old.unlink()
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(text)
                    git("add", "-A")
                    git("-c", "user.name=Release check test", "-c", "user.email=release-check@example.invalid", "commit", "-qm", "fixture")
                    return git("rev-parse", "HEAD").decode().strip()

                git("init", "-q")
                body = "// {}\n\n// stable context\n// separates independent edits\n\nfn value() -> u8 {{ {} }}\n"
                tests = root / "crates/x/tests"
                base_source = commit(tests / source_name, body.format("private provenance", 1))
                base_public = commit(tests / public_name, body.format("public description", 1))
                source = commit(tests / "it" / source_name, body.format("private provenance", 2))
                candidate = commit(tests / "it" / public_name, body.format("public description", 2))
                scratch = root / "scratch"
                scratch.mkdir()
                output = root / "moved"
                output.mkdir()
                real_git = CHECK.git
                renames = {} if source_name == public_name else {"crates/x/tests/it/" + source_name: "crates/x/tests/it/" + public_name}
                with patch.object(CHECK, "BASE_SOURCE", base_source), patch.object(CHECK, "BASE_PUBLIC", base_public), patch.dict(CHECK.RENAMES, renames), patch.object(CHECK, "git", lambda *args, **kwargs: real_git(*args, root=root, **kwargs)):
                    self.assertFalse(CHECK.compare(source, candidate, lambda data: data, lambda path: path, scratch, output))

    def test_curated_rule_applies_to_a_file_new_since_the_base(self):
        # Catches: release-tree-normalization.json rules applied only to files
        # that exist in the approved base pair. A file added after it then keeps
        # its private label, and the curated candidate is reported as drift.
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent) as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.check_output(["git", "-C", str(root), *args], stderr=subprocess.DEVNULL)

            def commit(text):
                path = root / "crates/new.rs"
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
                git("add", "-A")
                git("-c", "user.name=Release check test", "-c", "user.email=release-check@example.invalid", "commit", "-qm", "fixture")
                return git("rev-parse", "HEAD").decode().strip()

            git("init", "-q")
            (root / "keep").write_text("base\n")
            git("add", "keep")
            git("-c", "user.name=Release check test", "-c", "user.email=release-check@example.invalid", "commit", "-qm", "base")
            base = git("rev-parse", "HEAD").decode().strip()
            source = commit("// Private label 123.\nfn value() {}\n")
            candidate = commit("// Public description.\nfn value() {}\n")
            tables = root / "tables"
            tables.mkdir()
            rule = {"source": "Private label 123.", "public": "Public description.", "rule": "test"}
            (tables / "release-tree-normalization.json").write_text(json.dumps({"crates/new.rs": [rule]}))
            (tables / "release-tree-cleanup.json").write_text(json.dumps({"changes": {}}))
            scratch = root / "scratch"
            scratch.mkdir()
            output = root / "output"
            output.mkdir()
            real_git = CHECK.git
            with patch.object(CHECK, "BASE_SOURCE", base), patch.object(CHECK, "BASE_PUBLIC", base), patch.object(CHECK, "__file__", str(tables / "check-release-tree.py")), patch.object(CHECK, "git", lambda *args, **kwargs: real_git(*args, root=root, **kwargs)):
                self.assertFalse(CHECK.compare(source, candidate, lambda data: data, lambda path: path, scratch, output))

    def test_narrative_cleanup_cannot_erase_new_offline_denial(self):
        # Critic F1: the actual production filter deletes an executable inline-
        # narrative line, making an always-true stale public body look current.
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent) as directory:
            root = Path(directory)
            rules = self.rules

            def git(*args):
                return subprocess.check_output(["git", "-C", str(root), *args], stderr=subprocess.DEVNULL)

            def commit(text):
                source_path.write_text(text)
                git("add", "crates")
                git("-c", "user.name=Release check test", "-c", "user.email=release-check@example.invalid", "commit", "-qm", "fixture")
                return git("rev-parse", "HEAD").decode().strip()

            git("init", "-q")
            source_path = root / "crates/example.rs"
            source_path.parent.mkdir()
            # Recorded from critic-733-probe.py, not an invented filter response.
            before = "pub fn network_allowed(offline: bool) -> bool {\n    let mut allowed = true;\n\n    // Stable context separating the policy body from curation.\n\n    allowed\n}\n"
            base = commit(before)
            scratch = root / "scratch"
            scratch.mkdir()
            real_git = CHECK.git
            with patch.object(CHECK, "BASE_SOURCE", base), patch.object(CHECK, "BASE_PUBLIC", base), patch.object(CHECK, "git", lambda *args, **kwargs: real_git(*args, root=root, **kwargs)):
                for name, added_line, rejection in (
                    ("code", "    if offline { allowed = false; } // inspired by offline confinement\n", True),
                    # Even a whole-line comment is new cleanup, not reviewed curation.
                    ("unreviewed_comment", "    // inspired by offline confinement\n", True),
                ):
                    source = commit(before.replace("    allowed\n}", added_line + "    allowed\n}"))
                    output = root / name
                    output.mkdir()
                    with self.subTest(name=name):
                        self.assertEqual(CHECK.compare(source, base, rules.sanitize, rules.sanitize_path, scratch, output), rejection)
            # Catches: a // line inside a raw string is policy data, not a
            # removable comment. This source/public pair comes from the real
            # pinned-filter sibling probe, not a mocked sanitizer response.
            policy_before = 'pub const POLICY: &str = r#"\n// Allow network access\n"#;\n'
            policy_source = policy_before.replace('// Allow network access\n', '// Allow network access\n// inspired by offline confinement\n')
            base = commit(policy_before)
            source = commit(policy_source)
            output = root / "raw_string_policy_is_not_comment"
            output.mkdir()
            with patch.object(CHECK, "BASE_SOURCE", base), patch.object(CHECK, "BASE_PUBLIC", base), patch.object(CHECK, "git", lambda *args, **kwargs: real_git(*args, root=root, **kwargs)):
                with self.subTest(name="raw_string_policy_is_not_comment"):
                    self.assertTrue(CHECK.compare(source, base, rules.sanitize, rules.sanitize_path, scratch, output))

    def test_release_filter_handles_every_master_artifact_label(self):
        # Catches: a new artifact label on master that the pinned filter map does
        # not know. The pinned filter raises KeyError on it, so the release-tree
        # check crashes before it reports any difference.
        source = CHECK.tree("HEAD")
        blobs = CHECK.read_blobs((source,))
        unknown = {}
        for path, (_, oid) in sorted(source.items()):
            for data, sanitize in ((blobs[oid], self.rules.sanitize), (path.encode(), self.rules.sanitize_path)):
                try:
                    sanitize(data)
                except KeyError as error:
                    unknown.setdefault(error.args[0], []).append(path)
        self.assertEqual(unknown, {}, "map these labels in the filter or remove them from source")

    def test_every_master_file_replays_the_approved_curation_without_conflict(self):
        # Catches: a source edit inside a curated region (such as the tests/it
        # move rewriting paths in curated comments) that no normalization rule
        # covers. The replay then fails closed and the release export stops.
        # The public side here is the base public tree: only replay errors count.
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parent) as directory:
            output = Path(directory)
            scratch = output / "scratch"
            scratch.mkdir()
            CHECK.compare("HEAD", CHECK.BASE_PUBLIC, self.rules.sanitize, self.rules.sanitize_path, scratch, output)
            report = json.loads((output / "inventory.json").read_text())
        conflicts = [row["source"] for row in report["files"] if row.get("error") == "normalization replay conflict"]
        self.assertEqual(conflicts, [], "curate these regions in release-tree-normalization.json")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--filter-repo", type=Path, required=True)
    args, remaining = parser.parse_known_args()
    FILTER_REPO = args.filter_repo
    unittest.main(argv=[sys.argv[0], *remaining])
