//! B0.6 activation tests (`docs/b0-kernel.md` §"Upgrade, activation, rollback";
//! fixture cases 9 and 10).
//!
//! - `kernel_adapter_replacement_preserves_pending_work` — case 9: a compatible
//!   replacement consumes exported state; canonical ids/receipts/pending work
//!   survive and the in-flight operation stays pinned to its old binding.
//! - `staged_activation_happy_path_settles_present_effect` — the staged
//!   procedure settles an already-present in-flight effect and swaps bindings.
//! - `staged_activation_refuses_unmet_capabilities` — the S0d bind gate.
//! - `rollback_restore_reconciles_before_dispatch` — case 10: rollback retains
//!   the seed binding; restore reconciles ledger/bd/Git/processes before
//!   dispatch and parks on ambiguity (latch stays set).
//!
//! Isolated state lives under `target/test-runs/recovery-*` via the frozen
//! harness; the restore seam is a deterministic fake.

mod support;

use std::collections::BTreeSet;

use agalma_contracts::{
    BindingId, BindingRecord, BindingState, ContractError, ExecutionId, ExecutionPhase, LedgerApi,
    OperationId, TaskId,
};
use agalma_kernel::{
    active_binding, operation_input_hash, restore, rollback, run_activity, staged_activate,
    ActivityDef, ActivityRegistry, BindingPin, Command, ExecState, Inputs, KernelEngine,
    OperationRequest, OperationResult, Probe, ReconcileOutcome, RestoreDomain, RestoreReconciler,
    ACTIVATION_EVIDENCE_PREFIX, RESTORE_EVIDENCE_PREFIX,
};
use agalma_ledger::SqliteLedger;
use serde_json::json;

use support::harness::{self, EffectJournal, RecordingActivity};
use support::ops::{binding_label, BindingRecordingHandler, JournalHandler};

fn exec() -> ExecutionId {
    ExecutionId::derive(&TaskId::new("fix-answer"), 1)
}

fn build_def() -> ActivityDef {
    ActivityDef::new("fake/build", "v1")
}

fn task_inputs() -> serde_json::Value {
    json!({ "task": "fix-answer" })
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

fn binding(id: &str, implementation: &str, state: BindingState) -> BindingRecord {
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

/// Announce a pending `build` operation pinned to `pin` (durable, no effect).
fn announce_build(
    engine: &mut KernelEngine<SqliteLedger>,
    def: &ActivityDef,
    pin: &BindingPin,
) -> OperationId {
    let operation_id = OperationId::derive(&exec(), "build");
    let inputs_hash = operation_input_hash(def, &task_inputs());
    engine
        .apply(
            &exec(),
            &Command::Deliver {
                operation_id: operation_id.clone(),
                inputs_hash,
            },
            &Inputs {
                probe: Probe::Absent,
                binding: Some(pin.clone()),
                activity: Some(def.clone()),
                operation_inputs: Some(task_inputs()),
                phase: Some(ExecutionPhase::Build),
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .unwrap();
    operation_id
}

fn request(def: &ActivityDef) -> OperationRequest {
    OperationRequest {
        activity: def.clone(),
        step: "build".to_string(),
        phase: ExecutionPhase::Build,
        component_api: "harness".to_string(),
        inputs: task_inputs(),
        now_unix_ms: 2,
    }
}

// ---------------------------------------------------------------------------
// Fixture case 9 — kernel/adapter replacement
// ---------------------------------------------------------------------------

#[test]
fn kernel_adapter_replacement_preserves_pending_work() {
    let def = build_def();
    let seed = binding("binding:adapter", "adapter", BindingState::Active);
    let pin_seed = BindingPin::from_record(&seed);

    // Source ledger: one pending build pinned to the old adapter binding.
    let (_dir_a, ledger_a) = harness::open_isolated_ledger("adapter-replacement");
    let mut a = KernelEngine::open(ledger_a).unwrap();
    a.ledger_mut().upsert_binding(&seed).unwrap();
    start(&mut a);
    let op = announce_build(&mut a, &def, &pin_seed);
    let snapshot = a.export_state().unwrap();
    assert_eq!(snapshot.pending_work(), 1);

    // A compatible replacement consumes the exported state.
    let (dir_b, ledger_b) = harness::open_isolated_ledger("adapter-replacement-b");
    let mut b = KernelEngine::open(ledger_b).unwrap();
    b.import_state(&snapshot).unwrap();

    // Canonical ids and pending work survive.
    let pending = b.pending_dispatch().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].operation_id, op, "operation id unchanged");

    // The configured adapter is swapped to a new binding (old one retired)...
    b.ledger_mut()
        .upsert_binding(&binding(
            "binding:adapter",
            "adapter",
            BindingState::Retired,
        ))
        .unwrap();
    let replacement = binding("binding:replacement", "adapter-2", BindingState::Active);
    b.ledger_mut().upsert_binding(&replacement).unwrap();
    assert_eq!(
        active_binding(b.ledger(), "harness")
            .unwrap()
            .unwrap()
            .implementation,
        "adapter-2"
    );

    // ...but the pending operation reconciles against its *pinned* binding.
    let journal = EffectJournal::in_run_dir(&dir_b, "adapter-replacement-b");
    journal.record(op.as_str(), "execute", "applied").unwrap();
    let (handler, seen) =
        BindingRecordingHandler::new(def.clone(), RecordingActivity::new(journal.clone()));
    let mut registry = ActivityRegistry::new();
    registry.register(Box::new(handler)).unwrap();

    let result = run_activity(&mut b, &mut registry, &exec(), &request(&def)).unwrap();
    match result {
        OperationResult::Completed {
            operation_id,
            reconciled,
            duplicate,
            ..
        } => {
            assert_eq!(operation_id, op);
            assert!(reconciled, "an already-present effect is reconciled");
            assert!(!duplicate);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(
        seen.borrow().clone(),
        vec![Some(binding_label(&pin_seed))],
        "reconcile must use the pinned binding, never the configured adapter"
    );
    // Receipt is unchanged and the same consumer APIs still work.
    let receipt = b.receipt(&op).unwrap().unwrap();
    assert_eq!(receipt.operation_id, op);
    assert_ne!(b.load(&exec()).unwrap().state, ExecState::Parked);
}

// ---------------------------------------------------------------------------
// Staged activation
// ---------------------------------------------------------------------------

fn two_bindings() -> (BindingRecord, BindingRecord) {
    (
        binding("binding:seed", "seed", BindingState::Active),
        binding("binding:candidate", "candidate", BindingState::Active),
    )
}

#[test]
fn staged_activation_happy_path_settles_present_effect() {
    let def = build_def();
    let (seed, candidate) = two_bindings();
    let pin_seed = BindingPin::from_record(&seed);
    let (dir, ledger) = harness::open_isolated_ledger("activation-happy");
    let mut engine = KernelEngine::open(ledger).unwrap();
    engine.ledger_mut().upsert_binding(&seed).unwrap();
    start(&mut engine);
    let op = announce_build(&mut engine, &def, &pin_seed);

    // A present in-flight effect is settled (reconciled) during activation.
    let journal = EffectJournal::in_run_dir(&dir, "activation-happy");
    journal.record(op.as_str(), "execute", "applied").unwrap();
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();

    let report = staged_activate(&mut engine, &mut registry, &candidate, 10).unwrap();
    assert_eq!(
        report.steps,
        vec![
            agalma_kernel::ActivationStep::SetLatch,
            agalma_kernel::ActivationStep::SettleInFlight,
            agalma_kernel::ActivationStep::StageCandidate,
            agalma_kernel::ActivationStep::SwapBinding,
            agalma_kernel::ActivationStep::SchemaCheck,
            agalma_kernel::ActivationStep::ClearLatch,
        ]
    );
    assert_eq!(report.settled_operations, vec![op.to_string()]);
    assert_eq!(report.previous_binding.as_deref(), Some("binding:seed"));
    assert_eq!(report.active_binding.as_deref(), Some("binding:candidate"));

    // The candidate is active; the old binding drains but is retained.
    let bindings = engine.ledger().bindings().unwrap();
    let by_id = |id: &str| {
        bindings
            .iter()
            .find(|b| b.binding_id.as_str() == id)
            .cloned()
    };
    assert_eq!(
        by_id("binding:candidate").unwrap().state,
        BindingState::Active
    );
    assert_eq!(by_id("binding:seed").unwrap().state, BindingState::Draining);
    assert!(
        !engine.ledger().kill_latch().unwrap(),
        "latch cleared on resume"
    );
    assert!(
        engine.pending_dispatch().unwrap().is_empty(),
        "effect settled"
    );
    assert!(engine
        .ledger()
        .kernel_meta_get(&format!("{ACTIVATION_EVIDENCE_PREFIX}harness"))
        .unwrap()
        .is_some());
}

#[test]
fn staged_activation_refuses_unmet_capabilities() {
    let def = build_def();
    let (seed, mut candidate) = two_bindings();
    candidate
        .required_capabilities
        .insert("cap:not-advertised".to_string());
    let pin_seed = BindingPin::from_record(&seed);
    let (_dir, ledger) = harness::open_isolated_ledger("activation-capgate");
    let mut engine = KernelEngine::open(ledger).unwrap();
    engine.ledger_mut().upsert_binding(&seed).unwrap();
    start(&mut engine);
    let _ = announce_build(&mut engine, &def, &pin_seed);
    let mut registry = ActivityRegistry::new();

    let err = staged_activate(&mut engine, &mut registry, &candidate, 10).unwrap_err();
    assert!(
        matches!(err, ContractError::UnsupportedCapability(_)),
        "got {err:?}"
    );
    // Nothing was swapped; the latch was never set.
    assert_eq!(
        active_binding(engine.ledger(), "harness")
            .unwrap()
            .unwrap()
            .binding_id
            .as_str(),
        "binding:seed"
    );
    assert!(!engine.ledger().kill_latch().unwrap());
}

// ---------------------------------------------------------------------------
// Fixture case 10 — rollback / restore reconcile
// ---------------------------------------------------------------------------

/// A scripted restore seam: all domains reconcile except an optional ambiguous
/// one.
struct ScriptedReconciler {
    ambiguous: Option<RestoreDomain>,
    calls: Vec<String>,
}

impl ScriptedReconciler {
    fn all_reconciled() -> Self {
        ScriptedReconciler {
            ambiguous: None,
            calls: Vec::new(),
        }
    }

    fn ambiguous_at(domain: RestoreDomain) -> Self {
        ScriptedReconciler {
            ambiguous: Some(domain),
            calls: Vec::new(),
        }
    }
}

impl RestoreReconciler for ScriptedReconciler {
    fn reconcile(
        &mut self,
        domain: RestoreDomain,
        _execution: &ExecutionId,
    ) -> Result<ReconcileOutcome, ContractError> {
        self.calls.push(domain.as_str().to_string());
        if self.ambiguous == Some(domain) {
            Ok(ReconcileOutcome::Ambiguous {
                detail: format!("{} state could not be established", domain.as_str()),
            })
        } else {
            Ok(ReconcileOutcome::Reconciled {
                detail: format!("{} reconciled", domain.as_str()),
            })
        }
    }
}

#[test]
fn rollback_restore_reconciles_before_dispatch() {
    let def = build_def();
    let (seed, candidate) = two_bindings();
    let pin_seed = BindingPin::from_record(&seed);
    let (dir, ledger) = harness::open_isolated_ledger("rollback-restore");
    let mut engine = KernelEngine::open(ledger).unwrap();
    engine.ledger_mut().upsert_binding(&seed).unwrap();
    start(&mut engine);
    let op = announce_build(&mut engine, &def, &pin_seed);
    let mut registry = ActivityRegistry::new();
    // The effect is absent; activation leaves the intent pending.
    let journal = EffectJournal::in_run_dir(&dir, "rollback-restore");
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();

    // Activate the candidate, then roll back to the seed.
    staged_activate(&mut engine, &mut registry, &candidate, 10).unwrap();
    let rollback_report = rollback(&mut engine, &mut registry, &seed, 11).unwrap();
    assert_eq!(
        rollback_report.active_binding.as_deref(),
        Some("binding:seed")
    );
    assert_eq!(
        rollback_report.candidate_binding.as_deref(),
        Some("binding:candidate")
    );
    let bindings = engine.ledger().bindings().unwrap();
    let state_of = |id: &str| {
        bindings
            .iter()
            .find(|b| b.binding_id.as_str() == id)
            .map(|b| b.state)
            .expect("binding present")
    };
    assert_eq!(state_of("binding:seed"), BindingState::Active);
    assert_eq!(
        state_of("binding:candidate"),
        BindingState::Retired,
        "rollback retains the previous binding for reconciliation"
    );
    // Pending work kept its pinned binding through the rollback.
    let pending = engine.pending_dispatch().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].operation_id, op);

    // Restore: schema checked, all domains reconciled, latch cleared.
    let mut ok = ScriptedReconciler::all_reconciled();
    let restored = restore(&mut engine, &mut ok, &exec(), 12).unwrap();
    assert_eq!(restored.schema_version, 5);
    assert!(!restored.parked);
    assert_eq!(restored.outcomes.len(), 4);
    assert_eq!(
        ok.calls,
        vec!["ledger", "bd", "git", "processes"],
        "reconcile order is fixed"
    );
    assert!(!engine.ledger().kill_latch().unwrap());
    assert!(engine
        .ledger()
        .kernel_meta_get(&format!("{RESTORE_EVIDENCE_PREFIX}{}", exec().as_str()))
        .unwrap()
        .is_some());

    // An ambiguous Git reconciliation parks and keeps the latch set: nothing
    // dispatches on ambiguous external state.
    let mut ambiguous = ScriptedReconciler::ambiguous_at(RestoreDomain::Git);
    let parked = restore(&mut engine, &mut ambiguous, &exec(), 13).unwrap();
    assert!(parked.parked, "ambiguous domain parks");
    assert_eq!(
        parked.outcomes,
        vec![
            ("ledger".to_string(), true),
            ("bd".to_string(), true),
            ("git".to_string(), false),
        ]
    );
    assert_eq!(parked.outcomes.len(), 3, "stopped at the ambiguous domain");
    assert!(engine.ledger().kill_latch().unwrap(), "latch stays set");
    assert_eq!(engine.load(&exec()).unwrap().state, ExecState::Parked);
    assert_eq!(
        engine.pending_dispatch().unwrap().len(),
        1,
        "no work was consumed while parked"
    );
}
