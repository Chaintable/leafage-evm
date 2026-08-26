#!/usr/bin/env python3
"""Broad Arc mainnet Leafage/writer differential suite.

The existing deterministic A8 suite is intentionally deep.  This companion
suite expands horizontal coverage without counting repeated response fields as
independent cases.  A case is uniquely identified by target, semantic
scenario, historical block context, transaction actor, and RPC endpoint.

Only the two locally configured Arc mainnet endpoints are used.  Public RPCs
and testnet are deliberately outside this suite.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import sys
import time
import urllib.error
import urllib.request
from collections import Counter
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import verify_arc_queries as core

ANCHOR = core.ARC_MAINNET_BASELINE_BLOCK
ANCHOR_HASH = core.ARC_MAINNET_BASELINE_HASH
FUNDED = core.ARC_MAINNET_BASELINE_FUNDED
EMPTY = "0x000000000000000000000000000000000000beef"
BASELINE_FUNCTIONAL_CASES = 229
TEN_X_TARGET = BASELINE_FUNCTIONAL_CASES * 10
DEFAULT_GAS = 1_000_000
IMPLEMENTATION_SLOT = (
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
)
USDC_IMPLEMENTATION_SLOT = (
    "0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3"
)


ADDRESSES = {
    "usdc": core.USDC,
    "protocol_config": core.PROTOCOL_CONFIG,
    "validator_registry": core.VALIDATOR_REGISTRY,
    "validator_manager": "0x3600000000000000000000000000000000000003",
    "denylist": core.DENYLIST,
    "native_coin_authority": core.NATIVE_COIN_AUTHORITY,
    "native_coin_control": core.NATIVE_COIN_CONTROL,
    "system_accounting": core.SYSTEM_ACCOUNTING,
    "call_from": "0x1800000000000000000000000000000000000003",
    "pq": core.PQ_PRECOMPILE,
    "p256": core.P256_PRECOMPILE,
    "system_event": core.SYSTEM_ADDRESS,
    "eip2935_history": core.HISTORY_STORAGE,
    "deterministic_deployer": "0x4e59b44847b379578588920ca78fbf26c0b4956c",
    "multicall3": "0xca11bde05977b3631167028862be2a173976ca11",
    "multicall3_from": "0x522faf9a91c41c443c66765030741e4aace147d0",
    "memo": "0x5294e9927c3306dcbadb03fe70b92e01ccede505",
    "permit2": core.PERMIT2,
    "cctp_token_messenger": "0x28b5a0e9c621a5badaa536219b3a228c8168cf5d",
    "cctp_token_messenger_fees": "0x71f54f818671cd0d7ea140da213e5c8b5c92a408",
    "cctp_message_transmitter": "0x81d40f21f12a8f0e3252bccb954d722d4c464b64",
    "cctp_token_minter": "0xfd78ee919681417d192449715b2594ab58f5d002",
    "cctp_message_v2": "0xec546b6b005471ecf012e5af77fbec07e0fd8f78",
    "gateway_wallet": "0x77777777dcc4d5a8b6e418fd04d8997ef11000ee",
    "gateway_minter": "0x2222222d7164433c4c09b0b0d809a9b52c04c205",
    "arcane_launch_factory": "0x10fe9116add23758e94a633b57d0679d2997e92a",
    "arcane_launch_locker": "0x53129640fed1d72a9b980218a3217e0c635911a8",
    "arcane_curve_migrator": "0x50a802e2234b5bbd60389f40f477daab25a94590",
    "arcane_limit_orders": "0x2dca1c5acdcf362c6b61d91ec4661a410fe4e178",
    "arcane_v3_factory": "0x874dc9d64cd0af61146a68036e9afca7dadd736a",
    "arcane_v3_router": "0x86dfced95ad9231f3cbe0c73d4cb9d555357301c",
    "arcane_v3_quoter": "0x2bffd1f1b1ef9815cbfe8377221fccffd24d2c5e",
    "arcane_position_manager": "0x39ba5f639e3916f1c827cae95b72c0c5e9db553e",
    "aworp": "0x26d1ffbbb8b310b090ee0536748b4adfc88ae644",
    "ausd": "0xf5b08979251f398180385b54381ee3d6fa1bbe09",
    "agbp": "0xa073783b43dfbfa2a78e0ae015a82968d816f41a",
}


DEPLOYMENTS = {
    "usdc": 0,
    "protocol_config": 0,
    "validator_registry": 0,
    "validator_manager": 0,
    "denylist": 0,
    "native_coin_authority": 0,
    "native_coin_control": 0,
    "system_accounting": 0,
    "call_from": 0,
    "pq": 0,
    "p256": 0,
    "system_event": 0,
    "eip2935_history": 0,
    "deterministic_deployer": 0,
    "multicall3": 0,
    "multicall3_from": 3_087_090,
    "memo": 3_087_061,
    "permit2": 0,
    "cctp_token_messenger": 718_552,
    "cctp_token_messenger_fees": 4_344_965,
    "cctp_message_transmitter": 718_542,
    "cctp_token_minter": 718_533,
    "cctp_message_v2": 718_569,
    "gateway_wallet": 1_939_146,
    "gateway_minter": 1_938_946,
    "arcane_launch_factory": 11_494_827,
    "arcane_launch_locker": 11_494_816,
    "arcane_curve_migrator": 11_494_833,
    "arcane_limit_orders": 11_494_847,
    "arcane_v3_factory": 11_494_774,
    "arcane_v3_router": 11_494_779,
    "arcane_v3_quoter": 11_494_785,
    "arcane_position_manager": 11_494_797,
    "aworp": 4_461_378,
    "ausd": 4_453_705,
    "agbp": 4_454_029,
}


PROXY_TARGETS = (
    "usdc",
    "protocol_config",
    "validator_registry",
    "validator_manager",
    "denylist",
    "cctp_token_messenger",
    "cctp_token_messenger_fees",
    "cctp_message_transmitter",
    "gateway_wallet",
    "gateway_minter",
    "arcane_launch_factory",
    "arcane_launch_locker",
    "aworp",
    "ausd",
    "agbp",
)

PROXY_IMPLEMENTATION_SLOTS = {"usdc": USDC_IMPLEMENTATION_SLOT}

EXPECTED_UNDEPLOYED_ASSETS = {
    "eurc": "0x89b50855aa3be2f677cd6303cec089b5f319d72a",
    "usyc": "0xe9185f0c5f296ed1797aae4238d26ccabeadb86c",
}

DEBANK_RESULT_CODES = {-32_602, -32_601, 0, *range(-39_008, -38_999)}


@dataclass(frozen=True)
class HistoricalReplay:
    name: str
    tx_hash: str
    block: int
    request: dict[str, Any]
    nested_targets: tuple[str, ...]

    @property
    def state_block(self) -> int:
        return self.block - 1


HISTORICAL_REPLAYS = (
    HistoricalReplay(
        "cctp_deposit_for_burn",
        "0x4a96e71f38f1b0e449199b8b6a47df8e3a819d8721f88088de83f271f032db95",
        15_815_513,
        {
            "from": "0x018f2f6054d5109fe36cb6af281fb0b489bd4d3c",
            "to": "0x4414bc134a69423e8f8411f80f44ad9b2178fd09",
            "data": "0x271819380000000000000000000000000000000000000000000000000000000000970fe000000000000000000000000000000000000000000000000000000000000000060000000000000000000000003b5a569d6af0b57e85312a6fbebda633dbb3a1f50000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000007d0",
            "value": "0x0",
            "gas": "0xa0085",
            "maxFeePerGas": "0x65a519eaa",
            "maxPriorityFeePerGas": "0x3b9aca00",
            "nonce": "0x2",
            "type": "0x2",
            "accessList": [],
        },
        (
            ADDRESSES["cctp_token_messenger"],
            ADDRESSES["cctp_token_minter"],
            ADDRESSES["cctp_message_transmitter"],
            ADDRESSES["usdc"],
            ADDRESSES["native_coin_control"],
            ADDRESSES["native_coin_authority"],
        ),
    ),
    HistoricalReplay(
        "arcane_create_pool",
        "0x9f7e543426d5ccf4ba6186e486d64efc5ca6785baa960f65d19c8d0e1ab2fb81",
        11_495_087,
        {
            "from": "0xa6427ca1fb85acf8e8834a866069d691027e721c",
            "to": "0x43a65ed1afb8f46560d39fa2e8b8c7197f152f02",
            "data": "0x3f60b633000000000000000000000000ce1cb094488f444c2dad3abc8ff30e84c1a13d910000000000000000000000000000000000000000000000000000000000009c400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a6427ca1fb85acf8e8834a866069d691027e721c",
            "value": "0x0",
            "gas": "0x82bf7a",
            "maxFeePerGas": "0x4a817c800",
            "maxPriorityFeePerGas": "0x0",
            "nonce": "0x18",
            "type": "0x2",
            "accessList": [],
        },
        (ADDRESSES["arcane_position_manager"], ADDRESSES["arcane_v3_factory"]),
    ),
    HistoricalReplay(
        "arcane_position_migrate",
        "0x5b854a4a45b5bc77a5d6320cc360015d6f267598b7555be573125fa572deb73a",
        11_500_767,
        {
            "from": "0xa6427ca1fb85acf8e8834a866069d691027e721c",
            "to": "0xeadc74031e41d52c9274956c96d599aa30f04fe5",
            "data": "0xdbf94fd5000000000000000000000000ce1cb094488f444c2dad3abc8ff30e84c1a13d910000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006a5fcf06",
            "value": "0x0",
            "gas": "0x7b6bf",
            "maxFeePerGas": "0x4a817c800",
            "maxPriorityFeePerGas": "0x0",
            "nonce": "0x28",
            "type": "0x2",
            "accessList": [],
        },
        (
            ADDRESSES["arcane_position_manager"],
            ADDRESSES["arcane_v3_router"],
            ADDRESSES["usdc"],
        ),
    ),
    HistoricalReplay(
        "arcane_launch",
        "0x93064b490f5bca0b5d6fa5f8e486bf6cc45441d91c4092e8c4a0a3ba5756bcaf",
        11_519_383,
        {
            "from": "0x61092c188d1f6315023ef271268350f1e1379373",
            "to": ADDRESSES["arcane_launch_factory"],
            "data": "0x9d360156000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000014000000000000000000000000061092c188d1f6315023ef271268350f1e1379373000000000000000000000000000000000000000000000000000000000000271000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004746573740000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000045445535400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006068747470733a2f2f617263616e652e66692f6d657461646174612f363032346431653533343936633362373561393732613165663032643962613838353539316334376634653265383163613864336566623137316261303661362e6a736f6e",
            "value": "0x0",
            "gas": "0x196dc3",
            "gasPrice": "0xb2d05e000",
            "nonce": "0x4",
            "type": "0x0",
        },
        (ADDRESSES["arcane_launch_factory"],),
    ),
)


def word(value: int) -> str:
    return value.to_bytes(32, "big").hex()


def address_word(value: str) -> str:
    return word(int(core.address(value), 16))


def calldata(selector: str, *words: str) -> str:
    selector = core.data(selector)
    if len(selector) != 10:
        raise ValueError(f"selector is not four bytes: {selector}")
    for item in words:
        if not re.fullmatch(r"[0-9a-fA-F]{64}", item):
            raise ValueError(f"ABI argument is not one word: {item!r}")
    return selector + "".join(item.lower() for item in words)


def selector(selector_hex: str) -> str:
    return core.data(selector_hex)


def probe(name: str, selector_hex: str, *arguments: str) -> Probe:
    return Probe(
        name,
        calldata(selector_hex, *arguments) if arguments else selector(selector_hex),
    )


@dataclass(frozen=True)
class Probe:
    name: str
    data: str


@dataclass(frozen=True)
class Target:
    name: str
    address: str
    deployment: int
    category: str
    probes: tuple[Probe, ...]


@dataclass(frozen=True)
class Case:
    domain: str
    endpoint: str
    target: str
    scenario: str
    block: int
    actor: str
    request: dict[str, Any] = field(compare=False)
    target_name: str = ""
    group: str | None = None
    position: int | None = None

    @property
    def case_id(self) -> str:
        parts = (
            self.domain,
            self.endpoint,
            self.target_name or self.target,
            self.scenario,
            str(self.block),
            self.actor,
            self.group or "single",
            "" if self.position is None else str(self.position),
        )
        return "|".join(part.lower() for part in parts)

    @property
    def vector_id(self) -> str:
        request = {
            key: value
            for key, value in self.request.items()
            if key not in {"gas", "gasPrice", "maxFeePerGas", "maxPriorityFeePerGas"}
        }
        digest = hashlib.sha256(
            json.dumps(request, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()[:16]
        return "|".join(
            (
                self.domain.lower(),
                (self.target_name or self.target).lower(),
                self.scenario.lower(),
                self.actor.lower(),
                self.group or "single",
                digest,
            )
        )


@dataclass
class Plan:
    cases: list[Case]
    base_vectors: int
    endpoint_counts: dict[str, int]
    domain_counts: dict[str, int]
    target_counts: dict[str, int]

    @classmethod
    def from_cases(cls, cases: Iterable[Case]) -> Plan:
        materialized = list(cases)
        ids = [case.case_id for case in materialized]
        duplicates = [item for item, count in Counter(ids).items() if count > 1]
        if duplicates:
            raise ValueError(f"duplicate semantic case ids: {duplicates[:3]}")
        return cls(
            cases=materialized,
            base_vectors=len({case.vector_id for case in materialized}),
            endpoint_counts=dict(
                sorted(Counter(case.endpoint for case in materialized).items())
            ),
            domain_counts=dict(
                sorted(Counter(case.domain for case in materialized).items())
            ),
            target_counts=dict(
                sorted(
                    Counter(
                        case.target_name or case.target for case in materialized
                    ).items()
                )
            ),
        )


@dataclass
class CaseResult:
    case: Case
    passed: bool
    dimensions: dict[str, bool]
    expected: Any = None
    actual: Any = None
    note: str | None = None

    def as_dict(self) -> dict[str, Any]:
        result = {
            "case_id": self.case.case_id,
            "vector_id": self.case.vector_id,
            "domain": self.case.domain,
            "endpoint": self.case.endpoint,
            "target": self.case.target,
            "target_name": self.case.target_name,
            "scenario": self.case.scenario,
            "block": self.case.block,
            "actor": self.case.actor,
            "passed": self.passed,
            "dimensions": self.dimensions,
            "expected": compact(self.expected),
            "actual": compact(self.actual),
        }
        if self.case.group:
            result["group"] = self.case.group
            result["position"] = self.case.position
        if self.note:
            result["note"] = self.note
        return result


def compact(value: Any, limit: int = 512) -> Any:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":"), default=str)
    if len(encoded) <= limit:
        return value
    return {
        "json_bytes": len(encoded.encode()),
        "sha256": hashlib.sha256(encoded.encode()).hexdigest(),
        "prefix": encoded[:128],
    }


def summarize_results(results: Sequence[CaseResult]) -> dict[str, Any]:
    assertions = sum(len(result.dimensions) for result in results)
    passed = sum(result.passed for result in results)
    return {
        "cases": len(results),
        "passed": passed,
        "failed": len(results) - passed,
        "assertions": assertions,
        "assertions_passed": sum(
            passed_dimension
            for result in results
            for passed_dimension in result.dimensions.values()
        ),
    }


def historical_samples(deployment: int, anchor: int) -> tuple[int, ...]:
    if deployment < 0 or deployment > anchor:
        raise ValueError("deployment height is outside the selected history")
    if deployment == 0:
        candidates = (0, 1, 2, anchor // 2, anchor)
    else:
        candidates = (
            deployment - 1,
            deployment,
            deployment + 1,
            (deployment + anchor) // 2,
            anchor,
        )
    return tuple(dict.fromkeys(candidates))


def required_target_addresses() -> tuple[str, ...]:
    return tuple(dict.fromkeys(value.lower() for value in ADDRESSES.values()))


def standard_owner_probes() -> tuple[Probe, ...]:
    return (
        probe("owner", "0x8da5cb5b"),
        probe("pending_owner", "0xe30c3978"),
    )


def paused_probes() -> tuple[Probe, ...]:
    return (
        probe("paused", "0x5c975abb"),
        probe("pauser", "0x9fd0506d"),
    )


def build_targets(funded: str = FUNDED, empty: str = EMPTY) -> tuple[Target, ...]:
    funded_word = address_word(funded)
    empty_word = address_word(empty)
    usdc_word = address_word(ADDRESSES["usdc"])
    permit_word = address_word(ADDRESSES["permit2"])
    zero_word = word(0)
    one_word = word(1)
    targets: list[Target] = []

    def add(name: str, category: str, probes: Sequence[Probe]) -> None:
        targets.append(
            Target(name, ADDRESSES[name], DEPLOYMENTS[name], category, tuple(probes))
        )

    add(
        "usdc",
        "asset",
        (
            probe("name", "0x06fdde03"),
            probe("symbol", "0x95d89b41"),
            probe("decimals", "0x313ce567"),
            probe("total_supply", "0x18160ddd"),
            probe("domain_separator", "0x3644e515"),
            probe("paused", "0x5c975abb"),
            probe("owner", "0x8da5cb5b"),
            probe("version", "0x54fd4d50"),
            probe("currency", "0xe5a6b10f"),
            probe("blacklister", "0xbd102430"),
            probe("pauser", "0x9fd0506d"),
            probe("rescuer", "0x38a63183"),
            probe("master_minter", "0x35d99f35"),
            probe("decimals_scaling_factor", "0xccd92d3e"),
            probe("native_coin_authority", "0x7fd09916"),
            probe("native_coin_control", "0xdd0743b3"),
            probe("balance_funded", "0x70a08231", funded_word),
            probe("balance_empty", "0x70a08231", empty_word),
            probe("allowance_permit2", "0xdd62ed3e", funded_word, permit_word),
            probe("allowance_empty", "0xdd62ed3e", funded_word, empty_word),
            probe("nonces_funded", "0x7ecebe00", funded_word),
            probe("blacklisted_funded", "0xfe575a87", funded_word),
            probe("blacklisted_usdc", "0xfe575a87", usdc_word),
            probe("authorization_zero", "0xe94a0102", funded_word, zero_word),
            probe("minter_allowance_funded", "0x8a6db9c3", funded_word),
            probe("is_minter_funded", "0xaa271e1a", funded_word),
        ),
    )
    add(
        "protocol_config",
        "important_contract",
        (
            probe("fee_params", "0x9242164f"),
            probe("consensus_params", "0x9fd02a36"),
            probe("controller", "0xf77c4791"),
            *standard_owner_probes(),
            *paused_probes(),
        ),
    )
    add(
        "validator_registry",
        "important_contract",
        (
            probe("active_validator_count", "0xb86444b1"),
            probe("next_validator_id", "0x77db1669"),
            probe("active_validators", "0x24408a68"),
            *standard_owner_probes(),
            probe("validator_0", "0xb5d89627", zero_word),
            probe("validator_1", "0xb5d89627", one_word),
        ),
    )
    add(
        "validator_manager",
        "important_contract",
        (
            probe("registry", "0x06433b1b"),
            probe("default_voting_power", "0x7ffc51fd"),
            *standard_owner_probes(),
            *paused_probes(),
            probe("validator_funded", "0x1904bb2e", funded_word),
            probe("controller_funded", "0xb429afeb", funded_word),
            probe("registerer_funded", "0x048544a4", funded_word),
        ),
    )
    add(
        "denylist",
        "important_contract",
        (
            *standard_owner_probes(),
            probe("denylisted_funded", "0xe877a526", funded_word),
            probe("denylisted_usdc", "0xe877a526", usdc_word),
            probe("denylisted_empty", "0xe877a526", empty_word),
            probe("denylister_funded", "0x31d798b5", funded_word),
            probe("denylister_usdc", "0x31d798b5", usdc_word),
            probe("denylister_empty", "0x31d798b5", empty_word),
        ),
    )
    add(
        "native_coin_authority",
        "arc_feature",
        (probe("total_supply", "0x18160ddd"),),
    )
    add(
        "native_coin_control",
        "arc_feature",
        (
            probe("blocklisted_funded", "0x8e204c43", funded_word),
            probe("blocklisted_empty", "0x8e204c43", empty_word),
            probe("blocklisted_usdc", "0x8e204c43", usdc_word),
        ),
    )
    multicall_probes = (
        probe("block_number", "0x42cbb15c"),
        probe("coinbase", "0xa8b0574e"),
        probe("difficulty", "0x72425d9d"),
        probe("gas_limit", "0x86d516e8"),
        probe("block_timestamp", "0x0f28c97d"),
        probe("last_block_hash", "0x27e86d6e"),
        probe("base_fee", "0x3e64a696"),
        probe("chain_id", "0x3408e470"),
        probe("funded_eth_balance", "0x4d2301cc", funded_word),
        probe("empty_eth_balance", "0x4d2301cc", empty_word),
    )
    add("multicall3", "important_contract", multicall_probes)
    add("multicall3_from", "important_contract", multicall_probes)
    add("memo", "important_contract", (probe("memo_index", "0xf884e355"),))
    add(
        "permit2",
        "important_contract",
        (
            probe("domain_separator", "0x3644e515"),
            probe(
                "allowance_funded_usdc_permit2",
                "0x927da105",
                funded_word,
                usdc_word,
                permit_word,
            ),
            probe("nonce_bitmap_0", "0x4fe02b44", funded_word, zero_word),
            probe("nonce_bitmap_1", "0x4fe02b44", funded_word, one_word),
        ),
    )
    add(
        "cctp_token_messenger",
        "important_contract",
        (
            *standard_owner_probes(),
            probe("rescuer", "0x38a63183"),
            probe("fee_recipient", "0x46904840"),
            probe("min_fee", "0x24ec7590"),
            probe("message_body_version", "0x9cdbb181"),
        ),
    )
    add(
        "cctp_token_messenger_fees",
        "important_contract",
        (*standard_owner_probes(), probe("fee_manager", "0xd0fb0203")),
    )
    add(
        "cctp_message_transmitter",
        "important_contract",
        (
            *standard_owner_probes(),
            probe("rescuer", "0x38a63183"),
            probe("version", "0x54fd4d50"),
            *paused_probes(),
            probe("local_domain", "0x8d3638f4"),
            probe("max_message_body_size", "0xaf47b9bb"),
            probe("signature_threshold", "0xa82f2e26"),
        ),
    )
    add(
        "cctp_token_minter",
        "important_contract",
        (
            *standard_owner_probes(),
            probe("rescuer", "0x38a63183"),
            *paused_probes(),
            probe("token_controller", "0xeddd9d82"),
        ),
    )
    add(
        "cctp_message_v2",
        "important_contract",
        (
            probe("address_to_bytes32_funded", "0x82c947b7", funded_word),
            probe("address_to_bytes32_empty", "0x82c947b7", empty_word),
            probe("bytes32_to_address_funded", "0x5ced058e", funded_word),
            probe("bytes32_to_address_empty", "0x5ced058e", empty_word),
        ),
    )
    add(
        "gateway_wallet",
        "important_contract",
        (
            *standard_owner_probes(),
            *paused_probes(),
            probe("fee_recipient", "0x46904840"),
            probe("withdrawal_delay", "0xa7ab6961"),
            probe("domain", "0xc2fb26a6"),
            probe("domain_separator", "0xf698da25"),
            probe("usdc_supported", "0x75151b63", usdc_word),
        ),
    )
    add(
        "gateway_minter",
        "important_contract",
        (
            *standard_owner_probes(),
            *paused_probes(),
            probe("domain", "0xc2fb26a6"),
            probe("usdc_supported", "0x75151b63", usdc_word),
        ),
    )
    add(
        "arcane_launch_factory",
        "important_contract",
        (
            *standard_owner_probes(),
            probe("paused", "0x5c975abb"),
            probe("bps", "0x249d39e9"),
            probe("usdc", "0x3e413bee"),
            probe("migrator", "0x7cd07e47"),
            probe("platform_fee_recipient", "0xeb13554f"),
            probe("token_supply", "0xb152f6cf"),
        ),
    )
    add(
        "arcane_launch_locker",
        "important_contract",
        (
            *standard_owner_probes(),
            probe("paused", "0x5c975abb"),
            probe("bps", "0x249d39e9"),
            probe("usdc", "0x3e413bee"),
            probe("swap_router", "0xc31c9c07"),
            probe("factory", "0xc45a0155"),
            probe("pool_fee", "0xdd1b9c4a"),
            probe("platform_fee_recipient", "0xeb13554f"),
        ),
    )
    add(
        "arcane_curve_migrator",
        "important_contract",
        (
            probe("bps", "0x249d39e9"),
            probe("locker", "0xd7b96d4e"),
            probe("pool_fee", "0xdd1b9c4a"),
        ),
    )
    add(
        "arcane_limit_orders",
        "important_contract",
        (
            *standard_owner_probes(),
            probe("paused", "0x5c975abb"),
            probe("next_order_id", "0x2a58b330"),
            probe("usdc", "0x3e413bee"),
            probe("factory", "0xc45a0155"),
            probe("order_0", "0xa85c38ef", zero_word),
            probe("order_1", "0xa85c38ef", one_word),
        ),
    )
    add(
        "arcane_v3_factory",
        "important_contract",
        (
            probe("owner", "0x8da5cb5b"),
            probe("tick_spacing_100", "0x22afcccb", word(100)),
            probe("tick_spacing_500", "0x22afcccb", word(500)),
            probe("tick_spacing_3000", "0x22afcccb", word(3000)),
            probe("tick_spacing_10000", "0x22afcccb", word(10_000)),
            probe(
                "usdc_nca_pool_500",
                "0x1698ee82",
                usdc_word,
                address_word(core.NATIVE_COIN_AUTHORITY),
                word(500),
            ),
        ),
    )
    router_probes = (
        probe("factory", "0xc45a0155"),
        probe("weth9", "0x4aa4a4fc"),
    )
    add("arcane_v3_router", "important_contract", router_probes)
    add("arcane_v3_quoter", "important_contract", router_probes)
    add(
        "arcane_position_manager",
        "important_contract",
        (
            probe("name", "0x06fdde03"),
            probe("symbol", "0x95d89b41"),
            probe("total_supply", "0x18160ddd"),
            probe("domain_separator", "0x3644e515"),
            probe("permit_typehash", "0x30adf81f"),
            probe("factory", "0xc45a0155"),
            probe("weth9", "0x4aa4a4fc"),
            probe("base_uri", "0x6c0360eb"),
            probe("funded_balance", "0x70a08231", funded_word),
        ),
    )
    for label, token, holder, *_unused in core.IMPORTANT_ERC20_ASSETS[1:]:
        holder_word = address_word(holder)
        add(
            label,
            "asset",
            (
                probe("name", "0x06fdde03"),
                probe("symbol", "0x95d89b41"),
                probe("decimals", "0x313ce567"),
                probe("total_supply", "0x18160ddd"),
                probe("holder_balance", "0x70a08231", holder_word),
                probe("empty_balance", "0x70a08231", empty_word),
                probe(
                    "holder_permit2_allowance", "0xdd62ed3e", holder_word, permit_word
                ),
            ),
        )
    return tuple(targets)


def make_request(
    actor: str, target: str, input_data: str, gas: int = DEFAULT_GAS
) -> dict[str, Any]:
    return core.call_request(actor, target, input_data, gas=gas)


def add_endpoint_cases(
    cases: list[Case],
    *,
    domain: str,
    target_name: str,
    target: str,
    scenario: str,
    actor: str,
    request: dict[str, Any],
    block: int,
    endpoints: Sequence[str],
    group: str | None = None,
    position: int | None = None,
) -> None:
    for endpoint in endpoints:
        cases.append(
            Case(
                domain=domain,
                endpoint=endpoint,
                target=target.lower(),
                target_name=target_name,
                scenario=scenario,
                block=block,
                actor=actor.lower(),
                request=request,
                group=group,
                position=position,
            )
        )


def p256_variants() -> tuple[tuple[str, str], ...]:
    valid = bytes.fromhex(core.P256_VALID_INPUT[2:])
    variants: list[tuple[str, str]] = [("valid", core.P256_VALID_INPUT)]
    variants.append(("invalid_wrong_hash", core.P256_INVALID_WRONG_HASH_INPUT))
    for length in (0, 1, 4, 31, 32, 33, 63, 64, 65, 95, 96, 97, 127, 128, 129, 159):
        variants.append((f"truncated_{length}", "0x" + valid[:length].hex()))
    variants.append(("trailing_zero", "0x" + (valid + b"\x00").hex()))
    for index in (0, 31, 32, 63, 64, 95, 96, 127, 128, 159):
        mutated = bytearray(valid)
        mutated[index] ^= 1
        variants.append((f"flip_{index}", "0x" + mutated.hex()))
    for word_index in range(5):
        mutated = bytearray(valid)
        mutated[word_index * 32 : (word_index + 1) * 32] = bytes(32)
        variants.append((f"zero_word_{word_index}", "0x" + mutated.hex()))
    return tuple(variants)


def pq_variants() -> tuple[tuple[str, str], ...]:
    valid = bytes.fromhex(core.load_pq_valid_input()[2:])
    variants: list[tuple[str, str]] = [
        ("valid", "0x" + valid.hex()),
        ("invalid_signature", core.load_pq_invalid_signature_input()),
    ]
    for length in (
        0,
        1,
        3,
        4,
        31,
        32,
        67,
        68,
        95,
        96,
        99,
        100,
        127,
        128,
        255,
        1024,
        len(valid) - 1,
    ):
        variants.append((f"truncated_{length}", "0x" + valid[:length].hex()))
    variants.append(("trailing_zero", "0x" + (valid + b"\x00").hex()))
    for index in (0, 3, 4, 35, 67, 99, 100, len(valid) // 2, len(valid) - 1):
        mutated = bytearray(valid)
        mutated[index] ^= 1
        variants.append((f"flip_{index}", "0x" + mutated.hex()))
    return tuple(variants)


def deterministic_deployer_variants() -> tuple[tuple[str, str], ...]:
    constant_42_init = "600a80600b6000396000f3602a60005260206000f3"
    constant_43_init = "600a80600b6000396000f3602b60005260206000f3"
    return (
        ("salt_zero_constant_42", "0x" + "00" * 32 + constant_42_init),
        ("salt_one_constant_42", "0x" + "00" * 31 + "01" + constant_42_init),
        ("salt_42_constant_42", "0x" + "42" * 32 + constant_42_init),
        ("salt_ff_constant_42", "0x" + "ff" * 32 + constant_42_init),
        ("salt_42_constant_43", "0x" + "42" * 32 + constant_43_init),
        ("salt_zero_empty_runtime", "0x" + "00" * 32 + "60006000f3"),
        ("malformed_empty", "0x"),
        ("malformed_short_salt", "0x" + "00" * 31),
    )


def asset_mutation_vectors(empty: str) -> tuple[tuple[str, str, int | None], ...]:
    recipient = address_word(empty)
    zero_address = word(0)
    return (
        ("transfer_zero", calldata("0xa9059cbb", recipient, word(0)), None),
        ("transfer_one", calldata("0xa9059cbb", recipient, word(1)), None),
        ("transfer_zero_address", calldata("0xa9059cbb", zero_address, word(1)), None),
        ("approve_zero", calldata("0x095ea7b3", recipient, word(0)), None),
        ("approve_one", calldata("0x095ea7b3", recipient, word(1)), None),
        ("approve_max", calldata("0x095ea7b3", recipient, word((1 << 256) - 1)), None),
        (
            "transfer_from_zero",
            calldata("0x23b872dd", recipient, recipient, word(0)),
            None,
        ),
    )


def access_control_vectors(empty: str) -> tuple[tuple[str, str, str], ...]:
    empty_word = address_word(empty)
    one = word(1)
    zero = word(0)
    vectors = [
        (
            "protocol_config",
            "update_block_gas_limit",
            calldata("0x2bbdb79f", word(30_000_001)),
        ),
        ("protocol_config", "update_controller", calldata("0x06cb5b66", empty_word)),
        ("protocol_config", "pause", "0x8456cb59"),
        ("protocol_config", "unpause", "0x3f4ba83a"),
        (
            "denylist",
            "denylist_empty_unauthorized",
            calldata("0xe8746fed", word(32), one, empty_word),
        ),
        (
            "denylist",
            "undenylist_empty_unauthorized",
            calldata("0x7a7ceb16", word(32), one, empty_word),
        ),
        ("denylist", "add_denylister", calldata("0xfab48ccf", empty_word)),
        ("denylist", "remove_denylister", calldata("0xca2f7313", empty_word)),
        ("validator_registry", "activate_validator_1", calldata("0x4ebae617", one)),
        ("validator_registry", "remove_validator_1", calldata("0xf94e1867", one)),
        (
            "validator_registry",
            "update_voting_power_1",
            calldata("0xc4899f0c", one, one),
        ),
        ("validator_manager", "activate_validator", "0x263a3402"),
        ("validator_manager", "remove_validator", "0x7f77403d"),
        ("validator_manager", "update_voting_power", calldata("0x4cb4850a", one)),
        (
            "validator_manager",
            "configure_controller_unauthorized",
            calldata("0xd2c20cb3", empty_word, one, one),
        ),
        ("native_coin_control", "blocklist_empty", calldata("0xe5c7160b", empty_word)),
        (
            "native_coin_control",
            "unblocklist_empty",
            calldata("0x31b23020", empty_word),
        ),
        (
            "native_coin_authority",
            "mint_empty",
            calldata("0x40c10f19", empty_word, one),
        ),
        (
            "native_coin_authority",
            "burn_empty",
            calldata("0x9dc29fac", empty_word, one),
        ),
        (
            "native_coin_authority",
            "transfer_empty",
            calldata("0xbeabacc8", empty_word, empty_word, one),
        ),
        ("call_from", "disabled_empty", "0x"),
        (
            "call_from",
            "valid_abi_disabled_selector",
            calldata("0x1595ec0b", empty_word, empty_word, word(96), zero),
        ),
    ]
    ownable = (
        "protocol_config",
        "validator_registry",
        "validator_manager",
        "denylist",
        "cctp_token_messenger_fees",
        "gateway_wallet",
        "gateway_minter",
        "arcane_launch_factory",
        "arcane_launch_locker",
        "arcane_limit_orders",
    )
    for target in ownable:
        vectors.append(
            (target, "transfer_ownership", calldata("0xf2fde38b", empty_word))
        )
        vectors.append((target, "renounce_ownership", "0x715018a6"))
    for target in (
        "cctp_token_messenger",
        "cctp_message_transmitter",
        "cctp_token_minter",
    ):
        vectors.append(
            (target, "transfer_ownership", calldata("0xf2fde38b", empty_word))
        )
    vectors.append(
        ("arcane_v3_factory", "set_owner", calldata("0x13af4035", empty_word))
    )
    pausable = (
        "validator_manager",
        "cctp_message_transmitter",
        "cctp_token_minter",
        "gateway_wallet",
        "gateway_minter",
        "arcane_launch_factory",
        "arcane_launch_locker",
        "arcane_limit_orders",
    )
    for target in pausable:
        vectors.append((target, "pause", "0x8456cb59"))
        vectors.append((target, "unpause", "0x3f4ba83a"))
    return tuple(vectors)


EXECUTION_ENDPOINTS = (
    "eth_call",
    "contractMultiCall",
    "estimateGas",
    "simulateTransactions",
)


def build_plan(anchor: int, funded: str, empty: str) -> Plan:
    funded = core.address(funded)
    empty = core.address(empty)
    cases: list[Case] = []
    targets = build_targets(funded, empty)

    for target in targets:
        samples = historical_samples(target.deployment, anchor)
        for item in target.probes:
            request = make_request(funded, target.address, item.data)
            for block in samples:
                add_endpoint_cases(
                    cases,
                    domain=target.category,
                    target_name=target.name,
                    target=target.address,
                    scenario=f"view.{item.name}",
                    actor=funded,
                    request=request,
                    block=block,
                    endpoints=("eth_call",),
                )
            add_endpoint_cases(
                cases,
                domain=target.category,
                target_name=target.name,
                target=target.address,
                scenario=f"view.{item.name}",
                actor=funded,
                request=request,
                block=anchor,
                endpoints=("contractMultiCall", "estimateGas", "simulateTransactions"),
            )

    for name, target in ADDRESSES.items():
        deployment = DEPLOYMENTS[name]
        for block in historical_samples(deployment, anchor):
            cases.append(
                Case(
                    domain="code_history",
                    endpoint="eth_getCode",
                    target=target.lower(),
                    target_name=name,
                    scenario="deployment_state",
                    block=block,
                    actor=funded,
                    request={},
                )
            )
    for name in PROXY_TARGETS:
        for block in historical_samples(DEPLOYMENTS[name], anchor):
            cases.append(
                Case(
                    domain="proxy_history",
                    endpoint="eth_getStorageAt",
                    target=ADDRESSES[name],
                    target_name=name,
                    scenario="eip1967_implementation",
                    block=block,
                    actor=funded,
                    request={
                        "slot": PROXY_IMPLEMENTATION_SLOTS.get(
                            name, IMPLEMENTATION_SLOT
                        )
                    },
                )
            )

    for feature_name, feature_target, variants, domain in (
        ("p256", ADDRESSES["p256"], p256_variants(), "p256"),
        ("pq", ADDRESSES["pq"], pq_variants(), "pq"),
    ):
        for scenario, input_data in variants:
            add_endpoint_cases(
                cases,
                domain=domain,
                target_name=feature_name,
                target=feature_target,
                scenario=scenario,
                actor=funded,
                request=make_request(funded, feature_target, input_data, gas=2_000_000),
                block=anchor,
                endpoints=EXECUTION_ENDPOINTS,
            )

    for scenario, input_data in deterministic_deployer_variants():
        add_endpoint_cases(
            cases,
            domain="important_contract",
            target_name="deterministic_deployer",
            target=ADDRESSES["deterministic_deployer"],
            scenario=f"create2.{scenario}",
            actor=funded,
            request=make_request(
                funded,
                ADDRESSES["deterministic_deployer"],
                input_data,
            ),
            block=anchor,
            endpoints=EXECUTION_ENDPOINTS,
        )

    for offset in range(72):
        requested = anchor - offset
        add_endpoint_cases(
            cases,
            domain="system_accounting",
            target_name="system_accounting",
            target=ADDRESSES["system_accounting"],
            scenario=f"get_gas_values.offset_{offset}",
            actor=funded,
            request=make_request(
                funded,
                ADDRESSES["system_accounting"],
                calldata("0x80510815", word(requested)),
            ),
            block=anchor,
            endpoints=EXECUTION_ENDPOINTS,
        )

    history_offsets = tuple(range(33)) + (
        63,
        64,
        65,
        127,
        128,
        255,
        256,
        1024,
        4096,
        8190,
        8191,
        8192,
    )
    for offset in history_offsets:
        requested = anchor - offset
        add_endpoint_cases(
            cases,
            domain="eip2935",
            target_name="eip2935_history",
            target=ADDRESSES["eip2935_history"],
            scenario=f"block_hash.offset_{offset}",
            actor=funded,
            request=make_request(
                funded,
                ADDRESSES["eip2935_history"],
                "0x" + word(requested),
            ),
            block=anchor,
            endpoints=EXECUTION_ENDPOINTS,
        )

    for (
        label,
        token,
        holder,
        _name,
        _symbol,
        _decimals,
        _supply,
        balance,
    ) in core.IMPORTANT_ERC20_ASSETS:
        holder = holder.lower()
        for scenario, input_data, _unused in asset_mutation_vectors(empty):
            add_endpoint_cases(
                cases,
                domain="asset",
                target_name=label,
                target=token,
                scenario=f"mutation.{scenario}",
                actor=holder,
                request=make_request(holder, token, input_data),
                block=anchor,
                endpoints=EXECUTION_ENDPOINTS,
            )
        dynamic = (
            (
                "mutation.transfer_self_one",
                calldata("0xa9059cbb", address_word(holder), word(1)),
            ),
            (
                "mutation.transfer_overbalance",
                calldata("0xa9059cbb", address_word(empty), word(balance + 1)),
            ),
            (
                "mutation.transfer_from_without_allowance_one",
                calldata(
                    "0x23b872dd", address_word(holder), address_word(empty), word(1)
                ),
            ),
            (
                "mutation.transfer_from_without_allowance_large_amount",
                calldata(
                    "0x23b872dd",
                    address_word(holder),
                    address_word(empty),
                    word(balance + 1),
                ),
            ),
        )
        for scenario, input_data in dynamic:
            add_endpoint_cases(
                cases,
                domain="asset",
                target_name=label,
                target=token,
                scenario=scenario,
                actor=holder,
                request=make_request(holder, token, input_data),
                block=anchor,
                endpoints=EXECUTION_ENDPOINTS,
            )
        if label == "aworp":
            # AWORP is a restricted-transfer clone at this anchor.  Keep its
            # successful allowance update separate from the three assets that
            # support a complete approve -> transferFrom -> balance sequence.
            sequence = (
                (
                    "approve_self_one",
                    calldata("0x095ea7b3", address_word(holder), word(1)),
                ),
                (
                    "allowance_after_approve",
                    calldata("0xdd62ed3e", address_word(holder), address_word(holder)),
                ),
            )
            group = f"{label}.approve_then_allowance"
        else:
            sequence = (
                (
                    "approve_self_one",
                    calldata("0x095ea7b3", address_word(holder), word(1)),
                ),
                (
                    "transfer_from_one",
                    calldata(
                        "0x23b872dd",
                        address_word(holder),
                        address_word(empty),
                        word(1),
                    ),
                ),
                ("recipient_balance", calldata("0x70a08231", address_word(empty))),
            )
            group = f"{label}.approve_transfer_from_balance"
        for position, (scenario, input_data) in enumerate(sequence):
            add_endpoint_cases(
                cases,
                domain="asset",
                target_name=label,
                target=token,
                scenario=f"sequence.{scenario}",
                actor=holder,
                request=make_request(holder, token, input_data),
                block=anchor,
                endpoints=("simulateTransactions",),
                group=group,
                position=position,
            )
        if label != "aworp":
            overbalance_group = f"{label}.approve_max_then_overbalance"
            for position, (scenario, input_data) in enumerate(
                (
                    (
                        "approve_max_for_overbalance",
                        calldata(
                            "0x095ea7b3",
                            address_word(holder),
                            word((1 << 256) - 1),
                        ),
                    ),
                    (
                        "transfer_from_overbalance_after_approval",
                        calldata(
                            "0x23b872dd",
                            address_word(holder),
                            address_word(empty),
                            word(balance + 1),
                        ),
                    ),
                )
            ):
                add_endpoint_cases(
                    cases,
                    domain="asset",
                    target_name=label,
                    target=token,
                    scenario=f"sequence.{scenario}",
                    actor=holder,
                    request=make_request(holder, token, input_data),
                    block=anchor,
                    endpoints=("simulateTransactions",),
                    group=overbalance_group,
                    position=position,
                )

    for label, token in EXPECTED_UNDEPLOYED_ASSETS.items():
        cases.append(
            Case(
                domain="asset",
                endpoint="eth_getCode",
                target=token,
                target_name=label,
                scenario="not_deployed_at_anchor",
                block=anchor,
                actor=funded,
                request={},
            )
        )

    for replay in HISTORICAL_REPLAYS:
        add_endpoint_cases(
            cases,
            domain="historical_business",
            target_name=replay.name,
            target=str(replay.request["to"]),
            scenario=f"tx.{replay.tx_hash}",
            actor=str(replay.request["from"]),
            request=dict(replay.request),
            block=replay.state_block,
            endpoints=EXECUTION_ENDPOINTS,
        )

    for target_name, scenario, input_data in access_control_vectors(empty):
        add_endpoint_cases(
            cases,
            domain="arc_access_control",
            target_name=target_name,
            target=ADDRESSES[target_name],
            scenario=scenario,
            actor=funded,
            request=make_request(funded, ADDRESSES[target_name], input_data),
            block=anchor,
            endpoints=EXECUTION_ENDPOINTS,
        )

    return Plan.from_cases(cases)


class BatchRpcClient:
    """JSON-RPC batch client with logical and HTTP request accounting."""

    def __init__(self, url: str, timeout: float, retries: int):
        self.url = url
        self.timeout = timeout
        self.retries = retries
        self.logical_requests = 0
        self.http_requests = 0
        self.retry_count = 0
        self._next_id = 1

    def batch_capture(
        self,
        calls: Sequence[tuple[str, list[Any]]],
        *,
        chunk_size: int = 64,
    ) -> list[dict[str, Any]]:
        if chunk_size < 1:
            raise ValueError("batch chunk size must be positive")
        captured: list[dict[str, Any]] = []
        for start in range(0, len(calls), chunk_size):
            chunk = calls[start : start + chunk_size]
            payload = []
            order: list[int] = []
            for method, params in chunk:
                request_id = self._next_id
                self._next_id += 1
                order.append(request_id)
                payload.append(
                    {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "method": method,
                        "params": params,
                    }
                )
            self.logical_requests += len(chunk)
            body = json.dumps(payload, separators=(",", ":")).encode()
            response = self._request(body)
            if not isinstance(response, list):
                raise core.RpcTransportError("batch RPC response is not an array")
            indexed: dict[int, dict[str, Any]] = {}
            for item in response:
                if (
                    not isinstance(item, dict)
                    or item.get("jsonrpc") != "2.0"
                    or not isinstance(item.get("id"), int)
                    or isinstance(item.get("id"), bool)
                ):
                    raise core.RpcTransportError("batch RPC item has no integer id")
                if item["id"] in indexed:
                    raise core.RpcTransportError(
                        "batch RPC response contains duplicate ids"
                    )
                indexed[item["id"]] = item
            if set(indexed) != set(order):
                raise core.RpcTransportError(
                    "batch RPC response ids do not match request ids"
                )
            for request_id in order:
                item = indexed[request_id]
                if "error" in item:
                    error = item["error"]
                    if not isinstance(error, dict):
                        raise core.RpcTransportError("batch RPC error is not an object")
                    raw_code = error.get("code", -32603)
                    raw_message = error.get("message", "RPC error")
                    if not isinstance(raw_code, int) or isinstance(raw_code, bool):
                        raise core.RpcTransportError("batch RPC error code is not an integer")
                    if not isinstance(raw_message, str):
                        raise core.RpcTransportError("batch RPC error message is not a string")
                    normalized_error = {
                        "code": raw_code,
                        "message": raw_message,
                    }
                    if "data" in error:
                        normalized_error["data"] = error["data"]
                    captured.append({"ok": False, "error": normalized_error})
                elif "result" in item:
                    captured.append({"ok": True, "result": item["result"]})
                else:
                    raise core.RpcTransportError(
                        "batch RPC item has neither result nor error"
                    )
        return captured

    def capture(self, method: str, params: list[Any]) -> dict[str, Any]:
        return self.batch_capture([(method, params)], chunk_size=1)[0]

    def call(self, method: str, params: list[Any]) -> Any:
        result = self.capture(method, params)
        if result["ok"]:
            return result["result"]
        error = result["error"]
        raise core.RpcCallError(error["code"], error["message"])

    def _request(self, body: bytes) -> Any:
        for attempt in range(self.retries + 1):
            self.http_requests += 1
            request = urllib.request.Request(
                self.url,
                data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            try:
                with urllib.request.urlopen(request, timeout=self.timeout) as response:
                    return json.load(response)
            except urllib.error.HTTPError as error:
                retryable = error.code == 429 or 500 <= error.code <= 599
                if not retryable or attempt == self.retries:
                    raise core.RpcTransportError(f"HTTP {error.code}") from error
            except (
                urllib.error.URLError,
                TimeoutError,
                OSError,
                json.JSONDecodeError,
            ) as error:
                if attempt == self.retries:
                    raise core.RpcTransportError(
                        f"transport failure ({type(error).__name__})"
                    ) from error
            self.retry_count += 1
            time.sleep(0.5 * (2**attempt))
        raise AssertionError("unreachable")


def capture_error_class(result: dict[str, Any], *, leafage_custom: bool = False) -> str:
    if result.get("ok"):
        return "success"
    error = result.get("error")
    code = error.get("code") if isinstance(error, dict) else None
    if leafage_custom:
        return {
            core.LEAFAGE_EVM_REVERT: "revert",
            core.LEAFAGE_GAS_EXHAUSTED: "out-of-gas",
            core.LEAFAGE_BALANCE_EXHAUSTED: "insufficient-funds",
            -39003: "nonce",
        }.get(code, core.rpc_error_class(result))
    return core.rpc_error_class(result)


def capture_output(result: dict[str, Any]) -> str | None:
    if not result.get("ok"):
        return None
    return core.data(result.get("result"))


def normalize_error_reason(message: Any) -> str:
    """Normalize transport-specific prefixes without erasing the actual reason."""

    normalized = " ".join(str(message or "").strip().lower().split())
    if "out of gas" in normalized or "outofgas" in normalized:
        subtype = (
            ":invalid-operand"
            if "invalidoperand"
            in normalized.replace(" ", "").replace("_", "").replace("-", "")
            else ""
        )
        return f"out-of-gas{subtype}"
    if "opcodenotfound" in normalized.replace(" ", "").replace("_", "").replace(
        "-", ""
    ):
        return "opcode-not-found"
    prefixes = (
        "execution reverted",
        "execution revert",
        "revert",
        "evm error",
        "halted",
    )
    while normalized:
        previous = normalized
        for prefix in prefixes:
            if normalized == prefix:
                normalized = ""
                break
            if normalized.startswith(prefix + ":"):
                normalized = normalized[len(prefix) + 1 :].strip()
                break
        if normalized == previous:
            break
    return normalized.rstrip(": ")


def capture_error_details(result: dict[str, Any]) -> dict[str, Any] | None:
    if result.get("ok"):
        return None
    error = result.get("error")
    if not isinstance(error, dict):
        return {"code": None, "reason": "", "data": None}
    raw_data = error.get("data")
    if isinstance(raw_data, str) and raw_data.startswith("0x"):
        raw_data = core.data(raw_data)
    return {
        "code": error.get("code"),
        "reason": normalize_error_reason(error.get("message")),
        "data": raw_data,
    }


def custom_error_capture(code: Any, message: Any) -> dict[str, Any]:
    return {
        "ok": False,
        "error": {"code": code, "message": str(message or "")},
    }


def custom_result_schema(item: dict[str, Any]) -> bool:
    code = item.get("code")
    error = item.get("err")
    gas_used = item.get("gas_used")
    return (
        isinstance(code, int)
        and not isinstance(code, bool)
        and code in DEBANK_RESULT_CODES
        and isinstance(error, str)
        and (code != 0 or error == "")
        and isinstance(gas_used, int)
        and not isinstance(gas_used, bool)
        and gas_used >= 0
    )


def nonnegative_finite_number(value: Any) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
        and value >= 0
    )


def quantity_equals(value: Any, expected: int | None) -> bool:
    try:
        return core.quantity(value) == expected
    except (TypeError, ValueError):
        return False


def safe_quantity(value: Any) -> int | None:
    try:
        return core.quantity(value)
    except (TypeError, ValueError):
        return None


def safe_data(value: Any) -> str | None:
    try:
        return core.data(value)
    except (TypeError, ValueError):
        return None


def leafage_trace_flags_schema(traces: Any) -> bool:
    return isinstance(traces, list) and all(
        isinstance(trace, dict)
        and isinstance(trace.get("self_storage_change"), bool)
        and isinstance(trace.get("storage_change"), bool)
        for trace in traces
    )


def transaction_intrinsic_gas(request: dict[str, Any]) -> int:
    input_data = bytes.fromhex(core.data(request.get("data", "0x"))[2:])
    gas = 21_000 + sum(4 if byte == 0 else 16 for byte in input_data)
    if "to" not in request:
        gas += 32_000
    access_list = request.get("accessList", [])
    if not isinstance(access_list, list):
        raise TypeError("accessList must be an array")
    for entry in access_list:
        if not isinstance(entry, dict) or not isinstance(
            entry.get("storageKeys", []), list
        ):
            raise TypeError("invalid accessList entry")
        gas += 2_400 + 1_900 * len(entry.get("storageKeys", []))
    return gas


def failed_trace_root_matches(
    case: Case,
    normalized_traces: Any,
    expected_output: str,
) -> bool:
    if not isinstance(normalized_traces, list) or len(normalized_traces) != 1:
        return False
    roots = [frame for frame in normalized_traces if frame.get("path") == []]
    if len(roots) != 1:
        return False
    root = roots[0]
    expected_kind = "call" if "to" in case.request else "create"
    expected_target = (
        core.address(case.request["to"]) if "to" in case.request else root.get("to")
    )
    try:
        intrinsic_gas = transaction_intrinsic_gas(case.request)
        root_gas_used = root.get("gas_used")
        gas_used_matches = (
            isinstance(root_gas_used, int)
            and not isinstance(root_gas_used, bool)
            and 0 <= root_gas_used <= root["gas_limit"]
        )
        return (
            root.get("kind") == expected_kind
            and root.get("call_type") == expected_kind
            and root.get("from") == core.address(case.request.get("from"))
            and root.get("to") == expected_target
            and root.get("value") == core.quantity(case.request.get("value", "0x0"))
            and root.get("input") == core.data(case.request.get("data", "0x"))
            and root.get("output") == core.data(expected_output)
            and root.get("gas_limit")
            == core.quantity(case.request.get("gas")) - intrinsic_gas
            and gas_used_matches
        )
    except (TypeError, ValueError):
        return False


def error_detail_dimensions(
    expected: dict[str, Any],
    actual: dict[str, Any],
    *,
    compare_code: bool,
    compare_data: bool,
) -> tuple[dict[str, bool], dict[str, Any] | None, dict[str, Any] | None]:
    expected_details = capture_error_details(expected)
    actual_details = capture_error_details(actual)
    if expected_details is None or actual_details is None:
        return {}, expected_details, actual_details
    dimensions = {
        "error_reason": expected_details["reason"] == actual_details["reason"],
    }
    if compare_code:
        dimensions["error_code"] = expected_details["code"] == actual_details["code"]
    if compare_data:
        dimensions["error_data"] = expected_details["data"] == actual_details["data"]
    return dimensions, expected_details, actual_details


def result_from_dimensions(
    case: Case,
    dimensions: dict[str, bool],
    expected: Any,
    actual: Any,
    note: str | None = None,
) -> CaseResult:
    return CaseResult(
        case=case,
        passed=all(dimensions.values()),
        dimensions=dimensions,
        expected=expected,
        actual=actual,
        note=note,
    )


def execute_eth_calls(
    cases: Sequence[Case],
    leafage: BatchRpcClient,
    reference: BatchRpcClient,
    anchor: int,
) -> list[CaseResult]:
    calls = [("eth_call", [case.request, hex(case.block)]) for case in cases]
    expected = reference.batch_capture(calls)
    actual = leafage.batch_capture(calls)
    results: list[CaseResult] = []
    for case, writer_result, leafage_result in zip(cases, expected, actual):
        expected_class = capture_error_class(writer_result)
        actual_class = capture_error_class(leafage_result)
        expected_output = capture_output(writer_result)
        actual_output = capture_output(leafage_result)
        dimensions = {"class": expected_class == actual_class}
        if expected_class == "success":
            dimensions["output"] = expected_output == actual_output
        expected_error = actual_error = None
        if expected_class != "success" and actual_class != "success":
            error_dimensions, expected_error, actual_error = error_detail_dimensions(
                writer_result,
                leafage_result,
                compare_code=True,
                compare_data=True,
            )
            dimensions.update(error_dimensions)
        if case.scenario.startswith("view.") and case.block == anchor:
            dimensions["writer_view_nonempty"] = (
                expected_class == "success" and expected_output not in {None, "0x"}
            )
        results.append(
            result_from_dimensions(
                case,
                dimensions,
                {
                    "class": expected_class,
                    "output": expected_output,
                    "error": expected_error,
                },
                {
                    "class": actual_class,
                    "output": actual_output,
                    "error": actual_error,
                },
            )
        )
    return results


def execute_code_history(
    cases: Sequence[Case],
    leafage: BatchRpcClient,
    reference: BatchRpcClient,
    view_target_names: set[str],
) -> list[CaseResult]:
    calls = [("eth_getCode", [case.target, hex(case.block)]) for case in cases]
    expected = reference.batch_capture(calls)
    actual = leafage.batch_capture(calls)
    results: list[CaseResult] = []
    for case, writer_result, leafage_result in zip(cases, expected, actual):
        writer_code = capture_output(writer_result)
        leafage_code = capture_output(leafage_result)
        dimensions = {
            "rpc_success": writer_result.get("ok") is True
            and leafage_result.get("ok") is True,
            "code": writer_code == leafage_code,
        }
        if case.scenario == "not_deployed_at_anchor":
            dimensions["writer_deployment_state"] = writer_code == "0x"
        elif case.target_name in view_target_names:
            deployment = DEPLOYMENTS[case.target_name]
            should_exist = case.block >= deployment
            dimensions["writer_deployment_state"] = (
                writer_code not in {None, "0x"} if should_exist else writer_code == "0x"
            )
        results.append(
            result_from_dimensions(
                case,
                dimensions,
                writer_code,
                leafage_code,
            )
        )
    return results


def execute_proxy_history(
    cases: Sequence[Case],
    leafage: BatchRpcClient,
    reference: BatchRpcClient,
) -> list[CaseResult]:
    calls = [
        ("eth_getStorageAt", [case.target, case.request["slot"], hex(case.block)])
        for case in cases
    ]
    expected = reference.batch_capture(calls)
    actual = leafage.batch_capture(calls)
    results: list[CaseResult] = []
    for case, writer_result, leafage_result in zip(cases, expected, actual):
        writer_value = capture_output(writer_result)
        leafage_value = capture_output(leafage_result)
        deployment = DEPLOYMENTS[case.target_name]
        should_exist = case.block >= deployment
        expected_nonzero = writer_value is not None and int(writer_value, 16) != 0
        dimensions = {
            "rpc_success": writer_result.get("ok") is True
            and leafage_result.get("ok") is True,
            "storage": writer_value == leafage_value,
            "writer_deployment_state": expected_nonzero
            if should_exist
            else writer_value == "0x" + "00" * 32,
        }
        results.append(
            result_from_dimensions(case, dimensions, writer_value, leafage_value)
        )
    return results


def execute_estimates(
    cases: Sequence[Case],
    leafage: BatchRpcClient,
    reference: BatchRpcClient,
) -> list[CaseResult]:
    writer_calls = [
        ("eth_estimateGas", [case.request, hex(case.block)]) for case in cases
    ]
    leafage_calls = [
        (
            "estimateGas",
            [case.request, {"block_id": hex(case.block), "type": "Equals"}, None],
        )
        for case in cases
    ]
    expected = reference.batch_capture(writer_calls, chunk_size=32)
    actual = leafage.batch_capture(leafage_calls, chunk_size=32)
    results: list[CaseResult] = []
    for case, writer_result, leafage_result in zip(cases, expected, actual):
        expected_class = capture_error_class(writer_result)
        actual_class = capture_error_class(leafage_result, leafage_custom=True)
        expected_gas = (
            core.quantity(writer_result["result"]) if writer_result.get("ok") else None
        )
        actual_gas = (
            core.quantity(leafage_result["result"])
            if leafage_result.get("ok")
            else None
        )
        dimensions = {"class": expected_class == actual_class}
        if expected_class == "success":
            dimensions["gas"] = expected_gas == actual_gas
        expected_error = actual_error = None
        if expected_class != "success" and actual_class != "success":
            error_dimensions, expected_error, actual_error = error_detail_dimensions(
                writer_result,
                leafage_result,
                compare_code=False,
                compare_data=False,
            )
            dimensions.update(error_dimensions)
        results.append(
            result_from_dimensions(
                case,
                dimensions,
                {
                    "class": expected_class,
                    "gas": expected_gas,
                    "error": expected_error,
                },
                {
                    "class": actual_class,
                    "gas": actual_gas,
                    "error": actual_error,
                },
            )
        )
    return results


def chunks(values: Sequence[Any], size: int) -> list[list[Any]]:
    return [list(values[index : index + size]) for index in range(0, len(values), size)]


def execute_contract_multicalls(
    cases: Sequence[Case],
    leafage: BatchRpcClient,
    reference: BatchRpcClient,
    blocks: dict[int, dict[str, Any]],
) -> list[CaseResult]:
    writer_calls = [("eth_call", [case.request, hex(case.block)]) for case in cases]
    trace_calls = [
        ("debug_traceCall", [case.request, hex(case.block), {"tracer": "callTracer"}])
        for case in cases
    ]
    expected_calls = reference.batch_capture(writer_calls, chunk_size=32)
    expected_traces = reference.batch_capture(trace_calls, chunk_size=32)
    indexed_by_block: dict[int, list[tuple[int, Case]]] = {}
    for item in enumerate(cases):
        indexed_by_block.setdefault(item[1].block, []).append(item)
    grouped = [
        group
        for block_cases in indexed_by_block.values()
        for group in chunks(block_cases, 32)
    ]
    leafage_requests = [
        (
            "contractMultiCall",
            [
                [case.request for _index, case in group],
                {"block_id": hex(group[0][1].block), "type": "Equals"},
                None,
                None,
                False,
                False,
                False,
            ],
        )
        for group in grouped
    ]
    leafage_batches = leafage.batch_capture(leafage_requests, chunk_size=16)
    by_index: dict[int, tuple[dict[str, Any], dict[str, Any]]] = {}
    expected_group_success: dict[int, bool] = {}
    for group, batch_result in zip(grouped, leafage_batches):
        group_success = all(
            capture_error_class(expected_calls[index]) == "success"
            for index, _case in group
        )
        for index, _case in group:
            expected_group_success[index] = group_success
        if not batch_result.get("ok") or not isinstance(
            batch_result.get("result"), dict
        ):
            for index, _case in group:
                by_index[index] = (batch_result, {})
            continue
        payload = batch_result["result"]
        items = payload.get("results")
        stats = payload.get("stats")
        if (
            not isinstance(items, list)
            or len(items) != len(group)
            or not isinstance(stats, dict)
        ):
            malformed = {
                "ok": False,
                "error": {
                    "code": -1,
                    "message": "malformed contractMultiCall response",
                },
            }
            for index, _case in group:
                by_index[index] = (malformed, {})
            continue
        for (index, _case), item in zip(group, items):
            by_index[index] = ({"ok": True, "result": item}, stats)

    results: list[CaseResult] = []
    for index, (case, writer_result, trace_result) in enumerate(
        zip(cases, expected_calls, expected_traces)
    ):
        leafage_capture, stats = by_index[index]
        expected_class = capture_error_class(writer_result)
        expected_output = capture_output(writer_result)
        expected_gas: int | None = None
        if trace_result.get("ok") and isinstance(trace_result.get("result"), dict):
            trace = trace_result["result"]
            expected_gas = core.quantity(trace.get("gasUsed"))
        leafage_item = (
            leafage_capture.get("result") if leafage_capture.get("ok") else None
        )
        if isinstance(leafage_item, dict):
            actual_class = core.leafage_result_error_class(leafage_item)
            actual_output = safe_data(leafage_item.get("result"))
            actual_gas = safe_quantity(leafage_item.get("gas_used"))
            item_schema = custom_result_schema(leafage_item) and actual_output is not None
            from_cache = leafage_item.get("from_cache") is False
            time_cost = nonnegative_finite_number(leafage_item.get("time_cost"))
            actual_error_capture = custom_error_capture(
                leafage_item.get("code"), leafage_item.get("err")
            )
        else:
            item_schema = False
            actual_class = capture_error_class(leafage_capture, leafage_custom=True)
            actual_output = None
            actual_gas = None
            from_cache = False
            time_cost = False
            actual_error_capture = leafage_capture
        expected_error = actual_error = None
        error_dimensions: dict[str, bool] = {}
        if expected_class != "success" and actual_class != "success":
            error_dimensions, expected_error, actual_error = error_detail_dimensions(
                writer_result,
                actual_error_capture,
                compare_code=False,
                compare_data=False,
            )
        block = blocks.get(case.block)
        expected_block_hash = (
            core.block_hash(block.get("hash")) if isinstance(block, dict) else None
        )
        expected_block_time = (
            core.quantity(block.get("timestamp")) if isinstance(block, dict) else None
        )
        dimensions = {
            "schema": item_schema,
            "class": expected_class == actual_class,
            "gas": expected_gas == actual_gas if expected_gas is not None else False,
            "from_cache": from_cache,
            "time_cost": time_cost,
            "stats": (
                quantity_equals(stats.get("block_num"), case.block)
                and str(stats.get("block_hash", "")).lower() == expected_block_hash
                and quantity_equals(stats.get("block_time"), expected_block_time)
                and stats.get("success") is expected_group_success[index]
                and stats.get("cache_enabled") is False
            ),
            **error_dimensions,
        }
        if expected_class == "success":
            dimensions["output"] = expected_output == actual_output
        recorded_expected_output = (
            expected_output if expected_class == "success" else None
        )
        recorded_actual_output = actual_output if expected_class == "success" else None
        results.append(
            result_from_dimensions(
                case,
                dimensions,
                {
                    "class": expected_class,
                    "output": recorded_expected_output,
                    "gas": expected_gas,
                    "error": expected_error,
                },
                {
                    "class": actual_class,
                    "output": recorded_actual_output,
                    "gas": actual_gas,
                    "error": actual_error,
                },
                (
                    "contractMultiCall does not expose failed-call revert data; "
                    "class, reason, gas, schema, and metadata are compared."
                    if expected_class != "success"
                    else None
                ),
            )
        )
    return results


def simulation_groups(cases: Sequence[Case]) -> list[list[Case]]:
    view_cases = [case for case in cases if case.scenario.startswith("view.")]
    # Every request carries a 1M gas limit.  Groups of 32 would exceed Arc's
    # 30M block gas limit even though the actual view gas is much smaller.
    views_by_block: dict[int, list[Case]] = {}
    for case in view_cases:
        views_by_block.setdefault(case.block, []).append(case)
    grouped: list[list[Case]] = [
        group
        for block_cases in views_by_block.values()
        for group in chunks(block_cases, 16)
    ]
    explicit: dict[str, list[Case]] = {}
    singles: list[Case] = []
    for case in cases:
        if case.scenario.startswith("view."):
            continue
        if case.group:
            explicit.setdefault(case.group, []).append(case)
        else:
            singles.append(case)
    for group in explicit.values():
        grouped.append(sorted(group, key=lambda item: item.position or 0))
    grouped.extend([[case] for case in singles])
    return grouped


def simulation_item_class(item: dict[str, Any]) -> str:
    if core.quantity(item.get("status")) == 1:
        return "success"
    error = item.get("error")
    if isinstance(error, dict):
        return core.rpc_error_class({"ok": False, "error": error})
    return "other-halt"


def normalize_visible_reference_traces(
    traces: Any, root_output: str
) -> list[dict[str, Any]]:
    """Match DebankTrace's successful-frame-only representation.

    The writer's Parity trace includes reverted children, while DebankTrace has
    no failure field and intentionally omits them.  Drop failed subtrees,
    densify the remaining sibling paths, and take the root output from the
    already compared eth_simulateV1 result because pre_traceMany does not
    apply every environment override to that field.
    """

    if not isinstance(traces, list):
        raise TypeError("reference traces must be an array")
    by_path: dict[tuple[int, ...], dict[str, Any]] = {}
    for trace in traces:
        if not isinstance(trace, dict) or not isinstance(
            trace.get("traceAddress"), list
        ):
            raise TypeError("reference trace has no traceAddress")
        path = tuple(trace["traceAddress"])
        by_path[path] = trace
    kept: set[tuple[int, ...]] = set()
    for path in sorted(by_path, key=lambda item: (len(item), item)):
        trace = by_path[path]
        if trace.get("error") is not None:
            continue
        if path and path[:-1] not in kept:
            continue
        kept.add(path)
    normalized = core.normalize_reference_traces([by_path[path] for path in kept])
    children: dict[tuple[int, ...], list[int]] = {}
    for path in kept:
        if path:
            children.setdefault(path[:-1], []).append(path[-1])
    dense = {
        parent: {old: new for new, old in enumerate(sorted(indices))}
        for parent, indices in children.items()
    }
    for frame in normalized:
        original = tuple(frame["path"])
        frame["path"] = [
            dense[original[:index]][child] for index, child in enumerate(original)
        ]
        if not original:
            frame["output"] = core.data(root_output)
    return normalized


def execute_simulations(
    cases: Sequence[Case],
    leafage: BatchRpcClient,
    reference: BatchRpcClient,
    blocks: dict[int, dict[str, Any]],
    block_overrides: dict[int, dict[str, Any]],
) -> list[CaseResult]:
    grouped = simulation_groups(cases)
    if any(len({case.block for case in group}) != 1 for group in grouped):
        raise RuntimeError("simulation group spans multiple state blocks")
    reference_requests = [
        (
            "eth_simulateV1",
            [
                {
                    "blockStateCalls": [
                        {
                            "blockOverrides": block_overrides[group[0].block],
                            "calls": [case.request for case in group],
                        }
                    ],
                    "validation": False,
                    "traceTransfers": False,
                },
                hex(group[0].block),
            ],
        )
        for group in grouped
    ]
    leafage_requests = [
        (
            "simulateTransactions",
            [
                [case.request for case in group],
                {"block_id": hex(group[0].block), "type": "Equals"},
                block_overrides[group[0].block],
            ],
        )
        for group in grouped
    ]
    trace_requests = [
        (
            "pre_traceMany",
            [
                [case.request for case in group],
                hex(group[0].block + 1),
                None,
                None,
            ],
        )
        for group in grouped
    ]
    expected_groups = reference.batch_capture(reference_requests, chunk_size=8)
    expected_trace_groups = reference.batch_capture(trace_requests, chunk_size=8)
    actual_groups = leafage.batch_capture(leafage_requests, chunk_size=8)
    results: list[CaseResult] = []
    for group, writer_capture, trace_capture, leafage_capture in zip(
        grouped, expected_groups, expected_trace_groups, actual_groups
    ):
        writer_items: Any = None
        writer_trace_items: Any = None
        leafage_items: Any = None
        stats: dict[str, Any] = {}
        if writer_capture.get("ok"):
            simulated_blocks = writer_capture.get("result")
            if (
                isinstance(simulated_blocks, list)
                and len(simulated_blocks) == 1
                and isinstance(simulated_blocks[0], dict)
            ):
                writer_items = simulated_blocks[0].get("calls")
        if trace_capture.get("ok"):
            writer_trace_items = trace_capture.get("result")
        if leafage_capture.get("ok") and isinstance(
            leafage_capture.get("result"), dict
        ):
            leafage_items = leafage_capture["result"].get("results")
            raw_stats = leafage_capture["result"].get("stats")
            if isinstance(raw_stats, dict):
                stats = raw_stats
        schema_ok = (
            isinstance(writer_items, list)
            and isinstance(writer_trace_items, list)
            and isinstance(leafage_items, list)
            and len(writer_items) == len(group)
            and len(writer_trace_items) == len(group)
            and len(leafage_items) == len(group)
        )
        if not schema_ok:
            for case in group:
                results.append(
                    result_from_dimensions(
                        case,
                        {"schema": False},
                        writer_capture,
                        leafage_capture,
                    )
                )
            continue
        aggregate_success = all(
            isinstance(item, dict) and simulation_item_class(item) == "success"
            for item in writer_items
        )
        for case, writer_item, writer_trace_item, leafage_item in zip(
            group, writer_items, writer_trace_items, leafage_items
        ):
            if (
                not isinstance(writer_item, dict)
                or not isinstance(writer_trace_item, dict)
                or not isinstance(leafage_item, dict)
            ):
                results.append(
                    result_from_dimensions(
                        case, {"schema": False}, writer_item, leafage_item
                    )
                )
                continue
            expected_class = simulation_item_class(writer_item)
            actual_class = core.leafage_result_error_class(leafage_item)
            expected_output = core.data(writer_item.get("returnData", "0x"))
            try:
                actual_output = core.leafage_root_output(leafage_item)
            except (TypeError, ValueError):
                actual_output = None
            expected_gas = core.quantity(writer_item.get("gasUsed"))
            actual_gas = safe_quantity(leafage_item.get("gas_used"))
            item_schema = custom_result_schema(leafage_item)
            try:
                expected_logs = core.normalize_logs(writer_item.get("logs", []))
                actual_logs = core.normalize_leafage_events(
                    leafage_item.get("events", [])
                )
                logs_match = expected_logs == actual_logs
            except (TypeError, ValueError):
                expected_logs = None
                actual_logs = None
                logs_match = False
            raw_leafage_traces = leafage_item.get("traces")
            trace_schema = leafage_trace_flags_schema(raw_leafage_traces)
            expected_traces: Any = None
            actual_traces: Any = None
            try:
                core.leafage_event_attachments(
                    raw_leafage_traces, leafage_item.get("events")
                )
                actual_traces = core.normalize_leafage_traces(raw_leafage_traces)
            except (TypeError, ValueError):
                trace_schema = False
            reference_trace_success = writer_trace_item.get("error") is None
            trace_status = reference_trace_success == (actual_class == "success")
            trace_match = False
            if reference_trace_success and actual_class == "success" and trace_schema:
                try:
                    expected_traces = normalize_visible_reference_traces(
                        writer_trace_item.get("trace"), expected_output
                    )
                    trace_match = expected_traces == actual_traces
                except (TypeError, ValueError):
                    trace_match = False
            elif (
                not reference_trace_success
                and expected_class != "success"
                and actual_class != "success"
                and trace_schema
            ):
                trace_match = failed_trace_root_matches(
                    case,
                    actual_traces,
                    expected_output,
                )
            expected_error = actual_error = None
            error_dimensions: dict[str, bool] = {}
            if expected_class != "success" and actual_class != "success":
                raw_writer_error = writer_item.get("error")
                writer_error_capture = {
                    "ok": False,
                    "error": (
                        raw_writer_error
                        if isinstance(raw_writer_error, dict)
                        else {"code": None, "message": str(raw_writer_error or "")}
                    ),
                }
                leafage_error_capture = custom_error_capture(
                    leafage_item.get("code"), leafage_item.get("err")
                )
                error_dimensions, expected_error, actual_error = (
                    error_detail_dimensions(
                        writer_error_capture,
                        leafage_error_capture,
                        compare_code=False,
                        compare_data=False,
                    )
                )
            block = blocks.get(case.block)
            expected_block_hash = (
                core.block_hash(block.get("hash")) if isinstance(block, dict) else None
            )
            expected_block_time = (
                core.quantity(block.get("timestamp"))
                if isinstance(block, dict)
                else None
            )
            dimensions = {
                "schema": item_schema,
                "class": expected_class == actual_class,
                "output": expected_output == actual_output,
                "gas": expected_gas == actual_gas,
                "events": logs_match,
                "trace_schema": trace_schema,
                "trace_status": trace_status,
                "trace": trace_match,
                "stats": (
                    quantity_equals(stats.get("block_num"), case.block)
                    and str(stats.get("block_hash", "")).lower() == expected_block_hash
                    and quantity_equals(stats.get("block_time"), expected_block_time)
                    and stats.get("success") is aggregate_success
                ),
                **error_dimensions,
            }
            if case.scenario.startswith("view."):
                dimensions["view_storage_flags"] = isinstance(
                    raw_leafage_traces, list
                ) and all(
                    trace.get("self_storage_change") is False
                    and trace.get("storage_change") is False
                    for trace in raw_leafage_traces
                    if isinstance(trace, dict)
                )
            if case.target_name == "usdc" and case.scenario in {
                "mutation.transfer_one",
                "sequence.transfer_from_one",
            }:
                dimensions["eip7708_system_event"] = any(
                    item.get("address") == core.SYSTEM_ADDRESS
                    for item in expected_logs or []
                )
            results.append(
                result_from_dimensions(
                    case,
                    dimensions,
                    {
                        "class": expected_class,
                        "output": expected_output,
                        "gas": expected_gas,
                        "events": expected_logs,
                        "traces": expected_traces,
                        "error": expected_error,
                    },
                    {
                        "class": actual_class,
                        "output": actual_output,
                        "gas": actual_gas,
                        "events": actual_logs,
                        "traces": actual_traces,
                        "error": actual_error,
                    },
                    "Reasonless REVERT normalizes writer's generic message and Leafage's accepted empty message to the same semantic reason.",
                )
            )
    return results


def plan_metadata(plan: Plan) -> dict[str, Any]:
    return {
        "baseline_functional_cases": BASELINE_FUNCTIONAL_CASES,
        "ten_x_target": TEN_X_TARGET,
        "cases": len(plan.cases),
        "multiple_of_baseline": round(len(plan.cases) / BASELINE_FUNCTIONAL_CASES, 3),
        "base_vectors": plan.base_vectors,
        "endpoint_counts": plan.endpoint_counts,
        "domain_counts": plan.domain_counts,
        "target_counts": plan.target_counts,
        "counting_rule": (
            "unique target + selector/semantic scenario + block context + actor + endpoint; "
            "status/output/gas/events/trace fields are assertion dimensions, not cases"
        ),
    }


def call_tracer_targets(frame: Any) -> set[str]:
    targets: set[str] = set()

    def visit(item: Any) -> None:
        if not isinstance(item, dict):
            return
        raw_target = item.get("to")
        if isinstance(raw_target, str):
            targets.add(core.address(raw_target))
        children = item.get("calls", [])
        if isinstance(children, list):
            for child in children:
                visit(child)

    visit(frame)
    return targets


def transaction_matches_replay(transaction: Any, replay: HistoricalReplay) -> bool:
    if not isinstance(transaction, dict):
        return False
    expected = replay.request
    fields = {
        "from": core.address(transaction.get("from")),
        "to": core.address(transaction.get("to")),
        "data": core.data(transaction.get("input", "0x")),
        "value": core.quantity(transaction.get("value", "0x0")),
        "gas": core.quantity(transaction.get("gas")),
        "nonce": core.quantity(transaction.get("nonce")),
        "type": core.quantity(transaction.get("type", "0x0")),
    }
    required = {
        "from": core.address(expected.get("from")),
        "to": core.address(expected.get("to")),
        "data": core.data(expected.get("data", "0x")),
        "value": core.quantity(expected.get("value", "0x0")),
        "gas": core.quantity(expected.get("gas")),
        "nonce": core.quantity(expected.get("nonce")),
        "type": core.quantity(expected.get("type", "0x0")),
    }
    for fee_field in ("gasPrice", "maxFeePerGas", "maxPriorityFeePerGas"):
        if fee_field in expected:
            fields[fee_field] = core.quantity(transaction.get(fee_field))
            required[fee_field] = core.quantity(expected[fee_field])
    fields["accessList"] = transaction.get("accessList") or []
    required["accessList"] = expected.get("accessList") or []
    return (
        fields == required
        and core.quantity(transaction.get("blockNumber")) == replay.block
    )


def preflight(
    leafage: BatchRpcClient,
    reference: BatchRpcClient,
    plan: Plan,
    anchor: int,
    funded: str,
    empty: str,
) -> tuple[
    dict[str, Any],
    list[dict[str, Any]],
    dict[int, dict[str, Any]],
    dict[int, dict[str, Any]],
]:
    checks: list[dict[str, Any]] = []

    def add(name: str, passed: bool, expected: Any, actual: Any) -> None:
        checks.append(
            {
                "name": name,
                "passed": passed,
                "expected": compact(expected),
                "actual": compact(actual),
            }
        )

    writer_version = reference.call("web3_clientVersion", [])
    leafage_version = leafage.call("version", [])
    writer_chain = core.quantity(reference.call("eth_chainId", []))
    leafage_chain = core.quantity(leafage.call("eth_chainId", []))
    add(
        "chain_id",
        writer_chain == leafage_chain == core.ARC_CHAIN_ID,
        core.ARC_CHAIN_ID,
        {"writer": writer_chain, "leafage": leafage_chain},
    )
    add(
        "ten_x_case_count",
        len(plan.cases) >= TEN_X_TARGET,
        TEN_X_TARGET,
        len(plan.cases),
    )

    simulation_anchors = {
        case.block for case in plan.cases if case.endpoint == "simulateTransactions"
    }
    eip2935_heights = {
        anchor - int(case.scenario.rsplit("_", 1)[1])
        for case in plan.cases
        if case.domain == "eip2935" and 0 < int(case.scenario.rsplit("_", 1)[1]) < 8192
    }
    system_accounting_heights = {
        anchor - int(case.scenario.rsplit("_", 1)[1])
        for case in plan.cases
        if case.domain == "system_accounting"
        and case.scenario.startswith("get_gas_values.offset_")
        and int(case.scenario.rsplit("_", 1)[1]) < 64
    }
    heights = sorted(
        {case.block for case in plan.cases}
        | simulation_anchors
        | {height + 1 for height in simulation_anchors}
        | eip2935_heights
        | system_accounting_heights
        | {height + 1 for height in system_accounting_heights}
        | {anchor, anchor + 1}
    )
    block_calls = [("eth_getBlockByNumber", [hex(height), False]) for height in heights]
    writer_blocks = reference.batch_capture(block_calls)
    leafage_blocks = leafage.batch_capture(block_calls)
    blocks: dict[int, dict[str, Any]] = {}
    for height, writer_capture, leafage_capture in zip(
        heights, writer_blocks, leafage_blocks
    ):
        writer_block = (
            writer_capture.get("result") if writer_capture.get("ok") else None
        )
        leafage_block = (
            leafage_capture.get("result") if leafage_capture.get("ok") else None
        )
        writer_hash = (
            core.block_hash(writer_block.get("hash"))
            if isinstance(writer_block, dict)
            else None
        )
        leafage_hash = (
            core.block_hash(leafage_block.get("hash"))
            if isinstance(leafage_block, dict)
            else None
        )
        add(
            f"block_context.{height}",
            writer_hash is not None and writer_hash == leafage_hash,
            writer_hash,
            leafage_hash,
        )
        if isinstance(writer_block, dict):
            blocks[height] = writer_block
    anchor_block = blocks.get(anchor)
    successor = blocks.get(anchor + 1)
    if not isinstance(anchor_block, dict) or not isinstance(successor, dict):
        raise TypeError("anchor or successor block is missing")
    anchor_hash = core.block_hash(anchor_block.get("hash"))
    add(
        "audited_anchor_hash",
        anchor != ANCHOR or anchor_hash == ANCHOR_HASH,
        ANCHOR_HASH if anchor == ANCHOR else anchor_hash,
        anchor_hash,
    )
    if core.block_hash(successor.get("parentHash")) != anchor_hash:
        raise RuntimeError("successor does not descend from the selected anchor")
    funded_balance = core.quantity(
        reference.call("eth_getBalance", [funded, hex(anchor)])
    )
    funded_code = core.data(reference.call("eth_getCode", [funded, hex(anchor)]))
    empty_balance = core.quantity(
        reference.call("eth_getBalance", [empty, hex(anchor)])
    )
    empty_code = core.data(reference.call("eth_getCode", [empty, hex(anchor)]))
    add(
        "funded_fixture",
        funded_balance >= core.MIN_FUNDED_BALANCE and funded_code == "0x",
        {"min_balance": core.MIN_FUNDED_BALANCE, "code": "0x"},
        {"balance": funded_balance, "code": funded_code},
    )
    add(
        "empty_fixture",
        empty_balance == 0 and empty_code == "0x",
        {"balance": 0, "code": "0x"},
        {"balance": empty_balance, "code": empty_code},
    )

    replay_calls = [
        ("eth_getTransactionByHash", [replay.tx_hash]) for replay in HISTORICAL_REPLAYS
    ]
    replay_receipts = reference.batch_capture(
        [
            ("eth_getTransactionReceipt", [replay.tx_hash])
            for replay in HISTORICAL_REPLAYS
        ]
    )
    replay_traces = reference.batch_capture(
        [
            ("debug_traceTransaction", [replay.tx_hash, {"tracer": "callTracer"}])
            for replay in HISTORICAL_REPLAYS
        ],
        chunk_size=4,
    )
    replay_transactions = reference.batch_capture(replay_calls)
    for replay, transaction_capture, receipt_capture, trace_capture in zip(
        HISTORICAL_REPLAYS, replay_transactions, replay_receipts, replay_traces
    ):
        transaction = (
            transaction_capture.get("result") if transaction_capture.get("ok") else None
        )
        receipt = receipt_capture.get("result") if receipt_capture.get("ok") else None
        trace = trace_capture.get("result") if trace_capture.get("ok") else None
        add(
            f"historical_business.{replay.name}.transaction",
            transaction_matches_replay(transaction, replay)
            and isinstance(transaction, dict)
            and core.block_hash(transaction.get("blockHash"))
            == core.block_hash(blocks[replay.block].get("hash")),
            {
                "tx_hash": replay.tx_hash,
                "block": replay.block,
                "request": replay.request,
            },
            transaction,
        )
        add(
            f"historical_business.{replay.name}.receipt",
            isinstance(receipt, dict)
            and core.quantity(receipt.get("status")) == 1
            and core.quantity(receipt.get("blockNumber")) == replay.block,
            {"status": 1, "block": replay.block},
            receipt,
        )
        observed_targets = call_tracer_targets(trace)
        required_targets = {core.address(target) for target in replay.nested_targets}
        add(
            f"historical_business.{replay.name}.trace_targets",
            required_targets.issubset(observed_targets),
            sorted(required_targets),
            sorted(observed_targets),
        )

    overrides_by_anchor: dict[int, dict[str, Any]] = {}
    for state_block in simulation_anchors:
        state = blocks.get(state_block)
        execution = blocks.get(state_block + 1)
        if not isinstance(state, dict) or not isinstance(execution, dict):
            raise TypeError(f"missing simulation context at {state_block}")
        state_hash = core.block_hash(state.get("hash"))
        if core.block_hash(execution.get("parentHash")) != state_hash:
            raise RuntimeError(f"simulation successor mismatch at {state_block}")
        overrides_by_anchor[state_block] = core.simulation_block_overrides(
            state_block, state_hash, execution
        )
    return (
        {
            "leafage": leafage_version,
            "reference": writer_version,
        },
        checks,
        blocks,
        overrides_by_anchor,
    )


def execute_plan(
    plan: Plan,
    leafage: BatchRpcClient,
    reference: BatchRpcClient,
    anchor: int,
    blocks: dict[int, dict[str, Any]],
    block_overrides: dict[int, dict[str, Any]],
) -> list[CaseResult]:
    by_endpoint: dict[str, list[Case]] = {}
    for case in plan.cases:
        by_endpoint.setdefault(case.endpoint, []).append(case)
    view_target_names = {target.name for target in build_targets()}
    results: list[CaseResult] = []
    results.extend(
        execute_eth_calls(by_endpoint.get("eth_call", []), leafage, reference, anchor)
    )
    results.extend(
        execute_code_history(
            by_endpoint.get("eth_getCode", []), leafage, reference, view_target_names
        )
    )
    results.extend(
        execute_proxy_history(
            by_endpoint.get("eth_getStorageAt", []), leafage, reference
        )
    )
    results.extend(
        execute_estimates(by_endpoint.get("estimateGas", []), leafage, reference)
    )
    results.extend(
        execute_contract_multicalls(
            by_endpoint.get("contractMultiCall", []), leafage, reference, blocks
        )
    )
    results.extend(
        execute_simulations(
            by_endpoint.get("simulateTransactions", []),
            leafage,
            reference,
            blocks,
            block_overrides,
        )
    )
    if len(results) != len(plan.cases):
        raise RuntimeError(
            f"executed {len(results)} cases but planned {len(plan.cases)}"
        )
    apply_semantic_invariants(results, blocks, anchor)
    return results


def result_expected_output(result: CaseResult) -> str | None:
    if not isinstance(result.expected, dict):
        return None
    output = result.expected.get("output")
    return core.data(output) if isinstance(output, str) else None


def abi_uint_words(output: str | None, count: int) -> tuple[int, ...] | None:
    if output is None or len(output) != 2 + 64 * count:
        return None
    return tuple(
        int(output[2 + index * 64 : 2 + (index + 1) * 64], 16)
        for index in range(count)
    )


def apply_semantic_invariants(
    results: Sequence[CaseResult],
    blocks: dict[int, dict[str, Any]],
    anchor: int,
) -> None:
    by_key = {
        (result.case.endpoint, result.case.target_name, result.case.scenario): result
        for result in results
    }
    comparable_endpoints = ("eth_call", "contractMultiCall", "simulateTransactions")
    for endpoint in comparable_endpoints:
        for offset in range(8):
            current = by_key.get(
                (endpoint, "system_accounting", f"get_gas_values.offset_{offset}")
            )
            wrapped = by_key.get(
                (endpoint, "system_accounting", f"get_gas_values.offset_{offset + 64}")
            )
            if current is not None and wrapped is not None:
                current.dimensions["ring_64"] = result_expected_output(
                    current
                ) == result_expected_output(wrapped)
                current.passed = all(current.dimensions.values())

        for offset in range(64):
            current = by_key.get(
                (endpoint, "system_accounting", f"get_gas_values.offset_{offset}")
            )
            if current is None:
                continue
            words = abi_uint_words(result_expected_output(current), 3)
            block = blocks.get(anchor - offset)
            child = blocks.get(anchor - offset + 1)
            current.dimensions["gas_used_header"] = (
                words is not None
                and isinstance(block, dict)
                and words[0] == core.quantity(block.get("gasUsed"))
            )
            current.dimensions["next_base_fee_child"] = (
                words is not None
                and isinstance(child, dict)
                and words[2] == core.quantity(child.get("baseFeePerGas"))
            )
            current.passed = all(current.dimensions.values())

        nca = by_key.get((endpoint, "native_coin_authority", "view.total_supply"))
        usdc = by_key.get((endpoint, "usdc", "view.total_supply"))
        if nca is not None and usdc is not None:
            nca_output = result_expected_output(nca)
            usdc_output = result_expected_output(usdc)
            nca.dimensions["native_usdc_supply_ratio"] = (
                nca_output is not None
                and usdc_output is not None
                and int(nca_output, 16) == int(usdc_output, 16) * 10**12
            )
            nca.passed = all(nca.dimensions.values())

    for result in results:
        case = result.case
        if case.domain != "eip2935" or case.endpoint not in comparable_endpoints:
            continue
        offset = int(case.scenario.rsplit("_", 1)[1])
        valid_offset = (
            0 <= offset <= 8190
            if case.endpoint == "simulateTransactions"
            else 1 <= offset <= 8191
        )
        if not valid_offset:
            continue
        canonical = blocks.get(anchor - offset)
        expected_hash = (
            core.block_hash(canonical.get("hash"))
            if isinstance(canonical, dict)
            else None
        )
        output = result_expected_output(result)
        result.dimensions["canonical_block_hash"] = (
            output is not None and expected_hash is not None and output == expected_hash
        )
        result.passed = all(result.dimensions.values())


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--block", type=core.parse_block, default=ANCHOR)
    result.add_argument("--funded-address", type=core.parse_address, default=FUNDED)
    result.add_argument("--empty-address", type=core.parse_address, default=EMPTY)
    result.add_argument("--timeout", type=float, default=60.0)
    result.add_argument("--retries", type=int, default=2)
    result.add_argument("--plan-only", action="store_true")
    result.add_argument("--output", type=Path)
    return result


def write_report(payload: dict[str, Any], output: Path | None) -> None:
    encoded = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    if output:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded)
    else:
        print(encoded, end="")


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    if args.retries < 0 or args.timeout <= 0:
        parser().error("timeout must be positive and retries must be non-negative")
    plan = build_plan(args.block, args.funded_address, args.empty_address)
    started = time.monotonic()
    payload: dict[str, Any] = {
        "schema_version": 1,
        "suite": "arc-mainnet-breadth",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "scope": {
            "network": "Arc mainnet",
            "chain_id": core.ARC_CHAIN_ID,
            "public_rpc": False,
            "testnet": False,
            "leafage_writer_differential": not args.plan_only,
        },
        "block": args.block,
        "plan": plan_metadata(plan),
        "complete": args.plan_only,
        "summary": {
            "cases": 0,
            "passed": 0,
            "failed": 0,
            "assertions": 0,
            "assertions_passed": 0,
        },
        "preflight": [],
        "clients": {},
        "requests": {},
        "cases": [],
        "accepted_deviations": [
            {
                "id": "leafage.web3_clientVersion.missing",
                "behavior": "Leafage exposes pipeline version through version; standard web3_clientVersion is -32601.",
            },
            {
                "id": "leafage.reasonless_revert.empty_message",
                "behavior": "Failure code/status is compared; the already accepted empty reason string is not treated as a new mismatch.",
            },
            {
                "id": "pipeline.genesis.synthetic_id.legacy",
                "behavior": "The immutable 88aa7766 pipeline predates the accepted genesis bytes32 ID fix.",
            },
        ],
        "errors": [],
    }
    if args.plan_only:
        payload["summary"]["cases"] = len(plan.cases)
        payload["elapsed_seconds"] = round(time.monotonic() - started, 3)
        write_report(payload, args.output)
        print(
            f"Arc breadth plan: {len(plan.cases)} cases, {plan.base_vectors} base vectors, "
            f"{len(plan.cases) / BASELINE_FUNCTIONAL_CASES:.2f}x baseline"
        )
        return 0 if len(plan.cases) >= TEN_X_TARGET else 1

    leafage_url = os.environ.get("LEAFAGE_RPC", "")
    reference_url = os.environ.get("ARC_REFERENCE_RPC", "")
    if not leafage_url or not reference_url:
        payload["errors"].append("LEAFAGE_RPC and ARC_REFERENCE_RPC are required")
        payload["elapsed_seconds"] = round(time.monotonic() - started, 3)
        write_report(payload, args.output)
        return 2
    leafage = BatchRpcClient(leafage_url, args.timeout, args.retries)
    reference = BatchRpcClient(reference_url, args.timeout, args.retries)
    try:
        clients, preflight_checks, blocks, block_overrides = preflight(
            leafage,
            reference,
            plan,
            args.block,
            args.funded_address,
            args.empty_address,
        )
        payload["clients"] = clients
        payload["preflight"] = preflight_checks
        if not all(check["passed"] for check in preflight_checks):
            raise RuntimeError("preflight assertions failed")
        results = execute_plan(
            plan,
            leafage,
            reference,
            args.block,
            blocks,
            block_overrides,
        )
        payload["cases"] = [result.as_dict() for result in results]
        payload["summary"] = summarize_results(results)
        payload["complete"] = True
    except (
        core.RpcCallError,
        core.RpcTransportError,
        RuntimeError,
        TypeError,
        ValueError,
    ) as error:
        payload["errors"].append(str(error))
    payload["requests"] = {
        "leafage": {
            "logical": leafage.logical_requests,
            "http": leafage.http_requests,
            "retries": leafage.retry_count,
        },
        "reference": {
            "logical": reference.logical_requests,
            "http": reference.http_requests,
            "retries": reference.retry_count,
        },
    }
    payload["elapsed_seconds"] = round(time.monotonic() - started, 3)
    write_report(payload, args.output)
    summary = payload["summary"]
    print(
        f"Arc breadth differential: {summary['passed']}/{summary['cases']} cases passed, "
        f"{summary['failed']} failed, complete={payload['complete']}"
    )
    if not payload["complete"]:
        return 2
    return 1 if summary["failed"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
