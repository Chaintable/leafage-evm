#!/usr/bin/env python3
"""Read-only role discovery for the blocked direct-USDC authority corpus."""
import argparse
import json
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

from arc_skill_call_like import B, BLOCK_HASH, SENDER, compare


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    usdc = "0x3600000000000000000000000000000000000000"
    ncc = "0x1800000000000000000000000000000000000001"
    minter = "0xb43db544e2c27092c107639ad201b3defabcf192"
    cases = {"ncc-usdc": (ncc, "0x8e204c43" + usdc[2:].rjust(64, "0")),
             "owner": (usdc, "0x8da5cb5b"),
             "blacklister": (usdc, "0xbd102430"),
             "master-minter": (usdc, "0x35d99f35"),
             "cctp-is-minter": (usdc, "0xaa271e1a" + minter[2:].rjust(64, "0")),
             "cctp-allowance": (usdc, "0x8a6db9c3" + minter[2:].rjust(64, "0")),
             "funded-is-minter": (usdc, "0xaa271e1a" + SENDER[2:].rjust(64, "0"))}
    args.output.mkdir(parents=True, exist_ok=False)
    (args.output / "manifest.json").write_text(json.dumps({"cases": cases, "block": B,
        "hash": BLOCK_HASH, "max_requests": 18, "max_seconds": 120, "timeout": 10}, indent=2) + "\n")
    checks, raw = [], []
    started = time.monotonic()

    def rpc(case, side, method, params):
        if len(raw) >= 18 or time.monotonic() - started > 120:
            raise RuntimeError("diagnostic budget exhausted")
        request = {"jsonrpc": "2.0", "id": len(raw) + 1, "method": method, "params": params}
        record = {"case": case, "side": side, "request": request,
                  "started_at": datetime.now(timezone.utc).isoformat()}
        endpoint = "http://127.0.0.1:" + ("39545" if side == "writer" else "49545")
        try:
            req = urllib.request.Request(endpoint, json.dumps(request).encode(), {"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=10) as response:
                record["http_status"] = response.status
                record["response_text"] = response.read().decode()
            record["response"] = json.loads(record["response_text"])
        except (OSError, ValueError) as error:
            record["transport_error"] = str(error)
        raw.append(record)
        with (args.output / "raw.jsonl").open("a") as out:
            out.write(json.dumps(record) + "\n")
        response = record.get("response", {})
        checks.append(compare([record.get("http_status"), response.get("id")], [200, request["id"]], case + "/transport/" + side))
        return response

    for phase in ("before", "after"):
        for side in ("writer", "leafage"):
            header = rpc("anchor-" + phase, side, "eth_getBlockByNumber", [hex(B), False])
            if header.get("result", {}).get("hash") != BLOCK_HASH:
                raise RuntimeError("anchor unavailable or changed")
        if phase == "after":
            break
        for case, (target, data) in cases.items():
            tx = {"from": SENDER, "to": target, "data": data, "gas": "0xf4240", "gasPrice": "0x0"}
            responses = [rpc(case, side, "eth_call", [tx, hex(B)]) for side in ("writer", "leafage")]
            checks.append(compare(["result" in r for r in responses], [True, True], case + "/success"))
            checks.append(compare(responses[1].get("result"), responses[0].get("result"), case + "/output"))
            print(case, json.dumps(responses), flush=True)
    (args.output / "assertions.json").write_text(json.dumps(checks, indent=2) + "\n")
    summary = {"requests": len(raw), "duration_seconds": time.monotonic() - started,
               "failed": [c["name"] for c in checks if c["status"] != "PASS"]}
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
