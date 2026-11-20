//! An embedder's topics cross the node's own connections: a subscriber is
//! told the publisher, not the relay; nothing reaches a node that did not
//! subscribe, or Defra's own handling; and a payload over the limit, a second
//! subscription and a revoked publisher are refused.

use std::time::Duration;

use iroh::SecretKey;

use super::allowlist_tests::config;
use super::endpoint_config::AdmissionAuthority;
use super::{
    spawn_endpoint, AppMessage, AppSubscription, IrohAllowlistConfig, IrohTransport,
    APP_TOPIC_PREFIX, MAX_APP_PAYLOAD,
};
use crate::error::Error;
use crate::rt::time::{sleep, timeout, Instant};
use crate::sync::DispatchReceiver;
use crate::transport::{P2PTransport, TransportEvent};

type Events = DispatchReceiver<TransportEvent<iroh::endpoint::SendStream>>;

const TOPIC: &str = "waterfall/1";
const IO_TIMEOUT: Duration = Duration::from_secs(20);
const RETRY: Duration = Duration::from_millis(100);

struct TestNode {
    transport: IrohTransport,
    events: Events,
    task: edge::Task<()>,
}

impl TestNode {
    async fn spawn(allowlist: IrohAllowlistConfig) -> Self {
        let secret_key = SecretKey::generate();
        let (command_tx, events, _replicators, task) =
            spawn_endpoint(config(secret_key.clone(), allowlist), edge::testing::edge())
                .await
                .unwrap();
        Self {
            transport: IrohTransport::new(command_tx, secret_key),
            events,
            task,
        }
    }

    /// A node only `peer` may connect to.
    async fn admitting_only(peer: &TestNode) -> Self {
        let node = Self::spawn(IrohAllowlistConfig::Explicit(Default::default())).await;
        node.transport
            .allow_peer(peer.transport.local_peer_id(), AdmissionAuthority::full())
            .await
            .unwrap();
        node
    }

    async fn connect(&self, other: &TestNode) {
        let addrs = other.transport.listen_addresses().await.unwrap();
        self.transport
            .dial(other.transport.local_peer_id(), addrs)
            .await
            .unwrap();
        for (from, to) in [(self, other), (other, self)] {
            from.transport
                .poll_until_connected(to.transport.local_peer_id(), IO_TIMEOUT)
                .await
                .unwrap();
        }
    }

    async fn subscribe(&self, topic: &str) -> AppSubscription {
        self.transport.subscribe_app_topic(topic).await.unwrap()
    }

    /// The gossip events Defra's own handling was given so far.
    fn defra_gossip(&mut self) -> Vec<String> {
        let mut seen = Vec::new();
        while let Some(event) = self.events.try_recv() {
            match event {
                TransportEvent::GossipMessage { topic, .. }
                | TransportEvent::GossipRawMessage { topic, .. }
                | TransportEvent::PeerSubscribed { topic, .. }
                | TransportEvent::PeerUnsubscribed { topic, .. }
                    if topic.starts_with(APP_TOPIC_PREFIX) =>
                {
                    seen.push(topic);
                }
                _ => {}
            }
        }
        seen
    }

    async fn shutdown(self) {
        self.transport.shutdown().await.unwrap();
        self.task.await.unwrap();
    }
}

/// Publishes a numbered payload on `topic` until `subscription` receives one,
/// which a mesh still forming may take a few tries for. Returns what arrived.
async fn publish_until_received(
    publisher: &TestNode,
    topic: &str,
    label: &str,
    subscription: &mut AppSubscription,
) -> AppMessage {
    let deadline = Instant::now() + IO_TIMEOUT;
    for attempt in 0u32.. {
        assert!(Instant::now() < deadline, "{label} never arrived");
        let payload = format!("{label}/{attempt}");
        publisher
            .transport
            .publish_app_topic(topic, payload.as_bytes())
            .await
            .unwrap();
        if let Ok(Some(message)) = timeout(RETRY, subscription.recv()).await {
            return message;
        }
    }
    unreachable!()
}

async fn next(subscription: &mut AppSubscription) -> AppMessage {
    timeout(IO_TIMEOUT, subscription.recv())
        .await
        .expect("a message arrives")
        .expect("the node is running")
}

/// A reaches C only through B: A and C each admit no connection but B's.
async fn line() -> (TestNode, TestNode, TestNode) {
    let b = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    let a = TestNode::admitting_only(&b).await;
    let c = TestNode::admitting_only(&b).await;
    a.connect(&b).await;
    b.connect(&c).await;
    (a, b, c)
}

#[nuclei::test(threads = 0)]
async fn a_message_reaches_every_subscriber_named_by_its_publisher() {
    let (mut a, mut b, mut c) = line().await;
    let _sub_a = a.subscribe(TOPIC).await;
    let mut sub_b = b.subscribe(TOPIC).await;
    let mut sub_c = c.subscribe(TOPIC).await;

    let relayed = publish_until_received(&a, TOPIC, "row", &mut sub_c).await;
    assert_eq!(relayed.sender, *a.transport.local_peer_id());
    assert!(relayed.payload.starts_with(b"row/"));
    let direct = loop {
        let message = next(&mut sub_b).await;
        if message.payload == relayed.payload {
            break message;
        }
    };
    assert_eq!(direct.sender, *a.transport.local_peer_id());

    let connected = c.transport.connected_peers().await.unwrap();
    assert_eq!(connected, vec![b.transport.local_peer_id().clone()]);
    for node in [&mut a, &mut b, &mut c] {
        assert_eq!(node.defra_gossip(), Vec::<String>::new());
    }

    for node in [a, b, c] {
        node.shutdown().await;
    }
}

#[nuclei::test(threads = 0)]
async fn a_node_that_did_not_subscribe_receives_nothing() {
    let publisher = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    let subscriber = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    let mut bystander = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    publisher.connect(&subscriber).await;
    publisher.connect(&bystander).await;

    let _own = publisher.subscribe(TOPIC).await;
    let _own_other = publisher.subscribe("other").await;
    let mut subscribed = subscriber.subscribe(TOPIC).await;
    let mut other = bystander.subscribe("other").await;

    publish_until_received(&publisher, TOPIC, "row", &mut subscribed).await;
    let first = publish_until_received(&publisher, "other", "sentinel", &mut other).await;
    assert!(first.payload.starts_with(b"sentinel/"));
    assert_eq!(bystander.defra_gossip(), Vec::<String>::new());

    for node in [publisher, subscriber, bystander] {
        node.shutdown().await;
    }
}

/// Defra's raw topics are how the KMS listens. A node listening that way on
/// an application topic's gossip name is given nothing from it.
#[nuclei::test(threads = 0)]
async fn defras_own_handling_reads_nothing_from_an_application_topic() {
    let publisher = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    let mut listener = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    publisher.connect(&listener).await;

    let gossip_name = format!("{APP_TOPIC_PREFIX}{TOPIC}");
    let raw_topic = "encryption/test/_response".to_string();
    let _own = publisher.subscribe(TOPIC).await;
    listener
        .transport
        .subscribe_raw(gossip_name.clone())
        .await
        .unwrap();
    publisher
        .transport
        .subscribe_raw(raw_topic.clone())
        .await
        .unwrap();
    listener
        .transport
        .subscribe_raw(raw_topic.clone())
        .await
        .unwrap();

    let deadline = Instant::now() + IO_TIMEOUT;
    let mut raw_seen = false;
    for attempt in 0u32.. {
        assert!(Instant::now() < deadline, "the raw sentinel never arrived");
        let row = format!("row/{attempt}");
        publisher
            .transport
            .publish_app_topic(TOPIC, row.as_bytes())
            .await
            .unwrap();
        publisher
            .transport
            .publish_raw(
                raw_topic.clone(),
                format!("sentinel/{attempt}").into_bytes(),
            )
            .await
            .unwrap();
        sleep(RETRY).await;
        while let Some(event) = listener.events.try_recv() {
            match event {
                TransportEvent::GossipRawMessage { topic, .. } => {
                    assert_eq!(topic, raw_topic);
                    raw_seen = true;
                }
                TransportEvent::GossipMessage { topic, .. } => {
                    panic!("Defra decoded a message on {topic}");
                }
                TransportEvent::PeerSubscribed { topic, .. } => assert_ne!(topic, gossip_name),
                _ => {}
            }
        }
        if raw_seen && attempt >= 3 {
            break;
        }
    }

    for node in [publisher, listener] {
        node.shutdown().await;
    }
}

#[nuclei::test(threads = 0)]
async fn a_payload_over_the_limit_is_refused_and_one_at_the_limit_arrives() {
    let publisher = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    let subscriber = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    publisher.connect(&subscriber).await;
    let _own = publisher.subscribe(TOPIC).await;
    let mut subscription = subscriber.subscribe(TOPIC).await;

    let over = vec![7u8; MAX_APP_PAYLOAD + 1];
    let error = publisher
        .transport
        .publish_app_topic(TOPIC, &over)
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::GossipTooLarge { size, limit } if size == over.len() && limit == MAX_APP_PAYLOAD),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "gossip broadcast of {} bytes is over the {MAX_APP_PAYLOAD} byte limit",
            over.len()
        )
    );

    publish_until_received(&publisher, TOPIC, "ready", &mut subscription).await;
    let largest = vec![7u8; MAX_APP_PAYLOAD];
    publisher
        .transport
        .publish_app_topic(TOPIC, &largest)
        .await
        .unwrap();
    loop {
        let message = next(&mut subscription).await;
        if message.payload.len() == MAX_APP_PAYLOAD {
            assert_eq!(message.payload.as_ref(), largest.as_slice());
            break;
        }
    }

    for node in [publisher, subscriber] {
        node.shutdown().await;
    }
}

#[nuclei::test(threads = 0)]
async fn a_topic_has_one_subscription_until_it_is_dropped() {
    let node = TestNode::spawn(IrohAllowlistConfig::AcceptAll).await;
    let first = node.subscribe(TOPIC).await;
    let second = node.transport.subscribe_app_topic(TOPIC).await;
    assert!(
        matches!(second, Err(Error::AppTopicSubscribed(ref name)) if name == "/defra-app/waterfall/1")
    );
    assert!(matches!(
        node.transport.subscribe_app_topic("").await,
        Err(Error::InvalidAppTopic(_))
    ));

    drop(first);
    let again = node.subscribe(TOPIC).await;
    assert_eq!(again.topic(), TOPIC);

    drop(again);
    node.shutdown().await;
}

#[nuclei::test(threads = 0)]
async fn a_revoked_publishers_relayed_messages_are_dropped() {
    let (a, b, c) = line().await;
    let mut sub_a = a.subscribe(TOPIC).await;
    let mut sub_b = b.subscribe(TOPIC).await;
    let mut sub_c = c.subscribe(TOPIC).await;
    publish_until_received(&b, TOPIC, "ready", &mut sub_c).await;
    publish_until_received(&b, TOPIC, "ready", &mut sub_a).await;

    c.transport
        .deny_peer(a.transport.local_peer_id())
        .await
        .unwrap();
    publish_until_received(&a, TOPIC, "revoked", &mut sub_b).await;
    b.transport
        .publish_app_topic(TOPIC, b"sentinel")
        .await
        .unwrap();
    loop {
        let message = next(&mut sub_c).await;
        assert_eq!(message.sender, *b.transport.local_peer_id());
        if message.payload.as_ref() == b"sentinel" {
            break;
        }
    }

    for node in [a, b, c] {
        node.shutdown().await;
    }
}

/// What an embedder streaming rows can expect: `PAYLOAD`-byte messages from
/// A to B (a neighbour) and C (reached through B), at a steady `RATE` a
/// second or in one burst. Prints what arrived; asserts nothing about loss.
///
/// `cargo test -p p2p --features iroh-transport app_topic_tests::measure -- --ignored --nocapture`
mod measure {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    const PAYLOAD: usize = 3_000;
    const RATE: u32 = 10;
    const STEADY: u32 = 30 * RATE;
    const BURST: u32 = 1_000;

    #[derive(Default)]
    struct Arrivals {
        /// `(sequence, delay)` in arrival order.
        rows: Vec<(u32, Duration)>,
    }

    impl Arrivals {
        fn report(&self, who: &str, messages: u32, dropped: u64) {
            let mut seen = vec![false; messages as usize];
            let (mut duplicates, mut out_of_order, mut highest) = (0, 0, None);
            for (sequence, _) in &self.rows {
                if std::mem::replace(&mut seen[*sequence as usize], true) {
                    duplicates += 1;
                    continue;
                }
                if highest.is_some_and(|highest| *sequence < highest) {
                    out_of_order += 1;
                }
                highest = highest.max(Some(*sequence));
            }
            let arrived = seen.iter().filter(|seen| **seen).count();
            let missing: Vec<_> = (0..messages).filter(|n| !seen[*n as usize]).collect();
            let mut delays: Vec<_> = self.rows.iter().map(|(_, delay)| *delay).collect();
            delays.sort();
            let at = |quantile: f64| {
                delays
                    .get(((delays.len() as f64 - 1.0) * quantile).round() as usize)
                    .copied()
                    .unwrap_or_default()
            };
            println!(
                "{who}: {arrived}/{messages} arrived, {duplicates} duplicates, {out_of_order} out \
                 of order, {dropped} dropped at the subscription buffer; delay min {:?} p50 {:?} \
                 p99 {:?} max {:?}; missing {missing:?}",
                at(0.0),
                at(0.5),
                at(0.99),
                at(1.0),
            );
        }
    }

    fn row(sequence: u32, sent: Duration) -> Vec<u8> {
        let mut row = vec![0u8; PAYLOAD];
        row[..4].copy_from_slice(&sequence.to_be_bytes());
        row[4..20].copy_from_slice(&sent.as_nanos().to_be_bytes());
        row
    }

    async fn collect(
        subscription: &mut AppSubscription,
        start: Instant,
        done: &AtomicBool,
    ) -> Arrivals {
        let mut arrivals = Arrivals::default();
        loop {
            match timeout(Duration::from_secs(2), subscription.recv()).await {
                Ok(Some(message)) if message.payload.len() == PAYLOAD => {
                    let sequence = u32::from_be_bytes(message.payload[..4].try_into().unwrap());
                    let sent = u128::from_be_bytes(message.payload[4..20].try_into().unwrap());
                    let delay = start.elapsed().as_nanos().saturating_sub(sent);
                    arrivals
                        .rows
                        .push((sequence, Duration::from_nanos(delay as u64)));
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) if done.load(Ordering::Relaxed) => break,
                Err(_) => {}
            }
        }
        arrivals
    }

    /// `steady` paces `messages` at `RATE`; otherwise they go out back to back.
    async fn run(publisher_subscribes: bool, messages: u32, steady: bool) {
        let (a, b, c) = line().await;
        let mut sub_b = b.subscribe(TOPIC).await;
        let mut sub_c = c.subscribe(TOPIC).await;
        let _sub_a = if publisher_subscribes {
            Some(a.subscribe(TOPIC).await)
        } else {
            None
        };
        publish_until_received(&a, TOPIC, "ready", &mut sub_c).await;
        sleep(Duration::from_secs(1)).await;

        let start = Instant::now();
        let done = AtomicBool::new(false);
        let publish = async {
            let mut refused = 0;
            for sequence in 0..messages {
                if steady {
                    let due = Duration::from_secs_f64(f64::from(sequence) / f64::from(RATE));
                    sleep(due.saturating_sub(start.elapsed())).await;
                }
                let row = row(sequence, start.elapsed());
                if a.transport.publish_app_topic(TOPIC, &row).await.is_err() {
                    refused += 1;
                }
            }
            done.store(true, Ordering::Relaxed);
            refused
        };
        let (refused, at_b, at_c) = futures::join!(
            publish,
            collect(&mut sub_b, start, &done),
            collect(&mut sub_c, start, &done)
        );

        println!(
            "{messages} messages of {PAYLOAD} bytes, {}, publisher subscribed: \
             {publisher_subscribes}; {refused} refused at publish",
            if steady {
                format!("{RATE}/s")
            } else {
                "in one burst".to_string()
            }
        );
        at_b.report("B (neighbour of the publisher)", messages, sub_b.dropped());
        at_c.report("C (through B)", messages, sub_c.dropped());

        for node in [a, b, c] {
            node.shutdown().await;
        }
    }

    #[nuclei::test(threads = 0)]
    #[ignore = "a 30 second measurement, run by hand"]
    async fn rows_from_a_subscribed_publisher() {
        run(true, STEADY, true).await;
    }

    #[nuclei::test(threads = 0)]
    #[ignore = "a 30 second measurement, run by hand"]
    async fn rows_from_a_publisher_that_is_not_subscribed() {
        run(false, STEADY, true).await;
    }

    #[nuclei::test(threads = 0)]
    #[ignore = "a measurement, run by hand"]
    async fn rows_in_one_burst() {
        run(true, BURST, false).await;
    }
}
