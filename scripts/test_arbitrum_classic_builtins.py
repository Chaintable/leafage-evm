#!/usr/bin/env python3
"""Read-only Classic builtin and multicall regression checks against native AVM."""
import argparse
import json
from pathlib import Path
from test_arbitrum_classic import rpc, PROBE, SENDER

BUILTINS = [0x64, 0x66, 0x67, 0x68, 0x69, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0xc8]
PROBES = ['5c60da1b', 'a619486e', 'bb82aa5e']


def word(n):
    return format(n, '064x')


def address(n):
    return '0x' + format(n, '040x')


def decompress(payload, offset=0, length=None, abi_offset=64):
    size = len(payload) // 2 if length is None else length
    return ('31862ada' + word(abi_offset) + word(offset) + word(size)
            + payload + '00' * ((32 - len(payload) // 2 % 32) % 32))


def vectors():
    for at in BUILTINS:
        for selector in ['', '01', 'deadbeef'] + PROBES:
            yield 'unknown.%x.%s' % (at, selector), at, selector
    for selector in ['4420e486', 'd4b6b5da', 'a5025222', '8a186788', 'f6a455a2']:
        yield 'address_table.zero.' + selector, 0x66, selector + word(0)
    for payload in ['80', '00', '8100', '94' + '00' * 18 + 'beef',
                    'a0' + '00' * 30 + 'beef', '95' + 'ff' * 21, 'a1', '9400']:
        yield 'decompress.' + payload, 0x66, decompress(payload)
    for name, data in [
        ('offset', decompress('ff8094' + '00' * 20, offset=1)),
        ('bad_abi_offset', decompress('80', abi_offset=32)),
        ('past_end', decompress('80', offset=1)),
        ('padded', decompress('94' + '00' * 31, length=1000)),
        ('sparse_zero', decompress('80', offset=100, length=101)),
        ('oversized_offset', decompress('80', offset=2**64, length=2**64 + 1)),
        ('oversized_length', decompress('80', length=2**256 - 1)),
    ]:
        yield 'decompress.' + name, 0x66, data
    for at, selector in [
        (0x66, '949d225d'), (0x67, '857cdbb8'), (0x68, '88987068'),
        (0x69, '7b510fe8'), (0x6b, '84352b19'), (0x6c, '41b247a8'),
        (0x6d, '875883f2'), (0x6e, '81e6e083'), (0x6f, 'c59d4847'),
        (0x64, '25e16063'),
    ]:
        yield 'malformed.%x.%s' % (at, selector), at, selector + '01'
    yield 'sendTxToL1.bad_offset', 0x64, '928c169a' + word(1) + word(32) + word(0)
    yield 'sendTxToL1.truncated', 0x64, '928c169a' + word(1) + word(64) + word(1)
    yield 'sendTxToL1.oversized', 0x64, '928c169a' + word(1) + word(64) + word(2**256 - 1)
    for payload in ['', 'a1', '820401', '0180']:
        yield 'function_table.invalid.' + payload, 0x68, 'ce2ae159' + word(32) + word(len(payload) // 2) + payload
    yield 'burn_zero', 0x69, 'bb3480f9' + word(0)
    # These valid debug methods reject a nonzero caller before private-state access.
    for selector, count in [('7b510fe8', 1), ('d56aa31f', 1), ('1d79f325', 2),
                            ('e30443bc', 2), ('978190f1', 3), ('afefabf7', 3), ('f0c26aff', 3)]:
        yield 'debug.unauthorized.' + selector, 0x69, selector + word(0) * count


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--classic', required=True)
    parser.add_argument('--leafage', required=True)
    parser.add_argument('--heights', default='1,156000,4198902,22207815')
    parser.add_argument('--report', required=True)
    args = parser.parse_args()
    records = []

    def record(name, height, ok, **details):
        records.append(dict(name=name, height=height, ok=ok, **details))
        if ok is False:
            print('FAIL', height, name, json.dumps(details)[:1500], flush=True)

    def compare(name, height, params):
        reference = rpc(args.classic, 'eth_call', params)
        actual = rpc(args.leafage, 'eth_call', params)
        if 'result' in reference:
            ok = actual.get('result') == reference['result']
        else:
            ok = all('revert' in r.get('error', {}).get('message', '').lower()
                     for r in (reference, actual))
        record(name, height, ok, reference=reference, actual=actual)

    for height in [int(h) for h in args.heights.split(',')]:
        block = hex(height)
        overridden = rpc(args.classic, 'eth_call', [dict(to=PROBE), block,
                         {PROBE: dict(code='0x602a60005260206000f3')}])
        overrides_supported = overridden.get('result') == '0x' + word(42)
        for name, at, data in vectors():
            request = dict(to=address(at), data='0x' + data, gas='0x989680', **{'from': SENDER})
            if at != 0xc8:
                compare(name, height, [request, block])
            else:
                record(name, height, None, skipped='Native RPC intercepts 0xc8 outside EVM execution')
            if not overrides_supported:
                record(name + '.nested', height, None, skipped='Native code override unavailable')
                continue
            # CALL allows stateful methods; it returns success + raw returndata.
            parent = ('0x36600060003760006000366000600060%02x5af1' % at
                      + '6000523d600060203e3d6020016000f3')
            compare(name + '.nested', height, [dict(request, to=PROBE), block, {PROBE: dict(code=parent)}])
        if overrides_supported:
            for at, selector, count in [
                (0x66, '4420e486', 1), (0x67, '375a7c7f', 4),
                (0x6b, '84352b19', 1), (0x6c, '4290549e', 1),
                (0x6d, '6e928a6e', 1), (0x6e, 'f0b21a41', 1),
            ]:
                parent = '0x3660006000376000600036600060%02x5afa60005260206000f3' % at
                compare('static.' + selector, height, [dict(to=PROBE, data='0x' + selector + word(0) * count),
                        block, {PROBE: dict(code=parent)}])
        for at, selector in [(0x67, '857cdbb8' + word(1)), (0x6c, '41b247a8'),
                             (0x6d, '875883f2'), (0x66, '949d225d')]:
            actual = rpc(args.leafage, 'eth_call', [dict(to=address(at), data='0x' + selector), block])
            record('unavailable.%x' % at, height,
                   'requires unavailable Classic ArbOS' in actual.get('error', {}).get('message', ''), actual=actual)

    # Reproduce the platform's 15 proxy probes, including block zero. Native
    # block zero has an independent snapshot RPC bug, so use block one as oracle.
    calls = [dict(to=address(at), data='0x' + selector)
             for at in [0x67, 0x69, 0x6b, 0x6c, 0x6d] for selector in PROBES]
    calls.append(dict(to=address(0x64), data='0xd127f54a'))
    for height in [0, 1]:
        for method in ['eth_multiCall', 'contractMultiCall']:
            for parallel in [False, True]:
                def params(block):
                    if method == 'eth_multiCall':
                        return [calls, hex(block), False, parallel, True]
                    return [calls, dict(block_id=hex(block), type='Equals'), None, None, False, parallel, True]
                reference = rpc(args.classic, method, params(1))
                actual = rpc(args.leafage, method, params(height))
                def normalized(response):
                    return [(x.get('code') == 0, x.get('result'))
                            for x in response.get('result', {}).get('results', [])]
                expected = [(False, '0x')] * 15 + [(True, '0x' + word(42161))]
                record('%s.parallel=%s' % (method, parallel), height,
                       normalized(reference) == normalized(actual) == expected,
                       reference=reference, actual=actual)
    Path(args.report).write_text(json.dumps(records, indent=2) + '\n')
    failed = sum(r['ok'] is False for r in records)
    skipped = sum(r['ok'] is None for r in records)
    print(json.dumps(dict(total=len(records), passed=len(records)-failed-skipped, failed=failed, skipped=skipped)))
    raise SystemExit(bool(failed))


if __name__ == '__main__':
    main()
