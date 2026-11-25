#!/usr/bin/env python3
"""Check version-sensitive CLI comparisons after a patch bump in a temporary copy.

Run through the build slot on rb, or use --workflow-only locally without Cargo.
The source checkout and its fixtures stay intact.
"""
import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import textwrap


ROOT = Path(__file__).resolve().parents[1]


def check_release_tag(root):
    # Catches: reading an inherited crate version rejects every valid release tag.
    manifest = (root / "Cargo.toml").read_text()
    version = re.search(r'\[workspace\.package\]\s*\nversion = "([^"]+)"', manifest)[1]
    workflow = (root / ".github/workflows/release.yml").read_text()
    step = re.search(
        r"      - name: Check tag matches version\n.*?        run: \|\n"
        r"((?:          [^\n]*\n|\n)+)",
        workflow,
        re.DOTALL,
    )
    if step is None:
        raise RuntimeError("release workflow tag-check step was not found")
    script = textwrap.dedent(step[1])
    for tag, expected_code in [(f"v{version}", 0), (f"v{version}-mismatch", 1)]:
        result = subprocess.run(
            ["bash", "-e", "-o", "pipefail", "-c", script],
            cwd=root, env={**os.environ, "GITHUB_REF_NAME": tag},
            capture_output=True, text=True, check=False,
        )
        if result.returncode != expected_code:
            raise AssertionError(
                f"release tag {tag}: expected exit {expected_code}, got {result.returncode}\n"
                f"{result.stdout}{result.stderr}"
            )
        if expected_code and f"workspace.package.version {version}" not in result.stdout:
            raise AssertionError(f"missing workspace version diagnostic: {result.stdout}")
    print(f"Release workflow accepts v{version} and rejects a mismatched tag", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--workflow-only", action="store_true",
        help="check the actual release tag step without running Cargo",
    )
    args = parser.parse_args()
    check_release_tag(ROOT)
    scratch = Path.home() / "Gits" / ".tmp"
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ego-version-bump-", dir=scratch) as temp:
        copy = Path(temp) / "ego"
        shutil.copytree(
            ROOT,
            copy,
            symlinks=True,
            ignore=shutil.ignore_patterns(
                ".git", ".wiz", ".mdkb", ".claude", ".codex", "target",
                "target-clippy", "node_modules", "__pycache__",
            ),
        )
        manifest = copy / "Cargo.toml"
        text = manifest.read_text()
        version = re.search(
            r'(\[workspace\.package\]\s*\nversion = ")(\d+)\.(\d+)\.(\d+)(")',
            text,
        )
        if version is None:
            raise RuntimeError("workspace.package.version must declare the version")
        bumped = f"{version[2]}.{version[3]}.{int(version[4]) + 1}"
        manifest.write_text(
            text[:version.start()] + version[1] + bumped + version[5] + text[version.end():]
        )
        print(f"Trial version: {bumped}; source and fixtures are unchanged", flush=True)
        check_release_tag(copy)
        if args.workflow_only:
            return 0
        commands = [
            ["cargo", "test", "-p", "ego-tools", "--lib",
             "unconfined_policy_digest_retains_semantic_identity_across_root_materialization",
             "--no-fail-fast"],
            ["cargo", "test", "-p", "ego-cli", "--test", "launch_transcripts",
             "--test", "pty_harness", "--no-fail-fast"],
            ["cargo", "test", "-p", "ego-cli", "--test", "run",
             "version_is_written_to_stdout_and_exits_zero", "--no-fail-fast",
             "--", "--exact"],
            ["cargo", "test", "-p", "ego-cli", "--test", "live_surface_pty",
             "every_registered_slash_action_is_local_and_has_intentional_terminal_behavior",
             "--no-fail-fast", "--", "--exact"],
        ]
        failed = False
        for command in commands:
            failed |= subprocess.run(command, cwd=copy, check=False).returncode != 0
        return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
