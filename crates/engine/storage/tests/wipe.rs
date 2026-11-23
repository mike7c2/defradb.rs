//! `storage::wipe` empties the data directory, removes the owned
//! directories and never reaches a kept path.

use storage::corekv::{Reader, Store, Writer};
use storage::{RegolithStore, RegolithStoreOptions, WipeTarget};

#[test]
fn wipe_empties_the_store_and_removes_only_owned_dirs() {
    workers::block_on(async {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let keys = root.path().join("keys");
        let vera = root.path().join("vera");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::create_dir_all(&vera).unwrap();
        std::fs::write(vera.join("chain"), b"old").unwrap();

        let store =
            RegolithStore::open_with_options(&data, RegolithStoreOptions::default()).unwrap();
        let mut txn = store.new_txn(false).await.unwrap();
        txn.set(b"key", b"value").await.unwrap();
        txn.commit().await.unwrap();
        store.close().await.unwrap();
        drop(store);

        storage::wipe(&WipeTarget {
            store: Some((data.clone(), RegolithStoreOptions::default())),
            owned_dirs: vec![vera.clone()],
            kept: vec![keys.clone()],
        })
        .unwrap();

        assert!(data.is_dir(), "the data directory is emptied, not removed");
        assert!(!vera.exists(), "an owned directory is removed");
        assert!(keys.is_dir(), "a kept path survives");
        let store =
            RegolithStore::open_with_options(&data, RegolithStoreOptions::default()).unwrap();
        let txn = store.new_txn(true).await.unwrap();
        assert_eq!(txn.get(b"key").await.unwrap(), None);
    })
    .expect("run on a worker");
}

#[test]
fn wipe_refuses_an_owned_dir_holding_a_kept_path() {
    let root = tempfile::tempdir().unwrap();
    let keys = root.path().join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    let error = storage::wipe(&WipeTarget {
        store: None,
        owned_dirs: vec![root.path().to_path_buf()],
        kept: vec![keys.clone()],
    })
    .unwrap_err();
    assert!(error.to_string().contains("refusing to wipe"), "{error}");
    assert!(keys.is_dir());
}
