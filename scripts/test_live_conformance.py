"""Runner boundary tests; no model traffic and no credential reads."""
import argparse
import contextlib
import io
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("live_conformance", Path(__file__).with_name("live-conformance.py"))
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)
TMP = Path.home() / "Gits/.tmp"


def usage(amount="0.01", **tokens):
    return {"kind": "UsageUpdated", "attempt": {"attempt": "a"}, "usage": tokens,
            "cost_delta": {"cost": "known", "amount": {"amount": amount, "currency": "USD"}}}


class RunnerContracts(unittest.TestCase):
    def test_proxy_os_context_catches_default_ca_store_and_disabled_verification(self):
        import ssl
        from urllib.parse import urlsplit

        # The real loopback handler must pass an OS verifier, not Python's
        # independent CA bundle, to its upstream TLS boundary.
        proxy = live.FaultProxy("openai", "after")
        endpoint = urlsplit(proxy.start())
        client = live.http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=5)
        try:
            with patch.object(live.http.client, "HTTPSConnection") as upstream:
                upstream.return_value.request.side_effect = ssl.SSLCertVerificationError("fixture rejection")
                client.request("POST", "/v1/responses", body="{}")
                with self.assertRaises(live.http.client.RemoteDisconnected):
                    client.getresponse()
            context = upstream.call_args.kwargs.get("context")
            self.assertIsNotNone(context, "upstream must not fall back to Python's CA bundle")
            self.assertTrue(type(context).__module__.startswith("truststore."))
            self.assertEqual(context.verify_mode, ssl.CERT_REQUIRED)
            self.assertTrue(context.check_hostname)
            self.assertEqual(proxy.transport_errors, ["SSLCertVerificationError"])
            self.assertEqual(proxy.upstream_statuses, [])
            self.assertFalse(proxy.text_seen)
        finally:
            client.close()
            proxy.close()

    def test_cache_floor_exemption_does_not_hide_execution_errors(self):
        catalogue = Path(__file__).resolve().parents[1] / "crates/ego-ai/tests/goldens/openrouter/cache-prices-2026-10-27.json"
        for model, failed, expected, floor in [
            ("openrouter/z-ai/glm-4.7-flash", False, "pass", "not_applicable"),
            ("openrouter/deepseek/deepseek-v4-flash-0731", False, "fail", "fail"),
            ("openrouter/z-ai/glm-4.7-flash", True, "fail", "not_applicable"),
            ("openrouter/unknown/model", False, "fail", "fail"),
            ("openrouter/z-ai/glm-4.7-flash", True, "fail", "invalid_workload"),
        ]:
            with self.subTest(model=model, failed=failed), tempfile.TemporaryDirectory(dir=TMP) as directory:
                runner = live.Runner(argparse.Namespace(work=Path(directory), strict=True,
                                                        openrouter_catalogue=catalogue))
                rows = [usage(uncached_input_tokens=100)] + [
                    {"kind": "ToolCallStarted", "name": "read"} for _ in range(3)]
                run = {"rows": rows, "stdout": "LEDGER-419-OK", "exit": int(failed),
                       "reason": "padding_not_admitted" if floor == "invalid_workload" else ("process_failed" if failed else "completed"), "metrics": {}}
                fork = dict(run, rows=[], stdout=json.dumps({"result": "child"}))
                with patch.object(runner, "invoke", side_effect=[run, run, run, fork, run]), contextlib.redirect_stdout(io.StringIO()):
                    runner.cache_once(model, 0)
                case = runner.cases[0]
                self.assertEqual(case["status"], expected)
                self.assertEqual(case["cache_floor_status"], floor)
                self.assertEqual(case.get("cache_floor_reason"),
                                 "padding_not_admitted" if floor == "invalid_workload" else ("no cache price in catalogue" if floor == "not_applicable" else None))

    def test_padding_rejection_cannot_pass_as_valid_workload(self):
        # Catches: rejected oversized instructions still publish a valid cache
        # workload merely because ego exits successfully and reports usage.
        for strict in (True, False):
            with self.subTest(strict=strict), tempfile.TemporaryDirectory(dir=TMP) as directory:
                root = Path(directory)
                runner = live.Runner(argparse.Namespace(
                    work=root, strict=strict, ceiling=live.Decimal("0.70"),
                    provider_ceiling=live.Decimal("0.70"), binary=root / "ego",
                    ego_env=root / "unused.env", router_env=root / "unused.env"))
                workspace = runner.workspace("cache", padding=True)

                def rejected(*_args, **_kwargs):
                    store = runner.store / "source"
                    store.mkdir(parents=True)
                    # Recorded 658 failure shape: run_context.instructions=[]
                    # despite a completed generation and known usage.
                    payloads = [
                        {"evidence": {"kind": "run_context", "invocation": "rejected",
                                      "instructions": []}},
                        {"event": {"kind": usage(cache_read_tokens=100)}},
                    ]
                    (store / "events.jsonl").write_text("".join(
                        json.dumps({"seq": index, "payload": payload}) + "\n"
                        for index, payload in enumerate(payloads, 1)))
                    return argparse.Namespace(returncode=0, stdout="LEDGER-419-OK",
                                              stderr="AGENTS.md exceeds 65536 bytes")

                with patch.object(live, "child_env", return_value={}), \
                        patch.object(live.subprocess, "run", side_effect=rejected):
                    run = runner.invoke("openai-codex/model", workspace, "Read part-0.txt")
                with contextlib.redirect_stdout(io.StringIO()):
                    runner.record("openai-codex/model", "cache", "padding", [run], True, baseline=True)
                self.assertEqual(run["reason"], "padding_not_admitted")
                self.assertEqual(runner.cases[0]["status"], "fail")
                self.assertEqual(runner.spent, live.Decimal("0.01"))

    def test_padding_command_boundary_catches_compaction_bypass_and_local_fork_false_failure(self):
        for command, admitted, expected in ((None, True, "completed"),
                                             (["compact", "source"], True, "completed"),
                                             (["compact", "source"], False, "padding_not_admitted"),
                                             (["fork", "source"], False, "completed")):
            with self.subTest(command=command, admitted=admitted), tempfile.TemporaryDirectory(dir=TMP) as directory:
                root = Path(directory)
                runner = live.Runner(argparse.Namespace(
                    work=root, strict=True, ceiling=live.Decimal("0.70"),
                    provider_ceiling=live.Decimal("0.70"), binary=root / "ego",
                    ego_env=root / "unused.env", router_env=root / "unused.env"))
                workspace = runner.workspace("compaction", padding=True)

                def complete(*_args, **_kwargs):
                    store = runner.store / "source"
                    store.mkdir(parents=True)
                    instructions = [dict(path=path, **descriptor) for path, descriptor
                                    in runner.padding_fixtures[workspace].items()]
                    evidence = [
                        {"kind": "run_context", "invocation": "current",
                         "instructions": instructions if admitted else []},
                        {"kind": "generation_admission", "invocation": "current",
                         "attempt": {"attempt": "a"}, "target": {"canonical_model": "provider/model"}},
                    ] if command is None or command[0] != "fork" else []
                    payloads = [{"evidence": row} for row in evidence]
                    if command and command[0] == "compact":
                        # Bare target admission is empty; source prompt owns padding.
                        evidence[0]["instructions"] = []
                        payloads.append({"event": {"kind": {
                            "kind": "GenerationStarted", "role": "compaction",
                            "attempt": {"attempt": "a"},
                            "snapshot": {"system": {"sources": instructions if admitted else []}}}}})
                    (store / "events.jsonl").write_text("".join(
                        json.dumps({"seq": index, "payload": payload}) + "\n"
                        for index, payload in enumerate(payloads, 1)))
                    return argparse.Namespace(returncode=0, stdout="OK", stderr="")

                with patch.object(live, "child_env", return_value={}), \
                        patch.object(live.subprocess, "run", side_effect=complete):
                    run = runner.invoke("openai-codex/model", workspace, "OK", command=command)
                self.assertEqual(run["reason"], expected)
                self.assertEqual(run["process_exit"], 0)
                self.assertEqual(run["exit"], int(expected == "padding_not_admitted"))
                if command is None or command[0] != "fork":
                    self.assertEqual(run["metrics"]["padding_admission"]["admitted"], admitted)
                else:
                    self.assertNotIn("padding_admission", run["metrics"])

    def test_compaction_padding_catches_target_context_false_rejection(self):
        # Catches: bare target admission hides the unchanged padded source snapshot.
        fixture = json.loads((Path(__file__).parent / "fixtures/699-compaction-padding.json").read_text())
        expected = {entry["path"]: entry for entry in fixture[0]["payload"]["evidence"]["instructions"]}
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            root = Path(directory)
            runner = live.Runner(argparse.Namespace(
                work=root, strict=True, ceiling=live.Decimal("0.70"),
                provider_ceiling=live.Decimal("0.70"), binary=root / "ego",
                ego_env=root / "unused.env", router_env=root / "unused.env"))
            workspace = runner.workspace("compaction")
            runner.padding_fixtures[workspace] = expected
            store = runner.store / "source"
            store.mkdir(parents=True)
            journal = store / "events.jsonl"
            journal.write_text("".join(json.dumps(row) + "\n" for row in fixture[:3]))

            def recorded_compaction(*_args, **_kwargs):
                journal.write_text("".join(json.dumps(row) + "\n" for row in fixture))
                return argparse.Namespace(returncode=0, stdout="{}", stderr="")

            with patch.object(live, "child_env", return_value={}), \
                    patch.object(live.subprocess, "run", side_effect=recorded_compaction):
                run = runner.invoke("openai-codex/model", workspace, command=["compact", "source"])
            self.assertEqual(run["reason"], "completed")
            self.assertEqual(run["exit"], 0)
            self.assertTrue(run["metrics"]["padding_admission"]["admitted"])

    def test_compaction_padding_catches_missing_mutated_replaced_and_foreign_snapshots(self):
        fixture_path = Path(__file__).parent / "fixtures/699-compaction-padding.json"
        for defect in ("missing", "digest", "bytes", "path", "replaced", "foreign", "attempt", "unstarted",
                       "CompactionSeeded", "Compacted", "Forked"):
            with self.subTest(defect=defect), tempfile.TemporaryDirectory(dir=TMP) as directory:
                fixture = json.loads(fixture_path.read_text())
                expected = {entry["path"]: entry for entry in fixture[0]["payload"]["evidence"]["instructions"]}
                source = fixture[2]
                start = fixture[-1]["payload"]["event"]["kind"]
                if defect == "missing":
                    del source["payload"]["event"]["kind"]["snapshot"]
                elif defect in ("digest", "bytes", "path"):
                    field, value = {"digest": ("sha256", "wrong"), "bytes": ("bytes", 0),
                                    "path": ("path", "elsewhere")}[defect]
                    source["payload"]["event"]["kind"]["snapshot"]["system"]["sources"][-1][field] = value
                elif defect == "replaced":
                    start["snapshot"] = {"system": None}
                elif defect in ("CompactionSeeded", "Compacted", "Forked"):
                    fixture.insert(3, {"seq": 19, "payload": {"event": {"kind": {"kind": defect}}}})
                elif defect == "attempt":
                    start["attempt"]["attempt"] = "unrelated"
                elif defect == "unstarted":
                    fixture.pop()
                    # Valid target descriptors must not bypass missing source proof.
                    fixture[3]["payload"]["evidence"]["instructions"] = list(expected.values())
                root = Path(directory)
                store = root / "source"
                store.mkdir()
                if defect == "foreign":
                    other = root / "previous"
                    other.mkdir()
                    (other / "events.jsonl").write_text(json.dumps(source) + "\n")
                    fixture.remove(source)
                (store / "events.jsonl").write_text("".join(json.dumps(row) + "\n" for row in fixture))
                rows = [row for (session, seq), row in live.events(root).items()
                        if session == "source" and seq >= 20]
                self.assertFalse(live.padding_admission(rows, expected, compaction=True)["admitted"])

    def test_cache_fixture_is_stable_and_large_for_measured_runs(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            runner = live.Runner(argparse.Namespace(work=Path(directory), strict=True))
            first = runner.workspace("first", padding=True)
            second = runner.workspace("second", padding=True)
            paths = ["AGENTS.md", *(f"reference-{chunk}.md" for chunk in range(4))]
            texts = [(first / name).read_text() for name in paths]
            self.assertGreater(sum(len(text.split()) for text in texts), 32768)
            self.assertEqual(texts, [(second / name).read_text() for name in paths])
            # Catches: fixture grows beyond ego's per-file or aggregate limits.
            self.assertTrue(all((first / name).stat().st_size <= 65536 for name in paths))
            self.assertLessEqual(sum((first / name).stat().st_size for name in paths), 262144)
            self.assertEqual(texts[0].count("\n@reference-"), 4)

    def test_padding_admission_catches_missing_mutated_and_stale_instruction_records(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            runner = live.Runner(argparse.Namespace(work=Path(directory)))
            workspace = runner.workspace("padding", padding=True)
            expected = runner.padding_fixtures[workspace]
            instructions = [dict(path=path, **descriptor) for path, descriptor in expected.items()]
            context = {"kind": "RunContextEvidence", "invocation": "current", "instructions": instructions}
            generation = {"kind": "GenerationAdmissionEvidence", "invocation": "current"}
            self.assertTrue(live.padding_admission([context, generation], expected)["admitted"])
            for field, wrong in (("sha256", "wrong"), ("bytes", 0), ("path", "elsewhere")):
                with self.subTest(field=field):
                    changed = [dict(entry) for entry in instructions]
                    changed[-1][field] = wrong
                    self.assertFalse(live.padding_admission(
                        [dict(context, instructions=changed), generation], expected)["admitted"])
            for rows in ([generation], [context],
                         [dict(context, instructions=instructions[:-1]), generation],
                         [context, dict(generation, invocation="previous")]):
                self.assertFalse(live.padding_admission(rows, expected)["admitted"])

    def test_journal_padding_evidence_catches_dropped_invocation_binding(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            store = Path(directory) / "source"
            store.mkdir()
            evidence = [
                {"kind": "run_context", "invocation": "current", "instructions": []},
                {"kind": "generation_admission", "invocation": "current",
                 "attempt": {"attempt": "a"}, "target": {"canonical_model": "provider/model"}},
            ]
            (store / "events.jsonl").write_text("".join(
                json.dumps({"seq": seq, "payload": {"evidence": row}}) + "\n"
                for seq, row in enumerate(evidence, 1)))
            rows = list(live.events(Path(directory)).values())
            self.assertEqual([row["kind"] for row in rows],
                             ["RunContextEvidence", "GenerationAdmissionEvidence"])
            self.assertEqual([row["invocation"] for row in rows], ["current", "current"])

    def test_cache_partition_includes_writes_catches_inflated_hit_share(self):
        rows = [usage(uncached_input_tokens=100),
                usage(uncached_input_tokens=10, cache_read_tokens=60,
                      cache_write_tokens=20, cache_write_extended_tokens=10)]
        metrics = live.usage_metrics(rows)
        self.assertEqual(metrics["weighted_later_share"], .6)
        self.assertEqual(metrics["cost_usd"], "0.02")

    def test_turn_split_catches_resumed_turn_misclassified_as_tool_continuation(self):
        rows = [{"kind": "UserPrompt"}, usage(uncached_input_tokens=100),
                usage(cache_read_tokens=100), {"kind": "UserPrompt"},
                usage(uncached_input_tokens=100), usage(cache_read_tokens=100)]
        metrics = live.usage_metrics(rows)
        self.assertEqual(metrics["turn_split"]["first"],
                         {"requests": 2, "zero_hits": 2, "weighted_share": 0})
        self.assertEqual(metrics["turn_split"]["later"],
                         {"requests": 2, "zero_hits": 0, "weighted_share": 1})

    def test_no_replay_expected_failure_catches_false_baseline_status(self):
        runner = object.__new__(live.Runner)
        runner.cases = []
        runner.args = argparse.Namespace(strict=True)
        run = {"rows": [usage(uncached_input_tokens=20)], "exit": 1,
               "reason": "process_failed", "metrics": {"proxy_proven_zero_attempt": False}}
        with patch("builtins.print"):
            runner.record("openai/model", "recovery", "after", [run], True, baseline=True)
        self.assertEqual(runner.cases[0]["status"], "pass")

    def test_skill_model_evidence_catches_wrong_target_and_unused_admission(self):
        expected = "openai-codex/gpt-6-luna"
        admitted = {"kind": "SkillInstructionAdmitted", "model": expected}
        evidence = {"kind": "GenerationAdmissionEvidence", "attempt": {"attempt": "a"},
                    "target": {"canonical_model": expected}}
        started = {"kind": "GenerationStarted", "attempt": {"attempt": "a"}}
        self.assertTrue(live.skill_model_executed([admitted, evidence, started, usage()], expected))
        self.assertFalse(live.skill_model_executed([admitted, evidence, usage()], expected))
        wrong = dict(evidence, target={"canonical_model": "openai/gpt-6-luna"})
        self.assertFalse(live.skill_model_executed([admitted, wrong, started, usage()], expected))
        self.assertFalse(live.skill_model_executed([evidence, started, usage(), admitted], expected))

    def test_unknown_cost_catches_false_zero_spend(self):
        metrics = live.usage_metrics([{"kind": "UsageUpdated", "usage": {}}])
        self.assertTrue(metrics["unknown_cost"])
        self.assertIsNone(metrics["weighted_later_share"])

    def test_journal_coordinates_catch_same_seq_across_fork_sessions(self):
        with tempfile.TemporaryDirectory(dir=TMP) as folder:
            store = Path(folder)
            for session in ("source", "fork"):
                path = store / session
                path.mkdir()
                (path / "events.jsonl").write_text(json.dumps({"seq": 1, "payload": {"event": {"kind": usage()}}}) + "\n")
            self.assertEqual(len(live.events(store)), 2)

    def test_child_environment_catches_inherited_oauth_and_wrong_vendor_key(self):
        with tempfile.TemporaryDirectory(dir=TMP) as folder:
            source = Path(folder) / "fixture.env"
            source.write_text('OPENAI_API_KEY="fixture-not-a-key"\nANTHROPIC_API_KEY=wrong-vendor\n')
            with patch.dict(live.os.environ, {"CLAUDE_CODE_OAUTH_TOKEN": "must-drop", "ANTHROPIC_API_KEY": "must-drop", "OPENAI_BASE_URL": "must-drop"}):
                env = live.child_env("openai", source, source)
            self.assertEqual(env["OPENAI_API_KEY"], "fixture-not-a-key")
            self.assertNotIn("ANTHROPIC_API_KEY", env)
            self.assertNotIn("CLAUDE_CODE_OAUTH_TOKEN", env)
            self.assertNotIn("OPENAI_BASE_URL", env)

    def test_provider_environment_catches_operator_home_leaking_into_codex_auth(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            root = Path(directory)
            runner = live.Runner(argparse.Namespace(
                work=root / "runs", ceiling=live.Decimal("0.70"),
                provider_ceiling=live.Decimal("0.70"), binary=root / "ego",
                ego_env=root / "unused.env", router_env=root / "unused.env",
                operator_home=root / "operator", diagnostic_dir=root / "diagnostics"))
            workspace = runner.workspace("environment")
            completed = argparse.Namespace(returncode=0, stdout="OK", stderr="")
            with patch.object(live, "child_env", return_value={"EGO_HOME": "normal-auth-home"}), \
                    patch.object(live.subprocess, "run", return_value=completed) as invoke:
                runner.invoke("openrouter/openai/gpt-6-luna", workspace, "OK")
                routed = dict(invoke.call_args.kwargs["env"])
            with patch.object(live, "child_env", return_value={"EGO_HOME": "normal-auth-home"}), \
                    patch.object(live.subprocess, "run", return_value=completed) as invoke:
                runner.invoke("openai-codex/gpt-6-luna", workspace, "OK")
                codex = invoke.call_args.kwargs["env"]
            self.assertEqual(routed["EGO_HOME"], str(root / "operator"))
            self.assertEqual(codex["EGO_HOME"], "normal-auth-home")
            self.assertNotIn("EGO_LOG_FILE", routed)
            self.assertEqual(codex["EGO_LOG"], "off,ego_ai::codex_session=debug,ego_tools::http::websocket=debug")
            self.assertEqual(Path(codex["EGO_LOG_FILE"]).parent, root / "diagnostics")

    def test_private_stderr_catches_lost_admission_error_and_api_key_disclosure(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            root = Path(directory)
            runner = live.Runner(argparse.Namespace(
                work=root / "runs", ceiling=live.Decimal("0.05"),
                provider_ceiling=live.Decimal("0.05"), binary=root / "ego",
                ego_env=root / "unused.env", router_env=root / "unused.env"))
            workspace = runner.workspace("diagnostics")
            completed = argparse.Namespace(returncode=1, stdout="", stderr="admission refused fixture-secret")
            timeout = live.subprocess.TimeoutExpired(["ego"], 65, stderr=b"partial fixture-secret")
            paths = []
            for result in (completed, timeout):
                with patch.object(live, "child_env", return_value={"OPENROUTER_API_KEY": "fixture-secret"}), \
                        patch.object(live.subprocess, "run") as invoke:
                    if isinstance(result, Exception):
                        invoke.side_effect = result
                    else:
                        invoke.return_value = result
                    run = runner.invoke("openrouter/openai/gpt-6-luna", workspace, "OK")
                path = Path(run["stderr_path"])
                paths.append(path)
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                self.assertNotIn("fixture-secret", path.read_text())
            self.assertNotEqual(*paths)
            self.assertEqual(paths[0].read_text(), "admission refused [redacted]")
            self.assertEqual(paths[1].read_text(), "partial [redacted]")

    def test_run_config_catches_executable_grant_lost_or_leaked_to_local_commands(self):
        # Recorded ego fork/compact --help accepts no -c; only run owns overrides.
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            root = Path(directory)
            override = 'exec_paths=["/exact/tuic-bridge"]'
            runner = live.Runner(argparse.Namespace(
                work=root / "runs", ceiling=live.Decimal("2"),
                provider_ceiling=live.Decimal("2"), binary=root / "ego",
                ego_env=root / "unused", router_env=root / "unused",
                run_set=[override]))
            workspace = runner.workspace("argv")
            completed = argparse.Namespace(returncode=0, stdout="OK", stderr="")
            for command in (None, ["fork", "source"], ["compact", "source"]):
                with self.subTest(command=command), \
                        patch.object(live, "child_env", return_value={}), \
                        patch.object(live.subprocess, "run", return_value=completed) as process:
                    runner.invoke("openai-codex/gpt-6-luna", workspace, "OK", command=command)
                    argv = process.call_args.args[0]
                    if command is None:
                        self.assertEqual(argv[argv.index("-c") + 1], override)
                    else:
                        self.assertNotIn("-c", argv)
                        self.assertEqual(argv[1:3], command)
                        self.assertEqual(argv[argv.index("--output") + 1], "json")
                        if command[0] == "compact":
                            self.assertEqual(argv[argv.index("--budget") + 1], "2$")
                            self.assertEqual(argv[argv.index("--max-output-tokens") + 1], "2048")

    def test_generation_role_catches_compaction_usage_mixed_with_main(self):
        metrics = live.usage_metrics([
            {"kind": "GenerationStarted", "attempt": {"attempt": "a"}, "role": "compaction"},
            usage(uncached_input_tokens=12)])
        self.assertEqual(metrics["requests"][0]["role"], "compaction")


class ResultOracleRegressions(unittest.TestCase):
    """Plausible regressions must not publish a reassuring conformance result."""

    def test_baseline_does_not_hide_process_failure_after_usage(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            runner = live.Runner(argparse.Namespace(work=Path(directory)))
            rows = [{'kind': 'UsageUpdated', 'usage': {'uncached_input_tokens': 100},
                     'cost_delta': {'cost': 'known', 'amount': {'amount': '0', 'currency': 'USD'}}}]
            run = {'rows': rows, 'exit': 1, 'reason': 'process_failed', 'metrics': {}}
            with contextlib.redirect_stdout(io.StringIO()):
                runner.record('openai/gpt-6-luna', 'cache', 'resume', [run], False, baseline=True)
            self.assertEqual(runner.cases[0]['status'], 'fail',
                             'a crashed measured process must fail even outside strict mode')

    def test_skill_results_catch_prose_false_negative_and_forged_success(self):
        # Recorded ego journals: correct tool bytes must survive prose reformatting;
        # fluent prose must not hide missing admission, bad rendering or wrong model.
        import copy
        corpus = json.loads(Path(__file__).with_name("live-conformance-skill-events.json").read_text())
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            root = Path(directory)
            fixtures = root / "fixtures"
            fixtures.mkdir()
            (fixtures / "manifest.json").write_text(json.dumps(
                [{"case": case, "prompt": case} for case in corpus]))
            for case in corpus:
                skill = fixtures / case / ".ego/skills/check"
                skill.mkdir(parents=True)
                (skill / "SKILL.md").write_text("Recorded event replay; no model execution.\n")
            for defect in (None, "no_admission", "bad_result", "failed_result", "wrong_model", "missing_read"):
                with self.subTest(defect=defect):
                    runner = live.Runner(argparse.Namespace(work=root / str(defect), skills=fixtures))
                    def replay(_model, _workspace, prompt):
                        rows = copy.deepcopy(corpus[prompt]["rows"])
                        if defect == "no_admission":
                            rows = [row for row in rows if row["kind"] != "SkillInstructionAdmitted"]
                        if defect in ("bad_result", "failed_result"):
                            for row in rows:
                                if row["kind"] == "ToolCallCompleted":
                                    if defect == "bad_result":
                                        row["result"]["content"] = [{"text": "wrong rendered bytes"}]
                                    else:
                                        row["ok"] = False
                        if defect == "wrong_model":
                            for row in rows:
                                if row["kind"] == "GenerationAdmissionEvidence":
                                    row["target"]["canonical_model"] = "openai/other"
                        if defect == "missing_read" and prompt == "tools":
                            rows = [row for row in rows if not (row["kind"] == "ToolCallStarted" and row["name"] == "read")]
                        return {"rows": rows, "stdout": "Reformatted prose" if defect is None else
                                "AMBER-418-OK COBALT-418-OK IVORY-418-READ raw=[alpha beta] zero=[alpha] one=[beta] first=[alpha] missing=[$2]",
                                "exit": 0, "reason": "completed", "metrics": {}}
                    with patch.object(runner, "invoke", side_effect=replay), contextlib.redirect_stdout(io.StringIO()):
                        runner.skills("openai-codex/gpt-6-luna")
                    expected = {case: "pass" for case in corpus}
                    if defect in ("no_admission", "bad_result", "failed_result"):
                        expected = {case: "fail" for case in corpus}
                    elif defect == "wrong_model":
                        expected["model"] = "fail"
                    elif defect == "missing_read":
                        expected["tools"] = "fail"
                    self.assertEqual({case["case"]: case["status"] for case in runner.cases}, expected)


class ConformanceGapRegressions(unittest.TestCase):
    def test_default_threshold_miss_catches_reassuring_baseline(self):
        runner = object.__new__(live.Runner)
        with patch("sys.argv", ["live-conformance.py"]):
            runner.args = live.parse_args()
        runner.cases = []
        run = {"rows": [usage(uncached_input_tokens=100)], "exit": 0,
               "reason": "completed", "metrics": {}}
        with contextlib.redirect_stdout(io.StringIO()):
            runner.record("openai/model", "cache", "resume", [run], False, baseline=True)
        self.assertEqual(runner.cases[0]["status"], "fail")

    def test_compaction_catches_checkpoint_from_wrong_model(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            runner = live.Runner(argparse.Namespace(work=Path(directory)))
            checkpoint = "# Objective\n# Decisions and rationale\n# Ordered next steps"
            runs = [
                {"rows": [], "stdout": "READY", "exit": 0, "reason": "completed", "metrics": {}},
                {"rows": [{"kind": "CompactionSeeded", "checkpoint": checkpoint}],
                 "stdout": json.dumps({"target_session_id": "target"}), "exit": 0,
                 "reason": "completed", "metrics": {}},
                {"rows": [], "stdout": "ledger-419.txt", "exit": 0,
                 "reason": "completed", "metrics": {}}]
            with patch.object(runner, "invoke", side_effect=runs), contextlib.redirect_stdout(io.StringIO()):
                runner.compaction("openai/gpt-6-luna")
            self.assertEqual(runner.cases[0]["status"], "fail")

    def test_compaction_model_catches_wrong_role_and_unused_admission(self):
        expected = "openai/gpt-6-luna"
        evidence = {"kind": "GenerationAdmissionEvidence", "attempt": {"attempt": "a"},
                    "target": {"canonical_model": expected}}
        start = {"kind": "GenerationStarted", "attempt": {"attempt": "a"}, "role": "compaction"}
        self.assertTrue(live.compaction_model_executed([evidence, start, usage()], expected))
        self.assertFalse(live.compaction_model_executed([evidence, start], expected))
        self.assertFalse(live.compaction_model_executed([evidence, dict(start, role="main"), usage()], expected))
        self.assertFalse(live.compaction_model_executed([evidence, start, usage()], "anthropic/other"))

    def test_allowance_catches_provider_exhaustion_and_unknown_spend(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            runner = live.Runner(argparse.Namespace(work=Path(directory), ceiling=live.Decimal("0.95"),
                                                   provider_ceiling=live.Decimal("0.90")))
            runner.spent = live.Decimal("0.10")
            runner.by_provider["openai"] = live.Decimal("0.05")
            self.assertEqual(runner.allowance("openai"), live.Decimal("0.85"))
            runner.by_provider["openai"] = live.Decimal("0.90")
            with self.assertRaisesRegex(live.Stop, "cost_ceiling_reached"):
                runner.allowance("openai")
            runner.unaccounted = True
            with self.assertRaisesRegex(live.Stop, "unaccounted_attempt"):
                runner.allowance("openrouter")

    def test_unknown_attempts_continue_with_separate_once_only_envelope_carry(self):
        catalogue = Path(__file__).resolve().parents[1] / "crates/ego-ai/tests/goldens/openrouter/cache-prices-2026-10-27.json"
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            runner = live.Runner(argparse.Namespace(work=Path(directory), ceiling=live.Decimal("2"),
                provider_ceiling=live.Decimal("2"), openrouter_catalogue=catalogue))
            model = "openrouter/z-ai/glm-4.7-flash"
            row = dict(usage(), cost_delta={"cost": "unknown"})
            runner.account(model, [row, row])
            self.assertEqual(runner.spent, live.Decimal("0"))
            self.assertEqual(runner.carried, live.Decimal("0.0129192"))
            self.assertEqual(runner.allowance("openrouter"), live.Decimal("1.9870808"))
            self.assertTrue(runner.uncertain)
            self.assertFalse(runner.unaccounted)
            # A distinct possibly-sent attempt with no usage needs its own carry.
            runner.account(model, [{"kind": "GenerationStarted", "attempt": {"attempt": "b"}}])
            self.assertEqual(runner.carried, live.Decimal("0.0258384"))
            runner.account("openrouter/unlisted", [{"kind": "GenerationStarted", "attempt": {"attempt": "c"}}])
            with self.assertRaisesRegex(live.Stop, "unaccounted_attempt"):
                runner.allowance("openrouter")

    def test_recovery_catches_success_without_upstream_text(self):
        with tempfile.TemporaryDirectory(dir=TMP) as directory:
            runner = live.Runner(argparse.Namespace(work=Path(directory)))
            run = {"rows": [usage()], "stdout": "", "exit": 0,
                   "reason": "completed", "metrics": {}}
            class Proxy:
                def __init__(self, *_):
                    self.injected, self.requests, self.text_seen = True, 2, False
                    self.upstream_statuses, self.transport_errors = [500], []
                def start(self):
                    return "http://127.0.0.1/"
                def close(self):
                    pass
            with patch.object(live, "FaultProxy", Proxy), patch.object(runner, "invoke", return_value=run), contextlib.redirect_stdout(io.StringIO()):
                runner.recovery("openai/gpt-6-luna")
            self.assertEqual(runner.cases[0]["status"], "fail")


if __name__ == "__main__":
    unittest.main()
