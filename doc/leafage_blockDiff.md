# Rationale
We want to request for a new rpc endpoint `leafage_storageDiff`, which provides the storage diff of a given block on the parent block's state. This rpc is designed to be used by leafage-evm to update its state.

# Specification
## Request
### Method
`leafage_storageDiff`
### Parameters
The method takes 2 parameters: 
- `block_id`: BLOCKHASH|QUANTITY|TAG - block hash , integer block number, or the string "latest", "earliest" or "pending"
- `re_exec` BOOL, default false - whether to get storage diff of the block's parent block by executing the block's transactions on the parent block's state.

### Returns
The method returns bytes of  rlp encoded `StorageDiff` object.

`StorageDiff` object, which contains the following fields:
- `hash`: BLOCKHASH - block hash
- `parent_hash`: BLOCKHASH - parent block hash
- `new_accounts`: Array of `NewAccount` - new accounts created in this block
- `deleted_accounts`: Array of `Hash` - accounts deleted in this block
- `storage_diff`: Array of `AccountStorageDiff` -  storage diff of accounts modified in this block
- `new_codes`: Array of `NewCode` - new codes created in this block

#### NewAccount
- `address`: HASH - account hash
- `balance`: U256 - account balance
- `nonce`: U64 - account nonce
- `code_hash`: HASH - account code hash

#### AccountStorageDiff
- `address`: HASH - account hash
- `diffs`: Array of `(HASH,U256)` - storage diff of the account

#### NewCode
- `code_hash`: HASH - code hash
- `code`: Bytes - code
