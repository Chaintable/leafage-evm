#!/usr/bin/env python3
"""Compare `trace_debankBlock` outputs of bsc-x and leafage.

    compare_debank_block.py --expected http://bsc-x:8545 --actual http://leafage:8574 \
        --blocks 100,101,105-110 [--dump-dir out/]

Either endpoint may instead be a directory holding `<name>-<block>.json`
files (`--expected-dir` / `--actual-dir`). Rows are matched by id; the
fields that legitimately differ (process_start_timestamp) are ignored, and
state diffs are compared as sets of entries (bsc-x orders them randomly),
ignoring the accounts and codes geth lists without a net change.
"""

import argparse
import collections
import json
import os
import sys
import urllib.request

IGNORED_BLOCK_FIELDS = {"process_start_timestamp"}


def rpc(url, block):
    body = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": "trace_debankBlock", "params": [hex(block)]}
    ).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=600) as resp:
        reply = json.load(resp)
    if "error" in reply:
        raise RuntimeError(f"{url} block {block}: {reply['error']}")
    return reply["result"]


def load(url, directory, prefix, block):
    if directory:
        with open(os.path.join(directory, f"{prefix}-{block}.json")) as f:
            data = json.load(f)
        return data.get("result", data)
    return rpc(url, block)


def norm(value):
    # Hex quantities may differ only in leading zeros / case.
    if isinstance(value, str) and value.startswith("0x"):
        return value.lower()
    return value


def diff_rows(kind, expected, actual, out):
    exp = {row["id"]: row for row in expected}
    act = {row["id"]: row for row in actual}
    if len(exp) != len(expected):
        out[f"{kind}: duplicate ids in expected"].append(len(expected) - len(exp))
    for rid in exp.keys() - act.keys():
        out[f"{kind}: missing in actual"].append(rid)
    for rid in act.keys() - exp.keys():
        out[f"{kind}: extra in actual"].append(rid)
    for rid in exp.keys() & act.keys():
        e, a = exp[rid], act[rid]
        for field in sorted(set(e) | set(a)):
            if norm(e.get(field)) != norm(a.get(field)):
                out[f"{kind}.{field}"].append((rid, e.get(field), a.get(field)))
    same_order = [r["id"] for r in expected] == [r["id"] for r in actual]
    if not same_order and exp.keys() == act.keys():
        out[f"{kind}: order differs"].append(True)


def rlp_decode(data, i=0):
    prefix = data[i]
    if prefix < 0x80:
        return data[i:i + 1], i + 1
    if prefix < 0xb8:
        n = prefix - 0x80
        return data[i + 1:i + 1 + n], i + 1 + n
    if prefix < 0xc0:
        width = prefix - 0xb7
        n = int.from_bytes(data[i + 1:i + 1 + width], "big")
        start = i + 1 + width
        return data[start:start + n], start + n
    if prefix < 0xf8:
        n, start = prefix - 0xc0, i + 1
    else:
        width = prefix - 0xf7
        n = int.from_bytes(data[i + 1:i + 1 + width], "big")
        start = i + 1 + width
    items, j = [], start
    while j < start + n:
        item, j = rlp_decode(data, j)
        items.append(item)
    return items, start + n


def state_diff_entries(hex_diff):
    """`BlockStorageDiff` RLP as {(section, key): value} for a set comparison."""
    if not hex_diff or hex_diff == "0x":
        return {}
    root, parent, accounts, deleted, storage, codes = rlp_decode(bytes.fromhex(hex_diff[2:]))[0]
    entries = {("root", ""): (root.hex(), parent.hex())}
    for account in accounts:
        entries[("account", account[0].hex())] = tuple(field.hex() for field in account[1:])
    for address in deleted:
        entries[("deleted", address.hex())] = True
    for account in storage:
        for slot, value in account[1]:
            entries[("storage", account[0].hex() + "/" + slot.hex())] = value.hex()
    for code in codes:
        entries[("code", code[0].hex())] = code[1].hex()
    return entries


def compare(expected, actual):
    out = collections.defaultdict(list)
    for field in sorted(set(expected["header"]) | set(actual["header"])):
        if norm(expected["header"].get(field)) != norm(actual["header"].get(field)):
            out[f"header.{field}"].append(
                (expected["header"].get(field), actual["header"].get(field))
            )
    eb, ab = expected["block_file"], actual["block_file"]
    for field in sorted(set(eb["block"]) | set(ab["block"])):
        if field in IGNORED_BLOCK_FIELDS:
            continue
        if norm(eb["block"].get(field)) != norm(ab["block"].get(field)):
            out[f"block.{field}"].append((eb["block"].get(field), ab["block"].get(field)))
    for kind in ("txs", "traces", "error_traces", "events", "error_events"):
        diff_rows(kind, eb.get(kind) or [], ab.get(kind) or [], out)
    es, as_ = set(eb.get("storage_contracts") or []), set(ab.get("storage_contracts") or [])
    if es != as_:
        out["storage_contracts"].append((sorted(es - as_), sorted(as_ - es)))
    if expected.get("validation_hash") != actual.get("validation_hash"):
        out["validation_hash"].append((expected.get("validation_hash"), actual.get("validation_hash")))
    if norm(expected.get("state_diff")) != norm(actual.get("state_diff")):
        exp = state_diff_entries(norm(expected.get("state_diff")))
        act = state_diff_entries(norm(actual.get("state_diff")))
        for key in exp.keys() - act.keys():
            # geth also lists accounts (and EIP-7702 code) written without a
            # net change; leafage diffs carry net changes only.
            if key[0] in ("account", "code"):
                continue
            out[f"state_diff.{key[0]}: missing in actual"].append(key[1])
        for key in act.keys() - exp.keys():
            out[f"state_diff.{key[0]}: extra in actual"].append(key[1])
        for key in exp.keys() & act.keys():
            if exp[key] != act[key]:
                out[f"state_diff.{key[0]}"].append((key[1], exp[key], act[key]))
        # Same entries in another order is not a difference: bsc-x emits
        # them in Go map iteration order.
    return out


def parse_blocks(spec):
    blocks = []
    for part in spec.split(","):
        if "-" in part:
            lo, hi = part.split("-")
            blocks.extend(range(int(lo), int(hi) + 1))
        else:
            blocks.append(int(part))
    return blocks


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--expected")
    parser.add_argument("--actual")
    parser.add_argument("--expected-dir")
    parser.add_argument("--actual-dir")
    parser.add_argument("--expected-prefix", default="bscx")
    parser.add_argument("--actual-prefix", default="leafage")
    parser.add_argument("--blocks", required=True)
    parser.add_argument("--dump-dir")
    parser.add_argument("--show", type=int, default=3)
    args = parser.parse_args()

    totals = collections.Counter()
    clean = 0
    blocks = parse_blocks(args.blocks)
    for block in blocks:
        try:
            expected = load(args.expected, args.expected_dir, args.expected_prefix, block)
            actual = load(args.actual, args.actual_dir, args.actual_prefix, block)
        except Exception as err:  # noqa: BLE001
            print(f"block {block}: ERROR {err}")
            totals["errors"] += 1
            continue
        if args.dump_dir:
            for name, data in ((args.expected_prefix, expected), (args.actual_prefix, actual)):
                with open(os.path.join(args.dump_dir, f"{name}-{block}.json"), "w") as f:
                    json.dump(data, f, indent=1)
        out = compare(expected, actual)
        bf = actual["block_file"]
        sizes = " ".join(f"{k}={len(bf.get(k) or [])}" for k in
                         ("txs", "traces", "error_traces", "events", "error_events"))
        if not out:
            clean += 1
            print(f"block {block}: identical ({sizes})")
            continue
        print(f"block {block}: {len(out)} kinds of difference ({sizes})")
        for key, items in sorted(out.items()):
            totals[key] += len(items)
            print(f"  {key}: {len(items)}")
            for item in items[: args.show]:
                print(f"    {item}")
    print(f"\n{clean}/{len(blocks)} blocks identical")
    for key, count in sorted(totals.items()):
        print(f"  {key}: {count}")
    return 0 if clean == len(blocks) else 1


if __name__ == "__main__":
    sys.exit(main())
