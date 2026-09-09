#!/usr/bin/env python3
"""D0: ten predeclared diagnostic RPCs, never an acceptance promotion."""
import copy
import json
import sys
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

base = Path(sys.argv[1])
source = json.loads((base / "round0-retry1/responses.json").read_text())
target = base / "diagnostic0"
target.mkdir(exist_ok=False)


def original(case, side):
    return copy.deepcopy(source[case][side][0]["request"])


cases = []
same_block = original("simulate-environment", "writer")
same_block["params"][0]["blockStateCalls"][0]["blockOverrides"] = {"number": "0xf15dbd", "time": "0x6a812afe"}
cases.append(("simulate-at-B", "writer", same_block))
for side in ("writer", "leafage"):
    call_env = original("call-environment", side)
    call = call_env["params"][0] if side == "writer" else call_env["params"][0][0]
    call["gasPrice"] = "0x4a817c800"
    cases.append(("positive-fee-environment", side, call_env))
cases.append(("matched-simulation-environment", "writer", original("simulate-environment", "writer")))
leaf_env = original("simulate-environment", "leafage")
leaf_env["params"].append({"number": "0xf15dbe", "time": "0x6a812b0a", "baseFeePerGas": "0x0",
    "blockHash": {"0xf15dbd": "0x7f174676dd04917baf908fbe449c5210bec930175db34cc42303f790034e022f"}})
cases.append(("matched-simulation-environment", "leafage", leaf_env))
sequence = original("simulate-sequence", "leafage")["params"][0]
cases.append(("trace-callMany-sequence", "writer", {"method": "trace_callMany", "params": [[[call, ["trace", "stateDiff"]] for call in sequence], "0xf15dbd"]}))
for name in ("native", "usdc"):
    call = original("estimate-" + name, "writer")["params"][0]
    call["gas"] = source["estimate-" + name]["writer"][0]["response"]["result"]
    cases.append(("estimate-sufficient-" + name, "writer", {"method": "eth_call", "params": [call, "0xf15dbd"]}))
    cases.append(("estimate-sufficient-" + name, "leafage", {"method": "contractMultiCall", "params": [[call], {"type": "Equals", "block_id": "0xf15dbd"}]}))
(target / "manifest.json").write_text(json.dumps(cases, indent=2) + "\n")
start = time.monotonic()
for idx, (case, side, request) in enumerate(cases, 1):
    request.update(jsonrpc="2.0", id=idx)
    endpoint = "http://127.0.0.1:" + ("39545" if side == "writer" else "49545")
    record = {"case": case, "side": side, "request": request, "started_at": datetime.now(timezone.utc).isoformat()}
    try:
        req = urllib.request.Request(endpoint, json.dumps(request).encode(), {"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=10) as response:
            record["http_status"] = response.status
            record["response_text"] = response.read().decode()
        record["response"] = json.loads(record["response_text"])
        record["id_matches"] = record["response"].get("id") == idx
    except (OSError, ValueError) as error:
        record["transport_error"] = str(error)
    with (target / "raw.jsonl").open("a") as out:
        out.write(json.dumps(record) + "\n")
    print(case, side, json.dumps(record.get("response", record.get("transport_error"))), flush=True)
print("completed", len(cases), "requests", time.monotonic() - start, "seconds")
