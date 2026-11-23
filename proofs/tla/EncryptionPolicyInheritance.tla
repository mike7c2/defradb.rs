---- MODULE EncryptionPolicyInheritance ----
\* The story: one document has a few fields. Each field has a set of heads,
\* the latest blocks written for it, and each head is either encrypted ("E")
\* or plaintext ("P"). A field nobody has written has no heads. A local write
\* updates one field without an encryption config, or with an explicit one
\* (for that field or the whole document). Writes come in requests: a
\* request first runs the KMS preparation of each of its writes in order,
\* minting any fresh key a write needs, and only then builds them, in the
\* same order. So when a later write of a request is prepared, the earlier
\* ones are prepared but not written: their heads are not there yet. The
\* preparation counts each of them as a field with one head, encrypted as
\* its write prepared it (written_fields). Meanwhile P2P merges can add a
\* remote head, plaintext or encrypted, to any field.
\*
\* The rule the write follows (field_encryption_source):
\*   (a) an explicit config encrypts the field with a fresh key;
\*   (b) a field with heads follows them: encrypted if one head is;
\*   (c) a field with no heads gets a fresh document key only when the
\*       document has a field and every field is encrypted; else plaintext.
\* The composite block never links an encryption block, so the docID is Go's.
\*
\* What must never happen: a field with heads ignores them; an encrypted field
\* is written in plaintext without a config; a field with no heads gets the
\* document key on a document that is not wholly encrypted (or is empty); the
\* builder needs a key the preparation never minted; a decision rests on a
\* picture of the heads that never existed; a later write of a request
\* decides without the request's earlier writes; a composite carries a link.
\*
\* What is NOT true, and the model shows it: "every field encrypted" is not
\* monotone. A remote plaintext block for a field with no heads breaks it. So
\* the decision is not safe because the premise only grows; it is safe
\* because the heads are read once, unvalidated, every write is a blind
\* merge, and a stale decision is the decision the write would have taken
\* just before the concurrent merge: it serializes there.
\*
\* Code it models:
\*   crates/database/db/src/block/builder/write.rs  field_encryption_source,
\*       DocScope (new, covers), every_field_encrypted, doc_scope_fields,
\*       build_document_blocks (the field loop, the composite with no link),
\*       write_delete_block_with_inputs (the tombstone with no link)
\*   crates/database/db/src/block/builder/write.rs  DocScope::preparing and
\*       every_field_encrypted's `pending`: an earlier write's field counts
\*       as written, encrypted as prepared; a plaintext one ends rule (c)
\*   crates/database/db/src/block/builder/compute.rs compute_document_blocks
\*       (the create composite with no link)
\*   crates/database/db/src/write/prepare.rs        prepare_keys_with_stores
\*       (doc_scope_fields minted before the write; written_fields taken from
\*       the request's earlier preparation, `prior`, and extended)
EXTENDS Naturals, FiniteSets, Sequences

CONSTANTS
  Fields,             \* the document's fields
  Tear,               \* the field a torn snapshot reads late (RED only)
  MaxWrites,          \* how many local writes may run in all
  MaxReqWrites,       \* how many writes one request may carry
  MaxMerges,          \* how many remote heads may arrive
  KeepCompositeLink,  \* RED: the composite keeps an encryption link
  AnyField,           \* RED: one encrypted field is enough for rule (c)
  PlainHeadedDocKey,  \* RED: a plaintext-headed field takes the document key
  PlainHeadWins,      \* RED: one plaintext head makes the field plaintext
  Vacuous,            \* RED: an empty document counts as wholly encrypted
  UsePrepared,        \* GREEN TRUE: the builder takes the preparation's decision
  UsePending,         \* GREEN TRUE: a preparation counts the request's earlier writes
  TornSnapshot        \* RED: the heads are read field by field, at two times

VARIABLES
  heads,      \* each field's set of head kinds, {} when never written
  compLinked, \* whether a composite written so far links an encryption block
  phase,      \* "idle", "preparing" or "building": where the request is
  queue,      \* the request's prepared writes not built yet, in order
  pend,       \* written_fields: each field the request's preparations wrote,
              \* "E" or "P" as prepared, "none" for a field none wrote
  writes,     \* local writes built so far
  merges,     \* remote heads merged so far
  held,       \* every heads value the document has been at
  last        \* the last write's decision record, or {} before any

vars == <<heads, compLinked, phase, queue, pend, writes, merges, held, last>>

\* The two kinds of head a field can have.
Kinds == {"E", "P"}
\* The three configs a local write can carry.
Configs == {"none", "field", "doc"}
\* written_fields with nothing in it: no write of the request prepared yet.
NoPending == [g \in Fields |-> "none"]

\* The fields that have at least one head in a heads value H.
Written(H) == {g \in Fields : H[g] # {}}

\* The heads as a preparation of the request sees them: the committed heads,
\* except that a field an earlier write of the request prepared has one head,
\* of the kind that write prepared (DocScope::preparing with `pending`).
Overlay(H, P) == [g \in Fields |-> IF P[g] = "none" THEN H[g] ELSE {P[g]}]

\* Rule (c)'s premise as the code must compute it: some field exists and
\* every field that exists is encrypted.
DocAll(H) == Written(H) # {} /\ \A g \in Written(H) : "E" \in H[g]

\* Rule (c)'s premise as a possibly broken implementation computes it.
DocAllImpl(H) ==
  \* AnyField: one encrypted field is taken as enough.
  IF AnyField THEN \E g \in Written(H) : "E" \in H[g]
  \* Vacuous: the empty document passes the "every field" test.
  ELSE (Vacuous \/ Written(H) # {}) /\ \A g \in Written(H) : "E" \in H[g]

\* Rule (b): a field with heads is encrypted when one of its heads is.
FollowHeads(h) == IF "E" \in h THEN "E" ELSE "P"

\* Rule (b) as a possibly broken implementation computes it.
FollowHeadsImpl(f, h) ==
  \* PlainHeadWins: any plaintext head turns the field plaintext.
  IF PlainHeadWins /\ "P" \in h THEN "P"
  \* The field has an encrypted head: reuse its link.
  ELSE IF "E" \in h THEN "E"
  \* PlainHeadedDocKey: a plaintext field joins the document key anyway.
  ELSE IF PlainHeadedDocKey /\ \E g \in Fields \ {f} : "E" \in heads[g] THEN "E"
  \* Only plaintext heads: plaintext.
  ELSE "P"

\* A heads value: each field's set of head kinds.
HeadsValue == [Fields -> SUBSET Kinds]

\* A prepared write: its field and config, whether the preparation gave it
\* the document key and minted a fresh key, the committed heads it read
\* (base), those heads with the request's earlier writes on top (view), and
\* the heads its decision used (snap: the view, or base when RED drops them).
Prepared == [field : Fields, cfg : Configs, scoped : BOOLEAN, minted : BOOLEAN,
             base : HeadsValue, view : HeadsValue, snap : HeadsValue]

\* Every variable has the shape it should.
TypeOK ==
  /\ heads \in HeadsValue                          \* heads per field
  /\ compLinked \in BOOLEAN                        \* a flag
  /\ phase \in {"idle", "preparing", "building"}   \* the request's step
  /\ queue \in Seq(Prepared)                       \* prepared writes
  /\ Len(queue) <= MaxReqWrites                    \* bounded by the request
  /\ pend \in [Fields -> {"none", "E", "P"}]       \* written_fields
  /\ (phase = "idle") <=> (queue = <<>>)           \* idle exactly when empty
  /\ (phase = "idle") => (pend = NoPending)        \* reset between requests
  /\ writes \in 0..MaxWrites                       \* bounded
  /\ merges \in 0..MaxMerges                       \* bounded

\* Start from any document whose fields are unwritten, encrypted or plaintext,
\* the empty document included; nothing written yet in this run.
Init ==
  /\ heads \in [Fields -> {{}, {"E"}, {"P"}}]    \* any starting document
  /\ compLinked = FALSE                          \* no composite linked
  /\ phase = "idle"                              \* no request in flight
  /\ queue = <<>>                                \* nothing prepared
  /\ pend = NoPending                            \* written_fields is empty
  /\ writes = 0                                  \* no write done
  /\ merges = 0                                  \* no merge done
  /\ held = {heads}                              \* the start is a held state
  /\ last = {}                                   \* no decision yet

\* The KMS preparation of the request's next write, to field f with config c.
\* It reads the committed heads once, lays the request's earlier prepared
\* writes over them, decides whether f takes the document key, mints the
\* fresh key the write needs (explicit config, or the document key), and
\* records in written_fields how the write leaves f.
Prepare(f, c) ==
  /\ phase \in {"idle", "preparing"}             \* before the request builds
  /\ Len(queue) < MaxReqWrites                   \* room in the request
  /\ writes + Len(queue) < MaxWrites             \* bounded run
  /\ LET view == Overlay(heads, pend)            \* committed + earlier writes
         \* What the decision reads: the view, or (RED) committed heads only.
         seen == IF UsePending THEN view ELSE heads
         \* Rule (c): no heads, no config, and the document wholly encrypted.
         scoped == seen[f] = {} /\ c = "none" /\ DocAllImpl(seen)
         \* How the write leaves f: config, then its heads, then rule (c).
         enc == IF c # "none" THEN "E"
                ELSE IF seen[f] # {} THEN FollowHeads(seen[f])
                ELSE IF scoped THEN "E" ELSE "P"
     IN /\ queue' = Append(queue, [field |-> f, cfg |-> c, scoped |-> scoped,
                                   minted |-> (c # "none" \/ scoped),
                                   base |-> heads, view |-> view, snap |-> seen])
        /\ pend' = [pend EXCEPT ![f] = enc]      \* written_fields extended
  /\ phase' = "preparing"                        \* more writes may follow
  /\ UNCHANGED <<heads, compLinked, writes, merges, held, last>>

\* Every write of the request is prepared: the request starts building.
Execute ==
  /\ phase = "preparing"                         \* at least one prepared
  /\ phase' = "building"                         \* builds come next
  /\ UNCHANGED <<heads, compLinked, queue, pend, writes, merges, held, last>>

\* The block builder, for the request's first unbuilt write: it reads the
\* field's heads now, decides, writes one new head for the field, and a
\* composite with (GREEN) no encryption link.
Build ==
  /\ phase = "building"                          \* the request is building
  /\ LET r == Head(queue)                        \* the write built now
         f == r.field                            \* the field written
         h == heads[f]                           \* its heads at build time
         \* A torn read: the Tear field read now, the others at prepare time.
         tear(S) == [g \in Fields |-> IF g = Tear THEN heads[g] ELSE S[g]]
         snapD == IF TornSnapshot THEN tear(r.snap) ELSE r.snap
         baseD == IF TornSnapshot THEN tear(r.base) ELSE r.base
         \* Rule (c) for a field with no heads: the preparation's decision, or
         \* (RED) a fresh one on the current heads, or on a torn snapshot.
         headless == IF TornSnapshot THEN DocAllImpl(snapD)
                     ELSE IF UsePrepared THEN r.scoped
                     ELSE DocAllImpl(heads)
         \* The heads a headless decision used, and the committed part of them.
         used == IF UsePrepared \/ TornSnapshot THEN snapD ELSE heads
         usedBase == IF UsePrepared \/ TornSnapshot THEN baseD ELSE heads
         \* The heads it should use: the read with the request's earlier
         \* writes on top; a builder reading now already sees them.
         should == IF UsePrepared \/ TornSnapshot THEN r.view ELSE heads
         \* The decision: explicit config, else rule (b), else rule (c).
         dec == IF r.cfg # "none" THEN "E"
                ELSE IF h # {} THEN FollowHeadsImpl(f, h)
                ELSE IF headless THEN "E" ELSE "P"
         \* A fresh key is needed for an explicit config or the document key.
         fresh == r.cfg # "none" \/ (h = {} /\ dec = "E")
     IN /\ heads' = [heads EXCEPT ![f] = {dec}]  \* the new head supersedes f's
        /\ held' = held \cup {heads'}            \* the new value is held
        /\ last' = {[field |-> f, cfg |-> r.cfg, fheads |-> h, snap |-> used,
                     base |-> usedBase, view |-> should, dec |-> dec,
                     fresh |-> fresh, minted |-> r.minted]}
  /\ compLinked' = (compLinked \/ KeepCompositeLink)  \* GREEN: never linked
  /\ queue' = Tail(queue)                        \* this write is done
  /\ phase' = IF Len(queue) = 1 THEN "idle" ELSE "building"  \* request done?
  /\ pend' = IF Len(queue) = 1 THEN NoPending ELSE pend      \* next one starts empty
  /\ writes' = writes + 1                        \* count it
  /\ UNCHANGED <<merges>>

\* A P2P merge adds a concurrent remote head of kind k to field f: a blind
\* merge of the head-set operand, which never conflicts with the local write.
Merge(f, k) ==
  /\ merges < MaxMerges                          \* bounded run
  /\ heads' = [heads EXCEPT ![f] = @ \cup {k}]   \* one more head for f
  /\ held' = held \cup {heads'}                  \* the new value is held
  /\ merges' = merges + 1                        \* count it
  /\ UNCHANGED <<compLinked, phase, queue, pend, writes, last>>

\* Every step is a preparation, the start of the builds, a build or a merge.
Next ==
  \/ \E f \in Fields, c \in Configs : Prepare(f, c)  \* prepare a write
  \/ Execute                                         \* start building
  \/ Build                                           \* build the next write
  \/ \E f \in Fields, k \in Kinds : Merge(f, k)      \* a remote head arrives

\* The whole behaviour.
Spec == Init /\ [][Next]_vars

\* A field with heads and no config follows its own heads, as Go does.
INV_FollowsOwnHeads ==
  \A r \in last : (r.cfg = "none" /\ r.fheads # {}) => r.dec = FollowHeads(r.fheads)

\* A field with an encrypted head is never written in plaintext.
INV_NoDowngradeOfEncryptedField ==
  \A r \in last : "E" \in r.fheads => r.dec = "E"

\* A field with no heads and no config gets the document key exactly when the
\* heads it decided on have a field and every field encrypted.
INV_HeadlessAllEncrypted ==
  \A r \in last : (r.cfg = "none" /\ r.fheads = {}) => (r.dec = "E" <=> DocAll(r.snap))

\* A headless decision rests on committed heads the document really was at,
\* in which the field had no heads: the write serializes at that point,
\* before any merge that came after the read.
INV_StaleDecisionSerializable ==
  \A r \in last : (r.cfg = "none" /\ r.fheads = {}) =>
                    (r.base \in held /\ r.snap[r.field] = {})

\* A headless decision counts the request's earlier writes: it is the one the
\* document gets with those writes in place, in the request's order, so an
\* update after an encrypted create in the same request takes its key.
INV_SeesRequestWrites ==
  \A r \in last : (r.cfg = "none" /\ r.fheads = {}) => (r.dec = "E" <=> DocAll(r.view))

\* The builder never needs a fresh key the preparation did not mint.
INV_PreparedKeyCovers ==
  \A r \in last : r.fresh => r.minted

\* No composite links an encryption block, so the docID matches Go's.
INV_CompositeUnlinked == ~compLinked

\* The false premise: once every field is encrypted, it stays so. It holds
\* for local writes alone and fails under a remote plaintext merge.
PROP_EncryptedMonotone == [][DocAll(heads) => DocAll(heads')]_heads
====
