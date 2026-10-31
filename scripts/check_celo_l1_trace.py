#!/usr/bin/env python3
"""Compare transaction blockfiles with a cached native Celo trace oracle.

Only block_file, header and validation_hash are compared. Leafage does not
produce the consensus state_diff, which includes epoch finalization.
"""
import argparse
import json
from pathlib import Path
import time
import urllib.request


def rpc(url, number, options):
    request = urllib.request.Request(url, json.dumps({"jsonrpc": "2.0", "id": 1,
        "method": "trace_debankBlock", "params": [hex(number), options]}).encode(),
        {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=1800) as response:
        result = json.load(response)
    if "error" in result:
        raise RuntimeError(str(result["error"]))
    return result["result"]


def differences(expected, actual, path=""):
    if type(expected) is not type(actual):
        yield {"path": path, "expected": expected, "actual": actual}
    elif isinstance(expected, dict):
        for key in sorted(expected.keys() | actual.keys()):
            if key not in expected or key not in actual:
                yield {"path": f"{path}.{key}", "expected": expected.get(key), "actual": actual.get(key)}
            else:
                yield from differences(expected[key], actual[key], f"{path}.{key}")
    elif isinstance(expected, list):
        if len(expected) != len(actual):
            yield {"path": path + ".length", "expected": len(expected), "actual": len(actual)}
        for index, (left, right) in enumerate(zip(expected, actual)):
            yield from differences(left, right, f"{path}[{index}]")
    elif expected != actual:
        yield {"path": path, "expected": expected, "actual": actual}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", default="http://127.0.0.1:8665")
    parser.add_argument("--candidate")
    parser.add_argument("--blocks", required=True, help="comma-separated decimal heights")
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.cache.mkdir(parents=True, exist_ok=True)
    args.output.mkdir(parents=True, exist_ok=True)
    report = []
    for number in map(int, args.blocks.split(",")):
        cache = args.cache / f"after-{number}.json"
        if cache.exists():
            expected = json.loads(cache.read_text())
        else:
            start = time.monotonic()
            expected = rpc(args.oracle, number, {"mode": "archive"})
            cache.write_text(json.dumps(expected) + "\n")
            print(json.dumps({"oracle": number, "seconds": round(time.monotonic() - start, 3)}), flush=True)
        if not args.candidate:
            continue
        start = time.monotonic()
        row = {"block": number}
        try:
            actual = rpc(args.candidate, number, {"stateDiff": False})
            row["seconds"] = round(time.monotonic() - start, 3)
            (args.output / f"leafage-{number}.json").write_text(json.dumps(actual) + "\n")
            diffs = []
            for key in ("block_file", "header", "validation_hash"):
                diffs.extend(differences(expected[key], actual[key], key))
            row.update(ok=not diffs, differences=diffs,
                txs=len(actual["block_file"]["txs"]),
                traces=len(actual["block_file"]["traces"]),
                error_traces=len(actual["block_file"]["error_traces"]))
        except Exception as error:
            row.update(ok=False, error=str(error))
        report.append(row)
        summary = {k: v for k, v in row.items() if k != "differences"}
        summary["differences"] = len(row.get("differences", []))
        print(json.dumps(summary), flush=True)
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    raise SystemExit(any(not row["ok"] for row in report))


if __name__ == "__main__":
    main()
