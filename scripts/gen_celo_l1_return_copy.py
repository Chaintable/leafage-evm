#!/usr/bin/env python3
"""Record native Celo's precompile return-memory rules using eth_call overrides.

No transactions are sent and no persisted state is changed. Donut/Churrito
copies aliased return data after copying into caller memory; Espresso copies
before. Both identity and Celo's native transfer return their input slice.
"""
import argparse
import concurrent.futures
import json
from pathlib import Path

from check_celo_l1_rpc import rpc


def cases():
    driver = "0x" + "11" * 20
    sender = "0x" + "aa" * 20
    registry = "0x000000000000000000000000000000000000ce10"

    def push(value):
        assert 0 <= value < 256
        return "60" + f"{value:02x}"

    def call(op, to, start, size, out, length):
        return (push(length) + push(out) + push(size) + push(start) +
                ("6000" if op in ["f1", "f2"] else "") + push(to) + "5a" + op + "50")

    def case(name, height, code, overrides=None):
        overrides = dict(overrides or {})
        overrides[driver] = {"code": "0x" + code}
        return {"name": f"{height}/{name}", "params": [
            {"from": sender, "to": driver, "data": "0x", "gas": hex(1_000_000),
             "gasPrice": "0x0"}, hex(height), overrides]}

    store = "7f" + bytes(range(32)).hex() + "600052"
    result = []
    for height in [6_774_000, 8_498_434, 11_838_439, 11_838_440, 21_616_000]:
        for opcode in ["f1", "f2", "f4", "fa"]:
            for name, at, start, length, mutate in [
                ("right", 7, 0, 32, ""),
                ("left", 0, 7, 32, ""),
                ("short", 7, 0, 16, ""),
                ("separate", 64, 0, 32, ""),
                ("empty", 0, 0, 0, ""),
                ("later-write", 7, 0, 32, "60ff600053"),
            ]:
                code = (store + call(opcode, 4, start, 32, at, length) + mutate +
                        "6020600060003e60806000f3")
                result.append(case(f"identity-{opcode}-{name}", height, code))

        # Native transfer is the other precompile returning the input slice.
        # Authorize the driver through Registry, mint zero, and overlap its
        # 96-byte argument/result buffers. Extend memory without changing ABI words.
        code = ("73" + "22" * 20 + "6020526000606053" +
                call("f1", 0xfd, 0, 96, 7, 96) + "6060600060003e60806000f3")
        result.append(case("transfer-overlap", height, code, {
            registry: {"code": "0x73" + driver[2:] + "60005260206000f3"},
        }))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--oracle", default="http://127.0.0.1:8665")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    def record(case):
        case["expected"] = rpc(args.oracle, "eth_call", case["params"])
        if "result" not in case["expected"]:
            raise RuntimeError(case)
        return case

    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        result = list(pool.map(record, cases()))
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"cases": len(result), "output": str(args.output)}))


if __name__ == "__main__":
    main()
