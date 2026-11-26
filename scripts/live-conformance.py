#!/usr/bin/env python3
"""Opt-in real-model conformance measurements. No paid requests without --live."""
import argparse
from datetime import datetime, timezone
from decimal import Decimal
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import threading
import uuid

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MODELS = [
    "openai-codex/gpt-6-luna", "openai/gpt-6-luna",
    "anthropic/claude-sonnet-5-5", "openrouter/anthropic/claude-sonnet-5.5",
    "openrouter/openai/gpt-6-luna",
]
SUITES = ["cache", "skills", "compaction", "recovery"]
KEYS = ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY")
TOTAL_FIELDS = ("uncached_input_tokens", "cache_read_tokens", "cache_write_tokens",
                "cache_write_extended_tokens")


class Stop(Exception):
    """A safe, non-secret reason to stop paid work."""


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def events(store):
    """Journal coordinates, not attempt IDs, identify each individual event."""
    result = {}
    for path in sorted(store.glob("*/events.jsonl")):
        snapshot = None
        for line in path.read_text().splitlines():
            row = json.loads(line)
            payload = row.get("payload", {})
            event = payload.get("event")
            if event:
                kind = event["kind"]
                if kind.get("kind") in ("CompactionSeeded", "Compacted", "Forked"):
                    snapshot = kind.get("snapshot")
                elif kind.get("kind") == "GenerationStarted" and kind.get("snapshot") is not None:
                    snapshot = kind["snapshot"]
                if kind.get("kind") == "GenerationStarted":
                    # Cold compaction uses the durable source prompt, not the
                    # newly assembled target RunContext. Omitted snapshots keep
                    # the previous value, but never one from another journal.
                    system = (snapshot or {}).get("system") or {}
                    kind = dict(kind, effective_instructions=system.get("sources", []))
                result[(path.parent.name, row["seq"])] = kind
            evidence = payload.get("evidence", {})
            if evidence.get("kind") == "generation_admission":
                result[(path.parent.name, row["seq"])] = {
                    "kind": "GenerationAdmissionEvidence", "attempt": evidence["attempt"],
                    "target": evidence["target"], "invocation": evidence.get("invocation")}
            elif evidence.get("kind") == "run_context":
                result[(path.parent.name, row["seq"])] = {
                    "kind": "RunContextEvidence", "invocation": evidence["invocation"],
                    "instructions": evidence.get("instructions", [])}
    return result


def padding_admission(rows, expected, compaction=False):
    """Verify exact padding in fresh admissions or the compaction source snapshot."""
    contexts = {row["invocation"]: row["instructions"] for row in rows
                if row.get("kind") == "RunContextEvidence"}
    generations = [row for row in rows if row.get("kind") == "GenerationAdmissionEvidence"]
    starts = {row.get("attempt", {}).get("attempt"): row for row in rows
              if row.get("kind") == "GenerationStarted" and row.get("role") == "compaction"}
    verified = set()
    admitted_generations = []
    for generation in generations:
        invocation = generation.get("invocation")
        instructions = contexts.get(invocation, [])
        if compaction:
            start = starts.get(generation.get("attempt", {}).get("attempt"), {})
            instructions = start.get("effective_instructions", [])
        admitted = {entry["path"]: entry for entry in instructions}
        valid = invocation in contexts and all(
            all(admitted.get(path, {}).get(key) == descriptor[key] for key in ("sha256", "bytes"))
            for path, descriptor in expected.items())
        admitted_generations.append(valid)
        if valid:
            verified.add(invocation)
    return {"admitted": bool(generations) and all(admitted_generations),
            "expected_files": len(expected),
            "expected_bytes": sum(entry["bytes"] for entry in expected.values()),
            "verified_invocations": len(verified)}


def usage_metrics(rows):
    roles = {}
    requests = []
    cost = Decimal(0)
    unknown = False
    first_in_turn = True
    for row in rows:
        if row.get("kind") == "UserPrompt":
            first_in_turn = True
        attempt = row.get("attempt", {}).get("attempt")
        if row.get("kind") == "GenerationStarted":
            roles[attempt] = row.get("role", "Main")
        if row.get("kind") != "UsageUpdated":
            continue
        usage = row["usage"]
        total = sum(usage.get(k, 0) for k in TOTAL_FIELDS)
        read = usage.get("cache_read_tokens", 0)
        requests.append({"input_tokens": total, "cache_read_tokens": read,
                         "cache_read_share": read / total if total else None,
                         "role": roles.get(attempt, "unknown"),
                         "turn_position": "first" if first_in_turn else "later"})
        first_in_turn = False
        delta = row.get("cost_delta") or {}
        if delta.get("cost") != "known" or delta.get("amount", {}).get("currency") != "USD":
            unknown = True
        else:
            cost += Decimal(delta["amount"]["amount"])
    split = {}
    for position in ("first", "later"):
        group = [r for r in requests if r["turn_position"] == position]
        denominator = sum(r["input_tokens"] for r in group)
        split[position] = {"requests": len(group), "zero_hits": sum(r["cache_read_tokens"] == 0 for r in group),
                           "weighted_share": sum(r["cache_read_tokens"] for r in group) / denominator if denominator else None}
    later = requests[1:]
    total = sum(r["input_tokens"] for r in later)
    return {"turn_split": split, "requests": requests, "cost_usd": str(cost), "unknown_cost": unknown,
            "weighted_later_share": (sum(r["cache_read_tokens"] for r in later) / total
                                     if total else None),
            "later_zero_hits": sum(r["cache_read_tokens"] == 0 for r in later),
            "read_calls": sum(r.get("kind") == "ToolCallStarted" and r.get("name") == "read"
                              for r in rows)}



def skill_model_executed(rows, expected):
    """Require used generation-target evidence after admission of the override."""
    admitted = False
    targets = set()
    started = set()
    for row in rows:
        kind = row.get("kind")
        attempt = row.get("attempt", {}).get("attempt")
        if kind == "SkillInstructionAdmitted":
            admitted = row.get("model") == expected
        elif admitted and kind == "GenerationAdmissionEvidence":
            if row.get("target", {}).get("canonical_model") == expected:
                targets.add(attempt)
        elif kind == "GenerationStarted" and attempt in targets:
            started.add(attempt)
        elif kind == "UsageUpdated" and attempt in started:
            return True
    return False


def skill_result_matches(rows, case):
    """Check durable tool bytes, never the model's rendering of those bytes."""
    instructions = {
        "description": "Reply exactly AMBER-418-OK.\n",
        "model": "Reply exactly COBALT-418-OK.\n",
        "tools": "Read sample.txt using the read tool. Return its exact content. Do not invoke any other tool.\n",
        "arguments": "Reply with exactly these rendered fields: raw=[alpha beta]; zero=[alpha]; one=[beta]; first=[alpha]; missing=[$2]\n",
    }
    active_tool = None
    admitted = False
    matched_skill = False
    matched_read = False
    for row in rows:
        kind = row.get("kind")
        if kind == "ToolCallStarted":
            active_tool = row.get("name")
            admitted = False
        elif kind == "SkillInstructionAdmitted" and active_tool == "skill":
            admitted = True
        elif kind == "ToolCallCompleted":
            text = "".join(part.get("text", "") for part in row.get("result", {}).get("content", []))
            if row.get("ok") is True:
                if active_tool == "skill" and admitted and text == instructions[case]:
                    matched_skill = True
                elif active_tool == "read" and matched_skill and text == "IVORY-418-READ\n":
                    matched_read = True
            active_tool = None
            admitted = False
    return matched_skill and (case != "tools" or matched_read)


def compaction_model_executed(rows, expected):
    """A checkpoint alone does not prove which model generated the summary."""
    targets = set()
    started = set()
    for row in rows:
        kind = row.get("kind")
        attempt = row.get("attempt", {}).get("attempt")
        if kind == "GenerationAdmissionEvidence" and row.get("target", {}).get("canonical_model") == expected:
            targets.add(attempt)
        elif kind == "GenerationStarted" and attempt in targets and row.get("role") == "compaction":
            started.add(attempt)
        elif kind == "UsageUpdated" and attempt in started:
            return True
    return False


def child_env(provider, main, router):
    """Parse only the authorized name; never execute a dotenv file."""
    env = {k: v for k, v in os.environ.items()
           if k not in KEYS and k not in ("CLAUDE_CODE_OAUTH_TOKEN", "EGO_LOG", "EGO_LOG_FILE",
                                          "OPENAI_BASE_URL", "ANTHROPIC_BASE_URL", "EGO_MODEL")}
    key = {"openai": KEYS[0], "anthropic": KEYS[1], "openrouter": KEYS[2]}.get(provider)
    if key:
        source = router if provider == "openrouter" else main
        if not source.is_file():
            raise Stop("credential_file_missing")
        value = None
        for line in source.read_text().splitlines():
            name, separator, tail = line.removeprefix("export ").partition("=")
            if separator and name.strip() == key:
                value = tail.strip()
                if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                    value = value[1:-1]
        if not value:
            raise Stop("credential_name_missing")
        env[key] = value
    env.update(NO_COLOR="1", EGO_LOG="off")
    return env


class FaultProxy:
    """Forward real SSE without logging headers, keys, bodies, or upstream errors."""
    def __init__(self, provider, fault):
        self.origin = {"openai": "api.openai.com", "anthropic": "api.anthropic.com"}[provider]
        self.fault = fault
        self.requests = 0
        self.injected = False
        self.text_seen = False
        self.upstream_statuses = []
        self.transport_errors = []
        self.server = None

    def start(self):
        import ssl
        try:
            import truststore
        except ImportError as error:
            raise Stop("recovery_requires_python_3_10_and_truststore: install scripts/requirements-live-conformance.txt") from error
        context = truststore.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_POST(self):
                owner.requests += 1
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                if owner.requests == 1 and owner.fault == "before":
                    owner.injected = True
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    return
                connection = http.client.HTTPSConnection(owner.origin, timeout=35, context=context)
                try:
                    headers = {k: v for k, v in self.headers.items()
                               if k.lower() not in ("host", "connection", "accept-encoding")}
                    connection.request("POST", self.path, body, headers)
                    response = connection.getresponse()
                    owner.upstream_statuses.append(response.status)
                    self.send_response(response.status)
                    self.send_header("Content-Type", response.getheader("Content-Type", "text/event-stream"))
                    # A deliberately incomplete chunked body is a transport error, not clean EOF.
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    cut_after_frame = False
                    while line := response.readline():
                        self.wfile.write(f"{len(line):x}\r\n".encode() + line + b"\r\n")
                        self.wfile.flush()
                        text = b'"response.output_text.delta"' in line or b'"text_delta"' in line
                        if text:
                            owner.text_seen = True
                        if text and owner.requests == 1 and owner.fault == "after":
                            cut_after_frame = True
                        if cut_after_frame and not line.strip():
                            owner.injected = True
                            self.connection.shutdown(socket.SHUT_RDWR)
                            self.connection.close()
                            return
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                except (OSError, http.client.HTTPException) as error:
                    owner.transport_errors.append(type(error).__name__)
                    self.close_connection = True
                finally:
                    connection.close()

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return f"http://127.0.0.1:{self.server.server_port}" + ("/v1/" if self.origin == "api.openai.com" else "/")

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


class Runner:
    def __init__(self, args):
        self.args = args
        self.run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8]
        self.work = args.work / self.run_id
        self.work.mkdir(parents=True, mode=0o700)
        self.store = self.work / "sessions"
        self.spent = Decimal(0)
        self.by_provider = {}
        self.uncertain = False
        self.unaccounted = False
        self.carried = Decimal(0)
        self.by_provider_carry = {}
        self.carried_attempts = {}
        self.cases = []
        self.fixture_hashes = {}
        self.padding_fixtures = {}
        self.serial = 0

    def allowance(self, provider):
        if self.unaccounted:
            raise Stop("unaccounted_attempt_stops_paid_work")
        left = min(self.args.ceiling - self.spent - self.carried,
                   self.args.provider_ceiling - self.by_provider.get(provider, Decimal(0))
                   - self.by_provider_carry.get(provider, Decimal(0)))
        if left <= 0:
            raise Stop("cost_ceiling_reached")
        return left

    def model_envelope(self, model):
        """Conservative carry, not actual cost; only captured exact OpenRouter rows."""
        catalogue = getattr(self.args, "openrouter_catalogue", None)
        if not model.startswith("openrouter/") or catalogue is None:
            return None
        rows = json.loads(catalogue.read_text())["data"]
        row = next((row for row in rows if "openrouter/" + row["id"] == model), None)
        if row is None:
            return None
        try:
            context = Decimal(row["context_length"])
            prices = row["pricing"]
            inputs = [Decimal(prices[key]) for key in
                      ("prompt", "input_cache_read", "input_cache_write") if key in prices]
            prompt, output = Decimal(prices["prompt"]), Decimal(prices["completion"])
            if (not context.is_finite() or context <= 0 or not inputs
                    or any(not rate.is_finite() or rate < 0 for rate in inputs + [prompt, output])):
                return None
            return context * max(inputs) + Decimal(2048) * output
        except (KeyError, TypeError, ValueError, ArithmeticError):
            return None

    def account(self, model, rows):
        provider = model.split("/", 1)[0]
        metrics = usage_metrics(rows)
        cost = Decimal(metrics["cost_usd"])
        self.spent += cost
        self.by_provider[provider] = self.by_provider.get(provider, Decimal(0)) + cost
        starts = {r["attempt"]["attempt"] for r in rows if r.get("kind") == "GenerationStarted"}
        metered = {r["attempt"]["attempt"] for r in rows if r.get("kind") == "UsageUpdated"}
        unknown = {r["attempt"]["attempt"] for r in rows
                   if r.get("kind") == "UsageUpdated"
                   and ((r.get("cost_delta") or {}).get("cost") != "known"
                        or (r.get("cost_delta") or {}).get("amount", {}).get("currency") != "USD")}
        unsent = {r["attempt"]["attempt"] for r in rows
                  if r.get("kind") == "AttemptSettled"
                  and r.get("outcome", {}).get("delivery") == "definitely_unsent"}
        unresolved = (unknown | (starts - metered)) - unsent
        targets = {r["attempt"]["attempt"]: r["target"]["canonical_model"] for r in rows
                   if r.get("kind") == "GenerationAdmissionEvidence"}
        self.uncertain |= bool(unresolved) or metrics["unknown_cost"]
        for attempt in unresolved:
            if attempt in self.carried_attempts:
                continue
            target = targets.get(attempt, model)
            envelope = self.model_envelope(target)
            if envelope is None:
                self.unaccounted = True
                continue
            self.carried_attempts[attempt] = {"model": target, "upper_bound_usd": str(envelope)}
            self.carried += envelope
            # All authorized routed targets share the OpenRouter provider cap.
            target_provider = target.split("/", 1)[0]
            self.by_provider_carry[target_provider] = self.by_provider_carry.get(target_provider, Decimal(0)) + envelope
        return metrics

    def invoke(self, model, workspace, prompt=None, session=None, command=None, proxy=None, proxy_evidence=None):
        provider = model.split("/", 1)[0]
        if provider == "anthropic":
            raise Stop("external blocker: direct Anthropic key capped until 2026-11-01; no retry")
        allowance = self.allowance(provider)
        config = workspace / ".ego/config.toml"
        config.parent.mkdir(exist_ok=True)
        config.write_text(f'model = "{model}"\nthinking = "low"\nmax_output_tokens = 2048\nbudget = "{allowance}$"\n')
        env = child_env(provider, self.args.ego_env, self.args.router_env)
        if provider == "openrouter" and getattr(self.args, "operator_home", None):
            env["EGO_HOME"] = str(self.args.operator_home)
        if provider == "openai-codex" and getattr(self.args, "diagnostic_dir", None):
            self.args.diagnostic_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
            env["EGO_LOG"] = "off,ego_ai::codex_session=debug,ego_tools::http::websocket=debug"
            env["EGO_LOG_FILE"] = str(self.args.diagnostic_dir / f"{self.run_id}-{self.serial:03}-{uuid.uuid4().hex[:8]}.log")

        if proxy:
            env[{"openai": "OPENAI_BASE_URL", "anthropic": "ANTHROPIC_BASE_URL"}[provider]] = proxy
        common = ["-C", str(workspace), "--session-dir", str(self.store)]
        if command:
            argv = [str(self.args.binary), *command, *common, "--output", "json"]
            if command[0] == "compact":
                argv += ["--budget", f"{allowance}$", "--max-output-tokens", "2048"]
        else:
            argv = [str(self.args.binary), "run", *common, "-m", model, "--thinking", "low",
                    "--instructions", str(workspace / "AGENTS.md"), "--trust-workspace",
                    "--mode", "plan", "--sandbox", "workspace", "--tools", "read,skill" if (workspace / ".ego/skills").is_dir() else "read",
                    "--budget", f"{allowance}$", "--max-output-tokens", "1024", "--max-turns", "6", "--timeout", "50s",
                    "--retry", "1", "--output", "text", "--session-id", session or str(uuid.uuid4())]
            # fork/compact have their own grammars, not run's config authority.
            for override in getattr(self.args, "run_set", []):
                argv += ["-c", override]
            argv.append(prompt)
        before = events(self.store)
        timed_out = False
        try:
            completed = subprocess.run(argv, capture_output=True, text=True, env=env,
                                       cwd=workspace, timeout=65)
            code, stdout, stderr = completed.returncode, completed.stdout, completed.stderr
        except subprocess.TimeoutExpired as error:
            timed_out = True
            stderr = error.stderr or ""
            if isinstance(stderr, bytes):
                stderr = stderr.decode("utf-8", errors="replace")
            code, stdout = -1, ""
        # Private diagnostics are not part of the append-only public results.
        # Redact the keys already supplied to this child; never inspect a store.
        diagnostic = stderr
        for key in KEYS:
            if env.get(key):
                diagnostic = diagnostic.replace(env[key], "[redacted]")
        stderr_path = workspace / f"stderr-{uuid.uuid4().hex}.txt"
        descriptor = os.open(stderr_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w") as output:
            output.write(diagnostic)
        after = events(self.store)
        rows = [value for key, value in after.items() if key not in before]
        proxy_zero_booked = False
        if (proxy_evidence is not None and proxy_evidence.fault == "before"
                and proxy_evidence.injected):
            # Only the first incoming request is dropped without an upstream call.
            # Keep ego's original journal untouched; annotate the runner projection.
            first_attempt = next((r["attempt"]["attempt"] for r in rows
                                  if r.get("kind") == "GenerationStarted"), None)
            projected = []
            for row in rows:
                if (row.get("kind") == "UsageUpdated"
                        and row.get("attempt", {}).get("attempt") == first_attempt
                        and (row.get("cost_delta") or {}).get("cost") == "unknown"):
                    row = dict(row, cost_delta={"cost": "known",
                        "amount": {"amount": "0", "currency": "USD"},
                        "attribution": {"source": "runner_proxy_not_forwarded",
                                        "revision": "live-conformance/v1"}})
                    proxy_zero_booked = True
                projected.append(row)
            rows = projected
        metrics = self.account(model, rows)
        metrics["proxy_proven_zero_attempt"] = proxy_zero_booked
        reason = "timeout" if timed_out else ("reasoning_capability_unknown" if "nobody stated whether the model accepts" in stderr else ("budget_refused" if "budget denied" in stderr else
                                             ("process_failed" if code else "completed")))
        process_exit = code
        expected = self.padding_fixtures.get(workspace)
        if expected is not None and (not command or command[0] == "compact"):
            metrics["padding_admission"] = padding_admission(
                rows, expected, compaction=bool(command and command[0] == "compact"))
            if not metrics["padding_admission"]["admitted"]:
                # Account real usage first. A failed workload is never free,
                # and exit 0 cannot vouch for admitted instruction content.
                reason = "padding_not_admitted"
                code = code or 1
        return {"exit": code, "process_exit": process_exit, "reason": reason, "metrics": metrics, "rows": rows,
                "stdout": stdout, "stderr_path": str(stderr_path),
                "new_sessions": sorted({key[0] for key in after if key not in before})}

    def workspace(self, label, padding=False):
        self.serial += 1
        workspace = self.work / f"{self.serial:03}-{label}"
        workspace.mkdir()
        text = "Use only offered tools and skills. Never access credentials or external services. Keep answers brief.\n"
        padding_paths = []
        if padding:
            # Ego admits at most 64 KiB/file, 256 KiB total, and eight files.
            # Four 600-line imports preserve all 2400 stable references.
            for chunk in range(4):
                path = workspace / f"reference-{chunk}.md"
                path.write_text("\n".join(
                    f"Reference {i:04}: fixture seed 419 records the stable amber ledger; retain this reference unchanged."
                    for i in range(chunk * 600, (chunk + 1) * 600)) + "\n")
                padding_paths.append(path)
                text += f"\n@{path.name}\n"
        (workspace / "AGENTS.md").write_text(text)
        (workspace / "CLAUDE.md").symlink_to("AGENTS.md")
        if padding:
            paths = [workspace / "AGENTS.md", *padding_paths]
            self.padding_fixtures[workspace] = {
                str(path.resolve()): {"sha256": digest(path), "bytes": path.stat().st_size}
                for path in paths}
            for path in paths:
                self.fixture_hashes[f"419/padding/{path.name}"] = digest(path)
        for index in range(3):
            (workspace / f"part-{index}.txt").write_text(f"LEDGER-419-{index}\n")
        return workspace

    def record(self, model, suite, case, runs, passed, extra=None, baseline=False):
        rows = [row for run in runs for row in run["rows"]]
        metrics = usage_metrics(rows)
        metrics["proxy_proven_zero_attempt"] = any(run["metrics"].get("proxy_proven_zero_attempt", False) for run in runs)
        reasons = [run["reason"] for run in runs]
        expected_partial_failure = suite == "recovery" and case == "after" and passed
        padding_checks = [run["metrics"]["padding_admission"] for run in runs
                          if "padding_admission" in run["metrics"]]
        if padding_checks:
            metrics["padding_admission"] = padding_checks
        infrastructure = ((any(run["exit"] for run in runs) and not expected_partial_failure)
                          or any(not check["admitted"] for check in padding_checks)
                          or "padding_not_admitted" in reasons)
        # Only measured feature misses get baseline treatment, not absent usage.
        status = "pass" if passed and not infrastructure else "fail"
        if baseline and not getattr(self.args, "strict", True) and status == "fail" and not infrastructure and metrics["requests"]:
            status = "baseline"
        if "budget_refused" in reasons:
            status = "blocked"
            reasons.append("external blocker: OpenRouter Claude price row unavailable" if model.startswith("openrouter/anthropic/") else "pricing or reservation admission refused")
        self.cases.append({"model": model, "provider": model.split("/", 1)[0],
                           "suite": suite, "case": case, "status": status,
                           "reasons": reasons, "metrics": metrics, **(extra or {})})
        print(f"{suite}/{case} {model}: {status}", flush=True)

    def cache(self, model):
        for repeat in range(self.args.cache_repeats):
            self.cache_once(model, repeat)

    def cache_once(self, model, repeat):
        workspace = self.workspace("cache", padding=True)
        session = str(uuid.uuid4())
        runs = []
        for index in range(3):
            prompt = ("Read part-0.txt, part-1.txt, part-2.txt in three sequential read calls. "
                      f"Then reply LEDGER-419-OK. This is fixed turn {index}.")
            run = self.invoke(model, workspace, prompt, session=session)
            runs.append(run)
            if run["exit"]:
                break
        metrics = usage_metrics([row for run in runs for row in run["rows"]])
        share = metrics["weighted_later_share"]
        catalogue = getattr(self.args, "openrouter_catalogue", None)
        no_discount = False
        if model.startswith("openrouter/") and catalogue is not None:
            rows = json.loads(catalogue.read_text())["data"]
            entry = next((row for row in rows if "openrouter/" + row["id"] == model), None)
            no_discount = entry is not None and "input_cache_read" not in entry["pricing"]
        floor_passed = share is not None and share >= .80
        invalid_padding = any(run["reason"] == "padding_not_admitted" for run in runs)
        passed = not invalid_padding and len(runs) == 3 and metrics["read_calls"] >= 9 and (no_discount or floor_passed)
        self.record(model, "cache", f"multi-turn-tool-resume-{repeat + 1}", runs, passed,
                    {"threshold": None if no_discount else .85, "tolerance": .05,
                     "cache_floor_status": "invalid_workload" if invalid_padding else ("not_applicable" if no_discount else ("pass" if floor_passed else "fail")),
                     "cache_floor_reason": "padding_not_admitted" if invalid_padding else ("no cache price in catalogue" if no_discount else None),
                     "blocked_by": "padding admission" if invalid_padding else ("414 cache fix" if not passed else None)}, baseline=True)
        if not runs or runs[-1]["exit"]:
            return
        fork = self.invoke(model, workspace, command=["fork", session])
        try:
            target = json.loads(fork["stdout"])["result"]
            if not isinstance(target, str):
                raise ValueError()
        except (ValueError, KeyError, TypeError):
            self.record(model, "cache", f"fork-{repeat + 1}", [fork], False)
            return
        continuation = self.invoke(model, workspace, "Reply LEDGER-419-OK without tools.", session=target)
        self.record(model, "cache", f"fork-{repeat + 1}", [fork, continuation],
                    "LEDGER-419-OK" in continuation["stdout"], baseline=True)

    def skills(self, model):
        self.skill_fixtures(model)

    def skill_fixtures(self, model):
        fixture_root = self.args.skills
        manifest = fixture_root / "manifest.json"
        if not manifest.is_file():
            raise Stop("skills_418_manifest_missing")
        selected = {row["case"]: row for row in json.loads(manifest.read_text())}
        for case in ("description", "model", "tools", "arguments"):
            source = fixture_root / case
            workspace = self.workspace("skill-" + case)
            shutil.copytree(source / ".ego", workspace / ".ego", dirs_exist_ok=True)
            if (source / "sample.txt").is_file():
                shutil.copyfile(source / "sample.txt", workspace / "sample.txt")
            for path in sorted((workspace / ".ego/skills").rglob("*")):
                if path.is_file():
                    self.fixture_hashes[f"418/{case}/{path.relative_to(workspace)}"] = digest(path)
            run = self.invoke(model, workspace, selected[case]["prompt"])
            rows = run["rows"]
            invoked = any(row.get("kind") == "SkillInstructionAdmitted" for row in rows)
            tool_names = [row["name"] for row in rows if row.get("kind") == "ToolCallStarted"]
            passed = skill_result_matches(rows, case)
            if case == "tools":
                passed &= "read" in tool_names and set(tool_names) <= {"read", "skill"}
            override_attributed = None
            if case == "model":
                override_attributed = skill_model_executed(rows, "openai-codex/gpt-6-luna")
                passed &= override_attributed
            self.record(model, "skills", case, [run], passed,
                        {"invoked": invoked, "tool_names": tool_names,
                         "model_override_attributed": override_attributed,
                         "assistant_reply_diagnostic": run["stdout"]})

    def compaction(self, model):
        workspace = self.workspace("compaction", padding=True)
        session = str(uuid.uuid4())
        seed = self.invoke(model, workspace,
                           "Remember the chosen report filename ledger-419.txt. Our next task is to return that filename. Reply READY only.", session=session)
        if seed["exit"]:
            self.record(model, "compaction", "checkpoint-continuation", [seed], False)
            return
        compact = self.invoke(model, workspace, command=["compact", session])
        runs = [seed, compact]
        if compact["exit"]:
            self.record(model, "compaction", "checkpoint-continuation", runs, False)
            return
        checkpoint = next((row for row in compact["rows"] if row.get("kind") == "CompactionSeeded"), {})
        text = checkpoint.get("checkpoint", "")
        valid = all(heading in text for heading in ("# Objective", "# Decisions and rationale", "# Ordered next steps"))
        try:
            target = json.loads(compact["stdout"])["target_session_id"]
        except (ValueError, KeyError, TypeError):
            target = None
        if target:
            follow = self.invoke(model, workspace, "Return the chosen report filename from the checkpoint. No tools.", session=target)
            runs.append(follow)
        same_model = compaction_model_executed(compact["rows"], model)
        passed = valid and same_model and target is not None and "ledger-419.txt" in runs[-1]["stdout"]
        summary_usage = usage_metrics(compact["rows"])
        self.record(model, "compaction", "checkpoint-continuation", runs, passed,
                    {"checkpoint_valid": valid, "summary_model_attributed": same_model,
                     "summary_requests": summary_usage["requests"],
                     "summary_cache_policy": "baseline: ADR omits cache controls"})

    def recovery(self, model):
        provider = model.split("/", 1)[0]
        if provider not in ("openai", "anthropic"):
            self.cases.append({"model": model, "provider": provider, "suite": "recovery",
                               "case": "fault-proxy", "status": "not_applicable",
                               "reason": "provider_has_no_supported_base_url_override"})
            return
        for fault in ("before", "after"):
            workspace = self.workspace("recovery-" + fault)
            proxy = FaultProxy(provider, fault)
            url = proxy.start()
            try:
                run = self.invoke(model, workspace,
                                  "Write the integers from 1 to 80 separated by spaces. Do not use tools.", proxy=url, proxy_evidence=proxy)
            finally:
                proxy.close()
            passed = (proxy.injected and proxy.text_seen and proxy.upstream_statuses == [200]
                      and not proxy.transport_errors) and ((proxy.requests == 2 and run["exit"] == 0) if fault == "before"
                                         else (proxy.requests == 1 and proxy.text_seen and run["exit"] != 0))
            self.record(model, "recovery", fault, [run], passed,
                        {"proxy_requests": proxy.requests, "fault_injected": proxy.injected,
                         "real_text_seen": proxy.text_seen, "upstream_statuses": proxy.upstream_statuses,
                         "transport_errors": proxy.transport_errors}, baseline=True)

    def execute(self):
        for suite in self.args.suite:
            for model in self.args.model:
                try:
                    getattr(self, suite)(model)
                except Stop as error:
                    self.cases.append({"model": model, "provider": model.split("/", 1)[0],
                                       "suite": suite, "case": "prerequisite", "status": "blocked" if str(error).startswith(("blocked by", "external blocker")) else "stopped",
                                       "reason": str(error)})
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        result = {"schema": 1, "run_id": self.run_id, "date": datetime.now(timezone.utc).isoformat(),
                  "ego_commit": revision, "binary_sha256": digest(self.args.binary),
                  "models": self.args.model, "suites": self.args.suite, "fixture_seed": 419,
                  "fixture_hashes": self.fixture_hashes, "ceiling_usd": str(self.args.ceiling),
                  "provider_ceiling_usd": str(self.args.provider_ceiling), "cost_usd": str(self.spent),
                  "cost_uncertain": self.uncertain, "conservative_carry_usd": str(self.carried),
                  "unaccounted_attempts": self.unaccounted,
                  "carried_attempts": self.carried_attempts, "cases": self.cases}
        previous = None
        for path in sorted(self.args.results.glob("*/result.json"), reverse=True):
            candidate = json.loads(path.read_text())
            if candidate["models"] == result["models"] and candidate["suites"] == result["suites"]:
                previous = candidate
                break
        target = self.args.results / self.run_id
        target.mkdir(parents=True)
        with (target / "result.json").open("x") as output:
            json.dump(result, output, indent=2)
            output.write("\n")
        summary = ["# Live conformance " + self.run_id, "", f"Runner commit: `{revision}`.",
                   f"Known actual cost: USD {self.spent}; unknown actual cost: {self.uncertain}.",
                   f"Conservative carry (not actual): USD {self.carried}; unaccounted attempts: {self.unaccounted}.", "",
                   "| Model | Suite / case | Status | Later cache share |", "| --- | --- | --- | --- |"]
        for case in self.cases:
            share = case.get("metrics", {}).get("weighted_later_share")
            summary.append(f'| {case["model"]} | {case["suite"]}/{case["case"]} | {case["status"]} | {share if share is not None else "n/a"} |')
        summary += ["", f'Previous matching matrix: {previous["run_id"]}.' if previous else "No previous matching matrix."]
        if previous:
            summary.append(f'Accounted cost change: USD {self.spent - Decimal(previous["cost_usd"])}.')
            old = {(c["model"], c["suite"], c["case"]): c for c in previous["cases"]}
            for case in self.cases:
                key = (case["model"], case["suite"], case["case"])
                a = case.get("metrics", {}).get("weighted_later_share")
                b = old.get(key, {}).get("metrics", {}).get("weighted_later_share")
                if a is not None and b is not None:
                    summary.append(f"- {' / '.join(key)}: cache share change {a - b:+.4f}.")
        with (target / "summary.md").open("x") as output:
            output.write("\n".join(summary) + "\n")
        print(f"Results: {target}")
        failed = {"fail", "stopped", "budget_stop", "blocked"} | ({"baseline"} if self.args.strict else set())
        return int(self.uncertain or any(c["status"] in failed for c in self.cases))


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--run-set", action="append", default=[], metavar="K=V",
                        help="invocation config override for ego run only; never fork or compact")
    parser.add_argument("--operator-home", type=Path,
                        help="isolated OpenRouter operator facts home; other providers keep normal auth resolution")
    parser.add_argument("--openrouter-catalogue", type=Path,
                        help="recorded public model rows used to establish cache-floor eligibility")
    parser.add_argument("--diagnostic-dir", type=Path,
                        help="private directory for Codex hash/count/socket diagnostics only")
    parser.add_argument("--binary", type=Path, default=Path(shutil.which("ego") or "ego"))
    parser.add_argument("--model", action="append")
    parser.add_argument("--suite", action="append", choices=SUITES)
    parser.add_argument("--strict", action="store_true", default=True,
                        help="Fail measured regressions (the default).")
    parser.add_argument("--baseline", action="store_false", dest="strict",
                        help="Record measured feature misses as baselines instead of failures.")
    parser.add_argument("--cache-repeats", type=int, default=3)
    parser.add_argument("--ceiling", type=Decimal, default=Decimal("0.95"))
    parser.add_argument("--provider-ceiling", type=Decimal, default=Decimal("0.90"))
    home = Path.home() / "Gits"
    parser.add_argument("--ego-env", type=Path, default=home / "personal/ego/.env")
    parser.add_argument("--router-env", type=Path, default=home / "LS/jevclassifier/.env")
    parser.add_argument("--skills", type=Path, default=home / ".tmp/ego-418")
    parser.add_argument("--work", type=Path, default=home / ".tmp/ego-live-conformance")
    parser.add_argument("--results", type=Path, default=ROOT / "results/live-conformance")
    args = parser.parse_args()
    if args.cache_repeats < 1:
        parser.error("cache repeats must be positive")
    args.model = args.model or DEFAULT_MODELS
    args.suite = args.suite or SUITES
    args.binary = args.binary.resolve()
    if not args.ceiling.is_finite() or not args.provider_ceiling.is_finite() or min(args.ceiling, args.provider_ceiling) <= 0:
        parser.error("ceilings must be finite positive USD amounts")
    return args


def main():
    args = parse_args()
    if not args.live:
        print(json.dumps({"models": args.model, "suites": args.suite, "paid_calls": False}, indent=2))
        return 0
    # Private raw session evidence must not be readable by other local users.
    os.umask(0o077)
    return Runner(args).execute()


if __name__ == "__main__":
    raise SystemExit(main())
