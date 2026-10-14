//! `ExecutionApi`/`ExecutionOps` adapter for [`KernelEngine`] (B1.7).
//!
//! The kernel is the process-ready candidate replacement for the M1 seed
//! executor (`Executor<L>`, which hosts `Box<dyn Activity>` closures built from
//! conductor code and cannot cross a process boundary). This module makes the
//! kernel drivable through the frozen [`ExecutionApi`] and through the
//! kernel-specific [`ExecutionOps`] engine surface so it can sit behind the
//! `ExecutionApi` process boundary (`agalma-kernel-server`).
//!
//! ## What maps cleanly, and what does not
//!
//! The kernel separates the **pure transition** (`KernelEngine::apply`) from the
//! **external effect dispatch** (`run_activity`/`recover_pending`, which call a
//! registered [`crate::ActivityHandler`]). Handlers are Rust closures built by
//! the consumer and cannot cross a process boundary, so:
//!
//! - [`ExecutionApi::start`], [`ExecutionApi::cancel`], [`ExecutionApi::status`]
//!   map cleanly onto engine operations and are implemented faithfully.
//! - [`ExecutionApi::step`] and [`ExecutionApi::recover`] require executing and
//!   reconciling external effects, which needs a handler registry the server
//!   cannot host generically. They return
//!   [`ContractError::UnsupportedCapability`]; the gap is documented in
//!   `docs/b1-execution-evidence.md`. Callers drive the engine-op surface
//!   ([`ExecutionOps`]) instead.
//!
//! The [`ExecutionOps`] engine surface is expressed over JSON for the
//! kernel-native payloads (`Command`/`Inputs`/`ApplyOutcome`/`KernelState`/
//! `TimerFire`/`Reclaim`) because `agalma-ipc` (where the trait lives) must not
//! depend on `agalma-kernel` (that edge would be a cycle once the kernel server
//! depends on `agalma-ipc`).

use std::path::Path;

use agalma_contracts::{
    CancellationState, ContractError, DispatchIntent, ExecutionApi, ExecutionId, ExecutionPhase,
    ExecutionState, ExecutionStatus, LedgerApi, OperationId, OperationReceipt, StateSnapshot,
    StepOutcome, TaskId, TimerRecord,
};
use agalma_ipc::{
    EffectContext, EffectMode, EffectReport, ExecutionOps, HandlerKey, StepBegin,
};
use serde_json::Value;

use crate::activity::{
    begin_activity, finish_activity, pending_meta, ActivityContext, BeginOutcome, EffectReply,
    OperationRequest,
};
use crate::engine::KernelEngine;
use crate::exec_def::{coarse_state, Command, DirectivePlan, ExecState, Inputs};

/// The smallest generation whose execution id is not yet recorded.
fn next_generation<L: LedgerApi>(
    engine: &KernelEngine<L>,
    task: &TaskId,
) -> Result<u32, ContractError> {
    let mut generation = 1u32;
    loop {
        if engine
            .ledger()
            .execution(&ExecutionId::derive(task, generation))?
            .is_none()
        {
            return Ok(generation);
        }
        generation += 1;
    }
}

/// Recorded wall clock for the adapter's effect-free engine calls.
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn json_err(err: serde_json::Error) -> ContractError {
    ContractError::KnownFailure(format!("kernel exec_api json: {err}"))
}

impl<L: LedgerApi> ExecutionApi for KernelEngine<L> {
    /// Start a new generation for `task`: derive the next free execution id and
    /// apply the `Start` command. The kernel does not tie a lease to `start`
    /// (leases are explicit via [`ExecutionOps::begin_lease`]); the returned id
    /// matches the seed executor's `ExecutionId::derive(task, generation)`.
    fn start(&mut self, task: &TaskId) -> Result<ExecutionId, ContractError> {
        let generation = next_generation(self, task)?;
        let execution = ExecutionId::derive(task, generation);
        KernelEngine::apply(
            self,
            &execution,
            &Command::Start {
                task_id: task.clone(),
                generation,
            },
            &Inputs::default(),
        )?;
        Ok(execution)
    }

    /// Effect dispatch is handler-driven and cannot be hosted behind the process
    /// boundary (see the module note). Drive [`ExecutionOps`] instead.
    fn step(&mut self, _execution: &ExecutionId) -> Result<StepOutcome, ContractError> {
        Err(ContractError::UnsupportedCapability(
            "kernel ExecutionApi::step requires an activity handler registry (external effects \
             cannot cross the process boundary); drive the engine-op surface instead"
                .to_string(),
        ))
    }

    /// Recovery reconciles pending external effects through their handlers, which
    /// cannot cross the process boundary. The replay/projection check is
    /// available through [`ExecutionOps::load_state`].
    fn recover(&mut self) -> Result<(), ContractError> {
        Err(ContractError::UnsupportedCapability(
            "kernel ExecutionApi::recover reconciles pending effects through activity handlers \
             (not hostable behind the process boundary); use load_state/pending_dispatch instead"
                .to_string(),
        ))
    }

    /// Record a durable cancellation request as a kernel `Cancel` command.
    fn cancel(&mut self, execution: &ExecutionId) -> Result<(), ContractError> {
        KernelEngine::cancel(self, execution, now_ms())?;
        Ok(())
    }

    /// Project the reconstructed kernel state onto [`ExecutionStatus`].
    fn status(&self, execution: &ExecutionId) -> Result<ExecutionStatus, ContractError> {
        let state = KernelEngine::load(self, execution)?;
        if state.task_id.as_str().is_empty()
            && matches!(state.state, crate::exec_def::ExecState::Pending)
        {
            return Err(ContractError::KnownFailure(format!(
                "unknown execution {execution}"
            )));
        }
        let pending_operations = self
            .pending_dispatch()?
            .into_iter()
            .filter(|intent| &intent.execution_id == execution)
            .map(|intent| intent.operation_id)
            .collect();
        let settlement = KernelEngine::cancellation_settlement(self, execution)?;
        let cancelled = state.state == ExecState::Cancelled;
        let settled = settlement
            .as_ref()
            .is_some_and(|record| record.state == CancellationState::Settled);
        // Frozen projection: a kernel-native `Cancelled`, or a settled
        // cancellation, reports Failed/Parked with reason `Cancelled`. Completed
        // work keeps its completed outcome even when cancellation was a no-op.
        let (phase, exec_state, parked_reason) =
            if cancelled || (settled && state.state != ExecState::Completed) {
                (
                    ExecutionPhase::Parked,
                    ExecutionState::Failed,
                    Some("Cancelled".to_string()),
                )
            } else {
                (
                    state.phase,
                    coarse_state(state.state),
                    state.parked_reason.clone(),
                )
            };
        let cancellation = match settlement.as_ref().map(|record| record.state) {
            Some(CancellationState::Settled) => CancellationState::Settled,
            Some(CancellationState::Settling) => CancellationState::Settling,
            Some(CancellationState::Requested) => CancellationState::Requested,
            _ => CancellationState::None,
        };
        Ok(ExecutionStatus {
            execution_id: execution.clone(),
            phase,
            state: exec_state,
            attempt: state.attempt,
            pending_operations,
            parked_reason,
            cancellation,
        })
    }
}

impl<L: LedgerApi> ExecutionOps for KernelEngine<L> {
    fn apply(
        &mut self,
        execution_id: &ExecutionId,
        command: Value,
        inputs: Value,
    ) -> Result<Value, ContractError> {
        let command: Command = serde_json::from_value(command).map_err(json_err)?;
        let inputs: Inputs = serde_json::from_value(inputs).map_err(json_err)?;
        let outcome = KernelEngine::apply(self, execution_id, &command, &inputs)?;
        serde_json::to_value(outcome).map_err(json_err)
    }

    fn load_state(&self, execution_id: &ExecutionId) -> Result<Value, ContractError> {
        serde_json::to_value(KernelEngine::load(self, execution_id)?).map_err(json_err)
    }

    fn pending_dispatch(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        KernelEngine::pending_dispatch(self)
    }

    fn receipt(&self, operation_id: &OperationId) -> Result<Option<OperationReceipt>, ContractError> {
        KernelEngine::receipt(self, operation_id)
    }

    fn fire_due_timers(&mut self, now_unix_ms: u64) -> Result<Value, ContractError> {
        let fired = KernelEngine::fire_due_timers(self, now_unix_ms)?;
        serde_json::to_value(fired).map_err(json_err)
    }

    fn schedule_timer(&mut self, timer: &TimerRecord) -> Result<bool, ContractError> {
        KernelEngine::schedule_timer(self, timer)
    }

    fn due_timers(&self, now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError> {
        KernelEngine::due_timers(self, now_unix_ms)
    }

    fn begin_lease(&mut self, task: &TaskId) -> Result<u32, ContractError> {
        KernelEngine::begin_lease(self, task)
    }

    fn lease_generation(&self, task: &TaskId) -> Result<u32, ContractError> {
        KernelEngine::lease_generation(self, task)
    }

    fn reclaim(
        &mut self,
        task: &TaskId,
        execution_id: &ExecutionId,
        now_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        let reclaim = KernelEngine::reclaim(self, task, execution_id, now_unix_ms)?;
        serde_json::to_value(reclaim).map_err(json_err)
    }

    fn submit_directive(
        &mut self,
        execution_id: &ExecutionId,
        directive_id: &str,
        version: u64,
        plan: Value,
        now_unix_ms: u64,
    ) -> Result<Value, ContractError> {
        let plan: DirectivePlan = serde_json::from_value(plan).map_err(json_err)?;
        let outcome = KernelEngine::submit_directive(
            self,
            execution_id,
            directive_id,
            version,
            &plan,
            now_unix_ms,
        )?;
        serde_json::to_value(outcome).map_err(json_err)
    }

    fn export_snapshot_to(&self, path: &Path) -> Result<(), ContractError> {
        let snapshot: StateSnapshot = KernelEngine::export_state(self)?;
        let text = snapshot.to_canonical_json()?;
        std::fs::write(path, text).map_err(|e| {
            ContractError::KnownFailure(format!("write snapshot {}: {e}", path.display()))
        })
    }

    fn import_snapshot_from(&mut self, path: &Path) -> Result<(), ContractError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ContractError::KnownFailure(format!("read snapshot {}: {e}", path.display()))
        })?;
        let snapshot = StateSnapshot::from_json(&text)?;
        KernelEngine::import_state(self, &snapshot)
    }

    // ---- handler-registry protocol (caller-side effect dispatch) ----------

    fn begin_step(
        &mut self,
        execution_id: &ExecutionId,
        request: Value,
    ) -> Result<StepBegin, ContractError> {
        let request: OperationRequest = serde_json::from_value(request).map_err(json_err)?;
        begin_outcome(begin_activity(self, execution_id, &request)?)
    }

    fn begin_pending_step(
        &self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
    ) -> Result<StepBegin, ContractError> {
        let Some(meta) = pending_meta(self, operation_id)? else {
            // Already settled: a duplicate begin replays the recorded receipt.
            if let Some(receipt) = self.receipt(operation_id)? {
                let _ = receipt;
                return Ok(StepBegin::Done(StepOutcome::Dispatched {
                    operation: operation_id.clone(),
                    reconciled: false,
                    duplicate: true,
                }));
            }
            return Err(ContractError::KnownFailure(format!(
                "no pending operation {operation_id} to reconcile"
            )));
        };
        if &meta.execution_id != execution_id {
            return Err(ContractError::KnownFailure(format!(
                "operation {operation_id} does not belong to {execution_id}"
            )));
        }
        let state = self.load(execution_id)?;
        if state.state == ExecState::Parked {
            return Ok(StepBegin::Done(StepOutcome::Parked {
                reason: state.parked_reason.unwrap_or_default(),
            }));
        }
        if state.kill_latched {
            return Ok(StepBegin::Done(StepOutcome::Blocked {
                operation: Some(operation_id.clone()),
            }));
        }
        Ok(StepBegin::Effect(effect_context(
            &meta.context(self)?,
            EffectMode::Reconcile,
        )))
    }

    fn complete_step(
        &mut self,
        execution_id: &ExecutionId,
        operation_id: &OperationId,
        report: EffectReport,
        now_unix_ms: u64,
    ) -> Result<StepOutcome, ContractError> {
        let reply = match report {
            EffectReport::Present { outcome_ref } => {
                EffectReply::Present(crate::activity::ActivityOutcome::new(outcome_ref))
            }
            EffectReport::Executed { outcome_ref } => {
                EffectReply::Executed(crate::activity::ActivityOutcome::new(outcome_ref))
            }
            EffectReport::Ambiguous => EffectReply::Ambiguous,
            EffectReport::KnownFailure { message } => EffectReply::KnownFailure(message),
            EffectReport::UnknownOutcome { message } => EffectReply::UnknownOutcome(message),
        };
        let result = finish_activity(self, execution_id, operation_id, reply, now_unix_ms)?;
        to_step_outcome(result)
    }
}

/// Project a kernel [`ActivityContext`] onto the wire-safe [`EffectContext`].
fn effect_context(context: &ActivityContext, mode: EffectMode) -> EffectContext {
    EffectContext {
        execution_id: context.execution_id.clone(),
        operation_id: context.operation_id.clone(),
        task_id: context.task_id.clone(),
        phase: context.phase,
        attempt: context.attempt,
        activity: HandlerKey::new(context.activity.name.clone(), context.activity.version.clone()),
        binding: context
            .binding
            .as_ref()
            .and_then(|binding| serde_json::to_value(binding).ok()),
        inputs: context.inputs.clone(),
        now_unix_ms: context.now_unix_ms,
        mode,
    }
}

/// Project a begin outcome onto the wire [`StepBegin`].
fn begin_outcome(outcome: BeginOutcome) -> Result<StepBegin, ContractError> {
    Ok(match outcome {
        BeginOutcome::Terminal(result) => StepBegin::Done(to_step_outcome(result)?),
        BeginOutcome::Effect(begin) => {
            let mode = match begin.mode {
                crate::activity::EffectMode::Dispatch => EffectMode::Dispatch,
                crate::activity::EffectMode::Reconcile => EffectMode::Reconcile,
            };
            StepBegin::Effect(effect_context(&begin.context, mode))
        }
    })
}

/// Project a kernel [`OperationResult`](crate::activity::OperationResult) onto the
/// frozen [`StepOutcome`].
fn to_step_outcome(result: crate::activity::OperationResult) -> Result<StepOutcome, ContractError> {
    Ok(match result {
        crate::activity::OperationResult::Completed {
            operation_id,
            reconciled,
            duplicate,
            ..
        } => StepOutcome::Dispatched {
            operation: operation_id,
            reconciled,
            duplicate,
        },
        crate::activity::OperationResult::Parked { reason, .. } => StepOutcome::Parked { reason },
        crate::activity::OperationResult::Blocked { operation_id } => StepOutcome::Blocked {
            operation: Some(operation_id),
        },
        crate::activity::OperationResult::Conflict(conflict) => {
            return Err(ContractError::Conflict(conflict.reason))
        }
        crate::activity::OperationResult::UnknownActivity { activity } => {
            return Err(ContractError::UnsupportedCapability(format!(
                "no handler registered for activity {}",
                activity.key()
            )))
        }
    })
}
