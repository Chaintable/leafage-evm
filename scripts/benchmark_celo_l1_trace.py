#!/usr/bin/env python3
"""Compare complete Celo trace_debankBlock latency on warm historical blocks."""
import argparse
import json
from pathlib import Path
import statistics
import time

from check_celo_l1_trace import decode_diff, rpc


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", default="http://127.0.0.1:8665")
    parser.add_argument("--candidate", default="http://127.0.0.1:8567")
    parser.add_argument("--blocks", required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    rows = []
    for number in map(int, args.blocks.split(",")):
        expected = rpc(args.oracle, number, {"mode": "archive"})
        expected_diff = decode_diff(expected["state_diff"])
        def call(name):
            url = args.oracle if name == "native" else args.candidate
            options = {"mode": "archive"} if name == "native" else {}
            start = time.monotonic()
            result = rpc(url, number, options)
            elapsed = time.monotonic() - start
            for key in ("block_file", "header", "validation_hash"):
                assert result[key] == expected[key], (number, name, key)
            assert decode_diff(result["state_diff"]) == expected_diff, (number, name, "state_diff")
            return elapsed
        call("leafage")
        samples = {"native": [], "leafage": []}
        for repeat in range(args.repeats):
            order = ("native", "leafage") if repeat % 2 == 0 else ("leafage", "native")
            for name in order:
                samples[name].append(call(name))
        median = {name: statistics.median(values) for name, values in samples.items()}
        row = {"block": number, "txs": len(expected["block_file"]["txs"]),
            "seconds": samples, "median_seconds": median,
            "native_over_leafage": median["native"] / median["leafage"]}
        rows.append(row)
        print(json.dumps(row), flush=True)
        args.output.write_text(json.dumps({"conditions": "same host, complete outputs, warm caches, alternating order",
            "oracle": args.oracle, "candidate": args.candidate, "rows": rows}, indent=2) + "\n")


if __name__ == "__main__":
    main()
