//! A store written with the old DateTime index encoding is rebuilt on open.
//!
//! The encoding changed from one varint of nanoseconds to a marker plus
//! separate seconds and nanos varints. Nothing rebuilt those entries before:
//! the existing reindex path is driven by schema migration, not by a storage
//! format change. An upgraded store therefore answered range queries with
//! new-format bounds against old-format entries, so rows went missing, deletes
//! orphaned their entries, and unique indexes stopped rejecting duplicates.

use db::definition::migration::format::CURRENT_INDEX_FORMAT;
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

/// Opening a store with no format stamp rebuilds and then stamps it, so the
/// second open finds the store current instead of rebuilding again.
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

/// The JSON path `height` with 180 at it, in the layout a format-3 build wrote:
/// a property step had no bytes marker, and a descending key inverted its path.
fn format_three_json_leaf(descending: bool) -> Vec<u8> {
    let mut path = b"height".to_vec();
    path.extend_from_slice(&[0x00, 0x01, 0x01, b'/']);
    if descending {
        let inverted = path.iter().map(|byte| !byte).collect();
        storage::encoding::encode_float64_descending([vec![11], inverted].concat(), 180.0)
    } else {
        storage::encoding::encode_float64_ascending([vec![11], path].concat(), 180.0)
    }
}

/// Opening a format-3 store rebuilds its JSON indexes, ascending and
/// descending, and its empty array entries, which format 3 wrote as nulls.
#[test]
fn a_format_three_store_rebuilds_json_and_empty_array_entries() {
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
        // Each field's current value bytes and the format-3 bytes that replace them.
        let swaps = [
            ("up", encode(&leaf, false), format_three_json_leaf(false)),
            ("down", encode(&leaf, true), format_three_json_leaf(true)),
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
            .set(INDEX_FORMAT_KEY, &3u32.to_be_bytes())
            .await
            .unwrap();
        txn.commit().await.unwrap();

        // The old entries are what a format-3 store holds: these queries miss on them.
        for (request, expected) in &cases {
            let result = query(db.clone(), request).await;
            assert!(
                !result.errors.is_empty() || result.data.as_ref() != Some(expected),
                "{request} must not see format-3 entries as current"
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
                "the rebuild removes the format-3 entries"
            );
        }
        drop(datastore);
        txn.discard().unwrap();
    })
    .unwrap();
}

/// A format-3 build that stopped a full reindex after a walk page left that
/// page's documents in format-3 keys. The format-4 open must not resume past them.
#[test]
fn a_format_three_walk_page_is_not_resumed() {
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
        let old_key = [prefix.as_slice(), &format_three_json_leaf(false), suffix].concat();
        datastore.delete(key).await.unwrap();
        datastore.set(&old_key, value).await.unwrap();
        drop(datastore);

        // What the format-3 build left: a reindex owed and walked past John.
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
            .set(INDEX_FORMAT_KEY, &3u32.to_be_bytes())
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
            "the rebuild removes the format-3 entry"
        );
        txn.discard().unwrap();
    })
    .unwrap();
}
