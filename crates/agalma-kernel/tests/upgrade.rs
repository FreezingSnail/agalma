//! B0.6 upgrade tests (`docs/b0-kernel.md` §"Upgrade, activation, rollback";
//! fixture cases 8 and the state export/import contract).
//!
//! - `check_candidate_reconstructs_pending_state_without_effects` — a candidate
//!   is checked against saved history by reducer-only reconstruction.
//! - `upgrade_with_pending_executions_resumes_or_parks` — fixture case 8: a
//!   compatible candidate resumes pending executions; an unknown `exec_def`
//!   version parks until an explicit migration is registered.
//! - `export_import_round_trip_preserves_pending_work_and_receipts` — canonical
//!   JSON snapshot round-trips; a compatible replacement consumes it.
//! - `import_refuses_incompatible_versions` — format/schema mismatch is refused
//!   without partial application.
//!
//! Isolated state lives under `target/test-runs/recovery-*` via the frozen
//! harness.

mod support;

use std::collections::BTreeSet;

use agalma_contracts::{
    ArtifactRef, BindingId, BindingRecord, BindingState, ContractError, ExecutionId,
    ExecutionPhase, LedgerApi, OperationId, StateSnapshot, TaskId, TimerRecord, TimerState,
};
use agalma_kernel::{
    check_candidate, operation_input_hash, recover_pending, ActivityDef, ActivityRegistry, Command,
    DirectivePlan, ExecState, Inputs, KernelEngine, MigrationRegistry, OperationResult, Probe,
    VersionLabelMigration, EXEC_DEF_VERSION,
};
use agalma_ledger::SqliteLedger;
use serde_json::json;

use support::harness::{self, EffectJournal, RecordingActivity};
use support::ops::JournalHandler;

fn exec() -> ExecutionId {
    ExecutionId::derive(&TaskId::new("fix-answer"), 1)
}

fn build_def() -> ActivityDef {
    ActivityDef::new("fake/build", "v1")
}

fn start(engine: &mut KernelEngine<SqliteLedger>) {
    engine
        .apply(
            &exec(),
            &Command::Start {
                task_id: TaskId::new("fix-answer"),
                generation: 1,
            },
            &Inputs::default(),
        )
        .unwrap();
}

/// Announce an operation (durable intent, no effect) with optional binding.
fn announce(
    engine: &mut KernelEngine<SqliteLedger>,
    def: &ActivityDef,
    step: &str,
    phase: ExecutionPhase,
    inputs: serde_json::Value,
    now_unix_ms: u64,
) {
    let operation_id = OperationId::derive(&exec(), step);
    let inputs_hash = operation_input_hash(def, &inputs);
    engine
        .apply(
            &exec(),
            &Command::Deliver {
                operation_id,
                inputs_hash,
            },
            &Inputs {
                probe: Probe::Absent,
                activity: Some(def.clone()),
                operation_inputs: Some(inputs),
                phase: Some(phase),
                now_unix_ms,
                ..Inputs::default()
            },
        )
        .unwrap();
}

fn binding_record(id: &str, implementation: &str, state: BindingState) -> BindingRecord {
    BindingRecord {
        binding_id: BindingId::new(id),
        component_api: "harness".to_string(),
        api_version: 1,
        implementation: implementation.to_string(),
        implementation_version: "v1".to_string(),
        capabilities: BTreeSet::new(),
        required_capabilities: BTreeSet::new(),
        optional_capabilities: BTreeSet::new(),
        config_hash: "cfg".to_string(),
        generation: 1,
        state,
    }
}

// ---------------------------------------------------------------------------
// Candidate check (reducer-only)
// ---------------------------------------------------------------------------

#[test]
fn check_candidate_reconstructs_pending_state_without_effects() {
    let (dir, ledger) = harness::open_isolated_ledger("candidate-check");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let def = build_def();
    start(&mut engine);
    announce(
        &mut engine,
        &def,
        "build",
        ExecutionPhase::Build,
        json!({ "task": "fix-answer" }),
        1,
    );
    assert_eq!(engine.pending_dispatch().unwrap().len(), 1);

    let report = check_candidate(engine.ledger(), None).unwrap();
    assert!(report.compatible(), "report: {report:?}");
    assert_eq!(report.schema_version, 3);
    assert_eq!(report.executions.len(), 1);

    // Reducer-only: reconstruction runs no external effect.
    let journal = EffectJournal::in_run_dir(&dir, "candidate-check");
    assert_eq!(journal.count(), 0, "candidate check must not run effects");
}

// ---------------------------------------------------------------------------
// Fixture case 8 — upgrade with pending executions
// ---------------------------------------------------------------------------

#[test]
fn upgrade_with_pending_executions_resumes_or_parks() {
    let (_dir_a, ledger_a) = harness::open_isolated_ledger("upgrade-pending");
    let mut a = KernelEngine::open(ledger_a).unwrap();
    let def = build_def();
    start(&mut a);
    announce(
        &mut a,
        &def,
        "build",
        ExecutionPhase::Build,
        json!({ "task": "fix-answer" }),
        1,
    );
    assert_eq!(a.pending_dispatch().unwrap().len(), 1);

    // (1) Compatible candidate: reducer-only check passes; state is preserved.
    let report = check_candidate(a.ledger(), None).unwrap();
    assert!(report.compatible(), "report: {report:?}");
    let snapshot = a.export_state().unwrap();
    assert_eq!(snapshot.pending_work(), 1);

    // (2) A compatible replacement consumes the exported state and resumes the
    //     pending execution (no external effect before the swap).
    let (dir_b, ledger_b) = harness::open_isolated_ledger("upgrade-pending-b");
    let mut b = KernelEngine::open(ledger_b).unwrap();
    let journal_b = EffectJournal::in_run_dir(&dir_b, "upgrade-pending-b");
    assert_eq!(journal_b.count(), 0, "import applies no external effect");
    b.import_state(&snapshot).unwrap();
    assert_eq!(b.pending_dispatch().unwrap().len(), 1);
    assert_eq!(
        b.load(&exec()).unwrap().state,
        ExecState::Running,
        "pending work reconstructs after the compatible swap"
    );

    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal_b.clone()),
        )))
        .unwrap();
    let results = recover_pending(&mut b, &mut registry, &exec(), 2).unwrap();
    assert!(matches!(results[0], OperationResult::Completed { .. }));
    assert_eq!(journal_b.effect_count(), 1, "resumed exactly once");
    let loaded = b.load(&exec()).unwrap();
    assert_ne!(loaded.state, ExecState::Parked);
    assert!(loaded.matches_projection(&b.ledger().execution(&exec()).unwrap().unwrap()));

    // (3) Incompatible exec_def version parks (no guessing, no effect).
    let (dir_c, ledger_c) = harness::open_isolated_ledger("upgrade-pending-incompat");
    {
        let mut c = KernelEngine::open(ledger_c).unwrap();
        c.import_state(&snapshot).unwrap();
        c.ledger_mut()
            .kernel_meta_set("exec_def_version", "exec_def/v99")
            .unwrap();
    }
    let journal_c = EffectJournal::in_run_dir(&dir_c, "upgrade-pending-incompat");
    let c2 = KernelEngine::open(SqliteLedger::open(dir_c.join("ledger.sqlite")).unwrap()).unwrap();
    assert_eq!(c2.load(&exec()).unwrap().state, ExecState::Parked);
    let report_c = check_candidate(c2.ledger(), None).unwrap();
    assert!(report_c.unknown_exec_def);
    assert_eq!(report_c.parks(), 1);
    assert_eq!(
        journal_c.count(),
        0,
        "incompatible state never runs effects"
    );

    // (4) An approved migration relabels a compatible prior definition and the
    //     upgraded kernel resumes the same pending execution.
    let (dir_d, ledger_d) = harness::open_isolated_ledger("upgrade-pending-migrate");
    {
        let mut d = KernelEngine::open(ledger_d).unwrap();
        d.import_state(&snapshot).unwrap();
        d.ledger_mut()
            .kernel_meta_set("exec_def_version", "exec_def/v0")
            .unwrap();
    }
    let mut migrations = MigrationRegistry::new();
    migrations
        .register(Box::new(VersionLabelMigration::new(
            "exec_def/v0",
            EXEC_DEF_VERSION,
        )))
        .unwrap();
    let raw = SqliteLedger::open(dir_d.join("ledger.sqlite")).unwrap();
    let report_d = check_candidate(&raw, Some(&migrations)).unwrap();
    assert!(report_d.migration_available);
    assert_eq!(report_d.needs_migration(), 1);
    let d2 = KernelEngine::open_with_migrations(raw, &migrations).unwrap();
    assert_eq!(d2.exec_def_version(), Some(EXEC_DEF_VERSION));
    assert_eq!(d2.load(&exec()).unwrap().state, ExecState::Running);
    // The migration is recorded durably.
    assert_eq!(
        d2.ledger()
            .kernel_meta_get("migration:exec_def/v0")
            .unwrap()
            .as_deref(),
        Some(EXEC_DEF_VERSION)
    );
}

// ---------------------------------------------------------------------------
// Export / import round-trip
// ---------------------------------------------------------------------------

#[test]
fn export_import_round_trip_preserves_pending_work_and_receipts() {
    let (_dir_a, ledger_a) = harness::open_isolated_ledger("snapshot-round-trip");
    let mut a = KernelEngine::open(ledger_a).unwrap();
    let def = build_def();
    start(&mut a);

    // One completed operation (receipt) and one pending operation.
    let intake = OperationId::derive(&exec(), "intake");
    a.apply(
        &exec(),
        &Command::Deliver {
            operation_id: intake.clone(),
            inputs_hash: "h-intake".to_string(),
        },
        &Inputs {
            probe: Probe::Absent,
            now_unix_ms: 1,
            ..Inputs::default()
        },
    )
    .unwrap();
    a.apply(
        &exec(),
        &Command::Deliver {
            operation_id: intake.clone(),
            inputs_hash: "h-intake".to_string(),
        },
        &Inputs {
            probe: Probe::Present,
            outcome_ref: Some(ArtifactRef::new("artifact:intake")),
            now_unix_ms: 2,
            ..Inputs::default()
        },
    )
    .unwrap();
    announce(
        &mut a,
        &def,
        "build",
        ExecutionPhase::Build,
        json!({ "task": "fix-answer" }),
        3,
    );

    // A durable timer, directive, and binding row.
    a.schedule_timer(&TimerRecord {
        timer_id: "t1".to_string(),
        execution_id: exec(),
        kind: "retry".to_string(),
        fire_at_unix_ms: 100,
        state: TimerState::Pending,
        payload: json!({ "attempt": 1 }),
    })
    .unwrap();
    a.submit_directive(
        &exec(),
        "d1",
        1,
        &DirectivePlan {
            source: "test".to_string(),
            steps: vec![],
        },
        4,
    )
    .unwrap();
    a.ledger_mut()
        .upsert_binding(&binding_record(
            "binding:seed",
            "seed",
            BindingState::Active,
        ))
        .unwrap();

    let snapshot = a.export_state().unwrap();
    assert_eq!(snapshot.pending_work(), 1);
    assert_eq!(snapshot.receipts.len(), 1);
    assert_eq!(snapshot.timers.len(), 1);
    assert_eq!(snapshot.directives.len(), 1);
    assert_eq!(snapshot.bindings.len(), 1);
    let json1 = snapshot.to_canonical_json().unwrap();

    // Round-trip through JSON into a fresh ledger.
    let (_dir_b, ledger_b) = harness::open_isolated_ledger("snapshot-round-trip-b");
    let mut b = KernelEngine::open(ledger_b).unwrap();
    let parsed = StateSnapshot::from_json(&json1).unwrap();
    b.import_state(&parsed).unwrap();

    assert_eq!(b.receipt(&intake).unwrap().unwrap().inputs_hash, "h-intake");
    assert_eq!(b.pending_dispatch().unwrap().len(), 1);
    assert_eq!(
        b.ledger().timer("t1").unwrap().unwrap().state,
        TimerState::Pending
    );
    assert!(b.ledger().directive("d1").unwrap().is_some());
    assert_eq!(b.ledger().bindings().unwrap().len(), 1);

    // Canonical: a second export is byte-identical.
    let json2 = b.export_state().unwrap().to_canonical_json().unwrap();
    assert_eq!(json1, json2, "snapshot round-trip must be canonical");
}

#[test]
fn import_refuses_incompatible_versions() {
    let (_dir_a, ledger_a) = harness::open_isolated_ledger("snapshot-refuse");
    let mut a = KernelEngine::open(ledger_a).unwrap();
    start(&mut a);
    let snapshot = a.export_state().unwrap();

    let (_dir_b, ledger_b) = harness::open_isolated_ledger("snapshot-refuse-b");
    let mut b = KernelEngine::open(ledger_b).unwrap();

    // Unknown format header.
    let mut bad_format = snapshot.clone();
    bad_format.format = "agalma-state/v99".to_string();
    let err = b.import_state(&bad_format).unwrap_err();
    assert!(matches!(err, ContractError::Conflict(_)), "got {err:?}");
    assert!(
        b.ledger().executions().unwrap().is_empty(),
        "refused import must not partially apply"
    );

    // Mismatched physical schema.
    let mut bad_schema = snapshot.clone();
    bad_schema.schema_version = 2;
    let err = b.import_state(&bad_schema).unwrap_err();
    assert!(matches!(err, ContractError::Conflict(_)), "got {err:?}");
    assert!(b.ledger().executions().unwrap().is_empty());
}
