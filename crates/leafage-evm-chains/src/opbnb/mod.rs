//! opBNB mainnet (chain 204): its hardfork schedule and the precompiles each
//! block runs with, as `bnb-chain/op-geth` defines them (`params/config.go`
//! `OPBNBMainNetConfig`, `core/vm/contracts.go`).
//!
//! opBNB is an OP-stack chain with Regolith active from genesis. Its execution
//! follows the OP forks up to Fjord, plus three opBNB forks:
//! - Fermat (block 9,397,477) adds the BLS signature (0x66) and cometBFT light
//!   block (0x67) precompiles of BSC;
//! - Haber (shortly after Ecotone) adds P256VERIFY (0x100), which the OP stack
//!   only adds at Fjord;
//! - Wright makes the base fee zero and exempts a transaction whose effective
//!   gas price is zero from the L1 data fee (see [`is_gasless`]).
//!
//! Granite and later OP forks are not configured on opBNB. The Volta and
//! Fourier block-interval changes only affect how op-node builds blocks: the
//! EVM keeps second timestamps and reads PREVRANDAO from the header's mix
//! hash, whose first bytes hold the milliseconds.

use once_cell::race::OnceBox;
use op_revm::OpSpecId;
use revm::{
    context::{Cfg, ContextTr},
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, InterpreterResult},
    precompile::{secp256r1, Precompiles},
    primitives::{hardfork::SpecId, Address, AddressSet},
};
use std::boxed::Box;

use crate::bsc::precompile::{BLS_SIGNATURE_VALIDATION, COMETBFT_LIGHT_BLOCK_VALIDATION};

pub const OPBNB_MAINNET_CHAIN_ID: u64 = 204;

/// opBNB Fermat, by block number.
pub const OPBNB_MAINNET_FERMAT_BLOCK: u64 = 9_397_477;
/// Shanghai and Canyon (2024-06-20 08:00 UTC).
pub const OPBNB_MAINNET_CANYON_TIMESTAMP: u64 = 1_718_870_400;
/// Cancun and Ecotone (2024-06-20 08:20 UTC).
pub const OPBNB_MAINNET_ECOTONE_TIMESTAMP: u64 = 1_718_871_600;
/// opBNB Haber (2024-06-20 08:30 UTC).
pub const OPBNB_MAINNET_HABER_TIMESTAMP: u64 = 1_718_872_200;
/// opBNB Wright (2024-08-27 06:00 UTC).
pub const OPBNB_MAINNET_WRIGHT_TIMESTAMP: u64 = 1_724_738_400;
/// Fjord (2024-09-24 06:00 UTC), the last OP fork opBNB runs.
pub const OPBNB_MAINNET_FJORD_TIMESTAMP: u64 = 1_727_157_600;
/// The first block at the Cancun timestamp: from it on SELFDESTRUCT no longer
/// destroys a contract created earlier (EIP-6780).
pub const OPBNB_MAINNET_CANCUN_BLOCK: u64 = 27_117_877;
/// The create2 deployer predeploy, whose code op-geth sets in place at the
/// Canyon activation.
pub const OPBNB_MAINNET_CREATE2_DEPLOYER: [u8; 20] = [
    0x13, 0xb0, 0xd8, 0x5c, 0xcb, 0x8b, 0xf8, 0x60, 0xb6, 0xb7, 0x9a, 0xf3, 0x02, 0x9f, 0xca, 0x08,
    0x1a, 0xe9, 0xbe, 0xf2,
];

/// The OP fork rules of a block at `timestamp`. opBNB stops at Fjord.
pub fn opbnb_mainnet_spec_at(timestamp: u64) -> OpSpecId {
    if timestamp >= OPBNB_MAINNET_FJORD_TIMESTAMP {
        OpSpecId::FJORD
    } else if timestamp >= OPBNB_MAINNET_ECOTONE_TIMESTAMP {
        OpSpecId::ECOTONE
    } else if timestamp >= OPBNB_MAINNET_CANYON_TIMESTAMP {
        OpSpecId::CANYON
    } else {
        OpSpecId::REGOLITH
    }
}

pub fn is_fermat_active(number: u64) -> bool {
    number >= OPBNB_MAINNET_FERMAT_BLOCK
}

pub fn is_haber_active(timestamp: u64) -> bool {
    timestamp >= OPBNB_MAINNET_HABER_TIMESTAMP
}

pub fn is_wright_active(timestamp: u64) -> bool {
    timestamp >= OPBNB_MAINNET_WRIGHT_TIMESTAMP
}

/// op-geth sets the create2 deployer code in the one block whose timestamp is
/// the Canyon activation (`consensus/misc/create2deployer.go`).
pub fn is_canyon_activation_block(timestamp: u64) -> bool {
    timestamp == OPBNB_MAINNET_CANYON_TIMESTAMP
}

/// A non-deposit transaction that pays no L1 data fee: from Wright on, one
/// whose effective gas price is zero (`StateTransition.buyGas`; its L1 fee
/// vault credit is zero too).
pub fn is_gasless(timestamp: u64, effective_gas_price: u128) -> bool {
    effective_gas_price == 0 && is_wright_active(timestamp)
}

/// The precompiles of the block `(number, timestamp)`.
pub fn opbnb_mainnet_precompiles(number: u64, timestamp: u64) -> &'static Precompiles {
    if is_haber_active(timestamp) {
        haber()
    } else if timestamp >= OPBNB_MAINNET_ECOTONE_TIMESTAMP {
        cancun()
    } else if is_fermat_active(number) {
        fermat()
    } else {
        Precompiles::berlin()
    }
}

/// Berlin plus the BSC light-client precompiles.
fn fermat() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = Precompiles::berlin().clone();
        precompiles.extend([BLS_SIGNATURE_VALIDATION, COMETBFT_LIGHT_BLOCK_VALIDATION]);
        Box::new(precompiles)
    })
}

/// Cancun (KZG point evaluation) plus the BSC light-client precompiles.
fn cancun() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = Precompiles::cancun().clone();
        precompiles.extend([BLS_SIGNATURE_VALIDATION, COMETBFT_LIGHT_BLOCK_VALIDATION]);
        Box::new(precompiles)
    })
}

/// Haber and later: Cancun, the light-client precompiles and P256VERIFY at
/// its RIP-7212 price. Fjord adds nothing more on opBNB.
fn haber() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = cancun().clone();
        precompiles.extend([secp256r1::P256VERIFY]);
        Box::new(precompiles)
    })
}

/// The precompiles of one opBNB block. Which ones exist depends on the block
/// number (Fermat) and timestamp (Haber), not only on the OP spec, so the set
/// is fixed when the provider is made and `set_spec` keeps it.
#[derive(Debug, Clone)]
pub struct OpBnbPrecompiles {
    inner: EthPrecompiles,
}

impl OpBnbPrecompiles {
    pub fn new(number: u64, timestamp: u64) -> Self {
        let spec: SpecId = opbnb_mainnet_spec_at(timestamp).into();
        Self {
            inner: EthPrecompiles {
                precompiles: opbnb_mainnet_precompiles(number, timestamp),
                spec,
            },
        }
    }

    pub fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }
}

impl<CTX> PrecompileProvider<CTX> for OpBnbPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, _spec: OpSpecId) -> bool {
        false
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        PrecompileProvider::<CTX>::run(&mut self.inner, context, inputs)
    }

    fn warm_addresses(&self) -> &AddressSet {
        self.inner.precompiles.addresses_set()
    }

    fn contains(&self, address: &Address) -> bool {
        self.inner.precompiles.contains(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::precompile::u64_to_address;

    #[test]
    fn spec_follows_the_op_forks_up_to_fjord() {
        assert_eq!(opbnb_mainnet_spec_at(0), OpSpecId::REGOLITH);
        assert_eq!(
            opbnb_mainnet_spec_at(OPBNB_MAINNET_CANYON_TIMESTAMP - 1),
            OpSpecId::REGOLITH
        );
        assert_eq!(
            opbnb_mainnet_spec_at(OPBNB_MAINNET_CANYON_TIMESTAMP),
            OpSpecId::CANYON
        );
        assert_eq!(
            opbnb_mainnet_spec_at(OPBNB_MAINNET_ECOTONE_TIMESTAMP),
            OpSpecId::ECOTONE
        );
        assert_eq!(
            opbnb_mainnet_spec_at(OPBNB_MAINNET_HABER_TIMESTAMP),
            OpSpecId::ECOTONE
        );
        assert_eq!(
            opbnb_mainnet_spec_at(OPBNB_MAINNET_WRIGHT_TIMESTAMP),
            OpSpecId::ECOTONE
        );
        assert_eq!(
            opbnb_mainnet_spec_at(OPBNB_MAINNET_FJORD_TIMESTAMP),
            OpSpecId::FJORD
        );
        assert_eq!(opbnb_mainnet_spec_at(u64::MAX), OpSpecId::FJORD);
    }

    #[test]
    fn precompiles_per_era() {
        let bls = u64_to_address(0x66);
        let cometbft = u64_to_address(0x67);
        let kzg = u64_to_address(0x0a);
        let p256 = u64_to_address(0x100);

        let genesis = opbnb_mainnet_precompiles(1, 1_691_753_724);
        assert!(!genesis.contains(&bls) && !genesis.contains(&cometbft));
        assert!(!genesis.contains(&kzg) && !genesis.contains(&p256));

        let fermat = opbnb_mainnet_precompiles(OPBNB_MAINNET_FERMAT_BLOCK, 1_701_151_200);
        assert!(fermat.contains(&bls) && fermat.contains(&cometbft));
        assert!(!fermat.contains(&kzg) && !fermat.contains(&p256));
        assert!(
            !opbnb_mainnet_precompiles(OPBNB_MAINNET_FERMAT_BLOCK - 1, 1_701_151_199)
                .contains(&bls)
        );

        let canyon = opbnb_mainnet_precompiles(27_116_677, OPBNB_MAINNET_CANYON_TIMESTAMP);
        assert!(canyon.contains(&bls) && !canyon.contains(&kzg));

        let ecotone = opbnb_mainnet_precompiles(27_117_877, OPBNB_MAINNET_ECOTONE_TIMESTAMP);
        assert!(ecotone.contains(&kzg) && ecotone.contains(&bls) && !ecotone.contains(&p256));

        let haber = opbnb_mainnet_precompiles(27_118_477, OPBNB_MAINNET_HABER_TIMESTAMP);
        assert!(haber.contains(&p256) && haber.contains(&kzg) && haber.contains(&cometbft));
        assert_eq!(
            opbnb_mainnet_precompiles(200_000_000, OPBNB_MAINNET_FJORD_TIMESTAMP + 1)
                .addresses_set(),
            haber.addresses_set()
        );
    }

    #[test]
    fn create2_deployer_address() {
        assert_eq!(
            Address::from(OPBNB_MAINNET_CREATE2_DEPLOYER),
            revm::primitives::address!("13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2")
        );
    }

    #[test]
    fn gasless_needs_wright_and_a_zero_price() {
        assert!(!is_gasless(OPBNB_MAINNET_WRIGHT_TIMESTAMP - 1, 0));
        assert!(is_gasless(OPBNB_MAINNET_WRIGHT_TIMESTAMP, 0));
        assert!(!is_gasless(OPBNB_MAINNET_WRIGHT_TIMESTAMP, 1));
    }
}
