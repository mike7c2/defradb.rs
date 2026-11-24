---- MODULE RetryPreparedInputs ----
\* The story, in plain words.
\*
\* A client sends a write. Some of what the write needs comes from a provider
\* outside the database: an ACP decision, a write validator's answer, a KMS
\* key, an embedding. Asking a provider is slow and may have effects of its
\* own, so it is never done inside a database transaction. The request keeps a
\* memo of the answers it got, keyed by the exact question asked
\* (PreparedInputMemo in crates/engine/query/src/prepared.rs).
\*
\* First the request is prepared, outside any transaction
\* (QueryRunner::prepare_mutations in crates/engine/query/src/runner/prepare.rs,
\* which calls QueryRunner::authorize_mutation in
\* crates/engine/query/src/runner/mutation.rs): it asks the providers what it
\* can and keeps their answers in the memo. Then it runs attempts. An attempt
\* that needs an answer the memo does not hold ends at once with NeedsInput
\* (crates/engine/query/src/error.rs); the answer is fetched outside the
\* attempt and the request tries again. An attempt can also lose a commit
\* conflict and run again. Something the question depends on (a schema, a
\* head) can change between attempts, which makes it a new question.
\*
\* What must never happen:
\*   1. a provider is asked the same question twice in one request -
\*      INV_ProviderOncePerContext;
\*   2. a provider is asked while a transaction attempt is open -
\*      INV_NoProviderInsideAttempt;
\*   3. a write commits other than through an attempt that held its answer -
\*      INV_CommitHasExactPreparedInput.
\*
\* The guards, and the Rust each models:
\*   KeepPrepared        an answer got while preparing is kept in the memo
\*                       (authorize_mutation records a write validator's
\*                       answer as it records an ACP decision); FALSE is the
\*                       code before, which asked the write validator while
\*                       preparing and kept nothing, so the first attempt
\*                       ended asking the same question again;
\*   KeepMemoOnConflict  a commit conflict keeps the memo for the next attempt
\*                       (the memo belongs to the request, not the attempt);
\*   ResolveOutside      an answer is fetched only between attempts
\*                       (crates/database/db/src/write/autocommit/{create,update,delete}.rs).
\* This is the control protocol only: it does not model what a provider
\* answers, nor every place a provider is called.
EXTENDS Naturals

CONSTANT KeepPrepared,        \* TRUE: preparing keeps its answers in the memo
         KeepMemoOnConflict,  \* TRUE: a commit conflict keeps the memo
         ResolveOutside       \* TRUE: providers are asked only outside an attempt
ASSUME KeepPrepared \in BOOLEAN        \* each guard is on or off
ASSUME KeepMemoOnConflict \in BOOLEAN  \* each guard is on or off
ASSUME ResolveOutside \in BOOLEAN      \* each guard is on or off

VARIABLES prepared,          \* the request has been prepared
          active,            \* an attempt (an open transaction) is running
          context,           \* what the question depends on: "c0", then maybe "c1"
          memoContext,       \* which question the memo holds an answer to, or "none"
          providerCalls,     \* how many times a provider was asked
          networkInAttempt,  \* how many of those asks happened inside an attempt
          commits,           \* how many attempts committed
          attempts           \* how many attempts started
vars == <<prepared, active, context, memoContext, providerCalls, networkInAttempt, commits, attempts>>  \* every variable

\* At the start nothing has happened: not prepared, no attempt, an empty memo.
Init == /\ prepared = FALSE        \* not prepared yet
        /\ active = FALSE          \* no attempt open
        /\ context = "c0"          \* the first version of the question
        /\ memoContext = "none"    \* no answer in the memo
        /\ providerCalls = 0       \* nobody asked
        /\ networkInAttempt = 0    \* nobody asked inside an attempt
        /\ commits = 0             \* nothing committed
        /\ attempts = 0            \* no attempt started

\* The request is prepared, outside any attempt: the provider is asked once.
Prepare == /\ ~prepared                                    \* once per request
           /\ ~active                                      \* never inside an attempt
           /\ prepared' = TRUE                             \* now prepared
           /\ providerCalls' = providerCalls + 1           \* the provider was asked
           /\ memoContext' = IF KeepPrepared THEN context ELSE memoContext  \* MUTATION keep_prepared: the answer is dropped
           /\ UNCHANGED <<active, context, networkInAttempt, commits, attempts>>  \* the rest stays

\* An attempt starts, after preparation and while none is running.
Begin == /\ prepared                                       \* attempts follow preparation
         /\ ~active                                        \* one attempt at a time
         /\ attempts < 3                                   \* a bound to keep the run finite
         /\ active' = TRUE                                 \* the attempt is open
         /\ attempts' = attempts + 1                       \* one more attempt
         /\ UNCHANGED <<prepared, context, memoContext, providerCalls, networkInAttempt, commits>>  \* the rest stays

\* The attempt needs an answer the memo does not hold for this question, so it ends (NeedsInput).
NeedInput == /\ active                                     \* inside an attempt
             /\ memoContext # context                      \* the memo lacks this answer
             /\ active' = FALSE                            \* the attempt ends
             /\ UNCHANGED <<prepared, context, memoContext, providerCalls, networkInAttempt, commits, attempts>>  \* the rest stays

\* The missing answer is fetched and kept; the GREEN rule allows this only between attempts.
Resolve == /\ prepared                                     \* after preparation, which asked first
           /\ memoContext # context                        \* the memo lacks this answer
           /\ providerCalls < 3                            \* a bound to keep the run finite
           /\ (ResolveOutside => ~active)                  \* MUTATION resolve_outside: asked inside an attempt
           /\ memoContext' = context                       \* the answer is kept
           /\ providerCalls' = providerCalls + 1           \* the provider was asked
           /\ networkInAttempt' = networkInAttempt + IF active THEN 1 ELSE 0  \* counted if inside an attempt
           /\ UNCHANGED <<prepared, active, context, commits, attempts>>  \* the rest stays

\* The attempt commits; it can only when the memo holds the answer to this question.
Commit == /\ active                                        \* inside an attempt
          /\ memoContext = context                         \* the answer is there
          /\ commits < 2                                   \* a bound to keep the run finite
          /\ active' = FALSE                               \* the attempt ends
          /\ commits' = commits + 1                        \* it committed
          /\ UNCHANGED <<prepared, context, memoContext, providerCalls, networkInAttempt, attempts>>  \* the rest stays

\* The attempt loses a commit conflict and ends; the GREEN rule keeps the memo.
Conflict == /\ active                                      \* inside an attempt
            /\ active' = FALSE                             \* the attempt ends
            /\ memoContext' = IF KeepMemoOnConflict THEN memoContext ELSE "none"  \* MUTATION keep_memo: the memo is cleared
            /\ UNCHANGED <<prepared, context, providerCalls, networkInAttempt, commits, attempts>>  \* the rest stays

\* Between attempts, something the question depends on changes, so it becomes a new question.
ChangeContext == /\ ~active                                \* between attempts
                 /\ context = "c0"                         \* once
                 /\ context' = "c1"                        \* the new question
                 /\ UNCHANGED <<prepared, active, memoContext, providerCalls, networkInAttempt, commits, attempts>>  \* the old answer stays, under the old question

Next == Prepare \/ Begin \/ NeedInput \/ Resolve \/ Commit \/ Conflict \/ ChangeContext  \* any step, in any order
Spec == Init /\ [][Next]_vars                              \* every behaviour

\* Every variable holds a value of the right kind.
TypeOK == /\ prepared \in BOOLEAN
          /\ active \in BOOLEAN
          /\ context \in {"c0", "c1"}
          /\ memoContext \in {"none", "c0", "c1"}
          /\ providerCalls \in Nat
          /\ networkInAttempt \in Nat
          /\ commits \in Nat
          /\ attempts \in Nat

\* A provider is asked once per question: once for "c0", and once more only after the question changed.
INV_ProviderOncePerContext == providerCalls <= IF context = "c0" THEN 1 ELSE 2
\* No provider is asked while a transaction attempt is open.
INV_NoProviderInsideAttempt == networkInAttempt = 0
\* Every commit came from an attempt.
INV_CommitHasExactPreparedInput == commits <= attempts
====
