//! Mantle (chain 5000) execution rules after its Bedrock migration ("Tectonic").
//!
//! Mantle is an OP Stack chain with its own fork line. From mantle op-geth
//! (`params/mantle.go`, `core/genesis.go`, `core/vm`, `core/state_transition.go`):
//!
//! | fork     | activation                 | rules                                                 |
//! |----------|----------------------------|-------------------------------------------------------|
//! | Tectonic | block 61,171,946 (Bedrock) | Merge, plus PUSH0 and EIP-3860 initcode gas in CREATE |
//! | Everest  | timestamp 1,742,367,600    | adds P256VERIFY (RIP-7212, 3450 gas)                  |
//! | Skadi    | timestamp 1,756,278,000    | Shanghai, Cancun and Prague at once                   |
//! | Limb     | timestamp 1,768,374,000    | Osaka                                                 |
//! | Arsia    | timestamp 1,776,841,200    | OP Canyon..Jovian at once; tokenRatio gas removed     |
//!
//! Before Arsia no `OpSpecId` models a block: it runs on the Ethereum EVM with
//! - the Shanghai instruction table since Bedrock (`IsMantleBaseFee`) while the
//!   Shanghai rules themselves wait for Skadi: PUSH0 works and CREATE / CREATE2
//!   charge the EIP-3860 initcode gas, but COINBASE is not warm, a deployment
//!   transaction pays no initcode gas and no initcode size limit applies;
//! - Mantle's own precompile sets ([`MantleFork::precompiles`]);
//! - gas counted in `GasOracle.tokenRatio` units: the intrinsic gas and the
//!   EIP-7623 floor are multiplied by the ratio, the L1 data fee is taken out of
//!   the gas limit as `fee / gasPrice`, the rest divided by the ratio is what the
//!   EVM gets, and the unused part is multiplied back ([`MantleRun`]).
//!
//! From Arsia on a block runs the OP EVM; EIP-7825 never applies (op-geth skips
//! it on OP Stack chains) and the precompiles stay Ethereum's Osaka set, without
//! the OP input-size limits.

use crate::mantle::{GAS_ORACLE_ADDR, TOKEN_RATIO_SLOT};
use revm::bytecode::opcode;
use revm::context::result::ResultGas;
use revm::context::TxEnv;
use revm::context_interface::cfg::gas::calculate_initial_tx_gas_for_tx;
use revm::context_interface::cfg::{GasId, GasParams};
use revm::handler::instructions::EthInstructions;
use revm::interpreter::instructions::contract;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::RuntimeFlag;
use revm::interpreter::{
    as_usize_or_fail, gas, push, require_non_staticcall, Host, Instruction, InstructionContext,
    InstructionExecResult,
};
use revm::precompile::{secp256r1, Precompiles};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{address, Address, TxKind, U256};
use revm::DatabaseRef;
use std::sync::OnceLock;

/// Mantle mainnet chain id.
pub const MANTLE_CHAIN_ID: u64 = 5000;
/// First Bedrock ("Tectonic") block.
pub const MANTLE_BEDROCK_BLOCK: u64 = 61_171_946;
/// Everest activation timestamp (first block 77,118,644).
pub const MANTLE_EVEREST_TIMESTAMP: u64 = 1_742_367_600;
/// Skadi activation timestamp (first block 84,073,844).
pub const MANTLE_SKADI_TIMESTAMP: u64 = 1_756_278_000;
/// Limb activation timestamp (first block 90,121,844).
pub const MANTLE_LIMB_TIMESTAMP: u64 = 1_768_374_000;
/// Arsia activation timestamp (first block 94,355,444).
pub const MANTLE_ARSIA_TIMESTAMP: u64 = 1_776_841_200;

/// The L1Block predeploy: L1 base fee, fee overhead and fee scalar.
pub const L1_BLOCK_ADDR: Address = address!("4200000000000000000000000000000000000015");
const L1_BASE_FEE_SLOT: u64 = 1;
const L1_FEE_OVERHEAD_SLOT: u64 = 5;
const L1_FEE_SCALAR_SLOT: u64 = 6;
/// Decimals of the L1 fee scalar.
const L1_FEE_SCALAR_DIVISOR: u64 = 1_000_000;

/// EIP-3529 refund cap: a fifth of the gas used.
const REFUND_QUOTIENT: u64 = 5;
/// Static gas of PUSH0.
const PUSH0_GAS: u16 = 2;

/// A Mantle fork after the Bedrock migration, in activation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MantleFork {
    /// Bedrock: London / Merge rules with PUSH0.
    Tectonic,
    /// Adds the P256VERIFY precompile.
    Everest,
    /// Shanghai, Cancun and Prague.
    Skadi,
    /// Osaka.
    Limb,
    /// The OP forks up to Jovian; gas is no longer scaled by the token ratio.
    Arsia,
}

/// The fork a Mantle block runs, `None` before Bedrock.
pub fn mantle_fork_at(block_number: u64, timestamp: u64) -> Option<MantleFork> {
    if block_number < MANTLE_BEDROCK_BLOCK {
        return None;
    }
    Some(if timestamp >= MANTLE_ARSIA_TIMESTAMP {
        MantleFork::Arsia
    } else if timestamp >= MANTLE_LIMB_TIMESTAMP {
        MantleFork::Limb
    } else if timestamp >= MANTLE_SKADI_TIMESTAMP {
        MantleFork::Skadi
    } else if timestamp >= MANTLE_EVEREST_TIMESTAMP {
        MantleFork::Everest
    } else {
        MantleFork::Tectonic
    })
}

impl MantleFork {
    /// The Ethereum fork whose EVM rules the block runs.
    pub fn eth_spec(self) -> SpecId {
        match self {
            Self::Tectonic | Self::Everest => SpecId::MERGE,
            Self::Skadi => SpecId::PRAGUE,
            Self::Limb | Self::Arsia => SpecId::OSAKA,
        }
    }

    /// Whether gas is counted in token-ratio units (every fork before Arsia).
    pub fn scales_gas(self) -> bool {
        self < Self::Arsia
    }

    /// The gas parameters of the EVM run. Before Arsia the refund quotient is 1,
    /// so the result carries the whole refund counter and [`MantleRun::settle`]
    /// applies Mantle's own cap.
    pub fn gas_params(self) -> GasParams {
        let mut params = GasParams::new_spec(self.eth_spec());
        if self.scales_gas() {
            params.override_gas([(GasId::max_refund_quotient(), 1)]);
        }
        params
    }

    /// The precompiles of the fork (op-geth `PrecompiledContractsMantle*`). None
    /// of them has the OP input-size limits.
    pub fn precompiles(self) -> &'static Precompiles {
        static EVEREST: OnceLock<Precompiles> = OnceLock::new();
        static SKADI: OnceLock<Precompiles> = OnceLock::new();
        match self {
            Self::Tectonic => Precompiles::berlin(),
            Self::Everest => EVEREST.get_or_init(|| with_p256verify(Precompiles::berlin())),
            Self::Skadi => SKADI.get_or_init(|| with_p256verify(Precompiles::prague())),
            Self::Limb | Self::Arsia => Precompiles::osaka(),
        }
    }

    /// The instruction table of a pre-Arsia block. Before Skadi it is the Merge
    /// table plus PUSH0 and the EIP-3860 initcode gas on CREATE / CREATE2.
    pub fn instructions<H: Host>(self) -> EthInstructions<EthInterpreter, H> {
        let mut table = EthInstructions::new_mainnet_with_spec(self.eth_spec());
        if self >= Self::Skadi {
            return table;
        }
        table.insert_instruction(opcode::PUSH0, Instruction::new(push0::<H>), PUSH0_GAS);
        let create = table.gas_table()[opcode::CREATE as usize];
        table.insert_instruction(
            opcode::CREATE,
            Instruction::new(create_with_initcode_gas::<false, H>),
            create,
        );
        let create2 = table.gas_table()[opcode::CREATE2 as usize];
        table.insert_instruction(
            opcode::CREATE2,
            Instruction::new(create_with_initcode_gas::<true, H>),
            create2,
        );
        table
    }
}

/// `base` plus P256VERIFY at its RIP-7212 price.
fn with_p256verify(base: &Precompiles) -> Precompiles {
    let mut precompiles = base.clone();
    precompiles.extend([secp256r1::P256VERIFY]);
    precompiles
}

/// PUSH0 without the Shanghai check.
fn push0<H: Host + ?Sized>(
    ctx: InstructionContext<'_, H, EthInterpreter>,
) -> InstructionExecResult {
    push!(ctx.interpreter, U256::ZERO);
    Ok(())
}

/// CREATE / CREATE2 charging the EIP-3860 initcode gas before the Merge rules
/// run (op-geth `gasCreateEip3860`; its size limit waits for Shanghai).
fn create_with_initcode_gas<const IS_CREATE2: bool, H: Host + ?Sized>(
    ctx: InstructionContext<'_, H, EthInterpreter>,
) -> InstructionExecResult {
    require_non_staticcall!(ctx.interpreter);
    // Stack: value, offset, length. A short stack fails in `create`.
    if let Ok(len) = ctx.interpreter.stack.peek(2) {
        let len = as_usize_or_fail!(ctx.interpreter, len);
        gas!(ctx.interpreter, ctx.host.gas_params().initcode_cost(len));
    }
    contract::create::<IS_CREATE2, EthInterpreter, H>(ctx)
}

/// `GasOracle.tokenRatio`: chain gas per EVM gas before Arsia.
pub fn token_ratio<DB: DatabaseRef>(db: &DB) -> Result<u64, DB::Error> {
    let ratio = db.storage_ref(GAS_ORACLE_ADDR, TOKEN_RATIO_SLOT)?;
    Ok(ratio.saturating_to::<u64>().max(1))
}

/// Why a pre-Arsia gas limit cannot start the transaction; `required` is the
/// chain gas the limit has to reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MantleGasError {
    /// Below the intrinsic gas times the token ratio.
    Intrinsic { required: u64 },
    /// Below the EIP-7623 floor times the token ratio (Skadi and later).
    Floor { required: u64 },
    /// The L1 data fee does not fit in what the intrinsic gas left.
    L1Fee { required: u64 },
}

/// The gas accounting of one pre-Arsia transaction (op-geth `innerExecute`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MantleRun {
    fork: MantleFork,
    ratio: u64,
    /// The transaction's gas limit, in chain gas.
    gas_limit: u64,
    /// EIP-7623 floor in EVM gas, zero before Skadi.
    floor: u64,
    /// The gas limit of the EVM run: intrinsic gas plus the execution gas.
    pub evm_gas_limit: u64,
}

impl MantleRun {
    /// Splits `gas_limit` chain gas: `intrinsic * ratio` and `l1_gas` come off,
    /// and the EVM executes with the rest divided by the ratio.
    pub fn new(
        fork: MantleFork,
        ratio: u64,
        gas_limit: u64,
        intrinsic: u64,
        floor: u64,
        l1_gas: u64,
    ) -> Result<Self, MantleGasError> {
        let ratio = ratio.max(1);
        let floor = if fork >= MantleFork::Skadi { floor } else { 0 };
        let intrinsic_scaled = intrinsic.saturating_mul(ratio);
        if gas_limit < intrinsic_scaled {
            return Err(MantleGasError::Intrinsic {
                required: intrinsic_scaled,
            });
        }
        let floor_scaled = floor.saturating_mul(ratio);
        if gas_limit < floor_scaled {
            return Err(MantleGasError::Floor {
                required: floor_scaled,
            });
        }
        let left = gas_limit - intrinsic_scaled;
        if left < l1_gas {
            return Err(MantleGasError::L1Fee {
                required: intrinsic_scaled.saturating_add(l1_gas),
            });
        }
        Ok(Self {
            fork,
            ratio,
            gas_limit,
            floor,
            evm_gas_limit: intrinsic + (left - l1_gas) / ratio,
        })
    }

    /// The plan for `tx`, whose gas limit is in chain gas. `l1_gas_price` prices
    /// the L1 data fee in gas; zero leaves it out, as op-geth does for `eth_call`.
    /// The outer error is a failed state read.
    pub fn for_tx<DB: DatabaseRef>(
        fork: MantleFork,
        db: &DB,
        tx: &TxEnv,
        chain_id: u64,
        l1_gas_price: u128,
    ) -> Result<Result<Self, MantleGasError>, DB::Error> {
        let ratio = token_ratio(db)?;
        let l1_gas = l1_fee_gas(db, tx, chain_id, ratio, l1_gas_price)?;
        let initial = calculate_initial_tx_gas_for_tx(tx, fork.eth_spec(), None);
        Ok(Self::new(
            fork,
            ratio,
            tx.gas_limit,
            initial.initial_regular_gas,
            initial.floor_gas,
            l1_gas,
        ))
    }

    /// Converts the gas of the EVM run (limit [`Self::evm_gas_limit`], refund
    /// quotient 1, no EIP-7623 floor) into the transaction's chain gas.
    ///
    /// The refund cap is a fifth of `gas_limit / ratio - left` from Skadi on;
    /// before Skadi op-geth took the unscaled `gas_limit - left`, which leaves
    /// the refund counter practically uncapped.
    pub fn settle(&self, evm_gas: ResultGas) -> ResultGas {
        let left = self.evm_gas_limit.saturating_sub(evm_gas.total_gas_spent());
        let used_for_refund = if self.fork >= MantleFork::Skadi {
            (self.gas_limit / self.ratio).saturating_sub(left)
        } else {
            self.gas_limit.saturating_sub(left)
        };
        let refund = evm_gas
            .inner_refunded()
            .min(used_for_refund / REFUND_QUOTIENT);
        ResultGas::default()
            .with_total_gas_spent(
                self.gas_limit
                    .saturating_sub(left.saturating_mul(self.ratio)),
            )
            .with_refunded(refund.saturating_mul(self.ratio))
            .with_floor_gas(self.floor.saturating_mul(self.ratio))
    }
}

/// The pre-Arsia L1 data fee in wei:
/// `(data_gas + overhead) * l1_base_fee * scalar * ratio / 1e6`.
pub fn l1_fee(data_gas: u64, overhead: U256, l1_base_fee: U256, scalar: U256, ratio: u64) -> U256 {
    U256::from(data_gas)
        .saturating_add(overhead)
        .saturating_mul(l1_base_fee)
        .saturating_mul(scalar)
        .saturating_mul(U256::from(ratio))
        / U256::from(L1_FEE_SCALAR_DIVISOR)
}

/// The gas the L1 data fee of `tx` takes out of its gas limit before Arsia:
/// the fee at the block's L1 parameters divided by `gas_price`, zero when the
/// price is zero.
pub fn l1_fee_gas<DB: DatabaseRef>(
    db: &DB,
    tx: &TxEnv,
    chain_id: u64,
    ratio: u64,
    gas_price: u128,
) -> Result<u64, DB::Error> {
    if gas_price == 0 {
        return Ok(0);
    }
    let slot = |index: u64| db.storage_ref(L1_BLOCK_ADDR, U256::from(index));
    let fee = l1_fee(
        signed_tx_data_gas(tx, chain_id),
        slot(L1_FEE_OVERHEAD_SLOT)?,
        slot(L1_BASE_FEE_SLOT)?,
        slot(L1_FEE_SCALAR_SLOT)?,
        ratio,
    );
    Ok((fee / U256::from(gas_price)).saturating_to::<u64>())
}

/// Regolith L1 data gas of serialized transaction bytes: 4 per zero byte, 16 per
/// non-zero byte.
pub fn rollup_data_gas(bytes: &[u8]) -> u64 {
    let zeros = bytes.iter().filter(|byte| **byte == 0).count() as u64;
    zeros * 4 + (bytes.len() as u64 - zeros) * 16
}

/// The L1 data gas `tx` would have once signed. The request carries no
/// signature, so `r` and `s` count as 32 non-zero bytes each; a legacy request
/// is encoded as a legacy transaction, any other as EIP-1559.
pub fn signed_tx_data_gas(tx: &TxEnv, chain_id: u64) -> u64 {
    rollup_data_gas(&signed_tx_bytes(tx, chain_id))
}

fn signed_tx_bytes(tx: &TxEnv, chain_id: u64) -> Vec<u8> {
    let legacy = tx.tx_type == 0;
    let mut fields = Vec::with_capacity(tx.data.len() + 192);
    if !legacy {
        rlp_uint(&mut fields, chain_id as u128);
    }
    rlp_uint(&mut fields, tx.nonce as u128);
    if !legacy {
        rlp_uint(&mut fields, tx.gas_priority_fee.unwrap_or(tx.gas_price));
    }
    rlp_uint(&mut fields, tx.gas_price);
    rlp_uint(&mut fields, tx.gas_limit as u128);
    match tx.kind {
        TxKind::Call(to) => rlp_bytes(&mut fields, to.as_slice()),
        TxKind::Create => rlp_bytes(&mut fields, &[]),
    }
    let value = tx.value.to_be_bytes::<32>();
    rlp_bytes(&mut fields, trim_leading_zeros(&value));
    rlp_bytes(&mut fields, &tx.data);
    if !legacy {
        let mut items = Vec::new();
        for item in tx.access_list.0.iter() {
            let mut keys = Vec::with_capacity(item.storage_keys.len() * 33);
            for key in item.storage_keys.iter() {
                rlp_bytes(&mut keys, key.as_slice());
            }
            let mut entry = Vec::with_capacity(keys.len() + 24);
            rlp_bytes(&mut entry, item.address.as_slice());
            rlp_list(&mut entry, &keys);
            rlp_list(&mut items, &entry);
        }
        rlp_list(&mut fields, &items);
    }
    // v (EIP-155 for legacy, y parity otherwise), r, s.
    rlp_uint(
        &mut fields,
        if legacy { chain_id as u128 * 2 + 35 } else { 1 },
    );
    rlp_bytes(&mut fields, &[0xff; 32]);
    rlp_bytes(&mut fields, &[0xff; 32]);

    let mut out = Vec::with_capacity(fields.len() + 4);
    if !legacy {
        out.push(0x02);
    }
    rlp_list(&mut out, &fields);
    out
}

fn trim_leading_zeros(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn rlp_length(out: &mut Vec<u8>, len: usize, short_base: u8) {
    if len < 56 {
        out.push(short_base + len as u8);
    } else {
        let be = (len as u64).to_be_bytes();
        let be = trim_leading_zeros(&be);
        out.push(short_base + 55 + be.len() as u8);
        out.extend_from_slice(be);
    }
}

fn rlp_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        out.push(bytes[0]);
    } else {
        rlp_length(out, bytes.len(), 0x80);
        out.extend_from_slice(bytes);
    }
}

fn rlp_uint(out: &mut Vec<u8>, value: u128) {
    rlp_bytes(out, trim_leading_zeros(&value.to_be_bytes()));
}

fn rlp_list(out: &mut Vec<u8>, payload: &[u8]) {
    rlp_length(out, payload.len(), 0xc0);
    out.extend_from_slice(payload);
}
