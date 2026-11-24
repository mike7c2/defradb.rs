//! A peer's retry schedule outlives its last marker.
//!
//! The retry pass finds peers by their schedule row (`get_retry_peers`), so a
//! marker whose peer has no row is not retried until another registration
//! writes one. An ack (`CompleteMarker`) and an empty-peer clear (`ClearPeer`)
//! delete the row once they find no marker left; a registration of another
//! scope of the same peer can commit while either is running. These tests
//! commit that registration inside the first attempt of the ack or the clear,
//! after its snapshot was taken, which is the order that loses the row unless
//! the scan that found no marker is validated.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use crate::backends::RegolithStore;
use crate::keys::peerstore::ReplicatorAckedHeadsKey;

const PEER: &str = "peer";
const COLLECTION: &str = "collection";

/// Runs `inner`, and before its first attempt registers `doc` for [`PEER`] in
/// a transaction of its own that commits after the first attempt's snapshot.
struct RegisterDuringFirstAttempt {
    inner: crate::corekv::RetryAttempt,
    peerstore: Peerstore<RegolithStore>,
    doc: &'static str,
    attempts: Arc<AtomicUsize>,
}

impl crate::corekv::RetryAttemptFn for RegisterDuringFirstAttempt {
    fn run<'a>(
        &'a mut self,
        txn: &'a mut dyn crate::corekv::RetryTxn,
    ) -> crate::corekv::RetryAttemptFuture<'a> {
        Box::pin(async move {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                self.peerstore
                    .observe_push_head_with_generation(PEER, self.doc, COLLECTION)
                    .await
                    .map_err(crate::corekv::RetryAttemptError::Storage)?
                    .expect("the registration writes a marker");
            }
            self.inner.run(txn).await
        })
    }
}

/// Every marker of [`PEER`] has the schedule row the retry pass finds it by.
async fn assert_every_marker_is_scheduled(peerstore: &Peerstore<RegolithStore>, doc: &str) {
    let markers = peerstore.get_retry_documents(PEER).await.unwrap();
    assert!(
        markers.iter().any(|marker| marker.doc_id == doc),
        "{doc}'s marker was registered and must remain"
    );
    let scheduled = peerstore.get_replicator_retry_peers().await.unwrap();
    assert!(
        scheduled.iter().any(|(peer, _)| peer == PEER),
        "{PEER} has {} marker(s) but no retry schedule row, so the retry pass never visits it",
        markers.len()
    );
}

#[test]
fn an_ack_does_not_unschedule_a_registration_that_commits_during_it() {
    workers::block_on(async {
        let peerstore = Peerstore::new(Arc::new(RegolithStore::in_memory().unwrap()));
        peerstore
            .create_replicator(PEER, b"replicator")
            .await
            .unwrap();
        let generation = peerstore
            .observe_push_head_with_generation(PEER, "doc-a", COLLECTION)
            .await
            .unwrap()
            .unwrap();

        let attempts = Arc::new(AtomicUsize::new(0));
        let ack = PeerstoreRetryAttempt::CompleteMarker {
            peer_id: PEER.to_owned(),
            key: scope_marker_key(PEER, super::super::RetryScope::Document, "doc-a"),
            expected_generation: Some(generation),
            acked_key: ReplicatorAckedHeadsKey::new(PEER, "doc-a", false).bytes(),
            acked: b"head-a".to_vec(),
            schedule: super::super::RetrySchedule::default(),
        };
        let completed: bool = peerstore
            .transact_conflicts(Box::new(RegisterDuringFirstAttempt {
                inner: Box::new(ack),
                peerstore: peerstore.clone(),
                doc: "doc-b",
                attempts: attempts.clone(),
            }))
            .await
            .unwrap();

        assert!(completed, "doc-a's ack completes its own marker");
        assert!(peerstore
            .get_retry_documents(PEER)
            .await
            .unwrap()
            .iter()
            .all(|marker| marker.doc_id != "doc-a"));
        assert_every_marker_is_scheduled(&peerstore, "doc-b").await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "doc-b's marker lands in the range the ack found empty, so the ack runs again"
        );
    })
    .unwrap()
}

#[test]
fn an_ack_that_leaves_a_marker_does_not_conflict_with_a_registration() {
    workers::block_on(async {
        let peerstore = Peerstore::new(Arc::new(RegolithStore::in_memory().unwrap()));
        peerstore
            .create_replicator(PEER, b"replicator")
            .await
            .unwrap();
        let generation = peerstore
            .observe_push_head_with_generation(PEER, "doc-a", COLLECTION)
            .await
            .unwrap()
            .unwrap();
        peerstore
            .observe_push_head_with_generation(PEER, "doc-c", COLLECTION)
            .await
            .unwrap()
            .unwrap();

        let attempts = Arc::new(AtomicUsize::new(0));
        let ack = PeerstoreRetryAttempt::CompleteMarker {
            peer_id: PEER.to_owned(),
            key: scope_marker_key(PEER, super::super::RetryScope::Document, "doc-a"),
            expected_generation: Some(generation),
            acked_key: ReplicatorAckedHeadsKey::new(PEER, "doc-a", false).bytes(),
            acked: b"head-a".to_vec(),
            schedule: super::super::RetrySchedule::default(),
        };
        let completed: bool = peerstore
            .transact_conflicts(Box::new(RegisterDuringFirstAttempt {
                inner: Box::new(ack),
                peerstore: peerstore.clone(),
                doc: "doc-b",
                attempts: attempts.clone(),
            }))
            .await
            .unwrap();

        assert!(completed);
        assert_every_marker_is_scheduled(&peerstore, "doc-b").await;
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "doc-c's marker decides that the schedule stays; doc-b's registration changes nothing the ack decided"
        );
    })
    .unwrap()
}

#[test]
fn an_empty_peer_clear_does_not_unschedule_a_registration_that_commits_during_it() {
    workers::block_on(async {
        let peerstore = Peerstore::new(Arc::new(RegolithStore::in_memory().unwrap()));
        peerstore
            .create_replicator(PEER, b"replicator")
            .await
            .unwrap();
        assert!(peerstore
            .record_retry_after(PEER, std::time::Duration::ZERO)
            .await
            .unwrap());
        assert!(peerstore.get_retry_info(PEER).await.unwrap().is_some());

        let attempts = Arc::new(AtomicUsize::new(0));
        let clear = PeerstoreRetryAttempt::ClearPeer {
            peer_id: PEER.to_owned(),
            expected_incarnation: None,
        };
        let () = peerstore
            .transact_conflicts(Box::new(RegisterDuringFirstAttempt {
                inner: Box::new(clear),
                peerstore: peerstore.clone(),
                doc: "doc-b",
                attempts: attempts.clone(),
            }))
            .await
            .unwrap();

        assert_every_marker_is_scheduled(&peerstore, "doc-b").await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    })
    .unwrap()
}
