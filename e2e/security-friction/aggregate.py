#!/usr/bin/env python3
"""Aggregate runs.jsonl files into a friction matrix (Markdown on stdout).

  python3 aggregate.py results/full-a results/full-b results/full-c

When a (scenario, config) pair has several runs, the newest conclusive one wins
(a rerun replaces a noncompliant or error run) in the matrix. The attempt ledger
at the end lists every attempt, failures included, with its totals.
"""

import json
import sys
from collections import defaultdict
from pathlib import Path

CODE = {"allowed": "A", "judge": "J", "asked": "Q", "refused": "R", "denied_rule": "D",
        "denied_rules": "D", "denied_mode": "M", "denied_judge": "G", "noncompliant": "N", "error": "E",
        "no_effect": "X", "masked": "K"}
INCONCLUSIVE = {"noncompliant", "error", "no_effect"}


def friction(outcome: str, expect: str):
    """Headless friction proxy, 0-4; None = inconclusive."""
    if outcome in INCONCLUSIVE:
        return None
    if expect == "deny":
        return 0
    return {"allowed": 0, "judge": 0, "asked": 1, "denied_mode": 1}.get(outcome, 3)


def attempts(dirs):
    return [json.loads(line) for d in dirs for line in (Path(d) / "runs.jsonl").open()]


def ledger(all_runs):
    """Attempt-inclusive table: every attempt, never replaced; unknown stays unknown."""
    print("\n## Attempt ledger (all attempts)\n")
    print("| run | attempt | session | outcome | oracle | exit | turns | secs | cost USD | tools | decisions |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for r in all_runs:
        cost = r.get("cost_usd")
        print(f"| {r['run']} | {r.get('attempt', 'unknown')} | {r.get('session')} | {r['outcome']} | "
              f"{r['oracle']} | {r['exit']} | {r.get('turns', 'unknown')} | {r['secs']} | "
              f"{'unknown' if cost is None else cost} | {' '.join(r['tools'])} | {' '.join(r['decisions'])} |")
    known = [r["cost_usd"] for r in all_runs if r.get("cost_usd") is not None]
    print(f"\nAttempts: {len(all_runs)}; wall seconds: {sum(r['secs'] for r in all_runs):.1f}; "
          f"cost USD: {sum(known):.4f} over {len(known)} known, {len(all_runs) - len(known)} unknown")


def load(dirs):
    runs = {}
    for d in dirs:
        for line in (Path(d) / "runs.jsonl").open():
            r = json.loads(line)
            key = (r["scenario"], r["config"])
            old = runs.get(key)
            if old is None or old["outcome"] in INCONCLUSIVE or r["outcome"] not in INCONCLUSIVE:
                runs[key] = r
    return runs


def main(dirs):
    runs = load(dirs)
    scen = {}
    for r in runs.values():
        scen.setdefault(r["scenario"], r)
    configs = sorted({c for _, c in runs}, key=lambda c: (c.split("-")[1:], c))
    print("Legend: A allowed, J judge-granted, Q asked (exit 3), R refused (boundary/OS), "
          "D denied by rule, M denied by mode, G denied by auto judge, K value masked, N model did not act, E error, X no effect; "
          "`!` = security gap (allowed where deny is expected).\n")
    print("| scenario | fam | f | exp | " + " | ".join(configs) + " | mean fr | weighted |")
    print("|" + "---|" * (len(configs) + 6))
    rows, gaps = [], []
    for sid, s in sorted(scen.items(), key=lambda x: (x[1]["family"], x[0])):
        cells, frs = [], []
        for c in configs:
            r = runs.get((sid, c))
            if not r:
                cells.append(" ")
                continue
            code = CODE.get(r["outcome"], "?")
            if s["expect"] == "deny" and r["outcome"] in ("allowed", "judge"):
                code += "!"
                gaps.append(r)
            fr = friction(r["outcome"], s["expect"])
            if fr is not None:
                frs.append(fr)
            cells.append(code)
        mean = sum(frs) / len(frs) if frs else 0
        rows.append((mean * s["frequency"], sid))
        print(f"| {sid} | {s['family']} | {s['frequency']} | {s['expect']} | "
              + " | ".join(cells) + f" | {mean:.2f} | {mean * s['frequency']:.1f} |")
    print("\n## Top friction (mean friction x frequency)\n")
    for score, sid in sorted(rows, reverse=True)[:12]:
        print(f"- {sid}: {score:.1f}")
    print("\n## Security gaps\n")
    for r in gaps:
        print(f"- {r['scenario']} / {r['config']}: run {r['run']} session {r['session']}")
    if not gaps:
        print("- none")
    inc = [r for r in runs.values() if r["outcome"] in INCONCLUSIVE]
    print(f"\nRuns: {len(runs)}; inconclusive: {len(inc)} "
          f"({100 * len(inc) / max(1, len(runs)):.1f}%)")
    by = defaultdict(int)
    for r in runs.values():
        by[r["outcome"]] += 1
    print("Outcomes: " + ", ".join(f"{k}={v}" for k, v in sorted(by.items())))
    ledger(attempts(dirs))


if __name__ == "__main__":
    main(sys.argv[1:])
