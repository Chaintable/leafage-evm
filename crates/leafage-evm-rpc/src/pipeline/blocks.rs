//! Block bodies from an upstream RPC: the archive keeps only transaction
//! hashes.

use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::{ArrayParams, BatchRequestBuilder};
use jsonrpsee::http_client::HttpClient;

/// Full bodies of the blocks `hashes` from the upstream RPC, in one batch.
pub(crate) async fn fetch_blocks(
    client: &HttpClient,
    hashes: &[leafage_evm_types::H256],
) -> anyhow::Result<Vec<alloy::rpc::types::Block>> {
    let mut batch = BatchRequestBuilder::new();
    for hash in hashes {
        let mut params = ArrayParams::new();
        params.insert(hash)?;
        params.insert(true)?;
        batch.insert("eth_getBlockByHash", params)?;
    }
    let responses = client
        .batch_request::<Option<alloy::rpc::types::Block>>(batch)
        .await?;
    let mut blocks = Vec::with_capacity(hashes.len());
    for (hash, response) in hashes.iter().zip(responses) {
        let block = response
            .map_err(|err| anyhow::anyhow!("block {hash}: {err}"))?
            .ok_or_else(|| anyhow::anyhow!("upstream has no block {hash}"))?;
        anyhow::ensure!(
            block.header.hash == *hash,
            "upstream returned block {} for {hash}",
            block.header.hash
        );
        blocks.push(block);
    }
    anyhow::ensure!(
        blocks.len() == hashes.len(),
        "upstream returned fewer blocks than asked"
    );
    Ok(blocks)
}

/// [`fetch_blocks`] from `debug_getRawBlock`: the upstream only reads the
/// stored RLP, and the senders are recovered here, in parallel with the
/// replay, instead of by the upstream for its JSON.
pub(crate) async fn fetch_raw_blocks(
    client: &HttpClient,
    hashes: &[leafage_evm_types::H256],
) -> anyhow::Result<Vec<alloy::rpc::types::Block>> {
    // geth's errcodeResponseTooLarge: a batch past its response size limit
    // (25MB by default) answers the rest of it with this error.
    const RESPONSE_TOO_LARGE: i32 = -32003;

    let mut raws = Vec::with_capacity(hashes.len());
    while raws.len() < hashes.len() {
        let rest = &hashes[raws.len()..];
        let mut batch = BatchRequestBuilder::new();
        for hash in rest {
            let mut params = ArrayParams::new();
            params.insert(hash)?;
            batch.insert("debug_getRawBlock", params)?;
        }
        let responses = client
            .batch_request::<alloy::primitives::Bytes>(batch)
            .await?;
        let before = raws.len();
        for (hash, response) in rest.iter().zip(responses) {
            match response {
                Ok(raw) => raws.push((*hash, raw)),
                // Ask again for the rest, unless not even one block fit.
                Err(err) if err.code() == RESPONSE_TOO_LARGE && raws.len() > before => break,
                Err(err) => anyhow::bail!("block {hash}: {err}"),
            }
        }
        anyhow::ensure!(
            raws.len() > before,
            "upstream returned fewer blocks than asked"
        );
    }
    tokio::task::spawn_blocking(move || {
        raws.into_iter()
            .map(|(hash, raw)| decode_raw_block(hash, &raw))
            .collect()
    })
    .await?
}

fn decode_raw_block(
    hash: leafage_evm_types::H256,
    raw: &[u8],
) -> anyhow::Result<alloy::rpc::types::Block> {
    use alloy::consensus::transaction::SignerRecoverable;
    use alloy::rlp::Decodable;

    let block = alloy::consensus::Block::<alloy::consensus::TxEnvelope>::decode(&mut &raw[..])
        .map_err(|err| anyhow::anyhow!("decode block {hash}: {err}"))?;
    let decoded = block.header.hash_slow();
    anyhow::ensure!(
        decoded == hash,
        "upstream returned block {decoded} for {hash}"
    );
    let alloy::consensus::Block { header, body } = block;
    let number = header.number;
    let mut transactions = Vec::with_capacity(body.transactions.len());
    for (index, tx) in body.transactions.into_iter().enumerate() {
        let signer = tx
            .recover_signer()
            .map_err(|err| anyhow::anyhow!("block {number} tx {index}: {err}"))?;
        transactions.push(alloy::rpc::types::Transaction {
            inner: alloy::consensus::transaction::Recovered::new_unchecked(tx, signer),
            block_hash: Some(hash),
            block_number: Some(number),
            transaction_index: Some(index as u64),
            effective_gas_price: None,
            block_timestamp: None,
        });
    }
    Ok(alloy::rpc::types::Block {
        header: alloy::rpc::types::Header::from_consensus(
            alloy::consensus::Sealed::new_unchecked(header, hash),
            None,
            Some(alloy::primitives::U256::from(raw.len())),
        ),
        uncles: body.ommers.iter().map(|ommer| ommer.hash_slow()).collect(),
        transactions: alloy::rpc::types::BlockTransactions::Full(transactions),
        withdrawals: body.withdrawals,
    })
}
