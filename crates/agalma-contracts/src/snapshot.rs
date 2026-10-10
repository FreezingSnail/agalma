//! Canonical durable-state snapshot (`LedgerApi::export_state` / `import_state`).
//!
//! `docs/b0-kernel.md` §"Ledger schema v3" and
//! `docs/durable-execution-bootstrap.md` §"Incremental work" require a
//! versioned export/import contract so a compatible execution implementation can
//! replace the running one without losing canonical ids, receipts, or pending
//! work. The snapshot is a single canonical JSON document: history +
//! projection + receipts + dispatch intents + timers + directives + bindings +
//! kernel contract versions.
//!
//! The document carries an explicit `format` header. [`StateSnapshot::
//! check_compatible`] rejects a format the running build does not understand
//! and a physical `schema_version` that differs from the target, rather than
//! partially applying state. Serialization is canonical: `BTreeMap`s and the
//! ordered vectors make the JSON stable for a given state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::binding::BindingRecord;
use crate::error::ContractError;
use crate::ledger::{
    DirectiveRow, DispatchIntent, ExecutionEvent, ExecutionRecord, OperationReceipt, TimerRecord,
};

/// Current snapshot format header. Bump only with a compatible reader.
pub const SNAPSHOT_FORMAT: &str = "agalma-state/v1";

/// A complete, versioned durable-state snapshot.
///
/// The `format` header and `schema_version` gate import; the kernel's persisted
/// contract versions (`exec_def_version`, `kernel_version`) are carried so a
/// candidate can decide compatibility before it touches work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Snapshot format header ([`SNAPSHOT_FORMAT`]).
    pub format: String,
    /// Physical ledger schema version the snapshot was exported from.
    pub schema_version: u32,
    /// Kernel persisted `exec_def_version` at export time (may be empty when
    /// unset).
    pub exec_def_version: String,
    /// Kernel persisted `kernel_version` at export time (may be empty).
    pub kernel_version: String,
    /// Kill latch state.
    pub kill_latch: bool,
    /// Every `kernel_meta` row (worker records, migration markers, versions).
    pub kernel_meta: BTreeMap<String, String>,
    /// Per-task lease generations (fencing counters).
    pub lease_generations: BTreeMap<String, u32>,
    /// Execution projection rows, ordered by execution id.
    pub executions: Vec<ExecutionRecord>,
    /// Every recorded history event, ordered by `(execution_id, sequence)`.
    pub events: Vec<ExecutionEvent>,
    /// Every recorded operation receipt, ordered by operation id.
    pub receipts: Vec<OperationReceipt>,
    /// Every dispatch intent (consumed and unconsumed), ordered by id.
    pub intents: Vec<DispatchIntent>,
    /// Every timer row, ordered by timer id.
    pub timers: Vec<TimerRecord>,
    /// Every directive row, ordered by directive id.
    pub directives: Vec<DirectiveRow>,
    /// Every component binding record, ordered by binding id.
    pub bindings: Vec<BindingRecord>,
}

impl StateSnapshot {
    /// Whether this snapshot's format header and schema match `expected_schema`.
    ///
    /// Returns the refusal reason when incompatible. Import calls this before
    /// mutating anything: a snapshot the running build does not understand is
    /// never partially applied.
    pub fn check_compatible(&self, expected_schema: u32) -> Result<(), String> {
        if self.format != SNAPSHOT_FORMAT {
            return Err(format!(
                "snapshot format mismatch: recorded={:?} expected={:?}; refusing import",
                self.format, SNAPSHOT_FORMAT
            ));
        }
        if self.schema_version != expected_schema {
            return Err(format!(
                "snapshot schema mismatch: recorded={} expected={expected_schema}; refusing import",
                self.schema_version
            ));
        }
        Ok(())
    }

    /// Canonical JSON serialization of this snapshot.
    pub fn to_canonical_json(&self) -> Result<String, ContractError> {
        serde_json::to_string(self)
            .map_err(|e| ContractError::KnownFailure(format!("snapshot json: {e}")))
    }

    /// Parse a canonical JSON snapshot. Does not check compatibility.
    pub fn from_json(text: &str) -> Result<Self, ContractError> {
        serde_json::from_str(text)
            .map_err(|e| ContractError::KnownFailure(format!("snapshot json: {e}")))
    }

    /// Number of unconsumed dispatch intents (pending work) in the snapshot.
    pub fn pending_work(&self) -> usize {
        self.intents
            .iter()
            .filter(|intent| !intent.consumed)
            .count()
    }
}
