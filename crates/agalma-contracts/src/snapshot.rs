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
use crate::decision::{DecisionOutcome, DecisionRequest};
use crate::digest::FailureCode;
use crate::error::ContractError;
use crate::ids::OperationId;
use crate::ledger::{
    ComponentCommitAnchor, ComponentEventRow, ComponentOperationRow, ComponentReceiptRow,
    ComponentStateRow, ControlReceipt, DigestRecord, DirectiveRow, DispatchIntent, ExecutionEvent,
    ExecutionRecord, OperationReceipt, TimerRecord,
};

/// Current snapshot format header. Bump only with a compatible reader.
pub const SNAPSHOT_FORMAT: &str = "agalma-state/v1";

/// Complete-state snapshot format header (schema v5).
pub const COMPLETE_STATE_FORMAT: &str = "agalma-state/v2";

/// Complete-state physical schema version.
pub const COMPLETE_STATE_SCHEMA_VERSION: u32 = 5;

/// One persisted decision request row (schema v2 `decisions`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotDecisionRow {
    pub operation_id: OperationId,
    /// Must match `request.kind.as_str()`.
    pub kind: String,
    /// Preserved legacy conflict hash.
    pub inputs_hash: String,
    pub request: DecisionRequest,
    pub recorded_at_unix_ms: u64,
}

/// One persisted decision outcome row (schema v2 `decision_outcomes`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotDecisionOutcomeRow {
    pub operation_id: OperationId,
    pub outcome: DecisionOutcome,
    pub recorded_at_unix_ms: u64,
}

/// The complete durable-state snapshot (`agalma-state/v2`, schema v5).
///
/// Carries every legacy v1 section plus the previously omitted decision,
/// decision-outcome, digest, control-receipt and component sections. Unknown
/// sections are refused: a complete snapshot is validated before mutation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteStateV2 {
    /// Snapshot format header ([`COMPLETE_STATE_FORMAT`]).
    pub format: String,
    /// Physical ledger schema version the snapshot was exported from.
    pub schema_version: u32,
    pub exec_def_version: String,
    pub kernel_version: String,
    pub kill_latch: bool,
    pub kernel_meta: BTreeMap<String, String>,
    pub lease_generations: BTreeMap<String, u32>,
    pub executions: Vec<ExecutionRecord>,
    pub events: Vec<ExecutionEvent>,
    pub receipts: Vec<OperationReceipt>,
    pub intents: Vec<DispatchIntent>,
    pub timers: Vec<TimerRecord>,
    pub directives: Vec<DirectiveRow>,
    pub bindings: Vec<BindingRecord>,
    pub decisions: Vec<SnapshotDecisionRow>,
    pub decision_outcomes: Vec<SnapshotDecisionOutcomeRow>,
    pub digests: Vec<DigestRecord>,
    pub control_receipts: Vec<ControlReceipt>,
    pub component_states: Vec<ComponentStateRow>,
    pub component_events: Vec<ComponentEventRow>,
    pub component_operations: Vec<ComponentOperationRow>,
    pub component_receipts: Vec<ComponentReceiptRow>,
    pub component_commits: Vec<ComponentCommitAnchor>,
}

impl CompleteStateV2 {
    /// Gate the `format` family/version. A recognized family at the wrong
    /// version is `UnsupportedVersion`; an unrecognized family is
    /// `UnsupportedSchema`.
    pub fn version_gate(&self) -> Result<(), ContractError> {
        match parse_state_format_version(&self.format) {
            Ok(2) => Ok(()),
            Ok(other) => Err(FailureCode::UnsupportedVersion.failure(
                &self.format,
                format!("complete-state version {other} is unsupported (expected 2)"),
            )),
            Err(reason) => Err(FailureCode::UnsupportedSchema.failure(
                &self.format,
                format!("unrecognized snapshot format: {reason}"),
            )),
        }
    }

    /// Whether this snapshot's format/schema match `expected_schema`.
    ///
    /// Returns the refusal reason when incompatible; callers validate before
    /// mutating anything.
    pub fn check_compatible(&self, expected_schema: u32) -> Result<(), String> {
        self.version_gate()
            .map_err(|error| format!("{error}"))?;
        if self.schema_version != expected_schema {
            return Err(format!(
                "complete-state schema mismatch: recorded={} expected={expected_schema}; refusing import",
                self.schema_version
            ));
        }
        Ok(())
    }

    /// Canonical JSON serialization of this snapshot.
    pub fn to_canonical_json(&self) -> Result<String, ContractError> {
        serde_json::to_string(self)
            .map_err(|e| ContractError::KnownFailure(format!("complete-state json: {e}")))
    }

    /// Parse a canonical JSON snapshot. Does not check compatibility.
    pub fn from_json(text: &str) -> Result<Self, ContractError> {
        serde_json::from_str(text)
            .map_err(|e| ContractError::KnownFailure(format!("complete-state json: {e}")))
    }
}

/// Parse `agalma-state/vN` into `N`.
fn parse_state_format_version(format: &str) -> Result<u64, String> {
    let (family, version) = format
        .split_once('/')
        .ok_or_else(|| format!("{format:?} has no `/version`"))?;
    if family != "agalma-state" {
        return Err(format!("unknown family {family:?}"));
    }
    let digits = version
        .strip_prefix('v')
        .ok_or_else(|| format!("version {version:?} is not `vN`"))?;
    digits
        .parse::<u64>()
        .map_err(|_| format!("version {version:?} is not `vN`"))
}

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
