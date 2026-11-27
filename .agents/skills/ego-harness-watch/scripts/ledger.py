#!/usr/bin/env python3
"""Proposal ledger: Boss decides each harness gap once; stories are linked once.

A proposal item is one Markdown checkbox line that ends its bold title with
`<!-- hw:ID -->`. `sync` records Boss's ticks, `link` binds an approved item to
its story and writes the story ID into the proposal, and `list` gives the next
weekly run every item that it must not propose again.
"""
import argparse
import datetime
import fcntl
import json
from pathlib import Path
import re
import sys
import tempfile

ITEM = re.compile(r"^(?P<lead>\s*- \[)(?P<mark>[ xX])(?P<rest>\].*?<!-- hw:(?P<id>[a-z0-9][a-z0-9-]{0,63}) -->)(?P<tail>.*)$")
TITLE = re.compile(r"\*\*(.+?)\*\*")
STORY = re.compile(r"^\d{3,}-[0-9a-f]{4}$")


def load(path):
    return json.loads(path.read_text()) if path.exists() else {"items": {}}


def save(path, ledger):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as tmp:
        json.dump(ledger, tmp, indent=2, sort_keys=True)
        tmp.write("\n")
    Path(tmp.name).replace(path)


def items(text):
    found = {}
    for number, line in enumerate(text.split("\n")):
        match = ITEM.match(line)
        if not match:
            continue
        item_id = match["id"]
        if item_id in found:
            raise ValueError(f"duplicate item id {item_id}")
        title = TITLE.search(line)
        found[item_id] = {"line": number, "approved": match["mark"] in "xX",
                          "title": title[1].strip() if title else item_id}
    return found


def sync(ledger, proposal):
    today = datetime.date.today().isoformat()
    report = {"needs_story": [], "approved": [], "rejected": [], "unchanged": []}
    for item_id, item in items(proposal.read_text()).items():
        entry = ledger["items"].setdefault(item_id, {"first_proposed": today})
        status = "approved" if item["approved"] else "rejected"
        if entry.get("story"):
            report["unchanged"].append(item_id)
            continue
        if entry.get("status") != status:
            entry.update(status=status, decided=today)
        entry.update(title=item["title"], proposal=str(proposal))
        report[status].append(item_id)
        if status == "approved":
            report["needs_story"].append(item_id)
    return report


def link(ledger, item_id, story):
    if not STORY.match(story):
        raise ValueError(f"not a story id: {story}")
    entry = ledger["items"].get(item_id)
    if not entry or entry.get("status") != "approved":
        raise ValueError(f"{item_id} is not an approved item")
    if entry.get("story") not in (None, story):
        raise ValueError(f"{item_id} already has story {entry['story']}")
    entry["story"] = story
    proposal = Path(entry["proposal"])
    proposal.write_text(move_to_implemented(proposal.read_text(), item_id, story))
    return {"linked": item_id, "story": story}


def move_to_implemented(text, item_id, story):
    """Tag the item with its story and move it, with its indented lines, to the
    end of the "## Implemented" section (created at the end when missing)."""
    lines = text.split("\n")
    found = items(text)
    if item_id not in found:
        raise ValueError(f"{item_id} is missing from the proposal")
    start = found[item_id]["line"]
    match = ITEM.match(lines[start])
    if not match or match["id"] != item_id:
        raise ValueError(f"{item_id} does not match the proposal line")
    existing = re.match(r" \(story (\d{3,}-[0-9a-f]{4})\)", match["tail"])
    if existing and existing[1] != story:
        raise ValueError(f"{item_id} already has a different story in the proposal")
    end = start + 1
    while end < len(lines) and lines[end][:1].isspace() and lines[end].strip():
        end += 1
    match = ITEM.match(lines[start])
    line = lines[start] if existing else (
        f"{match['lead']}{match['mark']}{match['rest']} (story {story}){match['tail']}"
    )
    block = [line, *lines[start + 1:end]]
    del lines[start:end]
    if "## Implemented" not in lines:
        lines += ["", "## Implemented", ""]
    at = lines.index("## Implemented") + 1
    while at < len(lines) and not lines[at].startswith("#"):
        at += 1
    while lines[at - 1] == "" and at - 1 > lines.index("## Implemented") + 1:
        at -= 1
    lines[at:at] = block
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ledger", type=Path, required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("sync").add_argument("proposal", type=Path)
    linker = commands.add_parser("link")
    linker.add_argument("item")
    linker.add_argument("story")
    commands.add_parser("list")
    args = parser.parse_args(argv)
    args.ledger.parent.mkdir(parents=True, exist_ok=True)
    with args.ledger.with_suffix(".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        ledger = load(args.ledger)
        try:
            if args.command == "sync":
                result = sync(ledger, args.proposal)
            elif args.command == "link":
                result = link(ledger, args.item, args.story)
            else:
                result = ledger["items"]
        except ValueError as error:
            print(json.dumps({"error": str(error)}))
            return 1
        if args.command != "list":
            save(args.ledger, ledger)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
