use crate::citrea::CitreaHardfork;
use once_cell::race::OnceBox;
use revm::handler::EthPrecompiles;
use revm::precompile::{PrecompileSpecId, Precompiles};

mod schnorr;

pub struct CitreaPrecompiles {
    inner: EthPrecompiles,
}

impl CitreaPrecompiles {
    pub fn new(spec: CitreaHardfork) -> Self {
        let precompiles = Self::init_precompiles();
        Self {
            inner: EthPrecompiles {
                precompiles,
                spec: spec.into(),
            },
        }
    }

    fn init_precompiles() -> &'static Precompiles {
        static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
        INSTANCE.get_or_init(|| {
            let mut precompiles = Precompiles::new(PrecompileSpecId::BERLIN).clone();
            precompiles.extend([
                crate::cosmos::precompile::p256::P256_VERIFY,
                schnorr::SCHNORR_VERIFY,
            ]);
            Box::new(precompiles)
        })
    }

    #[inline]
    pub fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }
}

impl From<CitreaPrecompiles> for EthPrecompiles {
    #[inline]
    fn from(precompiles: CitreaPrecompiles) -> Self {
        precompiles.inner
    }
}
