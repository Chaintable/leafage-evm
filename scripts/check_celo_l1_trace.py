#!/usr/bin/env python3
"""Compare transaction blockfiles with a cached native Celo trace oracle.

Use --state-diff to also compare the decoded StateDiff, including epoch
finalization. Native Go map iteration can change its RLP list order.
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


def decode_diff(encoded):
    data = bytes.fromhex(encoded[2:] if encoded.startswith("0x") else encoded)
    def item(pos):
        prefix = data[pos]
        if prefix < 128:
            return data[pos:pos+1].hex(), pos+1
        is_list = prefix >= 192
        short = 192 if is_list else 128
        if prefix < short+56:
            length, start = prefix-short, pos+1
        else:
            size = prefix-(short+55)
            length = int.from_bytes(data[pos+1:pos+1+size], "big")
            start = pos+1+size
        end = start+length
        if end > len(data):
            raise ValueError("truncated StateDiff RLP")
        if not is_list:
            return data[start:end].hex(), end
        out = []
        while start < end:
            value, start = item(start)
            out.append(value)
        if start != end:
            raise ValueError("invalid StateDiff RLP list")
        return out, end
    result, end = item(0)
    if end != len(data) or len(result) != 6:
        raise ValueError("invalid BlockStorageDiff RLP")
    for index in range(2, 6):
        result[index].sort()
    for account in result[4]:
        account[1].sort()
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", default="http://127.0.0.1:8665")
    parser.add_argument("--candidate")
    parser.add_argument("--state-diff", action="store_true")
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
            actual = rpc(args.candidate, number, {"stateDiff": args.state_diff})
            row["seconds"] = round(time.monotonic() - start, 3)
            (args.output / f"leafage-{number}.json").write_text(json.dumps(actual) + "\n")
            diffs = []
            for key in ("block_file", "header", "validation_hash"):
                diffs.extend(differences(expected[key], actual[key], key))
            if args.state_diff:
                diffs.extend(differences(decode_diff(expected["state_diff"]),
                    decode_diff(actual["state_diff"]), "state_diff"))
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
