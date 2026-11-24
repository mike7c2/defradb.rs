# What a unique index guarantees

This chapter is the promise a client gets when two writers on one node race on a value a
unique index covers (#3265). It states today's behaviour as the guarantee; nothing here
changes which reads are validated. Each cell says where it comes from: **verified** means
a named test pins it, **by regolith contract** means it follows from regolith's documented
validation and is not tested here.

## Per operation and commit mode

Two writers each add (or update to) the same non-nil value of a field with
`@index(unique: true)`. Exactly one commits. What the other is told:

| Writer's commit mode | The loser is told | Source |
|---|---|---|
| Autocommit create (GraphQL `add_*`) | `can not index a doc's field(s) that violates unique index. DocID: …, <field>: <value>`, code `UNIQUE_CONSTRAINT_VIOLATION`; nothing stored | verified: testo `index::unique_refusal_code`, db `write::conflict_reasons::every_round_of_a_free_unique_race_has_one_winner_and_one_typed_refusal` |
| Autocommit update (GraphQL `update_*`) | `failed to update index: <same text>. IndexName: …`, code `UNIQUE_CONSTRAINT_VIOLATION`; the document keeps its old value | verified: testo `index::unique_refusal_code::an_update_refused_by_a_unique_index_carries_its_code` |
| Explicit transaction, embedded (in-process) | the second commit fails with `transaction conflict. Please retry`; nothing it wrote stays; re-running its create meets the unique code | verified: testo `txn::unique_isolation` |
| Explicit transaction, HTTP | the commit fails with `transaction conflict. Please retry` and no `code`; the re-run's mutation carries `UNIQUE_CONSTRAINT_VIOLATION` | verified for the text and the re-run's code: testo `txn::unique_isolation` (Rust HTTP client). The `409` status is `commit_error_response` in `crates/surfaces/http/src/handlers/graphql/transactions.rs`, not checked by that test; its `outcome: transaction_conflict_aborted` receipt is given only to a typed registry conflict, and whether this race gets it is not tested here |
| Explicit transaction, CLI | the same commit text; the CLI prints messages only, so no code is shown | verified for the text: testo `txn::unique_isolation::the_second_commit_of_a_unique_value_is_a_conflict` |
| REST document create (`POST /collections/{name}`) | `409` with the unique text and `"code": "UNIQUE_CONSTRAINT_VIOLATION"` | unit: `handlers::documents` and `error` tests in `crates/surfaces/http` |
| `commit` and `commit_nowait` | identical: a commit on a worker's queue uses `commit_nowait`, which validates exactly as `commit` does | by regolith contract: regolith v0.2.5 `src/transaction/nowait.rs:1-6`; `RegolithTxn::commit` |

The loser loses because the write path's check of the unique entry
(`UniqueIndex::save`, a `has` of the entry key) is a validated Ordinary read: a commit of
that key after the loser's snapshot aborts it. That validation is regolith's
(`RepeatableRead.tla`, `MC_DefraLevel_*`), assumed here. The race itself, in both modes, is
modeled by `proofs/tla/ClientUniqueAdmission.tla`; the words and codes by
`proofs/lean/UniqueRefusal.lean`.

## Scope

- **Non-nil values only.** A nil value is never checked: its entry key carries the DocID,
  so any number of documents may hold nil (verified: `nil_values_are_not_refused`).
- **No index, no refusal.** Without a unique index, a filtered read for the value is a
  plain scan that decides nothing, so both writers commit (verified:
  `without_an_index_both_commit`).
- **A filter read is not a decision.** A transaction whose only read of a value is a
  unique-index equality filter may or may not be aborted by a later writer of that value:
  whether that read is validated depends on the plan the query gets. This is outside the
  guarantee. Today the index plan reads the entry with a validated `get`, so the
  transaction loses with `transaction conflict. Please retry`;
  `a_unique_filter_read_decides_the_commit_under_todays_plan` pins that answer so a change
  to it is seen.
- **Identical documents are a DocID race.** Two creates with identical content have the
  same DocID and race on the DocID mapping before any unique entry; the tests use
  documents that differ in another field.

## Refusal kinds

| Kind | Where | Message | Code |
|---|---|---|---|
| Transaction conflict | an explicit commit | `transaction conflict. Please retry` | none; HTTP `409` |
| Unique violation | a GraphQL mutation | the index's text, naming the writer's DocID and value | `UNIQUE_CONSTRAINT_VIOLATION` |
| Unique violation | REST | the same text | HTTP `409`, `"code": "UNIQUE_CONSTRAINT_VIOLATION"` |

The code is additive: the message is unchanged, a client that matched the text still
matches, and Go and the CLI, which report no code, behave as before. Changing either the
message or the code is a breaking change for clients and goes through a requirement
(`reqs/defra/txn-isolation.md`) first.

## Boundaries

- **Peers.** Replication does not refuse: a merge that brings a second document holding a
  value resolves the collision and publishes a `UniqueIndexConflict` event after it
  commits (`crates/database/db/src/merge/merge_handler/composite.rs`). Which document
  keeps the entry is the merge's rule and is not part of this guarantee.
- **Backends.** regolith is the only storage engine, so the guarantee holds on every
  target (server, embedded, WASM) by the same validation.
