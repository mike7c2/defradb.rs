# Iroh Transport Notes

DefraDB's Iroh transport uses the same `P2PTransport` surface as libp2p, but the
wire mechanics are intentionally different where Iroh has different primitives.

## One connection per peer

Every Defra protocol shares a single QUIC connection, negotiated on the mux
ALPN `/defra-iroh/mux/0.2`. A stream names its protocol in-band: the first
frame it writes is a one-byte length followed by a tag such as
`/defra-iroh/docsync/0.1`, and `endpoint_streams::dispatch_stream` routes on
that tag. `protocols.rs` holds the ALPN, the tags, and the frame helpers.

The discriminator has to be in-band because ALPN is negotiated once per TLS
handshake. Naming each protocol with its own ALPN — as this transport did until
the tags were introduced — costs a separate connection, congestion controller,
path MTU probe, hole-punch, and keepalive timer per protocol against the same
peer. An embedded client touching identity, replication, doc sync, branchable
sync, and CAR fetch paid five of each.

Two consequences worth knowing:

- The outbound connection cache in `endpoint_rpc.rs` is keyed by `EndpointId`
  alone, and an explicit dial adopts its connection into that cache. A stream
  failure therefore must not evict it — `evict_if_closed` drops a cached entry
  only once QUIC itself reports the connection closed, since every other
  protocol is still using it.
- Gossip rides the same connection. `gossip/` runs iroh-gossip's protocol
  state (not its networking) over one uni stream per direction, tagged
  `/defra-iroh/gossip/0.1`, each frame a four-byte length and a postcard
  message. Gossip never dials or closes a connection: it asks for the normal
  dial, so revoking a peer (which closes its connections) also ends its gossip.
- Peers on the older `/defra-iroh/mux/0.1` still connect and use every protocol
  but gossip. Both versions are offered on dial and accepted.

DocSync and BranchableSync ride these streams rather than the `pubsub_rpc` layer
introduced for libp2p in #828. The `pubsub_rpc` topic model is tied to libp2p
peer IDs and gossipsub topic meshes. Iroh peers are addressed by `EndpointId`,
so reusing the libp2p response-topic convention would require a peer-id
translation layer that does not exist on the Go-compatible wire path.

The CBOR message structs stay shared with libp2p; only the transport envelope is
Iroh-specific. Iroh SE push and SE query messages are signed with the sending
transport identity and are verified against the QUIC peer that opened the stream
before transport events are emitted.

## An embedder's stream protocols

An application embedding a node can serve protocols of its own on the same
connection (`stream_protocols.rs`). Their tags live outside `/defra-iroh/`,
so a tag Defra adds later never takes one. Admission looks a tag up in Defra's
table first, then in the embedder's, and reads the request exactly as for a
Defra message. The request's room in the budget moves into the
`ProtocolRequest` the handler gets, and comes back when the handler releases
it, so the budget still bounds what a peer's requests hold. The handler
borrows the stream and runs on the stream's own task until it returns, its
deadline passes or the connection closes; shutdown aborts it. Because it only
borrows, nothing of the stream, its request or the request's room outlives
that task. Both ends get wrapped stream halves (`protocol_stream.rs`), the
opener's wrapped on the endpoint task as soon as the stream opens, that reset
when dropped unfinished, where iroh's would finish the stream and pass a
cut-off message as whole.

## An embedder's topics

An application embedding a node can publish and subscribe on gossip topics of
its own (`gossip/app_topics.rs`). A topic named `waterfall/1` gossips as
`/defra-app/waterfall/1`, a prefix no Defra topic has, and
`Delivery::received` refuses that prefix, so Defra never decodes an
application message and an embedder never reads Defra's. The publisher seals
each payload with its endpoint key (`gossip/raw_origin.rs`), and the gossip
driver hands a subscription the sealer, not the neighbour that relayed the
message. A message from a revoked peer is dropped even when an admitted
neighbour relays it; the inbound allowlist decides who connects, not whose
relayed messages are read.

Nothing is stored and nothing is retried. A message is lost when the
publisher has no neighbour on the topic, when a peer's send queue (64
messages) or the driver's inbox (1024) is full, and when a subscription's
buffer (`APP_TOPIC_BUFFER`) is full; the subscription counts only the last.
iroh-gossip identifies a message by the hash of its bytes, so a payload
published twice within its ID retention (90 seconds) is delivered once.

## Raw topics

`publish_raw`, `subscribe_raw` and `register_pubsub_rpc_topic` carry the KMS
`pubsub_rpc` exchange over gossip. A raw payload is sealed by its publisher
and at most `max_raw_publish()` bytes; a received one becomes a
`TransportEvent::GossipRawMessage` for the coordinator's dispatcher.

`topic_peers()` is also not a full libp2p equivalent on Iroh. libp2p can ask
gossipsub for all peers known for a topic. `iroh-gossip` exposes the direct
neighbors of a joined topic, so Iroh returns those topic-scoped neighbors rather
than all connected peers. This keeps the result topic-specific, but callers must
not treat it as complete topic membership. If the local node has not subscribed
to the topic, Iroh returns an empty list; callers that relied on the previous
all-connected fallback should subscribe first or use `connected_peers()`.

## Audit follow-up references

This file documents the Iroh transport decisions behind #965, #966, #967, and
#968, and the surrounding P2P audit work tracked by #962 through #968.
