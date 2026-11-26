#!/usr/bin/env python3
"""Harness evaluation tooling. `affected` is information only and never gates."""
import argparse
import hashlib
import json
import re
import subprocess
import sys

try:
    import tomllib
except ImportError:
    sys.exit("harness.py needs Python 3.11 or later (tomllib); this is Python "
             f"{sys.version.split()[0]}. Run it with a newer python3, for example "
             "/opt/homebrew/bin/python3.")
from pathlib import Path

import decision
import runner
import suite

HERE = Path(__file__).resolve().parent


def expand_braces(pattern):
    match = re.search(r"\{([^{}]*)\}", pattern)
    if not match:
        return [pattern]
    head, tail = pattern[:match.start()], pattern[match.end():]
    return [p for alt in match.group(1).split(",") for p in expand_braces(head + alt + tail)]


def glob_regex(pattern):
    out, i = "", 0
    while i < len(pattern):
        if pattern.startswith("**", i):
            out, i = out + ".*", i + 2
        elif pattern[i] == "*":
            out, i = out + "[^/]*", i + 1
        else:
            out, i = out + re.escape(pattern[i]), i + 1
    return re.compile(out + r"\Z")


def matcher(patterns):
    regexes = [glob_regex(p) for pat in patterns for p in expand_braces(pat)]
    return lambda path: any(r.match(path) for r in regexes)


def git(root, *args):
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    return result.returncode, result.stdout


def default_base(root):
    for ref in ("main", "master"):
        code, out = git(root, "merge-base", ref, "HEAD")
        if code == 0:
            return out.strip()
    return "HEAD"


def surface_digest(root, is_surface_file):
    """Digest of the working-tree content of every tracked file on the surface."""
    _, listing = git(root, "ls-files", "-z")
    digest = hashlib.sha256()
    for path in sorted(p for p in listing.split("\0") if p and is_surface_file(p)):
        file = Path(root) / path
        if file.is_file():
            digest.update(path.encode() + b"\0" + hashlib.sha256(file.read_bytes()).digest())
    return digest.hexdigest()


def affected(root, base, manifest):
    code, out = git(root, "diff", "--name-only", base)
    if code != 0:
        print(f"harness: cannot diff against {base}", file=sys.stderr)
        return {}
    changed = {p for p in out.splitlines() if p}
    _, untracked = git(root, "ls-files", "--others", "--exclude-standard")
    changed |= {p for p in untracked.splitlines() if p}
    hits = {}
    for name, patterns in manifest["surfaces"].items():
        is_surface_file = matcher(patterns)
        files = sorted(p for p in changed if is_surface_file(p))
        if files:
            hits[name] = (files, surface_digest(root, is_surface_file))
    return hits


def run_verify(root, update):
    if update:
        suite.write_digests(root)
    errors = suite.verify_suite(root)
    for line in errors:
        print(line)
    return 1 if errors else 0


def run_command(args):
    sys.path.insert(0, str(HERE.parent / "judge"))
    from run import load_keys  # loads only the named keys; values are never printed

    config = tomllib.loads((HERE / "manifest.toml").read_text())["run"]
    if args.model:
        config["model"] = args.model
    rows = runner.run_matrix(args.suite, args.baseline_bin, args.candidate_bin, config, args.tasks,
                             args.runs or config["runs"], args.out, load_keys())
    Path(args.out, "meta.json").write_text(json.dumps({"model": config["model"], "runs": args.runs or config["runs"]}))
    print(f"{len(rows)} runs written under {args.out}")
    return 0


def decide_command(args):
    summary = decision.write_decision(args.results, tomllib.loads((HERE / "manifest.toml").read_text())["decision"])
    print(f"{summary['decision']}{' (cost flag)' if summary['cost_flag'] else ''}: {decision.NOTE}")
    return 0


def record_command(args):
    root = Path(args.root)
    manifest = tomllib.loads((HERE / "manifest.toml").read_text())
    digests = {name: surface_digest(root, matcher(patterns)) for name, patterns in manifest["surfaces"].items()}
    base = args.base_commit or default_base(root)
    candidate = args.candidate_commit or git(root, "rev-parse", "HEAD")[1].strip()
    print(decision.write_record(args.results, args.story, HERE / "records", digests, base, candidate))
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    cmd = sub.add_parser("affected", help="print touched performance surfaces (exit 0)")
    cmd.add_argument("--base", help="base ref (default: merge-base with main)")
    cmd.add_argument("--root", default=str(HERE.parents[1]), help=argparse.SUPPRESS)
    cmd.add_argument("--manifest", default=str(HERE / "manifest.toml"), help=argparse.SUPPRESS)
    verify = sub.add_parser("verify-suite", help="check digests, references and untouched workspaces")
    verify.add_argument("--suite", default=str(HERE / "tasks"), help=argparse.SUPPRESS)
    verify.add_argument("--update-digests", action="store_true", help="rewrite DIGESTS.sha256 after an intended change")
    run = sub.add_parser("run", help="run baseline and candidate binaries on the suite")
    run.add_argument("--baseline-bin", required=True)
    run.add_argument("--candidate-bin", required=True)
    run.add_argument("--model")
    run.add_argument("--runs", type=int)
    run.add_argument("--tasks", nargs="*")
    run.add_argument("--out", required=True)
    run.add_argument("--suite", default=str(HERE / "tasks"), help=argparse.SUPPRESS)
    dec = sub.add_parser("decide", help="write summary.json and REPORT.md for a results directory")
    dec.add_argument("results")
    rec = sub.add_parser("record", help="write the tracked evaluation record")
    rec.add_argument("results")
    rec.add_argument("--story", required=True)
    rec.add_argument("--base-commit")
    rec.add_argument("--candidate-commit")
    rec.add_argument("--root", default=str(HERE.parents[1]), help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.command == "decide":
        return decide_command(args)
    if args.command == "record":
        return record_command(args)
    if args.command == "run":
        return run_command(args)
    if args.command == "verify-suite":
        return run_verify(Path(args.suite), args.update_digests)
    manifest = tomllib.loads(Path(args.manifest).read_text())
    hits = affected(args.root, args.base or default_base(args.root), manifest)
    for name, (files, digest) in hits.items():
        print(f"{name}: surface digest {digest}")
        for path in files:
            print(f"  {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
