#!/usr/bin/env python3
"""Reject public Markdown command examples absent from an actual ego binary.

Run against a public checkout, not the private documentation archive. Only
Markdown code spans and fenced blocks are command examples; prose is not argv.
The binary's Commands section is authoritative: unknown words otherwise become
interactive prompts, so invoking a purported command is not a valid test.
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys


def commands(binary):
    help_text = subprocess.check_output([str(binary), "--help"], text=True)
    section = help_text.split("\nCommands:\n", 1)[1]
    result = set()
    for line in section.splitlines():
        match = re.match(r"^  ([a-z][a-z0-9-]*)\s{2,}\S", line)
        if match:
            result.add(match[1])
        elif line.strip() and result:
            break
    if not result:
        raise ValueError("binary help contains no command inventory")
    return result


def without_quoted_values(text):
    # Prompts are data, even when they contain command-like words or separators.
    return re.sub(r"\\.|\"(?:\\.|[^\"\\])*\"|'[^']*'", "", text)


def references(text):
    fence = None
    for number, line in enumerate(text.splitlines(), 1):
        # Markdown permits fenced examples inside one or more blockquotes.
        line = re.sub(r"^(?:[ \t]{0,3}>[ \t]?)+", "", line)
        marker = re.match(r"^\s*(`{3,}|~{3,})", line)
        if marker:
            if fence is None:
                fence = marker[1][0]
            elif marker[1][0] == fence:
                fence = None
            continue
        snippets = re.findall(r"`+([^`]+)`+", line)
        if fence:
            # Help transcripts contain narrative such as "Serve scoped ego runs".
            # In a block, recognize command positions, not every prose mention.
            for match in re.finditer(
                r"(?:^\s*(?:\$\s+|Usage:\s*)?|[|;&]\s*)ego[ \t]+([a-z][a-z0-9-]*)(?![\w-])",
                without_quoted_values(line),
            ):
                yield number, match[1]
        for snippet in snippets:
            for match in re.finditer(
                r"(?<![\w/.-])ego[ \t]+([a-z][a-z0-9-]*)(?![\w-])",
                without_quoted_values(snippet),
            ):
                yield number, match[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--docs-root", type=Path, required=True)
    args = parser.parse_args()
    root = args.docs_root
    files = [root / "README.md", root / "CLI.md"]
    if not (root / "docs").is_dir():
        parser.error("docs-root must contain docs/")
    files += sorted((root / "docs").rglob("*.md"))
    try:
        available = commands(args.binary.resolve())
        failures = []
        count = 0
        for path in files:
            for line, command in references(path.read_text(encoding="utf-8")):
                count += 1
                if command not in available:
                    failures.append(f"{path.relative_to(root)}:{line}: unknown command: ego {command}")
        for failure in failures:
            print(failure, file=sys.stderr)
        print(f"{len(files)} Markdown files; {count} command references; {len(failures)} unknown")
        return bool(failures)
    except (OSError, ValueError, IndexError, subprocess.CalledProcessError) as error:
        print(f"public docs check failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
