"""Offline collector boundary tests; all successful upstream bytes are recorded."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("collector", ROOT.parent / "scripts/collector.py")
collector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collector)


class CollectorTests(unittest.TestCase):
    def setUp(self):
        # Never create test state or Python caches outside the authorized tree.
        self.tmp = tempfile.TemporaryDirectory(dir=ROOT)
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.state = self.dir / "state.json"
        self.fixtures = ROOT / "fixtures"

    def run_cli(self, *args, fixtures=None):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = collector.main(["--state", str(self.state), "--fixtures",
                                     str(fixtures or self.fixtures), *args])
        return result, json.loads(output.getvalue())

    def read(self):
        return json.loads(self.state.read_text())

    def bootstrap(self, page=1):
        path = self.dir / "seed"
        path.mkdir(exist_ok=True)
        (path / "claude-1.json").write_bytes(
            (self.fixtures / f"claude-{page}.json").read_bytes())
        return self.run_cli("collect", "--source", "claude", "--bootstrap", fixtures=path)

    def ack(self):
        ids = list(self.read()["pending"])
        report = self.dir / "review.md"
        report.write_text("# Saved review\n\n" + "\n".join(f"Chunk: {i}" for i in ids))
        return self.run_cli("ack", "--report", str(report), *ids)

    def test_repeat_collection_does_not_duplicate_or_discard_unreviewed_chunks(self):
        self.bootstrap()
        before = self.state.read_bytes()
        self.run_cli("collect", "--source", "claude")
        self.assertEqual(self.state.read_bytes(), before)
        self.ack()
        self.run_cli("collect", "--source", "claude")
        self.assertEqual(self.read()["pending"], {})

    def test_missing_baseline_does_not_silently_skip_unseen_releases(self):
        self.bootstrap(3)
        self.ack()
        before = self.state.read_bytes()
        # The list ends (empty page) before the saved ID appears.
        pages = self.dir / "ended"
        pages.mkdir()
        for page in (1, 2):
            (pages / f"claude-{page}.json").write_bytes(
                (self.fixtures / f"claude-{page}.json").read_bytes())
        (pages / "claude-3.json").write_text("[]")
        result, out = self.run_cli("collect", "--source", "claude", fixtures=pages)
        self.assertEqual(result, 1)
        self.assertIn("baseline not found", out["claude"]["error"])
        self.assertEqual(self.state.read_bytes(), before)

    def test_partial_failure_does_not_rollback_success_or_advance_failed_source(self):
        path = self.dir / "partial"
        path.mkdir()
        (path / "claude-1.json").write_bytes((self.fixtures / "claude-1.json").read_bytes())
        result, out = self.run_cli("collect", "--source", "claude", "--source", "codex",
                                   "--bootstrap", fixtures=path)
        self.assertEqual(result, 1)
        self.assertIn("error", out["codex"])
        self.assertIn("claude", self.read()["sources"])
        self.assertNotIn("codex", self.read()["sources"])
        self.assertTrue(self.read()["pending"])

    def test_chunk_limit_never_advances_cursor_past_unqueued_content(self):
        self.bootstrap(3)
        self.ack()
        before = self.state.read_bytes()
        result, out = self.run_cli("collect", "--source", "claude", "--chunks", "1")
        self.assertEqual(result, 1)
        self.assertIn("chunk", out["claude"]["error"])
        self.assertEqual(self.state.read_bytes(), before)
        result, _ = self.run_cli("collect", "--source", "claude")
        self.assertEqual(result, 0)
        state = self.read()
        expected = json.loads((self.fixtures / "claude-1.json").read_bytes())[0]
        self.assertEqual(state["sources"]["claude"]["id"], str(expected["id"]))
        self.assertLessEqual(len(state["pending"]), 16)
        for chunk in state["pending"].values():
            self.assertLessEqual(len(json.dumps(chunk).encode()), 65536)
        _, out = self.run_cli("pending", "--limit", "1")
        self.assertEqual(len(out), 1)

    # Catches: a page ceiling that strands the cursor after a long gap (Codex
    # published 33 releases after its baseline by 2026-11-13; ceiling was 20).
    # No page count is passed: collection pages until the saved ID by itself.
    def test_baseline_behind_thirty_three_releases_is_reachable(self):
        record = json.loads((self.fixtures / "codex-1.json").read_bytes())[0]
        pages = self.dir / "burst"
        pages.mkdir()
        for page in range(1, 35):
            # Alpha releases carry empty notes; distinct ids keep each page unique.
            item = dict(record, id=record["id"] + 34 - page, body="",
                        tag_name=f"{record['tag_name']}-alpha.{34 - page}")
            (pages / f"codex-{page}.json").write_text(json.dumps([item]))
        seed = self.dir / "seed"
        seed.mkdir()
        (seed / "codex-1.json").write_bytes((pages / "codex-34.json").read_bytes())
        self.run_cli("collect", "--source", "codex", "--bootstrap", fixtures=seed)
        self.ack()
        result, out = self.run_cli("collect", "--source", "codex", "--chunks", "64",
                                   fixtures=pages)
        self.assertEqual(result, 0, out)
        self.assertEqual(out["codex"]["new_entries"], 33)
        self.assertEqual(self.read()["sources"]["codex"]["id"], str(record["id"] + 33))

    def test_ack_requires_existing_report_and_exact_chunk_receipts(self):
        self.bootstrap()
        before = self.state.read_bytes()
        ids = list(self.read()["pending"])
        missing = self.dir / "missing.md"
        with self.assertRaises((ValueError, OSError)):
            self.run_cli("ack", "--report", str(missing), *ids)
        missing.write_text("# Review\nNo chunk receipts.\n")
        with self.assertRaises(ValueError):
            self.run_cli("ack", "--report", str(missing), *ids)
        self.assertEqual(self.state.read_bytes(), before)
        self.ack()
        self.assertFalse(self.read()["pending"])
        self.assertTrue(self.read()["reviews"][-1]["sha256"])

    def test_first_run_requires_explicit_bootstrap_and_parses_each_real_source(self):
        result, _ = self.run_cli("collect")
        self.assertEqual(result, 1)
        self.assertEqual(self.read()["sources"], {})
        result, _ = self.run_cli("collect", "--bootstrap")
        self.assertEqual(result, 0)
        self.assertEqual(set(self.read()["sources"]), {"claude", "codex", "gemini", "grok"})
        self.assertEqual(self.read()["sources"]["grok"]["version_kind"], "source-revision")
        self.assertTrue(any(c["source"] == "grok" for c in self.read()["pending"].values()))


if __name__ == "__main__":
    unittest.main()
