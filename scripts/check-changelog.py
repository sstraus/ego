#!/usr/bin/env python3
"""Refuse a release whose CHANGELOG.md is missing, incomplete or not moved.

check-changelog.py release [--version V] [--history [--since REF]] [--ref REF] [--root DIR]
check-changelog.py extract --version V [--root DIR]

`release` checks the format contract for version V (default: the workspace
version in Cargo.toml). `--history` also needs the private Git history: every
story ID in a feat/fix/perf commit since the previous version must be in the V
section, as a listed change or in an `<!-- internal: ... -->` comment. The
previous version starts at the oldest commit that set it in Cargo.toml; pass
`--since REF` when it was released from a later commit. Public
squash snapshots carry no story IDs, so the tag workflow omits `--history`.

`extract` prints the V section without internal comments, for release notes.
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys

HEADING = re.compile(r"^## \[([^\]]+)\](?: - (\d{4}-\d{2}-\d{2}))?\s*$")
ENTRY = re.compile(r"^\s*[-*] \S")
INTERNAL = re.compile(r"<!--\s*internal:.*?-->", re.S)
CHANGE = re.compile(r"^(feat|fix|perf)(\([^)]*\))?!?:")
STORY = re.compile(r"#(\d{3,5}-[0-9a-f]{4})\b")


def sections(text):
    """Ordered (name, date, body) for every `## [name]` heading."""
    found, name, date, body = [], None, None, []
    for line in text.splitlines():
        match = HEADING.match(line)
        if match:
            if name is not None:
                found.append((name, date, "\n".join(body)))
            name, date, body = match.group(1), match.group(2), []
        elif name is not None:
            body.append(line)
    if name is not None:
        found.append((name, date, "\n".join(body)))
    return found


def workspace_version(root):
    text = (root / "Cargo.toml").read_text(encoding="utf-8")
    block = re.search(r"^\[workspace\.package\]\n(.*?)(?=^\[|\Z)", text, re.M | re.S)
    match = block and re.search(r'^version = "([^"]+)"', block.group(1), re.M)
    if not match:
        sys.exit("Cargo.toml has no [workspace.package] version")
    return match.group(1)


def git(root, *args):
    return subprocess.run(
        ["git", *args], cwd=root, check=True, text=True, capture_output=True
    ).stdout


def version_commit(root, version, ref):
    """The oldest commit at or before REF that set the workspace version."""
    commits = git(root, "log", "--format=%H", "-S", f'version = "{version}"', ref, "--", "Cargo.toml").split()
    return commits[-1] if commits else None


def missing_stories(root, previous, since, ref, body):
    start = since or version_commit(root, previous, ref)
    if start is None:
        return [f"no commit in Cargo.toml sets the previous version {previous}"]
    subjects = git(root, "log", "--no-merges", "--format=%s", f"{start}..{ref}").splitlines()
    stories = sorted({story for subject in subjects if CHANGE.match(subject) for story in STORY.findall(subject)})
    return [f"#{story} is neither listed nor marked internal" for story in stories if f"#{story}" not in body]


def check_release(root, version, history, since, ref):
    path = root / "CHANGELOG.md"
    if not path.is_file():
        return ["CHANGELOG.md is missing"]
    found = sections(path.read_text(encoding="utf-8"))
    errors = []
    if not found or found[0][0] != "Unreleased":
        errors.append("the first section must be ## [Unreleased]")
    elif any(ENTRY.match(line) for line in found[0][2].splitlines()):
        errors.append("## [Unreleased] still has entries; move them to the release section")
    released = [section for section in found if section[0] != "Unreleased"]
    if not released or released[0][0] != version:
        errors.append(f"the newest release section must be ## [{version}]")
        return errors
    _, date, body = released[0]
    if date is None:
        errors.append(f"## [{version}] needs a release date: ## [{version}] - YYYY-MM-DD")
    if not any(ENTRY.match(line) for line in INTERNAL.sub("", body).splitlines()):
        errors.append(f"## [{version}] lists no changes")
    if history and len(released) > 1:
        errors += missing_stories(root, released[1][0], since, ref, body)
    return errors


def extract(root, version):
    for name, _, body in sections((root / "CHANGELOG.md").read_text(encoding="utf-8")):
        if name == version:
            return INTERNAL.sub("", body).strip() + "\n"
    sys.exit(f"CHANGELOG.md has no ## [{version}] section")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=["release", "extract"])
    parser.add_argument("--version")
    parser.add_argument("--history", action="store_true")
    parser.add_argument("--since")
    parser.add_argument("--ref", default="HEAD")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    args = parser.parse_args()
    version = args.version or workspace_version(args.root)
    if args.command == "extract":
        sys.stdout.write(extract(args.root, version))
        return
    errors = check_release(args.root, version, args.history, args.since, args.ref)
    for error in errors:
        print(f"CHANGELOG: {error}", file=sys.stderr)
    if errors:
        sys.exit(1)
    print(f"CHANGELOG: {version} is ready for release")


if __name__ == "__main__":
    main()
