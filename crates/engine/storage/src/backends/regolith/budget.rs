//! A memory budget for the store's engine.
//!
//! The server profile sizes the engine for a machine of its own: a 512 MiB
//! block cache, two 64 MiB write buffers and their recycling pool, and every
//! open table's index and filters pinned outside the cache. That is about
//! 900 MiB before the first index grows, which a small machine, or a machine
//! that runs several nodes, cannot give. A budget divides one number between
//! the parts the engine holds, and puts the index and filters inside it.

use super::config::RegolithStoreOptions;

/// The smallest write buffer a budget gives: below it a flush per few
/// writes costs more than the memory saves.
const MIN_WRITE_BUFFER: usize = 1 << 20;

/// The largest: the server profile's.
const MAX_WRITE_BUFFER: usize = 64 << 20;

/// A block cache shard holds at least this much, so one large block (a
/// blob's value is one block) does not empty its shard to be admitted.
const MIN_SHARD_BYTES: usize = 4 << 20;

/// The server profile's shard count, as bits.
const MAX_SHARD_BITS: u32 = 6;

/// How one budget is divided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    /// The block cache's capacity: data, index and filter blocks.
    pub block_cache: usize,
    /// One memtable's size. Two are written and two may wait for their
    /// flush, and the arena pool parks two more for reuse, so the memtables
    /// can hold six of these at once.
    pub write_buffer: usize,
    /// Block cache shards, as bits.
    pub shard_bits: u32,
}

impl MemoryBudget {
    /// Divide `bytes` for reads first: half for the block cache, and at most
    /// six sixteenths for the memtables, which leaves an eighth for the arena
    /// chunks a memtable reserves past its size. Below 16 MiB the smallest
    /// write buffer no longer fits.
    pub fn of(bytes: usize) -> Self {
        Self::divided(bytes, bytes / 2, bytes / 16)
    }

    /// Divide `bytes` for writes first: a quarter for the block cache, and at
    /// most six tenths for the memtables. A store of large values that are
    /// written once and seldom read again (recordings, files) gains little
    /// from a cache, and a larger write buffer flushes fewer, larger tables,
    /// which compaction then rewrites less often: taking in 120 KiB blobs,
    /// the device wrote 80 times the payload with 16 MiB write buffers, 62
    /// with 26 MiB, 45 with 40 MiB and 32 with 64 MiB.
    pub fn for_writes(bytes: usize) -> Self {
        Self::divided(bytes, bytes / 4, bytes / 10)
    }

    fn divided(bytes: usize, block_cache: usize, write_buffer: usize) -> Self {
        let block_cache = block_cache.min(bytes);
        let write_buffer = write_buffer.clamp(MIN_WRITE_BUFFER, MAX_WRITE_BUFFER);
        let shards = (block_cache / MIN_SHARD_BYTES).max(1);
        let shard_bits = shards.ilog2().min(MAX_SHARD_BITS);
        Self {
            block_cache,
            write_buffer,
            shard_bits,
        }
    }
}

impl RegolithStoreOptions {
    /// Bound what the engine holds in memory by `bytes`, divided as
    /// [`MemoryBudget::of`] says.
    pub fn with_memory_budget(self, bytes: usize) -> Self {
        self.with_memory(MemoryBudget::of(bytes))
    }

    /// Bound what the engine holds in memory as `budget` divides it. Index
    /// and filter blocks are read through the block cache, so the budget
    /// holds however many tables the store has: a point read pays one cache
    /// lookup for them, and a read from disk when they were evicted.
    ///
    /// What the engine holds is not all a node holds: DefraDB's own queues
    /// and the network have bounds of their own (`SyncTuning`), and the
    /// allocator keeps some freed memory.
    pub fn with_memory(mut self, budget: MemoryBudget) -> Self {
        self.engine = self
            .engine
            .block_cache_size(budget.block_cache)
            .block_cache_num_shard_bits(budget.shard_bits)
            .write_buffer_size(budget.write_buffer)
            .max_write_buffer_number(2)
            .cache_index_and_filter_blocks(true);
        self
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::MemoryBudget;
    use crate::corekv::{Reader, Store};
    use crate::{RegolithStore, RegolithStoreOptions};

    #[test]
    fn a_budget_is_divided_within_itself() {
        for mib in [16usize, 64, 256, 1024, 4096] {
            let bytes = mib << 20;
            for budget in [MemoryBudget::of(bytes), MemoryBudget::for_writes(bytes)] {
                assert!(
                    budget.block_cache + 6 * budget.write_buffer <= bytes,
                    "{mib} MiB: {budget:?}"
                );
                assert!(
                    budget.block_cache >> budget.shard_bits >= 4 << 20 || budget.shard_bits == 0,
                    "{mib} MiB: {budget:?}"
                );
            }
        }
        assert_eq!(MemoryBudget::of(256 << 20).block_cache, 128 << 20);
        assert_eq!(MemoryBudget::of(256 << 20).write_buffer, 16 << 20);
        assert_eq!(MemoryBudget::for_writes(256 << 20).block_cache, 64 << 20);
        assert_eq!(
            MemoryBudget::for_writes(512 << 20).write_buffer,
            (512 << 20) / 10
        );
        assert_eq!(MemoryBudget::of(4096 << 20).write_buffer, 64 << 20);
        assert_eq!(MemoryBudget::of(4 << 20).write_buffer, 1 << 20);
    }

    /// Write and read back many times the budget of blob-sized values that
    /// do not compress: the engine's memory stays inside the budget, and the
    /// index and filters are in the cache rather than pinned beside it.
    #[test]
    fn the_engine_stays_inside_its_budget_while_the_store_grows() {
        let budget = 16usize << 20;
        stays_inside(budget, MemoryBudget::of(budget));
        stays_inside(budget, MemoryBudget::for_writes(budget));
    }

    fn stays_inside(budget: usize, division: MemoryBudget) {
        workers::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let mut options = RegolithStoreOptions::default().with_memory(division);
            options.engine = options
                .engine
                .clone()
                .compression(regolith::CompressionType::None);
            let store = RegolithStore::open_with_options(dir.path(), options).unwrap();
            let mut state = 0x9e37_79b9_7f4a_7c15u64;
            let mut worst = 0u64;
            for batch in 0u32..80 {
                let mut txn = store.new_txn(false).await.unwrap();
                for n in batch * 8..batch * 8 + 8 {
                    let value: Vec<u8> = (0..240 * 1024 / 8)
                        .flat_map(|_| {
                            state ^= state << 13;
                            state ^= state >> 7;
                            state ^= state << 17;
                            state.to_le_bytes()
                        })
                        .collect();
                    txn.set(format!("blob/{n:08}").as_bytes(), &value)
                        .await
                        .unwrap();
                }
                txn.commit().await.unwrap();
                let txn = store.new_txn(true).await.unwrap();
                for n in (0..batch * 8).step_by(3) {
                    assert!(txn
                        .get(format!("blob/{n:08}").as_bytes())
                        .await
                        .unwrap()
                        .is_some());
                }
                txn.discard();
                let memory = store.memory();
                let held = memory.block_cache_bytes
                    + memory.memtable_reserved_bytes
                    + memory.arena_pool_bytes
                    + memory.pinned_metadata_bytes;
                worst = worst.max(held);
            }
            let memory = store.memory();
            assert!(memory.table_bytes > 4 * budget as u64, "{memory:?}");
            assert!(
                worst <= budget as u64,
                "held {worst} of a {budget} budget: {memory:?}"
            );
            assert_eq!(
                memory.block_cache_capacity, division.block_cache as u64,
                "{memory:?}"
            );
            assert_eq!(memory.pinned_metadata_bytes, 0, "{memory:?}");
        })
        .unwrap();
    }
}
