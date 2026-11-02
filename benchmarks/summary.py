#!/usr/bin/env python3
"""Render a benchmark JSON file as a GitHub Actions job summary."""
import json
import sys

with open(sys.argv[1], encoding="utf-8") as file:
    benchmark = json.load(file)

print("## Horde performance benchmark\n")
print("Informational only: shared runners and the local server can affect results. "
      "Compare multiple runs on similar hardware; no performance threshold is enforced.\n")
print("| Scenario | Concurrency | Median req/s | Range req/s |")
print("| --- | ---: | ---: | ---: |")
for case in benchmark["cases"]:
    rates = [sample["report"]["requests_per_second"] for sample in case["samples"]]
    print(f"| {case['name']} | {case['concurrency']} | "
          f"{case['median_requests_per_second']:.0f} | {min(rates):.0f}–{max(rates):.0f} |")
print("\nDetailed samples, resource usage and environment metadata are in the "
      "`horde-benchmark-results` artifact.")
