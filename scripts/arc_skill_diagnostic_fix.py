#!/usr/bin/env python3
"""D1: correct our JSON map key; retain D0 unchanged. Two read-only RPCs."""
import json
import sys
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

base = Path(sys.argv[1])
system_probe = len(sys.argv) > 2 and sys.argv[2] == "--system-probe"
cases = [r for r in json.loads((base / "diagnostic0/manifest.json").read_text())
         if r[0] == "matched-simulation-environment"]
assert len(cases) == 2
for _, side, request in cases:
    if side == "leafage":
        hashes = request["params"][2]["blockHash"]
        request["params"][2]["blockHash"] = {str(int(k, 16)): v for k, v in hashes.items()}
    if system_probe:
        calls = request["params"][0]["blockStateCalls"][0]["calls"] if side == "writer" else request["params"][0]
        assert len(calls) == 1
        calls[0]["to"] = "0x0000f90827f1c53a10cb7a02335b175320002935"
        calls[0]["data"] = "0x" + format(15818173, "064x")
        calls[0]["value"] = "0x0"
if system_probe:
    cases = [("matched-system-history", side, request) for _, side, request in cases]
target = base / ("diagnostic2" if system_probe else "diagnostic1")
target.mkdir(exist_ok=False)
(target / "manifest.json").write_text(json.dumps(cases, indent=2) + "\n")
started = time.monotonic()
for idx, (case, side, request) in enumerate(cases, 1):
    request.update(jsonrpc="2.0", id=idx)
    endpoint = "http://127.0.0.1:" + ("39545" if side == "writer" else "49545")
    record = {"case": case, "side": side, "endpoint": endpoint, "request": request,
              "started_at": datetime.now(timezone.utc).isoformat()}
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
(target / "run.json").write_text(json.dumps({"requests": len(cases), "duration_seconds": time.monotonic() - started}, indent=2) + "\n")
