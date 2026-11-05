//! OP Mainnet pre-Bedrock (legacy l2geth, OVM 2.0) execution rules.
//!
//! Before Bedrock, OP Mainnet ran l2geth: Ethereum Istanbul plus an OVM change
//! set until [`OP_MAINNET_BERLIN_BLOCK`], then Berlin; it never had London. No
//! `OpSpecId` models that, so pre-Bedrock calls run on the Ethereum EVM with:
//! - Istanbul blocks charging the EIP-2929 cold cost on every state access, with
//!   no access list and nothing warm (l2geth `enableMinimal2929`);
//! - COINBASE as the OVM fee wallet and DIFFICULTY as zero (l2geth `core/evm.go`);
//! - CREATE limited to whitelisted deployers while the OVM deployer whitelist has
//!   an owner (l2geth `EVM.AddressWhitelisted`). The whitelist was retired for good
//!   (owner set to zero) at block 1,256,440, before Berlin, so only Istanbul
//!   blocks check it.
//!
//! L1BLOCKNUMBER (0x4B) returns the L1 block of each L2 block's transaction,
//! which is not stored, so it stays an invalid opcode.

use alloy_sol_types::{Revert, SolError};
use revm::bytecode::opcode;
use revm::handler::instructions::EthInstructions;
use revm::interpreter::instructions::contract;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::{InputsTr, ReturnData, RuntimeFlag};
use revm::interpreter::{
    as_usize_or_fail, gas, popn, push, require_non_staticcall, Host, Instruction,
    InstructionContext, InstructionExecResult, InstructionResult,
};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{address, keccak256, Address, Bytes, U256};
use revm::DatabaseRef;

/// First OP Mainnet block executed with Berlin rules by l2geth.
pub const OP_MAINNET_BERLIN_BLOCK: u64 = 3_950_000;
/// l2geth `dump.OvmFeeWallet`, the COINBASE of every pre-Bedrock block.
pub const OVM_FEE_WALLET: Address = address!("4200000000000000000000000000000000000011");
/// l2geth `dump.OvmWhitelistAddress` (OVM_DeployerWhitelist).
pub const OVM_DEPLOYER_WHITELIST: Address = address!("4200000000000000000000000000000000000002");

const COLD_SLOAD_COST: u16 = 2100;
const COLD_ACCOUNT_ACCESS_COST: u16 = 2600;
const SELFDESTRUCT_BASE_COST: u16 = 5000;

/// The Ethereum rules l2geth applied at a pre-Bedrock OP Mainnet block.
pub fn op_legacy_spec_at(block_number: u64) -> SpecId {
    if block_number >= OP_MAINNET_BERLIN_BLOCK {
        SpecId::BERLIN
    } else {
        SpecId::ISTANBUL
    }
}

/// The instruction table for `spec` (from [`op_legacy_spec_at`]). On Istanbul
/// blocks every SLOAD, account access and call pays the EIP-2929 cold cost
/// (`enableMinimal2929` keeps the Istanbul SSTORE rules), and CREATE / CREATE2
/// check the deployer whitelist.
pub fn op_legacy_instructions<H: Host>(spec: SpecId) -> EthInstructions<EthInterpreter, H> {
    let mut table = EthInstructions::new_mainnet_with_spec(spec);
    if spec.is_enabled_in(SpecId::BERLIN) {
        return table;
    }
    table.insert_gas(opcode::SLOAD, COLD_SLOAD_COST);
    for op in [
        opcode::BALANCE,
        opcode::EXTCODESIZE,
        opcode::EXTCODECOPY,
        opcode::EXTCODEHASH,
        opcode::CALL,
        opcode::CALLCODE,
        opcode::DELEGATECALL,
        opcode::STATICCALL,
    ] {
        table.insert_gas(op, COLD_ACCOUNT_ACCESS_COST);
    }
    table.insert_gas(
        opcode::SELFDESTRUCT,
        SELFDESTRUCT_BASE_COST + COLD_ACCOUNT_ACCESS_COST,
    );
    let create = table.gas_table()[opcode::CREATE as usize];
    table.insert_instruction(
        opcode::CREATE,
        Instruction::new(create_whitelisted::<false, H>),
        create,
    );
    let create2 = table.gas_table()[opcode::CREATE2 as usize];
    table.insert_instruction(
        opcode::CREATE2,
        Instruction::new(create_whitelisted::<true, H>),
        create2,
    );
    table
}

/// Storage key of `whitelist[deployer]` (mapping at slot 1).
fn whitelist_key(deployer: Address) -> U256 {
    let mut preimage = [0u8; 64];
    preimage[12..32].copy_from_slice(deployer.as_slice());
    preimage[63] = 1;
    keccak256(preimage).into()
}

/// l2geth `AddressWhitelisted`: an owner of zero (slot 0) disables the whitelist.
/// `sload` reads a whitelist storage slot; `None` is a database failure.
fn deployer_allowed(
    mut sload: impl FnMut(U256) -> Option<U256>,
    deployer: Address,
) -> Option<bool> {
    if sload(U256::ZERO)?.is_zero() {
        return Some(true);
    }
    Some(!sload(whitelist_key(deployer))?.is_zero())
}

/// Whether `deployer` may create a contract at the state behind `db`.
pub fn deployer_allowed_ref<DB: DatabaseRef>(
    db: &DB,
    deployer: Address,
) -> Result<bool, DB::Error> {
    let mut error = None;
    let allowed = deployer_allowed(
        |key| match db.storage_ref(OVM_DEPLOYER_WHITELIST, key) {
            Ok(value) => Some(value),
            Err(e) => {
                error = Some(e);
                None
            }
        },
        deployer,
    );
    match (allowed, error) {
        (_, Some(e)) => Err(e),
        (allowed, None) => Ok(allowed.unwrap_or(false)),
    }
}

/// The revert data of a rejected deployment (l2geth `util.EncodeSolidityError`).
pub fn not_whitelisted_output(deployer: Address) -> Bytes {
    Revert::from(format!(
        "deployer address not whitelisted: {}",
        deployer.to_checksum(None)
    ))
    .abi_encode()
    .into()
}

/// CREATE / CREATE2 behind the deployer whitelist. l2geth rejects a deployer
/// inside `evm.create`: the opcode's own gas (memory, CREATE2 hashing) is spent,
/// the forwarded gas comes back, 0 is pushed and the Solidity error becomes the
/// return data.
fn create_whitelisted<const IS_CREATE2: bool, H: Host + ?Sized>(
    ctx: InstructionContext<'_, H, EthInterpreter>,
) -> InstructionExecResult {
    let deployer = ctx.interpreter.input.target_address();
    let allowed = deployer_allowed(
        |key| {
            ctx.host
                .sload(OVM_DEPLOYER_WHITELIST, key)
                .map(|slot| slot.data)
        },
        deployer,
    )
    .ok_or(InstructionResult::FatalExternalError)?;
    if allowed {
        return contract::create::<IS_CREATE2, EthInterpreter, H>(ctx);
    }
    require_non_staticcall!(ctx.interpreter);
    popn!([_value, code_offset, len], ctx.interpreter);
    let len = as_usize_or_fail!(ctx.interpreter, len);
    if len != 0 {
        let code_offset = as_usize_or_fail!(ctx.interpreter, code_offset);
        ctx.interpreter
            .resize_memory(ctx.host.gas_params(), code_offset, len)?;
    }
    if IS_CREATE2 {
        popn!([_salt], ctx.interpreter);
        gas!(ctx.interpreter, ctx.host.gas_params().create2_cost(len));
    } else {
        gas!(ctx.interpreter, ctx.host.gas_params().create_cost());
    }
    ctx.interpreter
        .return_data
        .set_buffer(not_whitelisted_output(deployer));
    push!(ctx.interpreter, U256::ZERO);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn berlin_starts_at_its_block() {
        assert_eq!(op_legacy_spec_at(0), SpecId::ISTANBUL);
        assert_eq!(
            op_legacy_spec_at(OP_MAINNET_BERLIN_BLOCK - 1),
            SpecId::ISTANBUL
        );
        assert_eq!(op_legacy_spec_at(OP_MAINNET_BERLIN_BLOCK), SpecId::BERLIN);
    }

    #[test]
    fn whitelist_reads_owner_then_mapping() {
        let deployer = address!("1111111111111111111111111111111111111111");
        let owner = U256::from(0xabcu64);
        // No owner: anyone may deploy, the mapping is not read.
        assert_eq!(
            deployer_allowed(|key| (key == U256::ZERO).then_some(U256::ZERO), deployer),
            Some(true)
        );
        // Owner set: the mapping decides.
        let listed = |value: u64| {
            move |key: U256| {
                Some(if key == U256::ZERO {
                    owner
                } else {
                    assert_eq!(key, whitelist_key(deployer));
                    U256::from(value)
                })
            }
        };
        assert_eq!(deployer_allowed(listed(0), deployer), Some(false));
        assert_eq!(deployer_allowed(listed(1), deployer), Some(true));
        // A database failure is not a decision.
        assert_eq!(deployer_allowed(|_| None, deployer), None);
    }

    #[test]
    fn rejection_is_a_solidity_error_with_the_checksummed_deployer() {
        let deployer = address!("1111111111111111111111111111111111111111");
        let output = not_whitelisted_output(deployer);
        assert_eq!(&output[..4], &[0x08, 0xc3, 0x79, 0xa0]);
        let reason = Revert::abi_decode(&output).unwrap().reason;
        assert_eq!(
            reason,
            "deployer address not whitelisted: 0x1111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn istanbul_table_charges_cold_costs() {
        type Ctx = revm::Context;
        let istanbul = op_legacy_instructions::<Ctx>(SpecId::ISTANBUL);
        let gas = istanbul.gas_table();
        assert_eq!(gas[opcode::SLOAD as usize], 2100);
        for op in [
            opcode::BALANCE,
            opcode::EXTCODEHASH,
            opcode::CALL,
            opcode::STATICCALL,
        ] {
            assert_eq!(gas[op as usize], 2600);
        }
        assert_eq!(gas[opcode::SELFDESTRUCT as usize], 7600);
        // SSTORE keeps its Istanbul (EIP-2200) static part.
        let plain = EthInstructions::<EthInterpreter, Ctx>::new_mainnet_with_spec(SpecId::ISTANBUL);
        assert_eq!(
            gas[opcode::SSTORE as usize],
            plain.gas_table()[opcode::SSTORE as usize]
        );
        // Berlin is the plain Berlin table.
        let berlin = op_legacy_instructions::<Ctx>(SpecId::BERLIN);
        let plain_berlin =
            EthInstructions::<EthInterpreter, Ctx>::new_mainnet_with_spec(SpecId::BERLIN);
        assert_eq!(berlin.gas_table(), plain_berlin.gas_table());
    }
}
