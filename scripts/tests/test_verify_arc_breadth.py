import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "verify_arc_breadth.py"
SPEC = importlib.util.spec_from_file_location("verify_arc_breadth", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class VerifyArcBreadthTests(unittest.TestCase):
    def test_historical_samples_cover_deployment_boundary_and_anchor(self):
        self.assertEqual(
            MODULE.historical_samples(100, 1_000),
            (99, 100, 101, 550, 1_000),
        )
        self.assertEqual(
            MODULE.historical_samples(0, 1_000),
            (0, 1, 2, 500, 1_000),
        )

    def test_generated_case_ids_are_unique_and_reach_ten_x_target(self):
        plan = MODULE.build_plan(MODULE.ANCHOR, MODULE.FUNDED, MODULE.EMPTY)
        ids = [case.case_id for case in plan.cases]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual(len(ids), 3_042)
        self.assertGreaterEqual(len(ids), MODULE.TEN_X_TARGET)
        self.assertEqual(plan.base_vectors, 565)
        self.assertEqual(
            plan.endpoint_counts,
            {
                "contractMultiCall": 495,
                "estimateGas": 495,
                "eth_call": 1_283,
                "eth_getCode": 182,
                "eth_getStorageAt": 75,
                "simulateTransactions": 512,
            },
        )
        self.assertEqual(
            set(plan.endpoint_counts),
            {
                "eth_call",
                "contractMultiCall",
                "estimateGas",
                "simulateTransactions",
                "eth_getCode",
                "eth_getStorageAt",
            },
        )

    def test_usdc_proxy_history_uses_its_custom_implementation_slot(self):
        plan = MODULE.build_plan(MODULE.ANCHOR, MODULE.FUNDED, MODULE.EMPTY)
        cases = [
            case
            for case in plan.cases
            if case.domain == "proxy_history" and case.target_name == "usdc"
        ]
        self.assertEqual(len(cases), 5)
        self.assertEqual(
            {case.request["slot"] for case in cases},
            {MODULE.USDC_IMPLEMENTATION_SLOT},
        )

    def test_access_control_vectors_only_use_deployed_owner_selectors(self):
        vectors = {
            (target, scenario, data)
            for target, scenario, data in MODULE.access_control_vectors(MODULE.EMPTY)
        }
        for target in (
            "cctp_token_messenger",
            "cctp_message_transmitter",
            "cctp_token_minter",
        ):
            self.assertNotIn((target, "renounce_ownership", "0x715018a6"), vectors)
        self.assertNotIn(
            (
                "arcane_v3_factory",
                "transfer_ownership",
                MODULE.calldata("0xf2fde38b", MODULE.address_word(MODULE.EMPTY)),
            ),
            vectors,
        )
        self.assertIn(
            (
                "arcane_v3_factory",
                "set_owner",
                MODULE.calldata("0x13af4035", MODULE.address_word(MODULE.EMPTY)),
            ),
            vectors,
        )

    def test_plan_covers_every_frozen_important_target_and_asset(self):
        plan = MODULE.build_plan(MODULE.ANCHOR, MODULE.FUNDED, MODULE.EMPTY)
        covered = {case.target for case in plan.cases}
        self.assertTrue(set(MODULE.required_target_addresses()).issubset(covered))
        for _label, token, holder, *_rest in MODULE.core.IMPORTANT_ERC20_ASSETS:
            self.assertIn(token.lower(), covered)
            self.assertTrue(any(case.actor == holder.lower() for case in plan.cases))
        for label, token in MODULE.EXPECTED_UNDEPLOYED_ASSETS.items():
            self.assertTrue(
                any(
                    case.target_name == label
                    and case.target == token
                    and case.scenario == "not_deployed_at_anchor"
                    for case in plan.cases
                )
            )

    def test_transferable_assets_have_approved_overbalance_sequences(self):
        plan = MODULE.build_plan(MODULE.ANCHOR, MODULE.FUNDED, MODULE.EMPTY)
        for label in ("usdc", "ausd", "agbp"):
            cases = [
                case
                for case in plan.cases
                if case.group == f"{label}.approve_max_then_overbalance"
            ]
            self.assertEqual(len(cases), 2)
            self.assertEqual(
                [case.scenario for case in cases],
                [
                    "sequence.approve_max_for_overbalance",
                    "sequence.transfer_from_overbalance_after_approval",
                ],
            )

    def test_arc_feature_domains_are_present(self):
        plan = MODULE.build_plan(MODULE.ANCHOR, MODULE.FUNDED, MODULE.EMPTY)
        domains = plan.domain_counts
        for domain in (
            "asset",
            "important_contract",
            "p256",
            "pq",
            "system_accounting",
            "eip2935",
            "arc_access_control",
            "arc_feature",
            "historical_business",
        ):
            self.assertGreater(domains.get(domain, 0), 0, domain)

    def test_historical_business_replays_cover_all_execution_endpoints(self):
        plan = MODULE.build_plan(MODULE.ANCHOR, MODULE.FUNDED, MODULE.EMPTY)
        for replay in MODULE.HISTORICAL_REPLAYS:
            endpoints = {
                case.endpoint for case in plan.cases if case.target_name == replay.name
            }
            self.assertEqual(endpoints, set(MODULE.EXECUTION_ENDPOINTS))

    def test_duplicate_semantic_case_is_rejected(self):
        case = MODULE.Case(
            domain="test",
            endpoint="eth_call",
            target="0x" + "11" * 20,
            scenario="same",
            block=1,
            actor="0x" + "22" * 20,
            request={"to": "0x" + "11" * 20, "data": "0x"},
        )
        with self.assertRaises(ValueError):
            MODULE.Plan.from_cases([case, case])

    def test_summary_does_not_count_assertion_dimensions_as_cases(self):
        case = MODULE.Case(
            domain="test",
            endpoint="eth_call",
            target="0x" + "11" * 20,
            scenario="one",
            block=1,
            actor="0x" + "22" * 20,
            request={"to": "0x" + "11" * 20, "data": "0x"},
        )
        result = MODULE.CaseResult(
            case=case,
            passed=True,
            dimensions={"status": True, "output": True, "gas": True},
        )
        summary = MODULE.summarize_results([result])
        self.assertEqual(summary["cases"], 1)
        self.assertEqual(summary["assertions"], 3)

    def test_eth_call_rejects_same_class_with_different_error_detail(self):
        class FakeClient:
            def __init__(self, response):
                self.response = response

            def batch_capture(self, _calls, **_kwargs):
                return [self.response]

        case = MODULE.Case(
            domain="test",
            endpoint="eth_call",
            target="0x" + "11" * 20,
            scenario="different_revert_reason",
            block=1,
            actor="0x" + "22" * 20,
            request={"to": "0x" + "11" * 20, "data": "0x"},
        )
        writer = {
            "ok": False,
            "error": {
                "code": 3,
                "message": "execution reverted: reason A",
                "data": "0x01",
            },
        }
        leafage = {
            "ok": False,
            "error": {
                "code": 3,
                "message": "execution reverted: reason B",
                "data": "0x02",
            },
        }
        result = MODULE.execute_eth_calls(
            [case], FakeClient(leafage), FakeClient(writer), 1
        )[0]
        self.assertFalse(result.passed)
        self.assertFalse(result.dimensions["error_reason"])
        self.assertFalse(result.dimensions["error_data"])

    def test_simulation_rejects_unexpected_child_trace(self):
        class FakeClient:
            def __init__(self, responses):
                self.responses = list(responses)

            def batch_capture(self, _calls, **_kwargs):
                return self.responses.pop(0)

        actor = "0x" + "22" * 20
        target = "0x" + "11" * 20
        tx_id = "0x" + "aa" * 32
        case = MODULE.Case(
            domain="test",
            endpoint="simulateTransactions",
            target=target,
            scenario="trace_child_injection",
            block=1,
            actor=actor,
            request=MODULE.make_request(actor, target, "0x"),
        )
        writer_simulate = {
            "ok": True,
            "result": [
                {
                    "calls": [
                        {
                            "status": "0x1",
                            "returnData": "0x",
                            "gasUsed": "0x5208",
                            "logs": [],
                        }
                    ]
                }
            ],
        }
        writer_trace = {
            "ok": True,
            "result": [
                {
                    "error": None,
                    "gasUsed": "0x5208",
                    "trace": [
                        {
                            "type": "call",
                            "traceAddress": [],
                            "action": {
                                "callType": "call",
                                "from": actor,
                                "to": target,
                                "value": "0x0",
                                "input": "0x",
                                "gas": "0xf4240",
                            },
                            "result": {"output": "0x", "gasUsed": "0x5208"},
                        }
                    ],
                }
            ],
        }
        root = {
            "id": "root",
            "tx_id": tx_id,
            "parent_trace_id": "",
            "pos_in_parent_trace": 0,
            "type": "call",
            "call_type": "call",
            "from_addr": actor,
            "to_addr": target,
            "value": "0x0",
            "input": "0x",
            "output": "0x",
            "gas_limit": "0xf4240",
            "gas_used": "0x5208",
            "self_storage_change": False,
            "storage_change": False,
        }
        unexpected_child = {
            **root,
            "id": "child",
            "parent_trace_id": "root",
            "pos_in_parent_trace": 0,
            "from_addr": target,
            "to_addr": actor,
            "gas_limit": "0x100",
            "gas_used": "0x1",
        }
        leafage_simulate = {
            "ok": True,
            "result": {
                "results": [
                    {
                        "code": 0,
                        "err": "",
                        "gas_used": 21_000,
                        "events": [],
                        "traces": [root, unexpected_child],
                    }
                ],
                "stats": {
                    "block_num": 1,
                    "block_hash": "0x" + "bb" * 32,
                    "block_time": 2,
                    "success": True,
                },
            },
        }
        result = MODULE.execute_simulations(
            [case],
            FakeClient([[leafage_simulate]]),
            FakeClient([[writer_simulate], [writer_trace]]),
            {1: {"hash": "0x" + "bb" * 32, "timestamp": "0x2"}},
            {1: {}},
        )[0]
        self.assertFalse(result.passed)
        self.assertFalse(result.dimensions["trace"])

    def test_custom_result_rejects_boolean_code_and_success_error(self):
        class FakeClient:
            def __init__(self, responses):
                self.responses = list(responses)

            def batch_capture(self, _calls, **_kwargs):
                return self.responses.pop(0)

        actor = "0x" + "22" * 20
        target = "0x" + "11" * 20
        case = MODULE.Case(
            domain="test",
            endpoint="contractMultiCall",
            target=target,
            scenario="invalid_custom_schema",
            block=1,
            actor=actor,
            request=MODULE.make_request(actor, target, "0x"),
        )
        writer_call = {"ok": True, "result": "0x"}
        writer_trace = {
            "ok": True,
            "result": {"gasUsed": "0x5208", "output": "0x"},
        }
        leafage = {
            "ok": True,
            "result": {
                "results": [
                    {
                        "code": False,
                        "err": "should-not-be-present",
                        "result": "0x",
                        "gas_used": 21_000,
                        "from_cache": False,
                        "time_cost": 0.0,
                    }
                ],
                "stats": {
                    "block_num": 1,
                    "block_hash": "0x" + "bb" * 32,
                    "block_time": 2,
                    "success": True,
                    "cache_enabled": False,
                },
            },
        }
        result = MODULE.execute_contract_multicalls(
            [case],
            FakeClient([[leafage]]),
            FakeClient([[writer_call], [writer_trace]]),
            {1: {"hash": "0x" + "bb" * 32, "timestamp": "0x2"}},
        )[0]
        self.assertFalse(result.passed)
        self.assertFalse(result.dimensions["schema"])

    def test_custom_result_schema_rejects_invalid_scalar_types(self):
        valid = {"code": 0, "err": "", "gas_used": 21_000}
        self.assertTrue(MODULE.custom_result_schema(valid))
        for code in (-32_602, -32_601):
            self.assertTrue(
                MODULE.custom_result_schema(
                    {"code": code, "err": "rpc error", "gas_used": 21_000}
                )
            )
        for field, invalid_values in {
            "code": (False, 0.0, 1, -1, -39_009),
            "err": (None, 1, "unexpected"),
            "gas_used": (True, 21_000.0, "0x5208", -1),
        }.items():
            for invalid in invalid_values:
                item = {**valid, field: invalid}
                self.assertFalse(
                    MODULE.custom_result_schema(item), (field, invalid)
                )
        for invalid in (True, 1.0, "1"):
            self.assertFalse(MODULE.quantity_equals(invalid, 1))
        for invalid in (-1, float("nan"), float("inf"), True, "0"):
            self.assertFalse(MODULE.nonnegative_finite_number(invalid))

    def test_failed_simulation_validates_root_trace_fields(self):
        class FakeClient:
            def __init__(self, responses):
                self.responses = list(responses)

            def batch_capture(self, _calls, **_kwargs):
                return self.responses.pop(0)

        actor = "0x" + "22" * 20
        target = "0x" + "11" * 20
        case = MODULE.Case(
            domain="test",
            endpoint="simulateTransactions",
            target=target,
            scenario="failed_trace_root_injection",
            block=1,
            actor=actor,
            request=MODULE.make_request(actor, target, "0xdead"),
        )
        writer_simulate = {
            "ok": True,
            "result": [
                {
                    "calls": [
                        {
                            "status": "0x0",
                            "returnData": "0x",
                            "gasUsed": "0x5208",
                            "logs": [],
                            "error": {
                                "code": -32000,
                                "message": "execution reverted",
                            },
                        }
                    ]
                }
            ],
        }
        writer_trace = {
            "ok": True,
            "result": [{"error": "revert", "gasUsed": "0x0", "trace": []}],
        }
        bad_root = {
            "id": "root",
            "tx_id": "0x" + "aa" * 32,
            "parent_trace_id": "",
            "pos_in_parent_trace": 0,
            "type": "call",
            "call_type": "call",
            "from_addr": target,
            "to_addr": actor,
            "value": "0x0",
            "input": "0xbeef",
            "output": "0x",
            "gas_limit": "0x1",
            "gas_used": "0x1",
            "self_storage_change": False,
            "storage_change": False,
        }
        leafage_simulate = {
            "ok": True,
            "result": {
                "results": [
                    {
                        "code": MODULE.core.LEAFAGE_EVM_REVERT,
                        "err": "",
                        "gas_used": 21_000,
                        "events": [],
                        "traces": [bad_root],
                    }
                ],
                "stats": {
                    "block_num": 1,
                    "block_hash": "0x" + "bb" * 32,
                    "block_time": 2,
                    "success": False,
                },
            },
        }
        result = MODULE.execute_simulations(
            [case],
            FakeClient([[leafage_simulate]]),
            FakeClient([[writer_simulate], [writer_trace]]),
            {1: {"hash": "0x" + "bb" * 32, "timestamp": "0x2"}},
            {1: {}},
        )[0]
        self.assertFalse(result.passed)
        self.assertFalse(result.dimensions["trace"])


if __name__ == "__main__":
    unittest.main()
