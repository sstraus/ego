"""Numeric and cost-admission guards for the opt-in matched collector."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("matched", Path(__file__).with_name("codex-matched-cache.py"))
matched = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(matched)


class MatchedCacheTests(unittest.TestCase):
    def test_first_and_small_requests_cannot_dilute_later_large_miss_share(self):
        rows = [
            {"ordinal": 1, "input_tokens": 5000, "cache_read_tokens": 0, "generation_interval_seconds": 1},
            {"ordinal": 2, "input_tokens": 4096, "cache_read_tokens": 0, "generation_interval_seconds": 2},
            {"ordinal": 3, "input_tokens": 5000, "cache_read_tokens": 0, "generation_interval_seconds": 3},
            {"ordinal": 4, "input_tokens": 6000, "cache_read_tokens": 5000, "generation_interval_seconds": 4},
        ]
        result = matched.summary(rows)
        self.assertEqual(result["later_large"], 2)
        self.assertEqual(result["later_large_zero"], 1)
        self.assertEqual(result["zero_share_large_later"], .5)
        self.assertAlmostEqual(result["weighted_share"], 5000 / 20096)

    def test_unknown_cost_cannot_be_treated_as_free_subscription(self):
        for delta in [None, {"cost": "unknown"}, {"cost": "known", "amount": {"currency": "EUR", "amount": "0"}}]:
            with self.subTest(delta=delta), self.assertRaisesRegex(RuntimeError, "unknown ego cost"):
                matched.checked_cost([{"kind": "UsageUpdated", "cost_delta": delta}])

    def test_process_deadline_keeps_partial_output_and_uses_the_requested_limit(self):
        with patch.object(matched.subprocess, "run", side_effect=matched.subprocess.TimeoutExpired(["client"], 300, output=b"partial", stderr=b"diagnostic")) as run:
            result, expired = matched.run_turn(["client"], {}, Path('.'), "prompt", 300)
        self.assertTrue(expired)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (124, "partial", "diagnostic"))
        self.assertEqual(run.call_args.kwargs["timeout"], 300)

    def test_resume_cannot_bypass_the_two_timeout_stop(self):
        experiment = matched.Experiment.__new__(matched.Experiment)
        experiment.result = {"timeouts_300": [{}, {}]}
        experiment.rates = Mock()
        with self.assertRaisesRegex(RuntimeError, "continuation forbidden"):
            experiment.run()
        experiment.rates.assert_not_called()

    def test_wall_distribution_cannot_hide_timed_out_calls(self):
        result = matched.wall_distribution([
            {"wall_seconds": 2, "exit": 0}, {"wall_seconds": 300, "exit": 124},
        ])
        self.assertEqual(result, {"calls": 2, "timeouts": 1, "mean": 151,
                                  "median": 151, "p95_nearest_rank": 300, "max": 300})

    def test_cli_transport_cannot_be_inferred_from_connection_or_models_lookup(self):
        text = '\n'.join([
            'INFO codex_otel.log_only: event.name=codex.websocket_connect status=200',
            'INFO codex_otel.log_only: event.name=codex.api_request url=https://chatgpt.com/backend-api/models',
            'user text says codex.websocket_request',
        ])
        self.assertEqual(matched.cli_transport_evidence(text)["transport"], "unknown")

    def test_cli_transport_preserves_mixed_request_evidence_instead_of_hiding_fallback(self):
        ws = 'INFO codex_otel.log_only: event.name=codex.websocket_request'
        http = 'INFO codex_otel.log_only: event.name=codex.api_request url=https://chatgpt.com/backend-api/codex/responses'
        self.assertEqual(matched.cli_transport_evidence(ws)["transport"], "websocket")
        self.assertEqual(matched.cli_transport_evidence(ws.replace("codex_otel.log_only", "codex_otel::events::session_telemetry"))["transport"], "websocket")
        self.assertEqual(matched.cli_transport_evidence(http)["transport"], "http")
        result = matched.cli_transport_evidence(ws + '\n' + http)
        self.assertEqual(result["transport"], "mixed")
        self.assertEqual(result["request_event_counts"], {"websocket": 1, "http": 1})

    def test_accounting_refusal_cannot_loop_or_retry_unrelated_failure(self):
        experiment = matched.Experiment.__new__(matched.Experiment)
        with patch.object(matched.time, "sleep") as wait:
            experiment._rates_once = Mock(side_effect=[matched.RateLimitRefused(), {"credits": "unchanged"}])
            self.assertEqual(experiment.rates("check"), {"credits": "unchanged"})
            wait.assert_called_once_with(60)
            self.assertEqual(experiment._rates_once.call_count, 2)
        with patch.object(matched.time, "sleep") as wait:
            experiment._rates_once = Mock(side_effect=[matched.RateLimitRefused(), matched.RateLimitRefused()])
            with self.assertRaises(matched.RateLimitRefused):
                experiment.rates("check")
            wait.assert_called_once_with(60)
            self.assertEqual(experiment._rates_once.call_count, 2)
        with patch.object(matched.time, "sleep") as wait:
            experiment._rates_once = Mock(side_effect=RuntimeError("timeout"))
            with self.assertRaisesRegex(RuntimeError, "timeout"):
                experiment.rates("check")
            wait.assert_not_called()
            experiment._rates_once.assert_called_once_with("check")

    def test_credit_top_up_cannot_hide_account_movement(self):
        for before, after in [(None, "1"), ("1", None), ("1", "0"), ("1", "2")]:
            with self.subTest(before=before, after=after), self.assertRaises(RuntimeError):
                matched.checked_credit(before, after)
        matched.checked_credit("62500.0", "62500.0000000000")

    def test_fisher_tail_does_not_reverse_ego_worse_direction(self):
        self.assertEqual(matched.fisher_ego_worse(1, 1, 0, 1), .5)
        self.assertEqual(matched.fisher_ego_worse(0, 1, 1, 1), 1)
        self.assertEqual(matched.fisher_ego_worse(0, 600, 0, 600), 1)
        # With two misses among four requests, the three fixed-margin
        # allocations have probabilities 1/6, 4/6, 1/6.
        self.assertAlmostEqual(matched.fisher_ego_worse(2, 2, 0, 2), 1 / 6)
        self.assertAlmostEqual(matched.fisher_ego_worse(1, 2, 1, 2), 5 / 6)

    def test_nonzero_cost_stops_even_below_the_launch_budget(self):
        row = {"kind": "UsageUpdated", "cost_delta": {"cost": "known", "amount": {"currency": "USD", "amount": "0.000001"}}}
        with self.assertRaisesRegex(RuntimeError, "nonzero ego subscription cost"):
            matched.checked_cost([row])
        row["cost_delta"]["amount"]["amount"] = "0"
        self.assertEqual(matched.checked_cost([row]), "0")


if __name__ == "__main__":
    unittest.main()
