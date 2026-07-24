use alloy::primitives::{Address, B256, Bytes, U256};
use std::collections::{HashMap, VecDeque};

use super::poster_gas::ArbPosterCharge;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArbitrumCallContext {
    pub depth: usize,
    pub callers_caller: Address,
}

impl Default for ArbitrumCallContext {
    fn default() -> Self {
        Self {
            depth: 1,
            callers_caller: Address::ZERO,
        }
    }
}

/// Nitro's `RecentWasms` LRU carried by this execution context. The first
/// insertion fixes capacity for the context lifetime, and get-on-hit updates
/// recency. A configured size of zero still retains one entry because geth's
/// `BasicLRU` clamps it to one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RecentWasms {
    capacity: Option<usize>,
    most_recent_first: VecDeque<B256>,
}

impl RecentWasms {
    fn insert(&mut self, code_hash: B256, retain: u16) -> bool {
        if let Some(index) = self
            .most_recent_first
            .iter()
            .position(|existing| *existing == code_hash)
        {
            self.most_recent_first.remove(index);
            self.most_recent_first.push_front(code_hash);
            return true;
        }

        let capacity = *self
            .capacity
            .get_or_insert_with(|| usize::from(retain).max(1));
        if self.most_recent_first.len() >= capacity {
            self.most_recent_first.pop_back();
        }
        self.most_recent_first.push_front(code_hash);
        false
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArbitrumExecutionContext {
    current_call: ArbitrumCallContext,
    current_l2_block_number: Option<U256>,
    current_l2_basefee: Option<u64>,
    current_poster_charge: Option<ArbPosterCharge>,
    activated_wasm_modules: HashMap<B256, Bytes>,
    recent_wasms: RecentWasms,
    stylus_pages_open: u16,
    stylus_pages_ever: u16,
    open_contract_frames: HashMap<Address, u32>,
}

impl ArbitrumExecutionContext {
    pub fn set_current_l2_context(&mut self, block_number: U256, basefee: u64) {
        self.current_l2_block_number = Some(block_number);
        self.current_l2_basefee = Some(basefee);
    }

    pub fn current_l2_block_number(&self) -> Option<U256> {
        self.current_l2_block_number
    }

    pub fn current_l2_basefee(&self) -> Option<u64> {
        self.current_l2_basefee
    }

    pub fn set_current_poster_charge(&mut self, charge: ArbPosterCharge) {
        self.current_poster_charge = Some(charge);
    }

    pub fn current_poster_charge(&self) -> Option<ArbPosterCharge> {
        self.current_poster_charge
    }

    pub fn clear_current_poster_charge(&mut self) {
        self.current_poster_charge = None;
    }

    pub fn set_current_call(&mut self, depth: usize, callers_caller: Address) {
        self.current_call = ArbitrumCallContext {
            depth,
            callers_caller,
        };
    }

    pub fn current_call(&self) -> ArbitrumCallContext {
        self.current_call
    }

    pub fn insert_activated_wasm_module(&mut self, module_hash: B256, module: Bytes) {
        self.activated_wasm_modules.insert(module_hash, module);
    }

    pub fn activated_wasm_module(&self, module_hash: B256) -> Option<&Bytes> {
        self.activated_wasm_modules.get(&module_hash)
    }

    /// Inserts `code_hash`, returning true when it was already present. This is
    /// deliberately outside revm's journal: a reverted/OOG child still warms a
    /// later Stylus call in the same transaction, matching Nitro.
    pub fn insert_recent_wasm(&mut self, code_hash: B256, retain: u16) -> bool {
        self.recent_wasms.insert(code_hash, retain)
    }

    pub fn stylus_pages_open(&self) -> u16 {
        self.stylus_pages_open
    }

    pub fn stylus_pages_ever(&self) -> u16 {
        self.stylus_pages_ever
    }

    pub fn set_stylus_pages_open(&mut self, pages: u16) {
        self.stylus_pages_open = pages;
        self.stylus_pages_ever = self.stylus_pages_ever.max(pages);
    }

    pub fn remaining_stylus_page_limit(&self, page_limit: u16) -> u16 {
        page_limit.saturating_sub(self.stylus_pages_open)
    }

    /// Tracks every open non-DELEGATECALL/CALLCODE contract frame, matching
    /// nitro's per-transaction `TxProcessor.Programs` counter.
    pub fn enter_contract_frame(&mut self, address: Address) {
        *self.open_contract_frames.entry(address).or_insert(0) += 1;
    }

    pub fn exit_contract_frame(&mut self, address: Address) {
        if let Some(open) = self.open_contract_frames.get_mut(&address) {
            *open = open.saturating_sub(1);
            if *open == 0 {
                self.open_contract_frames.remove(&address);
            }
        }
    }

    /// nitro `reentrant := p.Programs[acting] > 1` (`tx_processor.go:139`). The
    /// current frame has already been counted when Stylus execution begins.
    pub fn contract_is_reentrant(&self, address: Address) -> bool {
        self.open_contract_frames
            .get(&address)
            .is_some_and(|open| *open > 1)
    }

    pub fn open_contract_frame_count(&self, address: Address) -> u32 {
        self.open_contract_frames
            .get(&address)
            .copied()
            .unwrap_or_default()
    }

    pub fn clear_open_contract_frames(&mut self) {
        self.open_contract_frames.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_activated_wasm_module_and_tracks_remaining_pages() {
        let mut context = ArbitrumExecutionContext::default();
        let module_hash = B256::from([0x11; 32]);
        let module = Bytes::from_static(b"module");

        context.insert_activated_wasm_module(module_hash, module.clone());
        context.set_stylus_pages_open(5);
        context.set_current_l2_context(U256::from(123), 456);
        context.set_current_poster_charge(ArbPosterCharge {
            poster_gas: 7,
            ..Default::default()
        });

        assert_eq!(context.activated_wasm_module(module_hash), Some(&module));
        assert_eq!(context.current_l2_block_number(), Some(U256::from(123)));
        assert_eq!(context.current_l2_basefee(), Some(456));
        assert_eq!(context.current_poster_charge().unwrap().poster_gas, 7);
        assert_eq!(context.stylus_pages_open(), 5);
        assert_eq!(context.remaining_stylus_page_limit(8), 3);
        assert_eq!(context.remaining_stylus_page_limit(4), 0);
    }

    #[test]
    fn all_contract_frames_answer_the_reentrancy_question() {
        let mut context = ArbitrumExecutionContext::default();
        let program = Address::with_last_byte(7);

        assert!(!context.contract_is_reentrant(program));
        context.enter_contract_frame(program);
        assert!(!context.contract_is_reentrant(program));
        context.enter_contract_frame(program);
        assert!(context.contract_is_reentrant(program));

        context.exit_contract_frame(program);
        assert!(!context.contract_is_reentrant(program));
        assert_eq!(context.open_contract_frame_count(program), 1);
        context.exit_contract_frame(program);
        assert_eq!(context.open_contract_frame_count(program), 0);

        // An unbalanced exit must not underflow into a permanently-open frame.
        context.exit_contract_frame(program);
        assert_eq!(context.open_contract_frame_count(program), 0);

        context.enter_contract_frame(program);
        context.clear_open_contract_frames();
        assert_eq!(context.open_contract_frame_count(program), 0);
    }

    #[test]
    fn recent_wasms_matches_nitro_lru_recency_and_eviction() {
        let mut context = ArbitrumExecutionContext::default();
        let first = B256::from([1; 32]);
        let second = B256::from([2; 32]);
        let third = B256::from([3; 32]);

        assert!(!context.insert_recent_wasm(first, 2));
        assert!(!context.insert_recent_wasm(second, 2));
        assert!(context.insert_recent_wasm(first, 2));
        assert!(!context.insert_recent_wasm(third, 2));
        assert!(!context.insert_recent_wasm(second, 2), "second was LRU");
    }

    #[test]
    fn recent_wasms_clamps_zero_capacity_and_freezes_first_size() {
        let mut zero = ArbitrumExecutionContext::default();
        let first = B256::from([1; 32]);
        assert!(!zero.insert_recent_wasm(first, 0));
        assert!(zero.insert_recent_wasm(first, 0));

        let mut fixed = ArbitrumExecutionContext::default();
        let second = B256::from([2; 32]);
        assert!(!fixed.insert_recent_wasm(first, 1));
        assert!(!fixed.insert_recent_wasm(second, 100));
        assert!(!fixed.insert_recent_wasm(first, 100));
    }
}
