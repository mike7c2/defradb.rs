//! An application topic's name stays out of Defra's, and a subscriber is
//! handed only what its publisher sealed for that topic.

use iroh::SecretKey;

use super::*;
use crate::iroh::endpoint_config::AllowlistState;

const NAME: &str = "/defra-app/waterfall/1";

fn subscriber() -> (Subscriber, AppSubscription, Arc<PeerAdmission>) {
    let admission = Arc::new(PeerAdmission::new(AllowlistState::AcceptAll));
    let (subscriber, subscription) = Subscriber::new(
        NAME.to_string(),
        Arc::clone(&admission),
        Arc::new(Inbox::new(1)),
    );
    (subscriber, subscription, admission)
}

fn sealed(key: &SecretKey, topic: &str, payload: &[u8]) -> Bytes {
    Bytes::from(raw_origin::seal(key, topic, payload).unwrap())
}

#[test]
fn a_name_gossips_under_the_application_prefix() {
    assert_eq!(gossip_name("waterfall/1").unwrap(), NAME);
    assert_eq!(gossip_name("doc-sync").unwrap(), "/defra-app/doc-sync");
    assert!(is_app_topic(NAME));
    for defra in ["doc-sync", "encryption", "defra/service-records/1"] {
        assert!(!is_app_topic(defra));
    }
}

#[test]
fn a_name_outside_one_to_128_bytes_is_refused() {
    assert!(gossip_name("").is_err());
    assert!(gossip_name("a").is_ok());
    assert!(gossip_name(&"a".repeat(MAX_APP_TOPIC_LEN)).is_ok());
    let error = gossip_name(&"a".repeat(MAX_APP_TOPIC_LEN + 1)).unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid application topic: a name of 129 bytes; a name is 1 to 128 bytes"
    );
}

#[test]
fn a_sealed_message_names_its_publisher() {
    let (subscriber, mut subscription, _) = subscriber();
    let publisher = SecretKey::generate();
    subscriber.deliver(NAME, &sealed(&publisher, NAME, b"row"));

    let message = subscription.try_recv().unwrap();
    assert_eq!(message.sender, endpoint_id_to_peer_id(&publisher.public()));
    assert_eq!(message.payload.as_ref(), b"row");
    assert_eq!(subscription.topic(), "waterfall/1");
}

#[test]
fn an_unsealed_forged_or_retargeted_message_is_dropped() {
    let (subscriber, mut subscription, _) = subscriber();
    let publisher = SecretKey::generate();

    subscriber.deliver(NAME, &Bytes::from_static(b"row"));
    let mut forged = sealed(&publisher, NAME, b"row").to_vec();
    *forged.last_mut().unwrap() ^= 1;
    subscriber.deliver(NAME, &Bytes::from(forged));
    subscriber.deliver(NAME, &sealed(&publisher, "/defra-app/other", b"row"));

    assert!(subscription.try_recv().is_none());
    assert_eq!(subscription.dropped(), 0);
}

#[test]
fn a_revoked_publisher_is_not_heard() {
    let (subscriber, mut subscription, admission) = subscriber();
    let publisher = SecretKey::generate();
    admission.revoke(publisher.public());
    subscriber.deliver(NAME, &sealed(&publisher, NAME, b"row"));
    assert!(subscription.try_recv().is_none());
}

#[test]
fn a_full_buffer_drops_and_counts() {
    let (subscriber, mut subscription, _) = subscriber();
    let publisher = SecretKey::generate();
    for n in 0..APP_TOPIC_BUFFER + 3 {
        subscriber.deliver(NAME, &sealed(&publisher, NAME, &n.to_be_bytes()));
    }
    assert_eq!(subscription.dropped(), 3);
    let first = subscription.try_recv().unwrap();
    assert_eq!(first.payload.as_ref(), 0usize.to_be_bytes());
}
