//! Read model for `agalma status --json` (TUI.1, `agalma-3kr.2`).
//!
//! The TUI never opens the ledger itself: it consumes this snapshot, either as
//! an in-process call or by parsing the `--json` document (see
//! `docs/tui-design.md`, "Read model"). The snapshot is read-only and built from
//! the ledger contract alone (`LedgerApi::schema_version`, `executions`,
//! `pending_intents`, `kill_latch`, and the recorded harness binding events).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agalma_contracts::{ExecutionRecord, LedgerApi};
use serde::Serialize;

use crate::components::{ComponentSnapshot, PluginComponent, Transports};
use crate::economics::EconomicsSnapshot;
use crate::ledger_process::{LedgerHandle, LedgerTransport};
use crate::plugin_reports::ReportIngestor;

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
    /// Per-component vision: bindings, transports, socket liveness, health.
    pub components: Vec<ComponentSnapshot>,
    /// Registry/report-driven plugin components and their declarative views
    /// (HP.6). Empty when no production registry/report read model is attached.
    pub plugins: Vec<PluginComponent>,
    /// Economics + timeline: spend vs intake-fence allowance, digest summaries,
    /// parked/degradation alerts, curated event timeline (TUI.4).
    pub economics: EconomicsSnapshot,
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
    snapshot_with_transports(state_dir, &Transports::from_env())
}

/// Build the read model with explicit boundary transports.
///
/// The transport selectors are process-global (env-derived); passing them in
/// keeps the snapshot deterministic for tests and callers that already resolved
/// them. The per-component panel carries them so the TUI never re-derives them.
///
/// The plugin panel is derived from the **selected ledger service**: the
/// durable `RegistryCheckpoint` is read through the ledger handle and the
/// hub-owned installed manifests supply the declared views. The ledger binding
/// is the one pinned in `transports.ledger`.
pub fn snapshot_with_transports(
    state_dir: &Path,
    transports: &Transports,
) -> Result<Snapshot, String> {
    snapshot_with_selected_transport(state_dir, transports, ledger_transport_for(transports))
}

/// The selected ledger binding for the pinned boundary transports.
fn ledger_transport_for(transports: &Transports) -> LedgerTransport {
    match transports.ledger {
        "in-process" => LedgerTransport::InProcess,
        _ => LedgerTransport::Process,
    }
}

/// Build the read model over an explicit selected ledger binding.
///
/// This is the production read path for both `agalma status --json` and
/// `agalma watch`: the ledger is reached through the selected
/// [`LedgerHandle`] (never a direct SQLite helper), and the plugin panel comes
/// from the durable registry/report state. An absent ledger is not an error;
/// any ledger read failure after the file is present is (the caller reports it
/// and exits non-zero).
pub fn snapshot_with_selected_transport(
    state_dir: &Path,
    transports: &Transports,
    ledger_transport: LedgerTransport,
) -> Result<Snapshot, String> {
    let ledger_path = state_dir.join("ledger.sqlite");
    if !ledger_path.exists() {
        return Ok(Snapshot {
            state_dir: state_dir.display().to_string(),
            exists: false,
            kill_latch: false,
            schema_version: None,
            binding: None,
            executions: Vec::new(),
            components: crate::components::derive::<LedgerHandle>(state_dir, None, &[], transports),
            plugins: Vec::new(),
            economics: EconomicsSnapshot::default(),
        });
    }

    let ledger =
        LedgerHandle::from_transport_existing(state_dir, ledger_transport).map_err(|err| {
            format!(
                "cannot open selected ledger service at {}: {err}",
                ledger_path.display()
            )
        })?;
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
        components: crate::components::derive(state_dir, Some(&ledger), &records, transports),
        plugins: read_plugin_panel(state_dir, &ledger)?,
        economics: crate::economics::derive(&ledger, &records, &BTreeMap::new()),
    })
}

/// Derive the plugin panel from the durable registry state and the report
/// component store, both reached through the selected ledger handle.
///
/// The report ingestor is opened over the **same** ledger handle (a
/// `component-store/v1` read), so no second connection or private store is used.
///
/// Fails closed: a selected-ledger registry or report read failure is surfaced
/// as an error (the caller reports it and exits non-zero), never a silently
/// empty/healthy plugin panel.
fn read_plugin_panel(
    state_dir: &Path,
    ledger: &LedgerHandle,
) -> Result<Vec<PluginComponent>, String> {
    let sources = crate::components::load_plugin_sources(state_dir, ledger).map_err(|err| {
        format!("cannot read plugin registry state from the selected ledger: {err}")
    })?;
    let active_generations: BTreeMap<String, u64> = sources
        .iter()
        .filter_map(|source| {
            source
                .generation
                .map(|generation| (source.plugin_id.clone(), generation))
        })
        .collect();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(move || now_ms);
    let ingestor = ReportIngestor::open(
        Box::new(Mutex::new(ledger.clone())),
        clock,
        active_generations,
        BTreeSet::new(),
    )
    .map_err(|err| format!("cannot read plugin report state from the selected ledger: {err}"))?;
    Ok(crate::components::derive_plugin_components_from_sources(
        &sources,
        Some(&ingestor),
    ))
}

/// Serialize a snapshot as the compact JSON contract (`--json`).
pub fn to_json(snapshot: &Snapshot) -> Result<String, String> {
    serde_json::to_string(snapshot)
        .map_err(|err| format!("cannot serialize status snapshot: {err}"))
}

/// Reason recorded by the most recent `parked` event for an execution.
pub(crate) fn parked_reason<L: LedgerApi + ?Sized>(
    ledger: &L,
    record: &ExecutionRecord,
) -> Option<String> {
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
    use agalma_ledger::SqliteLedger;
    use agalma_testkit::ScratchDir;
    use serde_json::json;

    /// Read the snapshot over an explicit in-process selected ledger binding
    /// (deterministic; the env default is the process transport).
    fn snapshot_in_process(state_dir: &Path) -> Result<Snapshot, String> {
        snapshot_with_selected_transport(
            state_dir,
            &Transports::all_in_process(),
            LedgerTransport::InProcess,
        )
    }

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
        let snapshot = snapshot_in_process(dir.path()).expect("snapshot");
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
        let snapshot = snapshot_in_process(dir.path()).expect("snapshot");

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
        let snapshot = snapshot_in_process(dir.path()).expect("snapshot");
        assert!(snapshot.kill_latch);
    }
}
