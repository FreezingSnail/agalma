//! Kernel `ExecutionApi`/`ExecutionOps` adapter tests (B1.7).
//!
//! These live as an integration test (not `src/` unit tests) because they use
//! the `agalma-ledger` dev-dependency: the architecture lint forbids an
//! implementation crate's `src/` from referencing another implementation crate,
//! even in test modules. Integration tests are out of scope (dev-deps wire
//! implementations together legitimately).

use agalma_contracts::{
    ContractError, ExecutionApi, ExecutionId, ExecutionPhase, ExecutionState, TaskId,
};
use agalma_ipc::{register_execution, Server};
use agalma_kernel::server::{server_info, IMPL_NAME};
use agalma_kernel::{ExecState, KernelEngine};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;

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
