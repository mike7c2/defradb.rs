//! Claiming collections by a rule: a collection defined after the node
//! started is governed from the moment it exists.

use super::*;

/// Define a collection with a `grant` field and, when `root` is given, a
/// governance root, on a node that is already running.
async fn define(node: &Node, name: &str, id: &str, root: Option<&str>) {
    let mut collection = CollectionVersion::new(
        name,
        id,
        id,
        vec![
            FieldDescription::new(
                "1",
                "_docID",
                FieldKind::doc_id(),
                schema::CType::LwwRegister,
            ),
            FieldDescription::new(
                "2",
                "grant",
                FieldKind::string(),
                schema::CType::LwwRegister,
            ),
        ],
    );
    if let Some(root) = root {
        collection = collection.with_governance_root(root);
    }
    node.db.create_collection(collection).await.unwrap();
}

struct Verdict(MergeVerdict);

#[async_trait]
impl MergeValidator for Verdict {
    async fn validate(
        &self,
        _candidate: &MergeCandidate<'_>,
        _view: &dyn MergeView,
    ) -> Result<MergeVerdict, String> {
        Ok(self.0.clone())
    }
}

fn rooted(collection: &CollectionVersion) -> bool {
    collection.governance_root.is_some()
}

async fn node_claiming_rooted(verdict: MergeVerdict) -> Node {
    Node::open(
        RegolithStore::in_memory().unwrap(),
        MergeGovernance::claiming(rooted).with_validator(Arc::new(Verdict(verdict))),
        false,
    )
    .await
}

#[test]
fn a_rooted_collection_added_after_start_is_rejected_by_the_validator() {
    workers::block_on(async {
        let node = node_claiming_rooted(MergeVerdict::reject("no")).await;
        define(&node, "Book_a", "col-book-a", Some("root-a")).await;
        let writer = signer();

        let note = genesis("col-book-a", "grant", "x", &writer);
        assert_eq!(
            note.merge(&node, &writer.did).await,
            MergeOutcome::rejected("no")
        );
        assert!(node.doc_ids("Book_a").await.is_empty());
    })
    .unwrap()
}

#[test]
fn a_rooted_collection_added_after_start_merges_when_the_validator_accepts() {
    workers::block_on(async {
        let node = node_claiming_rooted(MergeVerdict::Accept).await;
        define(&node, "Book_a", "col-book-a", Some("root-a")).await;
        define(&node, "Book_b", "col-book-b", Some("root-b")).await;
        let writer = signer();

        for (id, name) in [("col-book-a", "Book_a"), ("col-book-b", "Book_b")] {
            let note = genesis(id, "grant", "x", &writer);
            assert_eq!(note.merge(&node, &writer.did).await, MergeOutcome::Merged);
            assert_eq!(node.doc_ids(name).await, vec![note.doc_id.clone()]);
        }
    })
    .unwrap()
}

#[test]
fn a_collection_the_rule_does_not_accept_stays_ungoverned() {
    workers::block_on(async {
        let node = node_claiming_rooted(MergeVerdict::reject("no")).await;
        define(&node, "Plain", "col-plain", None).await;
        let writer = signer();

        let note = genesis("col-plain", "grant", "x", &writer);
        assert_eq!(note.merge(&node, &writer.did).await, MergeOutcome::Merged);
        assert_eq!(node.doc_ids("Plain").await, vec![note.doc_id.clone()]);
    })
    .unwrap()
}

#[test]
fn a_rule_claims_beside_names_and_names_still_claim() {
    workers::block_on(async {
        let node = Node::open(
            RegolithStore::in_memory().unwrap(),
            MergeGovernance::new(["Named"])
                .with_rule(rooted)
                .with_validator(Arc::new(Verdict(MergeVerdict::reject("no")))),
            false,
        )
        .await;
        define(&node, "Named", "col-named", None).await;
        define(&node, "Book_a", "col-book-a", Some("root-a")).await;
        define(&node, "Plain", "col-plain", None).await;
        let writer = signer();

        for id in ["col-named", "col-book-a"] {
            let note = genesis(id, "grant", "x", &writer);
            assert_eq!(
                note.merge(&node, &writer.did).await,
                MergeOutcome::rejected("no")
            );
        }
        let plain = genesis("col-plain", "grant", "x", &writer);
        assert_eq!(plain.merge(&node, &writer.did).await, MergeOutcome::Merged);
    })
    .unwrap()
}

#[test]
fn a_rule_claim_without_a_validator_defers() {
    workers::block_on(async {
        let node = Node::open(
            RegolithStore::in_memory().unwrap(),
            MergeGovernance::claiming(rooted),
            false,
        )
        .await;
        define(&node, "Book_a", "col-book-a", Some("root-a")).await;
        let writer = signer();

        let note = genesis("col-book-a", "grant", "x", &writer);
        assert!(matches!(
            note.merge(&node, &writer.did).await,
            MergeOutcome::Skipped {
                terminal: false,
                ..
            }
        ));
        assert!(node.doc_ids("Book_a").await.is_empty());
    })
    .unwrap()
}
