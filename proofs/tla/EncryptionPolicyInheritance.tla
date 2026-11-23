---- MODULE EncryptionPolicyInheritance ----
\* The story: one document has a few fields. Each field has a set of heads,
\* the latest blocks written for it, and each head is either encrypted ("E")
\* or plaintext ("P"). A field nobody has written has no heads. A local write
\* updates one field without an encryption config, or with an explicit one
\* (for that field or the whole document). It runs in two steps: a KMS
\* preparation reads the heads and mints any fresh key the write needs, then
\* the block builder decides the block and writes it. Meanwhile P2P merges
\* can add a remote head, plaintext or encrypted, to any field.
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
\* picture of the heads that never existed; a composite carries a link.
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
\*   crates/database/db/src/block/builder/compute.rs compute_document_blocks
\*       (the create composite with no link)
\*   crates/database/db/src/write/prepare.rs        prepare_keys_with_stores
\*       (doc_scope_fields minted before the write, written_fields carried)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Fields,             \* the document's fields
  Tear,               \* the field a torn snapshot reads late (RED only)
  MaxWrites,          \* how many local writes may run
  MaxMerges,          \* how many remote heads may arrive
  KeepCompositeLink,  \* RED: the composite keeps an encryption link
  AnyField,           \* RED: one encrypted field is enough for rule (c)
  PlainHeadedDocKey,  \* RED: a plaintext-headed field takes the document key
  PlainHeadWins,      \* RED: one plaintext head makes the field plaintext
  Vacuous,            \* RED: an empty document counts as wholly encrypted
  UsePrepared,        \* GREEN TRUE: the builder takes the preparation's decision
  TornSnapshot        \* RED: the heads are read field by field, at two times

VARIABLES
  heads,      \* each field's set of head kinds, {} when never written
  compLinked, \* whether a composite written so far links an encryption block
  phase,      \* "idle" or "prepared": where the one local write is
  target,     \* the field the local write updates
  cfgW,       \* the local write's config: "none", "field" or "doc"
  prepSnap,   \* the heads the preparation read
  prepScope,  \* the fields the preparation decided get the document key
  minted,     \* whether the preparation minted a fresh key
  writes,     \* local writes done so far
  merges,     \* remote heads merged so far
  held,       \* every heads value the document has been at
  last        \* the last write's decision record, or {} before any

vars == <<heads, compLinked, phase, target, cfgW, prepSnap, prepScope,
          minted, writes, merges, held, last>>

\* The two kinds of head a field can have.
Kinds == {"E", "P"}
\* The three configs a local write can carry.
Configs == {"none", "field", "doc"}

\* The fields that have at least one head in a heads value H.
Written(H) == {g \in Fields : H[g] # {}}

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

\* Every variable has the shape it should.
TypeOK ==
  /\ heads \in [Fields -> SUBSET Kinds]          \* heads per field
  /\ compLinked \in BOOLEAN                      \* a flag
  /\ phase \in {"idle", "prepared"}              \* the write's step
  /\ target \in Fields                           \* a field
  /\ cfgW \in Configs                            \* a config
  /\ prepSnap \in [Fields -> SUBSET Kinds]       \* a heads value
  /\ prepScope \subseteq Fields                  \* some fields
  /\ minted \in BOOLEAN                          \* a flag
  /\ writes \in 0..MaxWrites                     \* bounded
  /\ merges \in 0..MaxMerges                     \* bounded

\* Start from any document whose fields are unwritten, encrypted or plaintext,
\* the empty document included; nothing written yet in this run.
Init ==
  /\ heads \in [Fields -> {{}, {"E"}, {"P"}}]    \* any starting document
  /\ compLinked = FALSE                          \* no composite linked
  /\ phase = "idle"                              \* no write in flight
  /\ target = CHOOSE g \in Fields : TRUE         \* placeholder field
  /\ cfgW = "none"                               \* placeholder config
  /\ prepSnap = heads                            \* placeholder snapshot
  /\ prepScope = {}                              \* nothing decided
  /\ minted = FALSE                              \* nothing minted
  /\ writes = 0                                  \* no write done
  /\ merges = 0                                  \* no merge done
  /\ held = {heads}                              \* the start is a held state
  /\ last = {}                                   \* no decision yet

\* The KMS preparation of a write to field f with config c: it reads the heads
\* once, decides whether f takes the document key, and mints the fresh key the
\* write needs (explicit config, or the document key).
Prepare(f, c) ==
  /\ phase = "idle"                              \* one write at a time
  /\ writes < MaxWrites                          \* bounded run
  /\ LET scoped == heads[f] = {} /\ c = "none" /\ DocAllImpl(heads)  \* rule (c)
     IN /\ prepScope' = IF scoped THEN {f} ELSE {}                    \* record it
        /\ minted' = (c # "none" \/ scoped)                           \* mint if needed
  /\ prepSnap' = heads                           \* the one read of the heads
  /\ target' = f                                 \* remember the field
  /\ cfgW' = c                                   \* remember the config
  /\ phase' = "prepared"                         \* the build comes next
  /\ UNCHANGED <<heads, compLinked, writes, merges, held, last>>

\* The block builder: it reads the field's heads now, decides, writes one new
\* head for the field, and a composite with (GREEN) no encryption link.
Build ==
  /\ phase = "prepared"                          \* a preparation ran
  /\ LET f == target                             \* the field written
         h == heads[f]                           \* its heads at build time
         \* A torn read: the Tear field read now, the others at prepare time.
         snapD == IF TornSnapshot
                  THEN [g \in Fields |-> IF g = Tear THEN heads[g] ELSE prepSnap[g]]
                  ELSE prepSnap
         \* Rule (c) for a field with no heads: the preparation's decision, or
         \* (RED) a fresh one on the current heads, or on a torn snapshot.
         headless == IF TornSnapshot THEN DocAllImpl(snapD)
                     ELSE IF UsePrepared THEN f \in prepScope
                     ELSE DocAllImpl(heads)
         \* The heads value the decision for a headless field rests on.
         used == IF UsePrepared \/ TornSnapshot THEN snapD ELSE heads
         \* The decision: explicit config, else rule (b), else rule (c).
         dec == IF cfgW # "none" THEN "E"
                ELSE IF h # {} THEN FollowHeadsImpl(f, h)
                ELSE IF headless THEN "E" ELSE "P"
         \* A fresh key is needed for an explicit config or the document key.
         fresh == cfgW # "none" \/ (h = {} /\ dec = "E")
     IN /\ heads' = [heads EXCEPT ![f] = {dec}]  \* the new head supersedes f's
        /\ held' = held \cup {heads'}            \* the new value is held
        /\ last' = {[field |-> f, cfg |-> cfgW, fheads |-> h, snap |-> used,
                     dec |-> dec, fresh |-> fresh, minted |-> minted]}
  /\ compLinked' = (compLinked \/ KeepCompositeLink)  \* GREEN: never linked
  /\ phase' = "idle"                             \* the write is done
  /\ writes' = writes + 1                        \* count it
  /\ UNCHANGED <<target, cfgW, prepSnap, prepScope, minted, merges>>

\* A P2P merge adds a concurrent remote head of kind k to field f: a blind
\* merge of the head-set operand, which never conflicts with the local write.
Merge(f, k) ==
  /\ merges < MaxMerges                          \* bounded run
  /\ heads' = [heads EXCEPT ![f] = @ \cup {k}]   \* one more head for f
  /\ held' = held \cup {heads'}                  \* the new value is held
  /\ merges' = merges + 1                        \* count it
  /\ UNCHANGED <<compLinked, phase, target, cfgW, prepSnap, prepScope,
                 minted, writes, last>>

\* Every step is a preparation, a build or a merge.
Next ==
  \/ \E f \in Fields, c \in Configs : Prepare(f, c)  \* start a local write
  \/ Build                                           \* finish it
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
\* document it decided on has a field and every field is encrypted.
INV_HeadlessAllEncrypted ==
  \A r \in last : (r.cfg = "none" /\ r.fheads = {}) => (r.dec = "E" <=> DocAll(r.snap))

\* A headless decision rests on a heads value the document really was at, in
\* which the field had no heads: the write serializes at that point, before
\* any merge that came after the read.
INV_StaleDecisionSerializable ==
  \A r \in last : (r.cfg = "none" /\ r.fheads = {}) => (r.snap \in held /\ r.snap[r.field] = {})

\* The builder never needs a fresh key the preparation did not mint.
INV_PreparedKeyCovers ==
  \A r \in last : r.fresh => r.minted

\* No composite links an encryption block, so the docID matches Go's.
INV_CompositeUnlinked == ~compLinked

\* The false premise: once every field is encrypted, it stays so. It holds
\* for local writes alone and fails under a remote plaintext merge.
PROP_EncryptedMonotone == [][DocAll(heads) => DocAll(heads')]_heads
====
