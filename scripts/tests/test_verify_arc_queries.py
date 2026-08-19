import contextlib
import importlib.util
import io
import json
import os
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "verify_arc_queries.py"
SPEC = importlib.util.spec_from_file_location("verify_arc_queries", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class VerifyArcQueriesTests(unittest.TestCase):
    def test_parse_block_requires_explicit_positive_height(self):
        self.assertEqual(MODULE.parse_block("0x10"), 16)
        self.assertEqual(MODULE.parse_block("16"), 16)
        for value in ("latest", "pending", "0", "-1", "bad"):
            with self.assertRaises(Exception):
                MODULE.parse_block(value)

    def test_rpc_urls_are_not_accepted_as_cli_arguments(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                MODULE.parser().parse_args(
                    ["--block", "16", "--leafage-rpc", "http://example.invalid"]
                )

    def test_missing_rpc_endpoint_writes_incomplete_report(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            with mock.patch.dict(
                os.environ,
                {"LEAFAGE_RPC": "", "ARC_REFERENCE_RPC": ""},
                clear=False,
            ):
                with contextlib.redirect_stdout(io.StringIO()):
                    exit_code = MODULE.main(
                        ["--block", "10", "--output", str(output)]
                    )
            payload = json.loads(output.read_text())
        self.assertEqual(exit_code, 2)
        self.assertFalse(payload["complete"])
        self.assertEqual(payload["summary"], {"checks": 1, "passed": 0, "failed": 1})
        self.assertEqual(payload["requests"]["leafage"], 0)
        self.assertEqual(payload["requests"]["reference"], 0)
        self.assertIn("LEAFAGE_RPC", payload["errors"][0])
        self.assertNotIn("http", json.dumps(payload))

    def test_quantity_is_canonical(self):
        self.assertEqual(MODULE.quantity("0x0"), 0)
        self.assertEqual(MODULE.quantity("0x13b2"), 5042)
        for value in ("0x", "0x00", "1", -1, True):
            with self.assertRaises(ValueError):
                MODULE.quantity(value)

    def test_leafage_event_reconstructs_rpc_log(self):
        event = {
            "contract_id": MODULE.SYSTEM_ADDRESS,
            "selector": "0x" + "11" * 32,
            "topics": ["0x" + "22" * 32],
            "data": "0x1234",
        }
        self.assertEqual(
            MODULE.normalize_leafage_events([event]),
            [
                {
                    "address": MODULE.SYSTEM_ADDRESS,
                    "topics": ["0x" + "11" * 32, "0x" + "22" * 32],
                    "data": "0x1234",
                }
            ],
        )

    def test_leafage_zero_topic_event_accepts_empty_selector(self):
        event = {
            "contract_id": "0x" + "11" * 20,
            "selector": "",
            "topics": [],
            "data": "0x1234",
        }
        self.assertEqual(
            MODULE.normalize_leafage_events([event]),
            [
                {
                    "address": "0x" + "11" * 20,
                    "topics": [],
                    "data": "0x1234",
                }
            ],
        )

    def test_root_output_requires_exactly_one_root(self):
        result = {
            "traces": [
                {"parent_trace_id": "", "output": "0x01"},
                {"parent_trace_id": "root", "output": "0x02"},
            ]
        }
        self.assertEqual(MODULE.leafage_root_output(result), "0x01")
        with self.assertRaises(ValueError):
            MODULE.leafage_root_output({"traces": []})

    def test_fixtures_include_standard_and_arc_specific_paths(self):
        sender = "0x" + "aa" * 20
        recipient = "0x" + "bb" * 20
        fixtures = MODULE.build_fixtures(sender, recipient, 10)
        self.assertEqual(
            set(fixtures),
            {
                "usdc_total_supply",
                "nca_total_supply",
                "p256_valid",
                "eip2935_parent_hash",
                "sequential_native_transfer",
                "failure_then_p256",
            },
        )
        self.assertEqual(fixtures["nca_total_supply"][0]["to"], MODULE.NATIVE_COIN_AUTHORITY)
        self.assertEqual(fixtures["p256_valid"][0]["to"], MODULE.P256_PRECOMPILE)
        self.assertEqual(fixtures["eip2935_parent_hash"][0]["to"], MODULE.HISTORY_STORAGE)
        self.assertEqual(
            fixtures["eip2935_parent_hash"][0]["data"], MODULE.uint256_calldata(10)
        )
        self.assertEqual(fixtures["p256_valid"][0]["gas"], "0xf4240")
        self.assertEqual(len(bytes.fromhex(MODULE.P256_VALID_INPUT[2:])), 160)
        self.assertEqual(fixtures["sequential_native_transfer"][1]["from"], recipient)
        failed_call, followup = fixtures["failure_then_p256"]
        self.assertEqual(failed_call["from"], recipient)
        self.assertTrue(failed_call["data"].startswith(MODULE.ERC20_TRANSFER))
        self.assertEqual(followup["to"], MODULE.P256_PRECOMPILE)

    def test_report_exit_data_counts_failures(self):
        class Client:
            request_count = 2
            retry_count = 0

        report = MODULE.Report(10)
        report.add("balance", "ok", True, 1, 1)
        report.add("estimate", "bad", False, 2, 1)
        payload = report.finish(Client(), Client())
        self.assertEqual(payload["summary"], {"checks": 2, "passed": 1, "failed": 1})
        self.assertTrue(payload["complete"])

    def test_balance_does_not_pass_when_both_oracles_error(self):
        class Client:
            def capture(self, _method, _params):
                return {"ok": False, "error": {"code": -1, "message": "missing"}}

        report = MODULE.Report(10)
        MODULE.compare_balance(
            report,
            Client(),
            Client(),
            "required_selector",
            "0x" + "11" * 20,
            "latest",
        )
        self.assertFalse(report.checks[0].passed)

    def test_balance_checks_only_use_supported_selectors(self):
        class Client:
            def __init__(self):
                self.calls = []

            def call(self, method, params):
                self.calls.append((method, params))
                if method == "eth_blockNumber":
                    return "0xa"
                if method == "eth_getBlockByNumber":
                    return {"hash": "0x" + "11" * 32}
                if method == "eth_getBalance":
                    return "0x1"
                raise AssertionError(method)

            def capture(self, method, params):
                self.calls.append((method, params))
                return {"ok": True, "result": "0x1"}

        leafage = Client()
        reference = Client()
        report = MODULE.Report(10)
        MODULE.run_balance_checks(
            report,
            leafage,
            reference,
            10,
            "0x" + "11" * 32,
            "0x" + "22" * 20,
            "0x" + "33" * 20,
        )
        selectors = [
            params[1]
            for method, params in leafage.calls
            if method == "eth_getBalance"
        ]
        self.assertTrue(
            all(
                len(params) == 2
                for method, params in leafage.calls
                if method == "eth_getBalance"
            )
        )
        self.assertNotIn("pending", selectors)
        self.assertNotIn("earliest", selectors)
        self.assertNotIn("safe", selectors)
        self.assertNotIn("finalized", selectors)
        self.assertTrue(all(check.passed for check in report.checks))

    def test_estimate_rejection_requires_insufficient_funds_errors(self):
        class Client:
            def __init__(self, outcome):
                self.outcome = outcome

            def capture(self, _method, _params):
                return self.outcome

        request = {
            "from": "0x" + "11" * 20,
            "to": "0x" + "22" * 20,
            "value": "0x1",
        }
        reference = {
            "ok": False,
            "error": {"code": -38014, "message": "insufficient funds for transfer"},
        }
        leafage = {
            "ok": False,
            "error": {
                "code": MODULE.LEAFAGE_BALANCE_EXHAUSTED,
                "message": "Insufficient funds",
            },
        }
        report = MODULE.Report(10)
        MODULE.run_estimate_rejection(
            report,
            Client(leafage),
            Client(reference),
            10,
            "zero_balance",
            request,
            "balance",
        )
        self.assertTrue(report.checks[0].passed)

        unrelated = MODULE.Report(10)
        MODULE.run_estimate_rejection(
            unrelated,
            Client({"ok": False, "error": {"code": -32603, "message": "internal"}}),
            Client(reference),
            10,
            "zero_balance",
            request,
            "balance",
        )
        self.assertFalse(unrelated.checks[0].passed)

    def test_fee_only_rejection_requires_gas_allowance_class(self):
        class Client:
            def __init__(self, outcome):
                self.outcome = outcome

            def capture(self, _method, _params):
                return self.outcome

        request = {
            "from": "0x" + "11" * 20,
            "to": "0x" + "22" * 20,
            "gasPrice": "0x1",
        }
        reference = {
            "ok": False,
            "error": {
                "code": -32003,
                "message": "gas required exceeds allowance (0)",
            },
        }
        leafage = {
            "ok": False,
            "error": {
                "code": MODULE.LEAFAGE_GAS_EXHAUSTED,
                "message": "Invalid gas limit",
            },
        }
        report = MODULE.Report(10)
        MODULE.run_estimate_rejection(
            report,
            Client(leafage),
            Client(reference),
            10,
            "zero_balance_fee",
            request,
            "gas_allowance",
        )
        self.assertTrue(report.checks[0].passed)

        ordinary_oog = MODULE.Report(10)
        MODULE.run_estimate_rejection(
            ordinary_oog,
            Client(
                {
                    "ok": False,
                    "error": {
                        "code": MODULE.LEAFAGE_GAS_EXHAUSTED,
                        "message": "Halted: OutOfGas(Basic)",
                    },
                }
            ),
            Client(reference),
            10,
            "zero_balance_fee",
            request,
            "gas_allowance",
        )
        self.assertFalse(ordinary_oog.checks[0].passed)

    def test_eip7825_rejection_locks_the_consensus_cap(self):
        class Client:
            def __init__(self, outcome):
                self.outcome = outcome

            def capture(self, _method, _params):
                return self.outcome

        request = {
            "from": "0x" + "11" * 20,
            "to": "0x" + "22" * 20,
            "gas": "0x17d7840",
        }
        reference = {
            "ok": False,
            "error": {
                "code": -32000,
                "message": "gas required exceeds allowance (16777216)",
            },
        }
        leafage = {
            "ok": False,
            "error": {
                "code": MODULE.LEAFAGE_GAS_EXHAUSTED,
                "message": "Invalid gas limit",
            },
        }
        report = MODULE.Report(10)
        MODULE.run_estimate_rejection(
            report,
            Client(leafage),
            Client(reference),
            10,
            "gas_cap",
            request,
            "gas_cap",
        )
        self.assertTrue(report.checks[0].passed)

        wrong_cap = MODULE.Report(10)
        MODULE.run_estimate_rejection(
            wrong_cap,
            Client(leafage),
            Client(
                {
                    "ok": False,
                    "error": {
                        "code": -32000,
                        "message": "gas required exceeds allowance (25000000)",
                    },
                }
            ),
            10,
            "gas_cap",
            request,
            "gas_cap",
        )
        self.assertFalse(wrong_cap.checks[0].passed)

    def test_eip7825_fixture_requires_more_than_the_consensus_cap(self):
        rejected = MODULE.eip7825_gas_cap_request(
            "0x" + "11" * 20,
            "0x" + "22" * 20,
            MODULE.GAS_CAP_FAILURE_CALLDATA_BYTES,
        )
        accepted = MODULE.eip7825_gas_cap_request(
            "0x" + "11" * 20,
            "0x" + "22" * 20,
            MODULE.GAS_CAP_SUCCESS_CALLDATA_BYTES,
        )
        rejected_floor = 21_000 + len(bytes.fromhex(rejected["data"][2:])) * 40
        accepted_floor = 21_000 + len(bytes.fromhex(accepted["data"][2:])) * 40
        self.assertGreater(rejected_floor, MODULE.EIP7825_TX_GAS_LIMIT)
        self.assertLessEqual(accepted_floor, MODULE.EIP7825_TX_GAS_LIMIT)
        self.assertEqual(rejected_floor - accepted_floor, 40)
        self.assertLess(rejected_floor, int(rejected["gas"], 16))

    def test_eip7825_success_boundary_requires_the_exact_cap(self):
        class Client:
            def __init__(self, gas):
                self.gas = gas

            def capture(self, method, _params):
                if method in {"eth_estimateGas", "estimateGas"}:
                    return {"ok": True, "result": hex(self.gas)}
                if method == "eth_call":
                    return {"ok": True, "result": "0x"}
                raise AssertionError(method)

        request = {"from": "0x" + "11" * 20, "to": "0x" + "22" * 20}
        report = MODULE.Report(10)
        MODULE.run_estimate(
            report,
            Client(MODULE.EIP7825_TX_GAS_LIMIT),
            Client(MODULE.EIP7825_TX_GAS_LIMIT),
            10,
            "gas_cap_success",
            request,
            0,
            exact_reference_gas=MODULE.EIP7825_TX_GAS_LIMIT,
        )
        self.assertTrue(all(check.passed for check in report.checks))

        leafage_low = MODULE.Report(10)
        MODULE.run_estimate(
            leafage_low,
            Client(MODULE.EIP7825_TX_GAS_LIMIT - 1),
            Client(MODULE.EIP7825_TX_GAS_LIMIT),
            10,
            "gas_cap_success",
            request,
            0,
            exact_reference_gas=MODULE.EIP7825_TX_GAS_LIMIT,
        )
        gas_check = next(
            check
            for check in leafage_low.checks
            if check.name == "gas_cap_success.gas"
        )
        self.assertFalse(gas_check.passed)

        wrong_reference = MODULE.Report(10)
        MODULE.run_estimate(
            wrong_reference,
            Client(MODULE.EIP7825_TX_GAS_LIMIT - 1),
            Client(MODULE.EIP7825_TX_GAS_LIMIT - 1),
            10,
            "gas_cap_success",
            request,
            0,
            exact_reference_gas=MODULE.EIP7825_TX_GAS_LIMIT,
        )
        exact_check = next(
            check
            for check in wrong_reference.checks
            if check.name == "reference_gas_cap_success_exact_gas"
        )
        self.assertFalse(exact_check.passed)

    def test_erc20_transfer_calldata_has_two_abi_words(self):
        recipient = "0x" + "12" * 20
        calldata = MODULE.erc20_transfer_calldata(recipient, 7)
        self.assertEqual(len(bytes.fromhex(calldata[2:])), 68)
        self.assertEqual(calldata[:10], MODULE.ERC20_TRANSFER)
        self.assertEqual(calldata[10:74], "0" * 24 + recipient[2:])
        self.assertEqual(int(calldata[74:], 16), 7)

    def test_erc20_balance_of_calldata_has_one_address_word(self):
        account = "0x" + "34" * 20
        calldata = MODULE.erc20_balance_of_calldata(account)
        self.assertEqual(len(bytes.fromhex(calldata[2:])), 36)
        self.assertEqual(calldata[:10], "0x70a08231")
        self.assertEqual(calldata[10:], "0" * 24 + account[2:])

    def test_simulation_overrides_pin_next_block_context(self):
        parent_hash = "0x" + "aa" * 32
        next_block = {
            "number": "0xb",
            "parentHash": parent_hash,
            "timestamp": "0x65",
            "gasLimit": "0x1c9c380",
            "miner": "0x" + "11" * 20,
            "mixHash": "0x" + "22" * 32,
            "baseFeePerGas": "0x0",
        }
        overrides = MODULE.simulation_block_overrides(10, parent_hash, next_block)
        self.assertEqual(overrides["number"], "0xb")
        self.assertEqual(overrides["blockHash"], {"10": parent_hash})
        self.assertEqual(overrides["time"], "0x65")

    def test_wrong_simulation_height_requires_invalid_params(self):
        class Client:
            def __init__(self, outcome):
                self.outcome = outcome

            def capture(self, _method, _params):
                return self.outcome

        expected_error = {
            "ok": False,
            "error": {
                "code": MODULE.LEAFAGE_INVALID_PARAMS,
                "message": MODULE.SIMULATION_NEXT_BLOCK_ERROR,
            },
        }
        report = MODULE.Report(10)
        client = Client(expected_error)
        MODULE.run_simulation_wrong_height_rejection(
            report,
            client,
            10,
            [{}],
            {"number": "0xb"},
        )
        self.assertEqual(len(report.checks), 2)
        self.assertTrue(all(check.passed for check in report.checks))

        internal_error = MODULE.Report(10)
        MODULE.run_simulation_wrong_height_rejection(
            internal_error,
            Client({"ok": False, "error": {"code": -32603, "message": "internal"}}),
            10,
            [{}],
            {"number": "0xb"},
        )
        self.assertEqual(len(internal_error.checks), 2)
        self.assertTrue(all(not check.passed for check in internal_error.checks))

    def test_eip2935_simulation_requires_parent_hash_output(self):
        parent_hash = "0x" + "33" * 32

        class Client:
            def __init__(self, outcome):
                self.outcome = outcome

            def capture(self, _method, _params):
                return self.outcome

        reference = {
            "ok": True,
            "result": [
                {
                    "number": "0xb",
                    "timestamp": "0x65",
                    "gasLimit": "0x100000",
                    "miner": "0x" + "11" * 20,
                    "calls": [
                        {
                            "status": "0x1",
                            "returnData": parent_hash,
                            "gasUsed": "0x1",
                            "logs": [],
                        }
                    ],
                }
            ],
        }
        result = {
            "code": 0,
            "err": "",
            "gas_used": "0x1",
            "events": [],
            "traces": [
                {"parent_trace_id": "", "output": parent_hash},
            ],
        }
        leafage = {
            "ok": True,
            "result": {
                "results": [result],
                "stats": {
                    "success": True,
                    "block_num": "0xa",
                    "block_hash": parent_hash,
                    "block_time": "0x64",
                },
            },
        }
        report = MODULE.Report(10)
        MODULE.run_simulation(
            report,
            Client(leafage),
            Client(reference),
            10,
            "eip2935_parent_hash",
            [{}],
            {
                "number": "0xb",
                "time": "0x65",
                "gasLimit": "0x100000",
                "coinbase": "0x" + "11" * 20,
            },
            100,
            parent_hash,
        )
        self.assertTrue(all(check.passed for check in report.checks))

        bad_reference = json.loads(json.dumps(reference))
        bad_reference["result"][0]["calls"][0]["returnData"] = "0x" + "44" * 32
        mismatch = MODULE.Report(10)
        MODULE.run_simulation(
            mismatch,
            Client(leafage),
            Client(bad_reference),
            10,
            "eip2935_parent_hash",
            [{}],
            {
                "number": "0xb",
                "time": "0x65",
                "gasLimit": "0x100000",
                "coinbase": "0x" + "11" * 20,
            },
            100,
            parent_hash,
        )
        semantic_check = next(
            check
            for check in mismatch.checks
            if check.name == "reference_eip2935_exposes_parent_hash"
        )
        self.assertFalse(semantic_check.passed)

    def test_simulation_failure_fixture_locks_fast_stop_contract(self):
        failed = {
            "code": -39000,
            "err": "Reverted",
            "gas_used": "0x1",
            "events": [],
            "traces": [],
        }

        class Client:
            def __init__(self, outcome):
                self.outcome = outcome
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                return self.outcome

        reference = {
            "ok": True,
            "result": [
                {
                    "number": "0xb",
                    "timestamp": "0x65",
                    "gasLimit": "0x100000",
                    "miner": "0x" + "11" * 20,
                    "calls": [
                        {
                            "status": "0x0",
                            "returnData": "0x",
                            "error": {"message": "reverted"},
                            "gasUsed": "0x1",
                            "logs": [],
                        },
                        {
                            "status": "0x1",
                            "returnData": "0x" + "00" * 31 + "01",
                            "gasUsed": "0x2",
                            "logs": [],
                        },
                    ],
                }
            ],
        }
        leafage = {
            "ok": True,
            "result": {
                "results": [failed, failed.copy()],
                "stats": {
                    "success": False,
                    "block_num": "0xa",
                    "block_hash": "0x" + "33" * 32,
                    "block_time": "0x64",
                },
            },
        }
        overrides = {
            "number": "0xb",
            "time": "0x65",
            "gasLimit": "0x100000",
            "coinbase": "0x" + "11" * 20,
        }
        report = MODULE.Report(10)
        leafage_client = Client(leafage)
        reference_client = Client(reference)
        MODULE.run_simulation(
            report,
            leafage_client,
            reference_client,
            10,
            "failure_then_p256",
            [{}, {}],
            overrides,
            100,
            "0x" + "33" * 32,
        )
        self.assertTrue(all(check.passed for check in report.checks))
        method, params = reference_client.calls[0]
        self.assertEqual(method, "eth_simulateV1")
        self.assertIs(params[0]["validation"], False)

        wrong_code = failed.copy()
        wrong_code["code"] = MODULE.LEAFAGE_GAS_EXHAUSTED
        wrong_code_leafage = {
            "ok": True,
            "result": {
                "results": [wrong_code, wrong_code.copy()],
                "stats": leafage["result"]["stats"],
            },
        }
        wrong_code_report = MODULE.Report(10)
        MODULE.run_simulation(
            wrong_code_report,
            Client(wrong_code_leafage),
            Client(reference),
            10,
            "failure_then_p256",
            [{}, {}],
            overrides,
            100,
            "0x" + "33" * 32,
        )
        error_code_check = next(
            check
            for check in wrong_code_report.checks
            if check.name == "failure_then_p256.error_code"
        )
        self.assertFalse(error_code_check.passed)


if __name__ == "__main__":
    unittest.main()
