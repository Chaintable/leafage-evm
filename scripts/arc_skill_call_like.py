#!/usr/bin/env python3
"""Bounded call-like Arc probes. Reuse reviewed local fixtures, never eth_simulateV1."""
import argparse
import hashlib
import importlib
import json
import time
import urllib.request
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
import sys

B = 15818173
BLOCK_HASH = "0x7f174676dd04917baf908fbe449c5210bec930175db34cc42303f790034e022f"
SENDER = "0x7e8f45d07f1a182fa59aa5b62012459c15309791"
EMPTY = "0x000000000000000000000000000000000000beef"
ENV_CODE = "0x43600052426020524860405241606052456080524460a0524660c052600143034060e0526101006000f3"


def compare(actual, expected, name, acceptance=None):
    return {"name": name, "status": "PASS" if actual == expected else "FAIL", "actual": actual,
            "expected": expected, "acceptance": acceptance if actual != expected else None}


def rpc_kind(response, core):
    code = response.get("error", {}).get("code")
    mapped = {-39000: "revert", -39001: "out-of-gas", -39002: "insufficient-funds", -39003: "nonce"}
    return mapped.get(code) or core.rpc_error_class(dict(response, ok="result" in response))


def revert_gas_from_trace(tx, root):
    """Frozen Osaka corpus: REVERT without authorization refund, including EIP-7623."""
    if root.get("error", "").lower() != "reverted" or tx.get("authorizationList"):
        raise ValueError("revert gas reconstruction requires REVERT without authorizationList")
    calldata = bytes.fromhex(tx.get("data", "0x")[2:])
    floor = 21_000 + 10 * sum(1 if byte == 0 else 4 for byte in calldata)
    spent = int(tx["gas"], 16) - int(root["action"]["gas"], 16) + int(root["result"]["gasUsed"], 16)
    return max(spent, floor)


def trace_acceptance(leaf, writer, core):
    actual, expected = core.normalize_leafage_traces(leaf), core.normalize_reference_traces(writer)
    # Existing shared converter may mark the call itself as suicide and omit the
    # derived Parity suicide child. Require all remaining root fields and gas exact.
    if len(actual) == 1 and len(expected) == 2 and actual[0]["kind"] == "selfdestruct" and expected[0]["kind"] == "call" and expected[1]["kind"] == "selfdestruct" and expected[1]["path"] == [0]:
        keys = ("path", "from", "to", "value", "input", "output")
        if all(actual[0][k] == expected[0][k] for k in keys) and leaf[0]["gas_limit"] == expected[0]["gas_limit"] and leaf[0]["gas_used"] == expected[0]["gas_used"]:
            return "existing shared SELFDESTRUCT trace contract; LEAFAGE_GENERIC_RPC_FOLLOWUPS.md:155"
    # Preserve raw full-tree FAIL; accept only the documented unsuccessful-child
    # projection, never arbitrary missing successful calls.
    kept, paths = [], set()
    for frame in sorted(writer, key=lambda t: t["traceAddress"]):
        path = tuple(frame["traceAddress"])
        if frame.get("error") is None and (not path or path[:-1] in paths):
            paths.add(path)
            kept.append(frame)
    if len(kept) < len(writer) and kept:
        projected = core.normalize_reference_traces(kept)
        children = {}
        for path in sorted(paths):
            if path:
                children.setdefault(path[:-1], []).append(path[-1])
        for frame in projected:
            original = tuple(frame["path"])
            frame["path"] = [children[original[:i]].index(child) for i, child in enumerate(original)]
        if actual == projected:
            return "existing failed-child omission contract; docs/todo.md:713"
    return None


def reverted_event_acceptance(case, item, core):
    if item["code"] != -39000:
        return None
    root = next(t for t in item["traces"] if not t["parent_trace_id"])
    target = root["to_addr"]
    expected = None
    if case == "log_then_revert":
        expected = [{"address": target, "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", core.abi_words(int(SENDER, 16)), core.abi_words(int(target, 16))], "data": core.abi_words(5)},
                    {"address": target, "topics": ["0x" + "11" * 32], "data": "0x" + "22" * 32}]
    elif case == "failed_create_log_revert":
        expected = [{"address": target, "topics": [core.abi_words(1)], "data": "0x"}]
    if expected is not None and core.normalize_leafage_events(item["events"]) == expected:
        return "existing top-level reverted-event retention; ARC_LEAFAGE_FEATURES_AND_RPC_ADAPTATION.md:107"
    return None


def event_acceptance(actual, expected, core, allowed_emitters=()):
    # Only the already accepted EIP-7708 visibility/emitter limitation. Ordinary logs
    # remain ordered and exact; never grant acceptance to arbitrary event differences.
    remaining = list(actual)
    for event in expected:
        if event["address"] != core.SYSTEM_ADDRESS:
            if not remaining or remaining.pop(0) != event:
                return None
        else:
            if len(event["topics"]) != 3 or event["topics"][0] != "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef" or len(event["data"]) != 66:
                return None
            if remaining and remaining[0]["topics"] == event["topics"] and remaining[0]["data"] == event["data"]:
                if remaining[0]["address"] not in {*allowed_emitters, core.SYSTEM_ADDRESS}:
                    return None
                remaining.pop(0)
    return "existing EIP-7708 inspector limitation; docs/todo.md Sep02/Sep07" if not remaining else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixtures", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--round", choices=("probe", "directed", "authority", "usdc-authority"), required=True)
    parser.add_argument("--replay-from", type=Path, help="Offline replay of exact saved requests, without any RPC")
    parser.add_argument("--fee-mode", choices=("positive", "zero"), default="positive")
    parser.add_argument("--plan-only", action="store_true")
    args = parser.parse_args()
    sys.path.insert(0, str(args.fixtures))
    core = importlib.import_module("verify_arc_queries")
    ctx = {"block_id": hex(B), "type": "Equals"}
    def call(to, data="0x", value=0):
        return core.call_request(SENDER, to, data, value=value, gas=1_000_000)
    approval = call(core.USDC, "0x095ea7b3" + EMPTY[2:].rjust(64, "0") + format(123, "064x"))
    allowance = call(core.USDC, "0xdd62ed3e" + SENDER[2:].rjust(64, "0") + EMPTY[2:].rjust(64, "0"))
    stateful = core.build_stateful_simulation_fixtures(SENDER, EMPTY, core.ARC_MAINNET_BASELINE_CREATED, B, SENDER)
    if args.round == "probe":
        cases = {"environment": [{"from": SENDER, "data": ENV_CODE, "gas": "0xf4240", "value": "0x0"}],
                 "history-H-minus-1": [call(core.HISTORY_STORAGE, "0x" + format(B - 1, "064x"))],
                 "approve-allowance": [approval, allowance],
                 "native-then-balance": [call(EMPTY, value=1), core.balance_probe_request(SENDER, EMPTY)],
                 "create-counter": stateful["sstore_sequence"],
                 "failure-fill": [call(core.USDC, "0xffffffff"), call(core.USDC, core.TOTAL_SUPPLY)]}
    elif args.round == "usdc-authority":
        # Roles were read from both nodes at B; evidence: authority-roles/raw.jsonl.
        # All changes below are ordinary simulated calls, never state overrides.
        master = "0x5a6e6899ad7387ee1aae23041e3eb29ba11140e3"
        blacklister = "0x2a2b7ff330f15f9a8c52af5fec27d775f5138467"
        recipient = "0x000000000000000000000000000000000000cafe"
        def usdc_call(sender, selector, *words):
            return dict(call(core.USDC, selector + "".join(format(n, "064x") for n in words)), **{"from": sender})
        configure = usdc_call(master, "0x4e44d956", int(EMPTY, 16), 2)
        def mint(amount, target=EMPTY):
            return usdc_call(EMPTY, "0x40c10f19", int(target, 16), amount)
        burn = usdc_call(EMPTY, "0x42966c68", 1)
        def block(target):
            return usdc_call(blacklister, "0xf9f92be4", int(target, 16))
        unblock = usdc_call(blacklister, "0x1a895266", int(EMPTY, 16))
        is_blocked = call(core.NATIVE_COIN_CONTROL, core.NCC_IS_BLOCKLISTED + EMPTY[2:].rjust(64, "0"))
        erc_balance = usdc_call(SENDER, "0x70a08231", int(EMPTY, 16))
        native_balance = core.balance_probe_request(SENDER, EMPTY)
        cases = {"usdc-mint-burn": [configure, mint(1), native_balance, erc_balance, burn, native_balance, erc_balance],
                 "usdc-mint-transfer-burn": [configure, mint(2), usdc_call(EMPTY, "0xa9059cbb", int(recipient, 16), 1), native_balance,
                                              core.balance_probe_request(SENDER, recipient), burn, native_balance],
                 "usdc-block-unblock": [block(EMPTY), is_blocked, unblock, is_blocked],
                 "usdc-blocked-mint-reject": [configure, block(recipient), mint(1, recipient), erc_balance],
                 "usdc-mint-zero-address-reject": [configure, mint(1, "0x" + "0" * 40), erc_balance],
                 "usdc-mint-zero-amount-reject": [configure, mint(0), erc_balance]}
    elif args.round == "authority":
        recipient = "0x000000000000000000000000000000000000cafe"
        def authority(to, data):
            return dict(call(to, data), **{"from": core.USDC})
        def mint(amount, target=EMPTY):
            return authority(core.NATIVE_COIN_AUTHORITY, "0x40c10f19" + target[2:].rjust(64, "0") + format(amount, "064x"))
        def burn(amount):
            return authority(core.NATIVE_COIN_AUTHORITY, "0x9dc29fac" + EMPTY[2:].rjust(64, "0") + format(amount, "064x"))
        blocklist = authority(core.NATIVE_COIN_CONTROL, "0xe5c7160b" + EMPTY[2:].rjust(64, "0"))
        unblocklist = authority(core.NATIVE_COIN_CONTROL, "0x31b23020" + EMPTY[2:].rjust(64, "0"))
        is_blocked = call(core.NATIVE_COIN_CONTROL, core.NCC_IS_BLOCKLISTED + EMPTY[2:].rjust(64, "0"))
        erc_balance = call(core.USDC, "0x70a08231" + EMPTY[2:].rjust(64, "0"))
        cases = {"authorized-mint-burn": [mint(10**12), core.balance_probe_request(SENDER, EMPTY), erc_balance, burn(10**12), core.balance_probe_request(SENDER, EMPTY), erc_balance],
                 "authorized-native-transfer": [mint(2 * 10**12), authority(core.NATIVE_COIN_AUTHORITY, "0xbeabacc8" + EMPTY[2:].rjust(64, "0") + recipient[2:].rjust(64, "0") + format(10**12, "064x")), core.balance_probe_request(SENDER, EMPTY), core.balance_probe_request(SENDER, recipient), burn(10**12), core.balance_probe_request(SENDER, EMPTY)],
                 "authorized-block-unblock": [blocklist, is_blocked, unblocklist, is_blocked],
                 "blocked-mint-reject": [blocklist, mint(10**12), erc_balance],
                 "mint-zero-address-reject": [mint(10**12, "0x" + "0" * 40)],
                 "mint-zero-amount-reject": [mint(0)]}
    else:
        cases = core.build_fixtures(SENDER, EMPTY, B)
        # This previously named fixture queried H as if executing H+1. Retain it as an
        # explicit current-height rejection, and add the valid H-1 query separately.
        cases["eip2935-current-height-reject"] = cases.pop("eip2935_parent_hash")
        cases["eip2935-previous-height"] = [call(core.HISTORY_STORAGE, "0x" + format(B - 1, "064x"))]
        cases.update(stateful)
        cases["approve-transferFrom-allowance"] = [approval,
            dict(call(core.USDC, "0x23b872dd" + SENDER[2:].rjust(64, "0") + EMPTY[2:].rjust(64, "0") + format(1, "064x")), **{"from": EMPTY}), allowance]
        cases["usdc-transfer-balances"] = [call(core.USDC, "0xa9059cbb" + EMPTY[2:].rjust(64, "0") + format(1, "064x")),
            call(core.USDC, "0x70a08231" + EMPTY[2:].rjust(64, "0")), core.balance_probe_request(SENDER, EMPTY)]
        for name, target, selector in (("nca-mint-unauthorized", core.NATIVE_COIN_AUTHORITY, "0x40c10f19"),
                                       ("nca-burn-unauthorized", core.NATIVE_COIN_AUTHORITY, "0x9dc29fac")):
            cases[name] = [call(target, selector + EMPTY[2:].rjust(64, "0") + format(1, "064x"))]
        for name, selector in (("ncc-blocklist-unauthorized", "0xe5c7160b"), ("ncc-unblocklist-unauthorized", "0x31b23020")):
            cases[name] = [call(core.NATIVE_COIN_CONTROL, selector + EMPTY[2:].rjust(64, "0"))]
        cases["system-accounting-unknown-selector"] = [call(core.SYSTEM_ACCOUNTING, "0xffffffff")]
        cases["nca-unknown-selector"] = [call(core.NATIVE_COIN_AUTHORITY, "0xffffffff")]
    fee_modes = {}
    for name, calls in cases.items():
        fee_modes[name] = "zero" if args.fee_mode == "zero" or args.round in ("authority", "usdc-authority") or any(tx.get("from", "").lower() == EMPTY for tx in calls) else "positive"
        for tx in calls:
            if "maxFeePerGas" not in tx:
                tx["gasPrice"] = "0x0" if fee_modes[name] == "zero" else "0x4a817c800"
    # Zero-price query cases explicitly request baseFee=0 on Leafage. This matches
    # writer prepare_call_env without altering H, system state or account balances.
    args.output.mkdir(parents=True, exist_ok=False)
    max_requests, max_seconds = {"probe": (40, 120), "directed": (300, 600), "authority": (80, 180), "usdc-authority": (80, 180)}[args.round]
    manifest = {"round": args.round, "comparison": "arc-call-like-v1", "base_block": B,
                "base_hash": BLOCK_HASH, "cases": cases, "fee_modes": fee_modes, "max_requests": max_requests,
                "max_seconds": max_seconds, "request_timeout_seconds": 10,
                "fixture_sha256": hashlib.sha256((args.fixtures / "verify_arc_queries.py").read_bytes()).hexdigest()}
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    if args.plan_only:
        print(json.dumps({"cases": list(cases), "max_requests": max_requests, "fee_modes": fee_modes}, indent=2))
        return
    checks, raw, count = [], [], 0
    replay = [json.loads(line) for line in (args.replay_from / "raw.jsonl").read_text().splitlines()] if args.replay_from else None
    started = time.monotonic()
    def rpc(case, side, method, params):
        nonlocal count
        if count >= max_requests or time.monotonic() - started > max_seconds:
            raise RuntimeError("round budget exhausted")
        count += 1
        request = {"jsonrpc": "2.0", "id": count, "method": method, "params": params}
        endpoint = "http://127.0.0.1:" + ("39545" if side == "writer" else "49545")
        record = {"case": case, "side": side, "endpoint": endpoint, "request": request,
                  "started_at": datetime.now(timezone.utc).isoformat()}
        if replay is not None:
            record = replay[count - 1]
            assert record["request"] == request and record["side"] == side, "offline request differs from frozen corpus"
        else:
            try:
                req = urllib.request.Request(endpoint, json.dumps(request).encode(), {"Content-Type": "application/json"})
                with urllib.request.urlopen(req, timeout=10) as response:
                    record["http_status"] = response.status
                    record["response_text"] = response.read().decode()
                record["response"] = json.loads(record["response_text"])
                record["id_matches"] = record["response"].get("id") == count
            except (OSError, ValueError) as error:
                record["transport_error"] = str(error)
        raw.append(record)
        with (args.output / "raw.jsonl").open("a") as out:
            out.write(json.dumps(record) + "\n")
        checks.append(compare(record.get("http_status") == 200 and record.get("id_matches") is True, True, f"rpc-{count}/transport-id"))
        return record.get("response", {"error": record.get("transport_error")})
    header = rpc("anchor", "writer", "eth_getBlockByNumber", [hex(B), False])["result"]
    leaf_header = rpc("anchor", "leafage", "eth_getBlockByNumber", [hex(B), False])["result"]
    next_header = rpc("anchor-next", "writer", "eth_getBlockByNumber", [hex(B + 1), False])["result"]
    if header["hash"] != BLOCK_HASH or leaf_header["hash"] != BLOCK_HASH or next_header["parentHash"] != BLOCK_HASH:
        raise RuntimeError("anchor mismatch")
    overrides = {"number": hex(B), "time": header["timestamp"], "baseFee": header["baseFeePerGas"],
                 "gasLimit": header["gasLimit"], "coinbase": header["miner"], "random": header["mixHash"]}
    for case, calls in cases.items():
        before = len(checks)
        leaf_overrides = {"baseFee": "0x0"} if fee_modes[case] == "zero" else None
        case_overrides = dict(overrides, baseFee="0x0") if fee_modes[case] == "zero" else overrides
        trace = rpc(case, "writer", "trace_callMany", [[[tx, ["trace", "stateDiff"]] for tx in calls], hex(B)])
        pre = rpc(case, "writer", "pre_traceMany", [calls, hex(B + 1), None, case_overrides])
        leaf = rpc(case, "leafage", "simulateTransactions", [calls, ctx, leaf_overrides] if leaf_overrides else [calls, ctx])
        if args.round != "probe" and len(calls) == 1:
            wcall = rpc(case, "writer", "eth_call", [calls[0], hex(B)])
            lcall = rpc(case, "leafage", "contractMultiCall", [calls, ctx, leaf_overrides, None, False, False, True])
            west = rpc(case, "writer", "eth_estimateGas", [calls[0], hex(B)])
            lest = rpc(case, "leafage", "estimateGas", [calls[0], ctx, leaf_overrides])
            if "result" in lcall:
                one = lcall["result"]["results"][0]
                checks.append(compare(core.leafage_result_error_class(one), core.rpc_error_class(dict(wcall, ok="result" in wcall)), case + "/multicall-status"))
                if "result" in wcall:
                    checks.append(compare(one["result"], wcall["result"], case + "/multicall-output"))
            else:
                checks.append({"name": case + "/multicall-rpc", "status": "FAIL", "actual": lcall, "expected": wcall})
            if "result" in west and "result" in lest:
                checks.append(compare(int(lest["result"], 16), int(west["result"], 16), case + "/estimate-exact"))
                for side, estimate in (("writer", west), ("leafage", lest)):
                    sufficient = dict(calls[0], gas=estimate["result"])
                    execution = rpc(case + "/estimate-sufficient", side, "eth_call", [sufficient, hex(B)])
                    checks.append(compare(execution.get("result"), wcall.get("result"), case + "/estimate-sufficient-" + side))
                    checks.append(compare("result" in execution, True, case + "/estimate-sufficient-status-" + side))
            else:
                checks.append(compare(rpc_kind(lest, core), rpc_kind(west, core), case + "/estimate-error-kind"))
                if rpc_kind(lest, core) == rpc_kind(west, core) == "revert":
                    lm, wm = lest["error"]["message"], west["error"]["message"]
                    acceptance = "existing empty revert message; generic RPC scope excluded" if lm == "" and wm == "execution reverted" else None
                    if lm.startswith("revert: ") and lm.removeprefix("revert: ") == wm.removeprefix("execution reverted: "):
                        acceptance = "existing revert message prefix; reason exact, generic RPC scope excluded"
                    checks.append(compare(lm.removeprefix("execution reverted: "), wm.removeprefix("execution reverted: "), case + "/estimate-revert-message", acceptance))
        if "error" in trace or "error" in pre or "error" in leaf:
            checks.append({"name": case + "/rpc-error", "status": "BLOCKED", "trace": trace, "pre": pre, "leaf": leaf})
        else:
            try:
                tt, pp, ll = trace["result"], pre["result"], leaf["result"]["results"]
                checks.append(compare([len(tt), len(pp), len(ll)], [len(calls)] * 3, case + "/lengths"))
                assert len(tt) == len(pp) == len(ll) == len(calls)
                stats = leaf["result"]["stats"]
                checks.append(compare([stats["block_num"], stats["block_hash"], stats["block_time"]], [B, BLOCK_HASH, int(header["timestamp"], 16)], case + "/stats-anchor"))
                stopped = None
                for i, (t, p, l) in enumerate(zip(tt, pp, ll)):
                    label = f"{case}/{i}"
                    if stopped is not None:
                        checks.append(compare(l, stopped, label + "/not-executed-fill"))
                        continue
                    root = next(tr for tr in t["trace"] if tr["traceAddress"] == [])
                    success = "error" not in root
                    checks.append(compare(l["code"] == 0, success, label + "/status"))
                    checks.append(compare(p.get("error") is None, success, label + "/writer-pre-status"))
                    core.leafage_event_attachments(l["traces"], l["events"])
                    if success:
                        checks.append(compare(core.normalize_reference_traces(p["trace"]), core.normalize_reference_traces(t["trace"]), label + "/writer-oracle-trace"))
                        checks.append(compare(core.normalize_leafage_traces(l["traces"]), core.normalize_reference_traces(t["trace"]), label + "/trace", trace_acceptance(l["traces"], t["trace"], core)))
                        checks.append(compare(l["gas_used"], p["gasUsed"], label + "/gas"))
                        logs, events = core.normalize_logs(p["logs"]), core.normalize_leafage_events(l["events"])
                        emitters = {a for tr in l["traces"] for a in (tr["from_addr"], tr["to_addr"])}
                        checks.append(compare(events, logs, label + "/events", event_acceptance(events, logs, core, emitters)))
                        checks.append(compare(l["err"], "", label + "/success-message"))
                    else:
                        error_kind = root["error"].lower()
                        expected_code = -39000 if "revert" in error_kind else (-39001 if "out of gas" in error_kind else -39004)
                        checks.append(compare(l["code"], expected_code, label + "/error-code"))
                        checks.append(compare(l["events"], [], label + "/reverted-events", reverted_event_acceptance(case, l, core)))
                        # This deployed trace_callMany DOES retain reverted output and
                        # frame gas. pre_traceMany instead substitutes gasUsed=0 on error.
                        # Never interpret that pre result as measured failure gas.
                        checks.append(compare(core.leafage_root_output(l), t["output"], label + "/failure-output-parity"))
                        checks.append(compare(core.normalize_leafage_traces(l["traces"])[0], core.normalize_reference_traces(t["trace"])[0], label + "/failure-root-trace"))
                        if root["error"].lower() == "reverted" and not calls[i].get("authorizationList") and root.get("result", {}).get("gasUsed") is not None:
                            reconstructed = revert_gas_from_trace(calls[i], root)
                            checks.append(compare(l["gas_used"], reconstructed, label + "/failure-gas-parity-floor"))
                        else:
                            reconstructed = None
                            checks.append({"name": label + "/failure-gas", "status": "NOT_RUN", "note": "No supported gas reconstruction for this trace/transaction"})
                        if args.round != "probe" and i == 0:
                            debug = rpc(case, "writer", "debug_traceCall", [calls[0], hex(B), {"tracer": "callTracer", "tracerConfig": {"withLog": True}}])
                            if "result" in debug and "gasUsed" in debug["result"]:
                                checks.append(compare(l["gas_used"], int(debug["result"]["gasUsed"], 16), label + "/failure-gas-debug"))
                                if reconstructed is not None:
                                    checks.append(compare(reconstructed, int(debug["result"]["gasUsed"], 16), label + "/writer-failure-gas-crosscheck"))
                            if "result" in debug and "output" in debug["result"]:
                                checks.append(compare(core.leafage_root_output(l), debug["result"]["output"], label + "/failure-output-debug"))
                        stopped = l
                checks.append(compare(stats["success"], stopped is None, case + "/stats-success"))
                expected_failure = {"usdc-blocked-mint-reject": 2, "usdc-mint-zero-address-reject": 1,
                                    "usdc-mint-zero-amount-reject": 1}.get(case)
                if expected_failure is not None:
                    first_failure = next((i for i, item in enumerate(ll) if item["code"] != 0), None)
                    checks.append(compare(first_failure, expected_failure, case + "/expected-failure-index"))
                if args.round == "probe":
                    outputs = [core.leafage_root_output(item) for item in ll]
                    expected = {"history-H-minus-1": [header["parentHash"]],
                                "approve-allowance": [core.abi_words(1), core.abi_words(123)],
                                "native-then-balance": ["0x", core.abi_words(1)],
                                "create-counter": [core.STATE_OVERRIDE_COUNTER_CODE, core.abi_words(0), core.abi_words(1)]}
                    expected["environment"] = [core.abi_words(B, int(header["timestamp"], 16), 0 if fee_modes[case] == "zero" else int(header["baseFeePerGas"], 16), int(header["miner"], 16), int(header["gasLimit"], 16), int(header["mixHash"], 16), 5042, int(header["parentHash"], 16))]
                    if case in expected:
                        checks.append(compare(outputs, expected[case], case + "/semantic-values"))
                else:
                    expected_output = core.expected_fixture_output(case, B)
                    if expected_output is not None and len(ll) == 1:
                        checks.append(compare([ll[0]["code"], core.leafage_root_output(ll[0])], [0, expected_output], case + "/fixture-semantic-value"))
                    expected_stateful = core.expected_stateful_outputs(case, B, core.ARC_MAINNET_BASELINE_CREATED)
                    if expected_stateful is not None:
                        for i, expected_value in enumerate(expected_stateful):
                            if expected_value is not None:
                                checks.append(compare(core.leafage_root_output(ll[i]), expected_value, f"{case}/{i}/fixture-semantic-value"))
                    known_sequences = {"approve-transferFrom-allowance": [core.abi_words(1), core.abi_words(1), core.abi_words(122)],
                                       "usdc-transfer-balances": [core.abi_words(1), core.abi_words(1), core.abi_words(10**12)],
                                       "sequential_native_transfer": ["0x", core.abi_words(1), "0x"],
                                       "eip2935-previous-height": [header["parentHash"]]}
                    known_sequences.update({"authorized-mint-burn": [core.abi_words(n) for n in (1, 10**12, 1, 1, 0, 0)],
                                            "authorized-native-transfer": [core.abi_words(n) for n in (1, 1, 10**12, 10**12, 1, 0)],
                                            "authorized-block-unblock": [core.abi_words(n) for n in (1, 1, 1, 0)]})
                    known_sequences.update({"usdc-mint-burn": [core.abi_words(1), core.abi_words(1), core.abi_words(10**12), core.abi_words(1), "0x", core.abi_words(0), core.abi_words(0)],
                                            "usdc-mint-transfer-burn": [core.abi_words(1), core.abi_words(1), core.abi_words(1), core.abi_words(10**12), core.abi_words(10**12), "0x", core.abi_words(0)],
                                            "usdc-block-unblock": ["0x", core.abi_words(1), "0x", core.abi_words(0)]})
                    if case in known_sequences:
                        checks.append(compare([core.leafage_root_output(item) for item in ll], known_sequences[case], case + "/semantic-values"))
            except (KeyError, TypeError, ValueError, AssertionError, StopIteration) as error:
                checks.append({"name": case + "/schema", "status": "FAIL", "error": repr(error)})
        current = checks[before:]
        print(case, dict(Counter(c["status"] for c in current)), flush=True)
        (args.output / "assertions.json").write_text(json.dumps(checks, indent=2) + "\n")
    for side in ("writer", "leafage"):
        after = rpc("anchor-after", side, "eth_getBlockByNumber", [hex(B), False])
        checks.append(compare(after.get("result", {}).get("hash"), BLOCK_HASH, "anchor-after/" + side))
        clean = rpc("persistent-allowance", side, "eth_call", [allowance, hex(B)])
        checks.append(compare(clean.get("result"), "0x" + "0" * 64, "persistent-allowance/" + side))
        if args.round in ("authority", "usdc-authority"):
            for target in (EMPTY, recipient):
                balance = rpc("persistent-balance", side, "eth_getBalance", [target, hex(B)])
                checks.append(compare(balance.get("result"), "0x0", "persistent-balance/" + side + "/" + target))
            blocked = rpc("persistent-blocklist", side, "eth_call", [is_blocked, hex(B)])
            checks.append(compare(blocked.get("result"), core.abi_words(0), "persistent-blocklist/" + side))
            if args.round == "usdc-authority":
                for label, tx in (("minter", usdc_call(SENDER, "0xaa271e1a", int(EMPTY, 16))),
                                  ("minter-allowance", usdc_call(SENDER, "0x8a6db9c3", int(EMPTY, 16))),
                                  ("recipient-blocked", call(core.NATIVE_COIN_CONTROL, core.NCC_IS_BLOCKLISTED + recipient[2:].rjust(64, "0")))):
                    clean = rpc("persistent-" + label, side, "eth_call", [tx, hex(B)])
                    checks.append(compare(clean.get("result"), core.abi_words(0), "persistent-" + label + "/" + side))
    (args.output / "assertions.json").write_text(json.dumps(checks, indent=2) + "\n")
    summary = {"request_count": count, "new_rpc_requests": count if replay is None else 0, "replay_source": str(args.replay_from) if replay is not None else None, "duration_seconds": time.monotonic() - started,
               "cases": len(cases), "assertions": dict(Counter(c["status"] for c in checks)),
               "accepted_differences": sum(c["status"] == "FAIL" and bool(c.get("acceptance")) for c in checks),
               "unresolved": [c["name"] for c in checks if c["status"] in ("FAIL", "BLOCKED") and not c.get("acceptance")],
               "unique_real_transactions": 0, "collected_entire_round": True}
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
