//! Kernel `ExecutionApi`/`ExecutionOps` adapter tests (B1.7).
//!
//! These live as an integration test (not `src/` unit tests) because they use
//! the `agalma-ledger` dev-dependency: the architecture lint forbids an
//! implementation crate's `src/` from referencing another implementation crate,
//! even in test modules. Integration tests are out of scope (dev-deps wire
//! implementations together legitimately).

use agalma_contracts::{
    ArtifactRef, ContractError, ExecutionApi, ExecutionId, ExecutionPhase, ExecutionState,
    OperationId, StepOutcome, TaskId,
};
use agalma_ipc::{register_execution, EffectMode, EffectReport, ExecutionOps, Server, StepBegin};
use agalma_kernel::server::{server_info, IMPL_NAME};
use agalma_kernel::{
    operation_input_hash, ActivityDef, Command, ExecState, Inputs, KernelEngine, OperationRequest,
    Probe,
};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;
use serde_json::json;

fn run_dir(case: &str) -> ScratchDir {
    ScratchDir::new(&format!("exec-api-{case}"))
}

/// The adapter must satisfy the server-hosting seam `ExecutionApi + Send`.
#[test]
fn kernel_engine_is_a_send_execution_api() {
    fn assert_seam<T: ExecutionApi + agalma_ipc::ExecutionOps + Send + 'static>() {}
    assert_seam::<KernelEngine<SqliteLedger>>();
}

#[test]
fn start_status_cancel_map_onto_engine_operations() {
    let dir = run_dir("start-status-cancel");
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let task = TaskId::new("fix-answer");

    let execution = engine.start(&task).unwrap();
    assert_eq!(execution, ExecutionId::derive(&task, 1));
    let status = engine.status(&execution).unwrap();
    assert_eq!(status.phase, ExecutionPhase::Intake);
    // Kernel `Start` sets `ExecState::Ready`, which coarsens to `Pending`.
    assert_eq!(status.state, ExecutionState::Pending);

    ExecutionApi::cancel(&mut engine, &execution).unwrap();
    let status = engine.status(&execution).unwrap();
    assert_eq!(status.state, ExecutionState::Failed);
    assert!(status.pending_operations.is_empty());
    assert_eq!(engine.load(&execution).unwrap().state, ExecState::Cancelled);

    // Unknown execution is a known failure (matches the seed executor).
    let unknown = ExecutionId::new("exec:nope:9");
    assert!(engine.status(&unknown).is_err());
}

#[test]
fn step_and_recover_report_unsupported_capability() {
    let dir = run_dir("unsupported");
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let execution = engine.start(&TaskId::new("fix-answer")).unwrap();
    assert!(matches!(
        engine.step(&execution),
        Err(ContractError::UnsupportedCapability(_))
    ));
    assert!(matches!(
        engine.recover(),
        Err(ContractError::UnsupportedCapability(_))
    ));
}

#[test]
fn server_registers_every_execution_method() {
    let dir = run_dir("handlers");
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let engine = KernelEngine::open(ledger).unwrap();
    let mut server = Server::new(server_info());
    register_execution(&mut server, engine);
    assert_eq!(server.info().impl_name, IMPL_NAME);
    agalma_ipc::execution_server::ensure_all_handlers(&server).expect("all handlers registered");
}

/// Announce an operation durably without running its effect.
fn announce(
    engine: &mut KernelEngine<SqliteLedger>,
    execution: &ExecutionId,
    def: &ActivityDef,
    operation_id: &OperationId,
    inputs: &serde_json::Value,
    now_unix_ms: u64,
) {
    engine
        .apply(
            execution,
            &Command::Deliver {
                operation_id: operation_id.clone(),
                inputs_hash: operation_input_hash(def, inputs),
            },
            &Inputs {
                probe: Probe::Absent,
                activity: Some(def.clone()),
                operation_inputs: Some(inputs.clone()),
                phase: Some(ExecutionPhase::Build),
                now_unix_ms,
                ..Inputs::default()
            },
        )
        .unwrap();
}

/// The handler-registry protocol reconciles a durable pending operation: the
/// engine owns begin/announce/commit; the caller supplies the effect report.
#[test]
fn handler_registry_protocol_reconciles_a_pending_operation() {
    let dir = run_dir("registry-pending");
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let task = TaskId::new("fix-answer");
    let execution = engine.start(&task).unwrap();
    let def = ActivityDef::new("fake/build", "v1");
    let inputs = json!({ "task": "fix-answer" });
    let op = OperationId::derive(&execution, "build");
    announce(&mut engine, &execution, &def, &op, &inputs, 2);

    // Begin hands the caller the pending context (reconcile mode).
    let begin = ExecutionOps::begin_pending_step(&engine, &execution, &op).unwrap();
    let StepBegin::Effect(ctx) = begin else {
        panic!("expected an effect, got {begin:?}")
    };
    assert_eq!(ctx.mode, EffectMode::Reconcile);
    assert_eq!(ctx.operation_id, op);
    assert_eq!(ctx.activity.name, "fake/build");
    assert_eq!(ctx.activity.version, "v1");
    assert_eq!(ExecutionOps::pending_dispatch(&engine).unwrap().len(), 1);

    // The caller found the effect present; complete it.
    let outcome = ExecutionOps::complete_step(
        &mut engine,
        &execution,
        &op,
        EffectReport::Present {
            outcome_ref: ArtifactRef::derive("build-output"),
        },
        3,
    )
    .unwrap();
    assert!(
        matches!(
            outcome,
            StepOutcome::Dispatched {
                reconciled: true,
                duplicate: false,
                ..
            }
        ),
        "present effect reconciles: {outcome:?}"
    );
    assert_eq!(ExecutionOps::pending_dispatch(&engine).unwrap().len(), 0);
    let receipt = engine.receipt(&op).unwrap().expect("receipt recorded");
    assert_eq!(receipt.kind, "operation");

    // A duplicate begin replays the recorded receipt rather than re-running.
    let again = ExecutionOps::begin_pending_step(&engine, &execution, &op).unwrap();
    assert!(
        matches!(
            again,
            StepBegin::Done(StepOutcome::Dispatched { duplicate: true, .. })
        ),
        "duplicate begin replays: {again:?}"
    );
}

/// `begin_step` dispatches a fresh operation: the intent is durable before the
/// caller runs the effect, and `complete_step` records the receipt.
#[test]
fn handler_registry_protocol_dispatches_a_fresh_operation() {
    let dir = run_dir("registry-dispatch");
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let execution = engine.start(&TaskId::new("fix-answer")).unwrap();
    let request = OperationRequest {
        activity: ActivityDef::new("fake/build", "v1"),
        step: "build".to_string(),
        phase: ExecutionPhase::Build,
        component_api: "harness".to_string(),
        inputs: json!({ "task": "fix-answer" }),
        now_unix_ms: 2,
    };
    let begin = ExecutionOps::begin_step(
        &mut engine,
        &execution,
        serde_json::to_value(&request).unwrap(),
    )
    .unwrap();
    let StepBegin::Effect(ctx) = begin else {
        panic!("expected an effect, got {begin:?}")
    };
    assert_eq!(ctx.mode, EffectMode::Dispatch);
    // The dispatch intent is durable before the effect runs.
    assert_eq!(ExecutionOps::pending_dispatch(&engine).unwrap().len(), 1);

    let outcome = ExecutionOps::complete_step(
        &mut engine,
        &execution,
        &ctx.operation_id,
        EffectReport::Executed {
            outcome_ref: ArtifactRef::derive("build-output"),
        },
        3,
    )
    .unwrap();
    assert!(
        matches!(
            outcome,
            StepOutcome::Dispatched {
                reconciled: false,
                duplicate: false,
                ..
            }
        ),
        "fresh dispatch: {outcome:?}"
    );
    assert!(engine.receipt(&ctx.operation_id).unwrap().is_some());
    assert_eq!(ExecutionOps::pending_dispatch(&engine).unwrap().len(), 0);
}

/// An ambiguous report parks the execution through the durable path.
#[test]
fn handler_registry_protocol_parks_on_ambiguous_effect() {
    let dir = run_dir("registry-ambiguous");
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let execution = engine.start(&TaskId::new("fix-answer")).unwrap();
    let def = ActivityDef::new("fake/build", "v1");
    let inputs = json!({ "task": "fix-answer" });
    let op = OperationId::derive(&execution, "build");
    announce(&mut engine, &execution, &def, &op, &inputs, 2);

    let outcome = ExecutionOps::complete_step(
        &mut engine,
        &execution,
        &op,
        EffectReport::Ambiguous,
        3,
    )
    .unwrap();
    assert!(
        matches!(outcome, StepOutcome::Parked { .. }),
        "ambiguous effect parks: {outcome:?}"
    );
    assert_eq!(engine.load(&execution).unwrap().state, ExecState::Parked);
    // Parked work is not settled: the intent is still pending for reconciliation.
    assert_eq!(ExecutionOps::pending_dispatch(&engine).unwrap().len(), 1);
}
