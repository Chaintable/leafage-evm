use crate::cosmos::config::TokenConfig;
use crate::cosmos::precompile::erc20::{create_erc20_precompile, Erc20Precompile};
use crate::cosmos::CosmosHardfork;
use alloy_evm::precompiles::DynPrecompile;
use once_cell::race::OnceBox;
use revm::handler::EthPrecompiles;
use revm::precompile::{PrecompileSpecId, Precompiles};

mod bech32;
mod erc20;
mod p256;

pub struct CosmosPrecompiles {
    inner: EthPrecompiles,
    native_token: Option<TokenConfig>,
}

impl CosmosPrecompiles {
    pub fn new(spec: CosmosHardfork, native_token: Option<TokenConfig>) -> Self {
        let precompiles = Self::init_precompiles(spec.clone());
        Self {
            inner: EthPrecompiles {
                precompiles,
                spec: spec.into(),
            },
            native_token,
        }
    }

    fn init_precompiles(spec: CosmosHardfork) -> &'static Precompiles {
        static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles =
                Precompiles::new(PrecompileSpecId::from_spec_id(spec.into())).clone();
            precompiles.extend([p256::P256_VERIFY, bech32::BECH32]);
            Box::new(precompiles)
        })
    }

    pub fn native_token_precompiles(&self) -> Option<DynPrecompile> {
        let Some(ref cfg) = self.native_token else {
            return None;
        };
        let precompile = Erc20Precompile::new(
            cfg.name.clone(),
            cfg.symbol.clone(),
            cfg.decimals,
            cfg.total_supply,
        );
        Some(create_erc20_precompile(precompile))
    }

    #[inline]
    pub fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }
}

impl From<CosmosPrecompiles> for EthPrecompiles {
    #[inline]
    fn from(precompiles: CosmosPrecompiles) -> Self {
        precompiles.inner
    }
}

pub(crate) mod unsupported {
    use revm::primitives::{address, Address};
    use std::collections::HashSet;
    use std::sync::LazyLock;

    const STAKING: Address = address!("0x0000000000000000000000000000000000000800");
    const DISTRIBUTION: Address = address!("0x0000000000000000000000000000000000000801");
    const ICS20: Address = address!("0x0000000000000000000000000000000000000802");
    const BANK: Address = address!("0x0000000000000000000000000000000000000804");
    const GOVERNANCE: Address = address!("0x0000000000000000000000000000000000000805");
    const SLASHING: Address = address!("0x0000000000000000000000000000000000000806");
    pub static UNSUPPORTED_LIST: LazyLock<HashSet<Address>> = LazyLock::new(|| {
        let unsupported_addresses = vec![STAKING, DISTRIBUTION, ICS20, BANK, GOVERNANCE, SLASHING];
        unsupported_addresses.into_iter().collect()
    });

    pub fn is_unsupported(addr: &Address) -> bool {
        UNSUPPORTED_LIST.contains(addr)
    }
}
