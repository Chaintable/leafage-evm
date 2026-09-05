// Copyright 2026 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! CallFrom registry and continuation state used by the Arc EVM execution loop.

use crate::arc::{
    precompile::{
        call_from::{CallFromPrecompile, CALL_FROM_ADDRESS, MEMO_ADDRESS, MULTICALL3_FROM_ADDRESS},
        subcall::{SubcallContinuationData, SubcallPrecompile},
    },
    ArcHardfork, ArcHardforkFlags,
};
use alloy::primitives::Address;
use revm::context_interface::journaled_state::JournalCheckpoint;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};

pub(crate) struct SubcallContinuation {
    pub(crate) precompile: Arc<dyn SubcallPrecompile>,
    pub(crate) gas_limit: u64,
    pub(crate) init_subcall_gas_overhead: u64,
    pub(crate) return_memory_offset: Range<usize>,
    pub(crate) continuation_data: SubcallContinuationData,
    pub(crate) checkpoint: JournalCheckpoint,
}

#[derive(Debug, Clone)]
pub(crate) enum AllowedCallers {
    Only(HashSet<Address>),
}

impl AllowedCallers {
    pub(crate) fn is_allowed(&self, caller: &Address) -> bool {
        match self {
            Self::Only(addresses) => addresses.contains(caller),
        }
    }
}

#[derive(Clone)]
struct SubcallRegistration {
    precompile: Arc<dyn SubcallPrecompile>,
    allowed_callers: AllowedCallers,
}

#[derive(Default, Clone)]
pub(crate) struct SubcallRegistry {
    precompiles: HashMap<Address, SubcallRegistration>,
}

impl SubcallRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn for_hardforks(hardfork_flags: ArcHardforkFlags) -> Self {
        let mut registry = Self::new();
        if hardfork_flags.is_active(ArcHardfork::Zero7) {
            registry.register(
                CALL_FROM_ADDRESS,
                Arc::new(CallFromPrecompile),
                AllowedCallers::Only(HashSet::from([MEMO_ADDRESS, MULTICALL3_FROM_ADDRESS])),
            );
        }
        registry
    }

    pub(crate) fn register(
        &mut self,
        address: Address,
        precompile: Arc<dyn SubcallPrecompile>,
        allowed_callers: AllowedCallers,
    ) {
        self.precompiles.insert(
            address,
            SubcallRegistration {
                precompile,
                allowed_callers,
            },
        );
    }

    pub(crate) fn get(
        &self,
        address: &Address,
    ) -> Option<(&Arc<dyn SubcallPrecompile>, &AllowedCallers)> {
        self.precompiles
            .get(address)
            .map(|registration| (&registration.precompile, &registration.allowed_callers))
    }
}

impl std::fmt::Debug for SubcallContinuation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubcallContinuation")
            .field("return_memory_offset", &self.return_memory_offset)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SubcallRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubcallRegistry")
            .field("addresses", &self.precompiles.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc::{ArcForkActivation, ArcHardforkSchedule};

    fn flags(zero7: ArcForkActivation) -> ArcHardforkFlags {
        ArcHardforkSchedule::new(
            ArcForkActivation::Never,
            ArcForkActivation::Never,
            ArcForkActivation::Never,
            ArcForkActivation::Never,
            zero7,
            ArcForkActivation::Never,
        )
        .flags_at(0, 0)
    }

    #[test]
    fn registry_is_empty_before_zero7() {
        let registry = SubcallRegistry::for_hardforks(flags(ArcForkActivation::Never));
        assert!(registry.get(&CALL_FROM_ADDRESS).is_none());
    }

    #[test]
    fn zero7_registry_allows_only_memo_and_multicall3_from() {
        let registry = SubcallRegistry::for_hardforks(flags(ArcForkActivation::Block(0)));
        let (_, allowed) = registry
            .get(&CALL_FROM_ADDRESS)
            .expect("Zero7 registers CallFrom");

        assert!(allowed.is_allowed(&MEMO_ADDRESS));
        assert!(allowed.is_allowed(&MULTICALL3_FROM_ADDRESS));
        assert!(!allowed.is_allowed(&Address::ZERO));
    }
}
