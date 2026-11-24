/-!
# A signed create's ID is known before it commits

The story, in plain words. A node or a client makes a new document. It
builds one small block per field, signs each one, then builds the document's
top block (the genesis composite) whose links are the field blocks' content
addresses, and signs that too. The document's ID is a UUID of the genesis
block's content address. Whoever builds those blocks therefore knows the ID
before anything is written.

Who acts: the creator (a node that signs, or a client that signs its own
graph), the node that commits, a peer that receives the blocks, a schema
migration, and a backup import.

What must never happen: a path commits the document under an ID other than
the one its creator computed. This file proves that for every path but one,
and shows the one exception (backup import re-creates the document under a
new genesis, so a new ID). It also proves that both kinds of signature reach
the ID, that a signer whose signature does not depend on a random nonce gives
the same ID every time, that one that does can give two IDs, that a create
whose ID is already stored is refused, and that a buggy builder which signs
the genesis after its address is taken loses the signer from the ID.

Hashes are the crypto boundary: we take them as injective (two different
blocks never share an address), as `proofs/README.md` records.

Rust owners:
* crates/database/db/src/block/builder/compute.rs `compute_document_blocks`
  (field blocks signed before their CIDs; the composite signed before its CID).
* crates/database/db/src/block/builder/mod.rs `derive_doc_id`
  (the ID is `DocID::new_v0` of the genesis CID).
* crates/database/db/src/block/builder/verified_client_create.rs
  `VerifiedClientCreate::verify`, `ExactSignatures::sign_sync` (a client-signed
  create replays only the client's signatures).
* crates/entry-points/defra-node/src/lib.rs `execute_verified_graph_in_txn`
  (refuses a committed `_docID` other than `graph.doc_id()`).
* crates/surfaces/http/src/client_signed_create.rs `conversation_on_socket`
  (the Rust client refuses an out-of-graph wave and a different `_docID`).
* crates/database/db/src/merge/merge_handler/doc_identity.rs (a peer derives
  the ID from the genesis it received).
* crates/database/db/src/write/autocommit/helpers.rs `register_created_doc`
  (a stored ID is refused, the `get_doc_ref` branch).
* crates/database/db/src/backup/import.rs (import creates again; the file's ID
  becomes an alias).

Verify: `cd proofs/lean && lake build Cid`. No `sorry`, `admit` or `axiom`.
-/

namespace Cid.SignedGenesis

-- A content address, kept abstract as a number.
abbrev Address := Nat
-- A document ID, kept abstract as a number.
abbrev DocId := Nat
-- A signing key, named by its public key.
abbrev Key := Nat
-- The randomness a signer may mix in; a deterministic signer ignores it.
abbrev Nonce := Nat

-- One field's block: which field, its encoded value, and the address of its
-- signature block once it is signed (`none` before signing).
structure FieldBlock where
  -- The field the block writes.
  field : Nat
  -- The field's encoded value.
  value : Nat
  -- The signature block's address, or `none` while unsigned.
  sig : Option Address
  deriving DecidableEq, Repr

-- The genesis composite block: the collection version it belongs to, the
-- addresses of its field blocks, and its own signature's address.
structure Composite where
  -- The collection version ID the genesis names.
  version : Nat
  -- The field blocks' addresses, in order.
  links : List Address
  -- The composite's signature block address, or `none` while unsigned.
  sig : Option Address
  deriving DecidableEq, Repr

-- What a signer signs: an unsigned field block or an unsigned composite.
inductive Payload where
  -- A field block before its signature is set.
  | field (b : FieldBlock)
  -- A composite before its signature is set.
  | composite (c : Composite)

-- A signature block: the public key it names and the signature value.
structure SigBlock where
  -- The signer's public key (the signature block names it).
  key : Key
  -- The signature bytes, kept abstract.
  value : Nat
  deriving DecidableEq, Repr

-- Our own injectivity, so the file needs nothing beyond Lean's core.
def Injective {α β : Type} (f : α → β) : Prop :=
  -- Equal outputs only come from equal inputs.
  ∀ a b, f a = f b → a = b

-- The world the proofs live in: how blocks get addresses, how an address
-- becomes an ID, and how a key signs.
structure World where
  -- The address of a field block (DAG-CBOR then SHA-256).
  addrF : FieldBlock → Address
  -- The address of a composite block.
  addrC : Composite → Address
  -- The address of a signature block.
  addrS : SigBlock → Address
  -- `derive_doc_id`: the ID of a genesis address (UUIDv5, version 0).
  docIdOf : Address → DocId
  -- A signer: key, payload and nonce give the signature value.
  sign : Key → Payload → Nonce → Nat

-- Sign one field block: sign its unsigned form, then set the signature's
-- address. This is compute.rs setting `field_block.signature` before encoding.
def signField (w : World) (k : Key) (n : Nonce) (b : FieldBlock) : FieldBlock :=
  -- The block keeps its field and value and gains its signature's address.
  { b with sig := some (w.addrS ⟨k, w.sign k (.field { b with sig := none }) n⟩) }

-- The genesis composite a create builds: sign every field block, link their
-- addresses, then sign the composite over its unsigned form.
def genesis (w : World) (k : Key) (n : Nonce) (version : Nat)
    (fields : List FieldBlock) : Composite :=
  -- The unsigned composite: the version and the signed fields' addresses.
  let unsigned : Composite :=
    { version := version, links := fields.map (fun b => w.addrF (signField w k n b)), sig := none }
  -- The signed composite: the same, with its own signature's address.
  { unsigned with sig := some (w.addrS ⟨k, w.sign k (.composite unsigned) n⟩) }

-- The ID a creator computes before anything commits: the ID of its genesis.
def precommitId (w : World) (k : Key) (n : Nonce) (version : Nat)
    (fields : List FieldBlock) : DocId :=
  -- `derive_doc_id(genesis_cid)`.
  w.docIdOf (w.addrC (genesis w k n version fields))

/-! ## Every path but import commits the precomputed ID -/

-- The ways a document reaches its committed state.
inductive Path where
  -- A node signs and commits in autocommit.
  | nodeAutocommit
  -- A node signs inside an explicit transaction, then it commits.
  | nodeExplicitTxn
  -- A client signs its own graph; the node replays exactly those signatures.
  | clientSigned
  -- A peer receives the genesis block and merges it.
  | peerMerge
  -- A schema migration keeps the one genesis the document has.
  | migration
  deriving DecidableEq, Repr

-- What a client-signed create's checks found: the replay used only the
-- client's signatures, and the committed ID was compared with the graph's.
structure ClientChecks where
  -- `ExactSignatures::sign_sync` replayed only the approved signatures.
  replayExact : Bool
  -- The committed `_docID` equals `graph.doc_id()` (lib.rs and the Rust client).
  idMatches : Bool

-- The ID a path commits, or `none` when the path refuses. Node paths commit
-- the genesis they computed; a peer derives the ID from the genesis it
-- received, which is the same block; a migration takes the unique genesis; a
-- client-signed create commits only when both of its checks hold.
def committedId (w : World) (k : Key) (n : Nonce) (version : Nat)
    (fields : List FieldBlock) (checks : ClientChecks) : Path → Option DocId
  -- Autocommit: the same `compute_document_blocks` result is written.
  | .nodeAutocommit => some (w.docIdOf (w.addrC (genesis w k n version fields)))
  -- Explicit transaction: the ID the response named is the ID committed.
  | .nodeExplicitTxn => some (w.docIdOf (w.addrC (genesis w k n version fields)))
  -- Client-signed: committed only when the replay and the ID check pass.
  | .clientSigned =>
      -- Both checks must hold, else the transaction is discarded.
      if checks.replayExact && checks.idMatches then
        -- The replayed graph is the client's own genesis.
        some (w.docIdOf (w.addrC (genesis w k n version fields)))
      else
        -- A refusal: nothing commits.
        none
  -- A peer: `derive_doc_id` of the genesis block's address it received.
  | .peerMerge => some (w.docIdOf (w.addrC (genesis w k n version fields)))
  -- A migration: the document's one genesis, unchanged.
  | .migration => some (w.docIdOf (w.addrC (genesis w k n version fields)))

-- Promise: on every path, whatever commits is the ID computed before commit.
theorem precommit_eq_committed (w : World) (k : Key) (n : Nonce) (version : Nat)
    (fields : List FieldBlock) (checks : ClientChecks) (p : Path) (id : DocId)
    (h : committedId w k n version fields checks p = some id) :
    id = precommitId w k n version fields := by
  -- Look at each path in turn.
  cases p with
  -- Autocommit: the committed ID is the precomputed one by definition.
  | nodeAutocommit => simp [committedId] at h; simp [precommitId, h]
  -- Explicit transaction: the same.
  | nodeExplicitTxn => simp [committedId] at h; simp [precommitId, h]
  -- Client-signed: a commit happens only on the branch that keeps the ID.
  | clientSigned =>
      -- Unfold the path's rule.
      simp only [committedId] at h
      -- Split on whether both checks passed.
      split at h
      -- They passed: the committed ID is the graph's.
      · simp at h; simp [precommitId, h]
      -- They did not: nothing committed, so this case cannot arise.
      · exact absurd h (by simp)
  -- A peer: the received genesis is the creator's.
  | peerMerge => simp [committedId] at h; simp [precommitId, h]
  -- A migration: the unique genesis.
  | migration => simp [committedId] at h; simp [precommitId, h]

-- Promise: a client-signed create whose ID check fails commits nothing.
theorem client_signed_refuses_a_different_id (w : World) (k : Key) (n : Nonce)
    (version : Nat) (fields : List FieldBlock) (checks : ClientChecks)
    (h : checks.idMatches = false) :
    committedId w k n version fields checks .clientSigned = none := by
  -- With the ID check false, the rule takes its refusing branch.
  simp [committedId, h]

/-! ## Import is the one exception -/

-- Backup import creates the document again on the importing node, with that
-- node's key and nonce, so it builds a new genesis.
def importedId (w : World) (k' : Key) (n' : Nonce) (version : Nat)
    (fields : List FieldBlock) : DocId :=
  -- A fresh create: its own precomputed ID.
  precommitId w k' n' version fields

-- Promise: import keeps the ID only when the importing node's signature is the
-- creator's; with injective addresses, a different composite signature gives a
-- different ID. So import does not preserve identity in general.
theorem import_is_not_identity_preserving (w : World)
    (hC : Injective w.addrC) (hId : Injective w.docIdOf) (hS : Injective w.addrS)
    (k k' : Key) (n n' : Nonce) (version : Nat) (fields : List FieldBlock)
    (hk : k ≠ k') :
    importedId w k' n' version fields ≠ precommitId w k n version fields := by
  -- Suppose the two IDs were equal.
  intro heq
  -- Equal IDs mean equal genesis addresses.
  have haddr := hId _ _ heq
  -- Equal addresses mean equal genesis blocks.
  have hblock := hC _ _ haddr
  -- Equal blocks have equal signature addresses.
  have hsig := congrArg Composite.sig hblock
  -- Unfold the two signatures.
  simp only [genesis] at hsig
  -- Equal `some` values: equal signature addresses.
  have hsa := Option.some.inj hsig
  -- Equal signature addresses mean equal signature blocks.
  have hsb := hS _ _ hsa
  -- So their keys are equal, which contradicts `k ≠ k'`.
  exact hk (congrArg SigBlock.key hsb).symm

/-! ## Both kinds of signature reach the ID -/

-- Promise: two genesis blocks that differ in the composite's signature have
-- different IDs (the composite is signed before its address is taken).
theorem composite_sig_reaches_doc_id (w : World)
    (hC : Injective w.addrC) (hId : Injective w.docIdOf)
    (c₁ c₂ : Composite) (h : c₁.sig ≠ c₂.sig) :
    w.docIdOf (w.addrC c₁) ≠ w.docIdOf (w.addrC c₂) := by
  -- Suppose the IDs were equal.
  intro heq
  -- Then the blocks are equal, so their signatures are equal.
  exact h (congrArg Composite.sig (hC _ _ (hId _ _ heq)))

-- Promise: two genesis blocks whose field links differ (a field block's
-- signature changed its address) have different IDs.
theorem field_sig_reaches_doc_id (w : World)
    (hC : Injective w.addrC) (hId : Injective w.docIdOf)
    (c₁ c₂ : Composite) (h : c₁.links ≠ c₂.links) :
    w.docIdOf (w.addrC c₁) ≠ w.docIdOf (w.addrC c₂) := by
  -- Suppose the IDs were equal.
  intro heq
  -- Then the blocks are equal, so their links are equal.
  exact h (congrArg Composite.links (hC _ _ (hId _ _ heq)))

-- Promise: a field signature reaches the field's address: two signed field
-- blocks with different signature blocks have different addresses.
theorem field_sig_reaches_field_address (w : World)
    (hF : Injective w.addrF) (hS : Injective w.addrS)
    (b : FieldBlock) (s₁ s₂ : SigBlock) (h : s₁ ≠ s₂) :
    w.addrF { b with sig := some (w.addrS s₁) } ≠ w.addrF { b with sig := some (w.addrS s₂) } := by
  -- Suppose the addresses were equal.
  intro heq
  -- Then the blocks are equal, so their signature addresses are equal.
  have hb := congrArg FieldBlock.sig (hF _ _ heq)
  -- Peel the `some` and the signature address.
  exact h (hS _ _ (Option.some.inj hb))

/-! ## Determinism of the signer -/

-- Promise: a signer that ignores its nonce (Ed25519, Secp256k1 with RFC 6979)
-- gives the same ID on every run.
theorem nonce_independent_signer_same_doc_id (w : World)
    (hdet : ∀ k p n n', w.sign k p n = w.sign k p n')
    (k : Key) (n n' : Nonce) (version : Nat) (fields : List FieldBlock) :
    precommitId w k n version fields = precommitId w k n' version fields := by
  -- The signed field blocks are the same under both nonces.
  have hf : ∀ b, signField w k n b = signField w k n' b := by
    -- Each field's signature value ignores the nonce.
    intro b; simp [signField, hdet k _ n n']
  -- So the genesis blocks are the same, and so the IDs.
  simp [precommitId, genesis, hf, hdet k _ n n']

-- Promise: a signer whose composite signature depends on its nonce
-- (randomized ECDSA, Secp256r1) gives two IDs for the same content.
theorem nonce_dependent_signer_can_differ (w : World)
    (hC : Injective w.addrC) (hId : Injective w.docIdOf) (hS : Injective w.addrS)
    (k : Key) (n n' : Nonce) (version : Nat) (fields : List FieldBlock)
    (hfields : ∀ b, signField w k n b = signField w k n' b)
    (hdiff : ∀ c, w.sign k (.composite c) n ≠ w.sign k (.composite c) n') :
    precommitId w k n version fields ≠ precommitId w k n' version fields := by
  -- Suppose the two IDs were equal.
  intro heq
  -- Then the genesis blocks are equal.
  have hblock := hC _ _ (hId _ _ heq)
  -- So their signature addresses are equal.
  have hsig := congrArg Composite.sig hblock
  -- Unfold both signatures; the field links agree by `hfields`.
  simp only [genesis, hfields] at hsig
  -- Equal signature addresses mean equal signature blocks, so equal values.
  have hv := congrArg SigBlock.value (hS _ _ (Option.some.inj hsig))
  -- That contradicts the nonce changing the composite's signature.
  exact hdiff _ hv

/-! ## A stored ID is refused -/

-- What the store holds under an ID: nothing, a live document, or a deleted one.
inductive Slot where
  -- No document has this ID.
  | absent
  -- A live document has this ID.
  | live
  -- A deleted document has this ID.
  | deleted
  deriving DecidableEq, Repr

-- Why a create is refused, with the ID it names (Go's two texts).
inductive Refusal where
  -- `a document with the given ID already exists. DocID: <id>`.
  | alreadyExists (id : DocId)
  -- `a document with the given ID has been deleted. DocID: <id>`.
  | deleted (id : DocId)
  deriving DecidableEq, Repr

-- `register_created_doc`'s `get_doc_ref` branch: a stored ID is refused,
-- deleted or not; an absent one is registered as live.
def register (store : DocId → Slot) (id : DocId) : Except Refusal (DocId → Slot) :=
  -- Look the create's ID up.
  match store id with
  -- Not stored: register it as live, leaving every other ID alone.
  | .absent => .ok (fun x => if x = id then .live else store x)
  -- Stored and live: refused as existing.
  | .live => .error (.alreadyExists id)
  -- Stored and deleted: refused as deleted; it stays deleted.
  | .deleted => .error (.deleted id)

-- Promise: a create whose ID is stored is refused with its own ID, and writes
-- nothing; the refusal says which state the ID was in.
theorem collision_refused (store : DocId → Slot) (id : DocId) (h : store id ≠ .absent) :
    register store id = .error (.alreadyExists id) ∧ store id = .live ∨
    register store id = .error (.deleted id) ∧ store id = .deleted := by
  -- Look at what the store holds under the ID.
  cases hs : store id with
  -- Absent contradicts the premise.
  | absent => exact absurd hs h
  -- Live: refused as existing.
  | live => left; simp [register, hs]
  -- Deleted: refused as deleted.
  | deleted => right; simp [register, hs]

/-! ## RED: signing the genesis after its address is taken -/

-- The bug compute.rs avoids: take the composite's address from its unsigned
-- form, then set the signature. The signature no longer reaches the ID.
def buggyId (w : World) (k : Key) (n : Nonce) (version : Nat)
    (fields : List FieldBlock) : DocId :=
  -- The address is taken before the signature is set.
  w.docIdOf (w.addrC { version := version,
                       links := fields.map (fun b => w.addrF (signField w k n b)),
                       sig := none })

-- Promise (of the bug): two signers that sign their field blocks alike but
-- their composites differently still get one ID, so the ID no longer names
-- who signed the genesis. `composite_sig_reaches_doc_id` is what this breaks.
theorem resign_after_encode_breaks_doc_id (w : World) (k₁ k₂ : Key) (n₁ n₂ : Nonce)
    (version : Nat) (fields : List FieldBlock)
    (hfields : ∀ b, signField w k₁ n₁ b = signField w k₂ n₂ b) :
    buggyId w k₁ n₁ version fields = buggyId w k₂ n₂ version fields := by
  -- The unsigned composites agree, and nothing else feeds the buggy ID.
  simp [buggyId, hfields]

-- Promise: under injective addresses, the correct builder tells those same two
-- signers apart whenever their composite signatures differ.
theorem correct_builder_tells_signers_apart (w : World)
    (hC : Injective w.addrC) (hId : Injective w.docIdOf)
    (k₁ k₂ : Key) (n₁ n₂ : Nonce) (version : Nat) (fields : List FieldBlock)
    (h : (genesis w k₁ n₁ version fields).sig ≠ (genesis w k₂ n₂ version fields).sig) :
    precommitId w k₁ n₁ version fields ≠ precommitId w k₂ n₂ version fields :=
  -- The composite's signature reaches the ID.
  composite_sig_reaches_doc_id w hC hId _ _ h

end Cid.SignedGenesis
