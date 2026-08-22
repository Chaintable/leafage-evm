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
ARC_MAINNET_BASELINE_BLOCK = 15_818_173
ARC_MAINNET_BASELINE_HASH = (
    "0x7f174676dd04917baf908fbe449c5210bec930175db34cc42303f790034e022f"
)
MIN_FUNDED_BALANCE = 1 << 64
LEAFAGE_EVM_REVERT = -39000
LEAFAGE_GAS_EXHAUSTED = -39001
LEAFAGE_BALANCE_EXHAUSTED = -39002
LEAFAGE_EVM_FAILED = -39004
LEAFAGE_INVALID_PARAMS = -32602
EIP7825_TX_GAS_LIMIT = 16_777_216
GAS_CAP_SUCCESS_CALLDATA_BYTES = 418_905
GAS_CAP_FAILURE_CALLDATA_BYTES = 418_906
USDC = "0x3600000000000000000000000000000000000000"
NATIVE_COIN_AUTHORITY = "0x1800000000000000000000000000000000000000"
NATIVE_COIN_CONTROL = "0x1800000000000000000000000000000000000001"
SYSTEM_ACCOUNTING = "0x1800000000000000000000000000000000000002"
PQ_PRECOMPILE = "0x1800000000000000000000000000000000000004"
PQ_SOURCE_FILE_SHA256 = (
    "3ad19c1064dc7030f777305a015c5ada899e116f7f09b2f4b59effb3aeb2c012"
)
PROTOCOL_CONFIG = "0x3600000000000000000000000000000000000001"
VALIDATOR_REGISTRY = "0x3600000000000000000000000000000000000002"
DENYLIST = "0x3600000000000000000000000000000000000004"
PERMIT2 = "0x000000000022d473030f116ddee9f6b43ac78ba3"
HISTORY_STORAGE = "0x0000f90827f1c53a10cb7a02335b175320002935"
P256_PRECOMPILE = "0x0000000000000000000000000000000000000100"
SYSTEM_ADDRESS = "0xfffffffffffffffffffffffffffffffffffffffe"
NATIVE_TOKEN_SENTINEL = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
INVALID_GAS_LIMIT_MESSAGE = "invalid gas limit"
SIMULATION_NEXT_BLOCK_ERROR = "simulation block number must equal state anchor + 1"
TOTAL_SUPPLY = "0x18160ddd"
ERC20_NAME = "0x06fdde03"
ERC20_SYMBOL = "0x95d89b41"
ERC20_DECIMALS = "0x313ce567"
ERC20_TRANSFER = "0xa9059cbb"
ERC20_ALLOWANCE = "0xdd62ed3e"
NCC_IS_BLOCKLISTED = "0x8e204c43"
SYSTEM_GET_GAS_VALUES = "0x80510815"
PROTOCOL_FEE_PARAMS = "0x9242164f"
PROTOCOL_CONSENSUS_PARAMS = "0x9fd02a36"
DENYLIST_IS_DENYLISTED = "0xe877a526"
ACTIVE_VALIDATOR_COUNT = "0xb86444b1"
PERMIT2_DOMAIN_SEPARATOR = "0x3644e515"
PQ_VERIFY = "0xbf4db8ba"
STANDARD_PRECOMPILE_VECTORS = {
    "sha256_abc": (
        "0x0000000000000000000000000000000000000002",
        "0x616263",
        "0xba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    ),
    "ripemd160_abc": (
        "0x0000000000000000000000000000000000000003",
        "0x616263",
        "0x0000000000000000000000008eb208f7e05d987a9b044a8e98c6b087f15a0bfc",
    ),
    "identity_deadbeef": (
        "0x0000000000000000000000000000000000000004",
        "0xdeadbeef",
        "0xdeadbeef",
    ),
}
IMPORTANT_ERC20_ASSETS = (
    (
        "usdc",
        USDC,
        "0x7e8f45d07f1a182fa59aa5b62012459c15309791",
        "USDC",
        "USDC",
        6,
        5_495_688_632_355,
        502_412_457,
    ),
    (
        "aworp",
        "0x26d1ffbbb8b310b090ee0536748b4adfc88ae644",
        "0xef4a72c1abfb41c6cedab62c125651850f9e1260",
        "Animus Wirex One Reward Token",
        "AWORP",
        18,
        306_380_137_240_548_911_027_800,
        1_010_741_033_000_000_000_000,
    ),
    (
        "ausd",
        "0xf5b08979251f398180385b54381ee3d6fa1bbe09",
        "0xcb77c3b7c0ea6fc352bb65d6b743918d2a1358d9",
        "Animus USD",
        "AUSD",
        18,
        39_506_257_464_037_451_088_972_200,
        228_000_000_000_000,
    ),
    (
        "agbp",
        "0xa073783b43dfbfa2a78e0ae015a82968d816f41a",
        "0x5f276b53a02ebf1afaf8284d977986f6482a46c5",
        "Animus GBP",
        "AGBP",
        18,
        569_921_466_374_000_000_000_000,
        647_429_999_999_985_579,
    ),
)
P256_VALID_INPUT = "0x" + "".join(
    (
        "4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4d",
        "a73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac",
        "36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d604",
        "aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff376",
        "18b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e",
    )
)
P256_INVALID_WRONG_HASH_INPUT = "0x3c" + P256_VALID_INPUT[4:]
STATE_OVERRIDE_COUNTER_CODE = "0x5f54805f526001015f5560205ff3"
STATE_OVERRIDE_COUNTER_CODE_HASH = (
    "0x5d16bb1f02f9afab90e1dd46e047cc45f8a51c11e8e78bef18b66a329cc0a287"
)
CONSTANT_42_RUNTIME = "0x602a60005260206000f3"
CREATE2_FACTORY_RUNTIME = "0x7f" + "00" * 32 + "600160006001f560005260206000f3"
SELFDESTRUCT_RUNTIME = "0x60003560601cff"
SELFDESTRUCT_RUNTIME_CODE_HASH = (
    "0x68f563de8ba0f64aed656adcd8bd634cd7f5adc8c4869aeab8bb4ad624a98a91"
)
LOG_THEN_REVERT_RUNTIME = (
    "0x7f"
    + "22" * 32
    + "6000527f"
    + "11" * 32
    + "60206000a160006000fd"
)
FAILED_CREATE_LOG_REVERT_INIT = "0x60015f5fa15f5ffd"
NESTED_BLOCKLIST_INIT = (
    "0x6318160ddd60e01b5f5260205f60045f600173"
    + USDC[2:]
    + "5af160205260405ff3"
)
NESTED_BLOCKLIST_OUTPUT = "0x08c379a0" + "00" * 60
ARC_MAINNET_BASELINE_FUNDED = "0x7e8f45d07f1a182fa59aa5b62012459c15309791"
ARC_MAINNET_BASELINE_CREATED = "0x117044844f1849405408614ce793f4fc398d9cfe"
ARC_MAINNET_BASELINE_CREATE2_CHILD = "0xa2a54253844aa0a2ae62e3e093d597ffd103133b"
EIP7702_AUTHORITY = "0x24c39d81dec592697628535c46a2971f873a2dcd"
EIP7702_AUTHORIZATION = {
    "chainId": hex(ARC_CHAIN_ID),
    "address": ARC_MAINNET_BASELINE_CREATED,
    "nonce": "0x0",
    "yParity": "0x0",
    "r": "0x1d3f20a0359139b445757de443f91388fd919d2f27bfbb055d094f81ebc2ad3c",
    "s": "0x05c0fd31142411f17217d33de2ffdf41e41fe0f295069a666c61ce4e71861be0",
}
EIP7702_ESTIMATE_GAS = 0xB52E
FEE_STATE_GAS_PRICE = 5
FEE_STATE_GAS_USED = 21_000
EIP1559_MAX_FEE = 20_000_001_000
EIP1559_PRIORITY_FEE = 7
EIP1559_EFFECTIVE_GAS_PRICE = 20_000_000_007
EIP1559_EFFECTIVE_GASPRICE_GUARD_INIT = (
    "0x3a6404a817c807141560115760006000f35b60006000fd"
)
ERC20_INSUFFICIENT_BALANCE_REASON = "ERC20: transfer amount exceeds balance"
CODE_BOUNDARY_HEIGHT = 15_818_121
CODE_BOUNDARY_ADDRESS = "0xa921629e64aa88237d670f19573bdc664526918c"
CODE_BOUNDARY_BEFORE_HASH = (
    "0xc17c9e36769dd39602821b77bf093832cfec1a00b53997c15ff5718a8c66492d"
)
CODE_BOUNDARY_AFTER_HASH = (
    "0xf0b713fe1346d22c497263468973551283966d2dc99c6f5e5af023674e3c0f54"
)
CODE_BOUNDARY_VALUE = (
    "0x363d3d373d3d363d7f360894a13ba1a3210667c828492db98dca3e2076cc3735"
    "a920a3ca505d382bbc545af43d6000803e6038573d6000fd5b3d6000f3"
)
STORAGE_BOUNDARY_HEIGHT = 15_817_920
STORAGE_BOUNDARY_ADDRESS = "0x03a13352ef67977d1601ec1276e9bc27c0ee7b75"
STORAGE_BOUNDARY_SLOT = "0x2"
STORAGE_BOUNDARY_BEFORE_HASH = (
    "0xaddbec9b47c559003a102c662e4d6935a6af1be0101831f54ffefb400e7334e9"
)
STORAGE_BOUNDARY_AFTER_HASH = (
    "0x42ebc800c70c1bc3146dda63e62dda66e5bd9ebdecd6f4803eaa821290e80eed"
)
STORAGE_BOUNDARY_BEFORE_VALUE = "0x" + "00" * 29 + "0b6fa8"
STORAGE_BOUNDARY_AFTER_VALUE = "0x" + "00" * 29 + "0b6fa9"
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


def storage_word(value: Any) -> str:
    normalized = data(value)
    if len(normalized) != 66:
        raise ValueError(f"storage value is not 32 bytes: {value!r}")
    return normalized


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
    mode: str = "differential"
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
            "mode": self.mode,
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


def optional_quantity(value: Any) -> int | None:
    return None if value is None else quantity(value)


def optional_address(value: Any) -> str | None:
    return None if value is None else address(value)


def canonical_trace_kind(frame_type: Any, call_type: Any = None) -> tuple[str, str]:
    raw_type = str(frame_type or "").lower()
    raw_call_type = str(call_type or "").lower()
    if raw_type in {"suicide", "selfdestruct"}:
        return "selfdestruct", "selfdestruct"
    if raw_type in {"create", "create2"}:
        # The DeBank trace schema exposes CREATE and CREATE2 as the same
        # create frame.  Parity's creationMethod is therefore not comparable.
        return "create", "create"
    if raw_type == "call":
        return "call", raw_call_type or "call"
    raise ValueError(f"unsupported trace type: {frame_type!r}")


def normalize_leafage_traces(traces: Any) -> list[dict[str, Any]]:
    """Normalize DeBank traces without relying on random ids or event positions."""

    if not isinstance(traces, list):
        raise ValueError("Leafage traces must be an array")
    indexed: dict[str, tuple[int, dict[str, Any]]] = {}
    children: dict[str, list[tuple[int, int, dict[str, Any]]]] = {}
    for order, trace in enumerate(traces):
        if not isinstance(trace, dict):
            raise ValueError("Leafage trace must be an object")
        trace_id = trace.get("id")
        parent_id = trace.get("parent_trace_id", "")
        position = trace.get("pos_in_parent_trace", 0)
        if not isinstance(trace_id, str) or not trace_id:
            raise ValueError("Leafage trace id must be non-empty")
        if trace_id in indexed:
            raise ValueError(f"duplicate Leafage trace id: {trace_id}")
        if not isinstance(parent_id, str):
            raise ValueError("Leafage parent trace id must be a string")
        if isinstance(position, bool) or not isinstance(position, int) or position < 0:
            raise ValueError("Leafage trace position must be a non-negative integer")
        if not parent_id and position != 0:
            raise ValueError("Leafage root trace position must be zero")
        indexed[trace_id] = (order, trace)
        children.setdefault(parent_id, []).append((position, order, trace))

    roots = children.get("", [])
    if len(roots) != 1:
        raise ValueError(f"expected one Leafage root trace, got {len(roots)}")
    for trace_id, (_order, trace) in indexed.items():
        parent_id = trace.get("parent_trace_id", "")
        if parent_id and parent_id not in indexed:
            raise ValueError(f"unknown Leafage parent trace id: {parent_id}")
    for parent_id, siblings in children.items():
        positions = [position for position, _order, _trace in siblings]
        if len(positions) != len(set(positions)):
            raise ValueError(f"duplicate Leafage trace position under parent {parent_id!r}")

    normalized: list[dict[str, Any]] = []
    visited: set[str] = set()

    def visit(trace: dict[str, Any], path: tuple[int, ...]) -> None:
        trace_id = trace["id"]
        if trace_id in visited:
            raise ValueError("Leafage trace graph contains a cycle")
        visited.add(trace_id)
        kind, call_type = canonical_trace_kind(trace.get("type"), trace.get("call_type"))
        gas_limit = optional_quantity(trace.get("gas_limit"))
        gas_used = optional_quantity(trace.get("gas_used"))
        if kind == "selfdestruct":
            gas_limit = None
            gas_used = None
        normalized.append(
            {
                "path": list(path),
                "kind": kind,
                "call_type": call_type,
                "from": optional_address(trace.get("from_addr")),
                "to": optional_address(trace.get("to_addr")),
                "value": optional_quantity(trace.get("value", "0x0")),
                "input": data(trace.get("input", "0x")),
                "output": data(trace.get("output", "0x")),
                "gas_limit": gas_limit,
                "gas_used": gas_used,
            }
        )
        # pos_in_parent_trace counts both calls and events.  Sorting trace
        # siblings by that field and assigning a dense call-only index matches
        # Parity traceAddress without guessing where events were emitted.
        for child_index, (_position, _order, child) in enumerate(
            sorted(children.get(trace_id, []), key=lambda item: (item[0], item[1]))
        ):
            visit(child, (*path, child_index))

    visit(roots[0][2], ())
    if len(visited) != len(indexed):
        raise ValueError("Leafage trace graph contains unreachable nodes")
    return normalized


def normalize_reference_traces(traces: Any) -> list[dict[str, Any]]:
    """Normalize Parity/OpenEthereum trace objects into the Leafage shape."""

    if not isinstance(traces, list):
        raise ValueError("reference traces must be an array")
    normalized = []
    seen_paths: set[tuple[int, ...]] = set()
    for trace in traces:
        if not isinstance(trace, dict):
            raise ValueError("reference trace must be an object")
        raw_path = trace.get("traceAddress")
        if not isinstance(raw_path, list) or any(
            isinstance(item, bool) or not isinstance(item, int) or item < 0
            for item in raw_path
        ):
            raise ValueError("reference traceAddress must contain non-negative integers")
        path = tuple(raw_path)
        if path in seen_paths:
            raise ValueError(f"duplicate reference trace path: {path}")
        seen_paths.add(path)
        action = trace.get("action")
        result = trace.get("result")
        if not isinstance(action, dict):
            raise ValueError("reference trace action must be an object")
        if result is not None and not isinstance(result, dict):
            raise ValueError("reference trace result must be an object")
        result = result or {}
        kind, call_type = canonical_trace_kind(
            trace.get("type"), action.get("callType") or action.get("creationMethod")
        )
        if kind == "create":
            target = result.get("address")
            raw_input = action.get("init", "0x")
            raw_output = result.get("code", "0x")
        elif kind == "selfdestruct":
            target = action.get("refundAddress")
            raw_input = "0x"
            raw_output = "0x"
        else:
            target = action.get("to")
            raw_input = action.get("input", "0x")
            raw_output = result.get("output", "0x")
        gas_limit = optional_quantity(action.get("gas"))
        gas_used = optional_quantity(result.get("gasUsed"))
        if kind == "selfdestruct":
            gas_limit = None
            gas_used = None
        normalized.append(
            {
                "path": list(path),
                "kind": kind,
                "call_type": call_type,
                "from": optional_address(action.get("from") or action.get("address")),
                "to": optional_address(target),
                "value": optional_quantity(action.get("value") or action.get("balance") or "0x0"),
                "input": data(raw_input),
                "output": data(raw_output),
                "gas_limit": gas_limit,
                "gas_used": gas_used,
            }
        )
    root_count = sum(not item["path"] for item in normalized)
    if root_count != 1:
        raise ValueError(f"expected exactly one reference root, got {root_count}")
    paths = {tuple(item["path"]) for item in normalized}
    for path in paths:
        if path and path[:-1] not in paths:
            raise ValueError(f"reference trace has missing parent path: {path}")
    return sorted(normalized, key=lambda item: tuple(item["path"]))


def leafage_event_attachments(
    traces: Any, events: Any
) -> list[dict[str, int | str]]:
    """Validate event parents and the shared trace/event member positions."""

    if not isinstance(traces, list) or not isinstance(events, list):
        raise ValueError("Leafage traces and events must be arrays")
    trace_ids: set[str] = set()
    trace_tx_ids: set[str] = set()
    trace_parents: list[str] = []
    occupied: set[tuple[str, int]] = set()
    for trace in traces:
        if not isinstance(trace, dict):
            raise ValueError("Leafage trace must be an object")
        trace_id = trace.get("id")
        parent_id = trace.get("parent_trace_id", "")
        position = trace.get("pos_in_parent_trace", 0)
        if not isinstance(trace_id, str) or not trace_id:
            raise ValueError("Leafage trace id must be non-empty")
        if trace_id in trace_ids:
            raise ValueError(f"duplicate Leafage trace id: {trace_id}")
        trace_ids.add(trace_id)
        trace_tx_ids.add(block_hash(trace.get("tx_id")))
        if not isinstance(parent_id, str):
            raise ValueError("Leafage parent trace id must be a string")
        if isinstance(position, bool) or not isinstance(position, int) or position < 0:
            raise ValueError("Leafage trace position must be a non-negative integer")
        if not parent_id and position != 0:
            raise ValueError("Leafage root trace position must be zero")
        trace_parents.append(parent_id)
        if parent_id:
            member = (parent_id, position)
            if member in occupied:
                raise ValueError(f"duplicate Leafage member position: {member}")
            occupied.add(member)

    for parent_id in trace_parents:
        if parent_id and parent_id not in trace_ids:
            raise ValueError(f"unknown Leafage parent trace id: {parent_id}")
    if len(trace_tx_ids) != 1:
        raise ValueError("Leafage traces must share one transaction id")
    result_tx_id = next(iter(trace_tx_ids))

    attachments: list[dict[str, int | str]] = []
    event_ids: set[str] = set()
    for event in events:
        if not isinstance(event, dict):
            raise ValueError("Leafage event must be an object")
        event_id = event.get("id")
        if not isinstance(event_id, str) or not event_id:
            raise ValueError("Leafage event id must be non-empty")
        if event_id in event_ids:
            raise ValueError(f"duplicate Leafage event id: {event_id}")
        event_ids.add(event_id)
        if block_hash(event.get("tx_id")) != result_tx_id:
            raise ValueError("Leafage event and trace transaction ids differ")
        parent_id = event.get("parent_trace_id")
        position = event.get("pos_in_parent_trace")
        if not isinstance(parent_id, str) or parent_id not in trace_ids:
            raise ValueError(f"unknown Leafage event parent trace id: {parent_id!r}")
        if isinstance(position, bool) or not isinstance(position, int) or position < 0:
            raise ValueError("Leafage event position must be a non-negative integer")
        member = (parent_id, position)
        if member in occupied:
            raise ValueError(f"duplicate Leafage member position: {member}")
        occupied.add(member)
        attachments.append(
            {"parent_trace_id": parent_id, "pos_in_parent_trace": position}
        )
    positions_by_parent: dict[str, list[int]] = {}
    for parent_id, position in occupied:
        positions_by_parent.setdefault(parent_id, []).append(position)
    for parent_id, positions in positions_by_parent.items():
        ordered = sorted(positions)
        if ordered != list(range(len(ordered))):
            raise ValueError(
                f"non-contiguous Leafage member positions under parent {parent_id!r}: "
                f"{ordered}"
            )
    return attachments


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


def balance_probe_request(sender: str, account: str, gas: int = 1_000_000) -> dict[str, Any]:
    """Create init code that returns BALANCE(account) as one ABI word."""

    init_code = "73" + address(account)[2:] + "3160005260206000f3"
    return {
        "from": address(sender),
        "data": "0x" + init_code,
        "value": "0x0",
        "gas": hex(gas),
        "gasPrice": "0x0",
    }


def extcodehash_probe_request(
    sender: str, account: str, gas: int = 1_000_000
) -> dict[str, Any]:
    """Create init code that returns EXTCODEHASH(account) as one ABI word."""

    init_code = "73" + address(account)[2:] + "3f60005260206000f3"
    return {
        "from": address(sender),
        "data": "0x" + init_code,
        "value": "0x0",
        "gas": hex(gas),
        "gasPrice": "0x0",
    }


def deployment_request(
    sender: str, runtime: str, value: int = 0, gas: int = 1_000_000
) -> dict[str, Any]:
    """Create a contract whose deployed code is exactly ``runtime``."""

    raw_runtime = bytes.fromhex(data(runtime)[2:])
    if not raw_runtime or len(raw_runtime) > 255:
        raise ValueError("stateful fixture runtime must be 1..255 bytes")
    init_code = bytes([0x60, len(raw_runtime)]) + bytes.fromhex("80600b6000396000f3")
    return {
        "from": address(sender),
        "data": "0x" + (init_code + raw_runtime).hex(),
        "value": hex(value),
        "gas": hex(gas),
        "gasPrice": "0x0",
    }


def balance_and_codehash_probe_request(
    sender: str,
    balance_account: str,
    code_account: str,
    gas: int = 1_000_000,
) -> dict[str, Any]:
    """Create init code returning BALANCE and EXTCODEHASH as two ABI words."""

    init_code = (
        "73"
        + address(balance_account)[2:]
        + "3160005273"
        + address(code_account)[2:]
        + "3f60205260406000f3"
    )
    return {
        "from": address(sender),
        "data": "0x" + init_code,
        "value": "0x0",
        "gas": hex(gas),
        "gasPrice": "0x0",
    }


def discover_created_address(
    reference: RpcClient, sender: str, block_number: int
) -> str:
    """Resolve the top-level CREATE address using the writer's call tracer."""

    request = deployment_request(sender, CONSTANT_42_RUNTIME)
    traced = reference.call(
        "debug_traceCall",
        [request, hex(block_number), {"tracer": "callTracer"}],
    )
    if not isinstance(traced, dict) or str(traced.get("type", "")).lower() != "create":
        raise ValueError("writer CREATE address probe did not return a CREATE root")
    created = address(traced.get("to"))
    if data(reference.call("eth_getCode", [created, hex(block_number)])) != "0x":
        raise ValueError("writer CREATE fixture address already has code at the anchor")
    return created


def build_stateful_simulation_fixtures(
    sender: str,
    selfdestruct_beneficiary: str,
    created: str,
    block_number: int,
    fee_beneficiary: str,
) -> dict[str, list[dict[str, Any]]]:
    """Build ordered batches whose later calls depend on earlier state changes."""

    sender = address(sender)
    selfdestruct_beneficiary = address(selfdestruct_beneficiary)
    fee_beneficiary = address(fee_beneficiary)
    created = address(created)
    create2_factory = deployment_request(sender, CREATE2_FACTORY_RUNTIME, value=1)
    selfdestruct_calldata = "0x" + selfdestruct_beneficiary[2:] + "00" * 12
    fee_payment = call_request(
        sender, selfdestruct_beneficiary, value=1, gas=1_000_000
    )
    fee_payment["gasPrice"] = hex(FEE_STATE_GAS_PRICE)
    fixtures = {
        "create_then_call": [
            deployment_request(sender, CONSTANT_42_RUNTIME),
            call_request(sender, created, gas=1_000_000),
        ],
        "sstore_sequence": [
            deployment_request(sender, STATE_OVERRIDE_COUNTER_CODE),
            call_request(sender, created, gas=1_000_000),
            call_request(sender, created, gas=1_000_000),
        ],
        "fee_then_balance": [
            fee_payment,
            balance_probe_request(sender, sender),
            balance_probe_request(sender, fee_beneficiary),
        ],
        "create_then_internal_create2": [
            create2_factory,
            call_request(sender, created, gas=1_000_000),
        ],
        "selfdestruct_eip6780": [
            deployment_request(sender, SELFDESTRUCT_RUNTIME, value=7),
            call_request(
                sender,
                created,
                calldata=selfdestruct_calldata,
                gas=1_000_000,
            ),
            balance_and_codehash_probe_request(
                sender, selfdestruct_beneficiary, created, gas=1_000_000
            ),
        ],
        "log_then_revert": [
            deployment_request(sender, LOG_THEN_REVERT_RUNTIME),
            call_request(sender, created, value=5, gas=1_000_000),
        ],
        "failed_create_log_revert": [
            {
                "from": sender,
                "data": FAILED_CREATE_LOG_REVERT_INIT,
                "value": "0x0",
                "gas": hex(1_000_000),
                "gasPrice": "0x0",
            }
        ],
        "nested_blocklist_revert": [
            {
                "from": sender,
                "data": NESTED_BLOCKLIST_INIT,
                "value": "0x1",
                "gas": hex(1_000_000),
                "gasPrice": "0x0",
            }
        ],
    }
    if (
        block_number == ARC_MAINNET_BASELINE_BLOCK
        and sender == ARC_MAINNET_BASELINE_FUNDED
        and created == ARC_MAINNET_BASELINE_CREATED
    ):
        authorization_call = call_request(
            sender, EIP7702_AUTHORITY, gas=1_000_000
        )
        authorization_call["authorizationList"] = [dict(EIP7702_AUTHORIZATION)]
        fixtures["eip7702_delegation_then_call"] = [
            deployment_request(sender, CONSTANT_42_RUNTIME),
            authorization_call,
            call_request(sender, EIP7702_AUTHORITY, gas=1_000_000),
        ]
    return fixtures


def expected_stateful_statuses(name: str, call_count: int) -> list[bool]:
    if name == "log_then_revert":
        return [True, False]
    if name == "failed_create_log_revert":
        return [False]
    return [True] * call_count


def expected_stateful_outputs(
    name: str, block_number: int, created: str
) -> list[str | None] | None:
    if name == "create_then_call":
        return [CONSTANT_42_RUNTIME, abi_words(42)]
    if name == "sstore_sequence":
        return [STATE_OVERRIDE_COUNTER_CODE, abi_words(0), abi_words(1)]
    if name == "create_then_internal_create2":
        child = (
            ARC_MAINNET_BASELINE_CREATE2_CHILD
            if block_number == ARC_MAINNET_BASELINE_BLOCK
            and address(created) == ARC_MAINNET_BASELINE_CREATED
            else None
        )
        return [CREATE2_FACTORY_RUNTIME, None if child is None else abi_words(int(child, 16))]
    if name == "selfdestruct_eip6780":
        return [
            SELFDESTRUCT_RUNTIME,
            "0x",
            abi_words(7, int(SELFDESTRUCT_RUNTIME_CODE_HASH, 16)),
        ]
    if name == "log_then_revert":
        return [LOG_THEN_REVERT_RUNTIME, None]
    if name == "failed_create_log_revert":
        return [None]
    if name == "nested_blocklist_revert":
        return [NESTED_BLOCKLIST_OUTPUT]
    if name == "eip7702_delegation_then_call":
        return [CONSTANT_42_RUNTIME, abi_words(42), abi_words(42)]
    return None


def expected_stateful_system_log_counts(name: str) -> list[int] | None:
    return {
        "create_then_call": [0, 0],
        "sstore_sequence": [0, 0, 0],
        "fee_then_balance": [1, 0, 0],
        "create_then_internal_create2": [1, 1],
        "selfdestruct_eip6780": [1, 1, 0],
        "log_then_revert": [0, 0],
        "failed_create_log_revert": [0],
        "nested_blocklist_revert": [1],
        "eip7702_delegation_then_call": [0, 0, 0],
    }.get(name)


def expected_fee_state_outputs(
    reference: RpcClient,
    block_number: int,
    sender: str,
    beneficiary: str,
) -> list[str]:
    sender_balance = quantity(
        reference.call("eth_getBalance", [address(sender), hex(block_number)])
    )
    beneficiary_balance = quantity(
        reference.call("eth_getBalance", [address(beneficiary), hex(block_number)])
    )
    fee = FEE_STATE_GAS_USED * FEE_STATE_GAS_PRICE
    if sender_balance < fee + 1:
        raise ValueError("fee-state sender does not have enough balance")
    return [
        "0x",
        abi_words(sender_balance - fee - 1),
        abi_words(beneficiary_balance + fee),
    ]


def stateful_event_layout(
    name: str, results: list[dict[str, Any]]
) -> list[dict[str, Any]] | None:
    """Describe the exact event/frame member layout for regression fixtures."""

    if name not in {"create_then_internal_create2", "selfdestruct_eip6780"}:
        return None
    layout: list[dict[str, Any]] = []
    calls = (0, 1)
    for call_index in calls:
        result = results[call_index]
        traces = result.get("traces")
        events = result.get("events")
        if not isinstance(traces, list) or not isinstance(events, list):
            raise ValueError("stateful trace/event layout requires arrays")
        roots = [trace for trace in traces if trace.get("parent_trace_id", "") == ""]
        if len(roots) != 1:
            raise ValueError("stateful trace/event layout requires one root")
        root = roots[0]
        root_id = root.get("id")
        if not isinstance(root_id, str) or not root_id:
            raise ValueError("stateful root trace id must be non-empty")
        if call_index == 0:
            if len(events) != 1:
                raise ValueError("value-bearing CREATE must have one event")
            event = events[0]
            layout.append(
                {
                    "call": 0,
                    "root_kind": canonical_trace_kind(
                        root.get("type"), root.get("call_type")
                    )[0],
                    "event_parent": "root"
                    if event.get("parent_trace_id") == root_id
                    else "other",
                    "event_pos": event.get("pos_in_parent_trace"),
                }
            )
            continue

        children = [trace for trace in traces if trace.get("parent_trace_id") == root_id]
        child_kind = "create" if name == "create_then_internal_create2" else "selfdestruct"
        matching = [
            trace
            for trace in children
            if canonical_trace_kind(trace.get("type"), trace.get("call_type"))[0]
            == child_kind
        ]
        if len(matching) != 1 or len(events) != 1:
            raise ValueError("stateful child/event layout is not unique")
        child = matching[0]
        child_id = child.get("id")
        event = events[0]
        layout.append(
            {
                "call": 1,
                "root_kind": canonical_trace_kind(
                    root.get("type"), root.get("call_type")
                )[0],
                "child_kind": child_kind,
                "child_pos": child.get("pos_in_parent_trace"),
                "event_parent": (
                    "child"
                    if event.get("parent_trace_id") == child_id
                    else "root"
                    if event.get("parent_trace_id") == root_id
                    else "other"
                ),
                "event_pos": event.get("pos_in_parent_trace"),
            }
        )
    return layout


def expected_stateful_event_layout(name: str) -> list[dict[str, Any]] | None:
    create_root = {
        "call": 0,
        "root_kind": "create",
        "event_parent": "root",
        "event_pos": 0,
    }
    if name == "create_then_internal_create2":
        return [
            create_root,
            {
                "call": 1,
                "root_kind": "call",
                "child_kind": "create",
                "child_pos": 0,
                "event_parent": "child",
                "event_pos": 0,
            },
        ]
    if name == "selfdestruct_eip6780":
        return [
            create_root,
            {
                "call": 1,
                "root_kind": "call",
                "child_kind": "selfdestruct",
                "child_pos": 1,
                "event_parent": "root",
                "event_pos": 0,
            },
        ]
    return None


def eip7825_gas_cap_request(
    sender: str, recipient: str, calldata_bytes: int
) -> dict[str, Any]:
    return call_request(
        sender,
        recipient,
        "0x" + "01" * calldata_bytes,
        gas=25_000_000,
    )


def eip7702_estimate_request(sender: str) -> dict[str, Any]:
    request = call_request(sender, EIP7702_AUTHORITY)
    request["authorizationList"] = [dict(EIP7702_AUTHORIZATION)]
    return request


def erc20_transfer_calldata(recipient: str, amount: int) -> str:
    recipient_word = "0" * 24 + address(recipient)[2:]
    amount_word = amount.to_bytes(32, "big").hex()
    return ERC20_TRANSFER + recipient_word + amount_word


def erc20_balance_of_calldata(account: str) -> str:
    return "0x70a08231" + "0" * 24 + address(account)[2:]


def address_calldata(selector: str, account: str) -> str:
    return data(selector) + "0" * 24 + address(account)[2:]


def two_address_calldata(selector: str, first: str, second: str) -> str:
    return (
        data(selector)
        + "0" * 24
        + address(first)[2:]
        + "0" * 24
        + address(second)[2:]
    )


def uint256_calldata(value: int) -> str:
    if value < 0 or value >= 1 << 256:
        raise ValueError("uint256 calldata value is out of range")
    return "0x" + value.to_bytes(32, "big").hex()


def selector_uint_calldata(selector: str, value: int) -> str:
    return data(selector) + uint256_calldata(value)[2:]


def abi_words(*values: int) -> str:
    if any(value < 0 or value >= 1 << 256 for value in values):
        raise ValueError("ABI word value is out of range")
    return "0x" + "".join(value.to_bytes(32, "big").hex() for value in values)


def abi_string(value: str) -> str:
    encoded = value.encode()
    return abi_words(32, len(encoded)) + encoded.hex() + "00" * (-len(encoded) % 32)


def abi_dynamic_bytes_call(selector: str, *values: str) -> str:
    raw_values = [bytes.fromhex(data(value)[2:]) for value in values]
    head_size = 32 * len(raw_values)
    head = bytearray()
    tail = bytearray()
    for value in raw_values:
        head.extend((head_size + len(tail)).to_bytes(32, "big"))
        tail.extend(len(value).to_bytes(32, "big"))
        tail.extend(value)
        tail.extend(b"\x00" * (-len(value) % 32))
    return data(selector) + (head + tail).hex()


def load_pq_valid_input() -> str:
    fixture_path = Path(__file__).with_name("fixtures") / "arc-pq-valid.json"
    fixture = json.loads(fixture_path.read_text())
    vector = fixture.get("vector")
    if fixture.get("source_file_sha256") != PQ_SOURCE_FILE_SHA256:
        raise ValueError("Arc PQ fixture source hash mismatch")
    if (
        not isinstance(vector, dict)
        or vector.get("is_valid") is not True
        or vector.get("scheme") != "SLH-DSA-SHA2-128s"
    ):
        raise ValueError("invalid Arc PQ fixture")
    verifying_key = data(vector.get("verifying_key"))
    message = data(vector.get("message"))
    signature = data(vector.get("signature"))
    if len(bytes.fromhex(verifying_key[2:])) != 32:
        raise ValueError("Arc PQ verifying key must be 32 bytes")
    if len(bytes.fromhex(signature[2:])) != 7_856:
        raise ValueError("Arc PQ signature must be 7856 bytes")
    return abi_dynamic_bytes_call(PQ_VERIFY, verifying_key, message, signature)


def load_pq_invalid_signature_input() -> str:
    """Keep the PQ ABI shape valid while corrupting one signature byte."""

    raw = bytearray.fromhex(load_pq_valid_input()[2:])
    arguments_start = 4
    signature_offset = int.from_bytes(
        raw[arguments_start + 64 : arguments_start + 96], "big"
    )
    length_offset = arguments_start + signature_offset
    signature_length = int.from_bytes(raw[length_offset : length_offset + 32], "big")
    if signature_length != 7_856:
        raise ValueError("Arc PQ invalid fixture signature length mismatch")
    signature_end = length_offset + 32 + signature_length
    if signature_end > len(raw):
        raise ValueError("Arc PQ invalid fixture signature exceeds calldata")
    raw[signature_end - 1] ^= 1
    return "0x" + raw.hex()


def expected_fixture_output(name: str, block_number: int) -> str | None:
    always = {
        "p256_valid": abi_words(1),
        "p256_invalid_wrong_hash": "0x",
        "pq_valid": abi_words(1),
        "pq_invalid_signature": abi_words(0),
        "eip2930_access_list_identity": STANDARD_PRECOMPILE_VECTORS[
            "identity_deadbeef"
        ][2],
        **{
            vector_name: expected
            for vector_name, (_target, _calldata, expected) in STANDARD_PRECOMPILE_VECTORS.items()
        },
    }
    if name in always:
        return always[name]
    if name == "eip1559_identity":
        return (
            "0x"
            if block_number == ARC_MAINNET_BASELINE_BLOCK
            else STANDARD_PRECOMPILE_VECTORS["identity_deadbeef"][2]
        )
    if block_number != ARC_MAINNET_BASELINE_BLOCK:
        return None
    baseline = {
        "usdc_total_supply": abi_words(5_495_688_632_355),
        "nca_total_supply": abi_words(5_495_688_632_355_000_000_000_000),
        "ncc_usdc_is_blocklisted": abi_words(1),
        "ncc_funded_is_not_blocklisted": abi_words(0),
        "system_accounting_at_anchor": abi_words(0, 6_224, 20_000_000_000),
        "protocol_fee_params": abi_words(
            20,
            200,
            5_000,
            20_000_000_000,
            20_000_000_000_000,
            30_000_000,
        ),
        "protocol_consensus_params": abi_words(
            3_000, 500, 1_000, 500, 1_000, 500, 5_000, 500
        ),
        "denylist_usdc": abi_words(0),
        "active_validator_count": abi_words(15),
        "permit2_domain_separator": (
            "0xa88a1b742ab6890402e6ee74f2359f3723ab547c6dc3b850249bdbffebe2b18f"
        ),
    }
    return baseline.get(name)


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


def eip7702_authority_state(client: RpcClient, block_number: int) -> dict[str, Any]:
    selector = hex(block_number)
    return {
        "code": data(client.call("eth_getCode", [EIP7702_AUTHORITY, selector])),
        "nonce": quantity(
            client.call("eth_getTransactionCount", [EIP7702_AUTHORITY, selector])
        ),
        "balance": quantity(
            client.call("eth_getBalance", [EIP7702_AUTHORITY, selector])
        ),
    }


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


def environment_probe_overrides(overrides: dict[str, Any]) -> dict[str, Any]:
    """Use distinctive values so a dropped BlockOverride cannot pass by accident."""

    probe = dict(overrides)
    probe.update(
        {
            "time": hex(quantity(overrides["time"]) + 17),
            "gasLimit": hex(25_000_000),
            "coinbase": "0x" + "00" * 18 + "c0fe",
            "random": "0x" + "11" * 32,
            "baseFee": hex(123),
            "blockHash": {
                str(key): "0x" + "22" * 32
                for key in overrides.get("blockHash", {})
            },
        }
    )
    return probe


def environment_probe_request(sender: str, block_number: int) -> dict[str, Any]:
    """Create init code that returns seven block-environment words as runtime code."""

    if block_number < 0 or block_number >= 1 << 256:
        raise ValueError("environment probe block number is out of range")
    encoded_number = block_number.to_bytes(
        max(1, (block_number.bit_length() + 7) // 8), "big"
    )
    push_number = bytes([0x5F + len(encoded_number)]) + encoded_number
    init_code = (
        bytes.fromhex("43600052426020524160405245606052486080524460a052")
        + push_number
        + bytes.fromhex("4060c05260e06000f3")
    )
    return {
        "from": address(sender),
        "data": "0x" + init_code.hex(),
        "value": "0x0",
        "gas": hex(1_000_000),
        "gasPrice": "0x0",
    }


def blockhash_probe_request(sender: str, queried_number: int) -> dict[str, Any]:
    """Create init code that returns BLOCKHASH(queried_number)."""

    if queried_number < 0 or queried_number >= 1 << 256:
        raise ValueError("BLOCKHASH probe number is out of range")
    encoded = queried_number.to_bytes(
        max(1, (queried_number.bit_length() + 7) // 8), "big"
    )
    init_code = (
        bytes([0x5F + len(encoded)])
        + encoded
        + bytes.fromhex("405f5260205ff3")
    )
    return {
        "from": address(sender),
        "data": "0x" + init_code.hex(),
        "value": "0x0",
        "gas": hex(1_000_000),
        "gasPrice": "0x0",
    }


def environment_probe_output(
    block_number: int,
    overrides: dict[str, Any],
    *,
    call_like: bool = False,
) -> str:
    execution_number = quantity(overrides.get("number"))
    block_hashes = overrides.get("blockHash")
    if not isinstance(block_hashes, dict) or str(block_number) not in block_hashes:
        raise ValueError("environment probe is missing the parent BLOCKHASH override")
    return abi_words(
        execution_number,
        quantity(overrides.get("time")),
        int(address(overrides.get("coinbase")), 16),
        quantity(overrides.get("gasLimit")),
        0 if call_like else quantity(overrides.get("baseFee")),
        int(block_hash(overrides.get("random")), 16),
        int(block_hash(block_hashes[str(block_number)]), 16),
    )


def environment_guard_request(
    sender: str,
    block_number: int,
    block_overrides: dict[str, Any],
    canonical_previous_hash: str | None = None,
) -> dict[str, Any]:
    """Create only when every explicit query-environment override is applied."""

    expected_number = quantity(block_overrides["number"])
    block_hashes = block_overrides.get("blockHash")
    if not isinstance(block_hashes, dict) or str(block_number) not in block_hashes:
        raise ValueError("environment guard is missing the parent BLOCKHASH override")

    def push(value: int) -> bytes:
        if value < 0 or value >= 1 << 256:
            raise ValueError("environment guard value is out of range")
        if value == 0:
            return bytes([0x5F])
        encoded = value.to_bytes((value.bit_length() + 7) // 8, "big")
        return bytes([0x5F + len(encoded)]) + encoded

    checks = [
        (bytes([0x43]), expected_number),
        (bytes([0x42]), quantity(block_overrides["time"])),
        (bytes([0x41]), int(address(block_overrides["coinbase"]), 16)),
        (bytes([0x45]), quantity(block_overrides["gasLimit"])),
        (bytes([0x48]), quantity(block_overrides["baseFee"])),
        (bytes([0x44]), int(block_hash(block_overrides["random"]), 16)),
        (push(block_number) + bytes([0x40]), int(block_hash(block_hashes[str(block_number)]), 16)),
    ]
    if canonical_previous_hash is not None:
        if block_number < 1:
            raise ValueError("environment guard previous BLOCKHASH requires block > 0")
        checks.append(
            (
                push(block_number - 1) + bytes([0x40]),
                int(block_hash(canonical_previous_hash), 16),
            )
        )
    init_code = bytearray()
    jump_offsets: list[int] = []
    for opcode, expected in checks:
        init_code.extend(opcode)
        init_code.extend(push(expected))
        init_code.extend(bytes([0x14, 0x15, 0x61]))
        jump_offsets.append(len(init_code))
        init_code.extend(bytes([0x00, 0x00, 0x57]))
    init_code.extend(bytes.fromhex("60006000f3"))
    failure_offset = len(init_code)
    if failure_offset >= 1 << 16:
        raise ValueError("environment guard jump destination is out of range")
    for offset in jump_offsets:
        init_code[offset : offset + 2] = failure_offset.to_bytes(2, "big")
    init_code.extend(bytes.fromhex("5b60006000fd"))

    base_fee = quantity(block_overrides["baseFee"])
    if base_fee == (1 << 256) - 1:
        raise ValueError("environment guard base fee is too large")
    return {
        "from": address(sender),
        "data": "0x" + init_code.hex(),
        "value": "0x0",
        "gasPrice": hex(base_fee + 1),
    }


def abi_word_at(value: Any, index: int) -> int:
    normalized = data(value)
    if index < 0:
        raise ValueError("ABI word index must be non-negative")
    raw = bytes.fromhex(normalized[2:])
    start = index * 32
    end = start + 32
    if end > len(raw):
        raise ValueError(f"ABI output does not contain word {index}")
    return int.from_bytes(raw[start:end], "big")


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
    hash_context = {"block_id": anchor_hash, "type": "Equals"}
    hash_selector = {"blockHash": anchor_hash, "requireCanonical": True}
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
        hash_expected = quantity(reference.call("eth_getBalance", [account, hash_selector]))
        hash_actual_result = leafage.capture(
            "getAddressBalance", [account, hash_context]
        )
        hash_actual = (
            quantity(hash_actual_result["result"])
            if hash_actual_result.get("ok")
            else hash_actual_result
        )
        report.add(
            "balance",
            f"getAddressBalance.canonical_hash.{label}",
            hash_actual_result.get("ok") is True and hash_actual == hash_expected,
            hash_expected,
            hash_actual,
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
    anchor_hash: str,
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

    hash_selector = {"blockHash": anchor_hash, "requireCanonical": True}
    reference_hash_call = reference.capture("eth_call", [call, hash_selector])
    leafage_hash_call = leafage.capture(
        "eth_call", [call, hash_selector, None, None]
    )
    report.add(
        "asset",
        "usdc_balance_of.canonical_hash",
        reference_hash_call.get("ok") is True
        and leafage_hash_call.get("ok") is True
        and data(reference_hash_call.get("result"))
        == data(leafage_hash_call.get("result")),
        reference_hash_call,
        leafage_hash_call,
    )


def add_state_read_comparison(
    report: Report,
    category: str,
    name: str,
    expected_result: dict[str, Any],
    actual_result: dict[str, Any],
    normalize: Any,
) -> Any:
    expected: Any = expected_result
    actual: Any = actual_result
    passed = False
    if expected_result.get("ok") and actual_result.get("ok"):
        expected = normalize(expected_result.get("result"))
        actual = normalize(actual_result.get("result"))
        passed = expected == actual
    report.add(category, name, passed, expected, actual)
    return expected if expected_result.get("ok") else None


def run_world_state_checks(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    anchor_hash: str,
    empty: str,
) -> None:
    """Compare account and storage reads through both Leafage API surfaces."""

    selector = hex(block_number)
    context = {"block_id": selector, "type": "Equals"}
    account_samples = (("usdc", USDC), ("empty", empty))
    reference_nonce: dict[str, int] = {}
    reference_code: dict[str, str] = {}
    for label, account in account_samples:
        nonce = reference.capture("eth_getTransactionCount", [account, selector])
        reference_nonce[label] = add_state_read_comparison(
            report,
            "world_state",
            f"nonce.standard.{label}",
            nonce,
            leafage.capture("eth_getTransactionCount", [account, selector]),
            quantity,
        )
        add_state_read_comparison(
            report,
            "world_state",
            f"nonce.debank.{label}",
            nonce,
            leafage.capture("getAddressNonce", [account, context]),
            quantity,
        )

        code = reference.capture("eth_getCode", [account, selector])
        reference_code[label] = add_state_read_comparison(
            report,
            "world_state",
            f"code.standard.{label}",
            code,
            leafage.capture("eth_getCode", [account, selector]),
            data,
        )
        add_state_read_comparison(
            report,
            "world_state",
            f"code.debank.{label}",
            code,
            leafage.capture("getAddressCode", [account, context]),
            data,
        )

    report.add(
        "fixture",
        "world_state_nonzero_and_zero_accounts",
        reference_nonce.get("usdc", 0) > 0
        and reference_nonce.get("empty") == 0
        and reference_code.get("usdc") not in {None, "0x"}
        and reference_code.get("empty") == "0x",
        "USDC has nonce/code; empty account has neither",
        {"nonce": reference_nonce, "code": reference_code},
    )

    hash_selector = {"blockHash": anchor_hash, "requireCanonical": True}
    hash_context = {"block_id": anchor_hash, "type": "Equals"}
    for label, account in account_samples:
        for method, debank_method, normalizer in (
            ("eth_getTransactionCount", "getAddressNonce", quantity),
            ("eth_getCode", "getAddressCode", data),
        ):
            expected = reference.capture(method, [account, hash_selector])
            add_state_read_comparison(
                report,
                "world_state",
                f"{method}.canonical_hash.{label}",
                expected,
                leafage.capture(method, [account, hash_selector]),
                normalizer,
            )
            add_state_read_comparison(
                report,
                "world_state",
                f"{debank_method}.canonical_hash.{label}",
                expected,
                leafage.capture(debank_method, [account, hash_context]),
                normalizer,
            )

    zero_word = "0x" + "00" * 32
    storage_samples = (
        ("usdc_slot_0", USDC, 0, True),
        ("nca_total_supply_slot_2", NATIVE_COIN_AUTHORITY, 2, True),
        ("empty_slot_0", empty, 0, False),
    )
    fixture_values: dict[str, str] = {}
    for label, account, slot, must_be_nonzero in storage_samples:
        slot_key = hex(slot)
        expected = reference.capture("eth_getStorageAt", [account, slot_key, selector])
        fixture_values[label] = add_state_read_comparison(
            report,
            "world_state",
            f"storage.standard.{label}",
            expected,
            leafage.capture("eth_getStorageAt", [account, slot_key, selector]),
            storage_word,
        )
        add_state_read_comparison(
            report,
            "world_state",
            f"storage.debank.{label}",
            expected,
            leafage.capture("getStorageAt", [account, slot_key, context]),
            storage_word,
        )
        add_state_read_comparison(
            report,
            "world_state",
            f"getStorageAt.canonical_hash.{label}",
            expected,
            leafage.capture(
                "getStorageAt", [account, hex(slot), hash_context]
            ),
            storage_word,
        )
        expected_value = fixture_values[label]
        report.add(
            "fixture",
            f"{label}.value_class",
            (expected_value != zero_word) is must_be_nonzero,
            "non-zero" if must_be_nonzero else zero_word,
            expected_value,
        )

    for label, account, slot in (
        ("usdc_slot_0", USDC, 0),
        ("empty_slot_0", empty, 0),
    ):
        expected = reference.capture(
            "eth_getStorageAt", [account, hex(slot), hash_selector]
        )
        add_state_read_comparison(
            report,
            "world_state",
            f"storage.canonical_hash.{label}",
            expected,
            leafage.capture(
                "eth_getStorageAt", [account, hex(slot), hash_selector]
            ),
            storage_word,
        )


def run_world_state_boundary_checks(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    anchor_height: int,
    nonce_height: int,
    nonce_account: str,
) -> None:
    """Reject archive off-by-one reads with observed nonce/code/storage changes."""

    def compare_boundary(
        label: str,
        standard_method: str,
        debank_method: str,
        params: list[Any],
        height: int,
        normalizer: Any,
    ) -> tuple[Any, Any]:
        observed: list[Any] = []
        for side, selected_height in (("before", height - 1), ("after", height)):
            block = reference.call(
                "eth_getBlockByNumber", [hex(selected_height), False]
            )
            if not isinstance(block, dict):
                raise ValueError(f"{label} boundary block is missing")
            block_hash_value = block_hash(block.get("hash"))
            numeric_selector = hex(selected_height)
            hash_selector = {
                "blockHash": block_hash_value,
                "requireCanonical": True,
            }
            numeric_context = {
                "block_id": numeric_selector,
                "type": "Equals",
            }
            hash_context = {"block_id": block_hash_value, "type": "Equals"}
            expected_numeric = reference.capture(
                standard_method, [*params, numeric_selector]
            )
            value = add_state_read_comparison(
                report,
                "world_state_boundary",
                f"{label}.{side}.standard_numeric",
                expected_numeric,
                leafage.capture(standard_method, [*params, numeric_selector]),
                normalizer,
            )
            add_state_read_comparison(
                report,
                "world_state_boundary",
                f"{label}.{side}.debank_numeric",
                expected_numeric,
                leafage.capture(debank_method, [*params, numeric_context]),
                normalizer,
            )
            expected_hash = reference.capture(
                standard_method, [*params, hash_selector]
            )
            add_state_read_comparison(
                report,
                "world_state_boundary",
                f"{label}.{side}.standard_hash",
                expected_hash,
                leafage.capture(standard_method, [*params, hash_selector]),
                normalizer,
            )
            add_state_read_comparison(
                report,
                "world_state_boundary",
                f"{label}.{side}.debank_hash",
                expected_hash,
                leafage.capture(debank_method, [*params, hash_context]),
                normalizer,
            )
            observed.append(value)
        return observed[0], observed[1]

    nonce_before, nonce_after = compare_boundary(
        "nonce",
        "eth_getTransactionCount",
        "getAddressNonce",
        [address(nonce_account)],
        nonce_height,
        quantity,
    )
    report.add(
        "fixture",
        "nonce_boundary_changes",
        isinstance(nonce_before, int)
        and isinstance(nonce_after, int)
        and nonce_after > nonce_before,
        "nonce increases at the observed transaction boundary",
        {"height": nonce_height, "before": nonce_before, "after": nonce_after},
    )
    balance_before, balance_after = compare_boundary(
        "balance",
        "eth_getBalance",
        "getAddressBalance",
        [address(nonce_account)],
        nonce_height,
        quantity,
    )
    report.add(
        "fixture",
        "balance_boundary_changes",
        isinstance(balance_before, int)
        and isinstance(balance_after, int)
        and balance_before != balance_after,
        "balance changes at the observed transaction boundary",
        {"height": nonce_height, "before": balance_before, "after": balance_after},
    )

    if anchor_height < max(CODE_BOUNDARY_HEIGHT, STORAGE_BOUNDARY_HEIGHT):
        return

    code_before, code_after = compare_boundary(
        "code",
        "eth_getCode",
        "getAddressCode",
        [CODE_BOUNDARY_ADDRESS],
        CODE_BOUNDARY_HEIGHT,
        data,
    )
    code_blocks = (
        reference.call(
            "eth_getBlockByNumber", [hex(CODE_BOUNDARY_HEIGHT - 1), False]
        ),
        reference.call(
            "eth_getBlockByNumber", [hex(CODE_BOUNDARY_HEIGHT), False]
        ),
    )
    report.add(
        "fixture",
        "code_boundary_fixed_values",
        code_before == "0x"
        and code_after == CODE_BOUNDARY_VALUE
        and all(isinstance(block, dict) for block in code_blocks)
        and block_hash(code_blocks[0].get("hash")) == CODE_BOUNDARY_BEFORE_HASH
        and block_hash(code_blocks[1].get("hash")) == CODE_BOUNDARY_AFTER_HASH,
        {
            "before": "0x",
            "after": CODE_BOUNDARY_VALUE,
            "hashes": [CODE_BOUNDARY_BEFORE_HASH, CODE_BOUNDARY_AFTER_HASH],
        },
        {"before": code_before, "after": code_after, "blocks": code_blocks},
    )

    storage_before, storage_after = compare_boundary(
        "storage",
        "eth_getStorageAt",
        "getStorageAt",
        [STORAGE_BOUNDARY_ADDRESS, STORAGE_BOUNDARY_SLOT],
        STORAGE_BOUNDARY_HEIGHT,
        storage_word,
    )
    storage_blocks = (
        reference.call(
            "eth_getBlockByNumber", [hex(STORAGE_BOUNDARY_HEIGHT - 1), False]
        ),
        reference.call(
            "eth_getBlockByNumber", [hex(STORAGE_BOUNDARY_HEIGHT), False]
        ),
    )
    report.add(
        "fixture",
        "storage_boundary_fixed_values",
        storage_before == STORAGE_BOUNDARY_BEFORE_VALUE
        and storage_after == STORAGE_BOUNDARY_AFTER_VALUE
        and all(isinstance(block, dict) for block in storage_blocks)
        and block_hash(storage_blocks[0].get("hash"))
        == STORAGE_BOUNDARY_BEFORE_HASH
        and block_hash(storage_blocks[1].get("hash"))
        == STORAGE_BOUNDARY_AFTER_HASH,
        {
            "before": STORAGE_BOUNDARY_BEFORE_VALUE,
            "after": STORAGE_BOUNDARY_AFTER_VALUE,
            "hashes": [STORAGE_BOUNDARY_BEFORE_HASH, STORAGE_BOUNDARY_AFTER_HASH],
        },
        {
            "before": storage_before,
            "after": storage_after,
            "blocks": storage_blocks,
        },
    )


def run_reference_world_state_boundary_preflight(
    report: Report,
    reference: RpcClient,
    block_number: int,
    search_depth: int,
) -> None:
    nonce_height, nonce_account, _before_balance, _after_balance = (
        select_balance_boundary(reference, block_number, search_depth)
    )
    nonce_before = quantity(
        reference.call(
            "eth_getTransactionCount", [nonce_account, hex(nonce_height - 1)]
        )
    )
    nonce_after = quantity(
        reference.call("eth_getTransactionCount", [nonce_account, hex(nonce_height)])
    )
    report.add(
        "preflight",
        "world_state.nonce_boundary",
        nonce_after > nonce_before,
        "nonce increases at the observed transaction boundary",
        {
            "height": nonce_height,
            "account": nonce_account,
            "before": nonce_before,
            "after": nonce_after,
        },
    )
    if block_number < max(CODE_BOUNDARY_HEIGHT, STORAGE_BOUNDARY_HEIGHT):
        return

    code_before = data(
        reference.call(
            "eth_getCode", [CODE_BOUNDARY_ADDRESS, hex(CODE_BOUNDARY_HEIGHT - 1)]
        )
    )
    code_after = data(
        reference.call(
            "eth_getCode", [CODE_BOUNDARY_ADDRESS, hex(CODE_BOUNDARY_HEIGHT)]
        )
    )
    code_before_block = reference.call(
        "eth_getBlockByNumber", [hex(CODE_BOUNDARY_HEIGHT - 1), False]
    )
    code_after_block = reference.call(
        "eth_getBlockByNumber", [hex(CODE_BOUNDARY_HEIGHT), False]
    )
    report.add(
        "preflight",
        "world_state.code_boundary",
        code_before == "0x"
        and code_after == CODE_BOUNDARY_VALUE
        and isinstance(code_before_block, dict)
        and isinstance(code_after_block, dict)
        and block_hash(code_before_block.get("hash")) == CODE_BOUNDARY_BEFORE_HASH
        and block_hash(code_after_block.get("hash")) == CODE_BOUNDARY_AFTER_HASH,
        {"before": "0x", "after": CODE_BOUNDARY_VALUE},
        {"before": code_before, "after": code_after},
    )

    storage_before = storage_word(
        reference.call(
            "eth_getStorageAt",
            [
                STORAGE_BOUNDARY_ADDRESS,
                STORAGE_BOUNDARY_SLOT,
                hex(STORAGE_BOUNDARY_HEIGHT - 1),
            ],
        )
    )
    storage_after = storage_word(
        reference.call(
            "eth_getStorageAt",
            [
                STORAGE_BOUNDARY_ADDRESS,
                STORAGE_BOUNDARY_SLOT,
                hex(STORAGE_BOUNDARY_HEIGHT),
            ],
        )
    )
    storage_before_block = reference.call(
        "eth_getBlockByNumber", [hex(STORAGE_BOUNDARY_HEIGHT - 1), False]
    )
    storage_after_block = reference.call(
        "eth_getBlockByNumber", [hex(STORAGE_BOUNDARY_HEIGHT), False]
    )
    report.add(
        "preflight",
        "world_state.storage_boundary",
        storage_before == STORAGE_BOUNDARY_BEFORE_VALUE
        and storage_after == STORAGE_BOUNDARY_AFTER_VALUE
        and isinstance(storage_before_block, dict)
        and isinstance(storage_after_block, dict)
        and block_hash(storage_before_block.get("hash"))
        == STORAGE_BOUNDARY_BEFORE_HASH
        and block_hash(storage_after_block.get("hash"))
        == STORAGE_BOUNDARY_AFTER_HASH,
        {
            "before": STORAGE_BOUNDARY_BEFORE_VALUE,
            "after": STORAGE_BOUNDARY_AFTER_VALUE,
        },
        {"before": storage_before, "after": storage_after},
    )


def rpc_error_class(result: dict[str, Any]) -> str:
    if result.get("ok"):
        return "success"
    error = result.get("error")
    message = str(error.get("message", "")).lower() if isinstance(error, dict) else ""
    if "revert" in message:
        return "revert"
    if (
        "out of gas" in message
        or "gas required exceeds" in message
        or "intrinsic gas too low" in message
    ):
        return "out-of-gas"
    if "insufficient funds" in message or "out of funds" in message:
        return "insufficient-funds"
    if "nonce" in message:
        return "nonce"
    if "block" in message and ("blocked" in message or "blocklist" in message):
        return "blocked"
    if isinstance(error, dict) and error.get("code") == LEAFAGE_INVALID_PARAMS:
        return "invalid-params"
    return "other-halt"


def leafage_result_error_class(result: dict[str, Any]) -> str:
    code = result.get("code")
    if code == 0:
        return "success"
    if code == LEAFAGE_EVM_REVERT:
        return "revert"
    if code == LEAFAGE_GAS_EXHAUSTED:
        return "out-of-gas"
    if code == LEAFAGE_BALANCE_EXHAUSTED:
        return "insufficient-funds"
    if code == -39003:
        return "nonce"
    message = str(result.get("err", "")).lower()
    if "blocked" in message or "blocklist" in message:
        return "blocked"
    return "other-halt"


def writer_call_params(
    request: dict[str, Any],
    block_selector: Any,
    state_override: dict[str, Any] | None = None,
    block_overrides: dict[str, Any] | None = None,
) -> list[Any]:
    selector = hex(block_selector) if isinstance(block_selector, int) else block_selector
    params: list[Any] = [request, selector]
    if state_override is not None or block_overrides is not None:
        params.extend([state_override, block_overrides])
    return params


def writer_trace_options(
    state_override: dict[str, Any] | None = None,
    block_overrides: dict[str, Any] | None = None,
) -> dict[str, Any]:
    options: dict[str, Any] = {"tracer": "callTracer"}
    if state_override is not None:
        options["stateOverrides"] = state_override
    if block_overrides is not None:
        options["blockOverrides"] = block_overrides
    return options


def run_contract_multicall(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    anchor_hash: str,
    anchor_timestamp: int,
    name: str,
    calls: list[dict[str, Any]],
    fixed_outputs: list[str | None] | None = None,
    fast_fail: bool = False,
    state_override: dict[str, Any] | None = None,
    block_overrides: dict[str, Any] | None = None,
    use_parallel: bool = False,
    use_hash_context: bool = False,
) -> None:
    """Compare independent Leafage calls with independent writer executions."""

    selector = anchor_hash if use_hash_context else hex(block_number)
    reference_selector: Any = (
        {"blockHash": anchor_hash, "requireCanonical": True}
        if use_hash_context
        else block_number
    )
    context = {"block_id": selector, "type": "Equals"}
    expected_calls: list[dict[str, Any]] = []
    for request in calls:
        call_result = reference.capture(
            "eth_call",
            writer_call_params(
                request, reference_selector, state_override, block_overrides
            ),
        )
        trace_result = reference.capture(
            "debug_traceCall",
            [
                request,
                selector,
                writer_trace_options(state_override, block_overrides),
            ],
        )
        gas_used: int | None = None
        if trace_result.get("ok"):
            trace_payload = trace_result.get("result")
            if not isinstance(trace_payload, dict):
                raise ValueError("debug_traceCall result must be an object")
            gas_used = quantity(trace_payload.get("gasUsed"))
        expected_calls.append(
            {
                "class": rpc_error_class(call_result),
                "output": data(call_result["result"]) if call_result.get("ok") else None,
                "gas_used": gas_used,
                "call": call_result,
                "trace": trace_result,
            }
        )

    leafage_result = leafage.capture(
        "contractMultiCall",
        [
            calls,
            context,
            block_overrides,
            state_override,
            fast_fail,
            use_parallel,
            False,
        ],
    )
    if not leafage_result.get("ok"):
        report.add("contract_multicall", f"{name}.rpc", False, "success", leafage_result)
        return

    try:
        payload = leafage_result.get("result")
        if not isinstance(payload, dict):
            raise ValueError("Leafage contractMultiCall response is not an object")
        results = payload.get("results")
        if not isinstance(results, list):
            raise ValueError("Leafage contractMultiCall results are not an array")
        report.add(
            "contract_multicall",
            f"{name}.result_count",
            len(results) == len(expected_calls) == len(calls),
            len(expected_calls),
            len(results),
        )
        if len(results) != len(expected_calls):
            return
        for result in results:
            if not isinstance(result, dict):
                raise ValueError("Leafage contractMultiCall item is not an object")
            code = result.get("code")
            if isinstance(code, bool) or not isinstance(code, int):
                raise ValueError("Leafage contractMultiCall code must be an integer")

        reference_classes = [item["class"] for item in expected_calls]
        reference_outputs = [item["output"] for item in expected_calls]
        reference_gas = [item["gas_used"] for item in expected_calls]
        expected_classes = list(reference_classes)
        expected_outputs = list(reference_outputs)
        expected_gas = list(reference_gas)
        first_failure = next(
            (
                index
                for index, error_class in enumerate(reference_classes)
                if error_class != "success"
            ),
            None,
        )
        if fast_fail and first_failure is not None:
            remaining = len(expected_calls) - first_failure
            expected_classes[first_failure:] = [reference_classes[first_failure]] * remaining
            expected_outputs[first_failure:] = [reference_outputs[first_failure]] * remaining
            expected_gas[first_failure:] = [reference_gas[first_failure]] * remaining
            report.add(
                "contract_multicall",
                f"{name}.fast_fail_clones_failure",
                all(item == results[first_failure] for item in results[first_failure:]),
                "the first failure is cloned without executing later requests",
                results[first_failure:],
            )
        actual_classes = [leafage_result_error_class(item) for item in results]
        report.add(
            "contract_multicall",
            f"{name}.status",
            actual_classes == expected_classes,
            expected_classes,
            actual_classes,
        )
        if name in {"revert_then_success", "fast_fail_revert"}:
            failed_indexes = range(len(results)) if fast_fail else (0,)
            reference_error = expected_calls[0]["call"].get("error")
            reference_reason = (
                str(reference_error.get("message", ""))
                if isinstance(reference_error, dict)
                else ""
            )
            actual_reasons = [str(results[index].get("err", "")) for index in failed_indexes]
            report.add(
                "contract_multicall",
                f"{name}.revert_reason",
                ERC20_INSUFFICIENT_BALANCE_REASON in reference_reason
                and all(
                    ERC20_INSUFFICIENT_BALANCE_REASON in reason
                    for reason in actual_reasons
                ),
                ERC20_INSUFFICIENT_BALANCE_REASON,
                {"reference": reference_reason, "leafage": actual_reasons},
            )
        if fixed_outputs is not None:
            if len(fixed_outputs) != len(reference_outputs):
                raise ValueError("fixed output count does not match contractMultiCall batch")
            fixed_matches = all(
                fixed is None or actual == fixed
                for fixed, actual in zip(fixed_outputs, reference_outputs)
            )
            report.add(
                "fixture",
                f"reference_{name}_fixed_outputs",
                fixed_matches,
                fixed_outputs,
                reference_outputs,
                "State-dependent asset values are fixed only at the audited mainnet anchor.",
            )
        actual_outputs = [
            data(item.get("result", "0x")) if error_class == "success" else None
            for item, error_class in zip(results, actual_classes)
        ]
        report.add(
            "contract_multicall",
            f"{name}.return_data",
            actual_outputs == expected_outputs,
            expected_outputs,
            actual_outputs,
        )
        actual_gas = [quantity(item.get("gas_used")) for item in results]
        report.add(
            "contract_multicall",
            f"{name}.gas_used",
            actual_gas == expected_gas,
            expected_gas,
            actual_gas,
        )

        stats = payload.get("stats")
        if not isinstance(stats, dict):
            raise ValueError("Leafage contractMultiCall stats are not an object")
        expected_success = all(item == "success" for item in reference_classes)
        report.add(
            "contract_multicall",
            f"{name}.stats",
            quantity(stats.get("block_num")) == block_number
            and block_hash(stats.get("block_hash")) == anchor_hash
            and quantity(stats.get("block_time")) == anchor_timestamp
            and stats.get("success") is expected_success,
            {
                "block_num": block_number,
                "block_hash": anchor_hash,
                "block_time": anchor_timestamp,
                "success": expected_success,
            },
            stats,
        )
        report.add(
            "contract_multicall",
            f"{name}.cache_metadata",
            stats.get("cache_enabled") is False
            and all(item.get("from_cache") is False for item in results),
            {"cache_enabled": False, "from_cache": [False] * len(results)},
            {
                "cache_enabled": stats.get("cache_enabled"),
                "from_cache": [item.get("from_cache") for item in results],
            },
        )
    except (KeyError, TypeError, ValueError) as error:
        report.add(
            "contract_multicall", f"{name}.schema", False, "valid response", str(error)
        )


def build_native_sentinel_calls(sender: str) -> list[dict[str, Any]]:
    return [
        call_request(
            sender,
            NATIVE_TOKEN_SENTINEL,
            erc20_balance_of_calldata(sender),
            gas=1_000_000,
        ),
        call_request(sender, NATIVE_TOKEN_SENTINEL, TOTAL_SUPPLY, gas=1_000_000),
        call_request(sender, NATIVE_TOKEN_SENTINEL, ERC20_DECIMALS, gas=1_000_000),
        call_request(sender, NATIVE_TOKEN_SENTINEL, ERC20_NAME, gas=1_000_000),
        call_request(sender, NATIVE_TOKEN_SENTINEL, ERC20_SYMBOL, gas=1_000_000),
    ]


def build_contract_multicall_boundary_batches(
    sender: str,
) -> dict[str, list[dict[str, Any]]]:
    target, calldata, _expected = STANDARD_PRECOMPILE_VECTORS["identity_deadbeef"]
    request = call_request(sender, target, calldata, gas=1_000_000)
    explicit_nonce = dict(request)
    explicit_nonce["nonce"] = "0x1"
    return {
        "empty": [],
        "above_32": [dict(request) for _index in range(33)],
        "explicit_nonce": [explicit_nonce],
    }


def build_contract_multicall_override_fixture(
    sender: str, target: str, block_number: int
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    environment = environment_probe_request(sender, block_number)
    environment["gasPrice"] = "0x7c"
    zero_price_environment = environment_probe_request(sender, block_number)
    calls = [
        environment,
        zero_price_environment,
        blockhash_probe_request(sender, block_number - 1),
        call_request(sender, target, gas=1_000_000),
        call_request(sender, target, gas=1_000_000),
        extcodehash_probe_request(sender, target),
    ]
    return calls, {
        address(target): {
            "code": STATE_OVERRIDE_COUNTER_CODE,
            "state": {"0x" + "00" * 32: abi_words(7)},
        }
    }


def run_native_sentinel_multicall(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    anchor_hash: str,
    anchor_timestamp: int,
    sender: str,
    limit: int | None = None,
) -> None:
    """Compare the DeBank native-token pseudo contract through both batch APIs."""

    calls = build_native_sentinel_calls(sender)
    if limit is not None:
        calls = calls[:limit]
    selector = hex(block_number)
    context = {"block_id": selector, "type": "Equals"}
    expected = reference.capture(
        "eth_multiCall", [calls, selector, False, False, True, None, None]
    )
    actual = leafage.capture(
        "contractMultiCall", [calls, context, None, None, False, False, True]
    )
    if not expected.get("ok") or not actual.get("ok"):
        report.add(
            "native_sentinel", "rpc", False, expected, actual,
            "The sentinel is a batch-API pseudo contract; eth_call is not its oracle.",
        )
        return
    try:
        expected_payload = expected.get("result")
        actual_payload = actual.get("result")
        if not isinstance(expected_payload, dict) or not isinstance(actual_payload, dict):
            raise ValueError("native sentinel response must be an object")
        expected_results = expected_payload.get("results")
        actual_results = actual_payload.get("results")
        if not isinstance(expected_results, list) or not isinstance(actual_results, list):
            raise ValueError("native sentinel results must be an array")
        report.add(
            "native_sentinel",
            "result_count",
            len(expected_results) == len(actual_results) == len(calls),
            len(expected_results),
            len(actual_results),
        )
        if len(expected_results) != len(actual_results):
            return
        for source, results in (("writer", expected_results), ("leafage", actual_results)):
            for item in results:
                if not isinstance(item, dict):
                    raise ValueError(f"{source} native sentinel item must be an object")
                code = item.get("code")
                if isinstance(code, bool) or not isinstance(code, int):
                    raise ValueError(f"{source} native sentinel code must be an integer")
        expected_normalized = [
            {
                "success": item.get("code") == 0,
                "result": data(item.get("result", "0x")),
                "gas_used": quantity(item.get("gasUsed")),
            }
            for item in expected_results
        ]
        actual_normalized = [
            {
                "success": item.get("code") == 0,
                "result": data(item.get("result", "0x")),
                "gas_used": quantity(item.get("gas_used")),
            }
            for item in actual_results
        ]
        report.add(
            "native_sentinel",
            "results",
            actual_normalized == expected_normalized,
            expected_normalized,
            actual_normalized,
            "This checks the existing DeBank pseudo-token API, not Arc USDC supply semantics.",
        )
        if len(calls) == 5:
            fixed = [None, abi_words(1), abi_words(18), abi_string("ETH"), abi_string("ETH")]
            report.add(
                "native_sentinel",
                "compatibility_metadata",
                [item["result"] for item in expected_normalized[1:]] == fixed[1:],
                fixed[1:],
                [item["result"] for item in expected_normalized[1:]],
                "totalSupply=1/name=ETH are legacy pseudo-token compatibility values.",
            )

        expected_stats = expected_payload.get("stats")
        actual_stats = actual_payload.get("stats")
        if not isinstance(expected_stats, dict) or not isinstance(actual_stats, dict):
            raise ValueError("native sentinel stats must be an object")
        normalized_expected_stats = {
            "block_num": quantity(expected_stats.get("blockNum")),
            "block_hash": block_hash(expected_stats.get("blockHash")),
            "block_time": quantity(expected_stats.get("blockTime")),
            "success": expected_stats.get("success"),
        }
        normalized_actual_stats = {
            "block_num": quantity(actual_stats.get("block_num")),
            "block_hash": block_hash(actual_stats.get("block_hash")),
            "block_time": quantity(actual_stats.get("block_time")),
            "success": actual_stats.get("success"),
        }
        required_stats = {
            "block_num": block_number,
            "block_hash": anchor_hash,
            "block_time": anchor_timestamp,
            "success": True,
        }
        report.add(
            "native_sentinel",
            "stats",
            normalized_expected_stats == normalized_actual_stats == required_stats,
            required_stats,
            {"writer": normalized_expected_stats, "leafage": normalized_actual_stats},
        )
        report.add(
            "native_sentinel",
            "cache_metadata",
            expected_stats.get("cacheEnabled") is False
            and actual_stats.get("cache_enabled") is False
            and all(item.get("fromCache") is False for item in expected_results)
            and all(item.get("from_cache") is False for item in actual_results),
            {"cache_enabled": False, "from_cache": [False] * len(actual_results)},
            {
                "writer": {
                    "cache_enabled": expected_stats.get("cacheEnabled"),
                    "from_cache": [item.get("fromCache") for item in expected_results],
                },
                "leafage": {
                    "cache_enabled": actual_stats.get("cache_enabled"),
                    "from_cache": [item.get("from_cache") for item in actual_results],
                },
            },
        )
    except (KeyError, TypeError, ValueError) as error:
        report.add("native_sentinel", "schema", False, "valid response", str(error))


def compare_simulation_traces(
    report: Report,
    reference: RpcClient,
    block_number: int,
    name: str,
    calls: list[dict[str, Any]],
    leafage_results: list[dict[str, Any]],
) -> None:
    """Compare successful simulation frames with writer state-H/H+1 traces."""

    expected = reference.capture(
        "pre_traceMany", [calls, hex(block_number + 1), None, None]
    )
    if not expected.get("ok"):
        report.complete = False
        report.errors.append(f"reference pre_traceMany failed for {name}")
        report.add("simulate_trace", f"{name}.reference", False, "success", expected)
        return
    try:
        reference_results = expected.get("result")
        if not isinstance(reference_results, list):
            raise ValueError("pre_traceMany result must be an array")
        report.add(
            "simulate_trace",
            f"{name}.result_count",
            len(reference_results) == len(leafage_results) == len(calls),
            len(calls),
            {"reference": len(reference_results), "leafage": len(leafage_results)},
        )
        if len(reference_results) != len(leafage_results):
            return
        for index, (reference_result, leafage_result) in enumerate(
            zip(reference_results, leafage_results)
        ):
            if not isinstance(reference_result, dict) or not isinstance(
                leafage_result, dict
            ):
                raise ValueError("simulation trace result must be an object")
            reference_success = reference_result.get("error") is None
            leafage_success = leafage_result.get("code") == 0
            report.add(
                "simulate_trace",
                f"{name}.{index}.status",
                reference_success == leafage_success,
                reference_success,
                leafage_success,
            )
            # pre_traceMany intentionally drops failed traces and their gas.
            # Failure detail is compared through eth_simulateV1 and Leafage's
            # own error contract instead of treating an empty trace as proof.
            if not reference_success or not leafage_success:
                continue
            expected_traces = normalize_reference_traces(reference_result.get("trace"))
            actual_traces = normalize_leafage_traces(leafage_result.get("traces"))
            if name == "nested_blocklist_revert":
                # DebankTrace has no failure field and the writer intentionally
                # omits failed children from this schema. Compare the complete
                # top-level frame while the reference preflight separately
                # locks the reverted child and its ABI error output.
                expected_traces = [
                    trace for trace in expected_traces if trace["path"] == []
                ]
            report.add(
                "simulate_trace",
                f"{name}.{index}.frames",
                actual_traces == expected_traces,
                expected_traces,
                actual_traces,
            )
            expected_gas = quantity(reference_result.get("gasUsed"))
            actual_gas = quantity(leafage_result.get("gas_used"))
            report.add(
                "simulate_trace",
                f"{name}.{index}.transaction_gas",
                actual_gas == expected_gas,
                expected_gas,
                actual_gas,
            )
    except (KeyError, TypeError, ValueError) as error:
        report.add(
            "simulate_trace", f"{name}.schema", False, "valid response", str(error)
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
    compare_traces: bool = False,
    fixed_outputs: list[str | None] | None = None,
    use_hash_context: bool = False,
) -> None:
    anchor_selector = anchor_hash if use_hash_context else hex(block_number)
    context = {"block_id": anchor_selector, "type": "Equals"}
    reference_result = reference.capture(
        "eth_simulateV1",
        [
            {
                "blockStateCalls": [
                    {"blockOverrides": block_overrides, "calls": calls}
                ],
                "validation": False,
                "traceTransfers": False,
            },
            anchor_selector,
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
        reference_return_data = [
            data(item.get("returnData", "0x")) for item in reference_calls
        ]
        expected_outputs = [
            output if success else None
            for output, success in zip(reference_return_data, expected_status)
        ]
        expected_errors = [
            not success and item.get("error") is not None
            for item, success in zip(reference_calls, expected_status)
        ]
        expected_gas = [quantity(item.get("gasUsed")) for item in reference_calls]
        expected_logs = [normalize_logs(item.get("logs", [])) for item in reference_calls]

        required_outputs = fixed_outputs
        if required_outputs is None:
            required_output = expected_fixture_output(name, block_number)
            required_outputs = None if required_output is None else [required_output]
        if required_outputs is not None:
            fixed_matches = len(required_outputs) == len(expected_outputs) and all(
                fixed is None or actual == fixed
                for fixed, actual in zip(required_outputs, expected_outputs)
            )
            report.add(
                "fixture",
                f"reference_{name}_fixed_outputs",
                fixed_matches,
                required_outputs,
                expected_outputs,
                "State-dependent outputs are fixed only at the audited Arc mainnet anchor.",
            )

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
                expected_status == [True, True, True]
                and system_log_per_call == [True, False, True],
                {"status": [True, True, True], "system_logs": [True, False, True]},
                {"status": expected_status, "system_logs": system_log_per_call},
            )
            report.add(
                "fixture",
                "reference_native_transfer_is_visible_to_next_call",
                expected_outputs[1] == abi_words(1),
                abi_words(1),
                expected_outputs[1],
                "The middle BALANCE probe prevents independent-per-call execution from passing.",
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
        elif expected_stateful_system_log_counts(name) is not None:
            required_status = expected_stateful_statuses(name, len(calls))
            system_log_counts = [
                sum(log["address"] == SYSTEM_ADDRESS for log in logs)
                for logs in expected_logs
            ]
            required_log_counts = expected_stateful_system_log_counts(name)
            report.add(
                "fixture",
                f"reference_{name}_stateful_contract",
                expected_status == required_status
                and system_log_counts == required_log_counts,
                {
                    "status": required_status,
                    "system_log_counts": required_log_counts,
                },
                {
                    "status": expected_status,
                    "system_log_counts": system_log_counts,
                },
            )
            if name == "fee_then_balance":
                report.add(
                    "fixture",
                    "reference_fee_then_balance_intrinsic_gas",
                    expected_gas[0] == FEE_STATE_GAS_USED,
                    FEE_STATE_GAS_USED,
                    expected_gas[0],
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
            reference_error = reference_calls[0].get("error")
            reference_reason = (
                str(reference_error.get("message", ""))
                if isinstance(reference_error, dict)
                else str(reference_error or "")
            )
            actual_reasons = [str(item.get("err", "")) for item in leafage_calls]
            report.add(
                "simulate",
                "failure_then_p256.revert_reason",
                ERC20_INSUFFICIENT_BALANCE_REASON in reference_reason
                and all(
                    ERC20_INSUFFICIENT_BALANCE_REASON in reason
                    for reason in actual_reasons
                ),
                ERC20_INSUFFICIENT_BALANCE_REASON,
                {"reference": reference_reason, "leafage": actual_reasons},
            )
        elif name in {"log_then_revert", "failed_create_log_revert"}:
            required_codes = (
                [0, LEAFAGE_EVM_REVERT]
                if name == "log_then_revert"
                else [LEAFAGE_EVM_REVERT]
            )
            report.add(
                "simulate",
                f"{name}.error_code",
                actual_codes == required_codes,
                required_codes,
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
        failed_trace_shapes: list[dict[str, Any]] = []
        failed_trace_ok = True
        for index, (result, success) in enumerate(zip(leafage_calls, actual_status)):
            if success:
                continue
            traces = result.get("traces")
            if not isinstance(traces, list):
                failed_trace_ok = False
                failed_trace_shapes.append({"index": index, "error": "not an array"})
                continue
            expected_index = 0 if name == "failure_then_p256" else index
            expected_output = reference_return_data[expected_index]
            request = calls[expected_index]
            try:
                normalized_traces = normalize_leafage_traces(traces)
            except ValueError as error:
                failed_trace_ok = False
                failed_trace_shapes.append({"index": index, "error": str(error)})
                continue
            root = next(item for item in normalized_traces if item["path"] == [])
            expected_kind = "call" if "to" in request else "create"
            expected_target = address(request["to"]) if "to" in request else root["to"]
            item_ok = (
                root["kind"] == expected_kind
                and root["from"] == address(request.get("from"))
                and root["to"] == expected_target
                and root["value"] == quantity(request.get("value", "0x0"))
                and root["input"] == data(request.get("data", "0x"))
                and root["output"] == expected_output
                and root["gas_limit"] is not None
                and root["gas_used"] is not None
            )
            failed_trace_ok = failed_trace_ok and item_ok
            failed_trace_shapes.append(
                {
                    "index": index,
                    "trace_count": len(normalized_traces),
                    "root": root,
                    "expected": {
                        "kind": expected_kind,
                        "from": address(request.get("from")),
                        "to": expected_target,
                        "value": quantity(request.get("value", "0x0")),
                        "input": data(request.get("data", "0x")),
                        "output": expected_output,
                    },
                }
            )
        if failed_trace_shapes:
            report.add(
                "simulate",
                f"{name}.failed_trace_root",
                failed_trace_ok,
                "one failed root trace preserving the revert output",
                failed_trace_shapes,
                "Full failed child-frame coverage remains in the in-tree Rust fixtures.",
            )

        actual_gas = [quantity(item.get("gas_used")) for item in leafage_calls]
        report.add(
            "simulate",
            f"{name}.gas_used",
            actual_gas == expected_leafage_gas,
            expected_leafage_gas,
            actual_gas,
        )

        event_attachments = [
            leafage_event_attachments(item.get("traces", []), item.get("events", []))
            for item in leafage_calls
        ]
        report.add(
            "simulate",
            f"{name}.event_member_structure",
            True,
            "every event references an existing trace and has a unique member position",
            event_attachments,
        )
        actual_logs = [
            normalize_leafage_events(item.get("events", [])) for item in leafage_calls
        ]
        report.add(
            "simulate",
            f"{name}.logs",
            actual_logs == expected_leafage_logs,
            expected_leafage_logs,
            actual_logs,
        )

        required_layout = expected_stateful_event_layout(name)
        if required_layout is not None:
            actual_layout = stateful_event_layout(name, leafage_calls)
            report.add(
                "simulate",
                f"{name}.event_frame_layout",
                actual_layout == required_layout,
                required_layout,
                actual_layout,
                "This locks the EIP-7708 event parent and shared trace/event position.",
            )
        if name == "sstore_sequence":
            storage_flags = []
            for result in leafage_calls[1:]:
                traces = result.get("traces")
                if not isinstance(traces, list):
                    raise ValueError("SSTORE fixture traces must be an array")
                roots = [
                    trace
                    for trace in traces
                    if trace.get("parent_trace_id", "") == ""
                ]
                if len(roots) != 1:
                    raise ValueError("SSTORE fixture must have one root trace")
                root = roots[0]
                storage_flags.append(
                    {
                        "self_storage_change": root.get("self_storage_change"),
                        "storage_change": root.get("storage_change"),
                    }
                )
            required_flags = [
                {"self_storage_change": True, "storage_change": True},
                {"self_storage_change": True, "storage_change": True},
            ]
            report.add(
                "simulate",
                "sstore_sequence.storage_flags",
                storage_flags == required_flags,
                required_flags,
                storage_flags,
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
        if compare_traces:
            compare_simulation_traces(
                report,
                reference,
                block_number,
                name,
                calls,
                leafage_calls,
            )
    except (KeyError, TypeError, ValueError) as error:
        report.add("simulate", f"{name}.schema", False, "valid response", str(error))


def run_simulation_environment_overrides(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    sender: str,
    block_overrides: dict[str, Any],
    anchor_timestamp: int,
    anchor_hash: str,
) -> None:
    """Prove every simulation BlockOverride, including Arc's fixed gas-limit rule."""

    request = environment_probe_request(sender, block_number)
    anchor_block = reference.call(
        "eth_getBlockByNumber", [hex(block_number), False]
    )
    if not isinstance(anchor_block, dict):
        raise ValueError("simulation environment anchor block is missing")
    canonical_previous_hash = block_hash(anchor_block.get("parentHash"))
    history_request = call_request(
        sender,
        HISTORY_STORAGE,
        uint256_calldata(block_number),
        gas=1_000_000,
    )
    previous_blockhash_request = blockhash_probe_request(sender, block_number - 1)
    distinctive = environment_probe_overrides(block_overrides)
    # Arc eth_simulateV1 requires the protocol 30M block gas limit.  The first
    # probe therefore changes every other field and compares the entire output.
    distinctive["gasLimit"] = block_overrides["gasLimit"]
    run_simulation(
        report,
        leafage,
        reference,
        block_number,
        "query_environment_overrides",
        [request, history_request, previous_blockhash_request],
        distinctive,
        anchor_timestamp,
        anchor_hash,
        fixed_outputs=[
            environment_probe_output(block_number, distinctive),
            anchor_hash,
            canonical_previous_hash,
        ],
    )

    # Standard eth_simulateV1 rejects a non-protocol gasLimit while Leafage's
    # custom simulation contract deliberately disables the block-gas check.
    # Use writer eth_call as the execution oracle and compare the GASLIMIT
    # opcode instead of treating the standard API's validity rule as canonical.
    gas_limit_overrides = dict(block_overrides)
    gas_limit_overrides["gasLimit"] = hex(25_000_000)
    expected = reference.capture(
        "eth_call",
        writer_call_params(request, block_number, None, gas_limit_overrides),
    )
    expected_words: list[int] | None = None
    if expected.get("ok"):
        try:
            output = data(expected.get("result"))
            expected_words = [abi_word_at(output, index) for index in (0, 1, 2, 3, 5, 6)]
        except ValueError:
            expected_words = None
    report.add(
        "fixture",
        "reference_query_environment_gas_limit_override",
        expected_words is not None and expected_words[3] == 25_000_000,
        {"gasLimit": 25_000_000},
        expected,
        "eth_call is the oracle for the custom simulation validity policy.",
    )

    context = {"block_id": hex(block_number), "type": "Equals"}
    actual = leafage.capture(
        "simulateTransactions", [[request], context, gas_limit_overrides]
    )
    try:
        if not actual.get("ok"):
            raise ValueError(f"Leafage simulation failed: {actual.get('error')}")
        payload = actual.get("result")
        if not isinstance(payload, dict):
            raise ValueError("Leafage simulation response is not an object")
        results = payload.get("results")
        if not isinstance(results, list) or len(results) != 1:
            raise ValueError("Leafage simulation must return one result")
        result = results[0]
        if not isinstance(result, dict) or result.get("code") != 0:
            raise ValueError("Leafage environment probe did not succeed")
        actual_output = leafage_root_output(result)
        actual_words = [
            abi_word_at(actual_output, index) for index in (0, 1, 2, 3, 5, 6)
        ]
        report.add(
            "simulate",
            "query_environment_gas_limit_override.words",
            expected_words is not None
            and actual_words == expected_words
            and abi_word_at(actual_output, 4)
            == quantity(gas_limit_overrides["baseFee"]),
            {
                "writer_non_basefee_words": expected_words,
                "simulation_basefee": quantity(gas_limit_overrides["baseFee"]),
            },
            {
                "leafage_non_basefee_words": actual_words,
                "simulation_basefee": abi_word_at(actual_output, 4),
            },
        )
    except (KeyError, TypeError, ValueError) as error:
        report.add(
            "simulate",
            "query_environment_gas_limit_override.schema",
            False,
            "one successful environment probe",
            str(error),
        )


def arc_next_base_fee_from_extra_data(extra_data: Any) -> int:
    raw = bytes.fromhex(data(extra_data)[2:])
    if len(raw) < 8:
        raise ValueError("Arc header extraData does not contain nextBaseFee")
    return int.from_bytes(raw[-8:], "big")


def single_leafage_simulation_output(
    leafage: RpcClient,
    request: dict[str, Any],
    block_number: int,
    block_overrides: dict[str, Any] | None,
) -> tuple[str, dict[str, Any]]:
    context = {"block_id": hex(block_number), "type": "Equals"}
    captured = leafage.capture(
        "simulateTransactions", [[request], context, block_overrides]
    )
    if not captured.get("ok"):
        raise ValueError(f"Leafage simulation failed: {captured.get('error')}")
    payload = captured.get("result")
    if not isinstance(payload, dict):
        raise ValueError("Leafage simulation response is not an object")
    results = payload.get("results")
    if not isinstance(results, list) or len(results) != 1:
        raise ValueError("Leafage simulation must return one result")
    result = results[0]
    if not isinstance(result, dict) or result.get("code") != 0:
        raise ValueError("Leafage simulation result did not succeed")
    return leafage_root_output(result), payload


def run_simulation_default_environment(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    sender: str,
    anchor_block: dict[str, Any],
) -> None:
    """Compare BlockOverrides=None against the writer's state-H call environment."""

    if block_number < 1:
        raise ValueError("default environment fixture requires a parent block")
    parent_hash = block_hash(anchor_block.get("parentHash"))
    base_fee = quantity(anchor_block.get("baseFeePerGas", "0x0"))
    request = environment_probe_request(sender, block_number - 1)
    request["gasPrice"] = hex(base_fee + 1)
    expected = reference.capture("eth_call", [request, hex(block_number)])
    expected_output: str | None = None
    if expected.get("ok"):
        try:
            expected_output = data(expected.get("result"))
        except ValueError:
            expected_output = None
    required_output = environment_probe_output(
        block_number - 1,
        {
            "number": hex(block_number),
            "time": anchor_block.get("timestamp"),
            "gasLimit": anchor_block.get("gasLimit"),
            "coinbase": address(anchor_block.get("miner")),
            "random": block_hash(anchor_block.get("mixHash")),
            "baseFee": hex(base_fee),
            "blockHash": {str(block_number - 1): parent_hash},
        },
    )
    report.add(
        "fixture",
        "reference_default_simulation_environment",
        expected_output == required_output,
        required_output,
        expected,
    )
    try:
        actual_output, payload = single_leafage_simulation_output(
            leafage, request, block_number, None
        )
        stats = payload.get("stats")
        if not isinstance(stats, dict):
            raise ValueError("Leafage simulation stats are not an object")
        report.add(
            "simulate",
            "default_environment.output",
            actual_output == required_output,
            required_output,
            actual_output,
        )
        report.add(
            "simulate",
            "default_environment.stats",
            quantity(stats.get("block_num")) == block_number
            and block_hash(stats.get("block_hash"))
            == block_hash(anchor_block.get("hash"))
            and quantity(stats.get("block_time"))
            == quantity(anchor_block.get("timestamp")),
            {
                "block_num": block_number,
                "block_hash": block_hash(anchor_block.get("hash")),
                "block_time": quantity(anchor_block.get("timestamp")),
            },
            stats,
        )
    except (KeyError, TypeError, ValueError) as error:
        report.add(
            "simulate",
            "default_environment.schema",
            False,
            "one successful simulation with BlockOverrides=None",
            str(error),
        )


def run_simulation_derived_next_base_fee(
    report: Report,
    leafage: RpcClient,
    block_number: int,
    sender: str,
    anchor_block: dict[str, Any],
    next_block: dict[str, Any],
    block_overrides: dict[str, Any],
) -> None:
    """Check Arc's nextBaseFee derivation when the override omits baseFee."""

    derived = arc_next_base_fee_from_extra_data(anchor_block.get("extraData"))
    next_base_fee = quantity(next_block.get("baseFeePerGas", "0x0"))
    report.add(
        "fixture",
        "next_base_fee_extra_data_matches_header",
        derived == next_base_fee,
        next_base_fee,
        derived,
    )
    overrides_without_base_fee = {
        key: value for key, value in block_overrides.items() if key != "baseFee"
    }
    expected_environment = dict(overrides_without_base_fee)
    expected_environment["baseFee"] = hex(derived)
    request = environment_probe_request(sender, block_number)
    try:
        actual_output, _payload = single_leafage_simulation_output(
            leafage, request, block_number, overrides_without_base_fee
        )
        expected_output = environment_probe_output(
            block_number, expected_environment
        )
        report.add(
            "simulate",
            "derived_next_base_fee.output",
            actual_output == expected_output,
            expected_output,
            actual_output,
            "The oracle is the canonical H+1 header and Arc parent extraData, not "
            "standard eth_simulateV1 fee defaults.",
        )
    except (KeyError, TypeError, ValueError) as error:
        report.add(
            "simulate",
            "derived_next_base_fee.schema",
            False,
            "one successful simulation without an explicit baseFee override",
            str(error),
        )


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


def reference_call_with_overrides(
    reference: RpcClient,
    request: dict[str, Any],
    block_number: int,
    block_overrides: dict[str, Any],
    gas_limit: int,
) -> dict[str, Any]:
    return reference.capture(
        "eth_call",
        writer_call_params(
            {**request, "gas": hex(gas_limit)},
            block_number,
            None,
            block_overrides,
        ),
    )


def reference_estimate_with_block_overrides(
    reference: RpcClient,
    request: dict[str, Any],
    block_number: int,
    block_overrides: dict[str, Any],
) -> int:
    """Reproduce Arc/Reth's estimate search using writer eth_call probes."""

    hard_cap = min(EIP7825_TX_GAS_LIMIT, quantity(block_overrides["gasLimit"]))
    request_limit = quantity(request["gas"]) if "gas" in request else hard_cap
    highest = min(hard_cap, request_limit)
    initial_request = {**request, "gas": hex(highest)}
    initial_call = reference.capture(
        "eth_call",
        writer_call_params(initial_request, block_number, None, block_overrides),
    )
    if not initial_call.get("ok"):
        raise ValueError(f"writer estimate cap probe failed: {initial_call.get('error')}")
    traced = reference.capture(
        "debug_traceCall",
        [
            initial_request,
            hex(block_number),
            writer_trace_options(None, block_overrides),
        ],
    )
    if not traced.get("ok") or not isinstance(traced.get("result"), dict):
        raise ValueError(f"writer estimate trace failed: {traced}")
    trace = traced["result"]
    if trace.get("error") is not None:
        raise ValueError(f"writer estimate trace reverted: {trace.get('error')}")
    gas_used = quantity(trace.get("gasUsed"))
    lowest = gas_used - 1 if gas_used else 0

    optimistic = (gas_used + 2_300) * 64 // 63
    if optimistic < highest:
        optimistic_result = reference_call_with_overrides(
            reference, request, block_number, block_overrides, optimistic
        )
        if not optimistic_result.get("ok"):
            raise ValueError(
                f"writer optimistic estimate probe failed: {optimistic_result.get('error')}"
            )
        highest = optimistic

    middle = min(gas_used * 3, (highest + lowest) // 2)
    while lowest + 1 < highest:
        if (highest - lowest) / highest < 0.015:
            break
        result = reference_call_with_overrides(
            reference, request, block_number, block_overrides, middle
        )
        # Arc/Reth treats any lower-gas execution failure as a lower-bound
        # probe. The initial cap probe above has already proven the request is
        # semantically valid under the same environment.
        if result.get("ok"):
            highest = middle
        else:
            error_class = rpc_error_class(result)
            if error_class not in {"revert", "out-of-gas"}:
                raise ValueError(
                    f"writer estimate probe returned {error_class}: {result.get('error')}"
                )
            lowest = middle
        middle = (highest + lowest) // 2
    return highest


def run_estimate_with_block_overrides(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    sender: str,
    block_overrides: dict[str, Any],
    name: str = "block_overrides",
    canonical_previous_hash: str | None = None,
) -> None:
    request = environment_guard_request(
        sender, block_number, block_overrides, canonical_previous_hash
    )
    without_override = reference.capture(
        "eth_call",
        [{**request, "gas": hex(EIP7825_TX_GAS_LIMIT)}, hex(block_number)],
    )
    report.add(
        "fixture",
        "estimate_block_override_is_required",
        not without_override.get("ok") and rpc_error_class(without_override) == "revert",
        "revert unless every distinctive BlockOverride is applied",
        without_override,
    )
    try:
        expected_gas = reference_estimate_with_block_overrides(
            reference, request, block_number, block_overrides
        )
    except ValueError as error:
        report.complete = False
        report.errors.append(str(error))
        report.add(
            "estimate",
            f"{name}.reference_search",
            False,
            "successful deterministic writer search",
            str(error),
        )
        return

    context = {"block_id": hex(block_number), "type": "Equals"}
    actual = leafage.capture(
        "estimateGas", [request, context, block_overrides]
    )
    actual_gas: int | None = None
    if actual.get("ok"):
        try:
            actual_gas = quantity(actual.get("result"))
        except ValueError:
            actual_gas = None
    report.add(
        "estimate",
        f"{name}.gas",
        actual_gas == expected_gas,
        expected_gas,
        actual,
        "The expected value is the Arc/Reth search reproduced with writer eth_call probes.",
    )
    if actual_gas is None:
        return
    replay = reference_call_with_overrides(
        reference, request, block_number, block_overrides, actual_gas
    )
    report.add(
        "estimate",
        f"{name}.reference_executes_with_leafage_limit",
        replay.get("ok") is True,
        "successful writer eth_call with the same BlockOverrides",
        replay,
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
    anchor_hash: str | None = None,
) -> None:
    if not preserve_gas:
        request = {key: value for key, value in request.items() if key != "gas"}
    reference_selector: Any = (
        {"blockHash": anchor_hash, "requireCanonical": True}
        if anchor_hash is not None
        else hex(block_number)
    )
    context = {
        "block_id": anchor_hash if anchor_hash is not None else hex(block_number),
        "type": "Equals",
    }
    expected = reference.capture("eth_estimateGas", [request, reference_selector])
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
        "eth_call", [{**request, "gas": hex(actual_gas)}, reference_selector]
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
    elif error_class == "revert":
        required_reason = (
            ERC20_INSUFFICIENT_BALANCE_REASON.lower()
            if name == "contract_revert"
            else None
        )
        reference_matches = "execution reverted" in reference_message and (
            required_reason is None or required_reason in reference_message
        )
        leafage_message = (
            str(leafage_error.get("message", "")).lower()
            if isinstance(leafage_error, dict)
            else ""
        )
        leafage_matches = (
            isinstance(leafage_error, dict)
            and leafage_error.get("code") == LEAFAGE_EVM_REVERT
            and "revert" in leafage_message
            and (required_reason is None or required_reason in leafage_message)
        )
        expected_leafage_code = LEAFAGE_EVM_REVERT
    elif error_class == "fee_conflict":
        reference_matches = (
            isinstance(reference_error, dict)
            and reference_error.get("code") == LEAFAGE_INVALID_PARAMS
            and reference_message
            == "both gasprice and (maxfeepergas or maxpriorityfeepergas) specified"
        )
        leafage_matches = (
            isinstance(leafage_error, dict)
            and leafage_error.get("code") == LEAFAGE_INVALID_PARAMS
            and str(leafage_error.get("message", "")).lower()
            == "invalid fee parameters"
        )
        expected_leafage_code = LEAFAGE_INVALID_PARAMS
    elif error_class == "authorization":
        reference_matches = (
            isinstance(reference_error, dict)
            and reference_error.get("code") == -32003
            and reference_message == "eip-7702 authorization list has invalid fields"
        )
        leafage_matches = (
            isinstance(leafage_error, dict)
            and leafage_error.get("code") == LEAFAGE_EVM_FAILED
            and "authorization" in str(leafage_error.get("message", "")).lower()
        )
        expected_leafage_code = LEAFAGE_EVM_FAILED
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


def blocked_error_matches(result: dict[str, Any], code: int | None = None) -> bool:
    if result.get("ok"):
        return False
    error = result.get("error")
    return (
        isinstance(error, dict)
        and (code is None or error.get("code") == code)
        and "blocked address" in str(error.get("message", "")).lower()
    )


def blocked_reference_results(
    reference: RpcClient,
    request: dict[str, Any],
    block_number: int,
    block_overrides: dict[str, Any],
) -> dict[str, dict[str, Any]]:
    estimate_request = {key: value for key, value in request.items() if key != "gas"}
    return {
        "contract_multicall": reference.capture(
            "eth_call", [request, hex(block_number)]
        ),
        "simulate": reference.capture(
            "eth_simulateV1",
            [
                {
                    "blockStateCalls": [
                        {"blockOverrides": block_overrides, "calls": [request]}
                    ],
                    "validation": False,
                    "traceTransfers": False,
                },
                hex(block_number),
            ],
        ),
        "estimate": reference.capture(
            "eth_estimateGas", [estimate_request, hex(block_number)]
        ),
    }


def run_blocked_execution_rejections(
    report: Report,
    leafage: RpcClient,
    reference: RpcClient,
    block_number: int,
    funded: str,
    empty: str,
    block_overrides: dict[str, Any],
) -> None:
    context = {"block_id": hex(block_number), "type": "Equals"}
    requests = {
        "sender": call_request(USDC, empty, gas=1_000_000),
        "receiver": call_request(funded, USDC, value=1, gas=1_000_000),
    }
    for role, request in requests.items():
        expected = blocked_reference_results(
            reference, request, block_number, block_overrides
        )
        for endpoint, result in expected.items():
            report.add(
                "fixture",
                f"reference_blocked_{role}.{endpoint}",
                blocked_error_matches(result),
                "Blocked address",
                result,
            )

        actual = {
            "contract_multicall": leafage.capture(
                "contractMultiCall",
                [[request], context, None, None, False, False, False],
            ),
            "simulate": leafage.capture(
                "simulateTransactions", [[request], context, block_overrides]
            ),
            "estimate": leafage.capture(
                "estimateGas",
                [
                    {key: value for key, value in request.items() if key != "gas"},
                    context,
                    None,
                ],
            ),
        }
        for endpoint, result in actual.items():
            report.add(
                endpoint,
                f"blocked_{role}.rejected",
                blocked_error_matches(result, LEAFAGE_EVM_FAILED),
                {"code": LEAFAGE_EVM_FAILED, "message": "Blocked address"},
                result,
            )


def build_fixtures(
    sender: str, empty: str, block_number: int
) -> dict[str, list[dict[str, Any]]]:
    gas = 1_000_000
    fixtures = {
        "usdc_total_supply": [call_request(sender, USDC, TOTAL_SUPPLY, gas=gas)],
        "nca_total_supply": [
            call_request(sender, NATIVE_COIN_AUTHORITY, TOTAL_SUPPLY, gas=gas)
        ],
        "ncc_usdc_is_blocklisted": [
            call_request(
                sender,
                NATIVE_COIN_CONTROL,
                address_calldata(NCC_IS_BLOCKLISTED, USDC),
                gas=gas,
            )
        ],
        "ncc_funded_is_not_blocklisted": [
            call_request(
                sender,
                NATIVE_COIN_CONTROL,
                address_calldata(NCC_IS_BLOCKLISTED, sender),
                gas=gas,
            )
        ],
        "system_accounting_at_anchor": [
            call_request(
                sender,
                SYSTEM_ACCOUNTING,
                selector_uint_calldata(SYSTEM_GET_GAS_VALUES, block_number),
                gas=gas,
            )
        ],
        "protocol_fee_params": [
            call_request(sender, PROTOCOL_CONFIG, PROTOCOL_FEE_PARAMS, gas=gas)
        ],
        "protocol_consensus_params": [
            call_request(sender, PROTOCOL_CONFIG, PROTOCOL_CONSENSUS_PARAMS, gas=gas)
        ],
        "denylist_usdc": [
            call_request(
                sender,
                DENYLIST,
                address_calldata(DENYLIST_IS_DENYLISTED, USDC),
                gas=gas,
            )
        ],
        "active_validator_count": [
            call_request(sender, VALIDATOR_REGISTRY, ACTIVE_VALIDATOR_COUNT, gas=gas)
        ],
        "permit2_domain_separator": [
            call_request(sender, PERMIT2, PERMIT2_DOMAIN_SEPARATOR, gas=gas)
        ],
        "p256_valid": [call_request(sender, P256_PRECOMPILE, P256_VALID_INPUT, gas=gas)],
        "p256_invalid_wrong_hash": [
            call_request(sender, P256_PRECOMPILE, P256_INVALID_WRONG_HASH_INPUT, gas=gas)
        ],
        "pq_valid": [call_request(sender, PQ_PRECOMPILE, load_pq_valid_input(), gas=gas)],
        "pq_invalid_signature": [
            call_request(
                sender,
                PQ_PRECOMPILE,
                load_pq_invalid_signature_input(),
                gas=gas,
            )
        ],
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
            balance_probe_request(sender, empty, gas=gas),
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
    identity_target, identity_input, _identity_output = STANDARD_PRECOMPILE_VECTORS[
        "identity_deadbeef"
    ]
    access_list_request = call_request(
        sender, identity_target, identity_input, gas=gas
    )
    access_list_request["accessList"] = [
        {"address": identity_target, "storageKeys": []}
    ]
    fixtures["eip2930_access_list_identity"] = [access_list_request]
    if block_number == ARC_MAINNET_BASELINE_BLOCK:
        eip1559_request = {
            "from": address(sender),
            "data": EIP1559_EFFECTIVE_GASPRICE_GUARD_INIT,
            "value": "0x0",
            "gas": hex(gas),
        }
    else:
        eip1559_request = call_request(
            sender, identity_target, identity_input, gas=gas
        )
    eip1559_request.pop("gasPrice", None)
    eip1559_request["maxFeePerGas"] = hex(EIP1559_MAX_FEE)
    eip1559_request["maxPriorityFeePerGas"] = hex(EIP1559_PRIORITY_FEE)
    fixtures["eip1559_identity"] = [eip1559_request]
    for name, (target, calldata, _expected) in STANDARD_PRECOMPILE_VECTORS.items():
        fixtures[name] = [call_request(sender, target, calldata, gas=gas)]
    return fixtures


def build_asset_read_fixtures(sender: str) -> list[tuple[str, dict[str, Any]]]:
    fixtures: list[tuple[str, dict[str, Any]]] = []
    for label, token, holder, _name, _symbol, _decimals, _supply, _balance in (
        IMPORTANT_ERC20_ASSETS
    ):
        fixtures.extend(
            (
                (f"{label}.name", call_request(sender, token, ERC20_NAME, gas=1_000_000)),
                (
                    f"{label}.symbol",
                    call_request(sender, token, ERC20_SYMBOL, gas=1_000_000),
                ),
                (
                    f"{label}.decimals",
                    call_request(sender, token, ERC20_DECIMALS, gas=1_000_000),
                ),
                (
                    f"{label}.total_supply",
                    call_request(sender, token, TOTAL_SUPPLY, gas=1_000_000),
                ),
                (
                    f"{label}.balance",
                    call_request(
                        sender,
                        token,
                        erc20_balance_of_calldata(holder),
                        gas=1_000_000,
                    ),
                ),
            )
        )
        if label == "usdc":
            fixtures.append(
                (
                    "usdc.allowance_permit2",
                    call_request(
                        sender,
                        token,
                        two_address_calldata(ERC20_ALLOWANCE, holder, PERMIT2),
                        gas=1_000_000,
                    ),
                )
            )
    return fixtures


def expected_asset_outputs(block_number: int) -> dict[str, str]:
    if block_number != ARC_MAINNET_BASELINE_BLOCK:
        return {}
    expected: dict[str, str] = {}
    for label, _token, _holder, name, symbol, decimals, supply, balance in (
        IMPORTANT_ERC20_ASSETS
    ):
        expected.update(
            {
                f"{label}.name": abi_string(name),
                f"{label}.symbol": abi_string(symbol),
                f"{label}.decimals": abi_words(decimals),
                f"{label}.total_supply": abi_words(supply),
                f"{label}.balance": abi_words(balance),
            }
        )
    expected["usdc.allowance_permit2"] = abi_words(0)
    return expected


def run_reference_override_preflight(
    report: Report,
    reference: RpcClient,
    block_number: int,
    anchor_hash: str,
    sender: str,
    empty: str,
    block_overrides: dict[str, Any],
) -> None:
    anchor_block = reference.call(
        "eth_getBlockByNumber", [hex(block_number), False]
    )
    if not isinstance(anchor_block, dict):
        raise ValueError("reference override preflight anchor block is missing")
    canonical_previous_hash = block_hash(anchor_block.get("parentHash"))
    request = environment_probe_request(sender, block_number)
    history_request = call_request(
        sender,
        HISTORY_STORAGE,
        uint256_calldata(block_number),
        gas=1_000_000,
    )
    previous_blockhash_request = blockhash_probe_request(sender, block_number - 1)
    distinctive = environment_probe_overrides(block_overrides)
    simulation_overrides = dict(distinctive)
    simulation_overrides["gasLimit"] = block_overrides["gasLimit"]
    simulated = reference.capture(
        "eth_simulateV1",
        [
            {
                "blockStateCalls": [
                    {
                        "blockOverrides": simulation_overrides,
                        "calls": [request, history_request, previous_blockhash_request],
                    }
                ],
                "validation": False,
                "traceTransfers": False,
            },
            hex(block_number),
        ],
    )
    simulation_output: tuple[str, str, str] | None = None
    if simulated.get("ok"):
        try:
            blocks = simulated.get("result")
            if not isinstance(blocks, list) or len(blocks) != 1:
                raise ValueError("environment simulation must return one block")
            calls = blocks[0].get("calls")
            if not isinstance(calls, list) or len(calls) != 3:
                raise ValueError("environment simulation must return three calls")
            if any(quantity(call.get("status")) != 1 for call in calls):
                raise ValueError("environment simulation failed")
            simulation_output = (
                data(calls[0].get("returnData")),
                data(calls[1].get("returnData")),
                data(calls[2].get("returnData")),
            )
        except (KeyError, TypeError, ValueError):
            simulation_output = None
    report.add(
        "preflight",
        "query_environment.simulation_overrides",
        simulation_output
        == (
            environment_probe_output(block_number, simulation_overrides),
            anchor_hash,
            canonical_previous_hash,
        ),
        (
            environment_probe_output(block_number, simulation_overrides),
            anchor_hash,
            canonical_previous_hash,
        ),
        simulated,
        "BLOCKHASH uses the explicit mapping while EIP-2935 keeps the canonical parent hash.",
    )

    gas_limit_call = reference.capture(
        "eth_call",
        writer_call_params(request, block_number, None, distinctive),
    )
    gas_limit_word: int | None = None
    if gas_limit_call.get("ok"):
        try:
            gas_limit_word = abi_word_at(gas_limit_call.get("result"), 3)
        except ValueError:
            gas_limit_word = None
    report.add(
        "preflight",
        "query_environment.custom_simulation_gas_limit_oracle",
        gas_limit_word == quantity(distinctive["gasLimit"]),
        quantity(distinctive["gasLimit"]),
        gas_limit_call,
    )

    override_calls, state_override = build_contract_multicall_override_fixture(
        sender, empty, block_number
    )
    expected_outputs = [
        environment_probe_output(block_number, distinctive),
        environment_probe_output(block_number, distinctive, call_like=True),
        canonical_previous_hash,
        abi_words(7),
        abi_words(7),
        STATE_OVERRIDE_COUNTER_CODE_HASH,
    ]
    observed_outputs: list[str | None] = []
    for call in override_calls:
        result = reference.capture(
            "eth_call",
            writer_call_params(call, block_number, state_override, distinctive),
        )
        observed_outputs.append(
            data(result.get("result")) if result.get("ok") else None
        )
    report.add(
        "preflight",
        "contract_multicall.state_block_overrides_and_isolation",
        observed_outputs == expected_outputs,
        expected_outputs,
        observed_outputs,
    )

    guard = environment_guard_request(
        sender, block_number, distinctive, canonical_previous_hash
    )
    without_override = reference.capture(
        "eth_call",
        [{**guard, "gas": hex(EIP7825_TX_GAS_LIMIT)}, hex(block_number)],
    )
    try:
        expected_gas = reference_estimate_with_block_overrides(
            reference, guard, block_number, distinctive
        )
        replay = reference_call_with_overrides(
            reference, guard, block_number, distinctive, expected_gas
        )
    except ValueError as error:
        expected_gas = 0
        replay = {"ok": False, "error": {"message": str(error)}}
    report.add(
        "preflight",
        "estimate.block_overrides",
        rpc_error_class(without_override) == "revert"
        and expected_gas > 0
        and replay.get("ok") is True,
        "revert without override and executable deterministic estimate with override",
        {
            "without_override": without_override,
            "expected_gas": expected_gas,
            "replay": replay,
        },
    )

    h_plus_2 = dict(distinctive)
    h_plus_2["number"] = hex(block_number + 2)
    h_plus_2_probe = environment_probe_request(sender, block_number)
    h_plus_2_probe["gasPrice"] = hex(quantity(h_plus_2["baseFee"]) + 1)
    h_plus_2_call = reference.capture(
        "eth_call",
        writer_call_params(h_plus_2_probe, block_number, None, h_plus_2),
    )
    h_plus_2_guard = environment_guard_request(
        sender, block_number, h_plus_2, canonical_previous_hash
    )
    try:
        h_plus_2_gas = reference_estimate_with_block_overrides(
            reference, h_plus_2_guard, block_number, h_plus_2
        )
    except ValueError:
        h_plus_2_gas = 0
    h_plus_2_output = environment_probe_output(block_number, h_plus_2)
    report.add(
        "preflight",
        "query_environment.h_plus_2_call_and_estimate",
        h_plus_2_call.get("ok") is True
        and data(h_plus_2_call.get("result")) == h_plus_2_output
        and h_plus_2_gas > 0,
        {
            "number": block_number + 2,
            "output": h_plus_2_output,
            "positive_estimate": True,
        },
        {"call": h_plus_2_call, "estimate": h_plus_2_gas},
        "Call-like and estimate endpoints accept arbitrary explicit block numbers; "
        "only simulateTransactions requires H+1.",
    )

    for role, request in (
        ("sender", call_request(USDC, empty, gas=1_000_000)),
        ("receiver", call_request(sender, USDC, value=1, gas=1_000_000)),
    ):
        blocked = blocked_reference_results(
            reference, request, block_number, block_overrides
        )
        report.add(
            "preflight",
            f"blocked_{role}.all_execution_apis",
            all(blocked_error_matches(result) for result in blocked.values()),
            {name: "Blocked address" for name in blocked},
            blocked,
        )


def run_reference_preflight(
    report: Report,
    reference: RpcClient,
    block_number: int,
    funded_address: str | None,
    search_depth: int,
) -> None:
    """Validate the fixed writer oracle without claiming a Leafage comparison."""

    report.mode = "reference-preflight"
    version = reference.call("web3_clientVersion", [])
    if not isinstance(version, str):
        raise ValueError("reference web3_clientVersion must be a string")
    report.clients = {"reference": version}
    chain_id = quantity(reference.call("eth_chainId", []))
    report.add("preflight", "chain_id", chain_id == ARC_CHAIN_ID, ARC_CHAIN_ID, chain_id)
    if chain_id != ARC_CHAIN_ID:
        raise RuntimeError("reference chain ID mismatch")

    block = reference.call("eth_getBlockByNumber", [hex(block_number), True])
    next_block = reference.call("eth_getBlockByNumber", [hex(block_number + 1), False])
    if not isinstance(block, dict) or not isinstance(next_block, dict):
        raise RuntimeError("reference anchor or successor block is missing")
    anchor_hash = block_hash(block.get("hash"))
    next_hash = block_hash(next_block.get("hash"))
    report.anchors = {
        "height": block_number,
        "hash": anchor_hash,
        "successor_height": block_number + 1,
        "successor_hash": next_hash,
    }
    if block_number == ARC_MAINNET_BASELINE_BLOCK:
        report.add(
            "preflight",
            "audited_mainnet_baseline_hash",
            anchor_hash == ARC_MAINNET_BASELINE_HASH,
            ARC_MAINNET_BASELINE_HASH,
            anchor_hash,
        )
        if anchor_hash != ARC_MAINNET_BASELINE_HASH:
            raise RuntimeError("audited Arc mainnet baseline hash mismatch")
        parent_base_fee = quantity(block.get("baseFeePerGas", "0x0"))
        parent_gas_used = quantity(block.get("gasUsed", "0x0"))
        next_base_fee = quantity(next_block.get("baseFeePerGas", "0x0"))
        extra_data_base_fee = arc_next_base_fee_from_extra_data(
            block.get("extraData")
        )
        ethereum_empty_block_fee = parent_base_fee - parent_base_fee // 8
        report.add(
            "preflight",
            "arc_next_base_fee_extra_data_oracle",
            parent_gas_used == 0
            and extra_data_base_fee == next_base_fee
            and next_base_fee != ethereum_empty_block_fee,
            {
                "parent_gas_used": 0,
                "next_base_fee": next_base_fee,
                "not_ethereum_empty_block_fee": ethereum_empty_block_fee,
            },
            {
                "parent_gas_used": parent_gas_used,
                "extra_data_next_base_fee": extra_data_base_fee,
                "next_header_base_fee": next_base_fee,
                "ethereum_empty_block_fee": ethereum_empty_block_fee,
            },
        )
    overrides = simulation_block_overrides(block_number, anchor_hash, next_block)
    funded = select_funded_account(
        reference, block, block_number, funded_address, search_depth
    )
    empty = select_empty_recipient(reference, block_number, funded)
    fixtures = build_fixtures(funded, empty, block_number)
    created = discover_created_address(reference, funded, block_number)
    stateful_fixtures = build_stateful_simulation_fixtures(
        funded,
        empty,
        created,
        block_number,
        address(next_block.get("miner")),
    )
    fixtures.update(stateful_fixtures)
    if block_number == ARC_MAINNET_BASELINE_BLOCK:
        report.add(
            "preflight",
            "stateful_create_address",
            created == ARC_MAINNET_BASELINE_CREATED,
            ARC_MAINNET_BASELINE_CREATED,
            created,
        )
        authority_state = eip7702_authority_state(reference, block_number)
        expected_authority_state = {"code": "0x", "nonce": 0, "balance": 0}
        report.add(
            "preflight",
            "eip7702_authority_is_unmodified_at_anchor",
            authority_state == expected_authority_state
            and "eip7702_delegation_then_call" in stateful_fixtures,
            {
                "state": expected_authority_state,
                "fixture_enabled": True,
            },
            {
                "state": authority_state,
                "fixture_enabled": "eip7702_delegation_then_call"
                in stateful_fixtures,
            },
        )

    for name, calls in fixtures.items():
        simulated = reference.capture(
            "eth_simulateV1",
            [
                {
                    "blockStateCalls": [{"blockOverrides": overrides, "calls": calls}],
                    "validation": False,
                    "traceTransfers": False,
                },
                hex(block_number),
            ],
        )
        if not simulated.get("ok"):
            report.add("preflight", f"{name}.simulate", False, "success", simulated)
            continue
        try:
            blocks = simulated.get("result")
            if not isinstance(blocks, list) or len(blocks) != 1:
                raise ValueError("eth_simulateV1 must return one block")
            reference_calls = blocks[0].get("calls")
            if not isinstance(reference_calls, list) or len(reference_calls) != len(calls):
                raise ValueError("eth_simulateV1 call count mismatch")
            statuses = [quantity(item.get("status")) == 1 for item in reference_calls]
            if name == "failure_then_p256":
                required_statuses = [False, True]
            elif name in stateful_fixtures:
                required_statuses = expected_stateful_statuses(name, len(calls))
            else:
                required_statuses = [True] * len(calls)
            report.add(
                "preflight",
                f"{name}.simulate_status",
                statuses == required_statuses,
                required_statuses,
                statuses,
            )
            outputs = [
                data(item.get("returnData", "0x")) if success else None
                for item, success in zip(reference_calls, statuses)
            ]
            fixed_output = expected_fixture_output(name, block_number)
            if fixed_output is not None:
                report.add(
                    "preflight",
                    f"{name}.fixed_output",
                    outputs == [fixed_output],
                    [fixed_output],
                    outputs,
                )
            stateful_outputs = expected_stateful_outputs(name, block_number, created)
            if name == "fee_then_balance":
                stateful_outputs = expected_fee_state_outputs(
                    reference,
                    block_number,
                    funded,
                    address(next_block.get("miner")),
                )
            if stateful_outputs is not None:
                fixed_matches = len(outputs) == len(stateful_outputs) and all(
                    fixed is None or actual == fixed
                    for fixed, actual in zip(stateful_outputs, outputs)
                )
                report.add(
                    "preflight",
                    f"{name}.fixed_outputs",
                    fixed_matches,
                    stateful_outputs,
                    outputs,
                )
            if name == "eip2935_parent_hash":
                report.add(
                    "preflight",
                    f"{name}.pre_execution",
                    outputs == [anchor_hash],
                    [anchor_hash],
                    outputs,
                )
            if name == "sequential_native_transfer":
                system_logs = [
                    any(
                        log["address"] == SYSTEM_ADDRESS
                        for log in normalize_logs(item.get("logs", []))
                    )
                    for item in reference_calls
                ]
                report.add(
                    "preflight",
                    f"{name}.system_logs",
                    system_logs == [True, False, True],
                    [True, False, True],
                    system_logs,
                )
                report.add(
                    "preflight",
                    f"{name}.state_commit",
                    outputs[1] == abi_words(1),
                    abi_words(1),
                    outputs[1],
                )
            required_system_logs = expected_stateful_system_log_counts(name)
            if required_system_logs is not None:
                system_log_counts = [
                    sum(
                        log["address"] == SYSTEM_ADDRESS
                        for log in normalize_logs(item.get("logs", []))
                    )
                    for item in reference_calls
                ]
                report.add(
                    "preflight",
                    f"{name}.system_logs",
                    system_log_counts == required_system_logs,
                    required_system_logs,
                    system_log_counts,
                )
            if name == "fee_then_balance":
                report.add(
                    "preflight",
                    "fee_then_balance.intrinsic_gas",
                    quantity(reference_calls[0].get("gasUsed"))
                    == FEE_STATE_GAS_USED,
                    FEE_STATE_GAS_USED,
                    reference_calls[0].get("gasUsed"),
                )
            if name == "nested_blocklist_revert":
                traced_blocklist = reference.capture(
                    "trace_call", [calls[0], ["trace"], hex(block_number)]
                )
                blocked_child: dict[str, Any] | None = None
                if traced_blocklist.get("ok") and isinstance(
                    traced_blocklist.get("result"), dict
                ):
                    raw_frames = traced_blocklist["result"].get("trace")
                    if isinstance(raw_frames, list):
                        blocked_child = next(
                            (
                                frame
                                for frame in raw_frames
                                if isinstance(frame, dict)
                                and frame.get("traceAddress") == [0]
                            ),
                            None,
                        )
                expected_revert = "0x08c379a0" + abi_string("Blocked address")[2:]
                child_action = (
                    blocked_child.get("action")
                    if isinstance(blocked_child, dict)
                    else None
                )
                child_result = (
                    blocked_child.get("result")
                    if isinstance(blocked_child, dict)
                    else None
                )
                child_output = (
                    data(child_result.get("output", "0x"))
                    if isinstance(child_result, dict)
                    else None
                )
                report.add(
                    "preflight",
                    "nested_blocklist_revert.failed_child",
                    isinstance(child_action, dict)
                    and address(child_action.get("to")) == USDC
                    and str(blocked_child.get("error", "")).lower() == "reverted"
                    and child_output == expected_revert,
                    {
                        "to": USDC,
                        "error": "Reverted",
                        "output": expected_revert,
                    },
                    blocked_child,
                    "Leafage's DebankTrace schema omits failed child frames; the "
                    "top-level fixed output separately proves CALL success=0.",
                )
        except (KeyError, TypeError, ValueError) as error:
            report.add("preflight", f"{name}.simulate_schema", False, "valid", str(error))
            continue

        if name not in {"failure_then_p256", "eip2935_parent_hash"}:
            traced = reference.capture(
                "pre_traceMany", [calls, hex(block_number + 1), None, None]
            )
            trace_ok = False
            trace_actual: Any = traced
            if traced.get("ok") and isinstance(traced.get("result"), list):
                trace_results = traced["result"]
                trace_ok = len(trace_results) == len(calls)
                if trace_ok:
                    for item, should_succeed in zip(trace_results, required_statuses):
                        if not isinstance(item, dict):
                            trace_ok = False
                            break
                        if should_succeed:
                            item_ok = (
                                item.get("error") is None
                                and quantity(item.get("gasUsed")) > 0
                                and bool(normalize_reference_traces(item.get("trace")))
                            )
                        else:
                            item_ok = (
                                item.get("error") is not None
                                and quantity(item.get("gasUsed")) == 0
                                and item.get("trace") == []
                            )
                        if not item_ok:
                            trace_ok = False
                            break
                trace_actual = {"count": len(trace_results), "success": trace_ok}
            report.add(
                "preflight",
                f"{name}.trace",
                trace_ok,
                {"count": len(calls), "success": True},
                trace_actual,
            )

        if name not in {
            "failure_then_p256",
            "failed_create_log_revert",
            "eip2935_parent_hash",
        }:
            estimate_request = {key: value for key, value in calls[0].items() if key != "gas"}
            estimate = reference.capture(
                "eth_estimateGas", [estimate_request, hex(block_number)]
            )
            estimate_ok = estimate.get("ok") and quantity(estimate.get("result")) > 0
            report.add(
                "preflight",
                f"{name}.estimate",
                estimate_ok,
                "positive gas estimate",
                estimate,
            )
        elif name in {"failure_then_p256", "failed_create_log_revert"}:
            estimate_request = {
                key: value for key, value in calls[0].items() if key != "gas"
            }
            estimate = reference.capture(
                "eth_estimateGas", [estimate_request, hex(block_number)]
            )
            estimate_message = ""
            if not estimate.get("ok") and isinstance(estimate.get("error"), dict):
                estimate_message = str(estimate["error"].get("message", "")).lower()
            report.add(
                "preflight",
                f"{name}.estimate_revert",
                not estimate.get("ok") and "execution reverted" in estimate_message,
                "execution reverted",
                estimate,
            )

    empty_simulation = reference.capture(
        "eth_simulateV1",
        [
            {
                "blockStateCalls": [
                    {"blockOverrides": overrides, "calls": []}
                ],
                "validation": False,
                "traceTransfers": False,
            },
            hex(block_number),
        ],
    )
    empty_trace = reference.capture(
        "pre_traceMany", [[], hex(block_number + 1), None, None]
    )
    empty_simulation_ok = False
    if empty_simulation.get("ok"):
        empty_blocks = empty_simulation.get("result")
        empty_simulation_ok = (
            isinstance(empty_blocks, list)
            and len(empty_blocks) == 1
            and isinstance(empty_blocks[0], dict)
            and empty_blocks[0].get("calls") == []
            and quantity(empty_blocks[0].get("gasUsed")) == 0
        )
    report.add(
        "preflight",
        "simulate_empty_batch",
        empty_simulation_ok
        and empty_trace.get("ok")
        and empty_trace.get("result") == [],
        {"calls": [], "gas_used": 0, "trace": []},
        {"simulate": empty_simulation, "trace": empty_trace},
    )

    run_reference_override_preflight(
        report,
        reference,
        block_number,
        anchor_hash,
        funded,
        empty,
        overrides,
    )
    run_reference_world_state_boundary_preflight(
        report,
        reference,
        block_number,
        search_depth,
    )

    nonce_fixture = build_contract_multicall_boundary_batches(funded)[
        "explicit_nonce"
    ][0]
    nonce_call = reference.capture("eth_call", [nonce_fixture, hex(block_number)])
    identity_output = STANDARD_PRECOMPILE_VECTORS["identity_deadbeef"][2]
    report.add(
        "preflight",
        "contract_multicall.explicit_nonce_is_ignored",
        nonce_call.get("ok") and data(nonce_call.get("result")) == identity_output,
        identity_output,
        nonce_call,
    )
    identity_target, identity_input, identity_output = STANDARD_PRECOMPILE_VECTORS[
        "identity_deadbeef"
    ]
    hash_request = call_request(
        funded, identity_target, identity_input, gas=1_000_000
    )
    eip1898_selector = {"blockHash": anchor_hash, "requireCanonical": True}
    hash_call = reference.capture("eth_call", [hash_request, eip1898_selector])
    hash_estimate_request = {
        key: value for key, value in hash_request.items() if key != "gas"
    }
    hash_estimate = reference.capture(
        "eth_estimateGas", [hash_estimate_request, eip1898_selector]
    )
    hash_simulation = reference.capture(
        "eth_simulateV1",
        [
            {
                "blockStateCalls": [
                    {"blockOverrides": overrides, "calls": [hash_request]}
                ],
                "validation": False,
                "traceTransfers": False,
            },
            anchor_hash,
        ],
    )
    hash_simulation_output: str | None = None
    if hash_simulation.get("ok"):
        hash_blocks = hash_simulation.get("result")
        if (
            isinstance(hash_blocks, list)
            and len(hash_blocks) == 1
            and isinstance(hash_blocks[0], dict)
            and isinstance(hash_blocks[0].get("calls"), list)
            and len(hash_blocks[0]["calls"]) == 1
            and quantity(hash_blocks[0]["calls"][0].get("status")) == 1
        ):
            hash_simulation_output = data(
                hash_blocks[0]["calls"][0].get("returnData")
            )
    report.add(
        "preflight",
        "execution_apis.hash_context",
        hash_call.get("ok")
        and data(hash_call.get("result")) == identity_output
        and hash_estimate.get("ok")
        and quantity(hash_estimate.get("result")) == 0x534E
        and hash_simulation_output == identity_output,
        {"output": identity_output, "estimate_gas": 0x534E},
        {
            "call": hash_call,
            "estimate": hash_estimate,
            "simulation": hash_simulation,
        },
    )

    fee_conflict = call_request(funded, empty)
    fee_conflict["gasPrice"] = "0x1"
    fee_conflict["maxFeePerGas"] = "0x2"
    invalid_authorization = call_request(funded, empty)
    invalid_authorization["authorizationList"] = []
    for name, request, expected_code, expected_message in (
        (
            "estimate_conflicting_fee_fields",
            fee_conflict,
            LEAFAGE_INVALID_PARAMS,
            "both gasPrice and (maxFeePerGas or maxPriorityFeePerGas) specified",
        ),
        (
            "estimate_empty_authorization_list",
            invalid_authorization,
            -32003,
            "EIP-7702 authorization list has invalid fields",
        ),
    ):
        rejected = reference.capture(
            "eth_estimateGas", [request, hex(block_number)]
        )
        error = rejected.get("error") if not rejected.get("ok") else None
        report.add(
            "preflight",
            name,
            isinstance(error, dict)
            and error.get("code") == expected_code
            and error.get("message") == expected_message,
            {"code": expected_code, "message": expected_message},
            rejected,
        )
    if (
        block_number == ARC_MAINNET_BASELINE_BLOCK
        and funded == ARC_MAINNET_BASELINE_FUNDED
    ):
        valid_authorization = reference.capture(
            "eth_estimateGas",
            [eip7702_estimate_request(funded), hex(block_number)],
        )
        report.add(
            "preflight",
            "estimate_valid_eip7702_authorization",
            valid_authorization.get("ok")
            and quantity(valid_authorization.get("result")) == EIP7702_ESTIMATE_GAS,
            EIP7702_ESTIMATE_GAS,
            valid_authorization,
        )

    asset_outputs = expected_asset_outputs(block_number)
    for name, request in build_asset_read_fixtures(funded):
        result = reference.capture("eth_call", [request, hex(block_number)])
        fixed = asset_outputs.get(name)
        passed = result.get("ok") and (
            fixed is None or data(result.get("result")) == fixed
        )
        report.add(
            "preflight",
            f"asset.{name}",
            passed,
            fixed or "successful eth_call",
            result,
        )

    sentinel_calls = build_native_sentinel_calls(funded)
    sentinel = reference.capture(
        "eth_multiCall",
        [sentinel_calls, hex(block_number), False, False, True, None, None],
    )
    sentinel_ok = False
    if sentinel.get("ok") and isinstance(sentinel.get("result"), dict):
        results = sentinel["result"].get("results")
        sentinel_ok = isinstance(results, list) and len(results) == len(sentinel_calls) and all(
            isinstance(item, dict) and item.get("code") == 0 for item in results
        )
    report.add(
        "preflight",
        "native_sentinel.writer_multicall",
        sentinel_ok,
        {"count": len(sentinel_calls), "success": True},
        sentinel,
        "This validates the writer pseudo-token oracle; it is not an Arc supply invariant.",
    )


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--block", required=True, type=parse_block)
    result.add_argument("--funded-address", type=parse_address)
    result.add_argument(
        "--reference-only",
        action="store_true",
        help="validate the writer oracle/corpus without claiming Leafage equivalence",
    )
    result.add_argument("--search-depth", type=int, default=128)
    result.add_argument(
        "--gas-tolerance-bps",
        type=int,
        default=0,
        help="allowed estimateGas difference in basis points (default: exact equality)",
    )
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
    label = (
        "Arc writer preflight"
        if payload.get("mode") == "reference-preflight"
        else "Arc Leafage differential checks"
    )
    print(
        f"{label}: {summary['passed']}/{summary['checks']} passed, "
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

    required_endpoints = (
        (("ARC_REFERENCE_RPC", reference_rpc),)
        if args.reference_only
        else (
            ("LEAFAGE_RPC", leafage_rpc),
            ("ARC_REFERENCE_RPC", reference_rpc),
        )
    )
    missing_endpoints = [name for name, value in required_endpoints if not value]
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
            [name for name, _value in required_endpoints],
            missing_endpoints,
        )
        payload = report.finish(leafage, reference)
        write_report(payload, args.output)
        print_summary(payload)
        return 2

    if args.reference_only:
        try:
            run_reference_preflight(
                report,
                reference,
                args.block,
                args.funded_address,
                args.search_depth,
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
        if args.block == ARC_MAINNET_BASELINE_BLOCK:
            report.add(
                "anchor",
                "audited_mainnet_baseline_hash",
                reference_hash == ARC_MAINNET_BASELINE_HASH,
                ARC_MAINNET_BASELINE_HASH,
                reference_hash,
            )
            if reference_hash != ARC_MAINNET_BASELINE_HASH:
                raise RuntimeError("audited Arc mainnet baseline hash mismatch")
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
            report, leafage, reference, args.block, reference_hash, funded
        )
        run_world_state_checks(
            report, leafage, reference, args.block, reference_hash, empty
        )
        run_world_state_boundary_checks(
            report,
            leafage,
            reference,
            args.block,
            boundary_height,
            boundary_account,
        )

        fixtures = build_fixtures(funded, empty, args.block)
        created = discover_created_address(reference, funded, args.block)
        stateful_fixtures = build_stateful_simulation_fixtures(
            funded,
            empty,
            created,
            args.block,
            address(simulation_block.get("miner")),
        )
        if args.block == ARC_MAINNET_BASELINE_BLOCK:
            report.add(
                "fixture",
                "stateful_create_address",
                created == ARC_MAINNET_BASELINE_CREATED,
                ARC_MAINNET_BASELINE_CREATED,
                created,
            )
            expected_authority_state = {"code": "0x", "nonce": 0, "balance": 0}
            reference_authority = eip7702_authority_state(reference, args.block)
            leafage_authority = eip7702_authority_state(leafage, args.block)
            report.add(
                "fixture",
                "eip7702_authority_is_unmodified_at_anchor",
                reference_authority
                == leafage_authority
                == expected_authority_state
                and "eip7702_delegation_then_call" in stateful_fixtures,
                {
                    "state": expected_authority_state,
                    "fixture_enabled": True,
                },
                {
                    "reference": reference_authority,
                    "leafage": leafage_authority,
                    "fixture_enabled": "eip7702_delegation_then_call"
                    in stateful_fixtures,
                },
            )
        distinctive_overrides = environment_probe_overrides(block_overrides)
        query_h_plus_2_overrides = dict(distinctive_overrides)
        query_h_plus_2_overrides["number"] = hex(args.block + 2)
        contract_multicall_items = [
            (name, calls[0])
            for name, calls in fixtures.items()
            if name not in {"sequential_native_transfer", "failure_then_p256"}
        ]
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "arc_read_batch",
            [request for _name, request in contract_multicall_items],
            [
                expected_fixture_output(name, args.block)
                for name, _request in contract_multicall_items
            ],
        )
        asset_items = build_asset_read_fixtures(funded)
        asset_outputs = expected_asset_outputs(args.block)
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "important_erc20_assets",
            [request for _name, request in asset_items],
            [asset_outputs.get(name) for name, _request in asset_items],
        )
        multicall_boundaries = build_contract_multicall_boundary_batches(funded)
        identity_target, identity_input, identity_output = STANDARD_PRECOMPILE_VECTORS[
            "identity_deadbeef"
        ]
        identity_request = call_request(
            funded, identity_target, identity_input, gas=1_000_000
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "empty_batch",
            multicall_boundaries["empty"],
            [],
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "batch_above_32",
            multicall_boundaries["above_32"],
            [identity_output] * len(multicall_boundaries["above_32"]),
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "explicit_nonce_ignored",
            multicall_boundaries["explicit_nonce"],
            [identity_output],
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "parallel_hint_identity",
            [identity_request],
            [identity_output],
            use_parallel=True,
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "hash_context_identity",
            [identity_request],
            [identity_output],
            use_hash_context=True,
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "revert_then_success",
            fixtures["failure_then_p256"],
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "fast_fail_revert",
            fixtures["failure_then_p256"],
            fast_fail=True,
        )
        override_calls, state_override = build_contract_multicall_override_fixture(
            funded, empty, args.block
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "state_and_block_overrides",
            override_calls,
            [
                environment_probe_output(args.block, distinctive_overrides),
                environment_probe_output(
                    args.block,
                    distinctive_overrides,
                    call_like=True,
                ),
                block_hash(reference_block.get("parentHash")),
                abi_words(7),
                abi_words(7),
                STATE_OVERRIDE_COUNTER_CODE_HASH,
            ],
            state_override=state_override,
            block_overrides=distinctive_overrides,
        )
        h_plus_2_probe = environment_probe_request(funded, args.block)
        h_plus_2_probe["gasPrice"] = hex(
            quantity(query_h_plus_2_overrides["baseFee"]) + 1
        )
        run_contract_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            "h_plus_2_block_overrides",
            [h_plus_2_probe],
            [environment_probe_output(args.block, query_h_plus_2_overrides)],
            block_overrides=query_h_plus_2_overrides,
        )
        run_native_sentinel_multicall(
            report,
            leafage,
            reference,
            args.block,
            reference_hash,
            anchor_timestamp,
            funded,
        )
        simulation_fixtures = {**fixtures, **stateful_fixtures}
        for name, calls in simulation_fixtures.items():
            fixed_outputs = expected_stateful_outputs(name, args.block, created)
            if name == "fee_then_balance":
                fixed_outputs = expected_fee_state_outputs(
                    reference,
                    args.block,
                    funded,
                    address(simulation_block.get("miner")),
                )
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
                compare_traces=name
                not in {
                    "failure_then_p256",
                    "eip2935_parent_hash",
                },
                fixed_outputs=fixed_outputs,
            )
        run_simulation(
            report,
            leafage,
            reference,
            args.block,
            "empty_batch",
            [],
            block_overrides,
            anchor_timestamp,
            reference_hash,
            compare_traces=True,
        )
        run_simulation(
            report,
            leafage,
            reference,
            args.block,
            "hash_context_identity",
            [identity_request],
            block_overrides,
            anchor_timestamp,
            reference_hash,
            fixed_outputs=[identity_output],
            use_hash_context=True,
        )

        run_simulation_environment_overrides(
            report,
            leafage,
            reference,
            args.block,
            funded,
            block_overrides,
            anchor_timestamp,
            reference_hash,
        )
        run_simulation_default_environment(
            report,
            leafage,
            reference,
            args.block,
            funded,
            reference_block,
        )
        run_simulation_derived_next_base_fee(
            report,
            leafage,
            args.block,
            funded,
            reference_block,
            simulation_block,
            block_overrides,
        )

        run_simulation_wrong_height_rejection(
            report,
            leafage,
            args.block,
            fixtures["eip2935_parent_hash"],
            block_overrides,
        )
        run_blocked_execution_rejections(
            report,
            leafage,
            reference,
            args.block,
            funded,
            empty,
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

        if (
            args.block == ARC_MAINNET_BASELINE_BLOCK
            and funded == ARC_MAINNET_BASELINE_FUNDED
        ):
            run_estimate(
                report,
                leafage,
                reference,
                args.block,
                "valid_eip7702_authorization",
                eip7702_estimate_request(funded),
                0,
                exact_reference_gas=EIP7702_ESTIMATE_GAS,
            )
        run_estimate(
            report,
            leafage,
            reference,
            args.block,
            "hash_context_identity",
            identity_request,
            0,
            exact_reference_gas=0x534E,
            anchor_hash=reference_hash,
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
        run_estimate_rejection(
            report,
            leafage,
            reference,
            args.block,
            "contract_revert",
            fixtures["failure_then_p256"][0],
            "revert",
        )
        fee_conflict = call_request(funded, empty)
        fee_conflict["gasPrice"] = "0x1"
        fee_conflict["maxFeePerGas"] = "0x2"
        run_estimate_rejection(
            report,
            leafage,
            reference,
            args.block,
            "conflicting_fee_fields",
            fee_conflict,
            "fee_conflict",
        )
        invalid_authorization = call_request(funded, empty)
        invalid_authorization["authorizationList"] = []
        run_estimate_rejection(
            report,
            leafage,
            reference,
            args.block,
            "empty_authorization_list",
            invalid_authorization,
            "authorization",
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
        run_estimate_with_block_overrides(
            report,
            leafage,
            reference,
            args.block,
            funded,
            distinctive_overrides,
            canonical_previous_hash=block_hash(reference_block.get("parentHash")),
        )
        run_estimate_with_block_overrides(
            report,
            leafage,
            reference,
            args.block,
            funded,
            query_h_plus_2_overrides,
            name="h_plus_2_block_overrides",
            canonical_previous_hash=block_hash(reference_block.get("parentHash")),
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
