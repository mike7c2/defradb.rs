# Prepared external inputs across write retries

`crates/engine/query/src/prepared.rs` keys resolved provider answers by the exact
request context. `crates/engine/query/src/runner/prepare.rs` (`prepare_mutations`)
asks the providers once before the first attempt, through
`crates/engine/query/src/runner/mutation.rs` (`authorize_mutation`), and keeps
each answer in the memo under the key an attempt looks up: ACP decisions and
write validators' answers alike. `crates/database/db/src/write/autocommit/{create,update,delete}.rs`
(with the create pipeline in `crates/database/db/src/write/create.rs`) turns
`NeedsInput` into an attempt abort, resolves it after the engine releases
the transaction, then re-enters. `Store::transact_retry` retries commit
conflicts inside the attempt owner. The model checks that a conflict retains a
matching memo and that a changed context requires a new answer. RED variants
clear the memo on conflict, permit the provider action inside the active
attempt, or drop the answer preparation got (the write validator before it was
kept), which asks the provider the same question again.

This is a control-flow abstraction. It does not prove provider response
correctness, signature validity, KMS policy, or every external check callsite.
Those remain implementation audit and focused-test obligations.
