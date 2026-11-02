#!/usr/bin/env python3
"""Render a benchmark JSON file as a GitHub Actions job summary."""
import json
import sys

with open(sys.argv[1], encoding="utf-8") as file:
    benchmark = json.load(file)

print("## Horde performance benchmark\n")
print("Informational only: shared runners and the local server can affect results. "
      "Compare multiple runs on similar hardware; no performance threshold is enforced.\n")
print("| Workload | Concurrency | Median | Range | Unit |")
print("| --- | ---: | ---: | ---: | --- |")
for case in benchmark["cases"]:
    rates = [sample["rate_per_second"] if "rate_per_second" in sample
             else sample["report"]["requests_per_second"] for sample in case["samples"]]
    median = case.get("median_rate_per_second", case.get("median_requests_per_second"))
    print(f"| {case['name']} | {case['concurrency']} | "
          f"{median:.0f} | {min(rates):.0f}–{max(rates):.0f} | {case.get('unit', 'requests/s')} |")
for case in benchmark.get("skipped_cases", []):
    print(f"\nSkipped {case['name']}: {case['reason']}.")
print("\nDetailed samples, resource usage and environment metadata are in the "
      "`horde-benchmark-results` artifact.")
