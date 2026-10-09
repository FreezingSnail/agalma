//! Agalma ledger service.
//!
//! Owning wave: **M0.2** (`agalma-4ui.4`). Implements `agalma_contracts::LedgerApi`
//! with SQLite via `rusqlite` (bundled). SQL, transactions, WAL, and the physical
//! schema stay private to this crate.
//!
//! Durability follows the S0c handoff (`docs/spikes/s0c-execution.md`): WAL with
//! `synchronous=FULL`, `busy_timeout=5000`, foreign keys on; every logical
//! transition commits the execution state, its event, any operation receipts and
//! dispatch intents in ONE transaction. No external I/O runs inside a
//! transaction. `schema_version` mismatch refuses/parks and never auto-migrates.
//!
//! ## Schema v2
//!
//! Schema v2 adds the additive `decisions`, `decision_outcomes`, and `digests`
//! tables. `open()` migrates v1 → v2 only when every execution is terminal;
//! with any in-flight execution it returns [`ContractError::KnownFailure`] and
//! leaves the database untouched (no in-flight migration). A stored version
//! that is neither 1 nor 2 is never migrated; use fails with a schema mismatch.
//!
//! ## Sequence assignment
//!
//! `execution_events.sequence` is owned by the ledger: any sequence supplied in
//! a [`CommitBatch`] event is ignored and the ledger assigns the next strictly
//! monotonic value per execution. Read events back through
//! [`LedgerApi::events`] to observe the recorded sequence.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agalma_contracts::{
    BindingRecord, BindingState, CommitBatch, CommitReceipt, ContractError, DecisionOutcome,
    DecisionRequest, DigestRecord, DispatchIntent, ExecutionEvent, ExecutionId, ExecutionPhase,
    ExecutionRecord, ExecutionState, LedgerApi, OperationId, OperationReceipt, TaskId,
};
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

#[cfg(test)]
mod tests;

/// Persisted physical schema version. Bump only with a migration.
pub const SCHEMA_VERSION: u32 = 2;

/// Physical schema v1: S0c seed plus `component_bindings`.
///
/// `IF NOT EXISTS` makes first-open idempotent; it never alters an existing
/// table, so a stored schema with a different version is left untouched.
const SCHEMA_V1_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS executions (
  execution_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL,
  generation INTEGER NOT NULL,
  phase TEXT NOT NULL,
  state TEXT NOT NULL,
  attempt INTEGER NOT NULL,
  revision INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS operations (
  operation_id TEXT PRIMARY KEY,
  execution_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  inputs_hash TEXT NOT NULL,
  result_json TEXT NOT NULL,
  completed_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS execution_events (
  execution_id TEXT NOT NULL,
  sequence INTEGER NOT NULL,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  recorded_at INTEGER NOT NULL,
  PRIMARY KEY (execution_id, sequence)
);
CREATE TABLE IF NOT EXISTS dispatch_intents (
  operation_id TEXT PRIMARY KEY,
  execution_id TEXT NOT NULL,
  payload TEXT NOT NULL,
  enqueued_at INTEGER NOT NULL,
  consumed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS kill_latch (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  active INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS component_bindings (
  binding_id TEXT PRIMARY KEY,
  component_api TEXT NOT NULL,
  api_version INTEGER NOT NULL,
  implementation TEXT NOT NULL,
  implementation_version TEXT NOT NULL,
  capabilities TEXT NOT NULL,
  required_capabilities TEXT NOT NULL,
  optional_capabilities TEXT NOT NULL,
  config_hash TEXT NOT NULL,
  generation INTEGER NOT NULL,
  state TEXT NOT NULL
);
INSERT OR IGNORE INTO kill_latch(id,active,updated_at) VALUES (1,0,0);
"#;

/// Additive schema v2: durable decisions and per-execution digests.
const SCHEMA_V2_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS decisions (
  operation_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  inputs_hash TEXT NOT NULL,
  request_json TEXT NOT NULL,
  recorded_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS decision_outcomes (
  operation_id TEXT PRIMARY KEY,
  outcome_json TEXT NOT NULL,
  recorded_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS digests (
  execution_id TEXT NOT NULL,
  version INTEGER NOT NULL,
  digest_json TEXT NOT NULL,
  artifact_ref TEXT,
  recorded_at INTEGER NOT NULL,
  PRIMARY KEY (execution_id, version)
);
"#;

/// SQLite-backed [`LedgerApi`].
pub struct SqliteLedger {
    conn: Connection,
}

impl SqliteLedger {
    /// Open (creating if absent) the ledger at `path`.
    ///
    /// Applies the durability pragmas per connection and runs first-open schema
    /// initialization. An existing database with a non-matching
    /// `schema_version` opens successfully but refuses all commits; it is never
    /// auto-migrated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ContractError> {
        let conn = Connection::open(path).map_err(db_err)?;
        configure(&conn)?;
        ensure_schema(&conn)?;
        Ok(SqliteLedger { conn })
    }

    /// Recorded `schema_version`, or `None` before first initialization.
    pub fn stored_schema_version(&self) -> Result<Option<u32>, ContractError> {
        Ok(meta_value(&self.conn, "schema_version")?.and_then(|v| v.parse().ok()))
    }

    fn ensure_compatible(&self) -> Result<(), ContractError> {
        match self.stored_schema_version()? {
            Some(v) if v == SCHEMA_VERSION => Ok(()),
            other => Err(ContractError::Conflict(format!(
                "schema version mismatch: recorded={other:?} expected={SCHEMA_VERSION}; refusing (no auto-migrate)"
            ))),
        }
    }

    #[cfg(test)]
    fn meta(&self, key: &str) -> Result<Option<String>, ContractError> {
        meta_value(&self.conn, key)
    }

    #[cfg(test)]
    fn journal_mode(&self) -> Result<String, ContractError> {
        self.conn
            .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .map_err(db_err)
    }

    #[cfg(test)]
    fn force_schema_version(&self, version: u32) -> Result<(), ContractError> {
        self.conn
            .execute(
                "UPDATE meta SET value=?1 WHERE key='schema_version'",
                params![version.to_string()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Simulate a genuine v1 database: drop the additive v2 tables and record
    /// schema version 1. Test-only; used to exercise the migration path.
    #[cfg(test)]
    fn downgrade_to_v1(&self) -> Result<(), ContractError> {
        self.conn
            .execute_batch(
                "DROP TABLE IF EXISTS decisions; \
                 DROP TABLE IF EXISTS decision_outcomes; \
                 DROP TABLE IF EXISTS digests; \
                 UPDATE meta SET value='1' WHERE key='schema_version';",
            )
            .map_err(db_err)?;
        Ok(())
    }
}

fn configure(conn: &Connection) -> Result<(), ContractError> {
    conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
        .map_err(db_err)?;
    conn.execute_batch(
        "PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
    )
    .map_err(db_err)?;
    Ok(())
}

fn ensure_schema(conn: &Connection) -> Result<(), ContractError> {
    conn.execute_batch(SCHEMA_V1_SQL).map_err(db_err)?;
    match meta_value(conn, "schema_version")? {
        None => {
            // Fresh database: create v2 and record the version.
            conn.execute_batch(SCHEMA_V2_SQL).map_err(db_err)?;
            let now = now_ms();
            let stmts: [(&str, String); 3] = [
                ("schema_version", SCHEMA_VERSION.to_string()),
                ("sqlite_version", rusqlite::version().to_string()),
                ("created_at", now.to_string()),
            ];
            for (key, value) in stmts {
                conn.execute(
                    "INSERT OR IGNORE INTO meta(key,value) VALUES(?1,?2)",
                    params![key, value],
                )
                .map_err(db_err)?;
            }
        }
        Some(raw) => {
            let version: u32 = raw.parse().unwrap_or(0);
            if version == 1 {
                // Auto-migrate v1 → v2 only when every execution is terminal.
                if !all_executions_terminal(conn)? {
                    return Err(ContractError::KnownFailure(
                        "refusing v1 -> v2 migration: in-flight executions present \
                         (no in-flight migration)"
                            .to_string(),
                    ));
                }
                conn.execute_batch(SCHEMA_V2_SQL).map_err(db_err)?;
                conn.execute(
                    "UPDATE meta SET value=?1 WHERE key='schema_version'",
                    params![SCHEMA_VERSION.to_string()],
                )
                .map_err(db_err)?;
            }
            // Version 2: tables already present. Any other version is a future
            // or unknown schema: leave it untouched; use refuses until corrected.
        }
    }
    Ok(())
}

/// True when every execution row is in a terminal state (`completed`, `failed`,
/// `parked`). An empty database is vacuously terminal. Unknown states count as
/// non-terminal so migration refuses rather than guessing.
fn all_executions_terminal(conn: &Connection) -> Result<bool, ContractError> {
    let mut stmt = conn
        .prepare("SELECT state FROM executions")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(db_err)?;
    for row in rows {
        let state = row.map_err(db_err)?;
        match state_from(&state) {
            Some(ExecutionState::Completed | ExecutionState::Failed | ExecutionState::Parked) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

impl LedgerApi for SqliteLedger {
    fn commit(&mut self, batch: CommitBatch) -> Result<CommitReceipt, ContractError> {
        self.ensure_compatible()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_err)?;

        // Reject operation-ID reuse under a different input hash; detect an
        // all-duplicate batch (duplicate delivery of a completed operation).
        let mut all_receipts_recorded = !batch.receipts.is_empty();
        for receipt in &batch.receipts {
            match read_receipt(&tx, receipt.operation_id.as_str())? {
                Some(existing) => {
                    if existing.inputs_hash != receipt.inputs_hash {
                        return Err(ContractError::Conflict(format!(
                            "operation {} reused with different inputs (existing={}, new={})",
                            receipt.operation_id, existing.inputs_hash, receipt.inputs_hash
                        )));
                    }
                }
                None => all_receipts_recorded = false,
            }
        }

        let now = now_ms();
        if all_receipts_recorded {
            // Duplicate delivery: nothing is written; the recorded receipt is
            // available via `operation_receipt`.
            let revision = current_revision(&tx, &batch)?;
            tx.commit().map_err(db_err)?;
            return Ok(CommitReceipt {
                revision,
                committed_at_unix_ms: now,
            });
        }

        // Reject dispatch-intent reuse with a different payload.
        for intent in &batch.intents {
            if let Some((payload, execution_id)) =
                read_intent_raw(&tx, intent.operation_id.as_str())?
            {
                let new_payload = serde_json::to_string(&intent.payload).map_err(json_err)?;
                if payload != new_payload || execution_id != intent.execution_id.as_str() {
                    return Err(ContractError::Conflict(format!(
                        "dispatch intent {} reused with a different payload",
                        intent.operation_id
                    )));
                }
            }
        }

        // State transition (optimistic revision guard).
        if let Some(record) = &batch.state {
            let new_revision = match read_revision(&tx, record.execution_id.as_str())? {
                Some(current) => {
                    if let Some(expected) = batch.expected_revision {
                        if expected != current {
                            return Err(ContractError::Conflict(format!(
                                "revision conflict for {}: expected {expected}, current {current}",
                                record.execution_id
                            )));
                        }
                    }
                    current + 1
                }
                None => {
                    if let Some(expected) = batch.expected_revision {
                        if expected != 0 {
                            return Err(ContractError::Conflict(format!(
                                "revision conflict for {}: expected {expected}, no existing row",
                                record.execution_id
                            )));
                        }
                    }
                    1
                }
            };
            write_state(&tx, record, new_revision)?;
        }

        // Events: ledger assigns a strictly monotonic per-execution sequence.
        for event in &batch.events {
            let next: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(sequence),0)+1 FROM execution_events WHERE execution_id=?1",
                    params![event.execution_id.as_str()],
                    |r| r.get(0),
                )
                .map_err(db_err)?;
            tx.execute(
                "INSERT INTO execution_events(execution_id,sequence,kind,payload,recorded_at) \
                 VALUES(?1,?2,?3,?4,?5)",
                params![
                    event.execution_id.as_str(),
                    next,
                    event.kind,
                    serde_json::to_string(&event.payload).map_err(json_err)?,
                    sql_u64(event.recorded_at_unix_ms),
                ],
            )
            .map_err(db_err)?;
        }

        // Dispatch intents (idempotent when the payload matches).
        for intent in &batch.intents {
            tx.execute(
                "INSERT OR IGNORE INTO dispatch_intents(operation_id,execution_id,payload,enqueued_at,consumed) \
                 VALUES(?1,?2,?3,?4,0)",
                params![
                    intent.operation_id.as_str(),
                    intent.execution_id.as_str(),
                    serde_json::to_string(&intent.payload).map_err(json_err)?,
                    sql_u64(intent.enqueued_at_unix_ms),
                ],
            )
            .map_err(db_err)?;
        }

        // Operation receipts; recording one consumes its dispatch intent.
        for receipt in &batch.receipts {
            if read_receipt(&tx, receipt.operation_id.as_str())?.is_none() {
                tx.execute(
                    "INSERT INTO operations(operation_id,execution_id,kind,inputs_hash,result_json,completed_at) \
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        receipt.operation_id.as_str(),
                        receipt.execution_id.as_str(),
                        receipt.kind,
                        receipt.inputs_hash,
                        serde_json::to_string(&receipt.result).map_err(json_err)?,
                        sql_u64(receipt.completed_at_unix_ms),
                    ],
                )
                .map_err(db_err)?;
            }
            tx.execute(
                "UPDATE dispatch_intents SET consumed=1 WHERE operation_id=?1",
                params![receipt.operation_id.as_str()],
            )
            .map_err(db_err)?;
        }

        let revision = current_revision(&tx, &batch)?;
        tx.commit().map_err(db_err)?;
        Ok(CommitReceipt {
            revision,
            committed_at_unix_ms: now,
        })
    }

    fn execution(&self, id: &ExecutionId) -> Result<Option<ExecutionRecord>, ContractError> {
        self.conn
            .query_row(
                "SELECT execution_id,task_id,generation,phase,state,attempt,revision \
                 FROM executions WHERE execution_id=?1",
                params![id.as_str()],
                execution_from_row,
            )
            .optional()
            .map_err(db_err)
    }

    fn executions(&self) -> Result<Vec<ExecutionRecord>, ContractError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT execution_id,task_id,generation,phase,state,attempt,revision \
                 FROM executions ORDER BY execution_id",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map([], execution_from_row).map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    fn operation_receipt(
        &self,
        id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError> {
        read_receipt(&self.conn, id.as_str())
    }

    fn pending_intents(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT operation_id,execution_id,payload,enqueued_at,consumed \
                 FROM dispatch_intents WHERE consumed=0 ORDER BY enqueued_at,operation_id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                let payload: String = row.get(2)?;
                Ok(DispatchIntent {
                    operation_id: OperationId::new(row.get::<_, String>(0)?),
                    execution_id: ExecutionId::new(row.get::<_, String>(1)?),
                    payload: serde_json::from_str(&payload).map_err(|e| conversion_err(2, e))?,
                    enqueued_at_unix_ms: row.get::<_, i64>(3)? as u64,
                    consumed: row.get::<_, i64>(4)? != 0,
                })
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    fn events(&self, id: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT execution_id,sequence,kind,payload,recorded_at \
                 FROM execution_events WHERE execution_id=?1 ORDER BY sequence",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![id.as_str()], |row| {
                let payload: String = row.get(3)?;
                Ok(ExecutionEvent {
                    execution_id: ExecutionId::new(row.get::<_, String>(0)?),
                    sequence: row.get::<_, i64>(1)? as u64,
                    kind: row.get(2)?,
                    payload: serde_json::from_str(&payload).map_err(|e| conversion_err(3, e))?,
                    recorded_at_unix_ms: row.get::<_, i64>(4)? as u64,
                })
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    fn kill_latch(&self) -> Result<bool, ContractError> {
        Ok(self
            .conn
            .query_row("SELECT active FROM kill_latch WHERE id=1", [], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
            .map_err(db_err)?
            .map(|v| v != 0)
            .unwrap_or(false))
    }

    fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError> {
        self.conn
            .execute(
                "UPDATE kill_latch SET active=?1, updated_at=?2 WHERE id=1",
                params![i64::from(latched), sql_u64(now_ms())],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError> {
        let key = lease_key(task);
        Ok(meta_value(&self.conn, &key)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError> {
        self.ensure_compatible()?;
        // The conductor holds the state-dir lock, so a single connection owns
        // the read-modify-write; WAL + IMMEDIATE would be the guard if not.
        let key = lease_key(task);
        let next = self
            .lease_generation(task)?
            .checked_add(1)
            .ok_or_else(|| ContractError::KnownFailure("lease generation overflow".to_string()))?;
        self.conn
            .execute(
                "INSERT INTO meta(key,value) VALUES(?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, next.to_string()],
            )
            .map_err(db_err)?;
        Ok(next)
    }

    fn upsert_binding(&mut self, binding: &BindingRecord) -> Result<(), ContractError> {
        self.conn
            .execute(
                "INSERT INTO component_bindings( \
                   binding_id,component_api,api_version,implementation,implementation_version, \
                   capabilities,required_capabilities,optional_capabilities,config_hash,generation,state \
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) \
                 ON CONFLICT(binding_id) DO UPDATE SET \
                   component_api=excluded.component_api, \
                   api_version=excluded.api_version, \
                   implementation=excluded.implementation, \
                   implementation_version=excluded.implementation_version, \
                   capabilities=excluded.capabilities, \
                   required_capabilities=excluded.required_capabilities, \
                   optional_capabilities=excluded.optional_capabilities, \
                   config_hash=excluded.config_hash, \
                   generation=excluded.generation, \
                   state=excluded.state",
                params![
                    binding.binding_id.as_str(),
                    binding.component_api,
                    i64::from(binding.api_version),
                    binding.implementation,
                    binding.implementation_version,
                    serde_json::to_string(&binding.capabilities).map_err(json_err)?,
                    serde_json::to_string(&binding.required_capabilities).map_err(json_err)?,
                    serde_json::to_string(&binding.optional_capabilities).map_err(json_err)?,
                    binding.config_hash,
                    sql_u64(binding.generation),
                    binding_state_str(binding.state),
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn bindings(&self) -> Result<Vec<BindingRecord>, ContractError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT binding_id,component_api,api_version,implementation,implementation_version, \
                 capabilities,required_capabilities,optional_capabilities,config_hash,generation,state \
                 FROM component_bindings ORDER BY binding_id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                let state: String = row.get(10)?;
                Ok(BindingRecord {
                    binding_id: agalma_contracts::BindingId::new(row.get::<_, String>(0)?),
                    component_api: row.get(1)?,
                    api_version: row.get::<_, i64>(2)? as u32,
                    implementation: row.get(3)?,
                    implementation_version: row.get(4)?,
                    capabilities: parse_set(row.get::<_, String>(5)?, 5)?,
                    required_capabilities: parse_set(row.get::<_, String>(6)?, 6)?,
                    optional_capabilities: parse_set(row.get::<_, String>(7)?, 7)?,
                    config_hash: row.get(8)?,
                    generation: row.get::<_, i64>(9)? as u64,
                    state: binding_state_from(&state).ok_or_else(|| bad_column(10, "state"))?,
                })
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    fn schema_version(&self) -> Result<u32, ContractError> {
        Ok(self.stored_schema_version()?.unwrap_or(0))
    }

    fn record_decision(&mut self, request: &DecisionRequest) -> Result<(), ContractError> {
        self.ensure_compatible()?;
        let inputs_hash = decision_inputs_hash(request)?;
        if let Some(existing) = read_decision_hash(&self.conn, request.operation_id.as_str())? {
            if existing != inputs_hash {
                return Err(ContractError::Conflict(format!(
                    "decision {} reused with different inputs (existing={existing}, new={inputs_hash})",
                    request.operation_id
                )));
            }
            return Ok(());
        }
        self.conn
            .execute(
                "INSERT INTO decisions(operation_id,kind,inputs_hash,request_json,recorded_at) \
                 VALUES(?1,?2,?3,?4,?5)",
                params![
                    request.operation_id.as_str(),
                    request.kind.as_str(),
                    inputs_hash,
                    serde_json::to_string(request).map_err(json_err)?,
                    sql_u64(now_ms()),
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn record_decision_outcome(
        &mut self,
        operation_id: &OperationId,
        outcome: &DecisionOutcome,
    ) -> Result<(), ContractError> {
        self.ensure_compatible()?;
        // A recorded outcome is terminal: a late or duplicate result never
        // supersedes it.
        if self.decision_outcome(operation_id)?.is_some() {
            return Ok(());
        }
        self.conn
            .execute(
                "INSERT INTO decision_outcomes(operation_id,outcome_json,recorded_at) \
                 VALUES(?1,?2,?3)",
                params![
                    operation_id.as_str(),
                    serde_json::to_string(outcome).map_err(json_err)?,
                    sql_u64(now_ms()),
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn decision_outcome(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<DecisionOutcome>, ContractError> {
        self.conn
            .query_row(
                "SELECT outcome_json FROM decision_outcomes WHERE operation_id=?1",
                params![operation_id.as_str()],
                |row| {
                    let raw: String = row.get(0)?;
                    serde_json::from_str(&raw).map_err(|e| conversion_err(0, e))
                },
            )
            .optional()
            .map_err(db_err)
    }

    fn record_digest(&mut self, digest: &DigestRecord) -> Result<(), ContractError> {
        self.ensure_compatible()?;
        self.conn
            .execute(
                "INSERT INTO digests(execution_id,version,digest_json,artifact_ref,recorded_at) \
                 VALUES(?1,?2,?3,?4,?5) \
                 ON CONFLICT(execution_id,version) DO UPDATE SET \
                   digest_json=excluded.digest_json, artifact_ref=excluded.artifact_ref, \
                   recorded_at=excluded.recorded_at",
                params![
                    digest.execution_id.as_str(),
                    i64::from(digest.version),
                    serde_json::to_string(digest).map_err(json_err)?,
                    digest.artifact_ref.as_ref().map(|a| a.as_str().to_string()),
                    sql_u64(digest.recorded_at_unix_ms),
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    fn digests_for_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Vec<DigestRecord>, ContractError> {
        let mut stmt = self
            .conn
            .prepare("SELECT digest_json FROM digests WHERE execution_id=?1 ORDER BY version")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![execution_id.as_str()], |row| {
                let raw: String = row.get(0)?;
                serde_json::from_str(&raw).map_err(|e| conversion_err(0, e))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }
}

fn execution_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExecutionRecord> {
    let phase: String = row.get(3)?;
    let state: String = row.get(4)?;
    Ok(ExecutionRecord {
        execution_id: ExecutionId::new(row.get::<_, String>(0)?),
        task_id: TaskId::new(row.get::<_, String>(1)?),
        generation: row.get::<_, i64>(2)? as u32,
        phase: phase_from(&phase).ok_or_else(|| bad_column(3, "phase"))?,
        state: state_from(&state).ok_or_else(|| bad_column(4, "state"))?,
        attempt: row.get::<_, i64>(5)? as u32,
        revision: row.get::<_, i64>(6)? as u64,
    })
}

fn write_state(
    tx: &Transaction<'_>,
    record: &ExecutionRecord,
    revision: u64,
) -> Result<(), ContractError> {
    tx.execute(
        "INSERT INTO executions(execution_id,task_id,generation,phase,state,attempt,revision) \
         VALUES(?1,?2,?3,?4,?5,?6,?7) \
         ON CONFLICT(execution_id) DO UPDATE SET \
           task_id=excluded.task_id, generation=excluded.generation, phase=excluded.phase, \
           state=excluded.state, attempt=excluded.attempt, revision=excluded.revision",
        params![
            record.execution_id.as_str(),
            record.task_id.as_str(),
            i64::from(record.generation),
            phase_str(record.phase),
            state_str(record.state),
            i64::from(record.attempt),
            sql_u64(revision),
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

fn read_revision(conn: &Connection, execution_id: &str) -> Result<Option<u64>, ContractError> {
    Ok(conn
        .query_row(
            "SELECT revision FROM executions WHERE execution_id=?1",
            params![execution_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(db_err)?
        .map(|v| v as u64))
}

fn read_receipt(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<OperationReceipt>, ContractError> {
    conn.query_row(
        "SELECT operation_id,execution_id,kind,inputs_hash,result_json,completed_at \
         FROM operations WHERE operation_id=?1",
        params![operation_id],
        |row| {
            let result: String = row.get(4)?;
            Ok(OperationReceipt {
                operation_id: OperationId::new(row.get::<_, String>(0)?),
                execution_id: ExecutionId::new(row.get::<_, String>(1)?),
                kind: row.get(2)?,
                inputs_hash: row.get(3)?,
                result: serde_json::from_str(&result).map_err(|e| conversion_err(4, e))?,
                completed_at_unix_ms: row.get::<_, i64>(5)? as u64,
            })
        },
    )
    .optional()
    .map_err(db_err)
}

fn read_intent_raw(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<(String, String)>, ContractError> {
    conn.query_row(
        "SELECT payload,execution_id FROM dispatch_intents WHERE operation_id=?1",
        params![operation_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(db_err)
}

fn read_decision_hash(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<String>, ContractError> {
    conn.query_row(
        "SELECT inputs_hash FROM decisions WHERE operation_id=?1",
        params![operation_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(db_err)
}

/// Deterministic content hash pinning a decision request's inputs.
///
/// `serde_json` serializes a struct in declaration order, so the canonical JSON
/// is stable for a given request value. FNV-1a is used because the pinned
/// dependency set has no hash crate; it is a conflict detector, not a security
/// primitive.
fn decision_inputs_hash(request: &DecisionRequest) -> Result<String, ContractError> {
    let bytes = serde_json::to_vec(request).map_err(json_err)?;
    Ok(format!("fnv1a64:{:016x}", fnv1a64(&bytes)))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Revision reported by a commit: the transitioned execution's revision when a
/// state row was supplied, otherwise the derived execution's current revision.
fn current_revision(tx: &Transaction<'_>, batch: &CommitBatch) -> Result<u64, ContractError> {
    if let Some(record) = &batch.state {
        return Ok(read_revision(tx, record.execution_id.as_str())?.unwrap_or(0));
    }
    match derived_execution_id(batch) {
        Some(id) => Ok(read_revision(tx, id)?.unwrap_or(0)),
        None => Ok(0),
    }
}

fn derived_execution_id(batch: &CommitBatch) -> Option<&str> {
    if let Some(record) = &batch.state {
        return Some(record.execution_id.as_str());
    }
    if let Some(event) = batch.events.first() {
        return Some(event.execution_id.as_str());
    }
    if let Some(receipt) = batch.receipts.first() {
        return Some(receipt.execution_id.as_str());
    }
    batch.intents.first().map(|i| i.execution_id.as_str())
}

/// `meta` key holding a task's current lease generation.
fn lease_key(task: &TaskId) -> String {
    format!("lease_generation:{}", task.as_str())
}

fn meta_value(conn: &Connection, key: &str) -> Result<Option<String>, ContractError> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
        r.get(0)
    })
    .optional()
    .map_err(db_err)
}

fn parse_set(raw: String, idx: usize) -> rusqlite::Result<BTreeSet<String>> {
    serde_json::from_str(&raw).map_err(|e| conversion_err(idx, e))
}

fn conversion_err(idx: usize, e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(e))
}

fn bad_column(idx: usize, name: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(idx, name.to_string(), Type::Text)
}

fn phase_str(phase: ExecutionPhase) -> &'static str {
    match phase {
        ExecutionPhase::Intake => "intake",
        ExecutionPhase::Checkout => "checkout",
        ExecutionPhase::Build => "build",
        ExecutionPhase::Verify => "verify",
        ExecutionPhase::Integrate => "integrate",
        ExecutionPhase::Done => "done",
        ExecutionPhase::Parked => "parked",
    }
}

fn phase_from(raw: &str) -> Option<ExecutionPhase> {
    Some(match raw {
        "intake" => ExecutionPhase::Intake,
        "checkout" => ExecutionPhase::Checkout,
        "build" => ExecutionPhase::Build,
        "verify" => ExecutionPhase::Verify,
        "integrate" => ExecutionPhase::Integrate,
        "done" => ExecutionPhase::Done,
        "parked" => ExecutionPhase::Parked,
        _ => return None,
    })
}

fn state_str(state: ExecutionState) -> &'static str {
    match state {
        ExecutionState::Pending => "pending",
        ExecutionState::Running => "running",
        ExecutionState::Completed => "completed",
        ExecutionState::Failed => "failed",
        ExecutionState::Parked => "parked",
    }
}

fn state_from(raw: &str) -> Option<ExecutionState> {
    Some(match raw {
        "pending" => ExecutionState::Pending,
        "running" => ExecutionState::Running,
        "completed" => ExecutionState::Completed,
        "failed" => ExecutionState::Failed,
        "parked" => ExecutionState::Parked,
        _ => return None,
    })
}

fn binding_state_str(state: BindingState) -> &'static str {
    match state {
        BindingState::Staged => "staged",
        BindingState::Active => "active",
        BindingState::Draining => "draining",
        BindingState::Retired => "retired",
    }
}

fn binding_state_from(raw: &str) -> Option<BindingState> {
    Some(match raw {
        "staged" => BindingState::Staged,
        "active" => BindingState::Active,
        "draining" => BindingState::Draining,
        "retired" => BindingState::Retired,
        _ => return None,
    })
}

fn db_err(e: rusqlite::Error) -> ContractError {
    ContractError::KnownFailure(format!("ledger: {e}"))
}

fn json_err(e: serde_json::Error) -> ContractError {
    ContractError::KnownFailure(format!("ledger json: {e}"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sql_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
