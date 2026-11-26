#!/usr/bin/env python3
"""Bounded official-source collection; no model calls or product changes."""
import argparse
import fcntl
import hashlib
import json
from pathlib import Path
import tempfile
import urllib.request

SOURCES = {
    "claude": ("anthropics/claude-code", "releases"),
    "codex": ("openai/codex", "releases"),
    "gemini": ("google-gemini/gemini-cli", "releases"),
    # Grok publishes no GitHub releases. Track public source syncs explicitly,
    # not invented binary versions. See the coverage gap in SKILL.md.
    "grok": ("xai-org/grok-build", "commits"),
}
MAX_RESPONSE = 2 * 1024 * 1024


def read_page(source, page, fixtures):
    repo, kind = SOURCES[source]
    url = f"https://api.github.com/repos/{repo}/{kind}?per_page=1&page={page}"
    if fixtures:
        name = f"grok-commits-{page}.json" if source == "grok" else f"{source}-{page}.json"
        raw = (fixtures / name).read_bytes()
    else:
        request = urllib.request.Request(url, headers={"User-Agent": "ego-harness-watch"})
        with urllib.request.urlopen(request, timeout=20) as response:
            raw = response.read(MAX_RESPONSE + 1)
    if len(raw) > MAX_RESPONSE:
        raise ValueError("response exceeds 2 MiB; cursor unchanged")
    values = json.loads(raw)
    if not isinstance(values, list) or len(values) > 1:
        raise ValueError("expected one-item upstream page")
    result = []
    for item in values:
        if kind == "commits":
            result.append({"id": item["sha"], "version": item["sha"],
                           "version_kind": "source-revision", "url": item["html_url"],
                           "revision": item["commit"]["committer"]["date"],
                           "body": item["commit"]["message"]})
        else:
            result.append({"id": str(item["id"]), "version": item["tag_name"],
                           "version_kind": "release-tag", "url": item["html_url"],
                           "revision": item["updated_at"], "body": item.get("body") or "",
                           "prerelease": item["prerelease"]})
    return result


def collect_source(source, baseline, bootstrap, fixtures):
    if baseline is None and not bootstrap:
        raise ValueError("no baseline; use --bootstrap for one recent entry, not full history")
    entries = []
    seen = set()
    # Page until the saved ID or the end of the list: a gap of any size
    # recovers. A bootstrap reads one page. A failed request (for example the
    # hourly GitHub limit) raises before the cursor moves.
    page = 0
    while True:
        page += 1
        values = read_page(source, page, fixtures)
        if not values:
            if baseline is not None:
                raise ValueError("saved baseline not found; cursor unchanged")
            raise ValueError("no upstream entries; no baseline established")
        entry = values[0]
        if baseline is not None and entry["id"] == baseline["id"]:
            return entries
        if entry["id"] not in seen:
            entries.append(entry)
            seen.add(entry["id"])
        if baseline is None:
            return entries


def chunks_for(source, entries):
    chunks = {}
    for entry in entries:
        # Split the serialized record, not the release. Concatenate all parts
        # before JSON decoding to recover its exact normalized content.
        text = json.dumps(entry, ensure_ascii=False, sort_keys=True)
        digest = hashlib.sha256((source + "\n" + text).encode()).hexdigest()
        parts = [text[i:i + 6000] for i in range(0, len(text), 6000)]
        for index, part in enumerate(parts):
            key = f"{source}-{digest}-{index + 1}"
            chunks[key] = {"id": key, "source": source, "part": index + 1,
                           "parts": len(parts), "content": part}
    return chunks


def save(path, state):
    # One atomic snapshot owns cursors and chunks: interruption cannot advance
    # a cursor without its pending bytes. The lock prevents concurrent lost updates.
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as file:
        tmp = Path(file.name)
        try:
            json.dump(state, file, indent=2, ensure_ascii=False)
            file.write("\n")
            file.flush()
            import os
            os.fsync(file.fileno())
            tmp.replace(path)
        finally:
            tmp.unlink(missing_ok=True)


def positive(maximum):
    def parse(value):
        number = int(value)
        if not 1 <= number <= maximum:
            raise argparse.ArgumentTypeError(f"must be between 1 and {maximum}")
        return number
    return parse


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", required=True, type=Path)
    parser.add_argument("--fixtures", type=Path, help="offline replay; never falls back to HTTP")
    commands = parser.add_subparsers(dest="command", required=True)
    collect = commands.add_parser("collect")
    collect.add_argument("--source", action="append", choices=SOURCES)
    collect.add_argument("--bootstrap", action="store_true")
    collect.add_argument("--chunks", type=positive(64), default=16)
    pending = commands.add_parser("pending")
    pending.add_argument("--limit", type=positive(64), default=4)
    ack = commands.add_parser("ack")
    ack.add_argument("--report", type=Path, required=True)
    ack.add_argument("ids", nargs="+")
    args = parser.parse_args(argv)
    args.state.parent.mkdir(parents=True, exist_ok=True)
    with args.state.with_suffix(args.state.suffix + ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        state = json.loads(args.state.read_text()) if args.state.exists() else {
            "sources": {}, "pending": {}, "reviews": []}
        original = json.dumps(state, sort_keys=True)
        code = 0
        if args.command == "collect":
            output = {}
            for source in dict.fromkeys(args.source or SOURCES):
                if any(c["source"] == source for c in state["pending"].values()):
                    output[source] = {"pending": "review existing chunks before collecting more"}
                    continue
                try:
                    entries = collect_source(source, state["sources"].get(source),
                                             args.bootstrap, args.fixtures)
                    chunks = chunks_for(source, entries)
                    if len(state["pending"]) + len(chunks) > args.chunks:
                        raise ValueError("pending chunk limit exceeded; review pending or raise --chunks (maximum 64)")
                    state["pending"].update(chunks)
                    if entries:
                        state["sources"][source] = {k: v for k, v in entries[0].items() if k != "body"}
                    output[source] = {"new_entries": len(entries), "new_chunks": len(chunks),
                                      "baseline": state["sources"].get(source)}
                except (OSError, ValueError, KeyError, TypeError) as error:
                    output[source] = {"error": str(error)}
                    code = 1
        elif args.command == "pending":
            output = list(state["pending"].values())[:args.limit]
        else:
            report = args.report.resolve()
            raw = report.read_bytes()
            receipts = set(raw.decode("utf-8").splitlines())
            ids = list(dict.fromkeys(args.ids))
            for key in ids:
                if key not in state["pending"]:
                    raise ValueError(f"unknown pending chunk: {key}")
                if f"Chunk: {key}" not in receipts:
                    raise ValueError(f"saved report lacks exact receipt line: Chunk: {key}")
            state["reviews"].append({"report": str(report),
                                     "sha256": hashlib.sha256(raw).hexdigest(), "chunks": ids})
            for key in ids:
                del state["pending"][key]
            output = {"acknowledged": ids, "report": str(report)}
        if original != json.dumps(state, sort_keys=True) or not args.state.exists():
            save(args.state, state)
        print(json.dumps(output, indent=2, ensure_ascii=False))
        return code


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        raise SystemExit(str(error)) from error
