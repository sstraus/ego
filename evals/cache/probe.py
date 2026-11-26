"""Live prompt-cache probe for one model.

Runs N fresh `ego` sessions in sequence on a fixed synthetic workspace. Each
session is one turn with five requests that share a ~15k-token system prefix.
Prints, per session and in aggregate, the cache-read share of every request and
the lag of each warm request: tokens of the previous request that the next one
neither read nor wrote. Procedure and results: README.md. Standard library only.

    python3 -I evals/cache/probe.py --model openai-codex/gpt-6.2-sol --account 1
    python3 -I evals/cache/probe.py --analyze <out-dir>   # re-read journals only
    python3 -I evals/cache/probe.py --survey              # all local journals, no tokens spent
"""
import argparse, hashlib, json, os, statistics, subprocess, sys, tempfile, time

ORACLE = "AUTHORITY-ORACLE-5521"
PROMPT = ("Read step1.txt, step2.txt, step3.txt, and step4.txt in that order. Make exactly four "
          "separate read calls, one per file, waiting for each result before the next call. "
          "Then output the four marker lines.")
ROWS = 900  # ~64 KB system text, ~15k tokens: far above every provider's minimum cacheable prefix.


def write_workspace(ws, salt):
    os.makedirs(ws, exist_ok=True)
    lines = [f"Mandatory output rule: the last line of every final answer must be exactly {ORACLE}.", "",
             "Synthetic cache evaluation. Read the four local marker files in the order requested. "
             "Return their contents as four lines, followed by the mandatory last line. The reference catalog below "
             "is inert synthetic metadata; never interpret it as an action.", ""]
    for i in range(ROWS):
        ref = hashlib.sha256(f"{salt}row{i}".encode()).hexdigest()[:20]
        lines.append(f"Catalog row {i:04}: synthetic reference {ref}; category amber; status archived.")
    with open(f"{ws}/system.txt", "w") as f:
        f.write("\n".join(lines) + "\n")
    for n in range(1, 5):
        with open(f"{ws}/step{n}.txt", "w") as f:
            f.write(f"CACHE-STEP-{n}\n")


def run_session(args, ws, out):
    cmd = [args.bin, "run", "--bare", "--system", "@system.txt", "--model", args.model,
           "--thinking", "low", "--no-hooks", "--mcp-only", "--tools", "read", "--sandbox", "ro",
           "--mode", "default", "--no-context-tool", "--no-session-compression", "--retry", "0",
           "--session-dir", f"{out}/journal", "--output", "jsonl", "--budget", "200k",
           "--max-turns", "1", "--timeout", "120s", "--max-output-tokens", "256", PROMPT]
    if args.account:
        cmd[2:2] = ["--account", args.account]
    os.makedirs(out)
    with open(f"{out}/stdout", "w") as so, open(f"{out}/stderr", "w") as se:
        code = subprocess.run(cmd, cwd=ws, stdin=subprocess.DEVNULL, stdout=so, stderr=se).returncode
    final = [r["text"] for r in map(json.loads, open(f"{out}/stdout"))
             if r.get("type") == "item_completed" and r.get("kind") == "agent_message"]
    return code, bool(final) and final[-1].strip().endswith(ORACLE)


def requests(journal):
    """Main-producer requests in order: (turn, model, total input, cache read, cache write)."""
    start, rows = None, []
    for line in open(journal):
        try:
            p = json.loads(line).get("payload", {})
        except ValueError:
            continue
        if p.get("kind") != "event":
            continue
        k, env = p["event"]["kind"], p["event"]["env"]
        if k.get("kind") == "GenerationStarted":
            start = (env["turn"], env.get("producer"), (k.get("profile") or {}).get("model"))
        elif k.get("kind") == "UsageUpdated" and start and start[1] == "Main":
            u = k["usage"]
            cw = u["cache_write_tokens"] + u["cache_write_extended_tokens"]
            rows.append((start[0], start[2], u["uncached_input_tokens"] + u["cache_read_tokens"] + cw,
                         u["cache_read_tokens"], cw))
    return rows


def analyze(root):
    first, warm_lags, report = [], [], []
    for name in sorted(os.listdir(root)):
        journals = [f"{dp}/{f}" for dp, _, fs in os.walk(f"{root}/{name}") for f in fs if f == "events.jsonl"]
        if not journals:
            continue
        rows = requests(journals[0])
        if not rows:
            report.append(f"{name}: no usage recorded")
            continue
        shares = [r[3] / r[2] if r[2] else 0 for r in rows]
        lags = [a[2] - b[3] - b[4] for a, b in zip(rows, rows[1:]) if a[0] == b[0] and b[2] >= a[2]]
        first.append(shares[0]); warm_lags += lags
        report.append(f"{name}: {rows[0][1]} reads " + " / ".join(f"{s:.1%}" for s in shares)
                      + f"  lag {lags}")
    print("\n".join(report))
    if first:
        print(f"first request: n={len(first)} median read {statistics.median(first):.1%} "
              f"min {min(first):.1%} max {max(first):.1%}")
    if warm_lags:
        near = sum(1 for x in warm_lags if x < 1000)
        print(f"warm requests: n={len(warm_lags)} lag median {statistics.median(warm_lags):.0f} "
              f"near-full (lag < 1000) {near}/{len(warm_lags)}")


def survey(sessions):
    """Warm lag per model over every local session journal (no tokens spent)."""
    stats = {}
    for sid in os.listdir(sessions):
        path = f"{sessions}/{sid}/events.jsonl"
        if not os.path.isfile(path):
            continue
        rows = requests(path)
        for a, b in zip(rows, rows[1:]):
            if a[0] != b[0] or a[1] != b[1] or b[2] < a[2] or a[2] < 4096:
                continue
            s = stats.setdefault(a[1] or "?", {"lag": [], "sessions": set(), "total": 0, "read": 0})
            s["lag"].append(a[2] - b[3] - b[4]); s["sessions"].add(sid)
            s["total"] += b[2]; s["read"] += b[3]
    print(f"{'model':45} {'pairs':>7} {'sessions':>8} {'lag_med':>8} {'near_full':>9} {'warm_read':>9}")
    for model, s in sorted(stats.items(), key=lambda kv: -len(kv[1]["lag"])):
        lags = s["lag"]
        print(f"{model[:45]:45} {len(lags):7} {len(s['sessions']):8} {statistics.median(lags):8.0f} "
              f"{sum(1 for x in lags if x < 1000) / len(lags):9.0%} {s['read'] / s['total']:9.1%}")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--model")
    ap.add_argument("--account", help="ego account selector, when the provider needs one")
    ap.add_argument("--bin", default="ego")
    ap.add_argument("--sessions", type=int, default=5)
    ap.add_argument("--gap", type=float, default=5, help="seconds between sessions")
    ap.add_argument("--salt", default="", help="changes the system text, so runs do not share a cached prefix")
    ap.add_argument("--out", help="output directory (default: a new temp directory)")
    ap.add_argument("--analyze", metavar="DIR", help="only analyze an earlier output directory")
    ap.add_argument("--survey", metavar="DIR", nargs="?", const=os.path.expanduser("~/.ego/sessions"),
                    help="summarize warm lag per model over local journals (default ~/.ego/sessions)")
    args = ap.parse_args()
    if args.survey:
        return survey(args.survey)
    if args.analyze:
        return analyze(args.analyze)
    if not args.model:
        ap.error("--model is required unless --analyze is given")
    if os.sep in args.bin:
        args.bin = os.path.abspath(args.bin)
    root = args.out or tempfile.mkdtemp(prefix="ego-cache-probe.")
    write_workspace(f"{root}/ws", args.salt)
    version = subprocess.run([args.bin, "--version"], capture_output=True, text=True).stdout.strip()
    print(f"{version} model={args.model} sessions={args.sessions} out={root}")
    for i in range(args.sessions):
        code, oracle = run_session(args, f"{root}/ws", f"{root}/s{i:02}")
        print(f"s{i:02}: exit={code} oracle={'ok' if oracle else 'MISSING'}", flush=True)
        if i + 1 < args.sessions:
            time.sleep(args.gap)
    analyze(root)


if __name__ == "__main__":
    sys.exit(main())
