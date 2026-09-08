import importlib.util
import json
import sys
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "verify-tempo-mainnet-fixtures.py"
SPEC = importlib.util.spec_from_file_location("verify_tempo_mainnet_fixtures", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)

MANIFEST = Path(__file__).parents[1] / "fixtures" / "tempo-t5-t10-mainnet.json"


class VerifyTempoMainnetFixturesTests(unittest.TestCase):
    def test_manifest_has_one_ordered_fixture_per_hardfork(self):
        manifest = json.loads(MANIFEST.read_text())
        fixtures = manifest["fixtures"]
        self.assertEqual(
            [fixture["hardfork"] for fixture in fixtures],
            [f"T{i}" for i in range(5, 11)],
        )
        self.assertEqual(len({fixture["id"] for fixture in fixtures}), 6)
        self.assertEqual(len({fixture["transaction_hash"] for fixture in fixtures}), 6)
        self.assertEqual(
            [fixture["block_number"] for fixture in fixtures],
            sorted(fixture["block_number"] for fixture in fixtures),
        )

    def test_verify_fixture_checks_transaction_event_and_state(self):
        fixture = {
            "id": "sample",
            "block_number": 10,
            "transaction_hash": "0xabc",
            "transaction": {
                "type": "0x76",
                "index": "0x0",
                "execution": {
                    "kind": "aa_call",
                    "index": 0,
                    "to": "0xdef",
                    "selector": "0x12345678",
                },
                "fields": {"signature.version": "v2"},
            },
            "receipt": {
                "status": "0x1",
                "gas_used": "0x20",
                "event": {
                    "log_index": "0x3",
                    "address": "0xfeed",
                    "topics": ["0xtopic"],
                },
            },
            "state_assertions": [
                {"to": "0xdef", "data": "0x90abcdef", "before": "0x00", "after": "0x01"}
            ],
        }
        tx = {
            "hash": "0xABC",
            "blockNumber": "0xa",
            "transactionIndex": "0x0",
            "type": "0x76",
            "calls": [{"to": "0xDEF", "input": "0x12345678ffff"}],
            "signature": {"version": "v2"},
        }
        receipt = {
            "blockNumber": "0xa",
            "status": "0x1",
            "gasUsed": "0x20",
            "logs": [{"logIndex": "0x3", "address": "0xFEED", "topics": ["0xtopic"]}],
        }

        def rpc(method, params):
            if method == "eth_getTransactionByHash":
                return tx
            if method == "eth_getTransactionReceipt":
                return receipt
            if method == "eth_call":
                return "0x00" if params[1] == "0x9" else "0x01"
            self.fail(f"unexpected RPC method {method}")

        MODULE.verify_fixture(fixture, rpc)

    def test_verify_fixture_rejects_selector_mismatch(self):
        fixture = {
            "id": "bad-selector",
            "block_number": 10,
            "transaction_hash": "0xabc",
            "transaction": {
                "type": "0x2",
                "index": "0x0",
                "execution": {
                    "kind": "transaction",
                    "to": "0xdef",
                    "selector": "0x12345678",
                },
            },
            "receipt": {
                "status": "0x1",
                "gas_used": "0x20",
                "event": {
                    "log_index": "0x0",
                    "address": "0xfeed",
                    "topics": ["0xtopic"],
                },
            },
        }
        tx = {
            "hash": "0xabc",
            "blockNumber": "0xa",
            "transactionIndex": "0x0",
            "type": "0x2",
            "to": "0xdef",
            "input": "0x87654321",
        }
        receipt = {
            "blockNumber": "0xa",
            "status": "0x1",
            "gasUsed": "0x20",
            "logs": [{"logIndex": "0x0", "address": "0xfeed", "topics": ["0xtopic"]}],
        }

        def rpc(method, _params):
            return tx if method == "eth_getTransactionByHash" else receipt

        with self.assertRaisesRegex(MODULE.VerificationError, "selector"):
            MODULE.verify_fixture(fixture, rpc)


if __name__ == "__main__":
    unittest.main()
