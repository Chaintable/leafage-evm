#!/usr/bin/env python3
"""Read-only, bounded Arc comparison probes; preserve every request and response."""
import argparse
import json
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    endpoints = {"writer": "http://127.0.0.1:39545", "leafage": "http://127.0.0.1:49545"}
    height = 15818173
    block = hex(height)
    block_hash = "0x7f174676dd04917baf908fbe449c5210bec930175db34cc42303f790034e022f"
    ctx = {"block_id": block, "type": "Equals"}
    sender = "0x7e8f45d07f1a182fa59aa5b62012459c15309791"
    empty = "0x000000000000000000000000000000000000beef"
    usdc = "0x3600000000000000000000000000000000000000"
    sentinel = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    balance = "0x70a08231" + sender[2:].rjust(64, "0")
    total = {"from": sender, "to": usdc, "data": "0x18160ddd", "gas": "0x493e0", "gasPrice": "0x0"}
    noop = {"from": sender, "to": empty, "value": "0x0", "gas": "0x493e0", "gasPrice": "0x0"}
    results = {}
    count = 0
    started = time.monotonic()
    raw_path = args.output / "raw.jsonl"

    def rpc(case, side, method, params):
        nonlocal count
        count += 1
        if count > 160 or time.monotonic() - started > 240:
            raise RuntimeError("declared round safety budget exhausted")
        request = {"jsonrpc": "2.0", "id": count, "method": method, "params": params}
        record = {"case": case, "side": side, "endpoint": endpoints[side], "request": request,
                  "started_at": datetime.now(timezone.utc).isoformat()}
        start = time.monotonic()
        try:
            req = urllib.request.Request(endpoints[side], json.dumps(request).encode(),
                                         {"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=10) as response:
                record["http_status"] = response.status
                record["response_text"] = response.read().decode()
            response = json.loads(record["response_text"])
            record["response"] = response
            record["id_matches"] = response.get("id") == count
        except (OSError, ValueError) as error:
            record["transport_error"] = str(error)
            response = {"transport_error": str(error)}
        record["duration_ms"] = round((time.monotonic() - start) * 1000, 3)
        with raw_path.open("a") as target:
            target.write(json.dumps(record) + "\n")
        results.setdefault(case, {}).setdefault(side, []).append(record)
        print(case, side, method, "error" if "error" in response or "transport_error" in response else "result", flush=True)
        return response.get("result")

    for side in endpoints:
        for method in ("eth_chainId", "web3_clientVersion", "eth_blockNumber"):
            rpc("identity", side, method, [])
    for n in (0, 1, height - 1, height, height + 1):
        headers = [rpc(f"header-{n}", s, "eth_getBlockByNumber", [hex(n), False]) for s in endpoints]
        if n == height and any(not h or h.get("hash") != block_hash for h in headers):
            raise RuntimeError("anchor hash invalid: dependent probes must stop")

    for address in (sender, empty, usdc):
        for lm, wm in (("getAddressBalance", "eth_getBalance"), ("getAddressNonce", "eth_getTransactionCount"), ("getAddressCode", "eth_getCode")):
            case = f"state-{lm}-{address}"
            rpc(case, "writer", wm, [address, block])
            rpc(case, "leafage", lm, [address, ctx])
    for address, slot in ((usdc, "0x0"), (usdc, "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc")):
        rpc(f"storage-{slot}", "writer", "eth_getStorageAt", [address, slot, block])
        rpc(f"storage-{slot}", "leafage", "getStorageAt", [address, slot, ctx])

    calls = [dict(total, data=data) for data in ("0x18160ddd", "0x313ce567", "0x06fdde03", "0x95d89b41", balance)]
    calls.append(dict(total, to=sentinel, data=balance))
    for call in calls[:-1]:
        rpc("portfolio", "writer", "eth_call", [call, block])
    rpc("portfolio", "writer", "eth_getBalance", [sender, block])
    for _ in range(2):
        rpc("portfolio", "leafage", "contractMultiCall", [calls, ctx, None, None, False, True, False])

    probe_address = "0x1000000000000000000000000000000000000001"
    # Eight ABI words: NUMBER, TIMESTAMP, BASEFEE, COINBASE, GASLIMIT, PREVRANDAO,
    # CHAINID, and BLOCKHASH(NUMBER-1). Also usable as CREATE initcode.
    env_code = "0x43600052426020524860405241606052456080524460a0524660c052600143034060e0526101006000f3"
    env_call = dict(noop, to=probe_address, data="0x")
    override = {probe_address: {"code": env_code}}
    rpc("call-environment", "writer", "eth_call", [env_call, block, override])
    rpc("call-environment", "leafage", "contractMultiCall", [[env_call], ctx, None, override, False, False, True])

    for name, call in (("native", noop), ("usdc", total)):
        rpc(f"estimate-{name}", "writer", "eth_estimateGas", [call, block])
        rpc(f"estimate-{name}", "leafage", "estimateGas", [call, ctx])
    rpc("trace-capability", "writer", "trace_call", [total, ["trace", "stateDiff"], block])
    rpc("debug-capability", "writer", "debug_traceCall", [total, block, {"tracer": "callTracer", "tracerConfig": {"withLog": True}}])

    approval = dict(total, data="0x095ea7b3" + empty[2:].rjust(64, "0") + hex(123)[2:].rjust(64, "0"))
    allowance = dict(total, data="0xdd62ed3e" + sender[2:].rjust(64, "0") + empty[2:].rjust(64, "0"))
    creation = {key: value for key, value in dict(noop, data=env_code).items() if key != "to"}
    revert_call = dict(total, data="0xffffffff")
    scenarios = {"simulate-getter": [total], "simulate-environment": [creation],
                 "simulate-sequence": [approval, allowance], "simulate-failure": [revert_call, total],
                 "simulate-nonce": [dict(noop, nonce="0xffff")],
                 "simulate-unfunded": [dict(noop, **{"from": empty, "value": "0x1"})]}
    for case, ordered in scenarios.items():
        rpc(case, "writer", "eth_simulateV1", [{"blockStateCalls": [{"calls": ordered}], "validation": False, "traceTransfers": False}, block])
        rpc(case, "leafage", "simulateTransactions", [ordered, ctx])
    rpc("multicall-isolation", "writer", "eth_call", [approval, block])
    rpc("multicall-isolation", "writer", "eth_call", [allowance, block])
    rpc("multicall-isolation", "leafage", "contractMultiCall", [[approval, allowance], ctx, None, None, False, False, True])
    for side in endpoints:
        rpc("persistence", side, "eth_call", [allowance, block])
        rpc("anchor-after", side, "eth_getBlockByNumber", [block, False])
    (args.output / "responses.json").write_text(json.dumps(results, indent=2) + "\n")
    (args.output / "run.json").write_text(json.dumps({"round": "R0", "chain_id": 5042, "base_block": height,
        "base_hash": block_hash, "request_count": count, "duration_seconds": time.monotonic() - started,
        "completed_collection": True, "note": "Collected probes, not an automatic PASS. Evaluate assertions separately."}, indent=2) + "\n")


if __name__ == "__main__":
    main()
