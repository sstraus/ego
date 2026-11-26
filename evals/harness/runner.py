"""Paired runner: run a baseline and a candidate ego binary on the frozen suite."""
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from decimal import Decimal
from pathlib import Path

import suite

HERE = Path(__file__).resolve().parent
EXIT_REASONS = {4: "provider_error", 2: "usage_error", 130: "interrupted", 143: "interrupted"}
WALL_CLOCK_LIMIT = 1200


def order(tasks, runs):
    for rep in range(runs):
        for i, task in enumerate(tasks):
            arms = ("baseline", "candidate") if (rep + i) % 2 == 0 else ("candidate", "baseline")
            for arm in arms:
                yield task, arm, rep


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def tree_digest(root):
    return {p.relative_to(root).as_posix(): sha256_file(p) for p in sorted(Path(root).rglob("*"))
            if p.is_file() and "__pycache__" not in p.parts}


def parse_events(text):
    """Return (tokens, cost_usd or None, tool_calls) from jsonl events."""
    tokens, tools, cost, unknown, priced = {}, {}, Decimal(0), False, False
    for line in text.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "item_started" and event.get("kind") == "tool":
            tools[event["name"]] = tools.get(event["name"], 0) + 1
        elif event.get("type") == "usage_updated":
            for key, value in event.get("usage_delta", {}).items():
                tokens[key] = tokens.get(key, 0) + value
            delta = event.get("cost_delta", {})
            if delta.get("cost") == "known" and delta["amount"]["currency"] == "USD":
                cost, priced = cost + Decimal(delta["amount"]["amount"]), True
            else:
                unknown = True
    return tokens, (float(cost) if priced and not unknown else None), tools


def names_suite(directory, needle):
    return any(needle.encode() in p.read_bytes() for p in Path(directory).rglob("*") if p.is_file())


def score(task_dir, work, hidden):
    scoring = Path(tempfile.mkdtemp(prefix="harness-score-"))
    try:
        shutil.copytree(work, scoring, dirs_exist_ok=True)
        shutil.copy(task_dir / hidden, scoring / Path(hidden).name)
        results = suite.run_hidden(scoring, hidden)
    finally:
        shutil.rmtree(scoring, ignore_errors=True)
    return bool(results) and all(results.values())


def run_one(task_dir, arm, rep, binary, config, out, env, work_root):
    meta = tomllib.loads((task_dir / "task.toml").read_text())
    run_dir = Path(out).resolve() / meta["id"] / f"{arm}-{rep}"
    run_dir.mkdir(parents=True)
    work = Path(tempfile.mkdtemp(prefix=f"{meta['id']}-", dir=work_root)) / "work"
    shutil.copytree(task_dir / "workspace", work)
    before = tree_digest(work)
    argv = [str(binary), "run", "-C", str(work), "--session-dir", str(run_dir / "sessions"),
            *config["flags"], "--output", "jsonl", "-m", config["model"], meta["prompt"]]
    start = time.monotonic()
    try:
        done = subprocess.run(argv, capture_output=True, text=True, env=env, cwd=work,
                              stdin=subprocess.DEVNULL, timeout=WALL_CLOCK_LIMIT)
        code, stdout = done.returncode, done.stdout
        (run_dir / "stderr.log").write_text(done.stderr)
    except subprocess.TimeoutExpired:
        code, stdout = 124, ""
    seconds = round(time.monotonic() - start, 1)
    (run_dir / "events.jsonl").write_text(stdout)
    after = tree_digest(work)
    changed = sorted(k for k in set(before) | set(after) if before.get(k) != after.get(k))
    forbidden = [c for c in changed if c not in meta["editable"]]
    invalid = EXIT_REASONS.get(code)
    suite_path = str(task_dir.parent.resolve())
    if names_suite(run_dir / "sessions", suite_path) or suite_path in stdout:
        invalid = "leakage"
    elif invalid is None and not stdout.strip():
        invalid = "no_events"
    tokens, cost, tools = parse_events(stdout)
    passed = invalid is None and not forbidden and score(task_dir, work, meta["hidden"])
    row = {"task": meta["id"], "arm": arm, "rep": rep, "exit": code, "seconds": seconds,
           "pass": bool(passed), "forbidden": forbidden, "invalid": invalid, "tokens": tokens,
           "cost_usd": cost, "tool_calls": tools, "binary_sha256": sha256_file(binary)}
    (run_dir / "run.json").write_text(json.dumps(row, indent=2))
    shutil.rmtree(work.parent, ignore_errors=True)
    return row


def run_matrix(suite_dir, baseline, candidate, config, tasks, runs, out, env, work_root=None):
    suite_dir = Path(suite_dir)
    ids = tasks or sorted(p.parent.name for p in suite_dir.glob("*/task.toml"))
    binaries = {"baseline": baseline, "candidate": candidate}
    work_root = work_root or tempfile.mkdtemp(prefix="harness-work-")
    rows = []
    for task, arm, rep in order(ids, runs):
        rows.append(run_one(suite_dir / task, arm, rep, binaries[arm], config, out, env, work_root))
    return rows
