#!/usr/bin/env python3
"""Capture help-only output from a supplied binary without provider calls."""
from datetime import date
import hashlib
from pathlib import Path
import re
import shlex
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: python3 tools/docs/capture-help.py /absolute/path/to/ego")
    binary = Path(sys.argv[1]).resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()

    def capture(arguments):
        result = subprocess.run(
            ["ego", *arguments], executable=str(binary), check=True,
            capture_output=True, text=True, timeout=10,
        )
        if result.stderr:
            raise RuntimeError("help emitted unexpected stderr: " + result.stderr)
        return "\n".join(line.rstrip() for line in result.stdout.splitlines()).rstrip()

    version = capture(["--version"])
    commands = re.findall(r"^## `(ego[^`]*)`$", (ROOT / "docs/reference/cli.md").read_text(), re.M)
    commands.append("ego resume --name show")
    sections = [
        "# CLI help from a verified binary\n\n"
        f"Captured from `{version}` on {date.today().isoformat()}. "
        f"Binary SHA-256: `{digest}`.\n\n"
        "This is a dated help-only snapshot, not a separate command specification. "
        "The [generated reference](cli.md) follows the source parser. "
        "For your installed binary, use `ego COMMAND SUBCOMMAND --help`.\n"
    ]
    for command in commands:
        arguments = [*shlex.split(command)[1:], "--help"]
        sections.append(f"\n## `{command} --help`\n\n```text\n{capture(arguments)}\n```\n")
    if hashlib.sha256(binary.read_bytes()).hexdigest() != digest:
        raise RuntimeError("binary changed during capture; snapshot was not written")
    (ROOT / "docs/reference/help.md").write_text("".join(sections))
    print(f"Captured {len(commands)} help surfaces from {version}; no provider calls.")


if __name__ == "__main__":
    main()
