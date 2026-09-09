#!/usr/bin/env python3
"""Offline assertions for the frozen R0/D0/D1/D2 corpus. No RPC or mutation."""
import json
import sys
from collections import Counter
from pathlib import Path

base = Path(sys.argv[1])
checks = []
rounds = {}
groups = {}
for name in ("round0", "round0-retry1", "diagnostic0", "diagnostic1", "diagnostic2"):
    records = [json.loads(line) for line in (base / name / "raw.jsonl").read_text().splitlines()]
    rounds[name] = {"attempts": len(records), "transport_errors": sum("transport_error" in r for r in records),
                    "rpc_errors": sum("error" in r.get("response", {}) for r in records)}
    groups[name] = {}
    for line, record in enumerate(records, 1):
        record["evidence"] = f"{name}/raw.jsonl:{line}"
        groups[name].setdefault(record["case"], {}).setdefault(record["side"], []).append(record)
    if name != "round0":
        checks.append({"name": name + "/transport-and-id", "status": "PASS" if all(
            r.get("http_status") == 200 and r.get("id_matches") is True
            and r["response"].get("jsonrpc") == "2.0" for r in records) else "FAIL"})


def record(case, side, n=0, phase="round0-retry1"):
    return groups[phase][case][side][n]


def result(case, side, n=0, phase="round0-retry1"):
    return record(case, side, n, phase)["response"]["result"]


def check(name, actual, expected, note="", evidence=None):
    checks.append({"name": name, "status": "PASS" if actual == expected else "FAIL",
                   "actual": actual, "expected": expected, "note": note, "evidence": evidence})


def gap(name, status, note):
    checks.append({"name": name, "status": status, "note": note})


def w_calls(case, phase="round0-retry1"):
    return result(case, "writer", phase=phase)[0]["calls"]


def l_calls(case, phase="round0-retry1"):
    return result(case, "leafage", phase=phase)["results"]


def root(tx):
    roots = [t for t in tx["traces"] if t["parent_trace_id"] == ""]
    assert len(roots) == 1
    return roots[0]


def normalized_leaf_trace(tx):
    by_id = {t["id"]: t for t in tx["traces"]}
    assert len(by_id) == len(tx["traces"])
    paths = {}

    def path(t):
        if t["id"] in paths:
            return paths[t["id"]]
        paths[t["id"]] = path(by_id[t["parent_trace_id"]]) + [t["pos_in_parent_trace"]] if t["parent_trace_id"] else []
        return paths[t["id"]]

    tx_id = root(tx)["tx_id"]
    assert all(t["tx_id"] == tx_id for t in tx["traces"])
    assert all(e["tx_id"] == tx_id and e["parent_trace_id"] in by_id for e in tx["events"])
    return [{"path": path(t), "from": t["from_addr"], "to": t["to_addr"], "input": t["input"],
             "output": t["output"], "value": int(t["value"], 16), "gas": t["gas_limit"],
             "gas_used": t["gas_used"], "type": t["type"], "call_type": t["call_type"].lower()}
            for t in tx["traces"]]


def normalized_writer_trace(traces):
    return [{"path": t["traceAddress"], "from": t["action"]["from"], "to": t["action"]["to"],
             "input": t["action"]["input"], "output": t["result"]["output"],
             "value": int(t["action"]["value"], 16), "gas": int(t["action"]["gas"], 16),
             "gas_used": int(t["result"]["gasUsed"], 16), "type": t["type"], "call_type": t["action"]["callType"]}
            for t in traces]


B = 15818173
HASH = "0x7f174676dd04917baf908fbe449c5210bec930175db34cc42303f790034e022f"
for side in ("writer", "leafage"):
    check("identity/chain-id/" + side, int(result("identity", side), 16), 5042)
    check("identity/tip/" + side, int(result("identity", side, 2), 16), B + 1)
    check("anchor-before/" + side, result("header-" + str(B), side)["hash"], HASH)
    check("anchor-after/" + side, result("anchor-after", side)["hash"], HASH)
gap("identity/clientVersion", "BLOCKED", "Leafage web3_clientVersion=-32601; deployment image digest provides identity instead, not a new Arc defect")
header_keys = ("number", "hash", "parentHash", "stateRoot", "timestamp", "baseFeePerGas", "gasLimit", "gasUsed", "miner", "mixHash", "receiptsRoot", "transactionsRoot")
for n in (0, 1, B - 1, B, B + 1):
    case = "header-" + str(n)
    check(case, {k: result(case, "leafage").get(k) for k in header_keys},
          {k: result(case, "writer").get(k) for k in header_keys})
for case in groups["round0-retry1"]:
    if case.startswith(("state-", "storage-")):
        check(case, result(case, "leafage"), result(case, "writer"))
portfolio = [r["response"]["result"] for r in groups["round0-retry1"]["portfolio"]["writer"]]
portfolio[-1] = "0x" + format(int(portfolio[-1], 16), "064x")
for idx in range(2):
    calls = result("portfolio", "leafage", idx)["results"]
    check(f"portfolio-{idx}/outputs", [c["result"] for c in calls], portfolio)
    check(f"portfolio-{idx}/status", [(c["code"], c["err"]) for c in calls], [(0, "")] * 6)
    check(f"portfolio-{idx}/anchor", result("portfolio", "leafage", idx)["stats"]["block_hash"], HASH)
for name, gas in (("native", 21000), ("usdc", 34020)):
    check("estimate-" + name, [int(result("estimate-" + name, side), 16) for side in ("writer", "leafage")], [gas, gas])
    sufficient = result("estimate-sufficient-" + name, "leafage", phase="diagnostic0")["results"][0]
    check("estimate-sufficient-" + name, (sufficient["code"], sufficient["result"]),
          (0, result("estimate-sufficient-" + name, "writer", phase="diagnostic0")))
gap("estimate-boundaries", "NOT_RUN", "No insufficient-gas/binary-search boundary proof in R0")
check("multicall-isolation", [(c["code"], c["result"]) for c in l_calls("multicall-isolation")],
      [(0, result("multicall-isolation", "writer", i)) for i in range(2)])
check("persistence", [int(result("persistence", side), 16) for side in ("writer", "leafage")], [0, 0])

for case in ("simulate-getter", "simulate-sequence", "simulate-nonce"):
    writer = w_calls(case)
    leaf = l_calls(case)
    check(case + "/observed-results", [(c["code"] == 0, root(c)["output"], c["gas_used"]) for c in leaf],
          [(c["status"] == "0x1", c["returnData"], int(c["gasUsed"], 16)) for c in writer],
          "Field equality only; general simulation oracle is BLOCKED")
    for idx, (w, l) in enumerate(zip(writer, leaf)):
        check(case + f"/events-{idx}", [(e["contract_id"], [e["selector"]] + e["topics"], e["data"]) for e in l["events"]],
              [(e["address"], e["topics"], e["data"]) for e in w["logs"]])
check("simulate-getter/trace", normalized_leaf_trace(l_calls("simulate-getter")[0]),
      normalized_writer_trace(result("trace-capability", "writer")["trace"]), "Same B state, getter-specific call tree including all gas and parent paths")
for i in range(2):
    check(f"simulate-sequence/trace-{i}", normalized_leaf_trace(l_calls("simulate-sequence")[i]),
          normalized_writer_trace(result("trace-callMany-sequence", "writer", phase="diagnostic0")[i]["trace"]))
check("debug-capability/output", result("debug-capability", "writer")["output"], portfolio[0], "Capability probe; full debug tree is not an extra acceptance case")
gap("simulation/stateDiff-assets", "NOT_RUN", "Writer stateDiff collected; no complete Leafage balance/nonce/storage asset-delta assertion")
gap("simulation/nonce-validation", "NOT_RUN", "Explicit nonce=0xffff is ignored/disabled in query paths; success is not nonce-validation coverage")

def env_words(data):
    assert data.startswith("0x") and len(data) == 514
    return ["0x" + data[2 + 64 * i:2 + 64 * (i + 1)] for i in range(8)]


check("call-environment/default", env_words(l_calls("call-environment")[0]["result"]), env_words(result("call-environment", "writer")),
      "Raw FAIL retained: only BASEFEE differs (Leafage 20gwei, writer zero); generic gasPrice=0 call semantics, outside Arc fixes")
check("call-environment/positive-fee", env_words(l_calls("positive-fee-environment", "diagnostic0")[0]["result"]),
      env_words(result("positive-fee-environment", "writer", phase="diagnostic0")))
check("simulation-environment/default", env_words(root(l_calls("simulate-environment")[0])["output"]),
      env_words(w_calls("simulate-environment")[0]["returnData"]), "Raw difference; B vs appended B+1 is not equivalent execution")
check("simulation-environment/corrected-overrides", (env_words(root(l_calls("matched-simulation-environment", "diagnostic1")[0])["output"]), l_calls("matched-simulation-environment", "diagnostic1")[0]["gas_used"]),
      (env_words(w_calls("matched-simulation-environment", "diagnostic1")[0]["returnData"]), int(w_calls("matched-simulation-environment", "diagnostic1")[0]["gasUsed"], 16)))
check("simulation-system-history/output", root(l_calls("matched-system-history", "diagnostic2")[0])["output"],
      w_calls("matched-system-history", "diagnostic2")[0]["returnData"], "BLOCKED oracle: writer applies pre-execution system changes; matching environment does not match storage")
check("simulation-system-history/gas", l_calls("matched-system-history", "diagnostic2")[0]["gas_used"], int(w_calls("matched-system-history", "diagnostic2")[0]["gasUsed"], 16))
check("D0/writer-same-block-rejected", record("simulate-at-B", "writer", phase="diagnostic0")["response"]["error"]["code"], -38020)
gap("D0/malformed-map-key", "FAIL", "Test request bug, not node: hex blockHash key rejected -32602. Original preserved; D1 decimal key succeeds")
failed = l_calls("simulate-failure")
check("simulation-failure/first-status-output-gas", (failed[0]["code"], root(failed[0])["output"], failed[0]["gas_used"], failed[0]["events"]), (-39000, "0x", 28525, []))
check("simulation-failure/fill", failed[1], failed[0], "Contract observation only; second item not executed and excluded from execution count")
gap("simulation-failure/sequence-oracle", "BLOCKED", "Writer continues second call; Leafage fills previous failure. Generic policy difference, not new Arc defect")
gap("simulation-failure/error-message", "FAIL", "Writer execution reverted; Leafage err empty. Known generic formatting difference; not accepted as exact equality")
check("simulation-unfunded/raw", l_calls("simulate-unfunded")[0]["code"] == 0, "error" not in record("simulate-unfunded", "writer")["response"],
      "Writer -38014; Leafage disables balance validation. Not an equivalent oracle for this request")
gap("simulation/general-oracle", "BLOCKED", "Environment corrected, but system processing and balance validation still differ; no layer promotion")
gap("layer1-and-real-transactions", "NOT_RUN", "R0 does not meet promotion gate; unique real transactions sampled/executed/completed=0/0/0")
gap("zero7-zero8-canonical", "NOT_RUN", "Future mainnet activation; user scheduled post-Sep10 testing, not a current defect")

report = {"rounds": rounds, "counts_are_assertions_not_cases": True,
          "assertion_statuses": dict(Counter(c["status"] for c in checks)), "unique_real_transactions": 0,
          "promotion_allowed": False, "checks": checks}
(base / "assertions.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps({k: v for k, v in report.items() if k != "checks"}, indent=2))
for c in checks:
    if c["status"] != "PASS":
        print(c["status"], c["name"], c.get("note", ""))
