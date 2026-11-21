//! An autocommit attempt that ends asking for an external input is not a
//! failure: the request runs again once the input is answered. It is logged
//! at debug with the input's kind, never at ERROR and never with the query.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use defra_core::RequestContext;
use document::{DocID, Document};
use kovan_queue::seg_queue::SegQueue;
use rapidhash::RapidHashSet;
use schema::{CollectionVersion, FieldDescription, FieldKind};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

use crate::error::{QueryError, Result};
use crate::executor::{QueryExecutor, QueryRequest};
use crate::mutator::{CreateResult, DeleteResult, DocMutator, UpdateResult};
use crate::prepared::{PreparedInputKind, PreparedInputMemo, PreparedInputRequest};
use crate::runner::QueryRunner;
use crate::test_utils::MockFetcher;

const SECRET: &str = "a field value the log must not carry";

fn embedding_request() -> PreparedInputRequest {
    PreparedInputRequest::new(PreparedInputKind::Embedding, b"embed".as_slice())
}

/// Creates a document only once the request's memo answers its embedding;
/// until then the attempt ends asking for it.
#[derive(Default)]
struct NeedsEmbedding {
    memo: Option<PreparedInputMemo>,
    resolved: Arc<AtomicUsize>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl DocMutator for NeedsEmbedding {
    fn crdts(&self) -> crdt::Crdts {
        crdt::Crdts::default()
    }

    fn with_prepared_inputs(&self, inputs: PreparedInputMemo) -> Option<Arc<dyn DocMutator>> {
        Some(Arc::new(Self {
            memo: Some(inputs),
            resolved: self.resolved.clone(),
        }))
    }

    async fn resolve_prepared_inputs_as(
        &self,
        _context: &RequestContext,
        requests: Vec<PreparedInputRequest>,
    ) -> Result<Vec<Arc<[u8]>>> {
        self.resolved.fetch_add(requests.len(), Ordering::SeqCst);
        Ok(requests
            .iter()
            .map(|_| Arc::<[u8]>::from(b"vector".as_slice()))
            .collect())
    }

    async fn create(&self, collection_name: &str, doc: Document) -> Result<CreateResult> {
        self.create_as(&RequestContext::ambient(), collection_name, doc)
            .await
    }

    async fn create_as(
        &self,
        _context: &RequestContext,
        _collection_name: &str,
        mut doc: Document,
    ) -> Result<CreateResult> {
        let answered = self
            .memo
            .as_ref()
            .is_some_and(|memo| memo.get(&embedding_request()).is_some());
        if !answered {
            return Err(QueryError::NeedsInput(embedding_request()));
        }
        let id = DocID::new_v0_from_seed("needs-input-log-doc");
        doc.set_id(id.clone());
        Ok(CreateResult::new(id, doc))
    }

    async fn update(
        &self,
        _collection_name: &str,
        doc: Document,
        modified_fields: RapidHashSet<String>,
    ) -> Result<UpdateResult> {
        Ok(UpdateResult::new(doc, modified_fields.len()))
    }

    async fn delete(&self, _collection_name: &str, doc_id: &DocID) -> Result<DeleteResult> {
        Ok(DeleteResult::new(doc_id.clone(), true))
    }

    async fn exists(&self, _collection_name: &str, _doc_id: &DocID) -> Result<bool> {
        Ok(false)
    }

    async fn get_for_update(
        &self,
        _collection_name: &str,
        _doc_id: &DocID,
    ) -> Result<Option<Document>> {
        Ok(None)
    }
}

/// One emitted event: its level and every field, the message included.
#[derive(Debug)]
struct Line {
    level: Level,
    fields: Vec<(&'static str, String)>,
}

impl Line {
    fn field(&self, name: &str) -> Option<&str> {
        let (_, value) = self.fields.iter().find(|(field, _)| *field == name)?;
        Some(value)
    }
}

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.push((field.name(), format!("{value:?}")));
    }
}

/// Keeps every event emitted on the thread it is the default for.
#[derive(Clone, Default)]
struct Capture(Arc<SegQueue<Line>>);

impl Capture {
    fn lines(&self) -> Vec<Line> {
        std::iter::from_fn(|| self.0.pop()).collect()
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut line = Line {
            level: *event.metadata().level(),
            fields: Vec::new(),
        };
        event.record(&mut line);
        self.0.push(line);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

fn notes() -> CollectionVersion {
    CollectionVersion::new(
        "Note",
        "v1",
        "coll-note",
        vec![
            FieldDescription::new(
                "1",
                "_docID",
                FieldKind::doc_id(),
                schema::CType::LwwRegister,
            ),
            FieldDescription::new("2", "body", FieldKind::string(), schema::CType::LwwRegister),
        ],
    )
}

#[test]
fn an_attempt_that_waits_for_an_input_is_logged_at_debug_without_the_query() {
    let resolved = Arc::new(AtomicUsize::new(0));
    let runner = QueryRunner::new(MockFetcher::new(), vec![notes()]).with_mutator(Arc::new(
        NeedsEmbedding {
            memo: None,
            resolved: resolved.clone(),
        },
    ));
    let request = QueryRequest::new(format!(
        r#"mutation {{ add_Note(input: {{ body: "{SECRET}" }}) {{ body }} }}"#
    ));

    let capture = Capture::default();
    let response = tracing::subscriber::with_default(capture.clone(), || {
        workers::block_on(runner.execute(request)).unwrap()
    });

    assert!(!response.has_errors(), "{:?}", response.errors);
    assert_eq!(
        response.data,
        Some(serde_json::json!({ "add_Note": [{ "body": SECRET }] }))
    );
    assert_eq!(resolved.load(Ordering::SeqCst), 1);

    let lines = capture.lines();
    let waits: Vec<_> = lines
        .iter()
        .filter(|line| line.field("message") == Some("query needs an external input"))
        .collect();
    assert_eq!(waits.len(), 1, "{lines:?}");
    assert_eq!(waits[0].level, Level::DEBUG);
    assert_eq!(waits[0].field("kind"), Some("Embedding"));
    assert!(
        lines.iter().all(|line| line.level != Level::ERROR),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| line.fields.iter().all(|(_, value)| !value.contains(SECRET))),
        "{lines:?}"
    );
}
