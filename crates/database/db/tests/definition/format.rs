//! Opening a store upgrades older secondary-index encodings.

use db::definition::migration::format::CURRENT_INDEX_FORMAT;
use schema::{CollectionVersion, FieldDescription, FieldKind};
use storage::RegolithStore;

/// The marker byte every encoded time key starts with, per `encoding::mod`.
const TIME_MARKER: u8 = 8;

/// The encoding this branch replaces: a single varint of nanoseconds.
fn old_format_key(unix_nanos: i64) -> Vec<u8> {
    storage::encoding::encode_varint_ascending(vec![TIME_MARKER], unix_nanos)
}

/// The two encodings genuinely differ, which is what makes a rebuild necessary.
/// Without this the rest of the test could pass for the wrong reason.
#[test]
fn the_old_and_new_encodings_differ() {
    let value = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z").unwrap();

    let new = storage::field_value::encode_field_value(
        Vec::new(),
        &document::NormalValue::Time(value),
        false,
    )
    .expect("the new encoding must succeed");
    let old = old_format_key(value.timestamp_nanos_opt().expect("in range"));

    assert_ne!(new, old, "a rebuild is only needed if the bytes differ");
}

/// Opening a new store with no format stamp stamps it, so the second open
/// finds the store current.
#[test]
fn an_unstamped_store_is_stamped_on_open() {
    workers::block_on(async {
        let store = RegolithStore::in_memory().unwrap();

        let db = db::DB::open(store.clone()).await.expect("first open");
        assert_eq!(
            db.stored_index_format().await.expect("read the stamp"),
            Some(CURRENT_INDEX_FORMAT),
            "opening must stamp the format it wrote"
        );
        drop(db);

        let db = db::DB::open(store).await.expect("second open");
        assert_eq!(
            db.stored_index_format().await.expect("read the stamp"),
            Some(CURRENT_INDEX_FORMAT),
            "the stamp must survive a reopen"
        );
    })
    .unwrap()
}

/// A fresh store carries no documents, so the rebuild costs nothing and the
/// stamp still lands.
#[test]
fn a_fresh_store_opens_clean() {
    workers::block_on(async {
        let db = db::DB::open(RegolithStore::in_memory().unwrap())
            .await
            .expect("open");
        assert_eq!(
            db.stored_index_format().await.unwrap(),
            Some(CURRENT_INDEX_FORMAT)
        );
    })
    .unwrap()
}

fn simple_schema() -> CollectionVersion {
    CollectionVersion::new(
        "Tallies",
        "tallies-v1",
        "tallies-collection",
        vec![
            FieldDescription::new(
                "1",
                "_docID",
                FieldKind::doc_id(),
                schema::CType::LwwRegister,
            ),
            FieldDescription::new("2", "count", FieldKind::int(), schema::CType::LwwRegister),
        ],
    )
}

/// Older and unstamped stores are rebuilt and stamped on open.
#[test]
fn older_index_formats_are_upgraded() {
    workers::block_on(async {
        for format in [None, Some(1u32), Some(2), Some(3), Some(4)] {
            let store = RegolithStore::in_memory().unwrap();
            let db = db::DB::open(store.clone()).await.unwrap();
            db.create_collection(simple_schema()).await.unwrap();
            let txn = db.new_txn(false).await.unwrap();
            let systemstore = txn.systemstore().unwrap();
            match format {
                Some(format) => systemstore
                    .set(
                        db::definition::migration::format::INDEX_FORMAT_KEY,
                        &format.to_be_bytes(),
                    )
                    .await
                    .unwrap(),
                None => systemstore
                    .delete(db::definition::migration::format::INDEX_FORMAT_KEY)
                    .await
                    .unwrap(),
            }
            drop(systemstore);
            txn.commit().await.unwrap();
            drop(db);
            let db = db::DB::open(store).await.unwrap();
            assert_eq!(
                db.stored_index_format().await.unwrap(),
                Some(CURRENT_INDEX_FORMAT)
            );
            assert!(db.require_collection("Tallies").is_ok());
        }
    })
    .unwrap()
}

/// A future format cannot be interpreted by this build.
#[test]
fn future_index_format_is_refused() {
    workers::block_on(async {
        let store = RegolithStore::in_memory().unwrap();
        let db = db::DB::open(store.clone()).await.unwrap();
        let txn = db.new_txn(false).await.unwrap();
        txn.systemstore().unwrap().set(db::definition::migration::format::INDEX_FORMAT_KEY, &(CURRENT_INDEX_FORMAT + 1).to_be_bytes()).await.unwrap();
        txn.commit().await.unwrap();
        drop(db);
        assert!(matches!(db::DB::open(store).await, Err(db::Error::UnsupportedIndexFormat { stored: Some(format), .. }) if format == CURRENT_INDEX_FORMAT + 1));
    }).unwrap()
}

/// A rebuild never treats a legacy key under /i/ as a new index entry.
#[test]
fn upgrade_rebuilds_entries_without_deleting_legacy_keys() {
    use db::AutoCommitMutator;
    use document::{Document, NormalValue};
    use query::DocMutator;
    use schema::IndexDescription;
    use std::sync::Arc;
    use storage::corekv::{IterOptions, Iterator as _};
    use storage::keys::IndexDataStoreKey;

    workers::block_on(async {
        let store = RegolithStore::in_memory().unwrap();
        let db = Arc::new(db::DB::open(store.clone()).await.unwrap());
        let schema =
            simple_schema().with_index(IndexDescription::new("counts").with_field("count", false));
        db.create_collection(schema.clone()).await.unwrap();
        let collection = db.require_collection("Tallies").unwrap();
        let prefix = IndexDataStoreKey::index_prefix(
            collection.resolved_root_id(),
            collection.schema().indexes[0].id,
        );
        let mut doc = Document::new(schema);
        doc.set("count", NormalValue::Int(42)).unwrap();
        AutoCommitMutator::new(db.clone())
            .create("Tallies", doc)
            .await
            .unwrap();
        let txn = db.new_txn(false).await.unwrap();
        let data = txn.datastore().unwrap();
        let mut iter = data
            .iterator(IterOptions::new().with_prefix(prefix.clone()))
            .await
            .unwrap();
        let entry = iter.next().await.unwrap().unwrap();
        iter.close().await.unwrap();
        drop(iter);
        data.delete(&entry.key).await.unwrap();
        data.set(b"/i/pk/legacy", b"preserve").await.unwrap();
        txn.systemstore()
            .unwrap()
            .set(
                db::definition::migration::format::INDEX_FORMAT_KEY,
                &3u32.to_be_bytes(),
            )
            .await
            .unwrap();
        let system = txn.systemstore().unwrap();
        system.set(b"/reindex/owed/tallies-collection", br#"{"token":17,"migration_destination":null}"#).await.unwrap();
        system.set(b"/reindex/progress/tallies-collection", br#"{"token":17,"stage":{"Walk":{"fence":null,"last":18446744073709551615}},"deferred":[]}"#).await.unwrap();
        drop(system);
        drop(data);
        txn.commit().await.unwrap();
        drop(db);
        let db = db::DB::open(store).await.unwrap();
        let txn = db.new_txn(true).await.unwrap();
        let data = txn.datastore().unwrap();
        assert_eq!(
            data.get(b"/i/pk/legacy").await.unwrap().unwrap().as_ref(),
            b"preserve"
        );
        let mut iter = data
            .iterator(IterOptions::new().with_prefix(prefix))
            .await
            .unwrap();
        assert!(iter.next().await.unwrap().is_some());
        assert!(iter.next().await.unwrap().is_none());
        iter.close().await.unwrap();
    })
    .unwrap()
}

/// A collection created through a constructor that skips the open sequence is
/// stamped in its creation transaction, so a later open does not mistake it
/// for a legacy unstamped store.
#[test]
fn a_collection_created_without_open_is_reopenable() {
    workers::block_on(async {
        let store = RegolithStore::in_memory().unwrap();
        let db = db::DB::new(store.clone()).expect("construct");
        db.create_collection(simple_schema()).await.expect("schema");
        drop(db);

        let db = db::DB::open(store).await.expect("reopen");
        assert_eq!(
            db.stored_index_format().await.expect("read the stamp"),
            Some(CURRENT_INDEX_FORMAT)
        );
        assert!(db.require_collection("Tallies").is_ok());
    })
    .unwrap()
}

/// The concrete failure a rebuild prevents: an entry written with the old
/// encoding falls outside the bounds a range query builds with the new one, so
/// the row is invisible rather than merely slow to find.
#[test]
fn an_old_format_entry_falls_outside_new_format_range_bounds() {
    let key_for = |rfc3339: &str| {
        let value = chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap();
        let new = storage::field_value::encode_field_value(
            Vec::new(),
            &document::NormalValue::Time(value),
            false,
        )
        .expect("encodes");
        let old = old_format_key(value.timestamp_nanos_opt().expect("in range"));
        (new, old)
    };

    let (lower, _) = key_for("2024-01-01T00:00:00Z");
    let (upper, _) = key_for("2024-12-31T23:59:59Z");
    let (target_new, target_old) = key_for("2024-06-15T12:00:00Z");

    assert!(
        lower <= target_new && target_new <= upper,
        "the new-format entry is inside the window, as it must be"
    );
    assert!(
        target_old < lower || upper < target_old,
        "the old-format entry for the same instant is outside the window, so a \
         scan over these bounds never returns it"
    );
}

/// The JSON path `height` with 180 at it, in the layout before format 5: a
/// property step had no bytes marker, and a descending key inverted its path.
fn old_json_leaf(descending: bool) -> Vec<u8> {
    let mut path = b"height".to_vec();
    path.extend_from_slice(&[0x00, 0x01, 0x01, b'/']);
    if descending {
        let inverted = path.iter().map(|byte| !byte).collect();
        storage::encoding::encode_float64_descending([vec![11], inverted].concat(), 180.0)
    } else {
        storage::encoding::encode_float64_ascending([vec![11], path].concat(), 180.0)
    }
}

/// Opening a format-3 or format-4 store rebuilds its JSON indexes, ascending
/// and descending, and its empty array entries, which both wrote as nulls.
#[test]
fn an_older_store_rebuilds_json_and_empty_array_entries() {
    for stamp in [3u32, 4] {
        older_store_rebuilds_json_and_empty_array_entries(stamp);
    }
}

fn older_store_rebuilds_json_and_empty_array_entries(stamp: u32) {
    use std::sync::Arc;

    use db::definition::migration::format::INDEX_FORMAT_KEY;
    use db::{AutoCommitMutator, LensedAutoCommitFetcher, DB};
    use document::{Document, JsonLeafValue, JsonPath, JsonScalarValue, NormalValue};
    use query::mutator::DocMutator;
    use query::{QueryExecutor, QueryRequest, QueryRunner};
    use storage::corekv::{IterOptions, Iterator as _};
    use storage::keys::IndexDataStoreKey;

    let query = |db: Arc<DB<RegolithStore>>, request: &'static str| async move {
        let schema = db.require_collection("User").unwrap().schema().clone();
        QueryRunner::new(LensedAutoCommitFetcher::new(db), vec![schema])
            .execute(QueryRequest::new(request))
            .await
    };
    let cases = [
        (
            "query { User(filter: {up: {height: {_gt: 170}}}) { name } }",
            serde_json::json!({"User": [{"name": "John"}]}),
        ),
        (
            "query { User(filter: {down: {height: {_gt: 170}}}) { name } }",
            serde_json::json!({"User": [{"name": "John"}]}),
        ),
        (
            "query { User(filter: {numbers: {_all: {_eq: 1}}}, order: {name: ASC}) { name } }",
            serde_json::json!({"User": [{"name": "Empty"}, {"name": "John"}]}),
        ),
    ];

    workers::block_on(async {
        let store = RegolithStore::in_memory().unwrap();
        let db = Arc::new(DB::open(store.clone()).await.unwrap());
        db.add_collection(
            &defra_core::RequestContext::ambient(),
            "type User { name: String, up: JSON @index, \
             down: JSON @index(direction: DESC), numbers: [Int!] @index }",
        )
        .await
        .unwrap();
        let collection = db.require_collection("User").unwrap();
        let schema = collection.schema_arc();
        for doc in [
            r#"{"name":"John","up":{"height":180},"down":{"height":180},"numbers":[1]}"#,
            r#"{"name":"Empty","numbers":[]}"#,
        ] {
            let document = Document::from_json_str(doc, schema.clone()).unwrap();
            AutoCommitMutator::new(db.clone())
                .create("User", document)
                .await
                .unwrap();
        }

        let leaf = NormalValue::JsonLeaf(JsonLeafValue::new(
            JsonPath::new().append_property("height"),
            JsonScalarValue::Number(180.0),
        ));
        let encode = |value: &NormalValue, descending| {
            storage::field_value::encode_field_value(Vec::new(), value, descending).unwrap()
        };
        // Each field's current value bytes and the older bytes that replace them.
        let swaps = [
            ("up", encode(&leaf, false), old_json_leaf(false)),
            ("down", encode(&leaf, true), old_json_leaf(true)),
            (
                "numbers",
                encode(&NormalValue::EmptyArray, false),
                encode(&NormalValue::Null, false),
            ),
        ];
        let txn = db.new_txn(false).await.unwrap();
        let datastore = txn.datastore().unwrap();
        let mut old_keys = Vec::new();
        for (field, new, old) in &swaps {
            let index = schema
                .indexes
                .iter()
                .find(|index| index.fields[0].name == *field)
                .unwrap();
            let prefix = IndexDataStoreKey::index_prefix(collection.resolved_root_id(), index.id);
            let mut iter = datastore
                .iterator(IterOptions::new().with_prefix(prefix.clone()))
                .await
                .unwrap();
            let mut pairs = Vec::new();
            while let Some(pair) = iter.next().await.unwrap() {
                pairs.push((pair.key.to_vec(), pair.value.to_vec()));
            }
            iter.close().await.unwrap();
            drop(iter);
            let mut swapped = 0;
            for (key, value) in pairs {
                let Some(suffix) = key[prefix.len()..].strip_prefix(new.as_slice()) else {
                    continue;
                };
                let old_key = [prefix.as_slice(), old, suffix].concat();
                datastore.delete(&key).await.unwrap();
                datastore.set(&old_key, &value).await.unwrap();
                old_keys.push(old_key);
                swapped += 1;
            }
            assert_eq!(swapped, 1, "{field} has one entry to rewrite");
        }
        drop(datastore);
        txn.systemstore()
            .unwrap()
            .set(INDEX_FORMAT_KEY, &stamp.to_be_bytes())
            .await
            .unwrap();
        txn.commit().await.unwrap();

        // The old entries are what an older store holds: these queries miss on them.
        for (request, expected) in &cases {
            let result = query(db.clone(), request).await;
            assert!(
                !result.errors.is_empty() || result.data.as_ref() != Some(expected),
                "{request} must not see older entries as current"
            );
        }
        drop(db);

        let reopened = Arc::new(DB::open(store).await.unwrap());
        assert_eq!(
            reopened.stored_index_format().await.unwrap(),
            Some(CURRENT_INDEX_FORMAT)
        );
        for (request, expected) in &cases {
            let result = query(reopened.clone(), request).await;
            assert!(result.errors.is_empty(), "{request}: {:?}", result.errors);
            assert_eq!(result.data.as_ref(), Some(expected), "{request}");
        }
        let txn = reopened.new_txn(true).await.unwrap();
        let datastore = txn.datastore().unwrap();
        for key in &old_keys {
            assert!(
                datastore.get(key).await.unwrap().is_none(),
                "the rebuild removes the older entries"
            );
        }
        drop(datastore);
        txn.discard().unwrap();
    })
    .unwrap();
}

/// An older build that stopped a full reindex after a walk page left that
/// page's documents in old keys. The current open must not resume past them.
#[test]
fn an_older_walk_page_is_not_resumed() {
    for stamp in [3u32, 4] {
        older_walk_page_is_not_resumed(stamp);
    }
}

fn older_walk_page_is_not_resumed(stamp: u32) {
    use std::sync::Arc;

    use db::definition::migration::format::INDEX_FORMAT_KEY;
    use db::{AutoCommitMutator, LensedAutoCommitFetcher, DB};
    use document::{Document, JsonLeafValue, JsonPath, JsonScalarValue, NormalValue};
    use query::mutator::DocMutator;
    use query::{QueryExecutor, QueryRequest, QueryRunner};
    use storage::corekv::{IterOptions, Iterator as _};
    use storage::keys::IndexDataStoreKey;

    let request = "query { User(filter: {up: {height: {_gt: 170}}}) { name } }";
    let expected = serde_json::json!({"User": [{"name": "John"}]});

    workers::block_on(async {
        let store = RegolithStore::in_memory().unwrap();
        let db = Arc::new(DB::open(store.clone()).await.unwrap());
        db.add_collection(
            &defra_core::RequestContext::ambient(),
            "type User { name: String, up: JSON @index }",
        )
        .await
        .unwrap();
        let collection = db.require_collection("User").unwrap();
        let schema = collection.schema_arc();
        let document =
            Document::from_json_str(r#"{"name":"John","up":{"height":180}}"#, schema.clone())
                .unwrap();
        AutoCommitMutator::new(db.clone())
            .create("User", document)
            .await
            .unwrap();

        let current = storage::field_value::encode_field_value(
            Vec::new(),
            &NormalValue::JsonLeaf(JsonLeafValue::new(
                JsonPath::new().append_property("height"),
                JsonScalarValue::Number(180.0),
            )),
            false,
        )
        .unwrap();
        let txn = db.new_txn(false).await.unwrap();
        let datastore = txn.datastore().unwrap();
        let prefix =
            IndexDataStoreKey::index_prefix(collection.resolved_root_id(), schema.indexes[0].id);
        let mut iter = datastore
            .iterator(IterOptions::new().with_prefix(prefix.clone()))
            .await
            .unwrap();
        let mut pairs = Vec::new();
        while let Some(pair) = iter.next().await.unwrap() {
            pairs.push((pair.key.to_vec(), pair.value.to_vec()));
        }
        iter.close().await.unwrap();
        drop(iter);
        assert_eq!(pairs.len(), 1, "John has one entry");
        let (key, value) = &pairs[0];
        let suffix = key[prefix.len()..]
            .strip_prefix(current.as_slice())
            .unwrap();
        let old_key = [prefix.as_slice(), &old_json_leaf(false), suffix].concat();
        datastore.delete(key).await.unwrap();
        datastore.set(&old_key, value).await.unwrap();
        drop(datastore);

        // What the older build left: a reindex owed and walked past John.
        let systemstore = txn.systemstore().unwrap();
        let collection_id = collection.collection_id();
        systemstore
            .set(
                format!("/reindex/owed/{collection_id}").as_bytes(),
                br#"{"token":7,"migration_destination":null}"#,
            )
            .await
            .unwrap();
        systemstore
            .set(
                format!("/reindex/progress/{collection_id}").as_bytes(),
                br#"{"token":7,"stage":{"Walk":{"fence":1000000,"last":1000000}},"deferred":[]}"#,
            )
            .await
            .unwrap();
        systemstore
            .set(INDEX_FORMAT_KEY, &stamp.to_be_bytes())
            .await
            .unwrap();
        drop(systemstore);
        txn.commit().await.unwrap();
        drop(db);

        let reopened = Arc::new(DB::open(store).await.unwrap());
        let schema = reopened
            .require_collection("User")
            .unwrap()
            .schema()
            .clone();
        let result = QueryRunner::new(LensedAutoCommitFetcher::new(reopened.clone()), vec![schema])
            .execute(QueryRequest::new(request))
            .await;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.data.as_ref(), Some(&expected));
        let txn = reopened.new_txn(true).await.unwrap();
        assert!(
            txn.datastore()
                .unwrap()
                .get(&old_key)
                .await
                .unwrap()
                .is_none(),
            "the rebuild removes the older entry"
        );
        txn.discard().unwrap();
    })
    .unwrap();
}
