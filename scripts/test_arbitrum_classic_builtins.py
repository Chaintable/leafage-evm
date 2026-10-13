#!/usr/bin/env python3
"""Read-only Classic builtin and multicall regression checks against native AVM."""
import argparse
import json
from pathlib import Path
from test_arbitrum_classic import rpc, PROBE, SENDER

BUILTINS = [0x64, 0x66, 0x67, 0x68, 0x69, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0xc8]
PROBES = ['5c60da1b', 'a619486e', 'bb82aa5e']
ARBOS34, ARBOS39, ARBOS42, ARBOS54 = 156077, 226011, 662405, 6835288


def word(n):
    return format(n, '064x')


def address(n):
    return '0x' + format(n, '040x')


def decompress(payload, offset=0, length=None, abi_offset=64):
    size = len(payload) // 2 if length is None else length
    return ('31862ada' + word(abi_offset) + word(offset) + word(size)
            + payload + '00' * ((32 - len(payload) // 2 % 32) % 32))


def upload(payload, length=None, abi_offset=32):
    size = len(payload) // 2 if length is None else length
    return 'ce2ae159' + word(abi_offset) + word(size) + payload


def builtin_program(at, data, static=False, input_size=None, expected_word=None):
    """Return on the asserted builtin result, revert otherwise.

    Native eth_call creation returns the created address, not initcode output.
    This program therefore exposes a boolean through success/revert and also
    works as ordinary overridden code in Leafage, including early history.
    """
    size = len(data) // 2
    visible = size if input_size is None else input_size
    call = ('604061020061%04x6000' % visible + ('' if static else '6000')
            + '60%02x5a' % at + ('fa' if static else 'f1'))
    if expected_word is not None:
        call += '50610200517f' + word(expected_word) + '14'
    prefix = '61%04x61%04x600039' % (size, 9 + len(call) // 2 + 14) + call
    return ('0x' + prefix + '60%02x57' % (len(prefix) // 2 + 8)
            + '60006000fd5b60006000f3' + data)


def vectors():
    for at in BUILTINS:
        for selector in ['', '01', 'deadbeef'] + PROBES:
            yield 'unknown.%x.%s' % (at, selector), at, selector
    for selector in ['4420e486', 'd4b6b5da', 'a5025222', '8a186788', 'f6a455a2']:
        yield 'address_table.zero.' + selector, 0x66, selector + word(0)
    for selector in ['4420e486', 'd4b6b5da', 'a5025222', 'f6a455a2']:
        yield 'address_table.dirty.' + selector, 0x66, selector + word(((1 << 96) - 1) << 160)
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
        ('last_byte', decompress('80', offset=2**64 - 101, length=2**64 - 100)),
        ('long_zero', decompress('a1' + '00' * 33)),
        ('long_wrapping', decompress('a1ff' + '00' * 32)),
        ('long_literal', decompress('ff' + 'ff' * 95 + '00' * 12 + '11' * 20)),
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
        yield 'function_table.invalid.' + payload, 0x68, upload(payload)
    for name, data in [
        ('long_count', upload('a1' + '00' * 33)),
        ('long_gas', upload('010123456701a1' + '00' * 33)),
        ('sparse', upload('01', length=7)),
        ('bad_offset_sparse', upload('', length=1000, abi_offset=0)),
    ]:
        yield 'function_table.' + name, 0x68, data
    yield 'bls.register', 0x67, '375a7c7f' + word(0) * 4
    yield 'bls.getPublicKey', 0x67, '857cdbb8' + word(1)
    for selector, count in [('08df6923', 0), ('e3a0a148', 1), ('a901dd92', 1),
                            ('29854f47', 1), ('c6cabb40', 2), ('f4f4e136', 2),
                            ('f589445c', 2), ('8ea93643', 3)]:
        yield 'legacy.' + selector, 0x6b, selector + word(0) * count
        yield 'legacy.malformed.' + selector, 0x6b, selector + word(0) * count + '01'
    for at, data in [(0x6c, '41b247a8'), (0x6d, '875883f2'), (0x66, '949d225d'),
                     (0x6c, 'c6f7de0e01')]:
        yield 'private.' + data, at, data
    yield 'burn_zero', 0x69, 'bb3480f9' + word(0)
    # These valid debug methods reject a nonzero caller before private-state access.
    for selector, count in [('7b510fe8', 1), ('d56aa31f', 1), ('1d79f325', 2),
                            ('e30443bc', 2), ('978190f1', 3), ('afefabf7', 3), ('f0c26aff', 3)]:
        yield 'debug.unauthorized.' + selector, 0x69, selector + word(0) * count


def needs_context(name, height):
    return (name.startswith('private.')
            or name.startswith('legacy.') and not name.startswith('legacy.malformed.')
            or name.startswith('bls.') and not ARBOS34 <= height < ARBOS42
            or name in ['decompress.sparse_zero', 'decompress.last_byte', 'function_table.sparse']
            or name in ['function_table.long_count', 'function_table.long_gas'] and height < ARBOS39
            or name == 'function_table.bad_offset_sparse' and height < ARBOS54
            or name == 'sendTxToL1.bad_offset' and height < ARBOS54
            or name in ['sendTxToL1.truncated', 'sendTxToL1.oversized'] and height < ARBOS42)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--classic', required=True)
    parser.add_argument('--leafage', required=True)
    parser.add_argument('--heights', default='1,156076,156077,226010,226011,662404,662405,6835287,6835288,22207815')
    parser.add_argument('--report', required=True)
    args = parser.parse_args()
    records = []

    def record(name, height, ok, **details):
        records.append(dict(name=name, height=height, ok=ok, **details))
        if ok is False:
            print('FAIL', height, name, json.dumps(details)[:1500], flush=True)

    def compare(name, height, params, unavailable=False, leafage_params=None):
        reference = rpc(args.classic, 'eth_call', params)
        actual = rpc(args.leafage, 'eth_call', params if leafage_params is None else leafage_params)
        if unavailable:
            ok = 'requires unavailable Classic ArbOS' in actual.get('error', {}).get('message', '')
        elif 'result' in reference:
            ok = ('result' in actual if leafage_params is not None
                  else actual.get('result') == reference['result'])
        else:
            ok = all('revert' in r.get('error', {}).get('message', '').lower()
                     for r in (reference, actual))
        record(name, height, ok, expectation='capability' if unavailable else 'native',
               reference=reference, actual=actual)

    def compare_program(name, height, code, unavailable=False):
        request = dict(data=code, gas='0x989680', **{'from': SENDER})
        compare(name, height, [request, hex(height)], unavailable,
                [dict(request, to=PROBE, data='0x'), hex(height), {PROBE: dict(code=code)}])

    for height in [int(h) for h in args.heights.split(',')]:
        block = hex(height)
        overridden = rpc(args.classic, 'eth_call', [dict(to=PROBE), block,
                         {PROBE: dict(code='0x602a60005260206000f3')}])
        overrides_supported = overridden.get('result') == '0x' + word(42)
        for name, at, data in vectors():
            caller = address(0) if name.startswith('legacy.') else SENDER
            request = dict(to=address(at), data='0x' + data, gas='0x989680', **{'from': caller})
            unavailable = needs_context(name, height)
            if at != 0xc8:
                compare(name, height, [request, block], unavailable)
            else:
                record(name, height, None, skipped='Native RPC intercepts 0xc8 outside EVM execution')
            if not overrides_supported:
                record(name + '.nested', height, None, skipped='Native code override unavailable')
                continue
            # CALL allows stateful methods; it returns success + raw returndata.
            parent = ('0x36600060003760006000366000600060%02x5af1' % at
                      + '6000523d600060203e3d6020016000f3')
            compare(name + '.nested', height, [dict(request, to=PROBE), block, {PROBE: dict(code=parent)}], unavailable)
        # Creation as a boolean oracle also covers heights before native code
        # overrides existed. Leafage only executes ordinary overridden code.
        for at, data, unavailable in [
            (0x66, '4420e486' + word(0), False),
            (0x66, 'f6a455a2' + word(0), False),
            (0x67, '375a7c7f' + word(0) * 4, height < ARBOS34),
            (0x68, upload('80'), height < ARBOS34),
            (0x6d, '6e928a6e' + word(0), height < ARBOS34),
            (0x64, '25e16063' + word(0), False),
            (0x64, '928c169a' + word(0) + word(64) + word(0), False),
            (0xc8, 'deadbeef', False),
        ]:
            compare_program('static.' + data[:8], height,
                            builtin_program(at, data, static=True), unavailable)
        # Mini extracts retain the parent's entire memory buffer. The child
        # sees 132 calldata bytes, then reads the following 21 bytes anyway.
        memory = decompress('00' * 32, offset=32, length=53) + '94' + '11' * 20
        compare_program('decompress.parent_memory', height,
                        builtin_program(0x66, memory, input_size=132,
                                        expected_word=int('11' * 20, 16)), unavailable=True)

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
    capability = sum(r['ok'] is True and r.get('expectation') == 'capability' for r in records)
    print(json.dumps(dict(total=len(records), passed=len(records)-failed-skipped,
                          capability_checks=capability, failed=failed, skipped=skipped)))
    raise SystemExit(bool(failed))


if __name__ == '__main__':
    main()
