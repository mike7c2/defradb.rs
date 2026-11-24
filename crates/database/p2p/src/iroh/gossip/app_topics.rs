//! Topics an embedder publishes and subscribes to beside Defra's own.
//!
//! An application topic gossips as `/defra-app/<name>`. No Defra topic starts
//! with that prefix (a collection or document ID, `doc-sync`, `encryption`,
//! their `_response` sub-topics, the service records), and Defra's delivery
//! refuses it, so neither side ever reads the other's messages.
//!
//! Every message is sealed by its publisher (`raw_origin`) over the gossiped
//! name and the payload. A subscriber is told the sealer, never the neighbour
//! that relayed the message. Messages arrive only over connections this node
//! admitted, and one sealed by a peer this node revoked is dropped even when
//! an admitted neighbour relays it. The inbound allowlist decides who may
//! connect, not whose relayed messages are read.
//!
//! Gossip keeps nothing: a message published with no neighbour on the topic,
//! dropped on a full queue on its way, or arriving while the subscriber's
//! buffer is full is lost. A payload published twice within iroh-gossip's
//! message-ID retention is delivered once, because the ID is the hash of the
//! sealed bytes; a publisher that may repeat a payload numbers it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use tracing::debug;

use super::inbox::Inbox;
use super::raw_origin::{self, Opened, MAX_RAW_PAYLOAD};
use super::Input;
use crate::error::{Error, Result};
use crate::iroh::endpoint_config::PeerAdmission;
use crate::iroh::peer_map::endpoint_id_to_peer_id;
use crate::rt::mpsc;
use crate::transport::PeerId;

/// Every application topic gossips under this, so a topic Defra adds later
/// cannot take an embedder's.
pub const APP_TOPIC_PREFIX: &str = "/defra-app/";

/// The longest application topic name, in bytes.
pub const MAX_APP_TOPIC_LEN: usize = 128;

/// The largest payload an application topic carries.
pub const MAX_APP_PAYLOAD: usize = MAX_RAW_PAYLOAD;

/// Messages a subscription holds before more are dropped.
pub const APP_TOPIC_BUFFER: usize = 256;

/// The name `topic` gossips under.
pub(in crate::iroh) fn gossip_name(topic: &str) -> Result<String> {
    if topic.is_empty() || topic.len() > MAX_APP_TOPIC_LEN {
        return Err(Error::InvalidAppTopic(format!(
            "a name of {} bytes; a name is 1 to {MAX_APP_TOPIC_LEN} bytes",
            topic.len()
        )));
    }
    Ok(format!("{APP_TOPIC_PREFIX}{topic}"))
}

pub(super) fn is_app_topic(gossip_name: &str) -> bool {
    gossip_name.starts_with(APP_TOPIC_PREFIX)
}

/// A message received on an application topic.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AppMessage {
    /// The peer that published the message, proved by its signature over the
    /// topic and the payload. A relaying neighbour is never named.
    pub sender: PeerId,
    /// What the sender published.
    pub payload: Bytes,
}

/// A subscription to one application topic. Dropping it leaves the topic.
pub struct AppSubscription {
    gossip_name: String,
    messages: mpsc::Receiver<AppMessage>,
    dropped: Arc<AtomicU64>,
    inbox: Arc<Inbox>,
}

impl AppSubscription {
    /// The topic's name, as it was subscribed.
    pub fn topic(&self) -> &str {
        &self.gossip_name[APP_TOPIC_PREFIX.len()..]
    }

    /// The next message, or `None` once the node has shut down.
    pub async fn recv(&mut self) -> Option<AppMessage> {
        self.messages.recv().await
    }

    /// A message already received, without waiting.
    pub fn try_recv(&mut self) -> Option<AppMessage> {
        self.messages.try_recv().ok()
    }

    /// Messages dropped so far because [`APP_TOPIC_BUFFER`] were waiting to
    /// be read.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for AppSubscription {
    fn drop(&mut self) {
        self.inbox
            .push(Input::AppQuit(std::mem::take(&mut self.gossip_name)));
    }
}

/// The driver's end of an [`AppSubscription`].
pub(super) struct Subscriber {
    messages: mpsc::Sender<AppMessage>,
    dropped: Arc<AtomicU64>,
    admission: Arc<PeerAdmission>,
}

impl Subscriber {
    pub(super) fn new(
        gossip_name: String,
        admission: Arc<PeerAdmission>,
        inbox: Arc<Inbox>,
    ) -> (Self, AppSubscription) {
        let (tx, rx) = mpsc::channel(APP_TOPIC_BUFFER);
        let dropped = Arc::new(AtomicU64::new(0));
        let subscriber = Self {
            messages: tx,
            dropped: Arc::clone(&dropped),
            admission,
        };
        let subscription = AppSubscription {
            gossip_name,
            messages: rx,
            dropped,
            inbox,
        };
        (subscriber, subscription)
    }

    /// Hands `content` to the subscription if its seal holds and this node
    /// has not revoked the sealer. Never waits: a full buffer drops the
    /// message.
    pub(super) fn deliver(&self, gossip_name: &str, content: &Bytes) {
        let (origin, payload) = match raw_origin::open(gossip_name, content) {
            Ok(Opened::Signed { origin, payload }) => (origin, content.slice_ref(payload)),
            Ok(Opened::Unsigned(_)) => {
                debug!(
                    topic = gossip_name,
                    "Dropping an unsealed application message"
                );
                return;
            }
            Err(error) => {
                debug!(topic = gossip_name, %error, "Dropping an application message whose seal does not hold");
                return;
            }
        };
        if self.admission.is_revoked(&origin) {
            debug!(
                topic = gossip_name,
                origin = %origin.fmt_short(),
                "Dropping an application message from a revoked peer"
            );
            return;
        }
        let message = AppMessage {
            sender: endpoint_id_to_peer_id(&origin),
            payload,
        };
        if self.messages.try_send(message).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/iroh/gossip_app_topics.rs"]
mod tests;
