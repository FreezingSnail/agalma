//! B0.3 activity-protocol tests: registry/duplicate/conflict, reconcile-before-
//! retry, ambiguous park, and pinned-binding reconciliation.
//!
//! Isolated state lives under `target/test-runs/recovery-*` via the frozen
//! harness; fake effects are journaled by the harness [`RecordingActivity`].

mod support;

use std::collections::BTreeSet;

use agalma_contracts::{
    BindingId, BindingRecord, BindingState, ExecutionId, ExecutionPhase, LedgerApi, OperationId,
    OperationReceipt, TaskId,
};
use agalma_kernel::{
    active_binding, operation_input_hash, recover_pending, run_activity, ActivityDef,
    ActivityRegistry, BindingPin, Command, EffectProbe, ExecState, Inputs, KernelEngine,
    OperationRequest, OperationResult, OperationState, Probe,
};
use agalma_ledger::SqliteLedger;
use serde_json::{json, Value};

use support::harness::{self, EffectJournal, RecordingActivity};
use support::ops::{BindingRecordingHandler, JournalHandler, ScriptedHandler};

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

fn request(
    def: &ActivityDef,
    step: &str,
    phase: ExecutionPhase,
    inputs: Value,
) -> OperationRequest {
    OperationRequest {
        activity: def.clone(),
        step: step.to_string(),
        phase,
        component_api: "harness".to_string(),
        inputs,
        now_unix_ms: 1,
    }
}

/// Announce an operation without executing it (simulates a pre-effect crash).
fn announce(
    engine: &mut KernelEngine<SqliteLedger>,
    def: &ActivityDef,
    step: &str,
    phase: ExecutionPhase,
    inputs: Value,
    binding: Option<BindingPin>,
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
                binding,
                activity: Some(def.clone()),
                operation_inputs: Some(inputs.clone()),
                phase: Some(phase),
                now_unix_ms,
                ..Inputs::default()
            },
        )
        .unwrap();
}

fn completed(result: OperationResult) -> (OperationReceipt, bool, bool) {
    match result {
        OperationResult::Completed {
            receipt,
            reconciled,
            duplicate,
            ..
        } => (receipt, duplicate, reconciled),
        other => panic!("expected Completed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[test]
fn registry_rejects_duplicate_definitions_and_unknown_activities_do_not_run() {
    let (dir, ledger) = harness::open_isolated_ledger("activity-registry");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let journal = EffectJournal::in_run_dir(&dir, "activity-registry");
    let def = build_def();

    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    assert!(registry.contains(&def));
    assert_eq!(registry.len(), 1);

    // The same `name@version` cannot be registered twice.
    let conflict = registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap_err();
    assert_eq!(conflict.id, "fake/build");
    assert!(conflict.reason.contains("already registered"));
    assert_eq!(registry.len(), 1);

    // A different version coexists.
    registry
        .register(Box::new(JournalHandler::new(
            ActivityDef::new("fake/build", "v2"),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    assert_eq!(registry.len(), 2);

    // An unregistered activity is never executed.
    start(&mut engine);
    let unknown = ActivityDef::new("fake/missing", "v1");
    let result = run_activity(
        &mut engine,
        &mut registry,
        &exec(),
        &request(&unknown, "missing", ExecutionPhase::Build, json!({})),
    )
    .unwrap();
    assert!(matches!(result, OperationResult::UnknownActivity { .. }));
    assert_eq!(journal.effect_count(), 0);
}

// ---------------------------------------------------------------------------
// Duplicate delivery + conflict
// ---------------------------------------------------------------------------

#[test]
fn duplicate_delivery_returns_stored_result_and_reuse_conflicts() {
    let (dir, ledger) = harness::open_isolated_ledger("activity-duplicate");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let journal = EffectJournal::in_run_dir(&dir, "activity-duplicate");
    let def = build_def();
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    start(&mut engine);

    let req = request(
        &def,
        "build",
        ExecutionPhase::Build,
        json!({ "task": "fix-answer" }),
    );
    let (receipt, duplicate, reconciled) =
        completed(run_activity(&mut engine, &mut registry, &exec(), &req).unwrap());
    assert!(!duplicate, "first delivery is not a duplicate");
    assert!(!reconciled);
    assert_eq!(journal.effect_count(), 1);

    // Duplicate delivery returns the recorded receipt and repeats no effect.
    let (again, duplicate, _) =
        completed(run_activity(&mut engine, &mut registry, &exec(), &req).unwrap());
    assert!(duplicate);
    assert_eq!(again.result, receipt.result);
    assert_eq!(again.inputs_hash, receipt.inputs_hash);
    assert_eq!(journal.effect_count(), 1, "duplicate repeats no effect");

    // Reusing the id with different inputs is a conflict.
    let mut changed = req.clone();
    changed.inputs = json!({ "task": "other" });
    let conflict = run_activity(&mut engine, &mut registry, &exec(), &changed).unwrap();
    assert!(matches!(conflict, OperationResult::Conflict(_)));
    assert_eq!(journal.effect_count(), 1);
}

// ---------------------------------------------------------------------------
// Reconcile before retry
// ---------------------------------------------------------------------------

#[test]
fn reconcile_present_completes_without_re_executing() {
    let (dir, ledger) = harness::open_isolated_ledger("activity-reconcile-present");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let journal = EffectJournal::in_run_dir(&dir, "activity-reconcile-present");
    let def = build_def();
    let op = OperationId::derive(&exec(), "build");

    // A surviving effect: the journal already holds the execute line.
    journal.record(op.as_str(), "execute", "applied").unwrap();

    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    start(&mut engine);
    announce(
        &mut engine,
        &def,
        "build",
        ExecutionPhase::Build,
        json!({ "task": "fix-answer" }),
        None,
        1,
    );

    let results = recover_pending(&mut engine, &mut registry, &exec(), 2).unwrap();
    assert_eq!(results.len(), 1);
    let (_receipt, duplicate, reconciled) = completed(results.into_iter().next().unwrap());
    assert!(reconciled, "present effect is reconciled, not re-executed");
    assert!(!duplicate);
    assert_eq!(journal.effect_count(), 1, "effect is not repeated");
    assert_eq!(
        journal
            .entries()
            .iter()
            .filter(|entry| entry.kind == "reconcile")
            .count(),
        1,
        "reconciliation is recorded"
    );
    let state = engine.load(&exec()).unwrap();
    assert_eq!(state.operations[&op].state, OperationState::Completed);
}

#[test]
fn reconcile_absent_retries_the_effect_exactly_once() {
    let (dir, ledger) = harness::open_isolated_ledger("activity-reconcile-absent");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let journal = EffectJournal::in_run_dir(&dir, "activity-reconcile-absent");
    let def = build_def();
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    start(&mut engine);
    announce(
        &mut engine,
        &def,
        "build",
        ExecutionPhase::Build,
        json!({ "task": "fix-answer" }),
        None,
        1,
    );

    let results = recover_pending(&mut engine, &mut registry, &exec(), 2).unwrap();
    let (_receipt, duplicate, reconciled) = completed(results.into_iter().next().unwrap());
    assert!(!reconciled, "absent effect is retried, not reconciled");
    assert!(!duplicate);
    assert_eq!(journal.effect_count(), 1, "retried exactly once");
}

// ---------------------------------------------------------------------------
// Ambiguous park
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_effect_parks_without_executing() {
    let (dir, ledger) = harness::open_isolated_ledger("activity-ambiguous");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let journal = EffectJournal::in_run_dir(&dir, "activity-ambiguous");
    let def = build_def();
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(ScriptedHandler::new(
            def.clone(),
            EffectProbe::Ambiguous,
            journal.clone(),
        )))
        .unwrap();
    start(&mut engine);
    announce(
        &mut engine,
        &def,
        "build",
        ExecutionPhase::Build,
        json!({ "task": "fix-answer" }),
        None,
        1,
    );

    let results = recover_pending(&mut engine, &mut registry, &exec(), 2).unwrap();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0], OperationResult::Parked { .. }));
    assert_eq!(journal.effect_count(), 0, "ambiguous effect never executes");
    assert_eq!(engine.load(&exec()).unwrap().state, ExecState::Parked);
}

// ---------------------------------------------------------------------------
// Pinned-binding reconciliation
// ---------------------------------------------------------------------------

fn pin(component_api: &str, implementation: &str, version: &str) -> BindingPin {
    BindingPin {
        binding_id: BindingId::new(format!("binding:{implementation}:{version}")),
        component_api: component_api.to_string(),
        api_version: 1,
        implementation: implementation.to_string(),
        implementation_version: version.to_string(),
    }
}

fn binding_record(pin: &BindingPin, state: BindingState) -> BindingRecord {
    BindingRecord {
        binding_id: pin.binding_id.clone(),
        component_api: pin.component_api.clone(),
        api_version: pin.api_version,
        implementation: pin.implementation.clone(),
        implementation_version: pin.implementation_version.clone(),
        capabilities: BTreeSet::new(),
        required_capabilities: BTreeSet::new(),
        optional_capabilities: BTreeSet::new(),
        config_hash: "cfg".to_string(),
        generation: 1,
        state,
    }
}

#[test]
fn reconcile_uses_pinned_binding_not_the_configured_adapter() {
    let (dir, ledger) = harness::open_isolated_ledger("activity-pinned-binding");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let journal = EffectJournal::in_run_dir(&dir, "activity-pinned-binding");
    let def = build_def();

    let (handler, seen) =
        BindingRecordingHandler::new(def.clone(), RecordingActivity::new(journal.clone()));
    let mut registry = ActivityRegistry::new();
    registry.register(Box::new(handler)).unwrap();

    // Dispatch under binding A (active).
    let pin_a = pin("harness", "harness-reference", "v1");
    engine
        .ledger_mut()
        .upsert_binding(&binding_record(&pin_a, BindingState::Active))
        .unwrap();
    start(&mut engine);
    announce(
        &mut engine,
        &def,
        "build",
        ExecutionPhase::Build,
        json!({ "task": "fix-answer" }),
        Some(pin_a.clone()),
        1,
    );

    // The configured adapter is swapped to binding B before recovery.
    let pin_b = pin("harness", "harness-opencode", "v2");
    engine
        .ledger_mut()
        .upsert_binding(&binding_record(&pin_a, BindingState::Retired))
        .unwrap();
    engine
        .ledger_mut()
        .upsert_binding(&binding_record(&pin_b, BindingState::Active))
        .unwrap();
    assert_eq!(
        active_binding(engine.ledger(), "harness")
            .unwrap()
            .unwrap()
            .implementation_version,
        "v2"
    );

    // Reconciliation runs against the pinned binding A, not the active B.
    let results = recover_pending(&mut engine, &mut registry, &exec(), 2).unwrap();
    completed(results.into_iter().next().unwrap());
    assert_eq!(
        *seen.borrow(),
        vec![Some("harness-reference:v1".to_string())],
        "reconcile must use the pinned binding, never the configured adapter"
    );
}
