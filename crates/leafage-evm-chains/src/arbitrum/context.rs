use alloy::primitives::{Address, Bytes, B256};
use std::collections::HashMap;

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
    activated_wasm_modules: HashMap<B256, Bytes>,
    stylus_pages_open: u16,
    stylus_pages_ever: u16,
}

impl ArbitrumExecutionContext {
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

    pub fn stylus_pages_open(&self) -> u16 {
        self.stylus_pages_open
    }

    pub fn set_stylus_pages_open(&mut self, pages: u16) {
        self.stylus_pages_open = pages;
        self.stylus_pages_ever = self.stylus_pages_ever.max(pages);
    }

    pub fn remaining_stylus_page_limit(&self, page_limit: u16) -> u16 {
        page_limit.saturating_sub(self.stylus_pages_open)
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

        assert_eq!(context.activated_wasm_module(module_hash), Some(&module));
        assert_eq!(context.stylus_pages_open(), 5);
        assert_eq!(context.remaining_stylus_page_limit(8), 3);
        assert_eq!(context.remaining_stylus_page_limit(4), 0);
    }
}
