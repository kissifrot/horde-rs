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
print("| Workload | Base median | PR median | Unit | Change |")
print("| --- | ---: | ---: | --- | ---: |")
for case in candidate["cases"]:
    previous = base_cases.get(case["name"])
    if (previous is None or previous["concurrency"] != case["concurrency"]
            or previous["options"] != case["options"]
            or previous.get("path", "/") != case.get("path", "/")
            or previous.get("unit", "requests/s") != case.get("unit", "requests/s")
            or previous.get("scenario_sha256") != case.get("scenario_sha256")
            or base["metadata"]["requests"] != candidate["metadata"]["requests"]
            or not base["metadata"].get("server_sha256")
            or base["metadata"].get("server_sha256") != candidate["metadata"].get("server_sha256")):
        print(f"| {case['name']} | — | — | {case.get('unit', 'requests/s')} | Not comparable |")
        continue
    old = previous.get("median_rate_per_second", previous.get("median_requests_per_second"))
    new = case.get("median_rate_per_second", case.get("median_requests_per_second"))
    change = f"{(new / old - 1) * 100:+.1f}%" if old > 0 else "N/A"
    print(f"| {case['name']} | {old:.0f} | {new:.0f} | {case.get('unit', 'requests/s')} | {change} |")
