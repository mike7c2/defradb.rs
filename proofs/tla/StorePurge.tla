---- MODULE StorePurge ----
\* The story, in plain words.
\*
\* A client asks a running node to purge: forget everything it stored and
\* start again with the same options, as Go's PurgeAndRestart does. The node
\* answers the request at once, then a process outside the request tears the
\* node down, wipes its store and starts it again.
\*
\* Who acts:
\*   * a client, which sends up to two purge requests while the node serves;
\*   * the purge handler, which answers each request and, through a
\*     once-only gate, starts at most one teardown per run of the node;
\*   * the teardown: drain the requests, stop the embedded Vera node, close
\*     the database, then wipe and start the node again.
\*
\* The teardown takes one of two paths:
\*   * Embedded: defra-node's Node::shutdown_and_wipe, whose shutdown (the
\*     NodeShutdown model) returns an error when the Vera node or the
\*     database did not stop, and which then wipes nothing; it wipes only
\*     once it has dropped the node, its last handle, since the closed
\*     store's directory stays locked while the node lives;
\*   * Cli: `defradb start`, whose run loop stops Vera and closes the
\*     database itself and returns Purged only when both succeeded.
\*
\* What the store holds, by key class:
\*   * Data: documents, blocks, indexes and ACP state, in the store;
\*   * StoreHeldPeerKey: a peer key kept in the store (no keyring);
\*   * KeyringHeld: the node identity and peer key in the keyring, which
\*     lives outside the store.
\* The store is on disk (Persistent) or in memory (Memory); the embedded
\* Vera node, when there is one, keeps its directories on disk either way.
\*
\* What must never happen:
\*   1. the wipe runs while the store is open or the Vera node holds its
\*      directories - INV_NoWipeWhileOpen;
\*   2. the node opens again before the wipe ran - INV_NoOpenBeforeWipe;
\*   3. anything of the old store, the store-held peer key or the old Vera
\*      directories is still there when the node serves again -
\*      INV_NothingSurvives;
\*   4. the keyring is removed - INV_KeyringKept;
\*   5. a refused purge (no handler) changes anything - INV_RefusedPurgeTouchesNothing;
\*   6. a run starts two teardowns - INV_OnePurgePerRun;
\*   7. the node starts again after a teardown or a wipe that failed -
\*      INV_NoRestartAfterFailedTeardown;
\* and what must happen: an accepted purge ends with the node serving again,
\* or with the node exited on a failure - LIVE_PurgeServesAgain.
\*
\* Each guard has a RED configuration that removes it (see each .cfg).
\*
\* Rust: crates/surfaces/http/src/handlers/utility.rs purge;
\* crates/entry-points/defra-node/src/purge.rs PurgeGate, Node::purge_as,
\* Node::shutdown_and_wipe; crates/entry-points/defra-node/src/lib.rs
\* Node::shutdown; crates/engine/storage/src/wipe.rs wipe;
\* crates/entry-points/cli/src/commands/start/run.rs Node::run;
\* crates/entry-points/cli/src/commands/start/mod.rs StartArgs::execute.
EXTENDS Naturals

CONSTANTS
  Paths,                  \* the teardown paths a run may take: "Embedded", "Cli"
  Stores,                 \* the store kinds a run may use: "Persistent", "Memory"
  Veras,                  \* whether an embedded Vera node runs: "None", "Embedded"
  Handlers,               \* for the Embedded path, whether a purge handler is registered
  WipeAfterClose,         \* guard: the wipe waits for the database to close
  WaitVera,               \* guard: the wipe waits for the Vera node to stop
  OpenAfterWipe,          \* guard: the node opens again only after the wipe
  ShutdownOutsideRequest, \* guard: the request is answered before the teardown starts
  HandlerChecked,         \* guard: without a handler the purge is refused and nothing stops
  GateOnce,               \* guard: one teardown per run of the node
  NoRestartAfterFailedWipe, \* guard: a failed wipe ends the run instead of restarting
  KeyringExcluded,        \* guard: the keyring is never in the removal set
  MemoryRemovesOwnedDirs, \* guard: the Vera directories are removed under a memory store too
  CliChecksVeraStop,      \* guard: the CLI wipes only when the Vera node stopped
  CliChecksClose          \* guard: the CLI wipes only when the database closed

\* Every key class the store itself holds.
StoreClasses == {"Data", "StoreHeldPeerKey"}

\* The steps of one purge, in order.
Phases == {"serving", "stopping", "drained", "veraStopped", "closed", "wiping", "exited"}

\* The result of a step that can fail.
Results == {"pending", "ok", "failed"}

VARIABLES
  path,          \* the teardown path this behaviour takes
  store,         \* the store kind
  vera,          \* whether an embedded Vera node runs
  handler,       \* whether a purge handler is registered
  phase,         \* where the node is in serving, tearing down and starting again
  run,           \* which start of the node this is: 1, then 2 after a restart
  storeOpen,     \* the database is open
  veraRunning,   \* the Vera node runs and holds its directories
  storeData,     \* the key classes of the old store still present
  veraDirs,      \* the old Vera directories are still on disk
  keyring,       \* the keyring still holds the node identity and peer key
  requests,      \* how many purge requests the client sent
  req,           \* the purge request in flight: "none", "waiting" for the
                 \* handler, or "held" by a teardown running inside it
  purges,        \* how many teardowns the gate let start in this run
  accepted,      \* a purge was accepted in the first run
  refused,       \* a purge was answered with the no-handler refusal
  veraStop,      \* how the Vera stop went
  closeResult,   \* how the database close went
  wiped,         \* the wipe ran in this teardown
  wipeResult,    \* how the wipe went
  wipedWhileHeld,       \* ghost: a wipe ran while the store was open or Vera held its dirs
  openedBeforeWipe,     \* ghost: the node opened again before its wipe
  restartedAfterFailure \* ghost: the node started again after a failed teardown or wipe

vars == <<path, store, vera, handler, phase, run, storeOpen, veraRunning,
          storeData, veraDirs, keyring, requests, req, purges, accepted,
          refused, veraStop, closeResult, wiped, wipeResult, wipedWhileHeld,
          openedBeforeWipe, restartedAfterFailure>>

\* Every variable has the kind of value it should.
TypeOK ==
  /\ path \in Paths
  /\ store \in Stores
  /\ vera \in Veras
  /\ handler \in BOOLEAN
  /\ phase \in Phases
  /\ run \in 1..2
  /\ storeOpen \in BOOLEAN
  /\ veraRunning \in BOOLEAN
  /\ storeData \subseteq StoreClasses
  /\ veraDirs \in BOOLEAN
  /\ keyring \in BOOLEAN
  /\ requests \in 0..2
  /\ req \in {"none", "waiting", "held"}
  /\ purges \in 0..2
  /\ accepted \in BOOLEAN
  /\ refused \in BOOLEAN
  /\ veraStop \in Results
  /\ closeResult \in Results
  /\ wiped \in BOOLEAN
  /\ wipeResult \in Results
  /\ wipedWhileHeld \in BOOLEAN
  /\ openedBeforeWipe \in BOOLEAN
  /\ restartedAfterFailure \in BOOLEAN

\* A node serving its first run, with everything stored.
Init ==
  \* Each behaviour picks one path, store kind and Vera setting.
  /\ path \in Paths
  /\ store \in Stores
  /\ vera \in Veras
  \* `defradb start` always has its run loop as the handler.
  /\ handler \in IF path = "Cli" THEN {TRUE} ELSE Handlers
  /\ phase = "serving"
  /\ run = 1
  /\ storeOpen = TRUE
  \* The Vera node runs only when one is configured.
  /\ veraRunning = (vera = "Embedded")
  /\ storeData = StoreClasses
  \* Its directories exist only when it runs.
  /\ veraDirs = (vera = "Embedded")
  /\ keyring = TRUE
  /\ requests = 0
  /\ req = "none"
  /\ purges = 0
  /\ accepted = FALSE
  /\ refused = FALSE
  /\ veraStop = "pending"
  /\ closeResult = "pending"
  /\ wiped = FALSE
  /\ wipeResult = "pending"
  /\ wipedWhileHeld = FALSE
  /\ openedBeforeWipe = FALSE
  /\ restartedAfterFailure = FALSE

\* The variables the teardown steps never touch.
fixed == <<path, store, vera, handler>>

\* The client sends a purge request, in the first run, while the node still
\* accepts requests, one at a time.
Request ==
  /\ run = 1
  /\ phase \in {"serving", "stopping"}
  /\ req = "none"
  /\ requests < 2
  /\ requests' = requests + 1
  /\ req' = "waiting"
  /\ UNCHANGED <<fixed, phase, run, storeOpen, veraRunning, storeData, veraDirs,
                 keyring, purges, accepted, refused, veraStop, closeResult, wiped,
                 wipeResult, wipedWhileHeld, openedBeforeWipe, restartedAfterFailure>>

\* The handler answers a request the node has no purge handler for.
\* Guarded: it is refused with 501 and nothing stops. Red: the node stops
\* anyway, as if the refusal were only a message.
HandleNoHandler ==
  /\ req = "waiting"
  /\ ~handler
  /\ req' = "none"
  /\ refused' = TRUE
  \* Without the guard the teardown starts all the same.
  /\ phase' = IF HandlerChecked THEN phase ELSE "stopping"
  /\ purges' = IF HandlerChecked THEN purges ELSE purges + 1
  /\ UNCHANGED <<fixed, run, storeOpen, veraRunning, storeData, veraDirs, keyring,
                 requests, accepted, veraStop, closeResult, wiped, wipeResult,
                 wipedWhileHeld, openedBeforeWipe, restartedAfterFailure>>

\* The handler answers a request the gate lets through: a teardown starts.
\* Guarded: the 200 goes out first and the teardown runs outside the request.
\* Red: the teardown runs inside the request, which then waits for a drain
\* that waits for it.
HandleAccepted ==
  /\ req = "waiting"
  /\ handler
  \* The gate opens once per run; without its guard it opens every time.
  /\ purges = 0 \/ ~GateOnce
  /\ purges' = purges + 1
  /\ accepted' = TRUE
  /\ phase' = "stopping"
  \* Answered now, or held until the teardown it runs ends.
  /\ req' = IF ShutdownOutsideRequest THEN "none" ELSE "held"
  /\ UNCHANGED <<fixed, run, storeOpen, veraRunning, storeData, veraDirs, keyring,
                 requests, refused, veraStop, closeResult, wiped, wipeResult,
                 wipedWhileHeld, openedBeforeWipe, restartedAfterFailure>>

\* A request the gate already let one teardown through for: answered 200,
\* nothing more starts.
HandleRepeat ==
  /\ req = "waiting"
  /\ handler
  /\ GateOnce
  /\ purges >= 1
  /\ req' = "none"
  /\ UNCHANGED <<fixed, phase, run, storeOpen, veraRunning, storeData, veraDirs,
                 keyring, requests, purges, accepted, refused, veraStop, closeResult,
                 wiped, wipeResult, wipedWhileHeld, openedBeforeWipe,
                 restartedAfterFailure>>

\* The drain stops accepting requests and waits for the ones in flight.
Drain ==
  /\ phase = "stopping"
  \* A held request never ends: it waits for this very drain.
  /\ req = "none"
  /\ phase' = "drained"
  /\ UNCHANGED <<fixed, run, storeOpen, veraRunning, storeData, veraDirs, keyring,
                 requests, req, purges, accepted, refused, veraStop,
                 closeResult, wiped, wipeResult, wipedWhileHeld, openedBeforeWipe,
                 restartedAfterFailure>>

\* The Vera node is told to stop. It stops, or misses its deadline and keeps
\* holding its directories.
StopVera ==
  /\ phase = "drained"
  /\ veraStop' \in IF vera = "Embedded" THEN {"ok", "failed"} ELSE {"ok"}
  \* Only a stop that succeeded frees the directories.
  /\ veraRunning' = (veraRunning /\ veraStop' = "failed")
  /\ phase' = "veraStopped"
  /\ UNCHANGED <<fixed, run, storeOpen, storeData, veraDirs, keyring, requests,
                 req, purges, accepted, refused, closeResult, wiped,
                 wipeResult, wipedWhileHeld, openedBeforeWipe, restartedAfterFailure>>

\* The database closes, or fails to and stays open.
Close ==
  /\ phase = "veraStopped"
  /\ closeResult' \in {"ok", "failed"}
  /\ storeOpen' = (closeResult' = "failed")
  \* A memory store's contents go with the closed process memory.
  /\ storeData' = IF store = "Memory" /\ closeResult' = "ok" THEN {} ELSE storeData
  /\ phase' = "closed"
  /\ UNCHANGED <<fixed, run, veraRunning, veraDirs, keyring, requests, req,
                 purges, accepted, refused, veraStop, wiped, wipeResult,
                 wipedWhileHeld, openedBeforeWipe, restartedAfterFailure>>

\* Whether this path treats the teardown as failed. The Embedded path's
\* shutdown returns both errors; the CLI checks each under its guard.
TeardownFails ==
  \/ veraStop = "failed" /\ (path = "Embedded" \/ CliChecksVeraStop)
  \/ closeResult = "failed" /\ (path = "Embedded" \/ CliChecksClose)

\* Whether the teardown really failed, whatever the code checked.
TeardownFailed == veraStop = "failed" \/ closeResult = "failed"

\* After the teardown: a failure ends the run with its error; otherwise the
\* wipe comes next.
Decide ==
  /\ phase = "closed"
  /\ phase' = IF TeardownFails THEN "exited" ELSE "wiping"
  /\ UNCHANGED <<fixed, run, storeOpen, veraRunning, storeData, veraDirs, keyring,
                 requests, req, purges, accepted, refused, veraStop,
                 closeResult, wiped, wipeResult, wipedWhileHeld, openedBeforeWipe,
                 restartedAfterFailure>>

\* Where the wipe may run: after the teardown; before the close without its
\* guard; before the Vera stop without that guard.
WipeAllowed ==
  \/ phase = "wiping"
  \/ phase = "veraStopped" /\ ~WipeAfterClose
  \/ phase = "drained" /\ ~WaitVera

\* The wipe: drop every key of a persistent store, then remove the owned
\* directories. It succeeds, or fails part way and leaves things as they were.
Wipe ==
  /\ ~wiped
  /\ WipeAllowed
  /\ wiped' = TRUE
  /\ wipeResult' \in {"ok", "failed"}
  \* A memory store has nothing on disk to drop.
  /\ storeData' = IF wipeResult' = "ok" /\ store = "Persistent" THEN {} ELSE storeData
  \* The Vera directories are owned under either store kind; without its
  \* guard a memory store skips them.
  /\ veraDirs' = IF wipeResult' = "ok" /\ (store = "Persistent" \/ MemoryRemovesOwnedDirs)
                 THEN FALSE ELSE veraDirs
  \* The keyring is never in the removal set; without the guard it is.
  /\ keyring' = IF wipeResult' = "ok" /\ ~KeyringExcluded THEN FALSE ELSE keyring
  /\ wipedWhileHeld' = (wipedWhileHeld \/ storeOpen \/ veraRunning)
  /\ UNCHANGED <<fixed, phase, run, storeOpen, veraRunning, requests, req,
                 purges, accepted, refused, veraStop, closeResult, openedBeforeWipe,
                 restartedAfterFailure>>

\* A failed wipe ends the run with its error.
WipeFailedExit ==
  /\ phase = "wiping"
  /\ wiped
  /\ wipeResult = "failed"
  /\ NoRestartAfterFailedWipe
  /\ phase' = "exited"
  /\ UNCHANGED <<fixed, run, storeOpen, veraRunning, storeData, veraDirs, keyring,
                 requests, req, purges, accepted, refused, veraStop,
                 closeResult, wiped, wipeResult, wipedWhileHeld, openedBeforeWipe,
                 restartedAfterFailure>>

\* The node starts again with the same options: a new run, the store and the
\* Vera node open, the gate closed again. Guarded: only after a wipe that
\* succeeded. Red: before the wipe, or after a failed one.
Open ==
  /\ phase = "wiping"
  /\ wiped \/ ~OpenAfterWipe
  /\ (wiped => (wipeResult = "ok" \/ ~NoRestartAfterFailedWipe))
  /\ phase' = "serving"
  /\ run' = 2
  /\ storeOpen' = TRUE
  /\ veraRunning' = (vera = "Embedded")
  /\ purges' = 0
  /\ openedBeforeWipe' = (openedBeforeWipe \/ ~wiped)
  /\ restartedAfterFailure' = (restartedAfterFailure \/ TeardownFailed \/ wipeResult = "failed")
  /\ UNCHANGED <<fixed, storeData, veraDirs, keyring, requests, req, accepted,
                 refused, veraStop, closeResult, wiped, wipeResult, wipedWhileHeld>>

Next ==
  \/ Request
  \/ HandleNoHandler
  \/ HandleAccepted
  \/ HandleRepeat
  \/ Drain
  \/ StopVera
  \/ Close
  \/ Decide
  \/ Wipe
  \/ WipeFailedExit
  \/ Open

\* Every step that can be taken is eventually taken.
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

\* The wipe never runs while the store is open or the Vera node holds its
\* directories.
INV_NoWipeWhileOpen == ~wipedWhileHeld

\* The node never opens again before its wipe ran.
INV_NoOpenBeforeWipe == ~openedBeforeWipe

\* Once the node serves again, nothing of the old store, its peer key or the
\* old Vera directories is left.
INV_NothingSurvives == (run = 2) => (storeData = {} /\ ~veraDirs)

\* The keyring always keeps the node identity and peer key.
INV_KeyringKept == keyring

\* A refused purge leaves the node serving with everything it stored.
INV_RefusedPurgeTouchesNothing ==
  refused => (phase = "serving" /\ storeOpen /\ storeData = StoreClasses /\ keyring)

\* One teardown per run of the node.
INV_OnePurgePerRun == purges <= 1

\* The node never starts again after a teardown or a wipe that failed.
INV_NoRestartAfterFailedTeardown == ~restartedAfterFailure

\* An accepted purge ends with the node serving again, or exited on an error.
LIVE_PurgeServesAgain == accepted ~> (run = 2 \/ phase = "exited")
====
