#!/usr/bin/env python3
"""Opt-in bounded subscription cache workload; never selects a paid API route."""
import argparse
from collections import Counter
from datetime import datetime, timezone
from decimal import Decimal
import importlib.util
import json
import math
import os
from pathlib import Path
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("conformance", ROOT / "scripts/live-conformance.py")
conformance = importlib.util.module_from_spec(spec)
spec.loader.exec_module(conformance)


def upper_bound(misses, count):
    """One-sided 95% exact binomial bound; requests are not independent in practice."""
    if not count or misses == count:
        return 1.0
    low, high = misses / count, 1.0
    for _ in range(80):
        probability = (low + high) / 2
        terms = [math.lgamma(count + 1) - math.lgamma(k + 1) - math.lgamma(count - k + 1)
                 + k * math.log(probability) + (count - k) * math.log1p(-probability)
                 for k in range(misses + 1)]
        peak = max(terms)
        cdf = math.exp(peak) * sum(math.exp(term - peak) for term in terms)
        if cdf > .05:
            low = probability
        else:
            high = probability
    return high


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--sessions", type=int, default=3)
    parser.add_argument("--turns", type=int, default=60)
    parser.add_argument("--max-seconds", type=int, default=3600)
    args = parser.parse_args()
    if min(args.sessions, args.turns, args.max_seconds) < 1:
        parser.error("bounds must be positive")
    if not args.live:
        print(json.dumps({"provider": "openai-codex", "live": False,
                          "sessions": args.sessions, "turns": args.turns}))
        return 0
    os.umask(0o077)
    model = "openai-codex/gpt-6-luna"
    runner = conformance.Runner(argparse.Namespace(
        binary=args.binary.resolve(), work=args.work, ceiling=Decimal("0.95"),
        provider_ceiling=Decimal("0.90"), operator_home=None,
        diagnostic_dir=args.work / "diagnostics", ego_env=ROOT / ".env",
        router_env=ROOT / ".env"))
    before_hash = conformance.digest(runner.args.binary)
    started = datetime.now(timezone.utc).isoformat()
    start = time.monotonic()
    sessions = []
    stop = None
    for _ in range(args.sessions):
        workspace = runner.workspace("long-cache")
        # A stable ~large-input reference makes this a long-prefix workload,
        # not a claim about the natural-workload cohort or matched CLI latency.
        with (workspace / "AGENTS.md").open("a") as output:
            output.write("\n" + "\n".join(
                f"Reference {index:04}: fixture seed 419 records the stable amber ledger; retain this reference unchanged."
                for index in range(1800)))
        session = str(uuid.uuid4())
        item = {"session": session, "invocations": [], "requests": []}
        sessions.append(item)
        for turn in range(args.turns):
            if time.monotonic() - start >= args.max_seconds:
                stop = "wall_time_bound"
                break
            prompt = ("Read part-0.txt, part-1.txt, part-2.txt in three sequential read calls. "
                      f"Then reply LEDGER-419-OK. This is fixed turn {turn}.")
            call_start = time.monotonic()
            run = runner.invoke(model, workspace, prompt, session=session)
            item["requests"].extend(run["metrics"]["requests"])
            passed = run["exit"] == 0 and "LEDGER-419-OK" in run["stdout"]
            item["invocations"].append({"turn": turn, "exit": run["exit"],
                "reason": run["reason"], "oracle_pass": passed,
                "wall_seconds": time.monotonic() - call_start,
                "requests": len(run["metrics"]["requests"]),
                "read_calls": run["metrics"]["read_calls"]})
            if not passed or runner.uncertain or runner.spent != 0:
                stop = "invocation_failure_or_unsettled_subscription_usage"
                break
            # Incremental checkpoint, not a final acceptance report.
            (runner.work / "checkpoint.json").write_text(json.dumps(sessions, indent=2) + "\n")
        if stop:
            break
    after_hash = conformance.digest(runner.args.binary)
    later = [request for session in sessions for request in session["requests"][1:]]
    all_requests = [request for session in sessions for request in session["requests"]]
    eligible = [request for request in later if request["input_tokens"] > 4096]
    misses = sum(request["cache_read_tokens"] == 0 for request in eligible)
    total = sum(request["input_tokens"] for request in all_requests)
    reads = sum(request["cache_read_tokens"] for request in all_requests)
    result = {"schema": 1, "run_id": runner.run_id, "started": started,
        "ended": datetime.now(timezone.utc).isoformat(), "wall_seconds": time.monotonic() - start,
        "model": model, "binary_sha256_before": before_hash, "binary_sha256_after": after_hash,
        "binary_stable": before_hash == after_hash, "bounds": vars(args) | {"binary": str(args.binary), "work": str(args.work)},
        "stop": stop, "sessions": sessions, "session_count": len(sessions),
        "request_count": len(all_requests), "later_count": len(later), "eligible_later_count": len(eligible),
        "misses": misses, "miss_rate_all_later": misses / len(later) if later else None,
        "miss_rate_eligible_later": misses / len(eligible) if eligible else None,
        "independent_request_upper95": upper_bound(misses, len(later)),
        "input_tokens": total, "cache_read_tokens": reads,
        "weighted_share": reads / total if total else None,
        "cost_usd": str(runner.spent), "cost_uncertain": runner.uncertain,
        "role_counts": dict(Counter(request["role"] for request in all_requests)),
        "limitations": "Fixed synthetic tool/resume workload; correlated requests; not a matched CLI or natural-workload sample. No parity or causal latency claim."}
    target = runner.work / "result.json"
    target.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key not in ("sessions", "bounds")}, indent=2))
    print("Results:", target)
    return int(bool(stop) or before_hash != after_hash or runner.uncertain)


if __name__ == "__main__":
    raise SystemExit(main())
