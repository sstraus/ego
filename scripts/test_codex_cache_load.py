"""Offline numeric guard for the opt-in subscription evidence collector."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "cache_load", Path(__file__).with_name("codex-cache-load.py"))
load = importlib.util.module_from_spec(spec)
spec.loader.exec_module(load)


class CacheLoadBounds(unittest.TestCase):
    def test_exact_upper_bound_catches_false_rare_miss_certification(self):
        # Catches using the observed miss fraction as its uncertainty bound.
        self.assertAlmostEqual(load.upper_bound(1, 880), .00537930, places=7)
        self.assertGreater(load.upper_bound(1, 880), .005)
        self.assertAlmostEqual(load.upper_bound(0, 598), 1 - .05 ** (1 / 598), places=12)
        self.assertLess(load.upper_bound(0, 598), .005)
        self.assertEqual(load.upper_bound(0, 0), 1)
        self.assertEqual(load.upper_bound(2, 2), 1)


if __name__ == "__main__":
    unittest.main()
