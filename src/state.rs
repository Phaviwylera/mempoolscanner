//! Fork head tracking (`ForkMonitor`) and shared multi-block state backend
//! feeding in-process `revm` simulations with cross-worker caching.

use alloy::consensus::BlockHeader;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::HeaderResponse;
use alloy::network::BlockResponse;
use alloy::providers::{DynProvider, Provider};
use futures_util::future::BoxFuture;
use revm::bytecode::Bytecode;
use revm::database_interface::async_db::DatabaseAsyncRef;
use revm::database_interface::DBErrorMarker;
use revm::primitives::{Address, StorageKey, StorageValue, B256, KECCAK_EMPTY, U256};
use revm::state::AccountInfo;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info, warn};

const BLOCK_WINDOW: u64 = 16;
const MAX_CODE_ENTRIES: usize = 4_096;
const MAX_ACCOUNT_ENTRIES: usize = 32_768;
const MAX_STORAGE_ENTRIES: usize = 65_536;
const MAX_BLOCK_HASHES: usize = 512;

#[derive(Clone, Debug)]
pub struct BlockSnapshot {
    pub number: u64,
    pub timestamp: u64,
    pub base_fee: u64,
    pub beneficiary: Address,
    pub gas_limit: u64,
    pub prevrandao: Option<B256>,
    pub excess_blob_gas: Option<u64>,
}

impl Default for BlockSnapshot {
    fn default() -> Self {
        Self {
            number: 1,
            timestamp: 1_790_000_000,
            base_fee: 1_000_000_000, // 1 gwei fallback
            beneficiary: Address::ZERO,
            gas_limit: 36_000_000,
            prevrandao: Some(B256::ZERO),
            excess_blob_gas: Some(0),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ForkStatus {
    pub block: Option<u64>,
    pub head: Option<u64>,
    pub lag: Option<u64>,
    pub snapshot: Option<BlockSnapshot>,
}

/// Polls state head and upstream head on a fixed cadence, recording lag and headers.
pub struct ForkMonitor {
    state_provider: DynProvider,
    upstream_provider: Option<DynProvider>,
    interval: Duration,
    refresh_lock: AsyncMutex<()>,
    status: RwLock<ForkStatus>,
    headers: RwLock<HashMap<u64, BlockSnapshot>>,
}

impl ForkMonitor {
    pub fn new(
        state_provider: DynProvider,
        upstream_provider: Option<DynProvider>,
        poll_seconds: f64,
    ) -> Self {
        Self {
            state_provider,
            upstream_provider,
            interval: Duration::from_secs_f64(poll_seconds.max(0.5)),
            refresh_lock: AsyncMutex::new(()),
            status: RwLock::new(ForkStatus::default()),
            headers: RwLock::new(HashMap::new()),
        }
    }

    pub fn status(&self) -> ForkStatus {
        self.status.read().expect("status lock").clone()
    }

    pub async fn current_status(&self) -> ForkStatus {
        let cur = self.status();
        if cur.block.is_some() {
            return cur;
        }
        self.refresh().await;
        self.status()
    }

    pub async fn snapshot_for(&self, block: u64) -> BlockSnapshot {
        if let Some(found) = self.headers.read().expect("headers lock").get(&block).cloned() {
            return found;
        }
        if let Some(fetched) = fetch_block_snapshot(&self.state_provider, BlockNumberOrTag::Number(block)).await {
            let mut guard = self.headers.write().expect("headers lock");
            if guard.len() >= 128 {
                guard.retain(|&k, _| k + 64 >= block);
            }
            guard.insert(block, fetched.clone());
            return fetched;
        }
        let mut fallback = self
            .status()
            .snapshot
            .unwrap_or_default();
        fallback.number = block;
        fallback
    }

    pub async fn refresh(&self) {
        let _guard = self.refresh_lock.lock().await;

        let state_snap = fetch_block_snapshot(&self.state_provider, BlockNumberOrTag::Latest).await;
        let state_block = match &state_snap {
            Some(s) => Some(s.number),
            None => match self.state_provider.get_block_number().await {
                Ok(n) => Some(n),
                Err(err) => {
                    warn!("State RPC head unavailable: {}", one_line_str(&err.to_string()));
                    None
                }
            },
        };

        let upstream_head = match &self.upstream_provider {
            Some(up) => match up.get_block_number().await {
                Ok(n) => Some(n),
                Err(err) => {
                    debug!("Upstream head read failed: {}", one_line_str(&err.to_string()));
                    state_block
                }
            },
            None => state_block,
        };

        if let Some(snap) = &state_snap {
            let mut hguard = self.headers.write().expect("headers lock");
            if hguard.len() >= 128 {
                let newest = snap.number;
                hguard.retain(|&k, _| k + 64 >= newest);
            }
            hguard.insert(snap.number, snap.clone());
        }

        let mut sguard = self.status.write().expect("status lock");
        if let Some(b) = state_block {
            sguard.block = Some(b);
        }
        if let Some(h) = upstream_head {
            sguard.head = Some(h);
        }
        if let Some(snap) = state_snap {
            sguard.snapshot = Some(snap);
        }
        sguard.lag = match (sguard.block, sguard.head) {
            (Some(b), Some(h)) => Some(h.saturating_sub(b)),
            _ => None,
        };
    }

    pub async fn poll_forever(self: Arc<Self>) {
        let mut previous: Option<u64> = None;
        loop {
            self.refresh().await;
            let cur = self.status();
            if let Some(block) = cur.block {
                if Some(block) != previous {
                    if previous.is_none() {
                        info!(
                            "State head block {}; upstream head {:?}; lag {:?}",
                            block, cur.head, cur.lag
                        );
                    } else {
                        info!("State head advanced to block {}; lag {:?}", block, cur.lag);
                    }
                    previous = Some(block);
                }
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

async fn fetch_block_snapshot(
    provider: &DynProvider,
    tag: BlockNumberOrTag,
) -> Option<BlockSnapshot> {
    let block = provider.get_block_by_number(tag).await.ok().flatten()?;
    let header = block.header();
    Some(BlockSnapshot {
        number: header.number(),
        timestamp: header.timestamp(),
        base_fee: header.base_fee_per_gas().unwrap_or(1_000_000_000),
        beneficiary: header.beneficiary(),
        gas_limit: header.gas_limit(),
        prevrandao: header.mix_hash(),
        excess_blob_gas: header.excess_blob_gas(),
    })
}

pub fn one_line_str(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(180).collect()
}

#[derive(Clone, Debug)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateError {}
impl DBErrorMarker for StateError {}

pub trait StateBackend: Send + Sync {
    fn basic(
        &self,
        block: u64,
        address: Address,
    ) -> BoxFuture<'_, Result<Option<AccountInfo>, StateError>>;

    fn code_by_hash(&self, code_hash: B256) -> BoxFuture<'_, Result<Bytecode, StateError>>;

    fn storage(
        &self,
        block: u64,
        address: Address,
        slot: StorageKey,
    ) -> BoxFuture<'_, Result<StorageValue, StateError>>;

    fn block_hash(&self, number: u64) -> BoxFuture<'_, Result<B256, StateError>>;

    fn has_code(&self, block: u64, address: Address) -> BoxFuture<'_, Result<bool, StateError>> {
        Box::pin(async move {
            match self.basic(block, address).await? {
                Some(info) => Ok(info.code_hash != KECCAK_EMPTY),
                None => Ok(false),
            }
        })
    }
}

#[derive(Default)]
struct WindowedCache {
    newest_block: u64,
    accounts: HashMap<(u64, Address), Option<AccountInfo>>,
    storage: HashMap<(u64, Address, StorageKey), StorageValue>,
}

impl WindowedCache {
    fn advance(&mut self, block: u64) {
        if block > self.newest_block {
            self.newest_block = block;
            let cutoff = block.saturating_sub(BLOCK_WINDOW);
            self.accounts.retain(|&(b, _), _| b >= cutoff);
            self.storage.retain(|&(b, _, _), _| b >= cutoff);
        }
    }
}

/// RPC-backed state provider with shared cross-worker bytecode, account, and storage caches.
pub struct RpcStateBackend {
    provider: DynProvider,
    bytecodes: RwLock<HashMap<B256, Bytecode>>,
    windowed: RwLock<WindowedCache>,
    block_hashes: RwLock<HashMap<u64, B256>>,
}

impl RpcStateBackend {
    pub fn new(provider: DynProvider) -> Self {
        Self {
            provider,
            bytecodes: RwLock::new(HashMap::new()),
            windowed: RwLock::new(WindowedCache::default()),
            block_hashes: RwLock::new(HashMap::new()),
        }
    }
}

impl StateBackend for RpcStateBackend {
    fn basic(
        &self,
        block: u64,
        address: Address,
    ) -> BoxFuture<'_, Result<Option<AccountInfo>, StateError>> {
        Box::pin(async move {
            if let Some(cached) = self
                .windowed
                .read()
                .expect("windowed read")
                .accounts
                .get(&(block, address))
                .cloned()
            {
                return Ok(cached);
            }

            let bid = BlockId::number(block);
            let nonce_fut = self.provider.get_transaction_count(address).block_id(bid);
            let bal_fut = self.provider.get_balance(address).block_id(bid);
            let code_fut = self.provider.get_code_at(address).block_id(bid);

            let (nonce, balance, raw_code) = tokio::try_join!(nonce_fut, bal_fut, code_fut)
                .map_err(|e| StateError(one_line_str(&e.to_string())))?;

            let info = if nonce == 0 && balance.is_zero() && raw_code.is_empty() {
                None
            } else {
                let (code_hash, bytecode) = if raw_code.is_empty() {
                    (KECCAK_EMPTY, Bytecode::default())
                } else {
                    let bc = Bytecode::new_raw(raw_code);
                    let hash = bc.hash_slow();
                    let mut cguard = self.bytecodes.write().expect("bytecode write");
                    if cguard.len() >= MAX_CODE_ENTRIES && !cguard.contains_key(&hash) {
                        if let Some(&victim) = cguard.keys().next() {
                            cguard.remove(&victim);
                        }
                    }
                    cguard.insert(hash, bc.clone());
                    (hash, bc)
                };
                Some(AccountInfo {
                    balance,
                    nonce,
                    code_hash,
                    code: Some(bytecode),
                    ..Default::default()
                })
            };

            let mut wguard = self.windowed.write().expect("windowed write");
            wguard.advance(block);
            if wguard.accounts.len() >= MAX_ACCOUNT_ENTRIES {
                if let Some(&k) = wguard.accounts.keys().next() {
                    wguard.accounts.remove(&k);
                }
            }
            wguard.accounts.insert((block, address), info.clone());
            Ok(info)
        })
    }

    fn code_by_hash(&self, code_hash: B256) -> BoxFuture<'_, Result<Bytecode, StateError>> {
        Box::pin(async move {
            if code_hash == KECCAK_EMPTY || code_hash == B256::ZERO {
                return Ok(Bytecode::default());
            }
            if let Some(bc) = self
                .bytecodes
                .read()
                .expect("bytecode read")
                .get(&code_hash)
                .cloned()
            {
                return Ok(bc);
            }
            Ok(Bytecode::default())
        })
    }

    fn storage(
        &self,
        block: u64,
        address: Address,
        slot: StorageKey,
    ) -> BoxFuture<'_, Result<StorageValue, StateError>> {
        Box::pin(async move {
            if let Some(&val) = self
                .windowed
                .read()
                .expect("windowed read")
                .storage
                .get(&(block, address, slot))
            {
                return Ok(val);
            }

            let bid = BlockId::number(block);
            let value: U256 = self
                .provider
                .get_storage_at(address, slot)
                .block_id(bid)
                .await
                .map_err(|e| StateError(one_line_str(&e.to_string())))?;

            let mut wguard = self.windowed.write().expect("windowed write");
            wguard.advance(block);
            if wguard.storage.len() >= MAX_STORAGE_ENTRIES {
                if let Some(&k) = wguard.storage.keys().next() {
                    wguard.storage.remove(&k);
                }
            }
            wguard.storage.insert((block, address, slot), value);
            Ok(value)
        })
    }

    fn block_hash(&self, number: u64) -> BoxFuture<'_, Result<B256, StateError>> {
        Box::pin(async move {
            if let Some(&h) = self
                .block_hashes
                .read()
                .expect("block_hashes read")
                .get(&number)
            {
                return Ok(h);
            }

            let maybe_block = self
                .provider
                .get_block_by_number(BlockNumberOrTag::Number(number))
                .await
                .map_err(|e| StateError(one_line_str(&e.to_string())))?;

            let hash = maybe_block
                .map(|b| b.header().hash())
                .unwrap_or(B256::ZERO);

            let mut bguard = self.block_hashes.write().expect("block_hashes write");
            if bguard.len() >= MAX_BLOCK_HASHES {
                if let Some(&k) = bguard.keys().next() {
                    bguard.remove(&k);
                }
            }
            bguard.insert(number, hash);
            Ok(hash)
        })
    }
}

/// Deterministic in-memory backend for fast, zero-network `revm` unit and integration tests.
#[derive(Default)]
pub struct InMemoryStateBackend {
    accounts: RwLock<HashMap<(u64, Address), AccountInfo>>,
    default_accounts: RwLock<HashMap<Address, AccountInfo>>,
    storage: RwLock<HashMap<(u64, Address, StorageKey), StorageValue>>,
    default_storage: RwLock<HashMap<(Address, StorageKey), StorageValue>>,
    codes: RwLock<HashMap<B256, Bytecode>>,
}

impl InMemoryStateBackend {
    pub fn insert_account(&self, address: Address, balance: U256, code_bytes: Option<Vec<u8>>) {
        let (code_hash, code) = match code_bytes {
            Some(raw) if !raw.is_empty() => {
                let bc = Bytecode::new_raw(raw.into());
                let h = bc.hash_slow();
                self.codes.write().unwrap().insert(h, bc.clone());
                (h, Some(bc))
            }
            _ => (KECCAK_EMPTY, Some(Bytecode::default())),
        };
        self.default_accounts.write().unwrap().insert(
            address,
            AccountInfo {
                balance,
                nonce: 1,
                code_hash,
                code,
                ..Default::default()
            },
        );
    }

    pub fn insert_storage(&self, address: Address, slot: StorageKey, value: StorageValue) {
        self.default_storage
            .write()
            .unwrap()
            .insert((address, slot), value);
    }

    pub fn insert_block_storage(
        &self,
        block: u64,
        address: Address,
        slot: StorageKey,
        value: StorageValue,
    ) {
        self.storage
            .write()
            .unwrap()
            .insert((block, address, slot), value);
    }

    pub fn insert_block_account(&self, block: u64, address: Address, info: AccountInfo) {
        self.accounts.write().unwrap().insert((block, address), info);
    }
}

impl StateBackend for InMemoryStateBackend {
    fn basic(
        &self,
        block: u64,
        address: Address,
    ) -> BoxFuture<'_, Result<Option<AccountInfo>, StateError>> {
        let res = self
            .accounts
            .read()
            .unwrap()
            .get(&(block, address))
            .cloned()
            .or_else(|| self.default_accounts.read().unwrap().get(&address).cloned());
        Box::pin(async move { Ok(res) })
    }

    fn code_by_hash(&self, code_hash: B256) -> BoxFuture<'_, Result<Bytecode, StateError>> {
        let bc = self
            .codes
            .read()
            .unwrap()
            .get(&code_hash)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(bc) })
    }

    fn storage(
        &self,
        block: u64,
        address: Address,
        slot: StorageKey,
    ) -> BoxFuture<'_, Result<StorageValue, StateError>> {
        let val = self
            .storage
            .read()
            .unwrap()
            .get(&(block, address, slot))
            .copied()
            .or_else(|| {
                self.default_storage
                    .read()
                    .unwrap()
                    .get(&(address, slot))
                    .copied()
            })
            .unwrap_or(StorageValue::ZERO);
        Box::pin(async move { Ok(val) })
    }

    fn block_hash(&self, _number: u64) -> BoxFuture<'_, Result<B256, StateError>> {
        Box::pin(async move { Ok(B256::ZERO) })
    }
}

/// Pins a `StateBackend` to a single block height for `revm::AsyncDb` + `CacheDB`.
#[derive(Clone)]
pub struct PinnedStateView {
    pub backend: Arc<dyn StateBackend>,
    pub block: u64,
}

impl DatabaseAsyncRef for PinnedStateView {
    type Error = StateError;

    async fn basic_async_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.backend.basic(self.block, address).await
    }

    async fn code_by_hash_async_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.backend.code_by_hash(code_hash).await
    }

    async fn storage_async_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.backend.storage(self.block, address, index).await
    }

    async fn block_hash_async_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.backend.block_hash(number).await
    }
}
