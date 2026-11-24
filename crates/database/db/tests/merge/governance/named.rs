//! `composite_fields_named`: the fields `composite_fields` reports, narrowed
//! to the names asked for.

use super::*;
use defra_core::block::{ImmutableDeltaPayload, IMMUTABLE_LINK_NAME};

/// Reads every composite both ways and keeps what each read reported.
#[derive(Default)]
struct BothReads {
    full: Atom<Option<Vec<(String, FieldValue)>>>,
    named: Atom<Option<Vec<(String, FieldValue)>>>,
}

#[async_trait]
impl MergeValidator for BothReads {
    async fn validate(
        &self,
        candidate: &MergeCandidate<'_>,
        view: &dyn MergeView,
    ) -> Result<MergeVerdict, String> {
        self.full.store(view.composite_fields(candidate.cid).await?);
        self.named.store(
            view.composite_fields_named(candidate.cid, &["grant", "missing"])
                .await?,
        );
        Ok(MergeVerdict::reject("read only"))
    }
}

fn string(value: &str) -> FieldValue {
    FieldValue::Value(NormalValue::String(value.to_string()))
}

fn cbor(value: &str) -> Vec<u8> {
    db::block::builder::encode_value_as_cbor(&NormalValue::String(value.to_string())).unwrap()
}

/// A genesis linking an immutable block that carries `grant` and `blob`,
/// and a field block for `body`.
fn with_immutable_block(by: &Signer) -> Genesis {
    let mut blocks = Vec::new();
    let immutable = Block::new(
        CrdtDelta::Immutable(ImmutableDeltaPayload {
            priority: 1,
            collection_version_id: "col-notes".to_string(),
            fields: [("blob", "a large value"), ("grant", "granted")]
                .into_iter()
                .map(|(name, value)| (name.to_string(), serde_bytes::ByteBuf::from(cbor(value))))
                .collect(),
        }),
        vec![],
        vec![],
    );
    let immutable_cid = immutable.generate_cid().unwrap();
    blocks.push((immutable_cid, immutable.to_dag_cbor().unwrap()));
    let body = Block::new(
        CrdtDelta::Lww(LwwDeltaPayload {
            field_name: "body".to_string(),
            priority: 1,
            collection_version_id: "col-notes".to_string(),
            data: cbor("text"),
        }),
        vec![],
        vec![],
    );
    let body_cid = body.generate_cid().unwrap();
    blocks.push((body_cid, body.to_dag_cbor().unwrap()));
    let mut composite = Block::new(
        CrdtDelta::Composite(CompositeDeltaPayload {
            collection_version_id: "col-notes".to_string(),
            priority: 1,
            status: 1,
        }),
        vec![],
        vec![
            DAGLink::new(IMMUTABLE_LINK_NAME, immutable_cid),
            DAGLink::new("body", body_cid),
        ],
    );
    let value = by.key.sign(&composite.to_dag_cbor().unwrap()).unwrap();
    let signature = Signature::new(
        SignatureHeader::new(
            SignatureType::EdDSA,
            hex::encode(by.key.public_key().raw()).into_bytes(),
        ),
        value,
    );
    let signature_cid = signature.generate_cid().unwrap();
    blocks.push((signature_cid, signature.to_dag_cbor().unwrap()));
    composite.signature = Some(signature_cid);
    let cid = composite.generate_cid().unwrap();
    blocks.push((cid, composite.to_dag_cbor().unwrap()));
    Genesis {
        cid,
        doc_id: db::block::builder::derive_doc_id(&cid),
        blocks,
        collection_id: "col-notes",
    }
}

/// Narrowed to `grant`, the read reports what the full read reports for
/// `grant` and nothing else, whether the field comes in a field block of
/// its own or in an immutable block beside a larger one.
#[test]
fn a_named_read_is_the_full_read_narrowed_to_the_names() {
    workers::block_on(async {
        let reads = Arc::new(BothReads::default());
        let node = Node::open(
            RegolithStore::in_memory().unwrap(),
            MergeGovernance::new(["Notes"]).with_validator(reads.clone()),
            true,
        )
        .await;
        let writer = signer();
        let narrowed = |reads: &BothReads| {
            reads.full.load_clone().map(|fields| {
                fields
                    .into_iter()
                    .filter(|(name, _)| name == "grant")
                    .collect::<Vec<_>>()
            })
        };

        let note = authored_fields(
            "col-notes",
            None,
            &[("body", "text"), ("grant", "granted")],
            &writer,
        );
        note.merge(&node, &writer.did).await;
        assert_eq!(reads.full.load_clone().unwrap().len(), 2);
        assert_eq!(
            reads.named.load_clone(),
            Some(vec![("grant".to_string(), string("granted"))])
        );
        assert_eq!(reads.named.load_clone(), narrowed(&reads));

        let chunk = with_immutable_block(&writer);
        chunk.merge(&node, &writer.did).await;
        assert_eq!(reads.full.load_clone().unwrap().len(), 3);
        assert_eq!(
            reads.named.load_clone(),
            Some(vec![("grant".to_string(), string("granted"))])
        );
        assert_eq!(reads.named.load_clone(), narrowed(&reads));
    })
    .unwrap()
}
