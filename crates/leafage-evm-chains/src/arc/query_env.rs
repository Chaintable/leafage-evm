use alloy::eips::eip7840::BlobParams;
use leafage_evm_types::{BlockEnv, BlockOverrides, Header, H256, U256};
use revm::context_interface::block::BlobExcessGasAndPrice;
use revm::database::CacheDB;
use std::{error::Error, fmt};

pub const INVALID_SIMULATION_BLOCK_NUMBER_MESSAGE: &str =
    "simulation block number must equal state anchor + 1";

/// Arc H + 1 simulation environment derived from a canonical state anchor.
#[derive(Debug, Clone)]
pub struct ArcNextBlockEnvironment {
    pub block_env: BlockEnv,
    /// Its hash is unknown and therefore left zero.
    pub synthetic_next_header: Header,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArcQueryEnvError {
    AnchorBlockNumberOverflow(u64),
    InvalidSimulationBlockNumber {
        anchor: u64,
        requested: Option<U256>,
        expected: u64,
    },
    MissingNextBaseFee {
        parent: u64,
        extra_data_len: usize,
    },
}

impl fmt::Display for ArcQueryEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnchorBlockNumberOverflow(anchor) => {
                write!(f, "Arc state anchor {anchor} has no next block")
            }
            Self::InvalidSimulationBlockNumber { .. } => {
                f.write_str(INVALID_SIMULATION_BLOCK_NUMBER_MESSAGE)
            }
            Self::MissingNextBaseFee {
                parent,
                extra_data_len,
            } => write!(
                f,
                "Arc parent block {parent} has no decodable nextBaseFee (extra_data length {extra_data_len}) and no fee-config fallback was supplied"
            ),
        }
    }
}

impl Error for ArcQueryEnvError {}

/// Decodes Arc's `nextBaseFee` header extension.
///
/// Arc encodes the value as exactly eight big-endian bytes. Any other
/// `extra_data` length means the caller must use the chain fee-config fallback.
pub fn decode_arc_next_base_fee(header: &Header) -> Option<u64> {
    let bytes: [u8; 8] = header.extra_data.as_ref().try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// Builds the Arc simulation block environment and request-local BLOCKHASH cache.
///
/// `fallback_next_base_fee` must be computed by the Arc fee-config implementation.
/// It is only used for `H + 1` when the canonical parent does not contain an
/// eight-byte `nextBaseFee`; this helper never guesses fee parameters.
pub fn build_arc_next_block_simulation_environment<DB>(
    anchor_header: Header,
    block_overrides: BlockOverrides,
    fallback_next_base_fee: Option<u64>,
    db: &mut CacheDB<DB>,
) -> Result<ArcNextBlockEnvironment, ArcQueryEnvError> {
    let mut block_env = block_env_from_header(&anchor_header);
    let anchor_number = anchor_header.number;
    let next_number = anchor_number
        .checked_add(1)
        .ok_or(ArcQueryEnvError::AnchorBlockNumberOverflow(anchor_number))?;
    if block_overrides.number != Some(U256::from(next_number)) {
        return Err(ArcQueryEnvError::InvalidSimulationBlockNumber {
            anchor: anchor_number,
            requested: block_overrides.number,
            expected: next_number,
        });
    }

    let explicit_base_fee = block_overrides
        .base_fee
        .map(|base_fee| base_fee.saturating_to());
    let next_base_fee = explicit_base_fee.map_or_else(
        || {
            decode_arc_next_base_fee(&anchor_header)
                .or(fallback_next_base_fee)
                .ok_or(ArcQueryEnvError::MissingNextBaseFee {
                    parent: anchor_number,
                    extra_data_len: anchor_header.extra_data.len(),
                })
        },
        Ok,
    )?;

    block_env.blob_excess_gas_and_price = Some(next_blob_environment(&anchor_header));
    apply_block_overrides_to_env(&block_overrides, explicit_base_fee, &mut block_env);
    block_env.basefee = next_base_fee;

    let mut synthetic_next_header = anchor_header.clone();
    synthetic_next_header.hash = H256::ZERO;
    synthetic_next_header.parent_hash = anchor_header.hash;
    synthetic_next_header.number = next_number;
    synthetic_next_header.base_fee_per_gas = Some(block_env.basefee);
    synthetic_next_header.excess_blob_gas = block_env
        .blob_excess_gas_and_price
        .map(|blob| blob.excess_blob_gas);
    // Arc payload construction disables blob transactions. This synthetic
    // pre-execution H + 1 header must not inherit H's blob usage.
    synthetic_next_header.blob_gas_used = Some(0);
    // The anchor's bytes encode H + 1 base fee. H + 1 extra_data would
    // encode H + 2, which this query environment does not compute.
    synthetic_next_header.extra_data = Default::default();
    apply_block_overrides_to_header(&block_overrides, &mut synthetic_next_header);

    let mut block_hashes = block_overrides.block_hash.unwrap_or_default();
    // An explicit blockHashes entry intentionally overrides BLOCKHASH while
    // the synthetic header's parent_hash remains the canonical anchor hash.
    block_hashes
        .entry(anchor_number)
        .or_insert(anchor_header.hash);
    db.cache.block_hashes.extend(
        block_hashes
            .into_iter()
            .map(|(number, hash)| (U256::from(number), hash)),
    );

    Ok(ArcNextBlockEnvironment {
        block_env,
        synthetic_next_header,
    })
}

fn block_env_from_header(header: &Header) -> BlockEnv {
    BlockEnv {
        number: U256::from(header.number),
        beneficiary: header.beneficiary,
        timestamp: U256::from(header.timestamp),
        difficulty: header.difficulty,
        basefee: header.base_fee_per_gas.unwrap_or_default(),
        gas_limit: header.gas_limit,
        prevrandao: if header.difficulty.is_zero() {
            Some(header.mix_hash)
        } else {
            Some(H256::ZERO)
        },
        blob_excess_gas_and_price: header.excess_blob_gas.map(|excess_gas| {
            let blob_gasprice = BlobParams::osaka().calc_blob_fee(excess_gas);
            BlobExcessGasAndPrice {
                excess_blob_gas: excess_gas,
                blob_gasprice,
            }
        }),
        slot_num: Default::default(),
    }
}

fn next_blob_environment(parent: &Header) -> BlobExcessGasAndPrice {
    let params = BlobParams::osaka();
    let excess_blob_gas = match (
        parent.excess_blob_gas,
        parent.blob_gas_used,
        parent.base_fee_per_gas,
    ) {
        (Some(excess_blob_gas), Some(blob_gas_used), Some(base_fee_per_gas)) => params
            .next_block_excess_blob_gas_osaka(excess_blob_gas, blob_gas_used, base_fee_per_gas),
        // Matches Reth's next-block environment: when Osaka blob parameters
        // are active but the parent has no blob fields, start from zero.
        _ => 0,
    };
    BlobExcessGasAndPrice {
        excess_blob_gas,
        blob_gasprice: params.calc_blob_fee(excess_blob_gas),
    }
}

fn apply_block_overrides_to_env(
    overrides: &BlockOverrides,
    explicit_base_fee: Option<u64>,
    env: &mut BlockEnv,
) {
    if let Some(number) = overrides.number {
        env.number = number;
    }
    if let Some(difficulty) = overrides.difficulty {
        env.difficulty = difficulty;
    }
    if let Some(time) = overrides.time {
        env.timestamp = U256::from(time);
    }
    if let Some(gas_limit) = overrides.gas_limit {
        env.gas_limit = gas_limit;
    }
    if let Some(coinbase) = overrides.coinbase {
        env.beneficiary = coinbase;
    }
    if let Some(random) = overrides.random {
        env.prevrandao = Some(random);
    }
    if let Some(base_fee) = explicit_base_fee {
        env.basefee = base_fee;
    }
    if let Some(blob_base_fee) = overrides.blob_base_fee {
        let excess_blob_gas = env
            .blob_excess_gas_and_price
            .map(|blob| blob.excess_blob_gas)
            .unwrap_or_default();
        env.blob_excess_gas_and_price = Some(BlobExcessGasAndPrice {
            excess_blob_gas,
            blob_gasprice: blob_base_fee.saturating_to(),
        });
    }
}

fn apply_block_overrides_to_header(overrides: &BlockOverrides, header: &mut Header) {
    if let Some(difficulty) = overrides.difficulty {
        header.difficulty = difficulty;
    }
    if let Some(time) = overrides.time {
        header.timestamp = time;
    }
    if let Some(gas_limit) = overrides.gas_limit {
        header.gas_limit = gas_limit;
    }
    if let Some(coinbase) = overrides.coinbase {
        header.beneficiary = coinbase;
    }
    if let Some(random) = overrides.random {
        header.mix_hash = random;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_types::{Address, Bytes};
    use revm::database::EmptyDB;
    use std::collections::BTreeMap;

    fn canonical_header(number: u64, hash: H256, extra_data: Bytes) -> Header {
        Header {
            hash,
            inner: alloy::consensus::Header {
                number,
                gas_limit: 30_000_000,
                gas_used: 15_000_000,
                timestamp: 1_700_000_000,
                base_fee_per_gas: Some(100),
                extra_data,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn next_environment_uses_parent_hash_and_extra_data_base_fee() {
        let hash = H256::with_last_byte(42);
        let next_base_fee = 777_u64;
        let extra_data = Bytes::copy_from_slice(&next_base_fee.to_be_bytes());
        let header = canonical_header(42, hash, extra_data.clone());
        let overrides = BlockOverrides {
            number: Some(U256::from(43)),
            time: Some(1_700_000_012),
            coinbase: Some(Address::with_last_byte(9)),
            ..Default::default()
        };
        let mut db = CacheDB::new(EmptyDB::default());

        let environment =
            build_arc_next_block_simulation_environment(header, overrides, Some(999), &mut db)
                .unwrap();

        assert_eq!(environment.block_env.number, U256::from(43));
        assert_eq!(environment.block_env.basefee, next_base_fee);
        let blob = environment.block_env.blob_excess_gas_and_price.unwrap();
        assert_eq!(blob.excess_blob_gas, 0);
        assert_eq!(blob.blob_gasprice, BlobParams::osaka().calc_blob_fee(0));
        assert_eq!(db.cache.block_hashes.get(&U256::from(42)), Some(&hash));
        assert!(!db.cache.block_hashes.contains_key(&U256::from(43)));
        let next_header = environment.synthetic_next_header;
        assert_eq!(next_header.number, 43);
        assert_eq!(next_header.parent_hash, hash);
        assert_eq!(next_header.hash, H256::ZERO);
        assert!(next_header.extra_data.is_empty());
        assert_eq!(next_header.base_fee_per_gas, Some(next_base_fee));
        assert_eq!(next_header.timestamp, 1_700_000_012);
        assert_eq!(next_header.beneficiary, Address::with_last_byte(9));
        assert_eq!(next_header.excess_blob_gas, Some(0));
        assert_eq!(next_header.blob_gas_used, Some(0));
    }

    #[test]
    fn explicit_next_base_fee_overrides_parent_extra_data() {
        let mut db = CacheDB::new(EmptyDB::default());
        let environment = build_arc_next_block_simulation_environment(
            canonical_header(
                42,
                H256::with_last_byte(42),
                Bytes::copy_from_slice(&777_u64.to_be_bytes()),
            ),
            BlockOverrides {
                number: Some(U256::from(43)),
                base_fee: Some(U256::from(888)),
                ..Default::default()
            },
            None,
            &mut db,
        )
        .unwrap();

        assert_eq!(environment.block_env.basefee, 888);
        assert_eq!(
            environment.synthetic_next_header.base_fee_per_gas,
            Some(888)
        );
    }

    #[test]
    fn next_environment_derives_osaka_excess_blob_gas_from_parent() {
        let parent_excess_blob_gas = 0;
        let parent_blob_gas_used = 917_504;
        let parent_base_fee = 1;
        let mut header = canonical_header(
            42,
            H256::with_last_byte(42),
            Bytes::copy_from_slice(&777_u64.to_be_bytes()),
        );
        header.excess_blob_gas = Some(parent_excess_blob_gas);
        header.blob_gas_used = Some(parent_blob_gas_used);
        header.base_fee_per_gas = Some(parent_base_fee);
        let mut db = CacheDB::new(EmptyDB::default());

        let environment = build_arc_next_block_simulation_environment(
            header,
            BlockOverrides {
                number: Some(U256::from(43)),
                ..Default::default()
            },
            None,
            &mut db,
        )
        .unwrap();

        let expected_excess = BlobParams::osaka().next_block_excess_blob_gas_osaka(
            parent_excess_blob_gas,
            parent_blob_gas_used,
            parent_base_fee,
        );
        assert_eq!(expected_excess, 131_072);
        let blob = environment.block_env.blob_excess_gas_and_price.unwrap();
        assert_eq!(blob.excess_blob_gas, expected_excess);
        assert_eq!(
            blob.blob_gasprice,
            BlobParams::osaka().calc_blob_fee(expected_excess)
        );

        let next_header = environment.synthetic_next_header;
        assert_eq!(next_header.excess_blob_gas, Some(expected_excess));
        assert_eq!(next_header.blob_gas_used, Some(0));
    }

    #[test]
    fn blob_base_fee_override_preserves_derived_excess_blob_gas() {
        let mut header = canonical_header(
            42,
            H256::with_last_byte(42),
            Bytes::copy_from_slice(&777_u64.to_be_bytes()),
        );
        header.excess_blob_gas = Some(1_000_000);
        header.blob_gas_used = Some(786_432);
        let expected_excess = next_blob_environment(&header).excess_blob_gas;
        let mut db = CacheDB::new(EmptyDB::default());

        let environment = build_arc_next_block_simulation_environment(
            header,
            BlockOverrides {
                number: Some(U256::from(43)),
                blob_base_fee: Some(U256::from(12_345)),
                ..Default::default()
            },
            None,
            &mut db,
        )
        .unwrap();

        let blob = environment.block_env.blob_excess_gas_and_price.unwrap();
        assert_eq!(blob.excess_blob_gas, expected_excess);
        assert_eq!(blob.blob_gasprice, 12_345);
    }

    #[test]
    fn simulation_requires_exactly_anchor_plus_one() {
        for requested in [None, Some(42_u64), Some(44)] {
            let header = canonical_header(
                42,
                H256::with_last_byte(42),
                Bytes::copy_from_slice(&777_u64.to_be_bytes()),
            );
            let overrides = BlockOverrides {
                number: requested.map(U256::from),
                ..Default::default()
            };
            let mut db = CacheDB::new(EmptyDB::default());

            let error =
                build_arc_next_block_simulation_environment(header, overrides, None, &mut db)
                    .unwrap_err();

            assert_eq!(
                error,
                ArcQueryEnvError::InvalidSimulationBlockNumber {
                    anchor: 42,
                    requested: requested.map(U256::from),
                    expected: 43,
                }
            );
            assert_eq!(error.to_string(), INVALID_SIMULATION_BLOCK_NUMBER_MESSAGE);
            assert!(db.cache.block_hashes.is_empty());
        }
    }

    #[test]
    fn next_base_fee_prefers_extra_data_then_uses_explicit_fallback() {
        let encoded = canonical_header(
            42,
            H256::with_last_byte(42),
            Bytes::copy_from_slice(&777_u64.to_be_bytes()),
        );
        assert_eq!(decode_arc_next_base_fee(&encoded), Some(777));

        let mut encoded_db = CacheDB::new(EmptyDB::default());
        let encoded_env = build_arc_next_block_simulation_environment(
            encoded,
            BlockOverrides {
                number: Some(U256::from(43)),
                ..Default::default()
            },
            Some(999),
            &mut encoded_db,
        )
        .unwrap();
        assert_eq!(encoded_env.block_env.basefee, 777);

        let missing = canonical_header(42, H256::with_last_byte(42), Bytes::from_static(b"bad"));
        assert_eq!(decode_arc_next_base_fee(&missing), None);

        let mut fallback_db = CacheDB::new(EmptyDB::default());
        let fallback_env = build_arc_next_block_simulation_environment(
            missing,
            BlockOverrides {
                number: Some(U256::from(43)),
                ..Default::default()
            },
            Some(999),
            &mut fallback_db,
        )
        .unwrap();
        assert_eq!(fallback_env.block_env.basefee, 999);
    }

    #[test]
    fn missing_next_base_fee_fails_instead_of_guessing() {
        let mut db = CacheDB::new(EmptyDB::default());
        let error = build_arc_next_block_simulation_environment(
            canonical_header(42, H256::with_last_byte(42), Bytes::from_static(b"bad")),
            BlockOverrides {
                number: Some(U256::from(43)),
                ..Default::default()
            },
            None,
            &mut db,
        )
        .unwrap_err();

        assert_eq!(
            error,
            ArcQueryEnvError::MissingNextBaseFee {
                parent: 42,
                extra_data_len: 3,
            }
        );
        assert!(db.cache.block_hashes.is_empty());
    }

    #[test]
    fn explicit_block_hash_override_wins_without_changing_parent_hash() {
        let canonical_hash = H256::with_last_byte(42);
        let explicit_hash = H256::with_last_byte(99);
        let mut db = CacheDB::new(EmptyDB::default());
        let environment = build_arc_next_block_simulation_environment(
            canonical_header(
                42,
                canonical_hash,
                Bytes::copy_from_slice(&777_u64.to_be_bytes()),
            ),
            BlockOverrides {
                number: Some(U256::from(43)),
                block_hash: Some(BTreeMap::from([(42, explicit_hash)])),
                ..Default::default()
            },
            None,
            &mut db,
        )
        .unwrap();

        assert_eq!(
            db.cache.block_hashes.get(&U256::from(42)),
            Some(&explicit_hash)
        );
        assert_eq!(
            environment.synthetic_next_header.parent_hash,
            canonical_hash
        );
    }
}
