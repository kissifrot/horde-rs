"""Tests for benchmark result validation and reporting."""
import copy
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from local import valid_report

DIRECTORY = Path(__file__).resolve().parent


class BenchmarkReportingTests(unittest.TestCase):
    def test_scenario_requires_all_steps_to_succeed(self):
        scenario = {"steps": [{}, {}]}
        report = {"elapsed_seconds": 1, "scenarios": {"completed": 3, "failed": 0},
                  "steps": [{"completed": 3, "failed": 0}, {"completed": 3, "failed": 0}]}
        self.assertTrue(valid_report(report, scenario, 3))
        report["steps"][1]["completed"] = 2
        self.assertFalse(valid_report(report, scenario, 3))
        report["steps"][1] = {"completed": 3, "failed": 1}
        self.assertFalse(valid_report(report, scenario, 3))

    def test_summary_and_comparison_support_both_formats(self):
        old = {"metadata": {"requests": 10, "server_sha256": "server"}, "cases": [
            {"name": "serial", "concurrency": 1, "options": [],
             "median_requests_per_second": 100, "samples": [{"report": {"requests_per_second": 100}}]}]}
        new = copy.deepcopy(old)
        new["cases"][0].update(median_rate_per_second=110, unit="requests/s")
        new["cases"].append({"name": "journey", "concurrency": 16, "options": [],
                             "median_rate_per_second": 20, "unit": "iterations/s", "scenario_sha256": "a",
                             "samples": [{"rate_per_second": 20, "report": {}}]})
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory) / "base.json"
            candidate = Path(directory) / "candidate.json"
            base.write_text(json.dumps(old))
            candidate.write_text(json.dumps(new))
            summary = subprocess.check_output([sys.executable, str(DIRECTORY / "summary.py"), str(candidate)], text=True)
            comparison = subprocess.check_output([sys.executable, str(DIRECTORY / "compare.py"), str(base), str(candidate)], text=True)
            self.assertIn("| serial | 100 | 110 | requests/s | +10.0% |", comparison)
            self.assertIn("| journey | — | — | iterations/s | Not comparable |", comparison)
            self.assertIn("| journey | 16 | 20 | 20–20 | iterations/s |", summary)
            old["cases"].append(copy.deepcopy(new["cases"][1]))
            old["cases"][1]["scenario_sha256"] = "different"
            base.write_text(json.dumps(old))
            comparison = subprocess.check_output([sys.executable, str(DIRECTORY / "compare.py"), str(base), str(candidate)], text=True)
            self.assertIn("| journey | — | — | iterations/s | Not comparable |", comparison)


if __name__ == "__main__":
    unittest.main()
