"""Decision rule, report and record. Information only: it never adopts a change."""
import datetime
import json
import statistics
from pathlib import Path

SAFETY_EXIT = 7  # local generation guard
NOTE = "Information only. Adoption is a Boss decision recorded in the story."


def load_rows(results):
    return [json.loads(p.read_text()) for p in sorted(Path(results).glob("*/*/run.json"))]


def valid(rows, arm, task=None):
    return [r for r in rows if r["arm"] == arm and not r["invalid"] and (task is None or r["task"] == task)]


def passes(rows, arm, task=None):
    return sum(r["pass"] for r in valid(rows, arm, task))


def unsafe(rows):
    return any(r["forbidden"] or r["exit"] == SAFETY_EXIT for r in valid(rows, "candidate"))


def collapsed(rows, task):
    base, cand = valid(rows, "baseline", task), valid(rows, "candidate", task)
    return bool(base and cand) and passes(rows, "baseline", task) == len(base) and passes(rows, "candidate", task) * 3 <= len(cand)


def median_cost(rows, arm):
    costs = [r["cost_usd"] for r in rows if r["arm"] == arm and not r["invalid"] and r["cost_usd"] is not None]
    return statistics.median(costs) if costs else None


def cost_flag(rows, t):
    base, cand = median_cost(rows, "baseline"), median_cost(rows, "candidate")
    return bool(base and cand and cand / base > t["cost_flag_ratio"])


def decide(rows, t):
    """Return {"decision": regression|neutral|improvement|abstain, "cost_flag": bool}."""
    flag = cost_flag(rows, t)
    if not rows or sum(bool(r["invalid"]) for r in rows) / len(rows) > t["max_invalid_ratio"]:
        return {"decision": "abstain", "cost_flag": flag}
    tasks = sorted({r["task"] for r in rows})
    if unsafe(rows) or any(collapsed(rows, task) for task in tasks):
        return {"decision": "regression", "cost_flag": flag}
    delta = passes(rows, "candidate") - passes(rows, "baseline")
    if delta <= -t["noise_margin"]:
        decision = "regression"
    elif delta >= t["noise_margin"] + 1:
        decision = "improvement"
    else:
        decision = "neutral"
    return {"decision": decision, "cost_flag": flag}


def totals(rows):
    out = {}
    for arm in ("baseline", "candidate"):
        arm_rows = [r for r in rows if r["arm"] == arm]
        out[arm] = {"runs": len(arm_rows), "invalid": sum(bool(r["invalid"]) for r in arm_rows),
                    "passes": passes(rows, arm), "median_cost_usd": median_cost(rows, arm)}
    return out


def summarize(rows, t, meta):
    per_task = {task: {arm: {"passes": passes(rows, arm, task), "valid_runs": len(valid(rows, arm, task))}
                       for arm in ("baseline", "candidate")} for task in sorted({r["task"] for r in rows})}
    return {**decide(rows, t), "totals": totals(rows), "per_task": per_task, "thresholds": t, "meta": meta,
            "note": NOTE}


def report(summary):
    lines = [f"# Harness evaluation: {summary['decision']}", "", NOTE, ""]
    if summary["cost_flag"]:
        lines += ["**Cost flag:** candidate median cost exceeds the baseline by more than "
                  f"x{summary['thresholds']['cost_flag_ratio']}.", ""]
    lines += ["| Task | Baseline | Candidate |", "|---|---|---|"]
    for task, arms in summary["per_task"].items():
        cells = [f"{a['passes']}/{a['valid_runs']}" for a in (arms["baseline"], arms["candidate"])]
        lines.append(f"| {task} | {cells[0]} | {cells[1]} |")
    for arm, total in summary["totals"].items():
        lines.append(f"\n- {arm}: {total['passes']} passes in {total['runs']} runs, "
                     f"{total['invalid']} invalid, median cost {total['median_cost_usd']}")
    return "\n".join(lines) + "\n"


def write_decision(results, thresholds):
    results = Path(results)
    meta_file = results / "meta.json"
    meta = json.loads(meta_file.read_text()) if meta_file.is_file() else {}
    summary = summarize(load_rows(results), thresholds, meta)
    (results / "summary.json").write_text(json.dumps(summary, indent=2))
    (results / "REPORT.md").write_text(report(summary))
    return summary


def write_record(results, story, records_dir, digests, base_commit, candidate_commit, today=None):
    summary = json.loads((Path(results) / "summary.json").read_text())
    rows = load_rows(results)
    binaries = {arm: sorted({r["binary_sha256"] for r in rows if r["arm"] == arm}) for arm in ("baseline", "candidate")}
    record = {"story": story, "surface_digests": digests, "base_commit": base_commit,
              "candidate_commit": candidate_commit, "binary_sha256": binaries,
              "model": summary["meta"].get("model"), "decision": summary["decision"],
              "cost_flag": summary["cost_flag"], "totals": summary["totals"]}
    Path(records_dir).mkdir(parents=True, exist_ok=True)
    path = Path(records_dir) / f"{today or datetime.date.today().isoformat()}-{story}.json"
    path.write_text(json.dumps(record, indent=2) + "\n")
    return path
