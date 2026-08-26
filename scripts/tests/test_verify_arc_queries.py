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

    def test_reference_only_summary_and_missing_endpoint_do_not_claim_leafage(self):
        payload = MODULE.Report(16)
        payload.mode = "reference-preflight"
        encoded = payload.finish(MODULE.RpcClient("", 1, 0, 0), MODULE.RpcClient("", 1, 0, 0))
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            MODULE.print_summary(encoded)
        self.assertIn("Arc writer preflight", stdout.getvalue())
        self.assertNotIn("Leafage", stdout.getvalue())

        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            with mock.patch.dict(os.environ, {}, clear=True):
                with contextlib.redirect_stdout(io.StringIO()):
                    status = MODULE.main(
                        [
                            "--reference-only",
                            "--block",
                            "16",
                            "--output",
                            str(output),
                        ]
                    )
            report = json.loads(output.read_text())
        self.assertEqual(status, 2)
        endpoint_check = report["checks"][0]
        self.assertEqual(endpoint_check["expected"], ["ARC_REFERENCE_RPC"])
        self.assertEqual(endpoint_check["actual"], ["ARC_REFERENCE_RPC"])

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

    def test_trace_normalization_compares_tree_paths_not_random_ids(self):
        sender = "0x" + "11" * 20
        proxy = "0x" + "22" * 20
        implementation = "0x" + "33" * 20
        reference = [
            {
                "type": "call",
                "action": {
                    "from": sender,
                    "to": proxy,
                    "callType": "call",
                    "input": "0x1234",
                    "value": "0x0",
                },
                "result": {"gasUsed": "0x20", "output": "0xab"},
                "traceAddress": [],
            },
            {
                "type": "call",
                "action": {
                    "from": proxy,
                    "to": implementation,
                    "callType": "delegatecall",
                    "input": "0x1234",
                    "value": "0x0",
                },
                "result": {"gasUsed": "0x10", "output": "0xab"},
                "traceAddress": [0],
            },
        ]
        leafage = [
            {
                "id": "random-root",
                "parent_trace_id": "",
                "pos_in_parent_trace": 0,
                "type": "call",
                "call_type": "call",
                "from_addr": sender,
                "to_addr": proxy,
                "input": "0x1234",
                "output": "0xab",
                "value": "0x0",
                "gas_used": "0x20",
            },
            {
                "id": "random-child",
                "parent_trace_id": "random-root",
                # Event positions share this counter. The call is still the
                # first child trace even when an event occupied position 0.
                "pos_in_parent_trace": 1,
                "type": "call",
                "call_type": "delegatecall",
                "from_addr": proxy,
                "to_addr": implementation,
                "input": "0x1234",
                "output": "0xab",
                "value": "0x0",
                "gas_used": "0x10",
            },
        ]
        self.assertEqual(
            MODULE.normalize_leafage_traces(leafage),
            MODULE.normalize_reference_traces(reference),
        )

    def test_trace_normalization_rejects_ambiguous_or_orphaned_trees(self):
        root = {
            "id": "root",
            "parent_trace_id": "",
            "pos_in_parent_trace": 0,
            "type": "call",
            "from_addr": "0x" + "11" * 20,
            "to_addr": "0x" + "22" * 20,
        }
        child = {
            **root,
            "id": "child-a",
            "parent_trace_id": "root",
            "pos_in_parent_trace": 1,
        }
        duplicate_position = {**child, "id": "child-b"}
        with self.assertRaisesRegex(ValueError, "duplicate Leafage trace position"):
            MODULE.normalize_leafage_traces([root, child, duplicate_position])

        orphan = {
            "type": "call",
            "action": {
                "from": "0x" + "11" * 20,
                "to": "0x" + "22" * 20,
            },
            "result": {},
            "traceAddress": [0],
        }
        with self.assertRaisesRegex(ValueError, "exactly one reference root"):
            MODULE.normalize_reference_traces([orphan])

    def test_trace_normalization_folds_create2_and_selfdestruct_gas(self):
        sender = "0x" + "11" * 20
        target = "0x" + "22" * 20
        child = "0x" + "33" * 20
        beneficiary = "0x" + "44" * 20
        reference = [
            {
                "type": "call",
                "action": {
                    "from": sender,
                    "to": target,
                    "callType": "call",
                    "input": "0x",
                    "value": "0x0",
                    "gas": "0x100",
                },
                "result": {"gasUsed": "0x20", "output": "0x"},
                "traceAddress": [],
            },
            {
                "type": "create",
                "action": {
                    "from": target,
                    "init": "0x00",
                    "creationMethod": "create2",
                    "value": "0x1",
                    "gas": "0x80",
                },
                "result": {"address": child, "gasUsed": "0x10", "code": "0x"},
                "traceAddress": [0],
            },
            {
                "type": "suicide",
                "action": {
                    "address": target,
                    "refundAddress": beneficiary,
                    "balance": "0x1",
                },
                "traceAddress": [1],
            },
        ]
        leafage = [
            {
                "id": "root",
                "parent_trace_id": "",
                "pos_in_parent_trace": 0,
                "type": "call",
                "call_type": "call",
                "from_addr": sender,
                "to_addr": target,
                "input": "0x",
                "output": "0x",
                "value": "0x0",
                "gas_limit": "0x100",
                "gas_used": "0x20",
            },
            {
                "id": "create",
                "parent_trace_id": "root",
                "pos_in_parent_trace": 0,
                "type": "create",
                "call_type": "",
                "from_addr": target,
                "to_addr": child,
                "input": "0x00",
                "output": "0x",
                "value": "0x1",
                "gas_limit": "0x80",
                "gas_used": "0x10",
            },
            {
                "id": "suicide",
                "parent_trace_id": "root",
                "pos_in_parent_trace": 1,
                "type": "suicide",
                "from_addr": target,
                "to_addr": beneficiary,
                "input": "0x",
                "output": "0x",
                "value": "0x1",
                "gas_limit": 0,
                "gas_used": 0,
            },
        ]
        self.assertEqual(
            MODULE.normalize_leafage_traces(leafage),
            MODULE.normalize_reference_traces(reference),
        )

    def test_event_attachments_reject_unknown_or_duplicate_members(self):
        tx_id = "0x" + "77" * 32
        traces = [
            {
                "id": "root",
                "tx_id": tx_id,
                "parent_trace_id": "",
                "pos_in_parent_trace": 0,
            },
            {
                "id": "child",
                "tx_id": tx_id,
                "parent_trace_id": "root",
                "pos_in_parent_trace": 1,
            },
        ]
        event = {
            "id": "event",
            "tx_id": tx_id,
            "parent_trace_id": "root",
            "pos_in_parent_trace": 0,
        }
        self.assertEqual(
            MODULE.leafage_event_attachments(traces, [event]),
            [{"parent_trace_id": "root", "pos_in_parent_trace": 0}],
        )
        with self.assertRaisesRegex(ValueError, "duplicate Leafage member position"):
            MODULE.leafage_event_attachments(
                traces,
                [
                    {
                        "id": "event",
                        "tx_id": tx_id,
                        "parent_trace_id": "root",
                        "pos_in_parent_trace": 1,
                    }
                ],
            )
        with self.assertRaisesRegex(ValueError, "unknown Leafage event parent"):
            MODULE.leafage_event_attachments(
                traces,
                [
                    {
                        "id": "event",
                        "tx_id": tx_id,
                        "parent_trace_id": "missing",
                        "pos_in_parent_trace": 0,
                    }
                ],
            )
        with self.assertRaisesRegex(ValueError, "transaction ids differ"):
            MODULE.leafage_event_attachments(
                traces,
                [{**event, "tx_id": "0x" + "88" * 32}],
            )
        with self.assertRaisesRegex(ValueError, "event id must be non-empty"):
            MODULE.leafage_event_attachments(
                traces,
                [{**event, "id": ""}],
            )
        with self.assertRaisesRegex(ValueError, "non-contiguous Leafage member"):
            MODULE.leafage_event_attachments(
                [
                    traces[0],
                    {**traces[1], "pos_in_parent_trace": 999},
                ],
                [],
            )
        with self.assertRaisesRegex(ValueError, "root trace position must be zero"):
            MODULE.leafage_event_attachments(
                [{**traces[0], "pos_in_parent_trace": 999}], []
            )

    def test_contract_multicall_matches_independent_writer_calls(self):
        sender = "0x" + "11" * 20
        first = MODULE.call_request(sender, "0x" + "22" * 20, "0x1234")
        second = MODULE.call_request(sender, "0x" + "33" * 20, "0xabcd")

        class Reference:
            def __init__(self):
                self.eth_call = iter(("0xaa", "0xbb"))
                self.debug_call = iter((0x31, 0x42))
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                if method == "eth_call":
                    return {"ok": True, "result": next(self.eth_call)}
                if method == "debug_traceCall":
                    return {
                        "ok": True,
                        "result": {"gasUsed": hex(next(self.debug_call))},
                    }
                raise AssertionError(method)

        class Leafage:
            def __init__(self):
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                return {
                    "ok": True,
                    "result": {
                        "results": [
                            {
                                "code": 0,
                                "err": "",
                                "result": "0xaa",
                                "gas_used": 0x31,
                                "from_cache": False,
                            },
                            {
                                "code": 0,
                                "err": "",
                                "result": "0xbb",
                                "gas_used": 0x42,
                                "from_cache": False,
                            },
                        ],
                        "stats": {
                            "block_num": 10,
                            "block_hash": "0x" + "44" * 32,
                            "block_time": 100,
                            "success": True,
                            "cache_enabled": False,
                        },
                    },
                }

        report = MODULE.Report(10)
        leafage = Leafage()
        reference = Reference()
        state_override = {"0x" + "55" * 20: {"code": "0x6000"}}
        block_overrides = {"number": "0xb", "time": "0x65"}
        MODULE.run_contract_multicall(
            report,
            leafage,
            reference,
            10,
            "0x" + "44" * 32,
            100,
            "read_batch",
            [first, second],
            state_override=state_override,
            block_overrides=block_overrides,
            use_parallel=True,
            use_hash_context=True,
        )
        self.assertTrue(all(check.passed for check in report.checks))
        self.assertEqual(leafage.calls[0][0], "contractMultiCall")
        self.assertEqual(
            [method for method, _params in reference.calls],
            ["eth_call", "debug_traceCall", "eth_call", "debug_traceCall"],
        )
        self.assertEqual(
            reference.calls[0][1][1],
            {"blockHash": "0x" + "44" * 32, "requireCanonical": True},
        )
        self.assertEqual(reference.calls[0][1][2:], [state_override, block_overrides])
        self.assertEqual(
            reference.calls[1][1][2],
            {
                "tracer": "callTracer",
                "stateOverrides": state_override,
                "blockOverrides": block_overrides,
            },
        )
        self.assertEqual(leafage.calls[0][1][2:4], [block_overrides, state_override])
        self.assertEqual(
            leafage.calls[0][1][1],
            {"block_id": "0x" + "44" * 32, "type": "Equals"},
        )
        self.assertIs(leafage.calls[0][1][5], True)

    def test_contract_multicall_continues_after_revert(self):
        calls = [{"to": "0x" + "11" * 20}, {"to": "0x" + "22" * 20}]

        class Reference:
            def __init__(self):
                self.outcomes = iter(
                    (
                        {
                            "ok": False,
                            "error": {
                                "code": 3,
                                "message": "execution reverted: ERC20: transfer amount exceeds balance",
                            },
                        },
                        {"ok": True, "result": {"gasUsed": "0x30"}},
                        {"ok": True, "result": "0xaa"},
                        {"ok": True, "result": {"gasUsed": "0x40"}},
                    )
                )

            def capture(self, _method, _params):
                return next(self.outcomes)

        class Leafage:
            def capture(self, _method, params):
                self.params = params
                return {
                    "ok": True,
                    "result": {
                        "results": [
                            {
                                "code": -39000,
                                "err": "revert: ERC20: transfer amount exceeds balance",
                                "result": "0x",
                                "gas_used": 0x30,
                                "from_cache": False,
                            },
                            {
                                "code": 0,
                                "err": "",
                                "result": "0xaa",
                                "gas_used": 0x40,
                                "from_cache": False,
                            },
                        ],
                        "stats": {
                            "block_num": 10,
                            "block_hash": "0x" + "44" * 32,
                            "block_time": 100,
                            "success": False,
                            "cache_enabled": False,
                        },
                    },
                }

        report = MODULE.Report(10)
        leafage = Leafage()
        MODULE.run_contract_multicall(
            report,
            leafage,
            Reference(),
            10,
            "0x" + "44" * 32,
            100,
            "revert_then_success",
            calls,
        )
        self.assertTrue(all(check.passed for check in report.checks))
        self.assertIs(leafage.params[4], False)

    def test_contract_multicall_fast_fail_clones_first_failure(self):
        calls = [{"to": "0x" + "11" * 20}, {"to": "0x" + "22" * 20}]

        class Reference:
            def __init__(self):
                self.outcomes = iter(
                    (
                        {
                            "ok": False,
                            "error": {
                                "code": 3,
                                "message": "execution reverted: ERC20: transfer amount exceeds balance",
                            },
                        },
                        {"ok": True, "result": {"gasUsed": "0x30"}},
                        {"ok": True, "result": "0xaa"},
                        {"ok": True, "result": {"gasUsed": "0x40"}},
                    )
                )

            def capture(self, _method, _params):
                return next(self.outcomes)

        failed = {
            "code": -39000,
            "err": "revert: ERC20: transfer amount exceeds balance",
            "result": "0x",
            "gas_used": 0x30,
            "from_cache": False,
        }

        class Leafage:
            def capture(self, _method, params):
                self.params = params
                return {
                    "ok": True,
                    "result": {
                        "results": [failed, failed.copy()],
                        "stats": {
                            "block_num": 10,
                            "block_hash": "0x" + "44" * 32,
                            "block_time": 100,
                            "success": False,
                            "cache_enabled": False,
                        },
                    },
                }

        report = MODULE.Report(10)
        leafage = Leafage()
        MODULE.run_contract_multicall(
            report,
            leafage,
            Reference(),
            10,
            "0x" + "44" * 32,
            100,
            "fast_fail_revert",
            calls,
            fast_fail=True,
        )
        self.assertTrue(all(check.passed for check in report.checks))
        self.assertIs(leafage.params[4], True)
        self.assertTrue(
            next(
                check
                for check in report.checks
                if check.name == "fast_fail_revert.fast_fail_clones_failure"
            ).passed
        )

    def test_contract_multicall_boundary_batches_cover_empty_and_above_32(self):
        sender = "0x" + "11" * 20
        batches = MODULE.build_contract_multicall_boundary_batches(sender)
        self.assertEqual(batches["empty"], [])
        self.assertEqual(len(batches["above_32"]), 33)
        self.assertEqual(batches["explicit_nonce"][0]["nonce"], "0x1")
        self.assertTrue(
            all(
                request["to"] == MODULE.STANDARD_PRECOMPILE_VECTORS["identity_deadbeef"][0]
                and request["data"]
                == MODULE.STANDARD_PRECOMPILE_VECTORS["identity_deadbeef"][1]
                for request in batches["above_32"]
            )
        )
        override_calls, state_override = MODULE.build_contract_multicall_override_fixture(
            sender, "0x" + "22" * 20, 10
        )
        self.assertEqual(len(override_calls), 6)
        self.assertNotIn("to", override_calls[0])
        self.assertEqual(override_calls[0]["gasPrice"], "0x7c")
        self.assertNotIn("to", override_calls[1])
        self.assertEqual(override_calls[1]["gasPrice"], "0x0")
        self.assertEqual(
            override_calls[2],
            MODULE.blockhash_probe_request(sender, 9),
        )
        self.assertEqual(override_calls[3], override_calls[4])
        self.assertNotIn("to", override_calls[5])
        self.assertEqual(
            override_calls[5]["data"],
            "0x73" + "22" * 20 + "3f60005260206000f3",
        )
        self.assertEqual(
            state_override["0x" + "22" * 20]["code"],
            MODULE.STATE_OVERRIDE_COUNTER_CODE,
        )
        self.assertEqual(
            state_override["0x" + "22" * 20]["state"],
            {"0x" + "00" * 32: MODULE.abi_words(7)},
        )

    def test_simulation_trace_uses_writer_h_plus_one_parent_state(self):
        sender = "0x" + "11" * 20
        target = "0x" + "22" * 20
        call = MODULE.call_request(sender, target, "0x1234", gas=100_000)
        reference_trace = {
            "type": "call",
            "action": {
                "from": sender,
                "to": target,
                "callType": "call",
                "gas": "0x100",
                "input": "0x1234",
                "value": "0x0",
            },
            "result": {"gasUsed": "0x20", "output": "0xab"},
            "traceAddress": [],
        }
        leafage_trace = {
            "id": "random",
            "parent_trace_id": "",
            "pos_in_parent_trace": 0,
            "type": "call",
            "call_type": "call",
            "from_addr": sender,
            "to_addr": target,
            "gas_limit": "0x100",
            "gas_used": "0x20",
            "input": "0x1234",
            "output": "0xab",
            "value": "0x0",
        }

        class Reference:
            def __init__(self):
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                return {
                    "ok": True,
                    "result": [
                        {"trace": [reference_trace], "logs": [], "gasUsed": 0x5228}
                    ],
                }

        report = MODULE.Report(10)
        reference = Reference()
        MODULE.compare_simulation_traces(
            report,
            reference,
            10,
            "simple_call",
            [call],
            [{"code": 0, "gas_used": 0x5228, "traces": [leafage_trace]}],
        )
        self.assertTrue(all(check.passed for check in report.checks))
        self.assertEqual(reference.calls, [("pre_traceMany", [[call], "0xb", None, None])])

    def test_estimate_comparison_is_exact_by_default(self):
        self.assertEqual(MODULE.parser().parse_args(["--block", "10"]).gas_tolerance_bps, 0)

    def test_writer_override_estimate_reproduces_arc_search(self):
        overrides = {
            "number": "0xb",
            "time": "0x76",
            "gasLimit": "0x17d7840",
            "coinbase": "0x" + "11" * 20,
            "random": "0x" + "22" * 32,
            "baseFee": "0x7b",
            "blockHash": {"10": "0x" + "33" * 32},
        }
        request = MODULE.environment_guard_request("0x" + "44" * 20, 10, overrides)

        class Reference:
            def __init__(self):
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                if method == "debug_traceCall":
                    return {"ok": True, "result": {"gasUsed": hex(53_318)}}
                if method == "eth_call":
                    gas = int(params[0]["gas"], 16)
                    if gas >= 53_318:
                        return {"ok": True, "result": "0x"}
                    return {
                        "ok": False,
                        "error": {"code": -32003, "message": "out of gas"},
                    }
                raise AssertionError(method)

        reference = Reference()
        self.assertEqual(
            MODULE.reference_estimate_with_block_overrides(
                reference, request, 10, overrides
            ),
            54_112,
        )
        self.assertTrue(
            all(
                params[-1] == overrides
                if method == "eth_call"
                else params[-1]["blockOverrides"] == overrides
                for method, params in reference.calls
            )
        )

        class InvalidReference(Reference):
            def capture(self, method, params):
                if method == "debug_traceCall":
                    return super().capture(method, params)
                if method == "eth_call" and int(params[0]["gas"], 16) < 100_000:
                    return {
                        "ok": False,
                        "error": {"code": -32602, "message": "invalid params"},
                    }
                return super().capture(method, params)

        with self.assertRaisesRegex(ValueError, "optimistic estimate probe"):
            MODULE.reference_estimate_with_block_overrides(
                InvalidReference(), request, 10, overrides
            )

    def test_world_state_compares_standard_and_debank_reads(self):
        empty = "0x" + "00" * 19 + "01"
        anchor_hash = "0x" + "44" * 32
        code = "0x6000"
        nonzero_word = "0x" + "00" * 31 + "01"
        zero_word = "0x" + "00" * 32

        class Reference:
            def capture(self, method, params):
                account = params[0]
                if method == "eth_getTransactionCount":
                    return {"ok": True, "result": "0x1" if account == MODULE.USDC else "0x0"}
                if method == "eth_getCode":
                    return {"ok": True, "result": code if account == MODULE.USDC else "0x"}
                if method == "eth_getStorageAt":
                    slot = int(params[1], 16)
                    value = nonzero_word if (account, slot) in {
                        (MODULE.USDC, 0),
                        (MODULE.NATIVE_COIN_AUTHORITY, 2),
                    } else zero_word
                    return {"ok": True, "result": value}
                raise AssertionError(method)

        class Leafage(Reference):
            def __init__(self):
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                standard = {
                    "getAddressNonce": "eth_getTransactionCount",
                    "getAddressCode": "eth_getCode",
                    "getStorageAt": "eth_getStorageAt",
                }.get(method, method)
                if standard == "eth_getStorageAt" and method == "getStorageAt":
                    params = [params[0], params[1], "0xa"]
                elif method in {"getAddressNonce", "getAddressCode"}:
                    params = [params[0], "0xa"]
                return super().capture(standard, params)

        report = MODULE.Report(10)
        leafage = Leafage()
        MODULE.run_world_state_checks(
            report, leafage, Reference(), 10, anchor_hash, empty
        )
        self.assertTrue(report.checks)
        self.assertTrue(all(check.passed for check in report.checks))
        self.assertIn(
            (
                "getAddressNonce",
                [MODULE.USDC, {"block_id": anchor_hash, "type": "Equals"}],
            ),
            leafage.calls,
        )
        self.assertIn(
            (
                "getStorageAt",
                [MODULE.USDC, "0x0", {"block_id": anchor_hash, "type": "Equals"}],
            ),
            leafage.calls,
        )

    def test_native_sentinel_uses_writer_multicall_not_eth_call(self):
        sender = "0x" + "11" * 20
        outputs = [MODULE.abi_words(7), MODULE.abi_words(1)]

        class Client:
            def __init__(self, writer):
                self.writer = writer
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                expected_method = "eth_multiCall" if self.writer else "contractMultiCall"
                self.assert_method(method, expected_method)
                result_keys = ("gasUsed", "timeCost") if self.writer else ("gas_used", "time_cost")
                results = []
                for output in outputs:
                    results.append(
                        {
                            "code": 0,
                            "err": "",
                            "result": output,
                            result_keys[0]: 0,
                            result_keys[1]: 0.0,
                            "fromCache" if self.writer else "from_cache": False,
                        }
                    )
                stats = (
                    {
                        "blockNum": 10,
                        "blockHash": "0x" + "44" * 32,
                        "blockTime": 100,
                        "success": True,
                        "cacheEnabled": False,
                    }
                    if self.writer
                    else {
                        "block_num": 10,
                        "block_hash": "0x" + "44" * 32,
                        "block_time": 100,
                        "success": True,
                        "cache_enabled": False,
                    }
                )
                return {"ok": True, "result": {"results": results, "stats": stats}}

            @staticmethod
            def assert_method(actual, expected):
                if actual != expected:
                    raise AssertionError(actual)

        report = MODULE.Report(10)
        leafage = Client(False)
        writer = Client(True)
        MODULE.run_native_sentinel_multicall(
            report, leafage, writer, 10, "0x" + "44" * 32, 100, sender, limit=2
        )
        self.assertTrue(all(check.passed for check in report.checks))
        self.assertEqual(writer.calls[0][0], "eth_multiCall")
        self.assertEqual(leafage.calls[0][0], "contractMultiCall")

    def test_fixtures_include_standard_and_arc_specific_paths(self):
        sender = "0x" + "aa" * 20
        recipient = "0x" + "bb" * 20
        fixtures = MODULE.build_fixtures(sender, recipient, 10)
        self.assertEqual(
            set(fixtures),
            {
                "usdc_total_supply",
                "nca_total_supply",
                "ncc_usdc_is_blocklisted",
                "ncc_funded_is_not_blocklisted",
                "system_accounting_at_anchor",
                "protocol_fee_params",
                "protocol_consensus_params",
                "denylist_usdc",
                "active_validator_count",
                "permit2_domain_separator",
                "sha256_abc",
                "ripemd160_abc",
                "identity_deadbeef",
                "p256_valid",
                "p256_invalid_wrong_hash",
                "pq_valid",
                "pq_invalid_signature",
                "eip2935_parent_hash",
                "sequential_native_transfer",
                "failure_then_p256",
                "eip2930_access_list_identity",
                "eip1559_identity",
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
        self.assertEqual(len(bytes.fromhex(MODULE.P256_INVALID_WRONG_HASH_INPUT[2:])), 160)
        self.assertNotEqual(MODULE.P256_INVALID_WRONG_HASH_INPUT, MODULE.P256_VALID_INPUT)
        self.assertEqual(
            MODULE.expected_fixture_output("p256_invalid_wrong_hash", 10), "0x"
        )
        self.assertEqual(fixtures["ncc_usdc_is_blocklisted"][0]["to"], MODULE.NATIVE_COIN_CONTROL)
        self.assertEqual(fixtures["pq_valid"][0]["to"], MODULE.PQ_PRECOMPILE)
        self.assertEqual(len(bytes.fromhex(fixtures["pq_valid"][0]["data"][2:])), 8_132)
        self.assertEqual(
            len(bytes.fromhex(fixtures["pq_invalid_signature"][0]["data"][2:])),
            8_132,
        )
        self.assertNotEqual(
            fixtures["pq_invalid_signature"][0]["data"],
            fixtures["pq_valid"][0]["data"],
        )
        self.assertEqual(
            MODULE.expected_fixture_output("pq_invalid_signature", 10),
            MODULE.abi_words(0),
        )
        pq_fixture = json.loads(
            (SCRIPT.with_name("fixtures") / "arc-pq-valid.json").read_text()
        )
        self.assertEqual(
            pq_fixture["source_file_sha256"], MODULE.PQ_SOURCE_FILE_SHA256
        )
        for name, (target, calldata, _expected) in MODULE.STANDARD_PRECOMPILE_VECTORS.items():
            self.assertEqual(fixtures[name][0]["to"], target)
            self.assertEqual(fixtures[name][0]["data"], calldata)
        sequential = fixtures["sequential_native_transfer"]
        self.assertEqual(len(sequential), 3)
        self.assertNotIn("to", sequential[1])
        self.assertEqual(
            sequential[1]["data"],
            "0x73" + recipient[2:] + "3160005260206000f3",
        )
        self.assertEqual(sequential[2]["from"], recipient)
        failed_call, followup = fixtures["failure_then_p256"]
        self.assertEqual(failed_call["from"], recipient)
        self.assertTrue(failed_call["data"].startswith(MODULE.ERC20_TRANSFER))
        self.assertEqual(followup["to"], MODULE.P256_PRECOMPILE)
        access_list = fixtures["eip2930_access_list_identity"][0]
        self.assertEqual(
            access_list["accessList"],
            [{"address": access_list["to"], "storageKeys": []}],
        )
        self.assertEqual(
            MODULE.expected_fixture_output("eip2930_access_list_identity", 10),
            MODULE.STANDARD_PRECOMPILE_VECTORS["identity_deadbeef"][2],
        )
        eip1559 = fixtures["eip1559_identity"][0]
        self.assertNotIn("gasPrice", eip1559)
        self.assertEqual(eip1559["maxFeePerGas"], hex(MODULE.EIP1559_MAX_FEE))
        self.assertEqual(eip1559["maxPriorityFeePerGas"], "0x7")
        baseline_eip1559 = MODULE.build_fixtures(
            sender, recipient, MODULE.ARC_MAINNET_BASELINE_BLOCK
        )["eip1559_identity"][0]
        self.assertNotIn("to", baseline_eip1559)
        self.assertEqual(
            baseline_eip1559["data"],
            MODULE.EIP1559_EFFECTIVE_GASPRICE_GUARD_INIT,
        )
        self.assertEqual(
            MODULE.expected_fixture_output(
                "eip1559_identity", MODULE.ARC_MAINNET_BASELINE_BLOCK
            ),
            "0x",
        )
        protocol_fee_params = MODULE.expected_fixture_output(
            "protocol_fee_params", MODULE.ARC_MAINNET_BASELINE_BLOCK
        )
        self.assertEqual(
            [
                int(protocol_fee_params[index : index + 64], 16)
                for index in range(2, len(protocol_fee_params), 64)
            ],
            [20, 200, 5_000, 20_000_000_000, 20_000_000_000_000, 30_000_000],
        )

    def test_stateful_simulation_fixtures_lock_ordered_arc_behaviour(self):
        sender = MODULE.ARC_MAINNET_BASELINE_FUNDED
        beneficiary = "0x" + "00" * 19 + "be"
        created = MODULE.ARC_MAINNET_BASELINE_CREATED
        fixtures = MODULE.build_stateful_simulation_fixtures(
            sender,
            beneficiary,
            created,
            MODULE.ARC_MAINNET_BASELINE_BLOCK,
            "0x" + "cc" * 20,
        )
        self.assertEqual(
            set(fixtures),
            {
                "create_then_call",
                "sstore_sequence",
                "fee_then_balance",
                "create_then_internal_create2",
                "selfdestruct_eip6780",
                "log_then_revert",
                "failed_create_log_revert",
                "nested_blocklist_revert",
                "eip7702_delegation_then_call",
            },
        )
        self.assertEqual(
            fixtures["create_then_call"][0]["data"],
            "0x600a80600b6000396000f3" + MODULE.CONSTANT_42_RUNTIME[2:],
        )
        self.assertEqual(fixtures["create_then_call"][1]["to"], created)
        self.assertEqual(
            MODULE.expected_stateful_outputs(
                "sstore_sequence", MODULE.ARC_MAINNET_BASELINE_BLOCK, created
            ),
            [MODULE.STATE_OVERRIDE_COUNTER_CODE, MODULE.abi_words(0), MODULE.abi_words(1)],
        )
        self.assertEqual(
            MODULE.expected_stateful_outputs(
                "create_then_internal_create2",
                MODULE.ARC_MAINNET_BASELINE_BLOCK,
                created,
            )[1],
            MODULE.abi_words(int(MODULE.ARC_MAINNET_BASELINE_CREATE2_CHILD, 16)),
        )
        self.assertEqual(
            MODULE.expected_stateful_outputs(
                "selfdestruct_eip6780",
                MODULE.ARC_MAINNET_BASELINE_BLOCK,
                created,
            )[2],
            MODULE.abi_words(7, int(MODULE.SELFDESTRUCT_RUNTIME_CODE_HASH, 16)),
        )
        authorization = fixtures["eip7702_delegation_then_call"][1]
        self.assertEqual(authorization["to"], MODULE.EIP7702_AUTHORITY)
        self.assertEqual(
            authorization["authorizationList"], [MODULE.EIP7702_AUTHORIZATION]
        )
        self.assertEqual(
            MODULE.expected_stateful_outputs(
                "nested_blocklist_revert",
                MODULE.ARC_MAINNET_BASELINE_BLOCK,
                created,
            ),
            [MODULE.NESTED_BLOCKLIST_OUTPUT],
        )

    def test_stateful_event_layout_locks_create2_and_selfdestruct_positions(self):
        def result(child_type, child_position, event_parent, event_position):
            return {
                "traces": [
                    {
                        "id": "root",
                        "parent_trace_id": "",
                        "pos_in_parent_trace": 0,
                        "type": "call" if child_type else "create",
                        "call_type": "call" if child_type else "",
                    },
                    *(
                        [
                            {
                                "id": "child",
                                "parent_trace_id": "root",
                                "pos_in_parent_trace": child_position,
                                "type": child_type,
                                "call_type": "",
                            }
                        ]
                        if child_type
                        else []
                    ),
                ],
                "events": [
                    {
                        "parent_trace_id": event_parent,
                        "pos_in_parent_trace": event_position,
                    }
                ],
            }

        create_result = result(None, 0, "root", 0)
        create2_result = result("create", 0, "child", 0)
        self.assertEqual(
            MODULE.stateful_event_layout(
                "create_then_internal_create2", [create_result, create2_result]
            ),
            MODULE.expected_stateful_event_layout("create_then_internal_create2"),
        )
        selfdestruct_result = result("suicide", 1, "root", 0)
        self.assertEqual(
            MODULE.stateful_event_layout(
                "selfdestruct_eip6780", [create_result, selfdestruct_result, {}]
            ),
            MODULE.expected_stateful_event_layout("selfdestruct_eip6780"),
        )

    def test_asset_corpus_includes_arc_usdc_and_active_mainnet_tokens(self):
        sender = "0x" + "aa" * 20
        assets = MODULE.build_asset_read_fixtures(sender)
        names = [name for name, _call in assets]
        self.assertEqual(len(assets), 21)
        self.assertIn("usdc.allowance_permit2", names)
        self.assertIn("aworp.symbol", names)
        self.assertIn("ausd.balance", names)
        self.assertIn("agbp.total_supply", names)
        aworp_symbol = MODULE.expected_asset_outputs(MODULE.ARC_MAINNET_BASELINE_BLOCK)[
            "aworp.symbol"
        ]
        self.assertEqual(aworp_symbol, MODULE.abi_string("AWORP"))

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
        self.assertIn(
            {
                "block_id": "0x" + "11" * 32,
                "type": "Equals",
            },
            [
                params[1]
                for method, params in leafage.calls
                if method == "getAddressBalance"
            ],
        )
        self.assertTrue(all(check.passed for check in report.checks))

    def test_usdc_balance_relation_uses_numeric_and_hash_selectors(self):
        account = "0x" + "22" * 20
        anchor_hash = "0x" + "33" * 32

        class Client:
            def __init__(self):
                self.calls = []

            def call(self, method, params):
                self.calls.append((method, params))
                if method != "eth_getBalance":
                    raise AssertionError(method)
                return hex(5 * 10**12 + 7)

            def capture(self, method, params):
                self.calls.append((method, params))
                if method != "eth_call":
                    raise AssertionError(method)
                return {"ok": True, "result": MODULE.abi_words(5)}

        leafage = Client()
        reference = Client()
        report = MODULE.Report(10)
        MODULE.run_native_usdc_balance_relation(
            report, leafage, reference, 10, anchor_hash, account
        )
        self.assertTrue(all(check.passed for check in report.checks))
        hash_selector = {"blockHash": anchor_hash, "requireCanonical": True}
        self.assertIn(
            ("eth_call", [mock.ANY, hash_selector]), reference.calls
        )
        self.assertIn(
            ("eth_call", [mock.ANY, hash_selector, None, None]), leafage.calls
        )

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

    def test_estimate_revert_requires_execution_revert_errors(self):
        class Client:
            def __init__(self, outcome):
                self.outcome = outcome

            def capture(self, _method, _params):
                return self.outcome

        request = {
            "from": "0x" + "11" * 20,
            "to": MODULE.USDC,
            "data": MODULE.erc20_transfer_calldata("0x" + "22" * 20, 1),
        }
        reference = {
            "ok": False,
            "error": {
                "code": 3,
                "message": "execution reverted: ERC20: transfer amount exceeds balance",
            },
        }
        leafage = {
            "ok": False,
            "error": {
                "code": MODULE.LEAFAGE_EVM_REVERT,
                "message": "execution reverted: ERC20: transfer amount exceeds balance",
            },
        }
        report = MODULE.Report(10)
        MODULE.run_estimate_rejection(
            report,
            Client(leafage),
            Client(reference),
            10,
            "contract_revert",
            request,
            "revert",
        )
        self.assertTrue(report.checks[0].passed)

        unrelated = MODULE.Report(10)
        MODULE.run_estimate_rejection(
            unrelated,
            Client(
                {
                    "ok": False,
                    "error": {
                        "code": MODULE.LEAFAGE_GAS_EXHAUSTED,
                        "message": "Invalid gas limit",
                    },
                }
            ),
            Client(reference),
            10,
            "contract_revert",
            request,
            "revert",
        )
        self.assertFalse(unrelated.checks[0].passed)

    def test_estimate_preparation_errors_keep_endpoint_specific_contracts(self):
        class Client:
            def __init__(self, outcome):
                self.outcome = outcome

            def capture(self, _method, _params):
                return self.outcome

        request = {
            "from": "0x" + "11" * 20,
            "to": "0x" + "22" * 20,
        }
        cases = (
            (
                "fee_conflict",
                {
                    "ok": False,
                    "error": {
                        "code": -32602,
                        "message": "both gasPrice and (maxFeePerGas or "
                        "maxPriorityFeePerGas) specified",
                    },
                },
                {
                    "ok": False,
                    "error": {"code": -32602, "message": "Invalid fee parameters"},
                },
            ),
            (
                "authorization",
                {
                    "ok": False,
                    "error": {
                        "code": -32003,
                        "message": "EIP-7702 authorization list has invalid fields",
                    },
                },
                {
                    "ok": False,
                    "error": {
                        "code": MODULE.LEAFAGE_EVM_FAILED,
                        "message": "authorization list has invalid fields",
                    },
                },
            ),
        )
        for error_class, reference, leafage in cases:
            with self.subTest(error_class=error_class):
                report = MODULE.Report(10)
                MODULE.run_estimate_rejection(
                    report,
                    Client(leafage),
                    Client(reference),
                    10,
                    error_class,
                    request,
                    error_class,
                )
                self.assertTrue(report.checks[0].passed)

    def test_blocked_sender_and_receiver_reject_all_three_execution_apis(self):
        blocked_writer = {
            "ok": False,
            "error": {"code": -32603, "message": "Blocked address"},
        }
        blocked_leafage = {
            "ok": False,
            "error": {
                "code": MODULE.LEAFAGE_EVM_FAILED,
                "message": "Blocked address",
            },
        }

        class Client:
            def __init__(self, result):
                self.result = result
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                return self.result

        writer = Client(blocked_writer)
        leafage = Client(blocked_leafage)
        report = MODULE.Report(10)
        MODULE.run_blocked_execution_rejections(
            report,
            leafage,
            writer,
            10,
            "0x" + "11" * 20,
            "0x" + "22" * 20,
            {"number": "0xb"},
        )
        self.assertTrue(all(check.passed for check in report.checks))
        self.assertEqual(
            [method for method, _params in writer.calls],
            ["eth_call", "eth_simulateV1", "eth_estimateGas"] * 2,
        )
        self.assertEqual(
            [method for method, _params in leafage.calls],
            ["contractMultiCall", "simulateTransactions", "estimateGas"] * 2,
        )
        sender_request = writer.calls[0][1][0]
        receiver_request = writer.calls[3][1][0]
        self.assertEqual(sender_request["from"], MODULE.USDC)
        self.assertEqual(receiver_request["to"], MODULE.USDC)
        self.assertEqual(receiver_request["value"], "0x1")

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

    def test_environment_probe_returns_every_explicit_block_override(self):
        parent_hash = "0x" + "aa" * 32
        overrides = MODULE.environment_probe_overrides(
            {
                "number": "0xb",
                "time": "0x65",
                "gasLimit": "0x1c9c380",
                "coinbase": "0x" + "11" * 20,
                "random": "0x" + "00" * 32,
                "baseFee": "0x4a817c800",
                "blockHash": {"10": parent_hash},
            }
        )
        self.assertEqual(overrides["number"], "0xb")
        self.assertEqual(overrides["time"], "0x76")
        self.assertEqual(overrides["gasLimit"], "0x17d7840")
        self.assertEqual(overrides["coinbase"], "0x" + "00" * 18 + "c0fe")
        self.assertEqual(overrides["random"], "0x" + "11" * 32)
        self.assertEqual(overrides["baseFee"], "0x7b")
        self.assertEqual(overrides["blockHash"], {"10": "0x" + "22" * 32})

        probe = MODULE.environment_probe_request("0x" + "22" * 20, 10)
        self.assertNotIn("to", probe)
        self.assertEqual(
            probe["data"],
            "0x43600052426020524160405245606052486080524460a052600a4060c05260e06000f3",
        )
        self.assertEqual(
            MODULE.environment_probe_output(10, overrides),
            MODULE.abi_words(
                11,
                0x76,
                0xC0FE,
                25_000_000,
                123,
                int("11" * 32, 16),
                int("22" * 32, 16),
            ),
        )
        self.assertEqual(
            MODULE.environment_probe_output(10, overrides, call_like=True),
            MODULE.abi_words(
                11,
                0x76,
                0xC0FE,
                25_000_000,
                0,
                int("11" * 32, 16),
                int("22" * 32, 16),
            ),
        )
        baseline_overrides = {
            "number": hex(MODULE.ARC_MAINNET_BASELINE_BLOCK + 1),
            "time": "0x6a812b10",
            "gasLimit": "0x17d7840",
            "coinbase": "0x" + "00" * 18 + "c0fe",
            "random": "0x" + "11" * 32,
            "baseFee": "0x7b",
            "blockHash": {
                str(MODULE.ARC_MAINNET_BASELINE_BLOCK): "0x" + "22" * 32
            },
        }
        guard = MODULE.environment_guard_request(
            "0x" + "22" * 20,
            MODULE.ARC_MAINNET_BASELINE_BLOCK,
            baseline_overrides,
        )
        self.assertEqual(
            guard["data"],
            "0x4362f15dbe141561008f5742636a812b10141561008f574161c0fe141561008f57"
            "4563017d7840141561008f5748607b141561008f57447f"
            + "11" * 32
            + "141561008f5762f15dbd407f"
            + "22" * 32
            + "14156100"
            "8f5760006000f35b60006000fd",
        )
        self.assertEqual(guard["gasPrice"], "0x7c")
        h_plus_2 = dict(baseline_overrides)
        h_plus_2["number"] = hex(MODULE.ARC_MAINNET_BASELINE_BLOCK + 2)
        self.assertEqual(
            MODULE.abi_word_at(
                MODULE.environment_probe_output(
                    MODULE.ARC_MAINNET_BASELINE_BLOCK, h_plus_2
                ),
                0,
            ),
            MODULE.ARC_MAINNET_BASELINE_BLOCK + 2,
        )
        merged_guard = MODULE.environment_guard_request(
            "0x" + "22" * 20,
            MODULE.ARC_MAINNET_BASELINE_BLOCK,
            h_plus_2,
            "0x" + "33" * 32,
        )
        self.assertIn(
            "62f15dbc407f" + "33" * 32,
            merged_guard["data"],
        )
        self.assertEqual(MODULE.abi_word_at(MODULE.abi_words(1, 2), 1), 2)

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

    def test_simulation_environment_probe_uses_split_gas_limit_oracle(self):
        anchor_hash = "0x" + "aa" * 32
        base_overrides = {
            "number": "0xb",
            "time": "0x65",
            "gasLimit": hex(30_000_000),
            "coinbase": "0x" + "11" * 20,
            "random": "0x" + "00" * 32,
            "baseFee": hex(20_000_000_000),
            "blockHash": {"10": anchor_hash},
        }
        distinctive = MODULE.environment_probe_overrides(base_overrides)
        distinctive["gasLimit"] = base_overrides["gasLimit"]
        full_output = MODULE.environment_probe_output(10, distinctive)
        history_output = anchor_hash
        previous_hash = "0x" + "55" * 32

        gas_overrides = dict(base_overrides)
        gas_overrides["gasLimit"] = hex(25_000_000)
        writer_gas_output = MODULE.environment_probe_output(
            10, gas_overrides, call_like=True
        )
        leafage_gas_output = MODULE.environment_probe_output(10, gas_overrides)

        class Reference:
            def __init__(self):
                self.calls = []

            def capture(self, method, params):
                self.calls.append((method, params))
                if method == "eth_simulateV1":
                    return {
                        "ok": True,
                        "result": [
                            {
                                "number": "0xb",
                                "timestamp": distinctive["time"],
                                "gasLimit": distinctive["gasLimit"],
                                "miner": distinctive["coinbase"],
                                "calls": [
                                    {
                                        "status": "0x1",
                                        "returnData": full_output,
                                        "gasUsed": "0x100",
                                        "logs": [],
                                    },
                                    {
                                        "status": "0x1",
                                        "returnData": history_output,
                                        "gasUsed": "0x101",
                                        "logs": [],
                                    },
                                    {
                                        "status": "0x1",
                                        "returnData": previous_hash,
                                        "gasUsed": "0x102",
                                        "logs": [],
                                    },
                                ],
                            }
                        ],
                    }
                if method == "eth_call":
                    return {"ok": True, "result": writer_gas_output}
                raise AssertionError(method)

            def call(self, method, params):
                self.calls.append((method, params))
                if method == "eth_getBlockByNumber":
                    return {"parentHash": previous_hash}
                raise AssertionError(method)

        class Leafage:
            def __init__(self):
                self.outputs = iter(
                    (
                        [full_output, history_output, previous_hash],
                        [leafage_gas_output],
                    )
                )

            def capture(self, method, _params):
                if method != "simulateTransactions":
                    raise AssertionError(method)
                outputs = next(self.outputs)
                return {
                    "ok": True,
                    "result": {
                        "results": [
                            {
                                "code": 0,
                                "err": "",
                                "gas_used": hex(0x100 + index),
                                "events": [],
                                "traces": [
                                    {
                                        "id": f"root-{index}",
                                        "tx_id": "0x" + "66" * 32,
                                        "parent_trace_id": "",
                                        "pos_in_parent_trace": 0,
                                        "output": output,
                                    }
                                ],
                            }
                            for index, output in enumerate(outputs)
                        ],
                        "stats": {
                            "success": True,
                            "block_num": 10,
                            "block_hash": anchor_hash,
                            "block_time": 100,
                        },
                    },
                }

        report = MODULE.Report(10)
        reference = Reference()
        MODULE.run_simulation_environment_overrides(
            report,
            Leafage(),
            reference,
            10,
            "0x" + "44" * 20,
            base_overrides,
            100,
            anchor_hash,
        )
        self.assertTrue(all(check.passed for check in report.checks))
        self.assertEqual(
            [method for method, _params in reference.calls],
            ["eth_getBlockByNumber", "eth_simulateV1", "eth_call"],
        )

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
                {
                    "id": "root",
                    "tx_id": "0x" + "66" * 32,
                    "parent_trace_id": "",
                    "pos_in_parent_trace": 0,
                    "output": parent_hash,
                },
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
        sender = "0x" + "11" * 20
        target = "0x" + "22" * 20
        failure_calls = [
            MODULE.call_request(sender, target, gas=0x100),
            MODULE.call_request(sender, MODULE.P256_PRECOMPILE, gas=0x100),
        ]
        failed = {
            "code": -39000,
            "err": "Reverted: ERC20: transfer amount exceeds balance",
            "gas_used": "0x1",
            "events": [],
            "traces": [
                {
                    "id": "failed-root",
                    "tx_id": "0x" + "66" * 32,
                    "parent_trace_id": "",
                    "pos_in_parent_trace": 0,
                    "type": "call",
                    "call_type": "call",
                    "from_addr": sender,
                    "to_addr": target,
                    "value": "0x0",
                    "input": "0x",
                    "output": "0x",
                    "gas_limit": "0x100",
                    "gas_used": "0x1",
                }
            ],
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
                            "error": {
                                "message": "execution reverted: ERC20: transfer amount exceeds balance"
                            },
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
            failure_calls,
            overrides,
            100,
            "0x" + "33" * 32,
        )
        self.assertTrue(all(check.passed for check in report.checks))
        method, params = reference_client.calls[0]
        self.assertEqual(method, "eth_simulateV1")
        self.assertIs(params[0]["validation"], False)
        self.assertIs(params[0]["traceTransfers"], False)

        empty_trace = json.loads(json.dumps(leafage))
        empty_trace["result"]["results"][0]["traces"] = []
        empty_trace["result"]["results"][1]["traces"] = []
        missing_trace = MODULE.Report(10)
        MODULE.run_simulation(
            missing_trace,
            Client(empty_trace),
            Client(reference),
            10,
            "failure_then_p256",
            failure_calls,
            overrides,
            100,
            "0x" + "33" * 32,
        )
        failed_root_check = next(
            check
            for check in missing_trace.checks
            if check.name == "failure_then_p256.failed_trace_root"
        )
        self.assertFalse(failed_root_check.passed)

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
            failure_calls,
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

    def test_simulation_default_and_derived_fee_environments_are_distinct(self):
        block_number = 10
        sender = "0x" + "11" * 20
        parent_hash = "0x" + "22" * 32
        anchor_hash = "0x" + "33" * 32
        random = "0x" + "44" * 32
        anchor = {
            "parentHash": parent_hash,
            "hash": anchor_hash,
            "baseFeePerGas": "0x5",
            "timestamp": "0x64",
            "gasLimit": "0x100000",
            "miner": sender,
            "mixHash": random,
            "extraData": "0x" + (123).to_bytes(8, "big").hex(),
        }
        default_environment = {
            "number": hex(block_number),
            "time": anchor["timestamp"],
            "gasLimit": anchor["gasLimit"],
            "coinbase": sender,
            "random": random,
            "baseFee": anchor["baseFeePerGas"],
            "blockHash": {str(block_number - 1): parent_hash},
        }
        default_output = MODULE.environment_probe_output(
            block_number - 1, default_environment
        )

        class Reference:
            def capture(self, method, params):
                self.call = (method, params)
                return {"ok": True, "result": default_output}

        class Leafage:
            def __init__(self, output, stats):
                self.output = output
                self.stats = stats

            def capture(self, method, params):
                self.call = (method, params)
                return {
                    "ok": True,
                    "result": {
                        "results": [
                            {
                                "code": 0,
                                "traces": [
                                    {"parent_trace_id": "", "output": self.output}
                                ],
                            }
                        ],
                        "stats": self.stats,
                    },
                }

        default_leafage = Leafage(
            default_output,
            {
                "block_num": hex(block_number),
                "block_hash": anchor_hash,
                "block_time": anchor["timestamp"],
            },
        )
        default_report = MODULE.Report(block_number)
        MODULE.run_simulation_default_environment(
            default_report,
            default_leafage,
            Reference(),
            block_number,
            sender,
            anchor,
        )
        self.assertTrue(all(check.passed for check in default_report.checks))
        self.assertIsNone(default_leafage.call[1][2])

        explicit_overrides = {
            "number": hex(block_number + 1),
            "time": "0x65",
            "gasLimit": "0x100000",
            "coinbase": sender,
            "random": random,
            "baseFee": "0x999",
            "blockHash": {str(block_number): anchor_hash},
        }
        derived_environment = dict(explicit_overrides)
        derived_environment["baseFee"] = hex(123)
        derived_output = MODULE.environment_probe_output(
            block_number, derived_environment
        )
        derived_leafage = Leafage(derived_output, {})
        derived_report = MODULE.Report(block_number)
        MODULE.run_simulation_derived_next_base_fee(
            derived_report,
            derived_leafage,
            block_number,
            sender,
            anchor,
            {"baseFeePerGas": hex(123)},
            explicit_overrides,
        )
        self.assertTrue(all(check.passed for check in derived_report.checks))
        self.assertNotIn("baseFee", derived_leafage.call[1][2])
        self.assertEqual(MODULE.arc_next_base_fee_from_extra_data(anchor["extraData"]), 123)
        with self.assertRaisesRegex(ValueError, "nextBaseFee"):
            MODULE.arc_next_base_fee_from_extra_data("0x01")

    def test_valid_eip7702_estimate_request_preserves_authorization(self):
        request = MODULE.eip7702_estimate_request(MODULE.ARC_MAINNET_BASELINE_FUNDED)
        self.assertEqual(request["to"], MODULE.EIP7702_AUTHORITY)
        self.assertEqual(
            request["authorizationList"], [MODULE.EIP7702_AUTHORIZATION]
        )
        self.assertEqual(MODULE.EIP7702_ESTIMATE_GAS, 0xB52E)


if __name__ == "__main__":
    unittest.main()
