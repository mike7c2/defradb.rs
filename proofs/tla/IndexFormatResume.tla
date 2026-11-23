---------------------------- MODULE IndexFormatResume ----------------------------
\* The story: an index key is written in some key format, and a newer build may
\* write a newer one. A build reads only keys in its own format. When a store
\* written by an older build is opened, the node rebuilds every index: a clear
\* removes the old entries, then a walk writes each document's entry again.
\*
\* The rebuild runs in pages, and a crash can stop it. A reindex a schema change
\* owes the collection runs the same way, and the format rebuild takes over a
\* reindex already owed instead of starting a second one. Each page saves its
\* progress, so the next open resumes from it: the walk skips the documents the
\* saved progress says were walked.
\*
\* The trap: an older build may have stopped in the middle of its walk. Those
\* walked documents have entries in the old format. If the newer build resumes
\* that walk, it skips them, and their old entries stay. So the progress names
\* the format it was written under, and progress from another format starts over
\* with the clear.
\*
\* Who acts: the node's starts (each one a build with some format, never older
\* than the last), a schema change that owes a reindex, the open-time format
\* check, and the rebuild's pages.
\*
\* What must never happen: an open node whose index holds an entry it cannot
\* read, or lacks one for a document.
\*
\* Code it models:
\*   crates/database/db/src/definition/migration/format.rs  DB::migrate_index_format (the check at open)
\*   crates/database/db/src/definition/migration/reindex/mod.rs  DB::reindex_in_pages (takes over an owed reindex)
\*   crates/database/db/src/definition/migration/reindex/progress.rs  read (progress from another format starts over)
\*   crates/database/db/src/definition/migration/reindex/page.rs  ReindexPage::step (clear, then walk)
EXTENDS Integers, FiniteSets

CONSTANTS
  Docs,         \* the documents
  MaxFormat,    \* the newest key format a build may write; formats run 1..MaxFormat
  MaxRestarts,  \* how many starts after the first the check explores
  FormatReset   \* "On": progress from another format starts over | "Off": it resumes as is

\* The switch takes one of its values.
ASSUME FormatReset \in {"On", "Off"}
\* At least one format.
ASSUME MaxFormat \in Nat \ {0}
\* A count of starts.
ASSUME MaxRestarts \in Nat

\* The key formats a build may write.
Formats == 1..MaxFormat

VARIABLES
  fmt,       \* the format this start's build writes and reads
  phase,     \* "Check" (the format check is next), "Rebuild" (a rebuild runs) or "Open"
  stamp,     \* the format stamped on disk, written once a format rebuild ends
  index,     \* the stored entries: pairs of a document and the format of its key
  owed,      \* whether a reindex is owed (its record is durable)
  progress,  \* the saved stage of the owed reindex: "Start" (clear next) or "Cleared" (walking)
  walked,    \* the documents the saved progress says were walked
  progFmt,   \* the format the saved progress was written under
  restarts   \* starts so far after the first

vars == <<fmt, phase, stamp, index, owed, progress, walked, progFmt, restarts>>

\* The formats of a document's entries.
Entries(d) == {f \in Formats : <<d, f>> \in index}

\* Every variable stays in its range.
TypeOK ==
  /\ fmt \in Formats                                 \* a format
  /\ phase \in {"Check", "Rebuild", "Open"}          \* a phase
  /\ stamp \in Formats                               \* a format
  /\ index \subseteq Docs \X Formats                 \* entries
  /\ owed \in BOOLEAN                                \* owed or not
  /\ progress \in {"Start", "Cleared"}               \* stages
  /\ walked \subseteq Docs                           \* walked documents
  /\ progFmt \in Formats                             \* a format
  /\ restarts \in 0..MaxRestarts                     \* bounded starts

\* The first build, with the oldest format, open with every document indexed.
Init ==
  /\ fmt = 1                                         \* the oldest build
  /\ phase = "Open"                                  \* taking writes
  /\ stamp = 1                                       \* stamped with its format
  /\ index = {<<d, 1>> : d \in Docs}                 \* each document has its entry
  /\ owed = FALSE                                    \* nothing owed
  /\ progress = "Start" /\ walked = {}               \* no progress saved
  /\ progFmt = 1                                     \* unused until a page saves progress
  /\ restarts = 0                                    \* the first start

\* A schema change while open owes a reindex, under a fresh token, and runs it.
OweReindex ==
  /\ phase = "Open" /\ ~owed                         \* nothing owed yet
  /\ owed' = TRUE                                    \* the owed record is written
  /\ progress' = "Start" /\ walked' = {}             \* a fresh token: no progress of its own
  /\ phase' = "Rebuild"                              \* its pages run
  /\ UNCHANGED <<fmt, stamp, index, progFmt, restarts>>

\* The check at open. A store stamped older than this build is rebuilt; the
\* rebuild takes over a reindex already owed, its progress included. A store
\* already current only settles what is owed.
CheckAtOpen ==
  /\ phase = "Check"                                 \* just started
  /\ IF stamp < fmt \/ owed
     THEN /\ owed' = TRUE                            \* a rebuild is owed
          /\ IF owed THEN UNCHANGED <<progress, walked>>   \* the owed one keeps its progress
             ELSE progress' = "Start" /\ walked' = {}       \* a new token starts clean
          /\ phase' = "Rebuild"                      \* its pages run
     ELSE /\ phase' = "Open"                         \* nothing to do
          /\ UNCHANGED <<owed, progress, walked>>
  /\ UNCHANGED <<fmt, stamp, index, progFmt, restarts>>

\* The progress a page reads: what was saved, or the start if it came from another format.
Stale == FormatReset = "On" /\ progFmt /= fmt

\* A clear page: every entry goes, in whatever format.
RebuildClear ==
  /\ phase = "Rebuild"                               \* a rebuild runs
  /\ progress = "Start" \/ Stale                     \* the clear is next
  /\ index' = {}                                     \* the entries are gone
  /\ progress' = "Cleared" /\ walked' = {}           \* the walk is next
  /\ progFmt' = fmt                                  \* saved under this build's format
  /\ UNCHANGED <<fmt, phase, stamp, owed, restarts>>

\* A walk page for d: its entry again, in this build's format. The page that
\* walks the last document clears the owed record, and the stamp follows.
RebuildWalk(d) ==
  /\ phase = "Rebuild"                               \* a rebuild runs
  /\ progress = "Cleared" /\ ~Stale                  \* walking, under this build's format
  /\ d \notin walked                                 \* not walked yet
  /\ index' = index \cup {<<d, fmt>>}                \* its entry in this format
  /\ IF walked \cup {d} = Docs                       \* the last document
     THEN /\ owed' = FALSE /\ phase' = "Open"        \* the rebuild is done
          /\ stamp' = fmt                            \* the store is current
          /\ progress' = "Start" /\ walked' = {}     \* the progress goes with the owed record
     ELSE /\ walked' = walked \cup {d}               \* saved with the page
          /\ UNCHANGED <<owed, phase, stamp, progress>>
  /\ UNCHANGED <<fmt, progFmt, restarts>>

\* A crash or a stop, then a start of the same build or a newer one. What was
\* durable stays: the entries, the stamp, the owed record and its progress.
Restart(f) ==
  /\ restarts < MaxRestarts                          \* bounded starts
  /\ f >= fmt                                        \* never an older build
  /\ restarts' = restarts + 1                        \* one more start
  /\ fmt' = f                                        \* this start's build
  /\ phase' = "Check"                                \* the check goes first
  /\ UNCHANGED <<stamp, index, owed, progress, walked, progFmt>>

\* Any one step happens next.
Next ==
  \/ OweReindex \/ CheckAtOpen \/ RebuildClear
  \/ \E d \in Docs : RebuildWalk(d)
  \/ \E f \in Formats : Restart(f)

\* Start in Init and take any steps.
Spec == Init /\ [][Next]_vars

\* Every value stays in its range.
INV_TypeOK == TypeOK

\* An open node's index has, for each document, exactly one entry, in the
\* format this build reads.
INV_OpenReadable ==
  phase = "Open" => \A d \in Docs : Entries(d) = {fmt}
====
