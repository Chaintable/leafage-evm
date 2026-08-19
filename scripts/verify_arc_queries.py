#!/usr/bin/env python3
"""Deterministic Arc differential checks for Leafage state and execution RPCs.

The suite deliberately uses a fixed block and compares Leafage with an Arc
archive RPC.  It covers state reads that do not need an Arc EVM, standard EVM
execution, and Arc-specific execution where a generic Ethereum EVM can return
plausible but incorrect results.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ARC_CHAIN_ID = 5042
MIN_FUNDED_BALANCE = 1 << 64
LEAFAGE_EVM_REVERT = -39000
LEAFAGE_GAS_EXHAUSTED = -39001
LEAFAGE_BALANCE_EXHAUSTED = -39002
LEAFAGE_INVALID_PARAMS = -32602
EIP7825_TX_GAS_LIMIT = 16_777_216
GAS_CAP_SUCCESS_CALLDATA_BYTES = 418_905
GAS_CAP_FAILURE_CALLDATA_BYTES = 418_906
USDC = "0x3600000000000000000000000000000000000000"
NATIVE_COIN_AUTHORITY = "0x1800000000000000000000000000000000000000"
HISTORY_STORAGE = "0x0000f90827f1c53a10cb7a02335b175320002935"
P256_PRECOMPILE = "0x0000000000000000000000000000000000000100"
SYSTEM_ADDRESS = "0xfffffffffffffffffffffffffffffffffffffffe"
INVALID_GAS_LIMIT_MESSAGE = "invalid gas limit"
SIMULATION_NEXT_BLOCK_ERROR = "simulation block number must equal state anchor + 1"
TOTAL_SUPPLY = "0x18160ddd"
ERC20_TRANSFER = "0xa9059cbb"
P256_VALID_INPUT = "0x" + "".join(
    (
        "4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4d",
        "a73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac",
        "36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d604",
        "aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff376",
        "18b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e",
    )
)
RECIPIENT_CANDIDATES = (
    "0x000000000000000000000000000000000000dead",
    "0x000000000000000000000000000000000000beef",
    "0x1111111111111111111111111111111111111111",
)
ADDRESS_RE = re.compile(r"^0x[0-9a-fA-F]{40}$")
DATA_RE = re.compile(r"^0x(?:[0-9a-fA-F]{2})*$")


class RpcCallError(RuntimeError):
    def __init__(self, code: int, message: str):
        super().__init__(f"RPC {code}: {message}")
        self.code = code
        self.message = message


class RpcTransportError(RuntimeError):
    pass


class RpcClient:
    def __init__(self, url: str, timeout: float, retries: int, interval: float):
        self.url = url
        self.timeout = timeout
        self.retries = retries
        self.interval = interval
        self.request_count = 0
        self.retry_count = 0
        self._next_id = 1

    def call(self, method: str, params: list[Any]) -> Any:
        payload = {
            "jsonrpc": "2.0",
            "id": self._next_id,
            "method": method,
            "params": params,
        }
        self._next_id += 1
        body = json.dumps(payload, separators=(",", ":")).encode()
        for attempt in range(self.retries + 1):
            if self.interval:
                time.sleep(self.interval)
            self.request_count += 1
            request = urllib.request.Request(
                self.url,
                data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            try:
                with urllib.request.urlopen(request, timeout=self.timeout) as response:
                    decoded = json.load(response)
                if not isinstance(decoded, dict):
                    raise RpcTransportError("RPC response is not an object")
                if "error" in decoded:
                    error = decoded["error"]
                    if not isinstance(error, dict):
                        raise RpcTransportError("RPC error is not an object")
                    raise RpcCallError(
                        int(error.get("code", -32603)), str(error.get("message", "RPC error"))
                    )
                if "result" not in decoded:
                    raise RpcTransportError("RPC response has no result")
                return decoded["result"]
            except RpcCallError:
                raise
            except urllib.error.HTTPError as error:
                retryable = error.code == 429 or 500 <= error.code <= 599
                if not retryable or attempt == self.retries:
                    raise RpcTransportError(f"HTTP {error.code}") from error
                delay = retry_after(error.headers) or 0.5 * (2**attempt)
            except (urllib.error.URLError, TimeoutError, OSError, json.JSONDecodeError) as error:
                if attempt == self.retries:
                    raise RpcTransportError(
                        f"transport failure ({type(error).__name__})"
                    ) from error
                delay = 0.5 * (2**attempt)
            self.retry_count += 1
            time.sleep(delay)
        raise AssertionError("unreachable")

    def capture(self, method: str, params: list[Any]) -> dict[str, Any]:
        try:
            return {"ok": True, "result": self.call(method, params)}
        except RpcCallError as error:
            return {
                "ok": False,
                "error": {"code": error.code, "message": error.message},
            }


def retry_after(headers: Any) -> float | None:
    if headers is None:
        return None
    value = headers.get("Retry-After")
    if value is None:
        return None
    try:
        return max(0.0, float(value))
    except ValueError:
        return None


def parse_block(value: str) -> int:
    if value.lower() in {"latest", "pending", "safe", "finalized", "earliest"}:
        raise argparse.ArgumentTypeError("--block must be an explicit height")
    try:
        number = int(value, 0)
    except ValueError as error:
        raise argparse.ArgumentTypeError("invalid block height") from error
    if number < 1:
        raise argparse.ArgumentTypeError("--block must be at least 1")
    return number


def parse_address(value: str) -> str:
    if not ADDRESS_RE.fullmatch(value):
        raise argparse.ArgumentTypeError("invalid 20-byte address")
    return value.lower()


def quantity(value: Any) -> int:
    if isinstance(value, bool):
        raise ValueError("boolean is not a quantity")
    if isinstance(value, int) and value >= 0:
        return value
    if isinstance(value, str) and re.fullmatch(r"0x(?:0|[1-9a-fA-F][0-9a-fA-F]*)", value):
        return int(value, 16)
    raise ValueError(f"invalid quantity: {value!r}")


def data(value: Any) -> str:
    if not isinstance(value, str) or not DATA_RE.fullmatch(value):
        raise ValueError(f"invalid hex data: {value!r}")
    return value.lower()


def address(value: Any) -> str:
    if not isinstance(value, str) or not ADDRESS_RE.fullmatch(value):
        raise ValueError(f"invalid address: {value!r}")
    return value.lower()


def block_hash(value: Any) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"0x[0-9a-fA-F]{64}", value):
        raise ValueError(f"invalid block hash: {value!r}")
    return value.lower()


def compact(value: Any, limit: int = 1000) -> Any:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":"), default=str)
    if len(encoded) <= limit:
        return value
    return encoded[:limit] + "..."


@dataclass
class Check:
    category: str
    name: str
    passed: bool
    expected: Any = None
    actual: Any = None
    note: str | None = None

    def as_dict(self) -> dict[str, Any]:
        result = {
            "category": self.category,
            "name": self.name,
            "passed": self.passed,
            "expected": compact(self.expected),
            "actual": compact(self.actual),
        }
        if self.note:
            result["note"] = self.note
        return result


@dataclass
class Report:
    block: int
    checks: list[Check] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    complete: bool = True
    clients: dict[str, str] = field(default_factory=dict)
    anchors: dict[str, Any] = field(default_factory=dict)

    def add(
        self,
        category: str,
        name: str,
        passed: bool,
        expected: Any = None,
        actual: Any = None,
        note: str | None = None,
    ) -> None:
        self.checks.append(Check(category, name, passed, expected, actual, note))

    def finish(self, leafage: RpcClient, reference: RpcClient) -> dict[str, Any]:
        passed = sum(item.passed for item in self.checks)
        failed = len(self.checks) - passed
        return {
            "schema_version": 1,
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "chain_id": hex(ARC_CHAIN_ID),
            "block": self.block,
            "clients": self.clients,
            "endpoint_types": {
                "leafage": endpoint_type(getattr(leafage, "url", "")),
                "reference": endpoint_type(getattr(reference, "url", "")),
            },
            "anchors": self.anchors,
            "complete": self.complete,
            "summary": {"checks": len(self.checks), "passed": passed, "failed": failed},
            "requests": {
                "leafage": leafage.request_count,
                "reference": reference.request_count,
                "retries": leafage.retry_count + reference.retry_count,
            },
            "checks": [item.as_dict() for item in self.checks],
            "errors": self.errors,
        }


def endpoint_type(url: str) -> str:
    parsed = urllib.parse.urlsplit(url)
    if parsed.hostname in {"127.0.0.1", "::1", "localhost"}:
        return "loopback"
    return parsed.scheme or "unknown"


def normalize_logs(logs: Any) -> list[dict[str, Any]]:
    if not isinstance(logs, list):
        raise ValueError("logs must be an array")
    normalized = []
    for log in logs:
        if not isinstance(log, dict):
            raise ValueError("log must be an object")
        topics = log.get("topics", [])
        if not isinstance(topics, list):
            raise ValueError("log topics must be an array")
        normalized.append(
            {
                "address": address(log.get("address")),
                "topics": [data(topic) for topic in topics],
                "data": data(log.get("data", "0x")),
            }
        )
    return normalized


def normalize_leafage_events(events: Any) -> list[dict[str, Any]]:
    if not isinstance(events, list):
        raise ValueError("events must be an array")
    normalized = []
    for event in events:
        if not isinstance(event, dict):
            raise ValueError("event must be an object")
        raw_selector = event.get("selector", "0x")
        selector = "0x" if raw_selector == "" else data(raw_selector)
        extra_topics = event.get("topics", [])
        if not isinstance(extra_topics, list):
            raise ValueError("event topics must be an array")
        topics = ([] if selector == "0x" else [selector]) + [
            data(topic) for topic in extra_topics
        ]
        normalized.append(
            {
                "address": address(event.get("contract_id")),
                "topics": topics,
                "data": data(event.get("data", "0x")),
            }
        )
    return normalized


def leafage_root_output(result: dict[str, Any]) -> str:
    traces = result.get("traces")
    if not isinstance(traces, list):
        raise ValueError("Leafage traces must be an array")
    roots = [trace for trace in traces if trace.get("parent_trace_id", "") == ""]
    if len(roots) != 1:
        raise ValueError(f"expected one Leafage root trace, got {len(roots)}")
    return data(roots[0].get("output", "0x"))


def call_request(
    sender: str,
    target: str,
    calldata: str = "0x",
    value: int = 0,
    gas: int | None = None,
) -> dict[str, Any]:
    request = {
        "from": sender,
        "to": target,
        "data": calldata,
        "value": hex(value),
        "gasPrice": "0x0",
    }
    if gas is not None:
        request["gas"] = hex(gas)
    return request


def eip7825_gas_cap_request(
    sender: str, recipient: str, calldata_bytes: int
) -> dict[str, Any]:
    return call_request(
        sender,
        recipient,
        "0x" + "01" * calldata_bytes,
        gas=25_000_000,
    )


def erc20_transfer_calldata(recipient: str, amount: int) -> str:
    recipient_word = "0" * 24 + address(recipient)[2:]
    amount_word = amount.to_bytes(32, "big").hex()
    return ERC20_TRANSFER + recipient_word + amount_word


def erc20_balance_of_calldata(account: str) -> str:
    return "0x70a08231" + "0" * 24 + address(account)[2:]


def uint256_calldata(value: int) -> str:
    if value < 0 or value >= 1 << 256:
        raise ValueError("uint256 calldata value is out of range")
    return "0x" + value.to_bytes(32, "big").hex()


def select_funded_account(
    reference: RpcClient,
    anchor: dict[str, Any],
    block_number: int,
    explicit: str | None,
    search_depth: int,
) -> str:
    candidates = [explicit] if explicit else [anchor.get("miner")]
    checked: set[str] = set()
    for distance in range(search_depth + 1):
        if distance and not explicit:
            block = reference.call("eth_getBlockByNumber", [hex(block_number - distance), True])
            if isinstance(block, dict):
                for transaction in block.get("transactions", []):
                    if isinstance(transaction, dict):
                        candidates.append(transaction.get("from"))
        while candidates:
            candidate = candidates.pop(0)
            if not isinstance(candidate, str) or not ADDRESS_RE.fullmatch(candidate):
                continue
            candidate = candidate.lower()
            if candidate in checked:
                continue
            checked.add(candidate)
            balance = quantity(reference.call("eth_getBalance", [candidate, hex(block_number)]))
            code = data(reference.call("eth_getCode", [candidate, hex(block_number)]))
            if balance >= MIN_FUNDED_BALANCE and code == "0x":
                return candidate
        if explicit:
            break
    raise RuntimeError(
        "no EOA with balance >= 2^64 found; pass --funded-address to exercise gas allowance"
    )


def select_empty_recipient(reference: RpcClient, block_number: int, sender: str) -> str:
    for candidate in RECIPIENT_CANDIDATES:
        if candidate == sender:
            continue
        balance = quantity(reference.call("eth_getBalance", [candidate, hex(block_number)]))
        code = data(reference.call("eth_getCode", [candidate, hex(block_number)]))
        if balance == 0 and code == "0x":
            return candidate
    raise RuntimeError("no deterministic empty recipient is empty at the selected block")


def select_balance_boundary(
    reference: RpcClient, end_block: int, search_depth: int
) -> tuple[int, str, int, int]:
    for distance in range(search_depth + 1):
        height = end_block - distance
        if height < 1:
            break
        block = reference.call("eth_getBlockByNumber", [hex(height), True])
        if not isinstance(block, dict):
            continue
        for transaction in block.get("transactions", []):
            if not isinstance(transaction, dict):
                continue
            sender = transaction.get("from")
            if not isinstance(sender, str) or not ADDRESS_RE.fullmatch(sender):
                continue
            sender = sender.lower()
            before = quantity(reference.call("eth_getBalance", [sender, hex(height - 1)]))
            after = quantity(reference.call("eth_getBalance", [sender, hex(height)]))
            if before != after:
                return height, sender, before, after
    raise RuntimeError("no transaction sender balance boundary found in the search range")


def compare_balance(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    name: str,
    account: str,
    leafage_selector: Any,
    reference_selector: Any | None = None,
) -> None:
    reference_result = reference.capture(
        "eth_getBalance", [account, reference_selector or leafage_selector]
    )
    leafage_result = leafage.capture("eth_getBalance", [account, leafage_selector])
    passed = False
    expected: Any = reference_result
    actual: Any = leafage_result
    if reference_result["ok"] and leafage_result["ok"]:
        expected = quantity(reference_result["result"])
        actual = quantity(leafage_result["result"])
        passed = expected == actual
    report.add("balance", name, passed, expected, actual)


def simulation_block_overrides(
    parent_number: int, parent_hash: str, next_block: dict[str, Any]
) -> dict[str, Any]:
    if quantity(next_block.get("number")) != parent_number + 1:
        raise ValueError("simulation anchor is not the next block")
    if block_hash(next_block.get("parentHash")) != parent_hash:
        raise ValueError("simulation block does not descend from the fixed block")
    overrides = {
        "number": hex(parent_number + 1),
        "time": next_block.get("timestamp"),
        "gasLimit": next_block.get("gasLimit"),
        "coinbase": address(next_block.get("miner")),
        "random": block_hash(next_block.get("mixHash")),
        "baseFee": next_block.get("baseFeePerGas", "0x0"),
        "blockHash": {str(parent_number): parent_hash},
    }
    quantity(overrides["time"])
    quantity(overrides["gasLimit"])
    quantity(overrides["baseFee"])
    return overrides


def run_balance_checks(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    anchor_hash: str,
    funded: str,
    empty: str,
) -> None:
    for height in (block_number - 1, block_number):
        for label, account in (("funded", funded), ("usdc", USDC), ("empty", empty)):
            compare_balance(
                report,
                leafage,
                reference,
                f"numeric.{height}.{label}",
                account,
                hex(height),
            )

    hash_selector = {"blockHash": anchor_hash, "requireCanonical": True}
    compare_balance(
        report, leafage, reference, "canonical_block_hash", funded, hash_selector
    )

    reference_head = quantity(reference.call("eth_blockNumber", []))
    stable_leafage_head: int | None = None
    observed_leafage_head: int | None = None
    leafage_latest: dict[str, Any] | None = None
    leafage_latest_hash: str | None = None
    reference_latest_hash: str | None = None
    for _attempt in range(3):
        head_before = quantity(leafage.call("eth_blockNumber", []))
        observed_leafage_head = head_before
        if head_before > reference_head:
            break
        leafage_header = leafage.call(
            "eth_getBlockByNumber", [hex(head_before), False]
        )
        reference_header = reference.call(
            "eth_getBlockByNumber", [hex(head_before), False]
        )
        if not isinstance(leafage_header, dict) or not isinstance(reference_header, dict):
            raise ValueError("stable latest header is missing")
        latest_candidate = leafage.capture("eth_getBalance", [funded, "latest"])
        head_after = quantity(leafage.call("eth_blockNumber", []))
        if head_before == head_after:
            stable_leafage_head = head_before
            leafage_latest = latest_candidate
            leafage_latest_hash = block_hash(leafage_header.get("hash"))
            reference_latest_hash = block_hash(reference_header.get("hash"))
            break
    leafage_head = stable_leafage_head if stable_leafage_head is not None else -1
    report.add(
        "anchor",
        "leafage_head_not_ahead_of_reference",
        stable_leafage_head is not None and leafage_head <= reference_head,
        f"<= {reference_head}",
        stable_leafage_head,
    )
    if stable_leafage_head is None:
        report.complete = False
        if observed_leafage_head is not None and observed_leafage_head > reference_head:
            report.errors.append(
                "reference RPC is behind Leafage; latest balance has no common oracle height"
            )
        else:
            report.errors.append("Leafage head changed during three latest-balance attempts")
    elif leafage_head <= reference_head:
        block_matches = leafage_latest_hash == reference_latest_hash
        report.add(
            "anchor",
            "latest_block_hash",
            block_matches,
            reference_latest_hash,
            leafage_latest_hash,
        )
        if not block_matches:
            report.complete = False
            report.errors.append("Leafage latest block differs from the Arc reference")
        reference_at_leafage_head = reference.capture(
            "eth_getBalance", [funded, hex(leafage_head)]
        )
        latest_matches = False
        expected_latest: Any = reference_at_leafage_head
        if (
            reference_at_leafage_head["ok"]
            and leafage_latest is not None
            and leafage_latest["ok"]
            and block_matches
        ):
            expected_latest = quantity(reference_at_leafage_head["result"])
            latest_value = quantity(leafage_latest["result"])
            latest_matches = latest_value == expected_latest
        report.add(
            "balance",
            "latest_at_stable_leafage_height",
            latest_matches,
            expected_latest,
            leafage_latest,
            "The Leafage head is sampled before and after the latest query.",
        )
    context = {"block_id": hex(block_number), "type": "Equals"}
    for label, account in (("funded", funded), ("empty", empty)):
        expected = quantity(reference.call("eth_getBalance", [account, hex(block_number)]))
        actual_result = leafage.capture("getAddressBalance", [account, context])
        actual = quantity(actual_result["result"]) if actual_result["ok"] else actual_result
        report.add(
            "balance",
            f"getAddressBalance.{label}",
            actual_result["ok"] and actual == expected,
            expected,
            actual,
        )


def run_balance_boundary_check(
    report: Report,
    leafage: RpcClient,
    height: int,
    account: str,
    before: int,
    after: int,
) -> None:
    leafage_before_result = leafage.capture("eth_getBalance", [account, hex(height - 1)])
    leafage_after_result = leafage.capture("eth_getBalance", [account, hex(height)])
    actual: Any = {
        "before": leafage_before_result,
        "after": leafage_after_result,
    }
    passed = False
    if leafage_before_result["ok"] and leafage_after_result["ok"]:
        actual = {
            "before": quantity(leafage_before_result["result"]),
            "after": quantity(leafage_after_result["result"]),
        }
        passed = actual == {"before": before, "after": after} and before != after
    report.add(
        "balance",
        "transaction_boundary_changes_at_exact_block",
        passed,
        {"height": height, "account": account, "before": before, "after": after},
        actual,
    )


def run_native_usdc_balance_relation(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    account: str,
) -> None:
    selector = hex(block_number)
    call = {
        "from": account,
        "to": USDC,
        "data": erc20_balance_of_calldata(account),
    }
    reference_native = quantity(reference.call("eth_getBalance", [account, selector]))
    leafage_native = quantity(leafage.call("eth_getBalance", [account, selector]))
    reference_call = reference.capture("eth_call", [call, selector])
    leafage_call = leafage.capture("eth_call", [call, selector, None, None])
    expected_token: Any = reference_call
    actual_token: Any = leafage_call
    passed = False
    if reference_call["ok"] and leafage_call["ok"]:
        expected_data = data(reference_call["result"])
        actual_data = data(leafage_call["result"])
        if len(expected_data) != 66 or len(actual_data) != 66:
            raise ValueError("USDC balanceOf must return a 32-byte word")
        expected_token = int(expected_data, 16)
        actual_token = int(actual_data, 16)
        passed = (
            reference_native // 10**12 == expected_token
            and leafage_native // 10**12 == actual_token
            and expected_token == actual_token
        )
    report.add(
        "balance",
        "native_18dec_matches_usdc_balanceOf_6dec",
        passed,
        {
            "native": reference_native,
            "usdc": expected_token,
            "dust": reference_native % 10**12,
        },
        {
            "native": leafage_native,
            "usdc": actual_token,
            "dust": leafage_native % 10**12,
        },
        "This relation uses eth_call and therefore also requires the Arc execution module.",
    )


def run_simulation(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    name: str,
    calls: list[dict[str, Any]],
    block_overrides: dict[str, Any],
    anchor_timestamp: int,
    anchor_hash: str,
) -> None:
    context = {"block_id": hex(block_number), "type": "Equals"}
    reference_result = reference.capture(
        "eth_simulateV1",
        [
            {
                "blockStateCalls": [
                    {"blockOverrides": block_overrides, "calls": calls}
                ],
                "validation": False,
            },
            hex(block_number),
        ],
    )
    if not reference_result["ok"]:
        report.complete = False
        report.errors.append(f"reference eth_simulateV1 failed for {name}")
        report.add(
            "fixture", f"reference_{name}_available", False, "success", reference_result
        )
        return

    try:
        reference_blocks = reference_result["result"]
        if not isinstance(reference_blocks, list):
            raise ValueError("reference simulation response is not an array")
        if len(reference_blocks) != 1 or not isinstance(reference_blocks[0], dict):
            raise ValueError("expected one eth_simulateV1 block")
        simulated_block = reference_blocks[0]
        reference_calls = simulated_block.get("calls")
        if not isinstance(reference_calls, list):
            raise ValueError("reference simulation calls are not an array")
        if len(reference_calls) != len(calls):
            raise ValueError(
                f"reference returned {len(reference_calls)} calls, expected {len(calls)}"
            )
        expected_number = quantity(block_overrides["number"])
        expected_time = quantity(block_overrides["time"])
        context_matches = (
            quantity(simulated_block.get("number")) == expected_number
            and quantity(simulated_block.get("timestamp")) == expected_time
            and quantity(simulated_block.get("gasLimit"))
            == quantity(block_overrides["gasLimit"])
            and address(simulated_block.get("miner")) == block_overrides["coinbase"]
        )
        report.add(
            "fixture",
            f"reference_{name}_block_context",
            context_matches,
            {
                "number": expected_number,
                "timestamp": expected_time,
                "gasLimit": quantity(block_overrides["gasLimit"]),
                "miner": block_overrides["coinbase"],
            },
            {
                "number": simulated_block.get("number"),
                "timestamp": simulated_block.get("timestamp"),
                "gasLimit": simulated_block.get("gasLimit"),
                "miner": simulated_block.get("miner"),
            },
        )

        expected_status = [quantity(item.get("status")) == 1 for item in reference_calls]
        expected_outputs = [
            data(item.get("returnData", "0x")) if success else None
            for item, success in zip(reference_calls, expected_status)
        ]
        expected_errors = [
            not success and item.get("error") is not None
            for item, success in zip(reference_calls, expected_status)
        ]
        expected_gas = [quantity(item.get("gasUsed")) for item in reference_calls]
        expected_logs = [normalize_logs(item.get("logs", [])) for item in reference_calls]

        if name == "p256_valid":
            expected_word = "0x" + "00" * 31 + "01"
            report.add(
                "fixture",
                "reference_p256_returns_one",
                expected_status == [True] and expected_outputs == [expected_word],
                [expected_word],
                expected_outputs,
            )
        elif name in {"usdc_total_supply", "nca_total_supply"}:
            valid_supply = (
                expected_status == [True]
                and len(expected_outputs) == 1
                and expected_outputs[0] is not None
                and len(expected_outputs[0]) == 66
                and int(expected_outputs[0], 16) > 0
            )
            report.add(
                "fixture",
                f"reference_{name}_is_nonzero_word",
                valid_supply,
                "non-zero 32-byte word",
                expected_outputs,
            )
        elif name == "sequential_native_transfer":
            system_log_per_call = [
                any(log["address"] == SYSTEM_ADDRESS for log in logs)
                for logs in expected_logs
            ]
            report.add(
                "fixture",
                "reference_native_transfers_emit_system_logs",
                expected_status == [True, True] and system_log_per_call == [True, True],
                {"status": [True, True], "system_logs": [True, True]},
                {"status": expected_status, "system_logs": system_log_per_call},
            )
        elif name == "failure_then_p256":
            report.add(
                "fixture",
                "reference_continues_after_revert",
                expected_status == [False, True],
                [False, True],
                expected_status,
            )
        elif name == "eip2935_parent_hash":
            report.add(
                "fixture",
                "reference_eip2935_exposes_parent_hash",
                expected_status == [True] and expected_outputs == [anchor_hash],
                [anchor_hash],
                expected_outputs,
                "The H+1 simulation must run the EIP-2935 pre-execution update on state H.",
            )
    except (KeyError, TypeError, ValueError) as error:
        report.complete = False
        report.errors.append(f"invalid reference simulation response for {name}: {error}")
        report.add("fixture", f"reference_{name}_schema", False, "valid response", str(error))
        return

    leafage_result = leafage.capture(
        "simulateTransactions", [calls, context, block_overrides]
    )
    if not leafage_result["ok"]:
        report.add("simulate", f"{name}.rpc", False, "success", leafage_result)
        return

    try:
        leafage_payload = leafage_result["result"]
        if not isinstance(leafage_payload, dict):
            raise ValueError("Leafage simulation response is not an object")
        leafage_calls = leafage_payload.get("results")
        if not isinstance(leafage_calls, list):
            raise ValueError("Leafage simulation calls are not an array")
        report.add(
            "simulate",
            f"{name}.result_count",
            len(leafage_calls) == len(reference_calls) == len(calls),
            len(reference_calls),
            len(leafage_calls),
        )
        if len(leafage_calls) != len(reference_calls):
            return

        actual_codes = [item.get("code") for item in leafage_calls]
        if any(isinstance(code, bool) or not isinstance(code, int) for code in actual_codes):
            raise ValueError("Leafage simulation code must be an integer")
        actual_status = [code == 0 for code in actual_codes]
        expected_leafage_status = expected_status
        expected_leafage_outputs = expected_outputs
        expected_leafage_errors = expected_errors
        expected_leafage_gas = expected_gas
        expected_leafage_logs = expected_logs
        if name == "failure_then_p256":
            expected_leafage_status = [False, False]
            expected_leafage_outputs = [None, None]
            expected_leafage_errors = [True, True]
            expected_leafage_gas = [expected_gas[0], expected_gas[0]]
            expected_leafage_logs = [expected_logs[0], expected_logs[0]]
            report.add(
                "simulate",
                "failure_then_p256.fast_stop_clones_failure",
                len(leafage_calls) == 2 and leafage_calls[0] == leafage_calls[1],
                "the first failure is cloned without executing the second request",
                leafage_calls,
                "This is the existing Leafage simulateTransactions contract; changing it "
                "requires a separate compatibility decision.",
            )
            report.add(
                "simulate",
                "failure_then_p256.error_code",
                actual_codes == [LEAFAGE_EVM_REVERT, LEAFAGE_EVM_REVERT],
                [LEAFAGE_EVM_REVERT, LEAFAGE_EVM_REVERT],
                actual_codes,
            )
        report.add(
            "simulate",
            f"{name}.status",
            actual_status == expected_leafage_status,
            expected_leafage_status,
            actual_status,
        )

        actual_outputs = [
            leafage_root_output(item) if success else None
            for item, success in zip(leafage_calls, actual_status)
        ]
        report.add(
            "simulate",
            f"{name}.return_data",
            actual_outputs == expected_leafage_outputs,
            expected_leafage_outputs,
            actual_outputs,
        )

        actual_errors = [
            not success and isinstance(item.get("err"), str) and bool(item.get("err"))
            for item, success in zip(leafage_calls, actual_status)
        ]
        report.add(
            "simulate",
            f"{name}.error_classification",
            actual_errors == expected_leafage_errors,
            expected_leafage_errors,
            actual_errors,
        )

        actual_gas = [quantity(item.get("gas_used")) for item in leafage_calls]
        report.add(
            "simulate",
            f"{name}.gas_used",
            actual_gas == expected_leafage_gas,
            expected_leafage_gas,
            actual_gas,
        )

        actual_logs = [normalize_leafage_events(item.get("events", [])) for item in leafage_calls]
        report.add(
            "simulate",
            f"{name}.logs",
            actual_logs == expected_leafage_logs,
            expected_leafage_logs,
            actual_logs,
        )

        expected_success = all(expected_status)
        stats = leafage_payload.get("stats", {})
        actual_success = stats.get("success")
        report.add(
            "simulate",
            f"{name}.aggregate_success",
            actual_success is expected_success,
            expected_success,
            actual_success,
        )
        report.add(
            "simulate",
            f"{name}.stats_block_num",
            quantity(stats.get("block_num")) == block_number,
            block_number,
            stats.get("block_num"),
            "Leafage stats identify the fixed state anchor, not the overridden execution block.",
        )
        report.add(
            "simulate",
            f"{name}.stats_block_time",
            quantity(stats.get("block_time")) == anchor_timestamp,
            anchor_timestamp,
            stats.get("block_time"),
            "Leafage stats identify the fixed state anchor, not the overridden execution block.",
        )
        report.add(
            "simulate",
            f"{name}.stats_block_hash",
            block_hash(stats.get("block_hash")) == anchor_hash,
            anchor_hash,
            stats.get("block_hash"),
            "Leafage stats identify the fixed state anchor, not the overridden execution block.",
        )
    except (KeyError, TypeError, ValueError) as error:
        report.add("simulate", f"{name}.schema", False, "valid response", str(error))


def run_simulation_wrong_height_rejection(
    report: Report,
    leafage: RpcClient,
    block_number: int,
    calls: list[dict[str, Any]],
    block_overrides: dict[str, Any],
) -> None:
    context = {"block_id": hex(block_number), "type": "Equals"}
    for label, execution_height in (
        ("current", block_number),
        ("skipped", block_number + 2),
    ):
        invalid_overrides = dict(block_overrides)
        invalid_overrides["number"] = hex(execution_height)
        actual = leafage.capture(
            "simulateTransactions", [calls, context, invalid_overrides]
        )
        error = actual.get("error") if not actual["ok"] else None
        passed = (
            isinstance(error, dict)
            and error.get("code") == LEAFAGE_INVALID_PARAMS
            and str(error.get("message", "")).lower() == SIMULATION_NEXT_BLOCK_ERROR
        )
        report.add(
            "simulate",
            f"{label}_execution_height_rejected",
            passed,
            {"code": LEAFAGE_INVALID_PARAMS, "message": SIMULATION_NEXT_BLOCK_ERROR},
            actual,
            "State H can only be used to construct the H+1 simulation environment.",
        )


def run_estimate(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    name: str,
    request: dict[str, Any],
    tolerance_bps: int,
    preserve_gas: bool = False,
    exact_reference_gas: int | None = None,
) -> None:
    if not preserve_gas:
        request = {key: value for key, value in request.items() if key != "gas"}
    context = {"block_id": hex(block_number), "type": "Equals"}
    expected = reference.capture("eth_estimateGas", [request, hex(block_number)])
    actual = leafage.capture("estimateGas", [request, context, None])
    if not expected["ok"]:
        report.complete = False
        report.errors.append(f"reference eth_estimateGas failed for {name}")
        report.add(
            "fixture",
            f"reference_{name}_estimate_available",
            False,
            "success",
            expected,
        )
        return
    if not actual["ok"]:
        report.add(
            "estimate",
            f"{name}.result",
            False,
            expected,
            actual,
            "All fixed estimate fixtures are required to succeed on the Arc reference node.",
        )
        return
    expected_gas = quantity(expected["result"])
    actual_gas = quantity(actual["result"])
    if exact_reference_gas is not None:
        report.add(
            "fixture",
            f"reference_{name}_exact_gas",
            expected_gas == exact_reference_gas,
            exact_reference_gas,
            expected_gas,
        )
    difference_gas = abs(actual_gas - expected_gas)
    difference_bps = difference_gas * 10_000 // max(expected_gas, 1)
    within_tolerance = (
        difference_gas * 10_000 <= tolerance_bps * max(expected_gas, 1)
    )
    report.add(
        "estimate",
        f"{name}.gas",
        within_tolerance,
        {"gas": expected_gas, "tolerance_bps": tolerance_bps},
        {
            "gas": actual_gas,
            "difference_gas": difference_gas,
            "difference_bps_floor": difference_bps,
        },
    )

    executable = reference.capture(
        "eth_call", [{**request, "gas": hex(actual_gas)}, hex(block_number)]
    )
    report.add(
        "estimate",
        f"{name}.reference_executes_with_leafage_limit",
        executable["ok"],
        "successful eth_call",
        executable,
    )


def run_estimate_rejection(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    name: str,
    request: dict[str, Any],
    error_class: str,
) -> None:
    context = {"block_id": hex(block_number), "type": "Equals"}
    expected = reference.capture("eth_estimateGas", [request, hex(block_number)])
    actual = leafage.capture("estimateGas", [request, context, None])
    reference_error = expected.get("error") if not expected["ok"] else None
    leafage_error = actual.get("error") if not actual["ok"] else None
    reference_message = (
        str(reference_error.get("message", "")).lower()
        if isinstance(reference_error, dict)
        else ""
    )
    if error_class == "balance":
        reference_matches = "insufficient funds" in reference_message
        leafage_matches = (
            isinstance(leafage_error, dict)
            and leafage_error.get("code") == LEAFAGE_BALANCE_EXHAUSTED
            and "insufficient funds"
            in str(leafage_error.get("message", "")).lower()
        )
        expected_leafage_code = LEAFAGE_BALANCE_EXHAUSTED
    elif error_class == "gas_allowance":
        reference_matches = "gas required exceeds allowance (0)" in reference_message
        leafage_matches = (
            isinstance(leafage_error, dict)
            and leafage_error.get("code") == LEAFAGE_GAS_EXHAUSTED
            and str(leafage_error.get("message", "")).lower()
            == INVALID_GAS_LIMIT_MESSAGE
        )
        expected_leafage_code = LEAFAGE_GAS_EXHAUSTED
    elif error_class == "gas_cap":
        reference_matches = (
            f"gas required exceeds allowance ({EIP7825_TX_GAS_LIMIT})"
            in reference_message
        )
        leafage_matches = (
            isinstance(leafage_error, dict)
            and leafage_error.get("code") == LEAFAGE_GAS_EXHAUSTED
            and str(leafage_error.get("message", "")).lower()
            == INVALID_GAS_LIMIT_MESSAGE
        )
        expected_leafage_code = LEAFAGE_GAS_EXHAUSTED
    else:
        raise ValueError(f"unknown estimate rejection class: {error_class}")
    if not expected["ok"] and not reference_matches:
        report.complete = False
        report.errors.append(
            f"reference eth_estimateGas returned the wrong error class for {name}"
        )
    report.add(
        "estimate",
        f"{name}.rejected",
        reference_matches and leafage_matches,
        expected,
        actual,
        f"The Arc reference and Leafage error class must match; expected Leafage code "
        f"{expected_leafage_code}. Unrelated RPC errors do not satisfy this check.",
    )


def build_fixtures(
    sender: str, empty: str, block_number: int
) -> dict[str, list[dict[str, Any]]]:
    gas = 1_000_000
    return {
        "usdc_total_supply": [call_request(sender, USDC, TOTAL_SUPPLY, gas=gas)],
        "nca_total_supply": [
            call_request(sender, NATIVE_COIN_AUTHORITY, TOTAL_SUPPLY, gas=gas)
        ],
        "p256_valid": [call_request(sender, P256_PRECOMPILE, P256_VALID_INPUT, gas=gas)],
        "eip2935_parent_hash": [
            call_request(
                sender,
                HISTORY_STORAGE,
                uint256_calldata(block_number),
                gas=gas,
            )
        ],
        "sequential_native_transfer": [
            call_request(sender, empty, value=1, gas=gas),
            call_request(empty, sender, value=1, gas=gas),
        ],
        "failure_then_p256": [
            call_request(
                empty,
                USDC,
                erc20_transfer_calldata(sender, 1),
                gas=gas,
            ),
            call_request(sender, P256_PRECOMPILE, P256_VALID_INPUT, gas=gas),
        ],
    }


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--block", required=True, type=parse_block)
    result.add_argument("--funded-address", type=parse_address)
    result.add_argument("--search-depth", type=int, default=128)
    result.add_argument("--gas-tolerance-bps", type=int, default=1000)
    result.add_argument("--timeout", type=float, default=30.0)
    result.add_argument("--retries", type=int, default=2)
    result.add_argument("--interval-ms", type=int, default=0)
    result.add_argument("--output", type=Path)
    return result


def write_report(payload: dict[str, Any], output: Path | None) -> None:
    encoded = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    if output:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded)


def print_summary(payload: dict[str, Any]) -> None:
    summary = payload["summary"]
    print(
        f"Arc Leafage RPC checks: {summary['passed']}/{summary['checks']} passed, "
        f"{summary['failed']} failed, complete={payload['complete']}"
    )
    for check in payload["checks"]:
        if not check["passed"]:
            print(f"FAIL {check['category']}.{check['name']}")
    for error in payload["errors"]:
        print(f"ERROR {error}")


def main(argv: list[str] | None = None) -> int:
    argument_parser = parser()
    args = argument_parser.parse_args(argv)
    leafage_rpc = os.environ.get("LEAFAGE_RPC")
    reference_rpc = os.environ.get("ARC_REFERENCE_RPC")
    if args.search_depth < 0 or args.gas_tolerance_bps < 0 or args.retries < 0:
        parser().error("search depth, tolerance, and retries must be non-negative")
    interval = max(0, args.interval_ms) / 1000
    leafage = RpcClient(leafage_rpc or "", args.timeout, args.retries, interval)
    reference = RpcClient(reference_rpc or "", args.timeout, args.retries, interval)
    report = Report(args.block)

    missing_endpoints = [
        name
        for name, value in (
            ("LEAFAGE_RPC", leafage_rpc),
            ("ARC_REFERENCE_RPC", reference_rpc),
        )
        if not value
    ]
    if missing_endpoints:
        report.complete = False
        report.errors.append(
            "missing required RPC environment variables: "
            + ", ".join(missing_endpoints)
        )
        report.add(
            "preflight",
            "rpc_endpoints_available",
            False,
            ["LEAFAGE_RPC", "ARC_REFERENCE_RPC"],
            missing_endpoints,
        )
        payload = report.finish(leafage, reference)
        write_report(payload, args.output)
        print_summary(payload)
        return 2

    try:
        leafage_version = leafage.call("web3_clientVersion", [])
        reference_version = reference.call("web3_clientVersion", [])
        if not isinstance(leafage_version, str) or not isinstance(reference_version, str):
            raise ValueError("web3_clientVersion must return a string")
        report.clients = {
            "leafage": leafage_version,
            "reference": reference_version,
        }
        leafage_chain = quantity(leafage.call("eth_chainId", []))
        reference_chain = quantity(reference.call("eth_chainId", []))
        report.add(
            "anchor",
            "chain_id",
            leafage_chain == reference_chain == ARC_CHAIN_ID,
            ARC_CHAIN_ID,
            {"leafage": leafage_chain, "reference": reference_chain},
        )
        if leafage_chain != ARC_CHAIN_ID or reference_chain != ARC_CHAIN_ID:
            raise RuntimeError("chain ID mismatch")

        reference_block = reference.call("eth_getBlockByNumber", [hex(args.block), True])
        simulation_block = reference.call(
            "eth_getBlockByNumber", [hex(args.block + 1), False]
        )
        leafage_block = leafage.call("eth_getBlockByNumber", [hex(args.block), False])
        if (
            not isinstance(reference_block, dict)
            or not isinstance(simulation_block, dict)
            or not isinstance(leafage_block, dict)
        ):
            raise RuntimeError("fixed block is missing")
        reference_hash = block_hash(reference_block.get("hash"))
        successor_hash = block_hash(simulation_block.get("hash"))
        leafage_hash = block_hash(leafage_block.get("hash"))
        report.anchors = {
            "height": args.block,
            "hash": reference_hash,
            "successor_height": args.block + 1,
            "successor_hash": successor_hash,
        }
        report.add(
            "anchor", "block_hash", leafage_hash == reference_hash, reference_hash, leafage_hash
        )
        if leafage_hash != reference_hash:
            raise RuntimeError("fixed block hash mismatch")
        block_overrides = simulation_block_overrides(
            args.block, reference_hash, simulation_block
        )
        anchor_timestamp = quantity(reference_block.get("timestamp"))

        funded = select_funded_account(
            reference, reference_block, args.block, args.funded_address, args.search_depth
        )
        empty = select_empty_recipient(reference, args.block, funded)
        boundary_height, boundary_account, boundary_before, boundary_after = (
            select_balance_boundary(reference, args.block, args.search_depth)
        )
        report.add(
            "fixture",
            "funded_eoa",
            True,
            "balance >= 2^64 and empty code",
            funded,
        )
        report.add("fixture", "empty_recipient", True, "zero balance and empty code", empty)

        run_balance_checks(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            funded,
            empty,
        )
        run_balance_boundary_check(
            report,
            leafage,
            boundary_height,
            boundary_account,
            boundary_before,
            boundary_after,
        )
        run_native_usdc_balance_relation(
            report, leafage, reference, args.block, funded
        )

        fixtures = build_fixtures(funded, empty, args.block)
        for name, calls in fixtures.items():
            run_simulation(
                report,
                leafage,
                reference,
                args.block,
                name,
                calls,
                block_overrides,
                anchor_timestamp,
                reference_hash,
            )

        run_simulation_wrong_height_rejection(
            report,
            leafage,
            args.block,
            fixtures["eip2935_parent_hash"],
            block_overrides,
        )

        for name, calls in fixtures.items():
            if name in {"failure_then_p256", "eip2935_parent_hash"}:
                continue
            estimate_name = "native_transfer" if name == "sequential_native_transfer" else name
            tolerance_bps = 0 if estimate_name == "native_transfer" else args.gas_tolerance_bps
            exact_reference_gas = 21_000 if estimate_name == "native_transfer" else None
            run_estimate(
                report,
                leafage,
                reference,
                args.block,
                estimate_name,
                calls[0],
                tolerance_bps,
                exact_reference_gas=exact_reference_gas,
            )

        run_estimate_rejection(
            report,
            leafage,
            reference,
            args.block,
            "zero_balance_value_transfer",
            call_request(empty, funded, value=1),
            "balance",
        )
        zero_balance_fee = call_request(empty, funded)
        zero_balance_fee["gasPrice"] = "0x1"
        run_estimate_rejection(
            report,
            leafage,
            reference,
            args.block,
            "zero_balance_fee_payment",
            zero_balance_fee,
            "gas_allowance",
        )
        gas_cap_success_request = eip7825_gas_cap_request(
            funded, empty, GAS_CAP_SUCCESS_CALLDATA_BYTES
        )
        run_estimate(
            report,
            leafage,
            reference,
            args.block,
            "explicit_gas_below_eip7825_cap",
            gas_cap_success_request,
            0,
            preserve_gas=True,
            exact_reference_gas=EIP7825_TX_GAS_LIMIT,
        )
        gas_cap_request = eip7825_gas_cap_request(
            funded, empty, GAS_CAP_FAILURE_CALLDATA_BYTES
        )
        run_estimate_rejection(
            report,
            leafage,
            reference,
            args.block,
            "explicit_gas_above_eip7825_cap",
            gas_cap_request,
            "gas_cap",
        )
        allowance_request = call_request(funded, USDC, TOTAL_SUPPLY)
        allowance_request["gasPrice"] = "0x1"
        run_estimate(
            report,
            leafage,
            reference,
            args.block,
            "large_balance_gas_allowance",
            allowance_request,
            args.gas_tolerance_bps,
        )

        final_reference = reference.call(
            "eth_getBlockByNumber", [hex(args.block), False]
        )
        final_leafage = leafage.call("eth_getBlockByNumber", [hex(args.block), False])
        final_reference_hash = block_hash(final_reference.get("hash"))
        final_leafage_hash = block_hash(final_leafage.get("hash"))
        report.add(
            "anchor",
            "stable_after_run",
            final_reference_hash == final_leafage_hash == reference_hash,
            reference_hash,
            {"reference": final_reference_hash, "leafage": final_leafage_hash},
        )
    except (RpcCallError, RpcTransportError, RuntimeError, TypeError, ValueError) as error:
        report.complete = False
        report.errors.append(str(error))

    payload = report.finish(leafage, reference)
    write_report(payload, args.output)
    print_summary(payload)
    if not payload["complete"]:
        return 2
    return 1 if payload["summary"]["failed"] else 0


if __name__ == "__main__":
    sys.exit(main())
