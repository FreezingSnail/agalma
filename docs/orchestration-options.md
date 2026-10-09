# Local orchestration options

Research date: 2026-10-09. Scope: a local macOS tool, native Rust conductor,
serial execution first, durable phase checkpoints, approval waits, and safe
recovery of agent subprocesses and Git changes. Temporal is deferred by decision.

## Current decision

Build an internal native Rust/SQLite execution kernel, informed by Temporal's
durable history and workflow/activity separation. Stand up a minimal serial
executor first; use it to run the [kernel bootstrap project](durable-execution-bootstrap.md)
after M1. External library adoption is superseded by this decision. The research
below remains available if the scope or constraints change.

## Earlier recommendation (superseded)

Use an explicit Rust task/phase state machine persisted in the existing SQLite
ledger. Evaluate **Apalis with a SQLite backend** as its embedded worker layer in
S0. This is the leading candidate, not an adopted implementation dependency.
There is no additional server to operate. bd remains the planning backlog;
Apalis jobs represent executable phase/operation deliveries, not a second backlog.

This recommendation is based on deployment and integration fit, not measured
memory or CPU performance. Apalis is a job processor: phase checkpointing,
approval records, budgets, fencing, and external side-effect reconciliation remain
Agalma responsibilities. Its documented sequential workflow can retry from the
beginning, so wrapping the whole factory loop in one job is insufficient.
[Apalis SQLite documentation](https://github.com/apalis-dev/apalis-sqlite)

## Shortlist

| Option | Deployment | Fit and limitations |
|---|---|---|
| Apalis + SQLite | Embedded native Rust library; local SQLite file | Persistent jobs, retries, delayed scheduling, worker heartbeats/orphan recovery. Best fit for adding worker machinery to a bounded state machine. Does not establish phase-level recovery or safe external effects by itself. |
| Restate | One additional native server with embedded storage | Full durable steps, timers, and external-event/approval coordination; Rust SDK and macOS binaries. Stronger orchestration option if a separate runtime is acceptable. Adds deployment registration, journal/version compatibility, and another store to manage. |
| durare | Embedded Rust library; SQLite backend available | DBOS-compatible checkpointed execution without a server. Community-maintained, distinct from the official DBOS Rust SDK. Worth a prototype if full replay is required; not selected without auditing recovery and compatibility contracts. |
| Obelisk | One additional server with SQLite; Rust workflows compiled to WASM | Durable execution and timers without PostgreSQL. WASM/WIT components add an integration boundary around native process supervision, Unix sockets, and Git operations; weaker fit for the current conductor. |

Sources: [Apalis](https://github.com/apalis-dev/apalis-sqlite),
[Restate deployment](https://docs.restate.dev/server/overview),
[Restate Rust SDK](https://docs.rs/restate-sdk/latest/restate_sdk/),
[Restate installation](https://docs.restate.dev/installation),
[durare](https://github.com/SamuelXing/durare),
[Obelisk Rust/WASM guide](https://obeli.sk/docs/latest/wasm/getting-started/).

### Apalis version selection

The newer standalone `apalis-sqlite` backend currently publishes a 1.0 release
candidate API. The older `apalis-sql` 0.7.4 release also provides SQLite. Select a
compatible core/backend pair deliberately in S0, pin versions and migrations,
and verify contracts against that pair. Do not assume examples from both release
lines share the same API or recovery behavior.
[Standalone backend](https://docs.rs/apalis-sqlite/latest/apalis_sqlite/),
[0.7.4 backend](https://docs.rs/crate/apalis-sql/0.7.4)

### Restate footprint

Restate supports single-node production workloads with persistent local disk
and no external database service. Its embedded state store includes RocksDB.
That reduces deployment dependencies, but a single binary does not establish a
small resident memory footprint. Its documented memory budgets include 2 GiB
for RocksDB and 1 GiB for the query engine; these are configurable budgets, not
measurements of idle consumption. A local spike must tune and measure it before
calling it lightweight for Agalma.
[Single-node deployment](https://docs.restate.dev/server/overview),
[Memory configuration](https://docs.restate.dev/server/memory)

### DBOS distinction

The official DBOS Rust implementation currently documents PostgreSQL persistence;
it does not remove the database service from this design. SQLite support in other
DBOS language SDKs or community Rust implementations is not evidence of that
support in the official Rust SDK.
[Official Rust README](https://github.com/dbos-inc/dbos-transact-rust),
[Community implementation](https://github.com/SamuelXing/durare)

## Evaluation criteria retained for the internal kernel

The internal kernel must demonstrate:

1. File-backed persistence and checkpoint recovery after conductor restart.
2. Durable dispatch intent: a crash between phase transition and enqueue cannot
   lose work; duplicate deliveries cannot repeat a completed operation.
3. Persisted retries/deadlines and approval waits without holding an agent session.
4. Reconciliation of orphaned workers before a recovered job performs effects;
   stale lease generations cannot advance an execution or integrate code.
5. A crash after Git ref update but before completion recording is reconciled
   without another merge; uncertain effects park instead of replaying blindly.
6. Explicit cancellation and process-tree cleanup, including descendants.
7. Compatible checkpoint/schema upgrades and database backup/restore procedures.
8. Acceptable measured idle memory/CPU and dependency footprint on macOS, with
   one execution worker and no optional dashboard or external queue service.

Apply these checks to the seed executor and bootstrap increments according to
their scope. External candidates can be reconsidered if future requirements
justify adoption; Temporal deployment remains deferred.

SQLite storage follows its [WAL documentation](https://sqlite.org/wal.html),
[synchronous setting](https://sqlite.org/pragma.html#pragma_synchronous), and
[backup API](https://sqlite.org/backup.html). Local durability does not replace
backups or reconciliation with Git and bd after restoration.
