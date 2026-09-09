// SPDX-License-Identifier: MIT
pragma solidity ^0.8.35;

// Compile for Osaka and execute the creation bytecode through read-only RPC.
// No transaction needs to be broadcast to exercise the nested calls.
contract TempoNestedChild {
    uint256 public value;
    event Written(uint256 value);
    error ChildFailure(uint256 value);

    function write(uint256 next, bool fail) external returns (uint256) {
        value = next;
        emit Written(next);
        if (fail) revert ChildFailure(next);
        return next;
    }
}

contract TempoNestedProbe {
    // Same slot as the child, to observe DELEGATECALL writes independently.
    uint256 public value;
    event Step(uint256 index);
    error OuterFailure();

    // 0: CALL success; 1: caught child revert; 2: outer revert;
    // 3: DELEGATECALL; 4: caught child OOG; 5: forbidden write in STATICCALL;
    // 6: nested Tempo TIP-20 precompile read.
    constructor(uint256 mode) {
        require(mode <= 6, "unknown mode");
        emit Step(0);
        TempoNestedChild child = new TempoNestedChild();
        emit Step(1);

        if (mode == 5) {
            (bool ok,) = address(child).staticcall(
                abi.encodeCall(child.write, (uint256(7), false))
            );
            require(!ok, "static write succeeded");
            emit Step(3);
        } else {
            uint256 callGas = mode == 4 ? 2300 : 2_000_000;
            try child.write{gas: callGas}(7, mode == 1 || mode == 2) {
                emit Step(2);
            } catch {
                emit Step(3);
                if (mode == 2) revert OuterFailure();
            }
        }

        if (mode == 3) {
            (bool ok,) = address(child).delegatecall(
                abi.encodeCall(child.write, (uint256(9), false))
            );
            require(ok && value == 9, "delegate write failed");
        }
        if (mode == 6) {
            (bool ok, bytes memory result) =
                address(0x20C0000000000000000000000000000000000000).staticcall(
                    abi.encodeWithSignature("decimals()")
                );
            require(ok && abi.decode(result, (uint256)) == 6, "TIP20 read failed");
        }

        uint256 expected = mode == 1 || mode == 4 || mode == 5 ? 0 : 7;
        require(child.value() == expected, "child state mismatch");
        emit Step(4);
    }
}
