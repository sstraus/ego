#!/usr/bin/env python3
"""Increment the workspace patch version; Cargo updates the lockfile afterward."""

from pathlib import Path
import re


WORKSPACE_VERSION = re.compile(
    r'(?m)^\[workspace\.package\]\n(?:(?!^\[).)*?^version = "(\d+)\.(\d+)\.(\d+)"$',
    re.S | re.M,
)


def bumped_manifest(text):
    match = WORKSPACE_VERSION.search(text)
    if match is None:
        raise ValueError("expected a stable workspace package version")
    major, minor, patch = map(int, match.groups())
    version = f"{major}.{minor}.{patch + 1}"
    start, end = match.span(3)
    return text[:start] + str(patch + 1) + text[end:], version


def main():
    manifest = Path(__file__).resolve().parent.parent / "Cargo.toml"
    text, version = bumped_manifest(manifest.read_text(encoding="utf-8"))
    manifest.write_text(text, encoding="utf-8")
    print(f"Workspace version: {version} (run cargo check to refresh Cargo.lock)")


if __name__ == "__main__":
    main()
