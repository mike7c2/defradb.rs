#![cfg(feature = "p2p")]

//! What a filtered replicator sends on: three nodes in a line.
//!
//! The writer replicates `Book` to the relay with no filter; the relay
//! replicates it to the reader with a filter on `to`. A document the relay
//! writes itself and that matches reaches the reader. A document the relay
//! received from the writer, that matches too, does not: the relay forwards a
//! merged head without the document (`forward_merged` in
//! `p2p/src/sync/replication/handlers.rs`), and a filtered replicator is
//! skipped for a push that carries no document to test (`push_jobs` in
//! `p2p/src/sync/coordinator/broadcast.rs`). A filtered replicator added once
//! the relay holds the document back-fills it, because the back-fill reads
//! the document from the relay's store and tests it there.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "support/clock.rs"]
mod clock;
#[path = "support/outcome.rs"]
mod outcome;

use clock::sleep;
use defra_node::{Node, NodeBuilder, P2PConfig, ReplicationFilter, ReplicationFilters};
use outcome::Result;

const SDL: &str = "type Book { to: String @index @immutable \n seq: Int @immutable }";

/// How long a document is given to arrive.
const ARRIVAL: Duration = Duration::from_secs(20);

/// How long a document that should not arrive is watched for, after a later
/// document on the same replicator arrived.
const QUIET: Duration = Duration::from_secs(3);

fn config() -> P2PConfig {
    P2PConfig {
        bind_addr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        port: Some(0),
        relay_mode: p2p::iroh::IrohRelayModeConfig::Disabled,
        discovery: p2p::iroh::IrohDiscoveryConfig::Disabled,
        peer_key: defra_node::PeerKeySource::Store,
        ..Default::default()
    }
}

struct Peer {
    node: Arc<Node>,
    addr: String,
}

impl Peer {
    async fn start() -> Result<Self> {
        let node = NodeBuilder::default().with_p2p(config()).build().await?;
        node.add_collection(SDL).await?;
        let ops = node.p2p_system().expect("p2p").ops().clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        let addr = loop {
            let addrs = ops.listen_addresses().await?;
            if let Some(addr) = addrs.into_iter().find(|addr| addr.contains("/p2p/")) {
                break addr;
            }
            assert!(Instant::now() < deadline, "no listen address");
            sleep(Duration::from_millis(50)).await;
        };
        Ok(Self {
            node: Arc::new(node),
            addr,
        })
    }

    /// Connects to `to` and replicates `Book` to it, filtered on `to` when
    /// `filter_to` is given.
    async fn replicate(&self, to: &Peer, filter_to: Option<&str>) -> Result<()> {
        let ops = self.node.p2p_system().expect("p2p").ops().clone();
        ops.connect_peer(&to.addr).await?;
        let mut filters = ReplicationFilters::new();
        if let Some(value) = filter_to {
            filters.insert(
                "Book".into(),
                ReplicationFilter::eq("to", serde_json::json!(value)),
            );
        }
        ops.add_replicator(
            vec!["Book".into()],
            Some(&to.addr),
            filters,
            Vec::new(),
            None,
        )
        .await?;
        Ok(())
    }

    async fn write(&self, to: &str, seq: i64) {
        let response = self
            .node
            .execute(&format!(
                "mutation {{ add_Book(input: {{to: \"{to}\", seq: {seq}}}) {{ _docID }} }}"
            ))
            .await;
        assert!(!response.has_errors(), "add_Book: {:?}", response.errors);
    }

    async fn seqs(&self) -> Vec<i64> {
        let response = self.node.execute("query { Book { seq } }").await;
        assert!(!response.has_errors(), "Book query: {:?}", response.errors);
        let mut seqs: Vec<i64> = response
            .data
            .and_then(|data| data["Book"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|book| book["seq"].as_i64())
            .collect();
        seqs.sort_unstable();
        seqs
    }

    /// Whether `seq` arrives within `within`.
    async fn holds_within(&self, seq: i64, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if self.seqs().await.contains(&seq) {
                return true;
            }
            sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

#[test]
fn a_filtered_replicator_sends_on_only_what_its_node_wrote() -> Result<()> {
    edge::testing::edge()
        .block_on(async {
            let (writer, relay, reader) = (
                Peer::start().await?,
                Peer::start().await?,
                Peer::start().await?,
            );
            writer.replicate(&relay, None).await?;
            relay.replicate(&reader, Some("reader")).await?;

            writer.write("reader", 1).await;
            writer.write("other", 2).await;
            assert!(
                relay.holds_within(1, ARRIVAL).await,
                "the relay receives the writer's documents"
            );
            assert!(relay.holds_within(2, ARRIVAL).await);

            relay.write("reader", 3).await;
            assert!(
                reader.holds_within(3, ARRIVAL).await,
                "the relay's own matching document passes its filtered replicator"
            );
            assert!(
                !reader.holds_within(1, QUIET).await,
                "a matching document the relay received is not sent on: reader holds {:?}",
                reader.seqs().await
            );
            assert_eq!(reader.seqs().await, vec![3]);

            let late = Peer::start().await?;
            relay.replicate(&late, Some("reader")).await?;
            assert!(
                late.holds_within(1, ARRIVAL).await,
                "a filtered replicator added later back-fills the received document from the store"
            );
            assert!(late.holds_within(3, ARRIVAL).await);
            sleep(QUIET).await;
            assert_eq!(
                late.seqs().await,
                vec![1, 3],
                "the back-fill applies the filter"
            );

            for peer in [&writer, &relay, &reader, &late] {
                peer.node.shutdown().await.unwrap();
            }
            Ok(())
        })
        .unwrap()
}
