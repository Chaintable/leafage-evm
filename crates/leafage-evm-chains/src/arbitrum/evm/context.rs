use alloy::primitives::{Address, Bytes, B256, U256};
use std::collections::HashMap;

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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArbitrumExecutionContext {
    current_call: ArbitrumCallContext,
    current_l2_block_number: Option<U256>,
    current_l2_basefee: Option<u64>,
    current_poster_charge: Option<ArbPosterCharge>,
    activated_wasm_modules: HashMap<B256, Bytes>,
    compiled_asm: HashMap<B256, Bytes>,
    stylus_pages_open: u16,
    stylus_pages_ever: u16,
    open_stylus_frames: HashMap<Address, u32>,
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

    /// Node-local native-asm cache keyed by the consensus moduleHash. The asm is
    /// a per-node derived artifact recompiled deterministically from on-chain
    /// wasm; the moduleHash anchors it to consensus. Only the native host target
    /// is compiled, so moduleHash alone is a sufficient key.
    pub fn insert_compiled_asm(&mut self, module_hash: B256, asm: Bytes) {
        self.compiled_asm.insert(module_hash, asm);
    }

    pub fn compiled_asm(&self, module_hash: B256) -> Option<&Bytes> {
        self.compiled_asm.get(&module_hash)
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

    /// Open Stylus frames for `address`, mirroring nitro's `TxProcessor.Programs`
    /// counter (`arbos/tx_processor.go:46`). nitro counts every non-delegate
    /// frame, but only ever queries the acting program's own address, and an
    /// address holds exactly one kind of code — so counting that address's
    /// Stylus frames is equivalent.
    pub fn enter_stylus_frame(&mut self, address: Address) {
        *self.open_stylus_frames.entry(address).or_insert(0) += 1;
    }

    pub fn exit_stylus_frame(&mut self, address: Address) {
        if let Some(open) = self.open_stylus_frames.get_mut(&address) {
            *open = open.saturating_sub(1);
            if *open == 0 {
                self.open_stylus_frames.remove(&address);
            }
        }
    }

    /// nitro `reentrant := p.Programs[acting] > 1` (`tx_processor.go:139`), asked
    /// before the frame being entered is counted.
    pub fn stylus_frame_is_open(&self, address: Address) -> bool {
        self.open_stylus_frames.contains_key(&address)
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
    fn open_stylus_frames_answer_the_reentrancy_question() {
        let mut context = ArbitrumExecutionContext::default();
        let program = Address::with_last_byte(7);

        assert!(!context.stylus_frame_is_open(program));
        context.enter_stylus_frame(program);
        // Entering again while the first frame is open is what nitro reports as
        // reentrant; the flag is read before the new frame is counted.
        assert!(context.stylus_frame_is_open(program));
        context.enter_stylus_frame(program);

        context.exit_stylus_frame(program);
        assert!(
            context.stylus_frame_is_open(program),
            "the outer frame is still open"
        );
        context.exit_stylus_frame(program);
        assert!(!context.stylus_frame_is_open(program));

        // An unbalanced exit must not underflow into a permanently-open frame.
        context.exit_stylus_frame(program);
        assert!(!context.stylus_frame_is_open(program));
    }
}
