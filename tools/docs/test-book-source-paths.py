#!/usr/bin/env python3
"""Protect navigation and examples at the mdBook preprocessing boundary."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "book_source_paths", Path(__file__).with_name("book-source-paths.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class SourcePathsTests(unittest.TestCase):
    def render(self, text):
        return module.render(
            text, Path("guides/interactive.md"),
            {Path("README.md"), Path("guides/interactive.md")},
        )

    def test_book_navigation_and_external_urls_remain_links(self):
        # Catches: a source-link conversion breaking actual book navigation.
        text = "[Home](../README.md#start) [Web](https://example.com) [Here](#input)"
        self.assertEqual(self.render(text), text.replace("../README.md", "../index.md"))

    def test_source_and_unlisted_documents_become_checkout_paths(self):
        # Catches: HTML links targeting files that mdBook does not publish.
        self.assertEqual(
            self.render("[Parser](../../crates/ego-cli/src/cli.rs) [Design](../../CLI.md#flags)"),
            "Parser (repository: `crates/ego-cli/src/cli.rs`) Design (repository: `CLI.md#flags`)",
        )
        self.assertEqual(
            self.render("[Research](../internals/README.md)"),
            "Research (repository: `ARCHITECTURE.md`)",
        )

    def test_code_formatted_link_labels_do_not_hide_checkout_targets(self):
        # Catches: inline-code protection leaving a broken source link intact.
        self.assertEqual(
            self.render("[`deny.toml`](../../deny.toml)"),
            "`deny.toml` (repository: `deny.toml`)",
        )

    def test_fenced_and_inline_examples_are_not_rewritten(self):
        # Catches: preprocessing changing literal example input.
        text = "```md\n[x](../../CLI.md)\n```\n`[x](../../CLI.md)`\n"
        self.assertEqual(self.render(text), text)

    def test_downloadable_book_assets_remain_links(self):
        # Catches: hiding a JSON attachment bundled with the guide.
        text = "[Context](../integrations/ide-context.json)"
        self.assertEqual(self.render(text), text)


if __name__ == "__main__":
    unittest.main()
