# Memory enablement

`memory_enabled` historically applied at daemon restart. Issue #752 adds live OFF
behavior for an already-running worker; the previous restart behavior was the
published contract. The source described here awaits shared QA and independent
review before acceptance.

OFF stops new automatic sync, indexing and observation extraction. Producers
check enablement and reserve capacity before loading or cloning extraction input.
At most two extraction payloads can be prepared, queued or executing. Saturation
skips this best-effort extraction before building a payload; send tasks never wait
with heavy transcripts. A second enablement check covers lifecycle insertion.

Prepared and queued payloads are registered separately from command delivery.
A worker supervisor checks OFF every 50 ms and drops these payloads, even when the
serial worker is waiting on another operation. Active extraction keeps its slot
until cleanup and permit settlement finish. Completed transcripts are released
before observation embedding.

Memory-owned operations carry an optional cancellation context. The worker installs
it around sync, search and extraction; shared embedding, observation and LLM callers
without that context retain their existing behavior. It is not a global model or
Codegraph kill switch, and does not change the separate PauseBackground policy.

Each model/batch admission checks the context. Waiting admission can be cancelled
before its commit; a permit that was concurrently admitted still gets settled.
In-flight HTTP waits observe OFF at 50 ms polling boundaries and drop the local
request. CLI fallback receives cancellation and finishes its existing kill/reap
cleanup before settlement. Cancellation stays latched for that operation, so it
cannot admit another embedding batch or model fallback after observing OFF.
Settlement retains the existing terminal error classification (`cancelled`).
Remote providers may continue work already accepted despite local HTTP cancellation.

Sync checks between files and sessions and before transcript hydration. Interrupted
full reindex removes its temporary database and retains the previous index;
file-change notifications preserve dirty state for retry. Synchronous file, CPU
and SQLite sections finish at their cooperative boundaries; the polling interval
is not a hard whole-operation or shutdown deadline. Durable settlement and existing
transport cleanup are awaited rather than abandoned to meet a timeout.

Status, file reads, observation reads, and keyword memory search remain available
on an existing worker while OFF. Search uses the retained index without starting
query embeddings. Archive projection commands keep their exact durable protocol:
a delivered projection returns its existing acknowledgement; a disabled or cancelled
sync returns an error without marking an undelivered projection complete. It remains
retryable. A successfully synchronized effect may finish its acknowledgement.

Shutdown signals cancellation outside the command queue, discards queued payloads,
closes command admission, preserves projection replies and waits for tracked active
extraction cleanup/settlement. Production handle shutdown returns after this work
finishes. No active pipeline is aborted before its settlement path returns.

Turning ON after startup without a memory worker still requires a daemon restart.
An existing worker can resume on its next sync. The TUI keeps the conservative
restart apply class and explicitly explains this asymmetric behavior.

The deterministic `live_memory_off` regressions are source-only until the manager's
shared QA run. This change does not claim to solve issue #751.
