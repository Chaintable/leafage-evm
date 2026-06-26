use crate::polygon::PolygonHardfork;
use once_cell::race::OnceBox;
use revm::precompile::{bls12_381, kzg_point_evaluation, modexp, secp256r1, Precompiles};
use std::boxed::Box;

mod pip88;

pub(crate) fn polygon_precompiles(spec: PolygonHardfork) -> &'static Precompiles {
    match spec {
        PolygonHardfork::Petersburg => Precompiles::byzantium(),
        PolygonHardfork::Istanbul => Precompiles::istanbul(),
        PolygonHardfork::Berlin | PolygonHardfork::London | PolygonHardfork::Shanghai => {
            Precompiles::berlin()
        }
        PolygonHardfork::Cancun => cancun(),
        PolygonHardfork::Prague => prague(),
        // Polygon mainnet activates MadhugiriPro at the same block as Madhugiri.
        PolygonHardfork::Madhugiri => madhugiri_pro(),
        // Polygon mainnet activates LisovoPro at the same block as Lisovo.
        PolygonHardfork::Lisovo => lisovo_pro(),
        PolygonHardfork::Chicago => chicago(),
    }
}

fn cancun() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = Precompiles::berlin().clone();
        precompiles.extend([secp256r1::P256VERIFY]);
        Box::new(precompiles)
    })
}

fn prague() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = cancun().clone();
        precompiles.extend(bls12_381::precompiles());
        Box::new(precompiles)
    })
}

fn madhugiri_pro() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = Precompiles::berlin().clone();
        precompiles.extend([modexp::OSAKA, kzg_point_evaluation::POINT_EVALUATION]);
        precompiles.extend(bls12_381::precompiles());
        precompiles.extend([secp256r1::P256VERIFY]);
        Box::new(precompiles)
    })
}

fn lisovo_pro() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = Precompiles::berlin().clone();
        precompiles.extend([modexp::OSAKA]);
        precompiles.extend(bls12_381::precompiles());
        precompiles.extend([secp256r1::P256VERIFY_OSAKA]);
        Box::new(precompiles)
    })
}

fn chicago() -> &'static Precompiles {
    static INSTANCE: OnceBox<Precompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = lisovo_pro().clone();
        precompiles.extend(pip88::precompiles());
        Box::new(precompiles)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::precompile::u64_to_address;

    #[test]
    fn chicago_matches_bor_precompile_addresses() {
        let precompiles = polygon_precompiles(PolygonHardfork::Chicago);

        for address in [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
            0x10, 0x11, 0x100,
        ] {
            assert!(
                precompiles.contains(&u64_to_address(address)),
                "missing {address:#x}"
            );
        }
        assert_eq!(precompiles.len(), 17);
        assert!(!precompiles.contains(&u64_to_address(0x0a)));
    }

    #[test]
    fn lisovo_pro_removes_kzg_and_reprices_p256() {
        let precompiles = polygon_precompiles(PolygonHardfork::Lisovo);

        assert_eq!(precompiles.len(), 17);
        assert!(!precompiles.contains(&u64_to_address(0x0a)));
        assert_eq!(
            precompiles
                .get(&u64_to_address(0x100))
                .unwrap()
                .execute(&[], u64::MAX)
                .unwrap()
                .gas_used,
            6_900
        );
    }

    #[test]
    fn madhugiri_pro_keeps_kzg_and_standard_p256() {
        let precompiles = polygon_precompiles(PolygonHardfork::Madhugiri);

        assert_eq!(precompiles.len(), 18);
        assert!(precompiles.contains(&u64_to_address(0x0a)));
        assert_eq!(
            precompiles
                .get(&u64_to_address(0x100))
                .unwrap()
                .execute(&[], u64::MAX)
                .unwrap()
                .gas_used,
            3_450
        );
    }
}
