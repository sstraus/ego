#!/usr/bin/env python3
"""Tests for harness.py. Run: python3 -B evals/harness/test_harness.py"""
import contextlib
import io
import json
import os
import stat
import sys
import subprocess
import tempfile
import unittest
from pathlib import Path

import decision
import harness
import runner
import suite

MANIFEST = Path(__file__).resolve().parent / "manifest.toml"
PROMPT = "crates/ego-core/src/model_prompts.rs"


def run_git(root, *args):
    subprocess.run(["git", "-C", str(root), "-c", "user.name=t", "-c", "user.email=t@t", *args],
                   check=True, capture_output=True)


class AffectedTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        run_git(self.root, "init", "-q")
        for path in (PROMPT, "README.md"):
            self.write(path, "v1")
        run_git(self.root, "add", "-A")
        run_git(self.root, "commit", "-qm", "base")

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, path, text):
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)

    def report(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = harness.main(["affected", "--base", "HEAD", "--root", str(self.root),
                                 "--manifest", str(MANIFEST)])
        return code, out.getvalue()

    def test_prompt_change_is_reported_with_digest(self):
        self.write(PROMPT, "v2")
        code, out = self.report()
        self.assertEqual(code, 0)
        self.assertIn("prompts: surface digest ", out)
        self.assertIn(PROMPT, out)
        before = out
        self.write(PROMPT, "v3")
        self.assertNotEqual(self.report()[1], before)

    def test_unrelated_change_reports_nothing_and_exits_zero(self):
        self.write("README.md", "v2")
        self.assertEqual(self.report(), (0, ""))

    def test_brace_and_recursive_globs(self):
        self.write("crates/ego-core/src/turn/judge.rs", "x")
        self.write("crates/ego-core/src/compact/a/b.rs", "x")
        self.write("crates/ego-core/src/turn/other.rs", "x")
        _, out = self.report()
        self.assertIn("turn/judge.rs", out)
        self.assertIn("compact/a/b.rs", out)
        self.assertNotIn("turn/other.rs", out)


class SuiteTests(unittest.TestCase):
    TEST = ("import unittest\nfrom m import f\n\nclass T(unittest.TestCase):\n"
            "    def test_f(self):\n        self.assertEqual(f(), 2)\n")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.suite = Path(self.tmp.name)
        task = self.suite / "t"
        for rel, text in {
            "task.toml": 'id = "t"\neditable = ["m.py"]\nhidden = "hidden/test_t.py"\n',
            "workspace/m.py": "def f():\n    return 1\n",
            "reference/m.py": "def f():\n    return 2\n",
            "hidden/test_t.py": self.TEST,
        }.items():
            (task / rel).parent.mkdir(parents=True, exist_ok=True)
            (task / rel).write_text(text)
        suite.write_digests(self.suite)

    def tearDown(self):
        self.tmp.cleanup()

    def edit(self, rel, text):
        (self.suite / "t" / rel).write_text(text)

    def test_valid_suite_passes(self):
        self.assertEqual(suite.verify_suite(self.suite), [])

    def test_changed_fixture_byte_fails_digest(self):
        self.edit("workspace/m.py", "def f():\n    return 1 \n")
        self.assertEqual(suite.verify_suite(self.suite), ["digest mismatch: t/workspace/m.py"])

    def test_broken_reference_fails(self):
        self.edit("reference/m.py", "def f():\n    return 3\n")
        suite.write_digests(self.suite)
        self.assertEqual(suite.verify_suite(self.suite), ["t: reference fails test_t.T.test_f"])

    def test_workspace_that_already_passes_fails(self):
        self.edit("workspace/m.py", "def f():\n    return 2\n")
        suite.write_digests(self.suite)
        self.assertEqual(suite.verify_suite(self.suite), ["t: untouched workspace passes test_t.T.test_f"])


THRESHOLDS = {"noise_margin": 2, "cost_flag_ratio": 1.20, "max_invalid_ratio": 0.10}


def row(task, arm, passed, cost=0.01, forbidden=(), invalid=None, code=0):
    return {"task": task, "arm": arm, "rep": 0, "exit": code, "pass": passed, "forbidden": list(forbidden),
            "invalid": invalid, "cost_usd": cost, "binary_sha256": arm}


def rows_for(task, arm, outcomes, **kw):
    return [row(task, arm, o, **kw) for o in outcomes]


class DecisionTests(unittest.TestCase):
    def test_forbidden_edit_or_safety_exit_is_regression_despite_more_passes(self):
        for bad in (row("a", "candidate", False, forbidden=["x"]), row("a", "candidate", False, code=7)):
            rows = (rows_for("a", "baseline", [True, False, False]) + rows_for("a", "candidate", [True] * 4) + [bad])
            self.assertEqual(decision.decide(rows, THRESHOLDS)["decision"], "regression")

    def test_one_broken_task_is_regression_with_tied_totals(self):
        rows = (rows_for("a", "baseline", [True] * 3) + rows_for("a", "candidate", [True, False, False])
                + rows_for("b", "baseline", [False] * 3) + rows_for("b", "candidate", [True, True, False]))
        self.assertEqual(decision.passes(rows, "baseline"), decision.passes(rows, "candidate"))
        self.assertEqual(decision.decide(rows, THRESHOLDS)["decision"], "regression")

    def test_too_many_invalid_runs_abstain(self):
        rows = rows_for("a", "baseline", [True] * 3) + rows_for("a", "candidate", [True] * 3)
        rows[0]["invalid"] = rows[3]["invalid"] = "provider_error"
        self.assertEqual(decision.decide(rows, THRESHOLDS)["decision"], "abstain")

    def test_equal_passes_with_higher_median_cost_is_neutral_with_flag(self):
        rows = rows_for("a", "baseline", [True] * 3) + rows_for("a", "candidate", [True] * 3, cost=0.0125)
        self.assertEqual(decision.decide(rows, THRESHOLDS), {"decision": "neutral", "cost_flag": True})

    def test_decide_and_record_write_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "res"
            for n, r in enumerate(rows_for("a", "baseline", [True] * 3) + rows_for("a", "candidate", [True] * 3)):
                d = out / "a" / f"{r['arm']}-{n}"
                d.mkdir(parents=True)
                (d / "run.json").write_text(json.dumps(r))
            (out / "meta.json").write_text('{"model": "m"}')
            summary = decision.write_decision(out, THRESHOLDS)
            self.assertIn("Boss decision", (out / "REPORT.md").read_text())
            path = decision.write_record(out, "945-x", Path(tmp) / "records", {"prompts": "d"}, "b1", "c1", "2026-11-08")
            record = json.loads(path.read_text())
            self.assertEqual((path.name, record["decision"], record["model"], record["binary_sha256"]),
                             ("2026-11-08-945-x.json", summary["decision"], "m",
                              {"baseline": ["baseline"], "candidate": ["candidate"]}))


FAKE_EGO = """#!{python}
import json, os, sys
argv = sys.argv
work = argv[argv.index("-C") + 1]
sessions = argv[argv.index("--session-dir") + 1]
mode = os.environ["FAKE_MODE"]
if mode == "stdin":
    sys.stdin.read()
if mode in ("stdin", "solve", "forbid", "leak", "unpriced"):
    open(os.path.join(work, "m.py"), "w").write("def f():\\n    return 2\\n")
if mode == "forbid":
    open(os.path.join(work, "TASK.md"), "w").write("tampered")
os.makedirs(sessions, exist_ok=True)
open(os.path.join(sessions, "events.jsonl"), "w").write(
    "read " + os.environ["FAKE_SUITE"] + "/t/hidden/test_t.py" if mode == "leak" else "ok")
cost = {{"cost": "unknown", "reason": "no_price"}} if mode == "unpriced" else {{"cost": "known",
        "amount": {{"amount": "0.01", "currency": "USD"}}, "attribution": {{"source": "provider_reported"}}}}
print(json.dumps({{"type": "item_started", "kind": "tool", "name": "read", "text": "{{}}"}}))
print(json.dumps({{"type": "item_started", "kind": "tool", "name": "read", "text": "{{}}"}}))
print(json.dumps({{"type": "usage_updated", "usage_delta": {{"output_tokens": 5}}, "cost_delta": cost}}))
print(json.dumps({{"type": "turn_completed", "ok": True, "exit_reason": "Completed"}}))
"""


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.suite = self.root / "tasks"
        task = self.suite / "t"
        for rel, text in {
            "task.toml": 'id = "t"\neditable = ["m.py"]\nhidden = "hidden/test_t.py"\nprompt = "go"\n',
            "workspace/m.py": "def f():\n    return 1\n",
            "workspace/TASK.md": "task",
            "hidden/test_t.py": SuiteTests.TEST,
        }.items():
            (task / rel).parent.mkdir(parents=True, exist_ok=True)
            (task / rel).write_text(text)
        self.binary = self.root / "ego"
        self.binary.write_text(FAKE_EGO.format(python=sys.executable))
        self.binary.chmod(self.binary.stat().st_mode | stat.S_IXUSR)
        self.config = {"model": "m", "flags": []}

    def tearDown(self):
        self.tmp.cleanup()

    def run_mode(self, mode):
        env = dict(os.environ, FAKE_MODE=mode, FAKE_SUITE=str(self.suite.resolve()))
        out = self.root / f"out-{mode}"
        work_root = self.root / "work"
        work_root.mkdir(exist_ok=True)
        rows = runner.run_matrix(self.suite, self.binary, self.binary, self.config, None, 1, out, env, work_root)
        stored = [json.loads(p.read_text()) for p in sorted(out.glob("t/*/run.json"))]
        self.assertEqual(rows, stored)
        return rows[0]

    def test_order_alternates_per_task_and_repetition(self):
        got = [(t, a) for t, a, _ in runner.order(["x", "y"], 2)]
        self.assertEqual(got, [("x", "baseline"), ("x", "candidate"), ("y", "candidate"), ("y", "baseline"),
                               ("x", "candidate"), ("x", "baseline"), ("y", "baseline"), ("y", "candidate")])

    def test_solved_run_records_metrics(self):
        row = self.run_mode("solve")
        self.assertTrue(row["pass"])
        self.assertEqual((row["tool_calls"], row["cost_usd"], row["tokens"]), ({"read": 2}, 0.01, {"output_tokens": 5}))
        self.assertEqual(row["binary_sha256"], runner.sha256_file(self.binary))

    def test_unsolved_workspace_does_not_pass(self):
        self.assertFalse(self.run_mode("idle")["pass"])

    def test_edit_outside_editable_scores_zero(self):
        row = self.run_mode("forbid")
        self.assertEqual((row["pass"], row["forbidden"]), (False, ["TASK.md"]))

    def test_session_naming_suite_path_is_invalid(self):
        row = self.run_mode("leak")
        self.assertEqual((row["pass"], row["invalid"]), (False, "leakage"))

    def test_relative_out_dir_keeps_sessions_out_of_workspace(self):
        cwd = os.getcwd()
        os.chdir(self.root)
        try:
            env = dict(os.environ, FAKE_MODE="solve", FAKE_SUITE="-")
            (self.root / "w").mkdir()
            row = runner.run_matrix(self.suite, self.binary, self.binary, self.config, None, 1, "rel-out", env, self.root / "w")[0]
        finally:
            os.chdir(cwd)
        self.assertEqual((row["pass"], row["forbidden"]), (True, []))

    def test_agent_never_waits_on_an_inherited_stdin(self):
        read_end, write_end = os.pipe()  # an open pipe that never reaches EOF, like a host shell
        saved = os.dup(0)
        os.dup2(read_end, 0)
        limit, runner.WALL_CLOCK_LIMIT = runner.WALL_CLOCK_LIMIT, 20
        try:
            row = self.run_mode("stdin")
        finally:
            runner.WALL_CLOCK_LIMIT = limit
            os.dup2(saved, 0)
            for fd in (read_end, write_end, saved):
                os.close(fd)
        self.assertEqual((row["pass"], row["invalid"]), (True, None))

    def test_unknown_cost_stays_null(self):
        self.assertIsNone(self.run_mode("unpriced")["cost_usd"])


class PythonVersionTests(unittest.TestCase):
    def test_harness_old_python_gives_clear_message(self):
        # Catches: a raw ImportError traceback on a Python without tomllib.
        script = Path(__file__).resolve().parent / "harness.py"
        hide = ("import runpy, sys; sys.modules['tomllib'] = None; "
                f"sys.argv = [{str(script)!r}, 'affected']; "
                f"runpy.run_path({str(script)!r}, run_name='__main__')")
        done = subprocess.run([sys.executable, "-B", "-c", hide], capture_output=True, text=True)
        self.assertNotEqual(done.returncode, 0)
        self.assertNotIn("Traceback", done.stderr)
        self.assertIn("Python 3.11", done.stderr)


if __name__ == "__main__":
    unittest.main()
