"""Proposal ledger tests through the CLI boundary, on real temporary files."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("ledger", ROOT.parent / "scripts/ledger.py")
ledger = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ledger)

PROPOSAL = """# Proposal

- [x] **Child fork.** <!-- hw:child-fork --> Inherit the parent.
  Second line of the item.
- [ ] **Plugin marketplace.** <!-- hw:plugin-marketplace --> Conflicts.
- [x] Not a ledger item, no marker.
"""


class LedgerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=ROOT)
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.ledger = self.dir / "ledger.json"
        self.proposal = self.dir / "proposal.md"
        self.proposal.write_text(PROPOSAL)

    def run_cli(self, *args):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = ledger.main(["--ledger", str(self.ledger), *args])
        return code, json.loads(output.getvalue())

    def test_ticks_become_decisions(self):
        # catches: an unticked item recorded as approved, or an unmarked line taken as an item
        code, result = self.run_cli("sync", str(self.proposal))
        self.assertEqual(code, 0)
        self.assertEqual(result["needs_story"], ["child-fork"])
        self.assertEqual(result["rejected"], ["plugin-marketplace"])
        _, items = self.run_cli("list")
        self.assertEqual(set(items), {"child-fork", "plugin-marketplace"})
        self.assertEqual(items["child-fork"]["title"], "Child fork.")

    def test_linked_story_is_written_once_and_not_requested_again(self):
        # catches: a second weekly sync asking for another story for the same item
        self.run_cli("sync", str(self.proposal))
        code, _ = self.run_cli("link", "child-fork", "840-ab12")
        self.assertEqual(code, 0)
        self.run_cli("link", "child-fork", "840-ab12")
        text = self.proposal.read_text()
        self.assertEqual(text.count("(story 840-ab12)"), 1)
        self.assertIn("<!-- hw:child-fork --> (story 840-ab12) Inherit", text)
        _, again = self.run_cli("sync", str(self.proposal))
        self.assertEqual(again["needs_story"], [])
        self.assertEqual(again["unchanged"], ["child-fork"])

    def test_linked_item_moves_to_implemented_with_its_lines(self):
        # catches: a linked item left among open proposals, or its second line left behind
        self.proposal.write_text(PROPOSAL + "\n## Implemented\n\nNone yet.\n\n## Gaps\n")
        self.run_cli("sync", str(self.proposal))
        self.run_cli("link", "child-fork", "840-ab12")
        head, implemented = self.proposal.read_text().split("## Implemented")
        self.assertNotIn("hw:child-fork", head)
        self.assertNotIn("Second line", head)
        section = implemented.split("## Gaps")[0]
        self.assertIn("(story 840-ab12) Inherit the parent.\n  Second line of the item.", section)

    def test_refuses_a_second_story_or_an_unapproved_item(self):
        # catches: two stories bound to one item, or a story for a rejected item
        self.run_cli("sync", str(self.proposal))
        self.run_cli("link", "child-fork", "840-ab12")
        code, result = self.run_cli("link", "child-fork", "841-cd34")
        self.assertEqual(code, 1)
        self.assertIn("already has story", result["error"])
        code, _ = self.run_cli("link", "plugin-marketplace", "842-ef56")
        self.assertEqual(code, 1)

    def test_unicode_separator_does_not_move_another_item(self):
        # catches: splitlines indexes diverging from newline-based proposal mutation
        self.proposal.write_text(
            "# Proposal\nEarlier\u2028prose\n"
            "- [x] **B.** <!-- hw:b --> B text\n"
            "- [x] **C.** <!-- hw:c --> C text\n"
        )
        self.run_cli("sync", str(self.proposal))
        code, _ = self.run_cli("link", "b", "840-ab12")
        self.assertEqual(code, 0)
        head, implemented = self.proposal.read_text().split("## Implemented")
        self.assertIn("<!-- hw:c --> C text", head)
        self.assertNotIn("<!-- hw:b -->", head)
        self.assertIn("<!-- hw:b --> (story 840-ab12) B text", implemented)
        self.assertNotIn("<!-- hw:c -->", implemented)

    def test_story_reference_in_prose_is_not_the_item_binding(self):
        # catches: a related story mentioned in rationale being mistaken for the binding
        self.proposal.write_text(
            "- [x] **Child fork.** <!-- hw:child-fork --> Related work (story 841-cd34).\n"
        )
        self.run_cli("sync", str(self.proposal))
        code, _ = self.run_cli("link", "child-fork", "840-ab12")
        self.assertEqual(code, 0)
        self.assertIn("<!-- hw:child-fork --> (story 840-ab12) Related work (story 841-cd34).",
                      self.proposal.read_text())

    def test_missing_item_does_not_record_a_story(self):
        # catches: a deleted proposal item silently becoming linked in the ledger
        self.run_cli("sync", str(self.proposal))
        self.proposal.write_text("# Proposal\n")
        code, result = self.run_cli("link", "child-fork", "840-ab12")
        self.assertEqual(code, 1)
        self.assertIn("missing", result["error"])
        _, items = self.run_cli("list")
        self.assertNotIn("story", items["child-fork"])

    def test_interrupted_link_rejects_a_different_story_and_recovers_same_story(self):
        # catches: a crash between proposal and ledger writes admitting two stories
        self.run_cli("sync", str(self.proposal))
        before_link = self.ledger.read_bytes()
        self.run_cli("link", "child-fork", "840-ab12")
        self.ledger.write_bytes(before_link)  # persisted state before ledger replacement
        code, _ = self.run_cli("link", "child-fork", "841-cd34")
        self.assertEqual(code, 1)
        self.assertNotIn("841-cd34", self.proposal.read_text())
        _, items = self.run_cli("list")
        self.assertNotIn("story", items["child-fork"])
        code, _ = self.run_cli("link", "child-fork", "840-ab12")
        self.assertEqual(code, 0)
        self.assertEqual(self.proposal.read_text().count("(story 840-ab12)"), 1)
        _, items = self.run_cli("list")
        self.assertEqual(items["child-fork"]["story"], "840-ab12")

    def test_duplicate_item_ids_are_refused(self):
        # catches: two proposal lines silently sharing one decision
        self.proposal.write_text(PROPOSAL + "- [x] **Again.** <!-- hw:child-fork -->\n")
        code, result = self.run_cli("sync", str(self.proposal))
        self.assertEqual(code, 1)
        self.assertIn("duplicate", result["error"])
        self.assertFalse(self.ledger.exists())


if __name__ == "__main__":
    unittest.main()
