#!/usr/bin/env python3
"""Compare matching benchmark scenarios without enforcing performance thresholds."""
import json
import sys

with open(sys.argv[1], encoding="utf-8") as file:
    base = json.load(file)
with open(sys.argv[2], encoding="utf-8") as file:
    candidate = json.load(file)

print("\n## Comparison with pull request base\n")
print("Informational only. Both binaries ran sequentially on the same shared runner; "
      "noise, run order and the local server can affect the result. "
      "A positive change means higher throughput.\n")
base_cases = {case["name"]: case for case in base["cases"]}
print("| Scenario | Base median req/s | PR median req/s | Change |")
print("| --- | ---: | ---: | ---: |")
for case in candidate["cases"]:
    previous = base_cases.get(case["name"])
    if (previous is None or previous["concurrency"] != case["concurrency"]
            or previous["options"] != case["options"]
            or base["metadata"]["requests"] != candidate["metadata"]["requests"]):
        print(f"| {case['name']} | — | — | Not comparable |")
        continue
    old = previous["median_requests_per_second"]
    new = case["median_requests_per_second"]
    change = f"{(new / old - 1) * 100:+.1f}%" if old > 0 else "N/A"
    print(f"| {case['name']} | {old:.0f} | {new:.0f} | {change} |")
