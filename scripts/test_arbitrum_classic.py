#!/usr/bin/env python3
"""Read-only differential checks for account-state-only Classic execution."""
import argparse
import json
import urllib.request
from pathlib import Path

PROBE = '0x000000000000000000000000000000000c1a551c'
CHILD = '0x000000000000000000000000000000000c1a551d'
SENDER = '0x000000000000000000000000000000000cafe123'
ARBSYS = '0x0000000000000000000000000000000000000064'
ARBINFO = '0x0000000000000000000000000000000000000065'
TOKENS = [
    '0x82af49447d8a07e3bd95bd0d56f35241523fbab1',  # WETH
    '0xff970a61a04b1ca14834a43f5de4533ebddb5cc8',  # native Classic USDC
]


def rpc(url, method, params):
    request = urllib.request.Request(
        url, json.dumps(dict(jsonrpc='2.0', id=1, method=method, params=params)).encode(),
        {'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--classic', required=True)
    parser.add_argument('--leafage', required=True)
    parser.add_argument('--heights', default='156000,1107013,4198902')
    parser.add_argument('--report', required=True)
    args = parser.parse_args()
    records = []

    def record(name, height, ok, **details):
        records.append(dict(name=name, height=height, ok=ok, **details))
        if not ok:
            print('FAIL', height, name, json.dumps(details)[:1200], flush=True)

    def compare(name, height, method, params):
        reference = rpc(args.classic, method, params)
        actual = rpc(args.leafage, method, params)
        ok = 'result' in reference and 'result' in actual and reference['result'] == actual['result']
        record(name, height, ok, reference=reference, actual=actual)
        return reference.get('result')

    def rejected(name, height, method, params):
        actual = rpc(args.leafage, method, params)
        record(name, height, 'Arbitrum Classic:' in actual.get('error', {}).get('message', ''), actual=actual)

    heights = [int(v) for v in args.heights.split(',')]
    for height in heights:
        block = hex(height)
        token_requests = []
        for token in TOKENS:
            for method, extra in [('eth_getCode', []), ('eth_getBalance', []),
                                  ('eth_getTransactionCount', []), ('eth_getStorageAt', ['0x0'])]:
                compare(f'{method}:{token}', height, method, [token, *extra, block])
            for selector in ['0x06fdde03', '0x95d89b41', '0x313ce567', '0x18160ddd',
                             '0x70a08231' + SENDER[2:].zfill(64)]:
                request = dict(to=token, data=selector, gas='0x989680')
                expected = compare(f'token:{token}:{selector[:10]}', height, 'eth_call', [request, block])
                token_requests.append((request, expected))

        for name, bytecode in [
            ('coinbase', '41'), ('difficulty', '44'), ('timestamp', '42'),
            ('chainid', '46'), ('caller', '33'), ('origin', '32'), ('selfbalance', '47'),
        ]:
            code = '0x' + bytecode + '60005260206000f3'
            compare(name, height, 'eth_call', [dict(to=PROBE, data='0x', gas='0xf4240', **{'from': SENDER}), block,
                    {PROBE: dict(code=code, balance='0x7b'), SENDER: dict(nonce='0x7', balance='0xde0b6b3a7640000')}])

        for name, code in [
            ('returndatacopy.empty', '6001600060003e60206000f3'),
            ('returndatacopy.zero_length', '600060ff60003e60206000f3'),
            ('sstore', '600160005560005460005260206000f3'),
        ]:
            compare(name, height, 'eth_call', [dict(to=PROBE, gas='0xf4240', **{'from': SENDER}), block,
                    {PROBE: dict(code='0x' + code), SENDER: dict(nonce='0x7', balance='0xde0b6b3a7640000')}])

        for selector in ['a3b1b31d', 'd127f54a', '08bd624c', '23ca0cd2' + SENDER[2:].zfill(64)]:
            compare('arbsys.direct:' + selector[:8], height, 'eth_call', [dict(to=ARBSYS, data='0x' + selector, **{'from': SENDER}), block, {SENDER: dict(nonce='0x7')}])
            code = '0x3660006000376020600036600060645afa5060206000f3'
            compare('arbsys.nested:' + selector[:8], height, 'eth_call', [dict(to=PROBE, data='0x' + selector, **{'from': SENDER}), block, {PROBE: dict(code=code), SENDER: dict(nonce='0x7')}])

        for selector in ['f8b2cb4f', '7e105ce2']:
            compare('arbinfo:' + selector, height, 'eth_call', [dict(to=ARBINFO, data='0x' + selector + TOKENS[0][2:].zfill(64)), block])

        for name, opcode in [('NUMBER', '43'), ('BLOCKHASH', '40'), ('GASLIMIT', '45'), ('GASPRICE', '3a'), ('BASEFEE', '48')]:
            # BLOCKHASH needs an operand even though Classic fails before lookup.
            code = '0x6000' + opcode + '60005260206000f3'
            rejected('unsupported:' + name, height, 'eth_call', [dict(to=PROBE), block, {PROBE: dict(code=code)}])
            # A parent that ignores a child failure must not return a fake value.
            parent = '0x6000600060006000600073' + CHILD[2:] + '5af150602a60005260206000f3'
            rejected('unsupported.nested:' + name, height, 'eth_call', [dict(to=PROBE), block, {PROBE: dict(code=parent), CHILD: dict(code=code)}])
        for address, data in [(ARBSYS, '0x051038f2'), ('0x000000000000000000000000000000000000006c', '0x')]:
            rejected('builtin:' + address + data, height, 'eth_call', [dict(to=address, data=data), block])
        rejected('trace.unsupported', height, 'pre_traceCall', [dict(to=ARBSYS, data='0x051038f2'), block])
        rejected('estimate.unsupported', height, 'debank_estimateGas', [dict(to=TOKENS[0], data='0x313ce567'), {'blockId': block, 'type': 'Equals'}])

        for parallel in [False, True]:
            actual = rpc(args.leafage, 'eth_multiCall', [[r for r, _ in token_requests], block, False, parallel, True])
            outputs = actual.get('result', {}).get('results', [])
            ok = len(outputs) == len(token_requests) and all(v.get('code') == 0 and v.get('result') == exp for v, (_, exp) in zip(outputs, token_requests))
            record('multicall.parallel=' + str(parallel), height, ok, actual=actual)
        actual = rpc(args.leafage, 'pre_traceCall', [dict(to=TOKENS[0], data='0x313ce567'), block])
        record('trace.supported', height, 'result' in actual and not actual['result'].get('failed', True), actual=actual)
        print('height', height, 'complete', flush=True)

    summary = dict(total=len(records), passed=sum(r['ok'] for r in records), failed=sum(not r['ok'] for r in records))
    Path(args.report).write_text(json.dumps(dict(summary=summary, records=records), indent=2) + '\n')
    print(json.dumps(summary))
    return int(summary['failed'] != 0)


if __name__ == '__main__':
    raise SystemExit(main())
