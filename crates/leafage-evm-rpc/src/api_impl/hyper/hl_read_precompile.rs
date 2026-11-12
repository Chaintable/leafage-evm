//! Resolves HyperEVM's HyperCore read-precompile calls through a HyperCore RPC.
//!
//! HyperEVM serves the perps engine's state through the read-only precompiles in
//! `0x800..=0x814`, which only exist in HyperCore. The EVM state leafage replays has none of
//! it, so a call that reaches one of those addresses looks like a call to an empty account and
//! silently returns no data — simulations then report the wrong balances, oracle prices and
//! positions for anything that reads them.
//!
//! There is no HyperCore state to replay from: it is not archived, and the EVM state diff
//! carries only EVM accounts. So the read is forwarded instead — the same `(address, calldata)`
//! pair is run through `eth_call` on a node that does hold the state, typically a local
//! hl-node. [`leafage_evm_chains::hyper::read_precompiles`] turns the answers into precompile
//! results and prices them; this module only fetches them.
//!
//! See nanoreth's `read_precompile_forwarder` for the original implementation of this approach.

use alloy_evm::precompiles::PrecompilesMap;
use jsonrpsee::{
    core::{client::ClientT, ClientError},
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
    types::ErrorObject,
};
use leafage_evm_chains::hyper::read_precompiles::{self, Resolver, Response};
use leafage_evm_types::{Address, Bytes};
use revm::{precompile::PrecompileError, primitives::hardfork::SpecId};
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};
use tokio::runtime::Handle;
use tracing::{debug, warn};

/// Timeout for a single forwarded call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on cached responses, so that a long-lived process cannot grow the cache without
/// bound.
const MAX_CACHE_ENTRIES: usize = 16_384;

/// Responses, keyed by the block they were fetched for.
///
/// HyperCore state is not archived, so a response is only valid for the block it was fetched
/// at: the block number is part of the key and entries are never reused across blocks. A
/// same-height reorg can therefore reuse a response until the next block is seen, which is a
/// narrow window and self-corrects.
#[derive(Debug, Default)]
struct Cache {
    /// Highest block seen, so entries the chain has moved past can be dropped first.
    newest_block_number: u64,
    responses: HashMap<(u64, Address, Bytes), Response>,
    in_flight: HashMap<(u64, Address, Bytes), Arc<InFlight>>,
}

#[derive(Debug, Default)]
struct InFlight {
    result: Mutex<Option<Result<Response, String>>>,
    ready: Condvar,
}

/// Resolves HyperCore read precompile calls through an upstream hl-node.
pub struct ReadPrecompileForwarder {
    client: HttpClient,
    handle: Handle,
    /// Responses already fetched from the upstream.
    ///
    /// Repeated calls are the norm rather than the exception: several transactions of one
    /// `simulateTransactions` batch read the same oracle price or account, and estimateGas
    /// re-executes the same call on every step of its binary search. Caching also makes all
    /// reads within one request agree with each other, which separate upstream calls do not
    /// guarantee.
    cache: Mutex<Cache>,
}

/// Summarises the cache rather than dumping it.
///
/// The execution context holds one of these and derives [`Debug`], so a derived impl here would
/// print every cached response — up to [`MAX_CACHE_ENTRIES`] of them — into any log line or error
/// that formats the context.
impl std::fmt::Debug for ReadPrecompileForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cache = self.cache.lock().unwrap();
        f.debug_struct("ReadPrecompileForwarder")
            .field("newest_block_number", &cache.newest_block_number)
            .field("cached_responses", &cache.responses.len())
            .field("in_flight_requests", &cache.in_flight.len())
            .finish_non_exhaustive()
    }
}

impl ReadPrecompileForwarder {
    /// Creates a forwarder that resolves calls against `url`.
    ///
    /// `handle` drives the requests, since precompiles are invoked from blocking RPC workers.
    pub fn new(url: &str, handle: Handle) -> Result<Self, jsonrpsee::core::client::Error> {
        let client = HttpClientBuilder::default()
            .request_timeout(REQUEST_TIMEOUT)
            .build(url)?;
        Ok(Self {
            client,
            handle,
            cache: Mutex::new(Cache::default()),
        })
    }

    /// Builds the precompiles a simulation executing at `block_number` runs with.
    ///
    /// The returned map is Ethereum's precompiles for `spec` plus a forwarding precompile for
    /// every HyperCore read address.
    pub fn precompiles(self: &Arc<Self>, spec: SpecId, block_number: u64) -> PrecompilesMap {
        read_precompiles::precompiles(
            spec,
            Arc::new(Session {
                forwarder: Arc::clone(self),
                block_number,
            }),
        )
    }

    /// Runs `input` against the read precompile at `address` for `block_number`.
    ///
    /// The upstream is always asked for `latest`: HyperCore state is not archived, so there is
    /// nothing else it could answer for.
    fn resolve(
        &self,
        address: Address,
        input: &[u8],
        block_number: u64,
    ) -> Result<Response, PrecompileError> {
        let input = Bytes::copy_from_slice(input);
        let key = (block_number, address, input.clone());

        let (in_flight, leader) = {
            let mut cache = self.cache.lock().unwrap();
            if let Some(cached) = cache.responses.get(&key) {
                return Ok(cached.clone());
            }
            if let Some(in_flight) = cache.in_flight.get(&key) {
                (Arc::clone(in_flight), false)
            } else {
                let in_flight = Arc::new(InFlight::default());
                cache.in_flight.insert(key.clone(), Arc::clone(&in_flight));
                (in_flight, true)
            }
        };

        if !leader {
            let mut result = in_flight.result.lock().unwrap();
            while result.is_none() {
                result = in_flight.ready.wait(result).unwrap();
            }
            return result.clone().unwrap().map_err(PrecompileError::Fatal);
        }

        let result = self.request(address, &input).map_err(|err| err.to_string());
        {
            let mut cache = self.cache.lock().unwrap();
            cache.in_flight.remove(&key);
            if let Ok(response) = &result {
                cache.newest_block_number = cache.newest_block_number.max(block_number);
                if cache.responses.len() >= MAX_CACHE_ENTRIES {
                    // Drop what the chain has moved past first, so a burst of calls at the head
                    // does not throw away the entries it is still working through.
                    let newest = cache.newest_block_number;
                    cache.responses.retain(|(block, _, _), _| *block >= newest);
                    if cache.responses.len() >= MAX_CACHE_ENTRIES {
                        cache.responses.clear();
                    }
                }
                cache.responses.insert(key, response.clone());
            }
        }

        *in_flight.result.lock().unwrap() = Some(result.clone());
        in_flight.ready.notify_all();
        result.map_err(PrecompileError::Fatal)
    }

    fn request(&self, address: Address, input: &Bytes) -> Result<Response, PrecompileError> {
        let client = self.client.clone();
        let params = rpc_params![
            serde_json::json!({ "to": address, "data": input }),
            "latest"
        ];

        // Precompiles are called from blocking RPC workers, so the request is driven on the
        // runtime and waited on here rather than awaited.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.handle.spawn(async move {
            let _ = tx.send(client.request::<Bytes, _>("eth_call", params).await);
        });

        // The client times out on its own; this only guards against the response never arriving.
        let result = rx.recv_timeout(REQUEST_TIMEOUT * 2).map_err(|_| {
            warn!(target: "rpc::eth", %address, "Timed out forwarding read precompile call");
            PrecompileError::Fatal(format!("read precompile {address} timed out"))
        })?;

        match result {
            Ok(bytes) => Ok(Response::Output(bytes)),
            Err(ClientError::Call(err)) if is_rejected_input(&err) => {
                debug!(target: "rpc::eth", %address, message = err.message(), "Read precompile rejected input");
                Ok(Response::Rejected)
            }
            Err(err) => {
                warn!(target: "rpc::eth", %address, %err, "Failed to forward read precompile call");
                Err(PrecompileError::Fatal(format!(
                    "failed to forward read precompile {address}: {err}"
                )))
            }
        }
    }
}

/// One block's view of the forwarder, as the chain-side precompile set expects it.
struct Session {
    forwarder: Arc<ReadPrecompileForwarder>,
    block_number: u64,
}

impl Resolver for Session {
    fn resolve(&self, address: Address, input: &[u8]) -> Result<Response, PrecompileError> {
        self.forwarder.resolve(address, input, self.block_number)
    }
}

/// JSON-RPC code hl-node answers a failed `eth_call` with.
const EXECUTION_ERROR_CODE: i32 = -32003;

/// Whether a JSON-RPC error means the upstream ran the precompile and it refused the input,
/// rather than that the upstream could not serve the request at all.
///
/// Only the former burns the gas limit. Everything else — a wrong URL, a node without `eth_call`,
/// a rate limit, an overloaded upstream — has to surface as an RPC error, because reporting it as
/// a refused input would hand the caller a plausible but wrong out-of-gas result instead of a
/// visible failure.
///
/// This deliberately matches on the message as well as the code. The codes EVM RPCs use for
/// execution failures (`-32003`, and `-32000` in particular) double as generic server-error
/// catch-alls, so matching the code alone is what lets a throttled response through as a refused
/// input. Being too strict only costs a loud error where a quiet one would do.
fn is_rejected_input(err: &ErrorObject<'_>) -> bool {
    err.code() == EXECUTION_ERROR_CODE && err.message().contains("PrecompileError")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::{server::ServerBuilder, types::ErrorObjectOwned, RpcModule};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn only_a_refused_input_burns_gas() {
        let err = |code, message: &str| ErrorObject::owned(code, message.to_string(), None::<()>);

        // What hl-node answers a read precompile that refused its input with.
        assert!(is_rejected_input(&err(
            EXECUTION_ERROR_CODE,
            "EVM error: PrecompileError"
        )));

        // Everything else has to surface as an error rather than as an out-of-gas result: an
        // upstream that throttled or fell over never ran the precompile at all.
        for (code, message) in [
            (EXECUTION_ERROR_CODE, "rate limit exceeded"),
            (-32000, "rate limited"),
            (-32000, "execution timeout"),
            (-32601, "method eth_call is not available"),
            (-32603, "internal error"),
            (
                EXECUTION_ERROR_CODE,
                "out of gas: gas exhausted during precompiled contract execution: 21000",
            ),
        ] {
            assert!(!is_rejected_input(&err(code, message)), "{code} {message}");
        }
    }

    /// `l1BlockNumber`: takes no input and returns one word.
    const L1_BLOCK_NUMBER: Address =
        alloy::primitives::address!("0x0000000000000000000000000000000000000809");

    /// `bbo`, which takes a 32-byte input and rejects anything else.
    const BBO: Address = alloy::primitives::address!("0x000000000000000000000000000000000000080e");

    /// Checks the whole round trip — request shape, error mapping and caching — against a stub
    /// upstream.
    ///
    /// The forwarder waits on the node's runtime, so it has to run on a blocking worker of a
    /// multi-threaded runtime rather than on an async test thread.
    #[tokio::test(flavor = "multi_thread")]
    async fn forwards_reads_and_fails_loudly() {
        let requests = Arc::new(AtomicUsize::new(0));
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(Arc::clone(&requests));
        module
            .register_method("eth_call", |params, requests, _| {
                requests.fetch_add(1, Ordering::SeqCst);
                let (call, tag): (serde_json::Value, String) = params.parse()?;
                assert_eq!(tag, "latest");
                let to = call["to"].as_str().unwrap_or_default().to_ascii_lowercase();
                match to.as_str() {
                    "0x0000000000000000000000000000000000000809" => {
                        Ok::<_, ErrorObjectOwned>(Bytes::from(vec![0x2a; 32]))
                    }
                    "0x000000000000000000000000000000000000080e" => Err(ErrorObject::owned(
                        EXECUTION_ERROR_CODE,
                        "EVM error: PrecompileError".to_string(),
                        None::<()>,
                    )),
                    // An upstream that cannot serve the request at all, rather than one that
                    // ran the precompile and refused its input.
                    _ => Err(ErrorObject::owned(
                        -32000,
                        "rate limited".to_string(),
                        None::<()>,
                    )),
                }
            })
            .unwrap();
        let _handle = server.start(module);

        let forwarder = Arc::new(ReadPrecompileForwarder::new(&url, Handle::current()).unwrap());

        tokio::task::spawn_blocking(move || {
            let output = forwarder.resolve(L1_BLOCK_NUMBER, &[], 1).unwrap();
            assert!(matches!(output, Response::Output(ref bytes) if bytes.len() == 32));

            // Identical reads inside one block are answered from the cache.
            forwarder.resolve(L1_BLOCK_NUMBER, &[], 1).unwrap();
            assert_eq!(requests.load(Ordering::SeqCst), 1);

            // A different block is a different HyperCore state, so it is asked again.
            forwarder.resolve(L1_BLOCK_NUMBER, &[], 2).unwrap();
            assert_eq!(requests.load(Ordering::SeqCst), 2);

            // An input the precompile rejects is reported as rejected, not as a failure.
            let rejected = forwarder.resolve(BBO, &[0xde, 0xad], 2).unwrap();
            assert!(matches!(rejected, Response::Rejected));

            // An upstream that merely failed must not look like a refused input, or the caller
            // would get a plausible but wrong out-of-gas result instead of a visible failure.
            let failed = forwarder.resolve(Address::repeat_byte(0x80), &[], 2);
            assert!(matches!(failed, Err(PrecompileError::Fatal(_))));
        })
        .await
        .unwrap();
    }
}
