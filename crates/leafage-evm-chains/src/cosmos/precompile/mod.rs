use crate::cosmos::CosmosHardfork;
use once_cell::race::OnceBox;
use revm::handler::EthPrecompiles;
use revm::precompile::{PrecompileSpecId, Precompiles};

mod bech32;
mod p256;
mod erc20;

pub struct CosmosPrecompiles {
    inner: EthPrecompiles,
}

impl CosmosPrecompiles {
    pub fn new(spec: CosmosHardfork) -> Self {
        let precompiles = Self::init_precompiles(spec.clone());
        Self {
            inner: EthPrecompiles {
                precompiles,
                spec: spec.into(),
            },
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
