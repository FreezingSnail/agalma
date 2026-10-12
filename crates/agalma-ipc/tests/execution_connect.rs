//! Execution/kernel boundary over the Connect transport (IPCX.7).
//!
//! The typed [`ExecutionApi`]/[`ExecutionOps`] client (`ExecutionClient`) is
//! exercised against a `register_execution` server served over both wires: the
//! B1 framed transport and the IPCX Connect/UDS transport. A deterministic stub
//! kernel drives a representative scenario — `start`/`status`/`cancel`, the
//! lease surface, `receipt`/`pending_dispatch`, and the caller-side
//! handler-registry protocol (`begin_pending_step`/`complete_step` driving
//! `ExecutionApi::step`, plus `recover`) — and the two wires must produce an
//! identical typed transcript, proving the execution/kernel boundary works over
//! Connect with parity to framed.
//!
//! The caller-side effect dispatch (`agalma-5de`/B1-F1) is part of the boundary
//! surface and must keep working over Connect: this test attaches a real
//! [`EffectDispatch`] registry and drives `step`/`recover` through it on both
//! wires.
//!
//! Servers bind under `target/test-runs/` — never `/tmp`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agalma_contracts::{
    ArtifactRef, ContractError, DispatchIntent, ExecutionApi, ExecutionId, ExecutionPhase,
    ExecutionState, ExecutionStatus, OperationId, OperationReceipt, StepOutcome, TaskId,
};
use agalma_ipc::execution_client::ExecutionOps;
use agalma_ipc::execution_server::{ensure_all_handlers, register_execution, serve_execution};
use agalma_ipc::server::{Server, ServerInfo};
use agalma_ipc::{
    EffectContext, EffectDispatch, EffectProbe as IpcProbe, EffectReport, ExecutionClient,
    HandlerError, HandlerKey, StepBegin, Wire,
};
use agalma_testkit::{retry_connect, ScratchDir};
use serde_json::{json, Value};

/// A deterministic in-memory kernel: one execution, one dispatch operation, no
/// clock, no ledger. Implements the tested surface faithfully; the methods not
/// reached by the parity transcript panic if called (never reached).
struct StubExecution {
    task: TaskId,
    execution: ExecutionId,
    operation: OperationId,
    leases: u32,
    pending: bool,
    state: ExecutionState,
    phase: ExecutionPhase,
}

impl StubExecution {
    fn new() -> StubExecution {
        let task = TaskId::new("stub-task");
        let execution = ExecutionId::derive(&task, 1);
        let operation = OperationId::derive(&execution, "build");
        StubExecution {
            task,
            execution,
            operation,
            leases: 0,
            pending: true,
            state: ExecutionState::Running,
            phase: ExecutionPhase::Build,
        }
    }

    /// The deterministic effect context handed to the caller for `operation`.
    fn context(&self, mode: agalma_ipc::EffectMode) -> EffectContext {
        EffectContext {
            execution_id: self.execution.clone(),
            operation_id: self.operation.clone(),
            task_id: self.task.clone(),
            phase: ExecutionPhase::Build,
            attempt: 1,
            activity: HandlerKey::new("stub/build", "v1"),
            binding: None,
            inputs: json!({ "task": "stub-task" }),
            now_unix_ms: 3,
            mode,
        }
    }
}

impl ExecutionApi for StubExecution {
    fn start(&mut self, task: &TaskId) -> Result<ExecutionId, ContractError> {
        self.task = task.clone();
        self.execution = ExecutionId::derive(task, 1);
        self.operation = OperationId::derive(&self.execution, "build");
        Ok(self.execution.clone())
    }

    fn step(&mut self, execution: &ExecutionId) -> Result<StepOutcome, ContractError> {
        assert_eq!(&self.execution, execution);
        Ok(StepOutcome::Idle { phase: self.phase })
    }

    fn recover(&mut self) -> Result<(), ContractError> {
        Ok(())
    }

    fn cancel(&mut self, execution: &ExecutionId) -> Result<(), ContractError> {
        assert_eq!(&self.execution, execution);
        self.state = ExecutionState::Failed;
        self.phase = ExecutionPhase::Done;
        Ok(())
    }

    fn status(&self, execution: &ExecutionId) -> Result<ExecutionStatus, ContractError> {
        assert_eq!(&self.execution, execution);
        Ok(ExecutionStatus {
            execution_id: self.execution.clone(),
            phase: self.phase,
            state: self.state,
            attempt: 1,
            pending_operations: if self.pending {
                vec![self.operation.clone()]
            } else {
                Vec::new()
            },
            parked_reason: None,
        })
    }
}

impl ExecutionOps for StubExecution {
    fn apply(
        &mut self,
        _execution_id: &ExecutionId,
        _command: Value,
        _inputs: Value,
    ) -> Result<Value, ContractError> {
        unimplemented!("stub kernel: apply unused")
    }

    fn load_state(&self, _execution_id: &ExecutionId) -> Result<Value, ContractError> {
        unimplemented!("stub kernel: load_state unused")
    }

    fn pending_dispatch(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        if self.pending {
            Ok(vec![DispatchIntent {
                operation_id: self.operation.clone(),
                execution_id: self.execution.clone(),
                payload: json!({ "step": "build" }),
                enqueued_at_unix_ms: 2,
                consumed: false,
            }])
        } else {
            Ok(Vec::new())
        }
    }

    fn receipt(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError> {
        if self.pending || operation_id != &self.operation {
            return Ok(None);
        }
        Ok(Some(OperationReceipt {
            operation_id: self.operation.clone(),
            execution_id: self.execution.clone(),
            kind: "operation".to_string(),
            inputs_hash: "hash:stub-v1".to_string(),
            result: json!({ "outcome": "artifact:stub-output" }),
            completed_at_unix_ms: 4,
        }))
    }

    fn fire_due_timers(&mut self, _now_unix_ms: u64) -> Result<Value, ContractError> {
        unimplemented!("stub kernel: fire_due_timers unused")
    }

    fn schedule_timer(
        &mut self,
        _timer: &agalma_contracts::TimerRecord,
    ) -> Result<bool, ContractError> {
        unimplemented!("stub kernel: schedule_timer unused")
    }

    fn due_timers(
        &self,
        _now_unix_ms: u64,
    ) -> Result<Vec<agalma_contracts::TimerRecord>, ContractError> {
        unimplemented!("stub kernel: due_timers unused")
    }

    fn begin_lease(&mut self, _task: &TaskId) -> Result<u32, ContractError> {
        self.leases += 1;
        Ok(self.leases)
    }

    fn lease_generation(&self, _task: &TaskId) -> Result<u32, ContractError> {
        Ok(self.leases)
    }

    fn reclaim(
        &mut self,
        _task: &TaskId,
        _execution_id: &ExecutionId,
        _now_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        unimplemented!("stub kernel: reclaim unused")
    }

    fn submit_directive(
        &mut self,
        _execution_id: &ExecutionId,
        _directive_id: &str,
        _version: u64,
        _plan: Value,
        _now_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        unimplemented!("stub kernel: submit_directive unused")
    }

    fn export_snapshot_to(&self, _path: &Path) -> Result<(), ContractError> {
        unimplemented!("stub kernel: export_snapshot_to unused")
    }

    fn import_snapshot_from(&mut self, _path: &Path) -> Result<(), ContractError> {
        unimplemented!("stub kernel: import_snapshot_from unused")
    }

    fn begin_step(
        &mut self,
        execution_id: &ExecutionId,
        _request: Value,
    ) -> Result<StepBegin, ContractError> {
        assert_eq!(&self.execution, execution_id);
        if self.pending {
            Ok(StepBegin::Effect(
                self.context(agalma_ipc::EffectMode::Reconcile),
            ))
        } else {
            Ok(StepBegin::Done(StepOutcome::Idle { phase: self.phase }))
        }
    }

    fn begin_pending_step(
        &self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
    ) -> Result<StepBegin, ContractError> {
        assert_eq!(&self.execution, execution_id);
        assert_eq!(&self.operation, operation_id);
        Ok(StepBegin::Effect(
            self.context(agalma_ipc::EffectMode::Reconcile),
        ))
    }

    fn complete_step(
        &mut self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
        report: EffectReport,
        _now_unix_ms: u64,
    ) -> Result<StepOutcome, ContractError> {
        assert_eq!(&self.execution, execution_id);
        assert_eq!(&self.operation, operation_id);
        assert!(
            matches!(report, EffectReport::Present { .. }),
            "the caller-side registry reports the present effect: {report:?}"
        );
        self.pending = false;
        Ok(StepOutcome::Dispatched {
            operation: self.operation.clone(),
            reconciled: true,
            duplicate: false,
        })
    }
}

fn execution_info() -> ServerInfo {
    ServerInfo {
        api: "ExecutionApi".to_string(),
        api_version: 0,
        impl_name: "stub".to_string(),
        impl_version: "stub/v1".to_string(),
        capabilities: BTreeSet::new(),
    }
}

fn spawn_execution_server(socket: PathBuf, wire: Wire) {
    let mut server = Server::new(execution_info());
    register_execution(&mut server, StubExecution::new());
    ensure_all_handlers(&server).expect("every execution method registered");
    std::thread::spawn(move || {
        let _ = serve_execution(server, &socket, wire);
    });
}

/// Poll until the server has bound the socket (the listener is created before
/// the `READY` line, so file existence implies it is accepting).
fn wait_for_socket(socket: &Path) {
    for _ in 0..500 {
        if socket.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("server never bound {}", socket.display());
}

/// Caller-side dispatch double: a fixed present effect (the `agalma-5de`
/// registry half that runs in the caller's process).
struct PresentDispatch {
    key: HandlerKey,
    outcome_ref: ArtifactRef,
}

impl EffectDispatch for PresentDispatch {
    fn contains(&self, activity: &HandlerKey) -> bool {
        activity == &self.key
    }

    fn probe(&mut self, _ctx: &EffectContext) -> Result<IpcProbe, HandlerError> {
        Ok(IpcProbe::Present(self.outcome_ref.clone()))
    }

    fn execute(&mut self, _ctx: &EffectContext) -> Result<ArtifactRef, HandlerError> {
        Ok(self.outcome_ref.clone())
    }
}

/// Drive the stub through the tested execution/kernel surface over a wire and
/// render a timing-free transcript.
fn run_scenario(client: &mut ExecutionClient) -> Vec<String> {
    let task = TaskId::new("stub-task");
    let execution = ExecutionId::derive(&task, 1);
    let operation = OperationId::derive(&execution, "build");

    let started = client.start(&task).expect("start");
    let status_start = client.status(&execution).expect("status start");

    // Lease surface.
    let lease = client.begin_lease(&task).expect("begin_lease");
    let lease_read = client.lease_generation(&task).expect("lease_generation");

    // Handler-registry protocol: `step` drives begin_pending_step → the
    // caller-side registry performs the effect → complete_step.
    let step = client.step(&execution).expect("step");
    let pending_after_step = client.pending_dispatch().expect("pending after step").len();
    let receipt = client
        .receipt(&operation)
        .expect("receipt")
        .expect("receipt recorded");

    // `recover` reconciles every durable pending intent (none left here).
    client.recover().expect("recover");

    client.cancel(&execution).expect("cancel");
    let status_cancelled = client.status(&execution).expect("status cancelled");

    vec![
        format!("started={started}"),
        format!(
            "status_start={:?}/{:?}",
            status_start.phase, status_start.state
        ),
        format!("lease={lease}/{lease_read}"),
        format!("step={step:?}"),
        format!("pending_after_step={pending_after_step}"),
        format!("receipt={}/{}", receipt.kind, receipt.inputs_hash),
        format!(
            "status_cancelled={:?}/{:?}",
            status_cancelled.phase, status_cancelled.state
        ),
    ]
}

#[test]
fn execution_api_over_connect_matches_framed() {
    let dir = ScratchDir::new("ipcx-exec");
    let framed_socket = dir.join("framed.sock");
    let connect_socket = dir.join("connect.sock");

    spawn_execution_server(framed_socket.clone(), Wire::Framed);
    spawn_execution_server(connect_socket.clone(), Wire::Connect);
    wait_for_socket(&framed_socket);
    wait_for_socket(&connect_socket);

    let mut framed = retry_connect(|| ExecutionClient::connect_wire(&framed_socket, Wire::Framed))
        .expect("framed connect")
        .with_effect_dispatch(Box::new(PresentDispatch {
            key: HandlerKey::new("stub/build", "v1"),
            outcome_ref: ArtifactRef::derive("stub-output"),
        }));
    let mut connect =
        retry_connect(|| ExecutionClient::connect_wire(&connect_socket, Wire::Connect))
            .expect("connect connect")
            .with_effect_dispatch(Box::new(PresentDispatch {
                key: HandlerKey::new("stub/build", "v1"),
                outcome_ref: ArtifactRef::derive("stub-output"),
            }));

    // Every typed call — including the caller-side effect-dispatch protocol —
    // produces the same transcript on both wires.
    assert_eq!(run_scenario(&mut framed), run_scenario(&mut connect));
}

#[test]
fn execution_connect_missing_socket_is_unavailable() {
    let dir = ScratchDir::new("ipcx-exec-miss");
    let err = ExecutionClient::connect_wire(&dir.join("nope.sock"), Wire::Connect)
        .err()
        .expect("connect to a missing socket must fail");
    assert!(err.is_unavailable(), "expected Unavailable, got {err:?}");
}

/// The method surface stays consistent: 21 methods including the three
/// handler-registry operations folded in by `agalma-5de`.
#[test]
fn execution_methods_include_handler_registry_ops() {
    use agalma_ipc::EXECUTION_METHODS;
    assert_eq!(EXECUTION_METHODS.len(), 21);
    assert!(EXECUTION_METHODS.contains(&"begin_step"));
    assert!(EXECUTION_METHODS.contains(&"begin_pending_step"));
    assert!(EXECUTION_METHODS.contains(&"complete_step"));
}
