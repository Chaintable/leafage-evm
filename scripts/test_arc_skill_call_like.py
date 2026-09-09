import unittest
from copy import deepcopy
from types import SimpleNamespace

from arc_skill_call_like import compare, event_acceptance, rpc_kind, trace_acceptance, reverted_event_acceptance, revert_gas_from_trace


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.core = SimpleNamespace(SYSTEM_ADDRESS="system")
        self.system = {"address": "system", "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", "from", "to"], "data": "0x" + "0" * 63 + "1"}
        self.ordinary = dict(self.system, address="usdc")

    def test_exact(self):
        self.assertEqual(compare(123, 123, "gas")["status"], "PASS")

    def test_mutated_gas_not_accepted(self):
        self.assertEqual(compare(124, 123, "gas")["status"], "FAIL")

    def test_accepted_difference_stays_fail(self):
        result = compare("frame", "system", "emitter", "accepted")
        self.assertEqual(result["status"], "FAIL")
        self.assertEqual(result["acceptance"], "accepted")

    def test_known_missing_system_log(self):
        self.assertIsNotNone(event_acceptance([], [self.system], self.core))

    def test_known_frame_emitter(self):
        self.assertIsNotNone(event_acceptance([dict(self.system, address="frame")], [self.system], self.core, {"frame"}))

    def test_unknown_emitter_rejected(self):
        self.assertIsNone(event_acceptance([dict(self.system, address="wrong")], [self.system], self.core, {"frame"}))

    def test_missing_ordinary_log_rejected(self):
        self.assertIsNone(event_acceptance([], [self.ordinary], self.core))

    def test_changed_system_value_rejected(self):
        self.assertIsNone(event_acceptance([dict(self.system, data="0x" + "0" * 64)], [self.system], self.core))

    def test_unknown_system_event_rejected(self):
        self.assertIsNone(event_acceptance([], [dict(self.system, topics=["unknown"])], self.core))

    def test_extra_event_rejected(self):
        self.assertIsNone(event_acceptance([self.ordinary], [], self.core))

    def test_ordinary_order_preserved(self):
        other = dict(self.ordinary, address="other")
        self.assertIsNone(event_acceptance([other, self.ordinary], [self.ordinary, other], self.core))

    def test_leafage_error_codes(self):
        core = SimpleNamespace(rpc_error_class=lambda response: "fallback")
        for code, kind in ((-39000, "revert"), (-39001, "out-of-gas"),
                           (-39002, "insufficient-funds"), (-39003, "nonce")):
            self.assertEqual(rpc_kind({"error": {"code": code, "message": ""}}, core), kind)
        self.assertEqual(rpc_kind({"error": {"code": -39004}}, core), "fallback")


class TraceAcceptanceTests(unittest.TestCase):
    def setUp(self):
        # This suite tests the narrow acceptance projection, not the fixture
        # library's separate serialization/normalization implementation.
        def normalize_writer(frames):
            return [dict({k: v for k, v in frame.items() if k not in ("traceAddress", "error")},
                         path=frame["traceAddress"]) for frame in deepcopy(frames)]
        self.core = SimpleNamespace(normalize_leafage_traces=deepcopy,
                                    normalize_reference_traces=normalize_writer)
        self.root = {"traceAddress": [], "kind": "call", "from": "sender", "to": "contract",
                     "value": "0x0", "input": "0x", "output": "0x", "gas_limit": 100, "gas_used": 70}
        self.failed = dict(self.root, traceAddress=[0], error="Reverted")
        self.successful = dict(self.root, traceAddress=[1], to="child")

    def test_failed_child_projection_preserves_successful_sibling(self):
        expected = self.core.normalize_reference_traces([self.root, self.successful])
        expected[1]["path"] = [0]
        self.assertIsNotNone(trace_acceptance(expected, [self.root, self.failed, self.successful], self.core))

    def test_missing_successful_child_rejected(self):
        actual = self.core.normalize_reference_traces([self.root])
        self.assertIsNone(trace_acceptance(actual, [self.root, self.failed, self.successful], self.core))

    def test_failed_child_does_not_allow_changed_root_gas(self):
        actual = self.core.normalize_reference_traces([self.root])
        actual[0]["gas_used"] += 1
        self.assertIsNone(trace_acceptance(actual, [self.root, self.failed], self.core))

    def test_selfdestruct_projection(self):
        child = dict(self.root, traceAddress=[0], kind="selfdestruct")
        actual = self.core.normalize_reference_traces([self.root])
        actual[0]["kind"] = "selfdestruct"
        self.assertIsNotNone(trace_acceptance(actual, [self.root, child], self.core))
        actual[0]["gas_used"] += 1
        self.assertIsNone(trace_acceptance(actual, [self.root, child], self.core))


class RevertGasTests(unittest.TestCase):
    def setUp(self):
        self.tx = {"gas": "0xf4240", "data": "0xffffffff"}
        self.root = {"error": "Reverted", "action": {"gas": "0xeeff8"}, "result": {"gasUsed": "0xc8"}}

    def test_execution_cost_above_floor(self):
        self.assertEqual(revert_gas_from_trace(self.tx, self.root), 21264)

    def test_calldata_floor_not_plain_intrinsic_plus_frame(self):
        self.tx["data"] = "0x40c10f19" + "00" * 30 + "beef" + "00" * 31 + "01"
        self.root["action"]["gas"] = "0xeeed4"
        self.assertEqual(revert_gas_from_trace(self.tx, self.root), 21890)

    def test_no_generalization_to_halt_or_authorization_refund(self):
        self.root["error"] = "Out of gas"
        with self.assertRaises(ValueError):
            revert_gas_from_trace(self.tx, self.root)
        self.root["error"] = "Reverted"
        self.tx["authorizationList"] = [{}]
        with self.assertRaises(ValueError):
            revert_gas_from_trace(self.tx, self.root)


class RevertedEventTests(unittest.TestCase):
    def setUp(self):
        self.core = SimpleNamespace(abi_words=lambda n: "0x" + format(n, "064x"),
                                    normalize_leafage_events=deepcopy)
        self.item = {"code": -39000, "traces": [{"parent_trace_id": None, "to_addr": "created"}],
                     "events": [{"address": "created", "topics": [self.core.abi_words(1)], "data": "0x"}]}

    def test_only_exact_documented_event_accepted(self):
        self.assertIsNotNone(reverted_event_acceptance("failed_create_log_revert", self.item, self.core))
        self.item["events"][0]["data"] = "0x01"
        self.assertIsNone(reverted_event_acceptance("failed_create_log_revert", self.item, self.core))

    def test_unknown_case_rejected(self):
        self.assertIsNone(reverted_event_acceptance("unknown", self.item, self.core))

    def test_non_revert_rejected(self):
        self.item["code"] = 0
        self.assertIsNone(reverted_event_acceptance("failed_create_log_revert", self.item, self.core))


if __name__ == "__main__":
    unittest.main()
