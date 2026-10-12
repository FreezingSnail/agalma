//! Read model for `agalma status --json` (TUI.1, `agalma-3kr.2`).
//!
//! The TUI never opens the ledger itself: it consumes this snapshot, either as
//! an in-process call or by parsing the `--json` document (see
//! `docs/tui-design.md`, "Read model"). The snapshot is read-only and built from
//! the ledger contract alone (`LedgerApi::schema_version`, `executions`,
//! `pending_intents`, `kill_latch`, and the recorded harness binding events).

use std::path::Path;

use agalma_contracts::{ExecutionRecord, LedgerApi};
use agalma_ledger::SqliteLedger;
use serde::Serialize;

/// Top-level read-model document emitted by `agalma status --json`.
///
/// Stable and additive only: bump `schema_version` to match the ledger when the
/// shape changes (the field here mirrors the ledger's stored schema version).
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// Resolved state directory the snapshot was taken from.
    pub state_dir: String,
    /// Whether a ledger file is present (absent ledger is not an error).
    pub exists: bool,
    /// Current kill-latch state (`false` when no ledger is present).
    pub kill_latch: bool,
    /// Stored ledger schema version; `null` when no ledger is present.
    pub schema_version: Option<u32>,
    /// Latest harness binding label (`<impl>@<version>`); `null` when none.
    pub binding: Option<String>,
    /// One entry per durable execution, ordered by execution id.
    pub executions: Vec<ExecutionSnapshot>,
}

/// One execution row in the read model.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionSnapshot {
    pub execution_id: String,
    /// `Debug` name of [`agalma_contracts::ExecutionPhase`] (e.g. `"Build"`).
    pub phase: String,
    /// `Debug` name of [`agalma_contracts::ExecutionState`] (e.g. `"Running"`).
    pub state: String,
    pub attempt: u32,
    /// Unconsumed dispatch intents for this execution (pending operations).
    pub pending_operations: Vec<String>,
    /// Reason recorded when the execution parked, if any (extra field).
    pub parked_reason: Option<String>,
    /// Latest harness binding label for this execution (extra field).
    pub harness: String,
}

/// Build the read model from `state_dir`.
///
/// An absent ledger is not an error: it yields `exists:false` with empty or
/// `null` fields. Any ledger read failure after the file is present is an error
/// (the caller reports it and exits non-zero).
pub fn snapshot(state_dir: &Path) -> Result<Snapshot, String> {
    let ledger_path = state_dir.join("ledger.sqlite");
    if !ledger_path.exists() {
        return Ok(Snapshot {
            state_dir: state_dir.display().to_string(),
            exists: false,
            kill_latch: false,
            schema_version: None,
            binding: None,
            executions: Vec::new(),
        });
    }

    let ledger = SqliteLedger::open(&ledger_path)
        .map_err(|err| format!("cannot open ledger {}: {err}", ledger_path.display()))?;
    let schema_version = ledger
        .schema_version()
        .map_err(|err| format!("cannot read ledger schema: {err}"))?;
    let kill_latch = ledger
        .kill_latch()
        .map_err(|err| format!("cannot read kill latch: {err}"))?;
    let records = ledger
        .executions()
        .map_err(|err| format!("cannot read executions: {err}"))?;
    let pending = ledger
        .pending_intents()
        .map_err(|err| format!("cannot read pending intents: {err}"))?;

    let executions = records
        .iter()
        .map(|record| {
            let pending_operations = pending
                .iter()
                .filter(|intent| intent.execution_id == record.execution_id)
                .map(|intent| intent.operation_id.to_string())
                .collect();
            ExecutionSnapshot {
                execution_id: record.execution_id.to_string(),
                phase: format!("{:?}", record.phase),
                state: format!("{:?}", record.state),
                attempt: record.attempt,
                pending_operations,
                parked_reason: parked_reason(&ledger, record),
                harness: binding_label(&ledger, record),
            }
        })
        .collect();

    Ok(Snapshot {
        state_dir: state_dir.display().to_string(),
        exists: true,
        kill_latch,
        schema_version: Some(schema_version),
        binding: overall_binding(&ledger, &records),
        executions,
    })
}

/// Serialize a snapshot as the compact JSON contract (`--json`).
pub fn to_json(snapshot: &Snapshot) -> Result<String, String> {
    serde_json::to_string(snapshot)
        .map_err(|err| format!("cannot serialize status snapshot: {err}"))
}

/// Reason recorded by the most recent `parked` event for an execution.
fn parked_reason<L: LedgerApi + ?Sized>(ledger: &L, record: &ExecutionRecord) -> Option<String> {
    ledger.events(&record.execution_id).ok().and_then(|events| {
        events
            .iter()
            .rev()
            .find(|event| event.kind == "parked")
            .and_then(|event| event.payload.get("reason"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    })
}

/// One-line harness binding label for an execution (latest recorded pin).
fn binding_label<L: LedgerApi + ?Sized>(ledger: &L, record: &ExecutionRecord) -> String {
    match crate::harness_binding::latest_binding(ledger, &record.execution_id) {
        Ok(Some(binding)) => format!(
            "{}@{}",
            binding.implementation, binding.implementation_version
        ),
        _ => "none".to_string(),
    }
}

/// Current harness binding label for the whole ledger: the latest recorded
/// per-execution pin (executions are ordered by id).
fn overall_binding<L: LedgerApi + ?Sized>(
    ledger: &L,
    records: &[ExecutionRecord],
) -> Option<String> {
    let mut label = None;
    for record in records {
        if let Ok(Some(binding)) =
            crate::harness_binding::latest_binding(ledger, &record.execution_id)
        {
            label = Some(format!(
                "{}@{}",
                binding.implementation, binding.implementation_version
            ));
        }
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::{
        CommitBatch, DispatchIntent, ExecutionId, ExecutionPhase, ExecutionState, OperationId,
        TaskId,
    };
    use agalma_testkit::ScratchDir;
    use serde_json::json;

    /// A ledger with one running execution, one pending operation, and a pin.
    fn ledger_with_execution(dir: &ScratchDir) -> ExecutionId {
        let exec = ExecutionId::derive(&TaskId::new("task-a"), 1);
        let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let record = ExecutionRecord {
            execution_id: exec.clone(),
            task_id: TaskId::new("task-a"),
            generation: 1,
            phase: ExecutionPhase::Build,
            state: ExecutionState::Running,
            attempt: 2,
            revision: 0,
        };
        let operation = OperationId::derive(&exec, "build@2");
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(record),
                events: vec![],
                receipts: vec![],
                intents: vec![DispatchIntent {
                    operation_id: operation,
                    execution_id: exec.clone(),
                    payload: json!({}),
                    enqueued_at_unix_ms: 1,
                    consumed: false,
                }],
            })
            .expect("seed execution + intent");
        let plan = crate::harness_binding::plan_binding(
            crate::harness_binding::HarnessKind::Reference,
            1,
            "model",
        )
        .expect("plan binding");
        crate::harness_binding::persist_binding(
            &mut ledger,
            &exec,
            &OperationId::derive(&exec, "build@2"),
            2,
            &plan,
        )
        .expect("persist binding");
        exec
    }

    #[test]
    fn absent_ledger_is_not_an_error() {
        let dir = ScratchDir::new("status-absent");
        let snapshot = snapshot(dir.path()).expect("snapshot");
        assert!(!snapshot.exists);
        assert!(!snapshot.kill_latch);
        assert_eq!(snapshot.schema_version, None);
        assert_eq!(snapshot.binding, None);
        assert!(snapshot.executions.is_empty());

        let json = to_json(&snapshot).expect("json");
        assert!(json.contains("\"exists\":false"), "{json}");
        assert!(json.contains("\"schema_version\":null"), "{json}");
        assert!(json.contains("\"binding\":null"), "{json}");
        assert!(json.contains("\"executions\":[]"), "{json}");
    }

    #[test]
    fn present_ledger_reports_schema_pending_and_binding() {
        let dir = ScratchDir::new("status-present");
        let exec = ledger_with_execution(&dir);
        let snapshot = snapshot(dir.path()).expect("snapshot");

        assert!(snapshot.exists);
        assert_eq!(snapshot.schema_version, Some(agalma_ledger::SCHEMA_VERSION));
        assert_eq!(snapshot.binding.as_deref(), Some("reference@reference/v1"));
        assert_eq!(snapshot.executions.len(), 1);

        let row = &snapshot.executions[0];
        assert_eq!(row.execution_id, exec.to_string());
        assert_eq!(row.phase, "Build");
        assert_eq!(row.state, "Running");
        assert_eq!(row.attempt, 2);
        assert_eq!(
            row.pending_operations,
            vec![OperationId::derive(&exec, "build@2").to_string()]
        );
        assert_eq!(row.parked_reason, None);
        assert_eq!(row.harness, "reference@reference/v1");

        let json = to_json(&snapshot).expect("json");
        assert!(
            json.contains(&format!(
                "\"schema_version\":{}",
                agalma_ledger::SCHEMA_VERSION
            )),
            "{json}"
        );
        assert!(json.contains("\"pending_operations\""), "{json}");
        assert!(
            json.contains("\"binding\":\"reference@reference/v1\""),
            "{json}"
        );
    }

    #[test]
    fn kill_latch_is_reported() {
        let dir = ScratchDir::new("status-latch");
        ledger_with_execution(&dir);
        {
            let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
            ledger.set_kill_latch(true).expect("set latch");
        }
        let snapshot = snapshot(dir.path()).expect("snapshot");
        assert!(snapshot.kill_latch);
    }
}
