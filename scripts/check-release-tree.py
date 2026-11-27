#!/usr/bin/env python3
"""Check committed crate parity before export; never change a Git ref.

Usage: python3 scripts/check-release-tree.py --source master --public <candidate> \
    --filter-repo <round-12-filter-checkout> --output <evidence-directory>

The approved 0.4.0 source/public pair records the hand-curated normalization
(comments, private labels, and recorded fixture data). Replay that delta onto
current source with a three-way merge, then apply the pinned production filter.
This is NOT a blanket exemption for files present in the approved public tree:
any later source edit must survive in the candidate, even in a renamed file.
Conflicts, missing/extra files, content and mode differences all fail closed.
The exact rules in release-tree-normalization.json apply to every source file,
also to files added after the base pair. Additional sanitizer edits require exact
path/before/after entries in the sibling release-tree-cleanup.json.
The filter repository is local; no network, build, export, or publication occurs.
"""

import argparse
import difflib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile

BASE_SOURCE = "87809f0764f3076f53e457d9a8ae5258de5eeb8f"
BASE_PUBLIC = "7a6748636aba30d2c9dca0c3bacd22aea836f288"
# Tree rebuilt from the git archive of filter commit 631057ad (the 0.4.3
# export); the original tree id was not recorded. A tree pin still resolves
# after any rebuild of the filter repository from the same bytes.
FILTER_PIN = "865c9e3ab480f6406206736a13a9954edb9a185a"
# These paths were deliberately replaced, not deleted, by public curation.
RENAMES = {
    "crates/ego-cli/src/run/executor/defaults_107_tests.rs": "crates/ego-cli/src/run/executor/defaults_tests.rs",
    "crates/ego-cli/tests/it/mcp_server_058.rs": "crates/ego-cli/tests/it/mcp_server.rs",
    "crates/ego-cli/tests/it/tuicommander_compatibility_092.rs": "crates/ego-cli/tests/it/tuicommander_compatibility.rs",
    "crates/ego-cli/tests/tuicommander_host_094.py": "crates/ego-cli/tests/tuicommander_host.py",
    "crates/ego-cli/tests/tuicommander_host_094.rs": "crates/ego-cli/tests/tuicommander_host.rs",
    "crates/ego-core/tests/fixtures/crash_424_start.json": "crates/ego-core/tests/fixtures/crash_start.json",
    "crates/ego-tools/tests/it/run_artifact_058.rs": "crates/ego-tools/tests/it/run_artifact.rs",
}
for old, new in (
    ("615-unrun-exec-without-reasoning.jsonl", "unrun-exec-without-reasoning.jsonl"),
    ("615-unrun-exec.jsonl", "unrun-exec.jsonl"),
    ("615-unrun-write.jsonl", "unrun-write.jsonl"),
    ("617-unrun-swift.jsonl", "unrun-swift.jsonl"),
    ("652-recorded-skill-calls.json", "recorded-skill-calls.json"),
    ("676-empty-cut-stream.jsonl", "empty-cut-stream.jsonl"),
    ("compaction/590-recorded-failure.json", "compaction/recorded-failure.json"),
):
    RENAMES["crates/ego-cli/tests/fixtures/" + old] = "crates/ego-cli/tests/fixtures/" + new

FILTER_INPUTS = """content_rules.py account_rules.py dcr_rules.py recording_rules.py
artifact_rules.py capture_rules.py runtime_rules.py provider_capture_rules.py
content-rules.tsv recorded-peer-map.tsv story-labels.txt children-blob-ids.txt
sessions-blob-ids.txt recorded-session-id-map.tsv artifact-neutral-map.tsv
captured-provenance-map.tsv runtime-telemetry-map.json provider-transaction-epochs.json
operation-lock-paths.json provider-capture-map.json public-sessions.fixture
public-children.fixture public-host-context.fixture public-managed-child.fixture
public-account-readme.fixture public-memory-entry.fixture public-memory-decisions.fixture
public-memory-readme.fixture public-memory-source.fixture""".split()


def before_move(path):
    # Integration tests moved into tests/it/ after 0.4.0 (one test binary per
    # crate). The approved base pair still records them under tests/.
    crate, moved, rest = path.partition("/tests/it/")
    return crate + "/tests/" + rest if moved else path


def git(*args, root=None, data=None):
    command = ["git"] + (["-C", str(root)] if root else []) + list(args)
    return subprocess.check_output(command, input=data)


def tree(ref):
    result = {}
    for row in git("ls-tree", "-rz", ref, "--", "crates/").split(b"\0"):
        if row:
            header, path = row.split(b"\t")
            mode, kind, oid = header.decode().split()
            if kind != "blob":
                raise ValueError("Unsupported crate entry: " + path.decode())
            result[path.decode()] = (mode, oid)
    return result


def read_blobs(trees):
    oids = sorted({oid for entries in trees for _, oid in entries.values()})
    output = git("cat-file", "--batch", data="".join(oid + "\n" for oid in oids).encode())
    result = {}
    offset = 0
    for oid in oids:
        end = output.index(b"\n", offset)
        size = int(output[offset:end].split()[2])
        result[oid] = output[end + 1:end + 1 + size]
        offset = end + 2 + size
    return result


def cleanup_changes(before, after):
    # Preserve newline bytes too. A // prefix can be string data, not a comment.
    old, new = before.splitlines(keepends=True), after.splitlines(keepends=True)
    return [
        {"before": b"".join(old[start:end]).decode("utf-8"),
         "after": b"".join(new[new_start:new_end]).decode("utf-8")}
        for kind, start, end, new_start, new_end in difflib.SequenceMatcher(None, old, new, autojunk=False).get_opcodes()
        if kind != "equal"
    ]


def reject_unreviewed_cleanup(path, changes, reviewed):
    if any(change not in reviewed.get(path, []) for change in changes):
        raise ValueError("unreviewed production cleanup; add exact path/content entries only after review")


def replay(public, base, current, scratch):
    if current == base:
        return public
    if public == base or public == current:
        return current
    files = [scratch / name for name in ("public", "base", "current")]
    for path, data in zip(files, (public, base, current)):
        path.write_bytes(data)
    merged = subprocess.run(["git", "merge-file", "-p", *map(str, files)], capture_output=True)
    if merged.returncode:
        raise ValueError("normalization replay conflict")
    return merged.stdout


def compare(source_ref, public_ref, sanitize, sanitize_path, scratch, output):
    source, public, old_source, old_public = map(tree, (source_ref, public_ref, BASE_SOURCE, BASE_PUBLIC))
    blobs = read_blobs((source, public, old_source, old_public))
    rows = []
    expected = {}
    errors = []
    mapped = set()
    comment_rules = json.loads(Path(__file__).with_name("release-tree-normalization.json").read_text())
    reviewed_cleanup = json.loads(Path(__file__).with_name("release-tree-cleanup.json").read_text())["changes"]

    def normalize_comment(path, data):
        for rule in comment_rules.get(path, []):
            data = data.replace(rule["source"].encode(), rule["public"].encode())
        return data

    for path in sorted(source):
        target = RENAMES.get(path, sanitize_path(path.encode()).decode())
        if target in mapped:
            raise ValueError("normalized path collision: " + target)
        mapped.add(target)
        row = {"source": path, "public": target, "source_blob": source[path][1]}
        try:
            if target in expected:
                raise ValueError("normalized path collision")
            current = blobs[source[path][1]]
            base, base_target = before_move(path), before_move(target)
            if base in old_source and base_target in old_public:
                data = replay(blobs[old_public[base_target][1]], normalize_comment(base, blobs[old_source[base][1]]), normalize_comment(path, current), scratch)
                row["rule"] = "approved historical curation replay + pinned production filter"
            else:
                # A file new since the base pair has no historical curation;
                # its exact curated rules still apply before the filter.
                data = normalize_comment(path, current)
                row["rule"] = "exact curation + pinned production filter"
            cleaned = sanitize(data)
            changes = cleanup_changes(data, cleaned)
            if changes:
                row["cleanup_changes"] = changes
            reject_unreviewed_cleanup(target, changes, reviewed_cleanup)
            data = cleaned
            expected[target] = (source[path][0], data)
            actual = public.get(target)
            row["public_blob"] = actual[1] if actual else None
            if actual is None or actual[0] != source[path][0] or blobs[actual[1]] != data:
                raise ValueError("missing file" if actual is None else "unapproved content or mode difference")
            row["status"] = "identical" if path == target and current == data else "approved normalization"
        except ValueError as error:
            row["status"] = "UNAPPROVED"
            row["error"] = str(error)
            errors.append(target)
        rows.append(row)
    # Public-only capture-integrity tests and fixture READMEs are exact inherited
    # additions. Changing their bytes or mode is NOT exempted.
    base_renames = {before_move(old): before_move(new) for old, new in RENAMES.items()}
    mapped_old = {base_renames.get(p, sanitize_path(p.encode()).decode()) for p in old_source}
    additions = set(old_public) - mapped_old
    for path in sorted(set(public) - mapped):
        if path in additions and public[path] == old_public[path]:
            rows.append({"public": path, "status": "approved public-only addition", "rule": BASE_PUBLIC})
        else:
            rows.append({"public": path, "status": "UNAPPROVED", "error": "extra file"})
            errors.append(path)
    for path in sorted(additions - set(public)):
        rows.append({"public": path, "status": "UNAPPROVED", "error": "missing public-only addition"})
        errors.append(path)
    patches = []
    for path, (mode, data) in expected.items():
        actual = public.get(path)
        if actual is None or blobs[actual[1]] != data:
            before = blobs[actual[1]] if actual else b""
            patches.extend(difflib.unified_diff(before.decode("utf-8", "replace").splitlines(True), data.decode("utf-8", "replace").splitlines(True), fromfile=path, tofile=path))
            destination = output / "expected" / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(data)
    (output / "differences.patch").write_text("".join(patches))
    report = {"source": git("rev-parse", source_ref).decode().strip(), "public": git("rev-parse", public_ref).decode().strip(), "base_source": BASE_SOURCE, "base_public": BASE_PUBLIC, "filter": FILTER_PIN, "files": rows, "unapproved": sorted(set(errors))}
    (output / "inventory.json").write_text(json.dumps(report, indent=2) + "\n")
    for path in report["unapproved"]:
        print("UNAPPROVED " + path)
    print(f"{len(rows)} files checked; {len(report['unapproved'])} unapproved")
    return bool(errors)


def load_filter(filter_repo, directory):
    """Load the committed pinned filter bytes; the caller owns sys.path cleanup."""
    for name in FILTER_INPUTS:
        (directory / name).write_bytes(git("show", FILTER_PIN + ":" + name, root=filter_repo))
    sys.path.insert(0, str(directory))
    spec = importlib.util.spec_from_file_location("content_rules", directory / "content_rules.py")
    rules = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(rules)
    return rules


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", default="master")
    parser.add_argument("--public", default="chore/public-release-tree")
    parser.add_argument("--filter-repo", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    # Load committed production bytes, never mutable checkout modules or pycache.
    sys.dont_write_bytecode = True
    with tempfile.TemporaryDirectory(prefix="normalization-", dir=args.output) as directory:
        scratch = Path(directory)
        rules = load_filter(args.filter_repo, scratch)
        return compare(args.source, args.public, rules.sanitize, rules.sanitize_path, scratch, args.output)


if __name__ == "__main__":
    raise SystemExit(main())
