#!/usr/bin/env python3
"""Compare Celo L1 calls against native Celo, with a reusable oracle cache.

Run next to native Celo and the candidate Leafage archive. eth_call overrides
are ephemeral; this script never sends transactions or changes node state.
"""
import argparse
import concurrent.futures
import json
from pathlib import Path
import time
import urllib.request


def rpc(url, method, params):
    request = urllib.request.Request(url, data=json.dumps({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
    }).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=180) as response:
        result = json.load(response)
    return {key: result[key] for key in ("result", "error") if key in result}


def address(value):
    return f"0x{value:040x}" if isinstance(value, int) else "0x" + value * 20


def word(value):
    return f"{value:064x}"


def cases():
    driver, target, sender = address("11"), address("22"), address("aa")
    token = "0x471ece3750da237f93b8e339c536989b8978a438"
    result = []

    def add(name, block, to, data="", overrides=None, caller=sender, gas=1_000_000):
        params = [{"from": caller, "to": to, "data": "0x" + data,
                   "gas": hex(gas), "gasPrice": "0x0"}, hex(block)]
        if overrides is not None:
            params.append(overrides)
        result.append({"name": name, "params": params})

    measure = "5a60005260206000f3"
    for block in [1, 6_773_999, 6_774_000, 11_838_439, 11_838_440,
                  21_615_999, 21_616_000, 31_056_499]:
        programs = {
            "sload-cold-warm": "6000545060005450" + measure,
            "balance-cold-warm": ("73" + target[2:] + "3150") * 2 + measure,
            "extcodehash-cold-warm": ("73" + target[2:] + "3f50") * 2 + measure,
            "sstore-set-reset": "60016000556000600055" + measure,
            "basefee": "4860005260206000f3",
            "gaslimit": "4560005260206000f3",
            "difficulty": "4460005260206000f3",
        }
        for name, code in programs.items():
            driver_override = {"code": "0x" + code}
            if name != "sstore-set-reset":
                driver_override["state"] = {"0x" + word(0): "0x" + word(2)}
            # Old Celo's full-storage override aliases committed and mutable
            # storage. Use the actual empty account for SSTORE gas checks.
            add(f"{block}/{name}", block, driver, overrides={
                driver: driver_override,
                target: {"balance": "0x7", "code": "0x00"},
            })
        add(f"{block}/epoch-size", block, address(0xf8))
        add(f"{block}/token-total-supply", block, token, "18160ddd")
        add(f"{block}/token-balance", block, token, "70a08231" + word(int(token, 16)))
        # Real GoldToken code and Registry state; the driver receives temporary
        # CELO, calls transfer(), and returns its and the recipient's balances.
        # MSTORE right-aligns the selector; use calldata offset 28.
        push_data = "63a9059cbb6000527f" + word(int(target, 16)) + "6020527f" + word(1000) + "604052"
        call = "600060006044601c600073" + token[2:] + "5af1604052"
        balances = "303160005273" + target[2:] + "3160205260606000f3"
        add(f"{block}/real-token-transfer", block, driver, overrides={
            driver: {"code": "0x" + push_data + call + balances, "balance": hex(10**18)},
            target: {"balance": "0x0"},
        })
        # Active/retired address boundaries, including Donut and Gingerbread.
        for precompile in [0xe1, 0xe2, 0xe3, 0xf3]:
            add(f"{block}/activation-{precompile:x}", block, address(precompile))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", default="http://127.0.0.1:8665")
    parser.add_argument("--candidate")
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--vectors", type=Path)
    parser.add_argument("--consensus", type=Path)
    parser.add_argument("--system-storage", type=Path, help="native system storage gas vectors")
    args = parser.parse_args()
    if args.cache.exists():
        baseline = json.loads(args.cache.read_text())
    else:
        baseline = []
        for case in cases():
            case["expected"] = rpc(args.oracle, "eth_call", case["params"])
            baseline.append(case)
            if len(baseline) % 20 == 0:
                print(f"recorded native calls: {len(baseline)}", flush=True)
        args.cache.write_text(json.dumps(baseline, indent=2) + "\n")
    if not args.candidate:
        print(f"native baseline saved: {len(baseline)} calls", flush=True)
        return
    if args.system_storage:
        baseline.extend(json.loads(args.system_storage.read_text()))
    if args.vectors:
        for vector in json.loads(args.vectors.read_text()):
            baseline.append({"name": "pure/" + vector["name"], "params": [{
                "to": address(vector["address"]), "data": "0x" + vector["input"],
                "gas": hex(99_000_000), "gasPrice": "0x0",
            }, hex(31_056_499 if vector["gingerbread"] else 10_000_000)],
                "expected": {"error": {}} if vector["error"] else {"result": "0x" + vector["expected"]}})
    if args.consensus:
        for case in json.loads(args.consensus.read_text())["calls"]:
            baseline.append({"name": "consensus/" + case["name"], "params": [{
                "to": address(case["address"]), "data": "0x" + case["input"],
                "gas": hex(10_000_000), "gasPrice": "0x0",
            }, hex(case["block"])],
                "expected": {"error": {}} if case["error"] else {"result": "0x" + case["expected"]}})
    start = time.monotonic()

    def check(case):
        actual = rpc(args.candidate, "eth_call", case["params"])
        expected = case["expected"]
        ok = (actual.get("result") == expected.get("result") and
              ("error" in actual) == ("error" in expected))
        if "error" in actual:
            message = str(actual["error"]).lower()
            if any(term in message for term in ["unavailable", "not found", "database", "internal error", "invalid params"]):
                ok = False
        return {"name": case["name"], "ok": ok, "expected": expected, "actual": actual}

    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        checked = list(pool.map(check, baseline))
    failed = [item for item in checked if not item["ok"]]
    report = {"calls": len(checked), "passed": len(checked) - len(failed),
              "failed": len(failed), "seconds": round(time.monotonic() - start, 3),
              "failures": failed, "results": checked}
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k not in ("results", "failures")}), flush=True)
    for failure in failed[:15]:
        print(json.dumps(failure), flush=True)
    raise SystemExit(bool(failed))


if __name__ == "__main__":
    main()
