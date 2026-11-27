#!/usr/bin/env python3
"""Persist refocus evidence and schedule refocus."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone


TRANSCRIPT_TAIL_BYTES = 256 * 1024
REFOCUS_INTERVAL_SECONDS = 60 * 60


def checkpoint_path(session_id: str) -> Path:
    digest = hashlib.sha256(session_id.encode("utf-8")).hexdigest()[:24]
    directory = Path(tempfile.gettempdir()) / "codex-workflow-refocus"
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(directory, 0o700)
    return directory / f"{digest}.md"


def state_path(session_id: str) -> Path:
    return checkpoint_path(session_id).with_suffix(".json")


def last_refocus(path: Path) -> float | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))["last_refocus"]
        return float(value)
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError):
        return None


def mark_refocus(path: Path, at: float) -> None:
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps({"last_refocus": at}), encoding="utf-8")
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)


def command_output(cwd: str, args: list[str]) -> str:
    try:
        result = subprocess.run(
            args,
            cwd=cwd,
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return f"unavailable: {error}"
    output = result.stdout.strip()
    if result.returncode != 0:
        detail = result.stderr.strip() or f"exit {result.returncode}"
        return f"unavailable: {detail}"
    return output or "none"


def transcript_tail(path_value: object) -> str:
    if not isinstance(path_value, str) or not path_value:
        return "unavailable: no transcript path"
    path = Path(path_value)
    try:
        with path.open("rb") as transcript:
            transcript.seek(0, os.SEEK_END)
            size = transcript.tell()
            transcript.seek(max(0, size - TRANSCRIPT_TAIL_BYTES))
            return transcript.read().decode("utf-8", errors="replace")
    except OSError as error:
        return f"unavailable: {error}"


def save_checkpoint(event: dict[str, object], target: Path) -> None:
    cwd = str(event.get("cwd") or ".")
    body = "\n".join(
        [
            "# Workflow refocus pre-compaction checkpoint",
            "",
            f"Captured: {datetime.now(timezone.utc).isoformat()}",
            f"Trigger: {event.get('trigger', 'unknown')}",
            f"Working directory: {cwd}",
            "",
            "## Git status",
            "",
            "```text",
            command_output(cwd, ["git", "status", "--short"]),
            "```",
            "",
            "## Recent commits",
            "",
            "```text",
            command_output(cwd, ["git", "log", "-8", "--oneline"]),
            "```",
            "",
            "## Pre-compaction transcript tail",
            "",
            "```jsonl",
            transcript_tail(event.get("transcript_path")),
            "```",
            "",
        ]
    )
    temporary = target.with_suffix(".tmp")
    temporary.write_text(body, encoding="utf-8")
    os.chmod(temporary, 0o600)
    os.replace(temporary, target)


def main() -> int:
    event = json.load(sys.stdin)
    session_id = str(event.get("session_id") or "unknown")
    target = checkpoint_path(session_id)
    state = state_path(session_id)
    name = event.get("hook_event_name")

    if name == "PreCompact":
        save_checkpoint(event, target)
        print(json.dumps({"continue": True, "suppressOutput": True}))
        return 0

    if name == "SessionStart" and event.get("source") == "compact":
        mark_refocus(state, time.time())
        context = (
            "Run repository-local $workflow-refocus before task work. "
            f"Read checkpoint first: {target}. Preserve scope and completed work. "
            "Apply one evidence-backed correction."
        )
        print(
            json.dumps(
                {
                    "hookSpecificOutput": {
                        "hookEventName": "SessionStart",
                        "additionalContext": context,
                    }
                }
            )
        )
        return 0

    if name == "Stop":
        now = time.time()
        previous = last_refocus(state)
        if event.get("stop_hook_active"):
            mark_refocus(state, now)
        elif previous is None:
            mark_refocus(state, now)
        elif now - previous >= REFOCUS_INTERVAL_SECONDS:
            print(
                json.dumps(
                    {
                        "decision": "block",
                        "reason": (
                            "Run repository-local $workflow-refocus. Use durable evidence. "
                            "Apply one safe correction. Report REFOCUS. Resume the active task."
                        ),
                    }
                )
            )
            return 0

    print(json.dumps({"continue": True, "suppressOutput": True}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
