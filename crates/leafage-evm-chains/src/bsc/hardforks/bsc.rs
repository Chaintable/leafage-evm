#![allow(unused)]
use alloy_chains::Chain;
use alloy_hardforks::{hardfork, EthereumHardfork, Hardfork};
use core::any::Any;
use revm::primitives::hardfork::SpecId;

hardfork!(
    /// The name of a bsc hardfork.
    ///
    /// When building a list of hardforks for a chain, it's still expected to mix with [`EthereumHardfork`].
    #[derive(Default)]
    BscHardfork {
        /// Initial hardfork of BSC.
        Frontier,
        /// BSC `Ramanujan` hardfork
        Ramanujan,
        /// BSC `Niels` hardfork
        Niels,
        /// BSC `MirrorSync` hardfork
        MirrorSync,
        /// BSC `Bruno` hardfork
        Bruno,
        /// BSC `Euler` hardfork
        Euler,
        /// BSC `Nano` hardfork
        Nano,
        /// BSC `Moran` hardfork
        Moran,
        /// BSC `Gibbs` hardfork
        Gibbs,
        /// BSC `Planck` hardfork
        Planck,
        /// BSC `Luban` hardfork
        Luban,
        /// BSC `Plato` hardfork
        Plato,
        /// BSC `Hertz` hardfork
        Hertz,
        /// BSC `HertzFix` hardfork
        HertzFix,
        /// BSC `Kepler` hardfork
        Kepler,
        /// BSC `Feynman` hardfork
        Feynman,
        /// BSC `FeynmanFix` hardfork
        FeynmanFix,
        /// BSC `Cancun` hardfork
        Cancun,
        /// BSC `Haber` hardfork
        Haber,
        /// BSC `HaberFix` hardfork
        HaberFix,
        /// BSC `Bohr` hardfork
        Bohr,
        /// BSC `Tycho` hardfork - June 2024, added blob transaction support
        Tycho,
        /// BSC `Pascal` hardfork - March 2025, added smart contract wallets
        Pascal,
        /// BSC `Lorentz` hardfork
        Lorentz,
        /// BSC `Maxwell` hardfork
        #[default]
        Maxwell,
        /// BSC `Fermi` hardfork
        Fermi,
        /// BSC `Osaka` hardfork
        Osaka,
        /// BSC `Mendel` hardfork
        Mendel,
    }
);

/// Match helper method since it's not possible to match on `dyn Hardfork`
fn match_hardfork<H, HF, BHF>(fork: H, hardfork_fn: HF, bsc_hardfork_fn: BHF) -> Option<u64>
where
    H: Hardfork,
    HF: Fn(&EthereumHardfork) -> Option<u64>,
    BHF: Fn(&BscHardfork) -> Option<u64>,
{
    let fork: &dyn Any = &fork;
    if let Some(fork) = fork.downcast_ref::<EthereumHardfork>() {
        return hardfork_fn(fork);
    }
    fork.downcast_ref::<BscHardfork>().and_then(bsc_hardfork_fn)
}

impl From<BscHardfork> for SpecId {
    fn from(spec: BscHardfork) -> Self {
        match spec {
            BscHardfork::Frontier
            | BscHardfork::Ramanujan
            | BscHardfork::Niels
            | BscHardfork::MirrorSync
            | BscHardfork::Bruno
            | BscHardfork::Euler
            | BscHardfork::Gibbs
            | BscHardfork::Nano
            | BscHardfork::Moran
            | BscHardfork::Planck
            | BscHardfork::Luban
            | BscHardfork::Plato => SpecId::MUIR_GLACIER,
            BscHardfork::Hertz | BscHardfork::HertzFix => SpecId::LONDON,
            BscHardfork::Kepler | BscHardfork::Feynman | BscHardfork::FeynmanFix => {
                SpecId::SHANGHAI
            }
            BscHardfork::Cancun
            | BscHardfork::Haber
            | BscHardfork::HaberFix
            | BscHardfork::Bohr
            | BscHardfork::Tycho => SpecId::CANCUN,
            BscHardfork::Pascal
            | BscHardfork::Lorentz
            | BscHardfork::Maxwell
            | BscHardfork::Fermi => SpecId::PRAGUE,
            BscHardfork::Osaka | BscHardfork::Mendel => SpecId::OSAKA,
        }
    }
}
