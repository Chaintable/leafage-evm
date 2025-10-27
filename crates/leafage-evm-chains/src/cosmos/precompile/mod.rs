use once_cell::race::OnceBox;
use revm::handler::EthPrecompiles;
use revm::precompile::{PrecompileSpecId, Precompiles};
use revm::primitives::hardfork::SpecId;

mod p256;

pub struct CosmosPrecompiles {
    inner: EthPrecompiles,
}

impl CosmosPrecompiles {
    pub fn new(spec: SpecId) -> Self {
        let precompiles = Self::init_precompiles(spec);
        Self {
            inner: EthPrecompiles { precompiles, spec },
        }
    }

    fn init_precompiles(spec: SpecId) -> &'static Precompiles {
        static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles = Precompiles::new(PrecompileSpecId::from_spec_id(spec)).clone();
            precompiles.extend([p256::P256_VERIFY]);
            Box::new(precompiles)
        })
    }

    #[inline]
    pub fn precompiles(&self) -> &EthPrecompiles {
        &self.inner
    }
}
