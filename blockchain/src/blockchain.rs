use std::cmp;
use std::time::Instant;
use std::sync::Arc;

use bigdecimal::BigDecimal;
use parking_lot::{MappedRwLockReadGuard, Mutex, RwLock, RwLockReadGuard};

use accounts::Accounts;
use database::{Environment, Transaction, ReadTransaction, WriteTransaction};
use hash::{Blake2bHash, Hash};
use network_primitives::networks::get_network_info;
use network_primitives::time::NetworkTime;
use primitives::account::AccountError;
use primitives::block::{Block, BlockHeader, BlockError, Target, TargetCompact, Difficulty};
use primitives::networks::NetworkId;
use primitives::policy;
use utils::iterators::Merge;
use utils::observer::Notifier;
use utils::unique_ptr::UniquePtr;

use crate::{chain_info::ChainInfo, chain_store::ChainStore, chain_store::Direction, chain_proof::ChainProof, transaction_cache::TransactionCache};
#[cfg(feature = "metrics")]
use crate::chain_metrics::BlockchainMetrics;

pub struct Blockchain<'env> {
    env: &'env Environment,
    pub network_id: NetworkId,
    network_time: Arc<NetworkTime>,
    pub notifier: RwLock<Notifier<'env, BlockchainEvent>>,
    chain_store: ChainStore<'env>,
    state: RwLock<BlockchainState<'env>>,
    push_lock: Mutex<()>,

    #[cfg(feature = "metrics")]
    pub metrics: BlockchainMetrics,
}

struct BlockchainState<'env> {
    accounts: Accounts<'env>,
    transaction_cache: TransactionCache,
    main_chain: ChainInfo,
    head_hash: Blake2bHash,
    chain_proof: Option<ChainProof>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PushResult {
    Invalid(PushError),
    Orphan,
    Known,
    Extended,
    Rebranched,
    Forked,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PushError {
    InvalidBlock(BlockError),
    InvalidSuccessor,
    DifficultyMismatch,
    DuplicateTransaction,
    AccountsError(AccountError),
    InvalidFork,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BlockchainEvent {
    Extended(Blake2bHash, UniquePtr<Block>),
    Rebranched(Vec<(Blake2bHash, Block)>, Vec<(Blake2bHash, Block)>),
}

impl<'env> Blockchain<'env> {
    const NIPOPOW_M: u32 = 240;
    const NIPOPOW_K: u32 = 120;
    const NIPOPOW_DELTA: f64 = 0.15;

    pub fn new(env: &'env Environment, network_id: NetworkId, network_time: Arc<NetworkTime>) -> Self {
        let chain_store = ChainStore::new(env);
        match chain_store.get_head(None) {
            Some(head_hash) => Blockchain::load(env, network_time, network_id, chain_store, head_hash),
            None => Blockchain::init(env, network_time, network_id, chain_store)
        }
    }

    fn load(env: &'env Environment, network_time: Arc<NetworkTime>, network_id: NetworkId, chain_store: ChainStore<'env>, head_hash: Blake2bHash) -> Self {
        // Check that the correct genesis block is stored.
        let network_info = get_network_info(network_id).unwrap();
        let genesis_info = chain_store.get_chain_info(&network_info.genesis_hash, false, None);
        assert!(genesis_info.is_some() && genesis_info.unwrap().on_main_chain,
            "Invalid genesis block stored. Reset your consensus database.");

        // Load main chain from store.
        let main_chain = chain_store
            .get_chain_info(&head_hash, true, None)
            .expect("Failed to load main chain. Reset your consensus database.");

        // Check that chain/accounts state is consistent.
        let accounts = Accounts::new(env);
        assert_eq!(main_chain.head.header.accounts_hash, accounts.hash(None),
            "Inconsistent chain/accounts state. Reset your consensus database.");

        // Initialize TransactionCache.
        let mut transaction_cache = TransactionCache::new();
        let blocks = chain_store.get_blocks_backward(&head_hash, transaction_cache.missing_blocks() - 1, true, None);
        for block in blocks.iter().rev() {
            transaction_cache.push_block(block);
        }
        transaction_cache.push_block(&main_chain.head);
        assert_eq!(transaction_cache.missing_blocks(), policy::TRANSACTION_VALIDITY_WINDOW.saturating_sub(main_chain.head.header.height));

        Blockchain {
            env,
            network_id,
            network_time,
            notifier: RwLock::new(Notifier::new()),
            chain_store,
            state: RwLock::new(BlockchainState {
                accounts,
                transaction_cache,
                main_chain,
                head_hash,
                chain_proof: None,
            }),
            push_lock: Mutex::new(()),

            #[cfg(feature = "metrics")]
            metrics: BlockchainMetrics::default(),
        }
    }

    fn init(env: &'env Environment, network_time: Arc<NetworkTime>, network_id: NetworkId, chain_store: ChainStore<'env>) -> Self {
        // Initialize chain & accounts with genesis block.
        let network_info = get_network_info(network_id).expect(&format!("No NetworkInfo for network {:?}", network_id));
        let main_chain = ChainInfo::initial(network_info.genesis_block.clone());
        let head_hash = network_info.genesis_hash.clone();

        // Initialize accounts.
        let accounts = Accounts::new(env);
        let mut txn = WriteTransaction::new(env);
        accounts.init(&mut txn, network_id);

        // Store genesis block.
        chain_store.put_chain_info(&mut txn, &head_hash, &main_chain, true);
        chain_store.set_head(&mut txn, &head_hash);
        txn.commit();

        // Initialize empty TransactionCache.
        let transaction_cache = TransactionCache::new();

        Blockchain {
            env,
            network_id,
            network_time,
            notifier: RwLock::new(Notifier::new()),
            chain_store,
            state: RwLock::new(BlockchainState {
                accounts,
                transaction_cache,
                main_chain,
                head_hash,
                chain_proof: None,
            }),
            push_lock: Mutex::new(()),

            #[cfg(feature = "metrics")]
            metrics: BlockchainMetrics::default(),
        }
    }

    pub fn push(&self, block: Block) -> PushResult {
        // We expect full blocks (with body).
        assert!(block.body.is_some(), "Block body expected");

        // Check (sort of) intrinsic block invariants.
        let info = get_network_info(self.network_id).unwrap();
        if let Err(e) = block.verify(self.network_time.now(), self.network_id, info.genesis_block.header.hash()) {
            warn!("Rejecting block - verification failed ({:?})", e);
            #[cfg(feature = "metrics")]
            self.metrics.note_invalid_block();
            return PushResult::Invalid(PushError::InvalidBlock(e))
        }

        // Only one push operation at a time.
        let lock = self.push_lock.lock();

        // Check if we already know this block.
        let hash: Blake2bHash = block.header.hash();
        if self.chain_store.get_chain_info(&hash, false, None).is_some() {
            #[cfg(feature = "metrics")]
            self.metrics.note_known_block();
            return PushResult::Known;
        }

        // Check if the block's immediate predecessor is part of the chain.
        let prev_info_opt = self.chain_store.get_chain_info(&block.header.prev_hash, false, None);
        if prev_info_opt.is_none() {
            warn!("Rejecting block - unknown predecessor");
            #[cfg(feature = "metrics")]
            self.metrics.note_orphan_block();
            return PushResult::Orphan;
        }

        // Check that the block is a valid successor of its predecessor.
        let prev_info = prev_info_opt.unwrap();
        if !block.is_immediate_successor_of(&prev_info.head) {
            warn!("Rejecting block - not a valid successor");
            #[cfg(feature = "metrics")]
            self.metrics.note_invalid_block();
            return PushResult::Invalid(PushError::InvalidSuccessor);
        }

        // Check that the difficulty is correct.
        let next_target = self.get_next_target(Some(&block.header.prev_hash));
        if block.header.n_bits != TargetCompact::from(next_target) {
            warn!("Rejecting block - difficulty mismatch");
            #[cfg(feature = "metrics")]
            self.metrics.note_invalid_block();
            return PushResult::Invalid(PushError::DifficultyMismatch);
        }

        // Block looks good, create ChainInfo.
        let chain_info = prev_info.next(block);

        // Check if the block extends our current main chain.
        if chain_info.head.header.prev_hash == self.state.read().head_hash {
            return self.extend(hash, chain_info, prev_info);
        }

        // Otherwise, check if the new chain is harder than our current main chain.
        if chain_info.total_difficulty > self.state.read().main_chain.total_difficulty {
            // A fork has become the hardest chain, rebranch to it.
            return self.rebranch(hash, chain_info);
        }

        // Otherwise, we are creating/extending a fork. Store ChainInfo.
        debug!("Creating/extending fork with block {}, height #{}, total_difficulty {}", hash, chain_info.head.header.height, chain_info.total_difficulty);
        let mut txn = WriteTransaction::new(self.env);
        self.chain_store.put_chain_info(&mut txn, &hash, &chain_info, true);
        txn.commit();

        #[cfg(feature = "metrics")]
        self.metrics.note_forked_block();
        return PushResult::Forked;
    }

    fn extend(&self, block_hash: Blake2bHash, mut chain_info: ChainInfo, mut prev_info: ChainInfo) -> PushResult {
        let mut txn = WriteTransaction::new(self.env);
        {
            let state = self.state.read();

            // Check transactions against TransactionCache to prevent replay.
            if state.transaction_cache.contains_any(&chain_info.head) {
                warn!("Rejecting block - transaction already included");
                txn.abort();
                #[cfg(feature = "metrics")]
                self.metrics.note_invalid_block();
                return PushResult::Invalid(PushError::DuplicateTransaction);
            }

            // Commit block to AccountsTree.
            if let Err(e) = state.accounts.commit_block(&mut txn, &chain_info.head) {
                warn!("Rejecting block - commit failed: {}", e);
                txn.abort();
                #[cfg(feature = "metrics")]
                self.metrics.note_invalid_block();
                return PushResult::Invalid(PushError::AccountsError(e));
            }
        }

        chain_info.on_main_chain = true;
        prev_info.main_chain_successor = Some(block_hash.clone());

        self.chain_store.put_chain_info(&mut txn, &block_hash, &chain_info, true);
        self.chain_store.put_chain_info(&mut txn, &chain_info.head.header.prev_hash, &prev_info, false);
        self.chain_store.set_head(&mut txn, &block_hash);

        {
            // Acquire write lock.
            let mut state = self.state.write();

            state.transaction_cache.push_block(&chain_info.head);

            state.main_chain = chain_info;
            state.head_hash = block_hash;

            state.chain_proof = None;

            txn.commit();
        }

        // Give up write lock before notifying.
        let state = self.state.read();
        let event = BlockchainEvent::Extended(state.head_hash.clone(), UniquePtr::new(&state.main_chain.head));
        self.notifier.read().notify(event);

        #[cfg(feature = "metrics")]
        self.metrics.note_extended_block();
        return PushResult::Extended;
    }

    fn rebranch(&self, block_hash: Blake2bHash, chain_info: ChainInfo) -> PushResult {
        debug!("Rebranching to fork {}, height #{}, total_difficulty {}", block_hash, chain_info.head.header.height, chain_info.total_difficulty);

        // Find the common ancestor between our current main chain and the fork chain.
        // Walk up the fork chain until we find a block that is part of the main chain.
        // Store the chain along the way.
        let read_txn = ReadTransaction::new(self.env);

        let mut fork_chain: Vec<(Blake2bHash, ChainInfo)> = vec![];
        let mut current: (Blake2bHash, ChainInfo) = (block_hash, chain_info);
        while !current.1.on_main_chain {
            let prev_hash = current.1.head.header.prev_hash.clone();
            let prev_info = self.chain_store
                .get_chain_info(&prev_hash, true, Some(&read_txn))
                .expect("Corrupted store: Failed to find fork predecessor while rebranching");

            fork_chain.push(current);
            current = (prev_hash, prev_info);
        }

        debug!("Found common ancestor {} at height #{}, {} blocks up", current.0, current.1.head.header.height, fork_chain.len());

        // Revert AccountsTree & TransactionCache to the common ancestor state.
        let mut revert_chain: Vec<(Blake2bHash, ChainInfo)> = vec![];
        let mut ancestor = current;

        let mut write_txn = WriteTransaction::new(self.env);
        let mut cache_txn;
        {
            let state = self.state.read();

            cache_txn = state.transaction_cache.clone();
            // XXX Get rid of the .clone() here.
            current = (state.head_hash.clone(), state.main_chain.clone());

            while current.0 != ancestor.0 {
                if let Err(e) = state.accounts.revert_block(&mut write_txn, &current.1.head) {
                    panic!("Failed to revert main chain while rebranching - {}", e);
                }

                cache_txn.revert_block(&current.1.head);

                let prev_hash = current.1.head.header.prev_hash.clone();
                let prev_info = self.chain_store
                    .get_chain_info(&prev_hash, true, Some(&read_txn))
                    .expect("Corrupted store: Failed to find main chain predecessor while rebranching");

                assert_eq!(prev_info.head.header.accounts_hash, state.accounts.hash(Some(&write_txn)),
                           "Failed to revert main chain while rebranching - inconsistent state");

                revert_chain.push(current);
                current = (prev_hash, prev_info);
            }

            // Fetch missing blocks for TransactionCache.
            assert!(cache_txn.is_empty() || cache_txn.head_hash() == ancestor.0);
            let start_hash = if cache_txn.is_empty() {
                ancestor.1.main_chain_successor.unwrap()
            } else {
                cache_txn.tail_hash()
            };
            let blocks = self.chain_store.get_blocks_backward(&start_hash, cache_txn.missing_blocks(), true, Some(&read_txn));
            for block in blocks.iter() {
                cache_txn.prepend_block(block);
            }
            assert_eq!(cache_txn.missing_blocks(), policy::TRANSACTION_VALIDITY_WINDOW.saturating_sub(ancestor.1.head.header.height));

            // Check each fork block against TransactionCache & commit to AccountsTree.
            for fork_block in fork_chain.iter().rev() {
                if cache_txn.contains_any(&fork_block.1.head) {
                    warn!("Failed to apply fork block while rebranching - transaction already included");
                    // TODO delete invalid fork from store
                    write_txn.abort();
                    #[cfg(feature = "metrics")]
                    self.metrics.note_invalid_block();
                    return PushResult::Invalid(PushError::InvalidFork);
                }

                if let Err(e) = state.accounts.commit_block(&mut write_txn, &fork_block.1.head) {
                    warn!("Failed to apply fork block while rebranching - {}", e);
                    // TODO delete invalid fork from store
                    write_txn.abort();
                    #[cfg(feature = "metrics")]
                    self.metrics.note_invalid_block();
                    return PushResult::Invalid(PushError::InvalidFork);
                }

                cache_txn.push_block(&fork_block.1.head);
            }
        }

        // Fork looks good.

        {
            // Acquire write lock.
            let mut state = self.state.write();

            // Unset onMainChain flag / mainChainSuccessor on the current main chain up to (excluding) the common ancestor.
            for reverted_block in revert_chain.iter_mut() {
                reverted_block.1.on_main_chain = false;
                reverted_block.1.main_chain_successor = None;
                self.chain_store.put_chain_info(&mut write_txn, &reverted_block.0, &reverted_block.1, false);
            }

            // Update the mainChainSuccessor of the common ancestor block.
            ancestor.1.main_chain_successor = Some(fork_chain.last().unwrap().0.clone());
            self.chain_store.put_chain_info(&mut write_txn, &ancestor.0, &ancestor.1, false);

            // Set onMainChain flag / mainChainSuccessor on the fork.
            for i in (0..fork_chain.len()).rev() {
                let main_chain_successor = match i > 0 {
                    true => Some(fork_chain[i - 1].0.clone()),
                    false => None
                };

                let fork_block = &mut fork_chain[i];
                fork_block.1.on_main_chain = true;
                fork_block.1.main_chain_successor = main_chain_successor;

                // Include the body of the new block (at position 0).
                self.chain_store.put_chain_info(&mut write_txn, &fork_block.0, &fork_block.1, i == 0);
            }

            // Commit transaction & update head.
            write_txn.commit();
            state.transaction_cache = cache_txn;

            state.main_chain = fork_chain[0].1.clone();
            state.head_hash = fork_chain[0].0.clone();

            // Reset chain proof.
            state.chain_proof = None;
        }

        // Give up write lock before notifying.
        let mut reverted_blocks = Vec::with_capacity(revert_chain.len());
        for (hash, chain_info) in revert_chain.into_iter().rev() {
            reverted_blocks.push((hash, chain_info.head));
        }
        let mut adopted_blocks = Vec::with_capacity(fork_chain.len());
        for (hash, chain_info) in fork_chain.into_iter().rev() {
            adopted_blocks.push((hash, chain_info.head));
        }
        let event = BlockchainEvent::Rebranched(reverted_blocks, adopted_blocks);
        self.notifier.read().notify(event);

        #[cfg(feature = "metrics")]
        self.metrics.note_rebranched_block();
        return PushResult::Rebranched;
    }

    pub fn get_next_target(&self, head_hash: Option<&Blake2bHash>) -> Target {
        let state = self.state.read();

        let chain_info;
        let head_info = match head_hash {
            Some(hash) => {
                chain_info = self.chain_store
                    .get_chain_info(hash, false, None)
                    .expect("Failed to compute next target - unknown head_hash");
                &chain_info
            }
            None => &state.main_chain
        };

        let tail_height = 1u32.max(head_info.head.header.height.saturating_sub(policy::DIFFICULTY_BLOCK_WINDOW));
        let tail_info;
        if head_info.on_main_chain {
            tail_info = self.chain_store
                .get_chain_info_at(tail_height, false, None)
                .expect("Failed to compute next target - tail block not found");
        } else {
            let mut prev_info;
            let mut prev_hash = head_info.head.header.prev_hash.clone();
            let mut i = 0;
            // XXX Mimic do ... while {} loop control flow.
            while {
                // Loop condition
                prev_info = self.chain_store
                    .get_chain_info(&prev_hash, false, None)
                    .expect("Failed to compute next target - fork predecessor not found");
                prev_hash = prev_info.head.header.prev_hash.clone();

                i < policy::DIFFICULTY_BLOCK_WINDOW && !prev_info.on_main_chain
            } { /* Loop body */ i += 1; }

            if prev_info.on_main_chain && prev_info.head.header.height > tail_height {
                tail_info = self.chain_store
                    .get_chain_info_at(tail_height, false, None)
                    .expect("Failed to compute next target - tail block not found");
            } else {
                tail_info = prev_info;
            }
        }

        let head = &head_info.head.header;
        let tail = &tail_info.head.header;
        assert!(head.height - tail.height == policy::DIFFICULTY_BLOCK_WINDOW
            || (head.height <= policy::DIFFICULTY_BLOCK_WINDOW && tail.height == 1),
            "Failed to compute next target - invalid head/tail block");

        let mut delta_total_difficulty = &head_info.total_difficulty - &tail_info.total_difficulty;
        let mut actual_time = head.timestamp - tail.timestamp;

        // Simulate that the Policy.BLOCK_TIME was achieved for the blocks before the genesis block, i.e. we simulate
        // a sliding window that starts before the genesis block. Assume difficulty = 1 for these blocks.
        if head.height <= policy::DIFFICULTY_BLOCK_WINDOW {
            actual_time += (policy::DIFFICULTY_BLOCK_WINDOW - head.height + 1) * policy::BLOCK_TIME;
            delta_total_difficulty += BigDecimal::from(policy::DIFFICULTY_BLOCK_WINDOW - head.height + 1).into();
        }

        // Compute the target adjustment factor.
        let expected_time = policy::DIFFICULTY_BLOCK_WINDOW * policy::BLOCK_TIME;
        let mut adjustment = actual_time as f64 / expected_time as f64;

        // Clamp the adjustment factor to [1 / MAX_ADJUSTMENT_FACTOR, MAX_ADJUSTMENT_FACTOR].
        adjustment = adjustment.max(1f64 / policy::DIFFICULTY_MAX_ADJUSTMENT_FACTOR);
        adjustment = adjustment.min(policy::DIFFICULTY_MAX_ADJUSTMENT_FACTOR);

        // Compute the next target.
        let average_difficulty = BigDecimal::from(delta_total_difficulty) / BigDecimal::from(policy::DIFFICULTY_BLOCK_WINDOW);
        let average_target = &*policy::BLOCK_TARGET_MAX / average_difficulty; // Do not use Difficulty -> Target conversion here to preserve precision.
        let mut next_target = average_target * BigDecimal::from(adjustment);

        // Make sure the target is below or equal the maximum allowed target (difficulty 1).
        // Also enforce a minimum target of 1.
        if next_target > *policy::BLOCK_TARGET_MAX {
            next_target = policy::BLOCK_TARGET_MAX.clone();
        }
        let min_target = BigDecimal::from(1);
        if next_target < min_target {
            next_target = min_target;
        }

        // XXX Reduce target precision to nBits precision.
        let n_bits: TargetCompact = Target::from(next_target).into();
        return Target::from(n_bits);
    }

    pub fn get_block_locators(&self, max_count: usize) -> Vec<Blake2bHash> {
        // Push top 10 hashes first, then back off exponentially.
        let mut hash = self.head_hash();
        let mut locators = vec![hash.clone()];

        for _ in 0..cmp::min(10, self.height()) {
            let block = self.chain_store.get_block(&hash, false, None);
            match block {
                Some(block) => {
                    hash = block.header.prev_hash.clone();
                    locators.push(hash.clone());
                },
                None => break,
            }
        }

        let mut step = 2;
        let mut height = self.height().saturating_sub(10 + step);
        let mut opt_block = self.chain_store.get_block_at(height);
        while let Some(block) = opt_block {
            locators.push(block.header.hash());

            // Respect max count.
            if locators.len() >= max_count {
                break;
            }

            step *= 2;
            height = match height.checked_sub(step) {
                Some(0) => break, // 0 or underflow means we need to end the loop
                Some(v) => v,
                None => break,
            };

            opt_block = self.chain_store.get_block_at(height);
        }

        // Push the genesis block hash.
        let network_info = get_network_info(self.network_id).unwrap();
        if locators.is_empty() || locators.last().unwrap() != &network_info.genesis_hash {
            // Respect max count, make space for genesis hash if necessary
            if locators.len() >= max_count {
                locators.pop();
            }
            locators.push(network_info.genesis_hash.clone());
        }

        locators
    }

    pub fn contains(&self, hash: &Blake2bHash, include_forks: bool) -> bool {
        match self.chain_store.get_chain_info(hash, false, None) {
            Some(chain_info) => include_forks || chain_info.on_main_chain,
            None => false
        }
    }

    pub fn get_block_at(&self, height: u32, include_body: bool) -> Option<Block> {
        self.chain_store.get_chain_info_at(height, include_body, None).map(|chain_info| chain_info.head)
    }

    pub fn get_block(&self, hash: &Blake2bHash, include_forks: bool, include_body: bool) -> Option<Block> {
        let chain_info_opt = self.chain_store.get_chain_info(hash, include_body, None);
        if chain_info_opt.is_some() {
            let chain_info = chain_info_opt.unwrap();
            if chain_info.on_main_chain || include_forks {
                return Some(chain_info.head);
            }
        }
        None
    }

    pub fn get_blocks(&self, start_block_hash: &Blake2bHash, count: u32, include_body: bool, direction: Direction) -> Vec<Block> {
        self.chain_store.get_blocks(start_block_hash, count, include_body, direction, None)
    }

    pub fn head_hash(&self) -> Blake2bHash {
        self.state.read().head_hash.clone()
    }

    pub fn height(&self) -> u32 {
        self.state.read().main_chain.head.header.height
    }

    pub fn head(&self) -> MappedRwLockReadGuard<Block> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| &s.main_chain.head)
    }

    pub fn total_work(&self) -> MappedRwLockReadGuard<Difficulty> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| &s.main_chain.total_work)
    }

    pub fn accounts(&self) -> MappedRwLockReadGuard<Accounts<'env>> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| &s.accounts)
    }

    pub fn transaction_cache(&self) -> MappedRwLockReadGuard<TransactionCache> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| &s.transaction_cache)
    }


    /* NiPoPoW prover */

    pub fn get_chain_proof(&self) -> ChainProof {
        let mut state = self.state.write();
        if state.chain_proof.is_none() {
            let start = Instant::now();
            let chain_proof = self.prove(&state.main_chain.head, Self::NIPOPOW_M, Self::NIPOPOW_K, Self::NIPOPOW_DELTA);
            trace!("Chain proof took {}ms to compute (prefix={}, suffix={})", utils::time::duration_as_millis(&(Instant::now() - start)), chain_proof.prefix.len(), chain_proof.suffix.len());
            state.chain_proof = Some(chain_proof);
        }
        // XXX Get rid of the clone here? ChainProof is typically >1mb.
        state.chain_proof.as_ref().unwrap().clone()
    }

    fn prove(&self, head: &Block, m: u32, k: u32, delta: f64) -> ChainProof {
        let mut prefix = vec![];
        let mut start_height = 1u32;

        let txn = ReadTransaction::new(self.env);
        let head_info = self.chain_store
            .get_chain_info_at(u32::max(head.header.height.saturating_sub(k), 1), false, Some(&txn))
            .expect("Failed to compute chain proof - prefix head block not found");
        let max_depth = head_info.super_block_counts.get_candidate_depth(m);

        for depth in (0..=max_depth).rev() {
            let super_chain = self.get_super_chain(depth, &head_info, start_height, Some(&txn));
            if super_chain.is_good(depth, m, delta) {
                assert!(super_chain.0.len() >= m as usize, "Good superchain too short");
                trace!("Found good superchain at depth {} with length {} (#{} - #{})", depth, super_chain.0.len(), start_height, head_info.head.header.height);
                start_height = super_chain.0[super_chain.0.len() - m as usize].head.header.height;
            }

            let merged = Merge::new(
                prefix.into_iter(),
                super_chain.0.into_iter().map(|chain_info| chain_info.head),
                |l, r| u32::cmp(&l.header.height, &r.header.height));
            prefix = merged.collect();
        }

        let suffix = self.get_header_chain(head.header.height - head_info.head.header.height, &head, Some(&txn));

        ChainProof { prefix, suffix }
    }

    fn get_super_chain(&self, depth: u8, head_info: &ChainInfo, tail_height: u32, txn_option: Option<&Transaction>) -> SuperChain {
        assert!(tail_height >= 1, "Tail height must be >= 1");
        let mut chain = vec![];

        // Include head if it is at the requested depth or below.
        let head_depth = Target::from(&head_info.head.header.pow()).get_depth();
        if head_depth >= depth {
            chain.push(head_info.clone());
        }

        let mut block;
        let mut head = &head_info.head;
        let mut j = i16::max(depth as i16 - Target::from(head.header.n_bits).get_depth() as i16, -1);
        while j < head.interlink.hashes.len() as i16 && head.header.height > tail_height {
            let reference = if j < 0 {
                &head.header.prev_hash
            } else {
                &head.interlink.hashes[j as usize]
            };

            let chain_info = self.chain_store
                .get_chain_info(reference, false, txn_option)
                .expect("Failed to construct superchain - missing block");
            block = chain_info.head.clone();
            chain.push(chain_info);

            head = &block;
            j = i16::max(depth as i16 - Target::from(head.header.n_bits).get_depth() as i16, -1);
        }

        if (chain.is_empty() || chain[chain.len() - 1].head.header.height > 1) && tail_height == 1 {
            let mut genesis_block = get_network_info(self.network_id).unwrap().genesis_block.clone();
            genesis_block.body = None;
            chain.push(ChainInfo::initial(genesis_block));
        }

        chain.reverse();
        SuperChain(chain)
    }

    fn get_header_chain(&self, length: u32, head: &Block, txn_option: Option<&Transaction>) -> Vec<BlockHeader> {
        let mut headers = vec![];

        if length > 0 {
            headers.push(head.header.clone());
        }

        let mut prev_hash = head.header.prev_hash.clone();
        let mut height = head.header.height;
        while headers.len() < length as usize && height > 1 {
            let block = self.chain_store
                .get_block(&prev_hash, false, txn_option)
                .expect("Failed to construct header chain - missing block");

            prev_hash = block.header.prev_hash.clone();
            height = block.header.height;

            headers.push(block.header);
        }

        headers.reverse();
        headers
    }
}

struct SuperChain(Vec<ChainInfo>);
impl SuperChain {
    pub fn is_good(&self, depth: u8, m: u32, delta: f64) -> bool {
        self.has_super_quality(depth, m, delta) && self.has_multi_level_quality(depth, m, delta)
    }

    fn has_super_quality(&self, depth: u8, m: u32, delta: f64) -> bool {
        let length = self.0.len();
        if length < m as usize {
            return false;
        }

        for i in m as usize..=length {
            let underlying_length = self.0[length - 1].head.header.height - self.0[length - i].head.header.height + 1;
            if !SuperChain::is_locally_good(i as u32, underlying_length, depth, delta) {
                return false;
            }
        }

        return true;
    }

    fn has_multi_level_quality(&self, depth: u8, k1: u32, delta: f64) -> bool {
        if depth == 0 {
            return true;
        }

        for i in 0..(self.0.len() - k1 as usize) {
            let tail_info = &self.0[i];
            let head_info = &self.0[i + k1 as usize];

            for mu in (1..=depth).rev() {
                let upper_chain_length = head_info.super_block_counts.get(mu) - tail_info.super_block_counts.get(mu);

                // Moderate badness check:
                for j in (0..=mu - 1).rev() {
                    let lower_chain_length = head_info.super_block_counts.get(j) - tail_info.super_block_counts.get(j);
                    if !SuperChain::is_locally_good(upper_chain_length, lower_chain_length, mu - j, delta) {
                        trace!("Chain badness detected at depth {}[{}:{}], failing at {}/{}", depth, i, i + k1 as usize, mu, j);
                        return false;
                    }
                }
            }
        }

        return true;
    }

    fn is_locally_good(super_length: u32, underlying_length: u32, depth: u8, delta: f64) -> bool {
        super_length as f64 > (1f64 - delta) * 2f64.powi(-(depth as i32)) * underlying_length as f64
    }
}
