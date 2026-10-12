//! `ExecutionClient` (B1.7): the kernel-candidate [`ExecutionApi`] over the
//! framed client, plus the kernel-specific engine surface ([`ExecutionOps`])
//! needed to drive the kernel behind the process boundary.
//!
//! ## Boundary
//!
//! The M1 seed executor (`Executor<L>`) hosts `Box<dyn Activity>` closures built
//! from conductor code and cannot cross a process boundary without
//! re-architecting phases. The process-ready candidate is the B0 durable kernel
//! (`KernelEngine<L>`), pure data-in/data-out over `LedgerApi` with a
//! `Box<dyn SandboxApi + Send>` seam. This client is the `ExecutionApi` half of
//! that candidate boundary; the engine-op half is [`ExecutionOps`].
//!
//! ## Faithfulness and gaps
//!
//! [`ExecutionApi::start`]/`cancel`/`status` are serviced by the kernel adapter.
//! [`ExecutionApi::step`]/`recover` need an activity handler registry (external
//! effects) that cannot be hosted generically behind the boundary; the kernel
//! returns [`ContractError::UnsupportedCapability`] for them. Consumers drive
//! the engine-op surface ([`ExecutionOps`]) instead; the gap is recorded in
//! `docs/b1-execution-evidence.md`.
//!
//! ## JSON engine surface
//!
//! Kernel-native payloads (`Command`/`Inputs`/`ApplyOutcome`/`KernelState`/
//! `TimerFire`/`Reclaim`) cross as JSON (params/result `Value`s) because
//! `agalma-ipc` is infrastructure and must not depend on `agalma-kernel` (that
//! edge would cycle once the kernel server depends on `agalma-ipc`).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;

use agalma_contracts::{
    ContractError, DispatchIntent, ExecutionApi, ExecutionId, ExecutionState, ExecutionStatus,
    OperationId, OperationReceipt, StepOutcome, TaskId, TimerRecord,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::error::IpcError;
use crate::execution_handler::{
    EffectContext, EffectDispatch, EffectMode, EffectProbe, EffectReport, HandlerError, StepBegin,
};

/// Params for `start`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartCall {
    pub task: TaskId,
}

/// Params for the execution-scoped methods (`step`, `cancel`, `status`,
/// `load_state`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecIdCall {
    pub execution_id: ExecutionId,
}

/// Params for `apply`: a kernel command + recorded inputs (both JSON-encoded
/// `agalma_kernel::{Command, Inputs}`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KernelCall {
    pub execution_id: ExecutionId,
    pub command: Value,
    pub inputs: Value,
}

/// Params for `receipt`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiptCall {
    pub operation_id: OperationId,
}

/// Params for the wall-clock-scoped methods (`fire_due_timers`, `due_timers`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NowCall {
    pub now_unix_ms: u64,
}

/// Params for `schedule_timer`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimerExecParams {
    pub timer: TimerRecord,
}

/// Params for the task-scoped lease methods (`begin_lease`, `lease_generation`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaseExecCall {
    pub task: TaskId,
}

/// Params for `reclaim`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReclaimCall {
    pub task: TaskId,
    pub execution_id: ExecutionId,
    pub now_unix_ms: u64,
}

/// Params for `submit_directive` (`plan` is a JSON `agalma_kernel::DirectivePlan`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectiveExecCall {
    pub execution_id: ExecutionId,
    pub directive_id: String,
    pub version: u64,
    pub plan: Value,
    pub now_unix_ms: u64,
}

/// Params for the snapshot file-transfer primitives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotPathCall {
    pub path: String,
}

/// Params for `begin_pending_step`: the durable operation to reconcile.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingStepCall {
    pub execution_id: ExecutionId,
    pub operation_id: OperationId,
}

/// Params for `begin_step`: a full dispatch/reconcile request (`request` is a
/// JSON `agalma_kernel::OperationRequest`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BeginStepCall {
    pub execution_id: ExecutionId,
    pub request: Value,
}

/// Params for `complete_step`: the caller's effect report.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompleteStepCall {
    pub execution_id: ExecutionId,
    pub operation_id: OperationId,
    pub report: EffectReport,
    pub now_unix_ms: u64,
}

/// The full kernel surface a kernel server hosts: the frozen [`ExecutionApi`]
/// plus the kernel-specific engine operations the `ExecutionApi` adapter cannot
/// express (dispatch/recovery are handler-driven and stay caller-side).
///
/// `KernelEngine<L>` implements this directly (in-process); [`ExecutionClient`]
/// implements it over the transport. This mirrors the `LedgerOps`/`TaskQueueOps`
/// precedent, with kernel-native payloads carried as JSON.
pub trait ExecutionOps: ExecutionApi {
    /// Apply one pure transition command with its recorded inputs; returns the
    /// JSON-encoded `agalma_kernel::ApplyOutcome`.
    fn apply(
        &mut self,
        execution_id: &ExecutionId,
        command: Value,
        inputs: Value,
    ) -> Result<Value, ContractError>;

    /// Reconstruct one execution's state (JSON-encoded `agalma_kernel::KernelState`).
    fn load_state(&self, execution_id: &ExecutionId) -> Result<Value, ContractError>;

    /// Durable (unconsumed) dispatch intents — the recovery work list.
    fn pending_dispatch(&self) -> Result<Vec<DispatchIntent>, ContractError>;

    /// Recorded receipt for a completed operation (idempotency anchor).
    fn receipt(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError>;

    /// Fire every overdue timer, returning the JSON-encoded `Vec<TimerFire>`.
    fn fire_due_timers(&mut self, now_unix_ms: u64) -> Result<Value, ContractError>;

    /// Schedule a durable retry timer or deadline.
    fn schedule_timer(&mut self, timer: &TimerRecord) -> Result<bool, ContractError>;

    /// Pending timers due at or before `now_unix_ms`.
    fn due_timers(&self, now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError>;

    /// Begin a new claim cycle for `task`; returns the lease generation.
    fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError>;

    /// The task's current lease generation (`0` when never claimed).
    fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError>;

    /// Reclaim `task`, returning the JSON-encoded `agalma_kernel::Reclaim`.
    fn reclaim(
        &mut self,
        task: &TaskId,
        execution_id: &ExecutionId,
        now_unix_ms: u64,
    ) -> Result<Value, ContractError>;

    /// Persist and apply a directive plan; returns the JSON-encoded
    /// `agalma_kernel::ApplyOutcome`.
    fn submit_directive(
        &mut self,
        execution_id: &ExecutionId,
        directive_id: &str,
        version: u64,
        plan: Value,
        now_unix_ms: u64,
    ) -> Result<Value, ContractError>;

    /// Write the canonical state snapshot to `path` (server-side; D3 file
    /// transfer — only the path crosses the wire, never the snapshot payload).
    fn export_snapshot_to(&self, path: &Path) -> Result<(), ContractError>;

    /// Import the canonical state snapshot from `path` (server-side).
    fn import_snapshot_from(&mut self, path: &Path) -> Result<(), ContractError>;

    // -----------------------------------------------------------------------
    // Handler-registry protocol (B1-F1): caller-side effect dispatch
    // -----------------------------------------------------------------------

    /// Begin an operation from an explicit request (`request` is a JSON
    /// `agalma_kernel::OperationRequest`): announce it durably (dispatch mode) or
    /// return the pending context (reconcile mode), then hand the caller an
    /// [`StepBegin`].
    fn begin_step(
        &mut self,
        execution_id: &ExecutionId,
        request: Value,
    ) -> Result<StepBegin, ContractError>;

    /// Begin reconciling the durable pending operation `operation_id`.
    fn begin_pending_step(
        &self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
    ) -> Result<StepBegin, ContractError>;

    /// Complete an operation the caller performed; returns its [`StepOutcome`].
    fn complete_step(
        &mut self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
        report: EffectReport,
        now_unix_ms: u64,
    ) -> Result<StepOutcome, ContractError>;
}

/// A [`ExecutionApi`]/[`ExecutionOps`] implementation backed by a kernel
/// component server.
///
/// With no [`EffectDispatch`] attached, `step`/`recover` forward to the kernel
/// server's `ExecutionApi` adapter (which reports `UnsupportedCapability` —
/// effects cannot be hosted generically). Attach one with
/// [`ExecutionClient::with_effect_dispatch`] to drive the handler-registry
/// protocol: the client performs effects through its local registry and the
/// server owns the durable announce/commit.
pub struct ExecutionClient {
    client: RefCell<Client>,
    dispatch: Option<Box<dyn EffectDispatch>>,
}

impl ExecutionClient {
    /// Connect to the kernel server listening on `socket`.
    pub fn connect(socket: &Path) -> Result<ExecutionClient, IpcError> {
        Ok(ExecutionClient {
            client: RefCell::new(Client::connect(socket)?),
            dispatch: None,
        })
    }

    /// Build a client from an already-connected transport (used by the process
    /// supervisor after the `READY` handshake).
    pub fn from_client(client: Client) -> ExecutionClient {
        ExecutionClient {
            client: RefCell::new(client),
            dispatch: None,
        }
    }

    /// Attach a caller-side handler registry, enabling faithful `step`/`recover`
    /// across the boundary.
    pub fn with_effect_dispatch(mut self, dispatch: Box<dyn EffectDispatch>) -> Self {
        self.dispatch = Some(dispatch);
        self
    }

    /// Whether a handler registry is attached.
    pub fn has_effect_dispatch(&self) -> bool {
        self.dispatch.is_some()
    }

    fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: &impl Serialize,
    ) -> Result<R, ContractError> {
        let mut client = self.client.borrow_mut();
        client.call_typed(method, params).map_err(map_ipc_error)
    }

    /// Perform a begun effect through the caller registry and complete it.
    ///
    /// Returns `Ok(None)` when `reconcile_absent_is_noop` is set and a reconcile
    /// probe reports the effect absent: the operation is left pending for a later
    /// dispatch (the `recover_pending` semantics). Otherwise the settle outcome.
    fn settle(
        &mut self,
        ctx: &EffectContext,
        reconcile_absent_is_noop: bool,
    ) -> Result<Option<StepOutcome>, ContractError> {
        let mut dispatch = self
            .dispatch
            .take()
            .expect("settle requires an attached effect dispatch");
        let report = match ctx.mode {
            EffectMode::Dispatch => match dispatch.execute(ctx) {
                Ok(outcome_ref) => EffectReport::Executed { outcome_ref },
                Err(HandlerError::KnownFailure(message)) => EffectReport::KnownFailure { message },
                Err(HandlerError::UnknownOutcome(message)) => {
                    EffectReport::UnknownOutcome { message }
                }
            },
            EffectMode::Reconcile => match dispatch.probe(ctx) {
                Ok(EffectProbe::Present(outcome_ref)) => EffectReport::Present { outcome_ref },
                Ok(EffectProbe::Absent) => {
                    if reconcile_absent_is_noop {
                        self.dispatch = Some(dispatch);
                        return Ok(None);
                    }
                    match dispatch.execute(ctx) {
                        Ok(outcome_ref) => EffectReport::Executed { outcome_ref },
                        Err(HandlerError::KnownFailure(message)) => {
                            EffectReport::KnownFailure { message }
                        }
                        Err(HandlerError::UnknownOutcome(message)) => {
                            EffectReport::UnknownOutcome { message }
                        }
                    }
                }
                Ok(EffectProbe::Ambiguous) => EffectReport::Ambiguous,
                Err(HandlerError::KnownFailure(message)) => EffectReport::KnownFailure { message },
                Err(HandlerError::UnknownOutcome(message)) => {
                    EffectReport::UnknownOutcome { message }
                }
            },
        };
        self.dispatch = Some(dispatch);
        let outcome = self.call(
            "complete_step",
            &CompleteStepCall {
                execution_id: ctx.execution_id.clone(),
                operation_id: ctx.operation_id.clone(),
                report,
                now_unix_ms: ctx.now_unix_ms,
            },
        )?;
        Ok(Some(outcome))
    }
}

/// Map a transport error onto the frozen [`ContractError`] surface.
///
/// `IpcError::Contract` passes through unchanged (including `UnknownOutcome`).
/// Transport-only faults have no contract variant: `Unavailable`/`Protocol`
/// become `KnownFailure` (retry-safe or caller-visible framing faults).
pub fn map_ipc_error(err: IpcError) -> ContractError {
    match err {
        IpcError::Contract(error) => error,
        IpcError::Unavailable(message) => {
            ContractError::KnownFailure(format!("execution transport unavailable: {message}"))
        }
        IpcError::Protocol(message) => {
            ContractError::KnownFailure(format!("execution protocol violation: {message}"))
        }
    }
}

impl ExecutionApi for ExecutionClient {
    fn start(&mut self, task: &TaskId) -> Result<ExecutionId, ContractError> {
        self.call("start", &StartCall { task: task.clone() })
    }

    fn step(&mut self, execution: &ExecutionId) -> Result<StepOutcome, ContractError> {
        if self.dispatch.is_none() {
            // No caller registry: the server adapter reports the capability gap.
            return self.call(
                "step",
                &ExecIdCall {
                    execution_id: execution.clone(),
                },
            );
        }
        // Drive the handler-registry protocol for the durable pending operation,
        // if any; otherwise project idle/parked from status.
        let pending = self.pending_dispatch()?;
        let operation = pending
            .into_iter()
            .find(|intent| &intent.execution_id == execution)
            .map(|intent| intent.operation_id);
        let Some(operation) = operation else {
            let status = self.status(execution)?;
            return Ok(match status.state {
                ExecutionState::Parked => StepOutcome::Parked {
                    reason: status.parked_reason.unwrap_or_else(|| "parked".to_string()),
                },
                _ => StepOutcome::Idle {
                    phase: status.phase,
                },
            });
        };
        let begin: StepBegin = self.call(
            "begin_pending_step",
            &PendingStepCall {
                execution_id: execution.clone(),
                operation_id: operation,
            },
        )?;
        match begin {
            StepBegin::Done(outcome) => Ok(outcome),
            StepBegin::Effect(ctx) => {
                if !self
                    .dispatch
                    .as_ref()
                    .expect("registry")
                    .contains(&ctx.activity)
                {
                    return Err(ContractError::UnsupportedCapability(format!(
                        "no handler registered for activity {}",
                        ctx.activity.key()
                    )));
                }
                self.settle(&ctx, false)?
                    .ok_or_else(|| ContractError::KnownFailure("step did not settle".to_string()))
            }
        }
    }

    fn recover(&mut self) -> Result<(), ContractError> {
        if self.dispatch.is_none() {
            self.call::<()>("recover", &serde_json::json!({}))?;
            return Ok(());
        }
        // Reconcile every durable pending intent (the `recover_pending` loop):
        // a park/block halts further recovery for that execution; an absent
        // effect is left pending (never re-executed here).
        let mut halted: BTreeSet<String> = BTreeSet::new();
        for intent in self.pending_dispatch()? {
            let exec_key = intent.execution_id.as_str().to_string();
            if halted.contains(&exec_key) {
                continue;
            }
            let begin: StepBegin = self.call(
                "begin_pending_step",
                &PendingStepCall {
                    execution_id: intent.execution_id.clone(),
                    operation_id: intent.operation_id.clone(),
                },
            )?;
            match begin {
                StepBegin::Done(StepOutcome::Parked { .. })
                | StepBegin::Done(StepOutcome::Blocked { .. }) => {
                    halted.insert(exec_key);
                }
                StepBegin::Done(_) => {}
                StepBegin::Effect(ctx) => {
                    if self
                        .dispatch
                        .as_ref()
                        .expect("registry")
                        .contains(&ctx.activity)
                    {
                        // Recover leaves an absent effect pending.
                        self.settle(&ctx, true)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn cancel(&mut self, execution: &ExecutionId) -> Result<(), ContractError> {
        self.call(
            "cancel",
            &ExecIdCall {
                execution_id: execution.clone(),
            },
        )
    }

    fn status(&self, execution: &ExecutionId) -> Result<ExecutionStatus, ContractError> {
        self.call(
            "status",
            &ExecIdCall {
                execution_id: execution.clone(),
            },
        )
    }
}

impl ExecutionOps for ExecutionClient {
    fn apply(
        &mut self,
        execution_id: &ExecutionId,
        command: Value,
        inputs: Value,
    ) -> Result<Value, ContractError> {
        self.call(
            "apply",
            &KernelCall {
                execution_id: execution_id.clone(),
                command,
                inputs,
            },
        )
    }

    fn load_state(&self, execution_id: &ExecutionId) -> Result<Value, ContractError> {
        self.call(
            "load_state",
            &ExecIdCall {
                execution_id: execution_id.clone(),
            },
        )
    }

    fn pending_dispatch(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        self.call("pending_dispatch", &serde_json::json!({}))
    }

    fn receipt(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError> {
        self.call(
            "receipt",
            &ReceiptCall {
                operation_id: operation_id.clone(),
            },
        )
    }

    fn fire_due_timers(&mut self, now_unix_ms: u64) -> Result<Value, ContractError> {
        self.call("fire_due_timers", &NowCall { now_unix_ms })
    }

    fn schedule_timer(&mut self, timer: &TimerRecord) -> Result<bool, ContractError> {
        self.call(
            "schedule_timer",
            &TimerExecParams {
                timer: timer.clone(),
            },
        )
    }

    fn due_timers(&self, now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError> {
        self.call("due_timers", &NowCall { now_unix_ms })
    }

    fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError> {
        self.call("begin_lease", &LeaseExecCall { task: task.clone() })
    }

    fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError> {
        self.call("lease_generation", &LeaseExecCall { task: task.clone() })
    }

    fn reclaim(
        &mut self,
        task: &TaskId,
        execution_id: &ExecutionId,
        now_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        self.call(
            "reclaim",
            &ReclaimCall {
                task: task.clone(),
                execution_id: execution_id.clone(),
                now_unix_ms,
            },
        )
    }

    fn submit_directive(
        &mut self,
        execution_id: &ExecutionId,
        directive_id: &str,
        version: u64,
        plan: Value,
        now_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        self.call(
            "submit_directive",
            &DirectiveExecCall {
                execution_id: execution_id.clone(),
                directive_id: directive_id.to_string(),
                version,
                plan,
                now_unix_ms,
            },
        )
    }

    fn export_snapshot_to(&self, path: &Path) -> Result<(), ContractError> {
        self.call::<()>(
            "export_snapshot_to",
            &SnapshotPathCall {
                path: path.to_string_lossy().into_owned(),
            },
        )
    }

    fn import_snapshot_from(&mut self, path: &Path) -> Result<(), ContractError> {
        self.call::<()>(
            "import_snapshot_from",
            &SnapshotPathCall {
                path: path.to_string_lossy().into_owned(),
            },
        )
    }

    fn begin_step(
        &mut self,
        execution_id: &ExecutionId,
        request: Value,
    ) -> Result<StepBegin, ContractError> {
        self.call(
            "begin_step",
            &BeginStepCall {
                execution_id: execution_id.clone(),
                request,
            },
        )
    }

    fn begin_pending_step(
        &self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
    ) -> Result<StepBegin, ContractError> {
        self.call(
            "begin_pending_step",
            &PendingStepCall {
                execution_id: execution_id.clone(),
                operation_id: operation_id.clone(),
            },
        )
    }

    fn complete_step(
        &mut self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
        report: EffectReport,
        now_unix_ms: u64,
    ) -> Result<StepOutcome, ContractError> {
        self.call(
            "complete_step",
            &CompleteStepCall {
                execution_id: execution_id.clone(),
                operation_id: operation_id.clone(),
                report,
                now_unix_ms,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_error_passes_through() {
        let err = map_ipc_error(IpcError::Contract(ContractError::Conflict("x".into())));
        assert!(matches!(err, ContractError::Conflict(_)));
    }

    #[test]
    fn unknown_outcome_passes_through() {
        let err = map_ipc_error(IpcError::unknown_outcome("lost"));
        assert!(matches!(err, ContractError::UnknownOutcome(_)));
    }

    #[test]
    fn unavailable_maps_to_known_failure() {
        let err = map_ipc_error(IpcError::unavailable("gone"));
        assert!(matches!(err, ContractError::KnownFailure(_)));
    }

    /// The client must satisfy the server-hosting / conductor seams.
    #[test]
    fn execution_client_is_a_send_execution_api() {
        fn assert_seam<T: ExecutionApi + ExecutionOps + Send + 'static>() {}
        assert_seam::<ExecutionClient>();
    }

    #[test]
    fn missing_socket_is_unavailable() {
        let err = ExecutionClient::connect(std::path::Path::new("/nonexistent/exec.sock"));
        assert!(matches!(err, Err(IpcError::Unavailable(_))));
    }
}
