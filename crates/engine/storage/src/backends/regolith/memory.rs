//! What the engine holds in memory, by what holds it.

use super::store::RegolithStore;

/// The engine's memory at one moment, read from regolith's properties.
///
/// These are what the engine itself accounts for. The rest of a process's
/// heap (DefraDB's own structures, the network, the allocator's free pages)
/// is not in here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreMemory {
    /// Bytes the block cache charges for the blocks it holds.
    pub block_cache_bytes: u64,
    /// The most the block cache holds before it evicts.
    pub block_cache_capacity: u64,
    /// Entry bytes in the active and frozen memtables.
    pub memtable_bytes: u64,
    /// Arena bytes the active and frozen memtables have taken from the allocator.
    pub memtable_reserved_bytes: u64,
    /// Arena bytes parked for the next memtable.
    pub arena_pool_bytes: u64,
    /// Index and filter blocks held outside the block cache, for every table.
    pub pinned_metadata_bytes: u64,
    /// Snapshots alive.
    pub live_snapshots: u64,
    /// Bytes of every table on disk.
    pub table_bytes: u64,
}

impl RegolithStore {
    /// What the engine holds in memory now.
    pub fn memory(&self) -> StoreMemory {
        let db = self.engine_db();
        let read = |name: &str| db.get_int_property(name).unwrap_or(0);
        StoreMemory {
            block_cache_bytes: read("regolith.block-cache-usage"),
            block_cache_capacity: read("regolith.block-cache-capacity"),
            memtable_bytes: read("regolith.cur-size-all-mem-tables"),
            memtable_reserved_bytes: read("regolith.memtable-reserved-bytes"),
            arena_pool_bytes: read("regolith.arena-pool-bytes"),
            pinned_metadata_bytes: read("regolith.pinned-metadata-bytes"),
            live_snapshots: read("regolith.num-snapshots"),
            table_bytes: read("regolith.total-sst-files-size"),
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use crate::corekv::Store;
    use crate::RegolithStore;

    #[test]
    fn a_write_shows_in_the_memtables_and_the_cache_has_its_capacity() {
        workers::block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let store = RegolithStore::open(dir.path()).unwrap();
            let before = store.memory();
            let mut txn = store.new_txn(false).await.unwrap();
            txn.set(b"k", &vec![7u8; 4096]).await.unwrap();
            txn.commit().await.unwrap();
            let after = store.memory();
            assert!(
                after.memtable_bytes >= before.memtable_bytes + 4096,
                "{before:?} then {after:?}"
            );
            assert!(after.block_cache_capacity > 0, "{after:?}");
        })
        .unwrap();
    }
}
