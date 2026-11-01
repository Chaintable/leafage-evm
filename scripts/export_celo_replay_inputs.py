#!/usr/bin/env python3
"""Save native Celo block/receipt fixtures for replay-input verification."""
import argparse
import gzip
import json
from pathlib import Path
import urllib.request


def rpc(url, method, params):
    request = urllib.request.Request(url, json.dumps({"jsonrpc": "2.0", "id": 1,
        "method": method, "params": params}).encode(), {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)
    if "error" in result or result.get("result") is None:
        raise RuntimeError(f"{method}: {result}")
    return result["result"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpc-addr", required=True)
    parser.add_argument("--blocks", required=True, help="comma-separated decimal heights")
    parser.add_argument("--output-dir", required=True, type=Path)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    for number in map(int, args.blocks.split(",")):
        if not 0 <= number < 31_056_500:
            raise ValueError("only Celo mainnet L1 heights are supported")
        block = rpc(args.rpc_addr, "eth_getBlockByNumber", [hex(number), True])
        block_hash = block["hash"]
        receipts = rpc(args.rpc_addr, "eth_getBlockReceipts", [block_hash])
        if any(r["blockHash"] != block_hash for r in receipts):
            raise RuntimeError("receipt block hash mismatch")
        payload = {"version": 1, "block": block, "receipts": receipts}
        encoded = gzip.compress(json.dumps(payload, separators=(",", ":")).encode(), mtime=0)
        destination = args.output_dir / f"{number}-{block_hash}.json.gz"
        if destination.exists() and destination.read_bytes() != encoded:
            raise RuntimeError(f"refusing to overwrite different input {destination}")
        destination.write_bytes(encoded)
        print(json.dumps({"number": number, "hash": block_hash, "transactions": len(block["transactions"]),
            "receipts": len(receipts), "bytes": len(encoded)}), flush=True)


if __name__ == "__main__":
    main()
