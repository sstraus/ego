#!/usr/bin/env python3
"""Opt-in matched, subscription-only native Codex CLI / ego cache experiment."""
import argparse
from datetime import datetime, timezone
from decimal import Decimal
import hashlib
import json
import math
import os
from pathlib import Path
import re
import selectors
import statistics
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]


class TransportInterrupted(RuntimeError):
    """A settled zero-USD attempt lacks usage; never resend it."""


class RateLimitRefused(RuntimeError):
    """The accounting RPC refused; its raw error stays in a private file."""


class TurnTimedOut(RuntimeError):
    """Exclude a timed-out turn and abandon its pair without resending it."""


def run_turn(argv, env, cwd, prompt, timeout_seconds):
    try:
        return subprocess.run(argv, env=env, cwd=cwd, input=prompt, text=True,
                              capture_output=True, timeout=timeout_seconds), False
    except subprocess.TimeoutExpired as error:
        def text(value):
            return value.decode(errors="replace") if isinstance(value, bytes) else value or ""
        return subprocess.CompletedProcess(argv, 124, text(error.stdout), text(error.stderr)), True


def wall_distribution(calls):
    values = sorted(c["wall_seconds"] for c in calls)
    return {"calls": len(values), "timeouts": sum(c["exit"] == 124 for c in calls),
            "mean": statistics.mean(values) if values else None,
            "median": statistics.median(values) if values else None,
            "p95_nearest_rank": values[math.ceil(.95 * len(values)) - 1] if values else None,
            "max": values[-1] if values else None}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def clean_env():
    return {k: v for k, v in os.environ.items() if k not in (
        "OPENAI_API_KEY", "OPENAI_BASE_URL", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN", "EGO_MODEL", "EGO_LOG", "EGO_LOG_FILE", "CODEX_HOME")}


def checked_cost(rows):
    amount = Decimal(0)
    for row in rows:
        if row.get("kind") != "UsageUpdated":
            continue
        cost = row.get("cost_delta") or {}
        if cost.get("cost") != "known" or cost.get("amount", {}).get("currency") != "USD":
            raise RuntimeError("unknown ego cost")
        amount += Decimal(cost["amount"]["amount"])
    if amount != 0:
        raise RuntimeError("nonzero ego subscription cost")
    return str(amount)


def checked_credit(before, after):
    if before is None or after is None:
        raise RuntimeError("credit accounting unknown; stop before scaling")
    if Decimal(after) != Decimal(before):
        raise RuntimeError("account credit balance moved; stop before scaling")


def fisher_ego_worse(ego_zero, ego_n, codex_zero, codex_n):
    """One-tailed conditional Fisher P(X >= ego_zero), with fixed margins."""
    misses = ego_zero + codex_zero
    return sum(math.comb(ego_n, x) * math.comb(codex_n, misses - x)
               for x in range(ego_zero, min(misses, ego_n) + 1)
               if 0 <= misses - x <= codex_n) / math.comb(ego_n + codex_n, misses)


def cli_transport_evidence(text):
    """Classify request events, never connection attempts or a client's default."""
    counts = {"websocket": 0, "http": 0}
    for line in text.splitlines():
        if not re.search(r"codex_otel(?:::|\.log_only)", line):
            continue
        if re.search(r"\bcodex\.websocket_request\b", line):
            counts["websocket"] += 1
        if re.search(r"\bcodex\.api_request\b", line) and "/responses" in line:
            counts["http"] += 1
    observed = [name for name, count in counts.items() if count]
    return {"transport": observed[0] if len(observed) == 1 else "mixed" if observed else "unknown",
            "request_event_counts": counts,
            "basis": "native log-only request telemetry; mixed includes retries/fallbacks"}


def summary(requests):
    later = [r for r in requests if r["ordinal"] > 1]
    eligible = [r for r in later if r["input_tokens"] > 4096]
    misses = sum(r["cache_read_tokens"] == 0 for r in eligible)
    total = sum(r["input_tokens"] for r in requests)
    read = sum(r["cache_read_tokens"] for r in requests)
    later_input = sum(r["input_tokens"] for r in later)
    latency = [r["generation_interval_seconds"] for r in requests]
    return {"requests": len(requests), "later": len(later), "later_large": len(eligible),
            "later_large_zero": misses, "zero_share_large_later": misses / len(eligible) if eligible else None,
            "input_tokens": total, "cache_read_tokens": read,
            "weighted_share": read / total if total else None,
            "weighted_later_share": sum(r["cache_read_tokens"] for r in later) / later_input if later_input else None,
            "generation_interval_mean_seconds": statistics.mean(latency) if latency else None,
            "generation_interval_median_seconds": statistics.median(latency) if latency else None}


class Experiment:
    def __init__(self, args):
        self.args = args
        self.started_monotonic = time.monotonic()
        self.base = args.work
        self.base.mkdir(parents=True, exist_ok=args.resume, mode=0o700)
        self.env = clean_env()
        self.env.update(NO_COLOR="1", RUST_LOG="off")
        self.result = {"schema": 1, "started": datetime.now(timezone.utc).isoformat(),
                       "binary_sha256": {"ego": digest(args.binary.resolve()), "codex": digest(Path(args.codex).resolve())},
                       "model": "gpt-6-luna", "thinking": "low", "requests": {"ego": [], "codex": []},
                       "calls": [], "cost_checks": [], "stop": None,
                       "latency_definition": "Codex task_started to single new token_count; ego GenerationStarted to UsageUpdated. Includes client/transport work, not server-only latency. Whole invocation timing retained separately.",
                       "workload": f"Identical prompts, initial 700-row stable reference, no tools, one generation per invocation; paired sessions of {args.turns} turns, target {args.target_later_large} matched later large per side, plus two-turn admission pilot. Native built-in instruction/tool overhead is not erased.",
                       "bounds": {"turns": args.turns, "pairs": args.pairs, "target_later_large": args.target_later_large, "wall_seconds": args.wall_seconds}}
        if args.resume:
            previous = json.loads((self.base / "result.json").read_text())
            if previous["binary_sha256"] != self.result["binary_sha256"]:
                raise RuntimeError("resume binary hashes changed")
            self.result = previous
            self.result.setdefault("prior_stops", []).append(self.result["stop"])
            self.result["stop"] = None
        self.restart_pair = None
        if args.restart_last_pair:
            if not args.resume or not self.result["calls"]:
                raise RuntimeError("fresh restart needs the retained aborted pair")
            last = self.result["calls"][-1]
            kinds = [json.loads(line).get("payload", {}).get("event", {}).get("kind", {})
                     for line in Path(last["journal"]).read_text().splitlines()]
            settled = [k for k in kinds if k.get("kind") == "AttemptSettled"]
            if last["side"] != "ego" or last["exit"] != 124 or not settled or settled[-1]["outcome"].get("delivery") != "definitely_unsent":
                raise RuntimeError("fresh restart is not a proven definitely-unsent ego deadline")
            checked_cost(kinds)
            self.restart_pair = int(last["label"].split("-")[1])
        for row in self.result["requests"]["codex"]:
            row.setdefault("transport_evidence", {"transport": "unknown", "basis": "historical journal-only collection"})
        self.result.setdefault("timeouts_300", [])
        self.result.setdefault("turn_deadline_windows", []).append({"started": datetime.now(timezone.utc).isoformat(), "turn_seconds": args.turn_timeout_seconds, "wall_seconds": args.wall_seconds})
        self.result.setdefault("interruptions", [])
        self.result["recovery_policy"] = f"At most {args.pairs} paired sessions; abandon a pair after settled zero-USD transient missing-usage failure. Never resend possibly-sent work. Retain failures and unmatched completed requests separately."
        if args.resume:
            self.result.setdefault("resumed_at", []).append(datetime.now(timezone.utc).isoformat())
            for call in self.result["calls"]:
                if call["exit"] == 4 and call["side"] == "ego":
                    kinds = [json.loads(line).get("payload", {}).get("event", {}).get("kind", {})
                             for line in Path(call["journal"]).read_text().splitlines()]
                    checked_cost(kinds)
                    settlements = [k for k in kinds if k.get("kind") == "AttemptSettled"]
                    if not settlements or settlements[-1]["outcome"].get("usage") != "missing":
                        raise RuntimeError("resume failure is not a known settled missing-usage attempt")
                    if not any(x["session"] == call["session"] for x in self.result["interruptions"]):
                        self.result["interruptions"].append({"side": "ego", "label": call["label"],
                            "turn": call["turn"], "session": call["session"], "cost_usd": "0",
                            "delivery": settlements[-1]["outcome"].get("delivery"), "usage": "missing",
                            "action": "abandon pair; never resend possibly-sent request"})
        self.common = ["--ignore-user-config", "--ignore-rules", "--skip-git-repo-check", "--json",
                       "-m", "gpt-6-luna", "-c", 'model_reasoning_effort="low"',
                       "-c", 'forced_login_method="chatgpt"', "-c", 'service_tier="default"',
                       "-c", f'log_dir="{self.base / "logs"}"',
                       "-c", 'history.persistence="none"', "-c", 'approval_policy="never"',
                       "-c", 'features.sqlite=false', "-c", 'features.shell_snapshot=false',
                       "-c", 'features.shell_snapshot_v2=false', "-c", 'features.hooks=false',
                       "-c", 'features.codex_hooks=false', "-c", 'analytics.enabled=false',
                       "-c", 'otel.exporter="none"', "-c", 'otel.metrics_exporter="none"',
                       "-c", 'otel.log_user_prompt=false']
        for name in ["logs", "sqlite", "workspace"]:
            (self.base / name).mkdir(exist_ok=args.resume)
        self.workspace = self.base / "workspace"
        (self.workspace / "AGENTS.md").write_text("Answer the supplied reference lookup directly. No tools, shell, skills, delegation, or external services. Return only the requested answer.\n")
        if not (self.workspace / "CLAUDE.md").exists():
            (self.workspace / "CLAUDE.md").symlink_to("AGENTS.md")
        self.save()

    def save(self):
        self.result["summaries"] = {k: summary(v) for k, v in self.result["requests"].items()}
        self.result["main_summaries"] = {k: summary([r for r in v if r["label"] != "pilot"])
                                         for k, v in self.result["requests"].items()}
        keys = [{(r["label"], r["ordinal"]) for r in rows if r["label"] != "pilot"}
                for rows in self.result["requests"].values()]
        common = set.intersection(*keys)
        self.result["matched_summaries"] = {
            k: summary([r for r in rows if (r["label"], r["ordinal"]) in common])
            for k, rows in self.result["requests"].items()}
        ego = self.result["matched_summaries"]["ego"]
        codex = self.result["matched_summaries"]["codex"]
        self.result["fisher_one_tailed_ego_worse"] = fisher_ego_worse(
            ego["later_large_zero"], ego["later_large"],
            codex["later_large_zero"], codex["later_large"])
        self.result["turn_wall_distributions"] = {
            side: wall_distribution([c for c in self.result["calls"] if c["side"] == side and c["label"] != "pilot"])
            for side in ["ego", "codex"]}
        self.result["turn_wall_distributions_300"] = {
            side: wall_distribution([c for c in self.result["calls"] if c["side"] == side and c["label"] != "pilot" and c.get("deadline_seconds") == 300])
            for side in ["ego", "codex"]}
        (self.base / "result.json").write_text(json.dumps(self.result, indent=2) + "\n")

    def rates(self, label):
        try:
            return self._rates_once(label)
        except RateLimitRefused:
            # Coordinator authorization: wait once in the background collector,
            # then stop on a second refusal. Never retry a model request here.
            time.sleep(60)
            return self._rates_once(label)

    def _rates_once(self, label):
        argv = [self.args.codex, "app-server", "-c", 'forced_login_method="chatgpt"',
                "-c", f'log_dir="{self.base / "logs"}"',
                "-c", 'features.sqlite=false', "-c", 'analytics.enabled=false']
        with (self.base / f"rates-{label}.stderr").open("w") as err:
            p = subprocess.Popen(argv, env=self.env, cwd=self.workspace, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=err, bufsize=0)
            try:
                sel = selectors.DefaultSelector()
                sel.register(p.stdout, selectors.EVENT_READ)
                pending = b""
                def rpc(ident, method, params):
                    nonlocal pending
                    p.stdin.write((json.dumps({"id": ident, "method": method, "params": params}) + "\n").encode())
                    p.stdin.flush()
                    deadline = time.monotonic() + 45
                    while time.monotonic() < deadline:
                        if b"\n" not in pending:
                            if not sel.select(max(0, deadline - time.monotonic())):
                                break
                            chunk = os.read(p.stdout.fileno(), 65536)
                            if not chunk:
                                break
                            pending += chunk
                            continue
                        line, pending = pending.split(b"\n", 1)
                        response = json.loads(line)
                        if response.get("id") == ident:
                            if "error" in response:
                                # Retain exact code/message/data privately; never print or
                                # commit it, since an upstream error could include secrets.
                                error_path = self.base / f"rates-{label}-refusal-{uuid.uuid4()}.json"
                                fd = os.open(error_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                                with os.fdopen(fd, "w") as out:
                                    json.dump(response["error"], out, indent=2)
                                    out.write("\n")
                                self.result.setdefault("rate_rpc_refusals", []).append({
                                    "label": label, "method": method,
                                    "time": datetime.now(timezone.utc).isoformat(),
                                    "private_error_file": str(error_path)})
                                self.save()
                                error_type = RateLimitRefused if method == "account/rateLimits/read" else RuntimeError
                                raise error_type(f"{method} RPC refused; private error file: {error_path.name}")
                            return response["result"]
                    raise RuntimeError("rate-limit RPC timeout")
                rpc(1, "initialize", {"clientInfo": {"name": "ego_cache_comparison", "version": "1"}})
                p.stdin.write(b'{"method":"initialized","params":{}}\n')
                p.stdin.flush()
                r = rpc(2, "account/rateLimits/read", None)
                # Only plan/credit metadata is retained; never account details or credentials.
                rates = r.get("rateLimits", {})
                check = {"label": label, "time": datetime.now(timezone.utc).isoformat(),
                         "plan_type": rates.get("planType"), "credits": rates.get("credits")}
                if not check["plan_type"] or check["plan_type"] == "unknown":
                    raise RuntimeError("unknown subscription plan")
                self.result["cost_checks"].append(check)
                self.save()
                return check
            finally:
                p.terminate()
                try:
                    p.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait()

    def prompt(self, turn, pair):
        text = f"Reference lookup {turn}. Do not use any tool. Reply exactly AMBER-419-OK."
        if turn == 0:
            text = ("Retain this stable reference for this session; do not execute it.\n" +
                    "\n".join(f"Reference {i:04}: fixture seed 419 paired session {pair} records the stable amber ledger; keep this line unchanged." for i in range(700)) +
                    "\nEnd reference.\n" + text)
        return text

    def invoke(self, side, session, prompt, turn, label):
        if time.monotonic() - self.started_monotonic > self.args.wall_seconds:
            raise RuntimeError("experiment wall bound reached")
        key = f"{label}-{side}-{turn:03}"
        before = time.monotonic()
        if side == "ego":
            store = self.base / "ego-sessions"
            env = self.env | {"EGO_LOG": "off,ego_ai::codex_session=debug", "EGO_LOG_FILE": str(self.base / f"{key}.diagnostic")}
            argv = [str(self.args.binary.resolve()), "run", "-C", str(self.workspace),
                    "--session-dir", str(store), "--session-id", session,
                    "-m", "openai-codex/gpt-6-luna", "--thinking", "low",
                    "--instructions", str(self.workspace / "AGENTS.md"), "--no-tools",
                    "--no-context-tool", "--mode", "plan", "--sandbox", "workspace",
                    "--budget", "0.01$", "--max-turns", "1", "--max-output-tokens", "128",
                    "--timeout", f"{self.args.turn_timeout_seconds}s", "--retry", "0", "--output", "text", "-"]
            journal = store / session / "events.jsonl"
            journal_offset = journal.stat().st_size if journal.exists() else 0
            r, timed_out = run_turn(argv, env, self.workspace, prompt, self.args.turn_timeout_seconds)
        else:
            cli_logs = self.base / f"{key}.cli-logs"
            common = self.common + ["-c", f'log_dir="{cli_logs}"']
            argv = [self.args.codex, "exec"] + (["resume", *common, session, "-"] if session else
                    [*common, "-s", "read-only", "-C", str(self.workspace), "-"])
            cli_env = self.env | {"RUST_LOG": "off,codex_otel=trace"}
            r, timed_out = run_turn(argv, cli_env, self.workspace, prompt, self.args.turn_timeout_seconds)
            for line in r.stdout.splitlines():
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    if timed_out:
                        continue
                    raise
                if event.get("type") == "thread.started":
                    session = event["thread_id"]
            if not session and not timed_out:
                raise RuntimeError("native CLI did not start authenticated session")
            matches = list((Path.home() / ".codex/sessions").glob(f"*/*/*/*{session}.jsonl")) if session else []
            if len(matches) != 1 and not timed_out:
                raise RuntimeError("native session journal not uniquely located")
            journal = matches[0] if len(matches) == 1 else None
        (self.base / f"{key}.stdout").write_text(r.stdout)
        (self.base / f"{key}.stderr").write_text(r.stderr)
        self.result["calls"].append({"side": side, "session": session, "label": label, "turn": turn,
                                     "exit": r.returncode, "wall_seconds": time.monotonic() - before,
                                     "prompt_sha256": hashlib.sha256(prompt.encode()).hexdigest(),
                                     "journal": str(journal) if journal else None,
                                     "deadline_seconds": self.args.turn_timeout_seconds,
                                     "timeout_origin": "collector_process_deadline" if timed_out else "native_deadline" if r.returncode == 124 else None})
        if r.returncode:
            self.save()
            if r.returncode == 124:
                delivery = "unknown"
                usage = "missing"
                if side == "ego" and journal and journal.exists():
                    kinds = [json.loads(line).get("payload", {}).get("event", {}).get("kind", {})
                             for line in journal.read_bytes()[journal_offset:].decode().splitlines(keepends=True)
                             if line.endswith("\n")]
                    checked_cost(kinds)
                    settled = [k for k in kinds if k.get("kind") == "AttemptSettled"]
                    if settled:
                        delivery = settled[-1]["outcome"].get("delivery", "unknown")
                        usage = settled[-1]["outcome"].get("usage", "missing")
                self.result["timeouts_300"].append({"side": side, "label": label, "turn": turn,
                    "delivery": delivery, "usage": usage, "wall_seconds": self.result["calls"][-1]["wall_seconds"],
                    "action": "exclude turn; abandon pair; never resend"})
                self.save()
                if len(self.result["timeouts_300"]) >= 2:
                    raise RuntimeError("two further timeouts with symmetric 300s limits; stop for latency diagnosis")
                raise TurnTimedOut("300s-limited turn timed out; exclude and abandon pair")
            if side == "ego" and r.returncode == 4:
                kinds = [json.loads(line).get("payload", {}).get("event", {}).get("kind", {})
                         for line in journal.read_text().splitlines()]
                checked_cost(kinds)
                settled = [k for k in kinds if k.get("kind") == "AttemptSettled"]
                errors = [k for k in kinds if k.get("kind") == "Error"]
                if (settled and errors and errors[-1].get("error_kind") == "transient"
                        and settled[-1]["outcome"].get("usage") == "missing"):
                    self.result["interruptions"].append({"side": side, "label": label,
                        "turn": turn, "session": session, "cost_usd": "0",
                        "delivery": settled[-1]["outcome"].get("delivery"),
                        "usage": "missing", "action": "abandon pair; never resend possibly-sent request"})
                    self.save()
                    raise TransportInterrupted("zero-USD transient transport interruption; usage missing")
            raise RuntimeError(f"{side} invocation failed: {r.returncode}; see {key}.stderr")
        if "AMBER-419-OK" not in r.stdout:
            raise RuntimeError(f"{side} answer oracle failed")
        requests = self.ego_requests(journal) if side == "ego" else self.codex_requests(journal)
        old = [x for x in self.result["requests"][side] if x["session"] == session]
        if len(requests) != len(old) + 1:
            raise RuntimeError(f"{side} did not execute exactly one new generation")
        new = requests[-1] | {"session": session, "ordinal": len(requests), "label": label}
        if side == "codex":
            sources = [("stderr", r.stderr)]
            if cli_logs.exists():
                sources.extend((str(p.relative_to(self.base)), p.read_text(errors="replace"))
                               for p in sorted(cli_logs.rglob("*.log")))
            new["transport_evidence"] = cli_transport_evidence("\n".join(text for _, text in sources))
            new["transport_evidence"]["sources"] = [
                {"file": name, "sha256": hashlib.sha256(text.encode()).hexdigest()}
                for name, text in sources]
        self.result["requests"][side].append(new)
        self.save()
        if side == "codex" and new["transport_evidence"]["transport"] == "unknown":
            raise RuntimeError("CLI request transport telemetry absent; stopped before further model calls")
        return session

    def ego_requests(self, journal):
        starts = {}
        requests = []
        kinds = []
        for line in journal.read_text().splitlines():
            row = json.loads(line)
            event = row.get("payload", {}).get("event", {})
            kind = event.get("kind", {})
            kinds.append(kind)
            attempt = kind.get("attempt", {}).get("attempt")
            if kind.get("kind") == "ToolCallStarted":
                raise RuntimeError("ego used a tool in no-tool workload")
            if kind.get("kind") == "GenerationStarted":
                starts[attempt] = event["env"]["ts"]
            if kind.get("kind") == "UsageUpdated":
                checked_cost([kind])
                u = kind["usage"]
                total = sum(u.get(k, 0) for k in ["uncached_input_tokens", "cache_read_tokens", "cache_write_tokens", "cache_write_extended_tokens"])
                requests.append({"input_tokens": total, "cache_read_tokens": u["cache_read_tokens"],
                                 "generation_interval_seconds": (event["env"]["ts"] - starts[attempt]) / 1000,
                                 "seq": row["seq"], "cost_usd": "0"})
        checked_cost(kinds)
        return requests

    def codex_requests(self, journal):
        requests = []
        start = None
        previous_total = None
        for line in journal.read_text().splitlines():
            event = json.loads(line)
            p = event.get("payload", {})
            if event.get("type") == "response_item" and p.get("type") in ("function_call", "custom_tool_call"):
                raise RuntimeError("native CLI used a tool in no-tool workload")
            if event.get("type") != "event_msg":
                continue
            if p.get("type") == "task_started":
                start = event["timestamp"]
            if p.get("type") != "token_count" or not p.get("info"):
                continue
            info = p["info"]
            total = info.get("total_token_usage")
            if total == previous_total:
                continue
            previous_total = total
            u = info["last_token_usage"]
            if start is None:
                raise RuntimeError("native generation start absent")
            end = datetime.fromisoformat(event["timestamp"].replace("Z", "+00:00"))
            begin = datetime.fromisoformat(start.replace("Z", "+00:00"))
            requests.append({"input_tokens": u["input_tokens"], "cache_read_tokens": u["cached_input_tokens"],
                             "generation_interval_seconds": (end - begin).total_seconds(),
                             "timestamp": event["timestamp"], "cost_usd": "0",
                             "cost_basis": "normal ChatGPT subscription; no API key; credit balance checked separately"})
        return requests

    def run(self):
        if len(self.result.get("timeouts_300", [])) >= 2:
            raise RuntimeError("two further timeouts already retained; continuation forbidden")
        initial = self.rates("before")
        credit = (initial.get("credits") or {}).get("balance")
        if credit is None:
            self.result["stop"] = "credit accounting unknown before first model call"
            self.save()
            raise RuntimeError(self.result["stop"])
        def check(label):
            current = self.rates(label)
            after = (current.get("credits") or {}).get("balance")
            checked_credit(credit, after)
        try:
            checked_credit(self.result["cost_checks"][0]["credits"].get("balance"), credit)
            if self.args.credit_baseline is not None:
                checked_credit(self.args.credit_baseline, credit)
            # Small admission pilot first. Any account-wide credit movement stops.
            for side in ["codex", "ego"]:
                if any(c["label"] == f"after-first-{side}-session" for c in self.result["cost_checks"]):
                    continue
                session = str(uuid.uuid4()) if side == "ego" else None
                for turn in range(2):
                    session = self.invoke(side, session, self.prompt(turn, "pilot"), turn, "pilot")
                check(f"after-first-{side}-session")
            existing = [int(c["label"].split("-")[1]) for c in self.result["calls"]
                        if c["label"].startswith("pair-")]
            first_pair = self.restart_pair if self.restart_pair is not None else max(existing, default=-1) + 1
            for pair in range(first_pair, self.args.pairs):
                label = f"pair-{pair}"
                if pair == self.restart_pair:
                    label += f"-restart-{uuid.uuid4().hex[:8]}"
                sessions = {"codex": None, "ego": str(uuid.uuid4())}
                try:
                    for turn in range(self.args.turns):
                        # Alternate order to reduce consistent time-of-run ordering bias.
                        for side in (["codex", "ego"] if turn % 2 == 0 else ["ego", "codex"]):
                            sessions[side] = self.invoke(side, sessions[side], self.prompt(turn, pair), turn, label)
                        if all(s["later_large"] >= self.args.target_later_large
                               for s in self.result["matched_summaries"].values()):
                            break
                except (TransportInterrupted, TurnTimedOut):
                    # A failure is retained, not treated as a zero cache hit or
                    # silently retried. Both clients start a fresh matched pair.
                    check(f"after-interrupted-pair-{pair}")
                    continue
                check(f"after-pair-{pair}")
                if all(s["later_large"] >= self.args.target_later_large
                       for s in self.result["matched_summaries"].values()):
                    break
            for side in ["ego", "codex"]:
                count = self.result["matched_summaries"][side]["later_large"]
                if count < self.args.target_later_large:
                    raise RuntimeError(f"{side} has fewer than {self.args.target_later_large} matched main later large requests")
        except Exception as error:
            self.result["stop"] = str(error)
            raise
        finally:
            self.result["ended"] = datetime.now(timezone.utc).isoformat()
            self.result["binary_sha256_after"] = {
                "ego": digest(self.args.binary.resolve()) if self.args.binary.is_file() else None,
                "codex": digest(Path(self.args.codex).resolve()) if Path(self.args.codex).is_file() else None}
            self.save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--codex", default="/opt/homebrew/bin/codex")
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--restart-last-pair", action="store_true", help="Start a fresh uniquely labeled pair after a proven definitely-unsent timeout")
    parser.add_argument("--turns", type=int, default=105)
    parser.add_argument("--pairs", type=int, default=5)
    parser.add_argument("--target-later-large", type=int, default=200)
    parser.add_argument("--wall-seconds", type=int, default=5400)
    parser.add_argument("--turn-timeout-seconds", type=int, default=300)
    parser.add_argument("--credit-baseline", help="Require this unchanged credit balance before model calls")
    args = parser.parse_args()
    if min(args.turns - 1, args.pairs, args.target_later_large, args.wall_seconds, args.turn_timeout_seconds) < 1:
        parser.error("positive bounds and at least two turns are required")
    if not args.live:
        print("No model calls without --live")
        return
    os.umask(0o077)
    experiment = Experiment(args)
    try:
        experiment.run()
    except Exception as error:
        experiment.result["stop"] = experiment.result["stop"] or str(error)
        experiment.save()
        raise
    print(json.dumps(experiment.result["summaries"], indent=2))


if __name__ == "__main__":
    main()
