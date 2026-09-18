#!/usr/bin/env python3
"""Verify the frozen Tempo T5-T10 mainnet fixture manifest."""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any

DEFAULT_RPC_URL = "https://rpc.tempo.xyz"
DEFAULT_MANIFEST = Path(__file__).parent / "fixtures" / "tempo-t5-t10-mainnet.json"
RpcCall = Callable[[str, list[Any]], Any]


class VerificationError(RuntimeError):
    """A frozen fixture no longer matches the RPC response."""


def normalize(value: Any) -> Any:
    return value.lower() if isinstance(value, str) and value.startswith("0x") else value


def require_equal(label: str, actual: Any, expected: Any) -> None:
    if normalize(actual) != normalize(expected):
        raise VerificationError(f"{label}: expected {expected!r}, got {actual!r}")


def nested_field(value: dict[str, Any], path: str) -> Any:
    current: Any = value
    for component in path.split("."):
        if not isinstance(current, dict) or component not in current:
            raise VerificationError(f"transaction field {path!r} is missing")
        current = current[component]
    return current


def execution_input(tx: dict[str, Any], execution: dict[str, Any]) -> tuple[Any, Any]:
    kind = execution["kind"]
    if kind == "transaction":
        return tx.get("to"), tx.get("input")
    if kind == "aa_call":
        calls = tx.get("calls")
        index = execution["index"]
        if not isinstance(calls, list) or index >= len(calls):
            raise VerificationError(f"AA call {index} is missing")
        return calls[index].get("to"), calls[index].get("input")
    raise VerificationError(f"unsupported execution kind {kind!r}")


def verify_fixture(fixture: dict[str, Any], rpc: RpcCall) -> None:
    fixture_id = fixture["id"]
    block_number = fixture["block_number"]
    tx = rpc("eth_getTransactionByHash", [fixture["transaction_hash"]])
    receipt = rpc("eth_getTransactionReceipt", [fixture["transaction_hash"]])
    if tx is None or receipt is None:
        raise VerificationError(f"{fixture_id}: transaction or receipt is missing")

    expected_tx = fixture["transaction"]
    require_equal(f"{fixture_id} tx hash", tx.get("hash"), fixture["transaction_hash"])
    require_equal(f"{fixture_id} block", tx.get("blockNumber"), hex(block_number))
    require_equal(f"{fixture_id} tx type", tx.get("type"), expected_tx["type"])
    require_equal(
        f"{fixture_id} tx index", tx.get("transactionIndex"), expected_tx["index"]
    )

    execution = expected_tx["execution"]
    target, calldata = execution_input(tx, execution)
    require_equal(f"{fixture_id} target", target, execution["to"])
    if not isinstance(calldata, str):
        raise VerificationError(f"{fixture_id}: execution calldata is missing")
    require_equal(f"{fixture_id} selector", calldata[:10], execution["selector"])

    for path, expected in expected_tx.get("fields", {}).items():
        require_equal(f"{fixture_id} field {path}", nested_field(tx, path), expected)

    expected_receipt = fixture["receipt"]
    require_equal(
        f"{fixture_id} receipt block", receipt.get("blockNumber"), hex(block_number)
    )
    require_equal(
        f"{fixture_id} status", receipt.get("status"), expected_receipt["status"]
    )
    require_equal(
        f"{fixture_id} gas used", receipt.get("gasUsed"), expected_receipt["gas_used"]
    )

    expected_event = expected_receipt["event"]
    logs = receipt.get("logs")
    if not isinstance(logs, list):
        raise VerificationError(f"{fixture_id}: receipt logs are missing")
    event = next(
        (
            log
            for log in logs
            if normalize(log.get("logIndex")) == normalize(expected_event["log_index"])
        ),
        None,
    )
    if event is None:
        raise VerificationError(
            f"{fixture_id}: log index {expected_event['log_index']} is missing"
        )
    require_equal(
        f"{fixture_id} event address", event.get("address"), expected_event["address"]
    )
    require_equal(
        f"{fixture_id} event topics", event.get("topics"), expected_event["topics"]
    )

    for index, assertion in enumerate(fixture.get("state_assertions", [])):
        call = {"to": assertion["to"], "data": assertion["data"]}
        before = rpc("eth_call", [call, hex(block_number - 1)])
        after = rpc("eth_call", [call, hex(block_number)])
        require_equal(f"{fixture_id} state {index} before", before, assertion["before"])
        require_equal(f"{fixture_id} state {index} after", after, assertion["after"])


def make_rpc(url: str, timeout: float) -> RpcCall:
    request_id = 0

    def call(method: str, params: list[Any]) -> Any:
        nonlocal request_id
        request_id += 1
        payload = json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        ).encode()
        request = urllib.request.Request(
            url,
            data=payload,
            headers={
                "content-type": "application/json",
                "user-agent": "leafage-tempo-fixture-verifier/1",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                body = json.load(response)
        except (OSError, urllib.error.URLError, json.JSONDecodeError) as error:
            raise VerificationError(f"{method}: RPC request failed: {error}") from error
        if "error" in body:
            raise VerificationError(f"{method}: RPC returned {body['error']}")
        if "result" not in body:
            raise VerificationError(f"{method}: RPC response has no result")
        return body["result"]

    return call


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rpc-url",
        default=os.environ.get("TEMPO_RPC_URL", DEFAULT_RPC_URL),
        help="Tempo archive RPC URL (default: TEMPO_RPC_URL or official public RPC)",
    )
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--timeout", type=float, default=20.0)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    manifest = json.loads(args.manifest.read_text())
    rpc = make_rpc(args.rpc_url, args.timeout)
    require_equal("chain id", rpc("eth_chainId", []), manifest["source"]["chain_id"])

    fixtures = manifest["fixtures"]
    for fixture in fixtures:
        verify_fixture(fixture, rpc)
        print(f"PASS {fixture['id']} block={fixture['block_number']}")
    print(f"PASS all {len(fixtures)} Tempo mainnet fixtures")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
