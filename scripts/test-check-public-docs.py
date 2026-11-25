#!/usr/bin/env python3
"""Behavioral controls using the actual built binary, never a fake help fixture.

EGO_DOCS_BINARY=/absolute/path/to/ego python3 -B scripts/test-check-public-docs.py
"""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

CHECKER = Path(__file__).with_name("check-public-docs.py")
BINARY = os.environ["EGO_DOCS_BINARY"]


class PublicDocsCheck(unittest.TestCase):
    def run_check(self, readme, cli="", guide=""):
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as temporary:
            root = Path(temporary)
            (root / "docs" / "nested").mkdir(parents=True)
            for path, text in (("README.md", readme), ("CLI.md", cli), ("docs/nested/guide.md", guide)):
                (root / path).write_text(text, encoding="utf-8")
            return subprocess.run(
                ["python3", "-B", str(CHECKER), "--binary", BINARY, "--docs-root", str(root)],
                text=True, capture_output=True,
            )

    def test_quoted_prompt_words_are_not_mistaken_for_a_subcommand(self):
        examples = [
            '`ego run "Explain ego semantics"`\n',
            "`ego run 'Explain ego semantics'`\n",
            '```sh\nego run "Explain ; ego semantics"\n```\n',
            '`ego run "Explain \\"ego semantics\\""`\n',
        ]
        for example in examples:
            with self.subTest(example=example):
                result = self.run_check(example)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_shell_escaped_apostrophe_does_not_expose_prompt_words_as_commands(self):
        result = self.run_check("`ego run 'Explain it'\\''s ego semantics'`\n")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_blockquoted_fence_cannot_hide_a_removed_command(self):
        for quote, fence in [('> ', '```'), ('> > ', '~~~')]:
            with self.subTest(quote=quote, fence=fence):
                result = self.run_check(f'{quote}{fence}sh\n{quote}ego orchestrator\n{quote}{fence}\n')
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn('README.md:2: unknown command: ego orchestrator', result.stderr)

    def test_removed_command_cannot_hide_in_any_public_markdown_surface(self):
        for surface in range(3):
            with self.subTest(surface=surface):
                docs = ["", "", ""]
                docs[surface] = "```sh\nego orchestrator\n```\n"
                result = self.run_check(*docs)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn(":2: unknown command: ego orchestrator", result.stderr)

    def test_removed_inline_command_is_not_accepted_as_an_interactive_prompt(self):
        result = self.run_check("| `ego orchestrator` | removed |\n")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("unknown command: ego orchestrator", result.stderr)

    def test_recorded_help_prose_is_not_mistaken_for_a_command(self):
        # Real help says "Serve scoped ego runs"; it is not an argv example.
        help_text = subprocess.check_output([BINARY, "--help"], text=True)
        result = self.run_check("```text\n" + help_text + "\n```\n")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_pipeline_and_usage_examples_cannot_hide_a_removed_command(self):
        result = self.run_check("```sh\nprintf x | ego orchestrator\n```\n", "```text\nUsage: ego orchestrator\n```\n")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("README.md:2: unknown command: ego orchestrator", result.stderr)
        self.assertIn("CLI.md:2: unknown command: ego orchestrator", result.stderr)

    def test_real_worktree_and_interactive_examples_do_not_become_false_commands(self):
        result = self.run_check(
            "ego is a harness. `ego --worktree feat/task`\n",
            "`ego worktree show RECEIPT_UUID`\n",
            "~~~sh\nego run --no-tools -\n~~~\n",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("2 command references; 0 unknown", result.stdout)


if __name__ == "__main__":
    unittest.main()
