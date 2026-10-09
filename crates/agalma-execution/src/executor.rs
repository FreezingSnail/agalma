//! Serial executor: the M0 phase state machine over a [`LedgerApi`].
//!
//! ```text
//! intake -> checkout -> build -> verify -> integrate -> done | parked
//! ```
//!
//! One logical operation runs at a time. Every phase transition commits the new
//! execution state, its event, and the next dispatch intent in one ledger
//! transaction (`LedgerApi::commit`); operation completion is a second atomic
//! commit that records the receipt (idempotency anchor) and consumes the
//! intent. **Receipts are persisted before the transition advances**, so a crash
//! between the two commits is recovered by re-deriving the transition from the
//! recorded receipt.
//!
//! Durability contract ported from S0c (`docs/spikes/s0c-execution.md`):
//!
//! - stable operation ids `op:<execution>:<step>` (unchanged across delivery
//!   retries; a retry is an explicit *new* operation, never an implicit one);
//! - duplicate delivery returns the recorded result and repeats no effect;
//! - dispatch intents survive a crash between the transition commit and
//!   dispatch;
//! - recovery reconstructs state from events only (no I/O during replay),
//!   compares it with the projections, then reconciles pending intents;
//! - a kill latch blocks dispatch, including recovered dispatch, and survives
//!   restart;
//! - incompatible schema / execution-definition versions park; nothing is
//!   auto-migrated, and no effect runs before the gates pass.
//!
//! No external I/O runs inside a ledger transaction: effects run in the
//! activity, outside `commit`.

use std::process;

use agalma_contracts::{
    CommitBatch, ContractError, DispatchIntent, ExecutionApi, ExecutionEvent, ExecutionId,
    ExecutionPhase, ExecutionRecord, ExecutionState, ExecutionStatus, LedgerApi, OperationId,
    OperationReceipt, StepOutcome, TaskId,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::activity::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, OperationContext,
};

/// Execution-definition version. Stored in the `execution_created` event; a
/// record written by a different version parks rather than being reinterpreted.
pub const EXECUTION_DEFINITION_VERSION: u32 = 1;

/// Physical ledger schema this executor understands. Must match
/// `agalma_ledger::SCHEMA_VERSION`; a mismatch parks dispatch (the ledger also
/// refuses commits, so this gate runs before any effect).
pub const LEDGER_SCHEMA_VERSION: u32 = 2;

/// Environment variable selecting a crash-injection point (test support). When
/// unset the hooks are inert. Points: `after_transition_commit`,
/// `after_effect_before_completion`.
const CRASH_ENV: &str = "AGALMA_EXEC_CRASH_AT";

/// Executor configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutorConfig {
    /// Bounded build/verify retries for a red verify.
    pub max_attempts: u32,
    /// Execution-definition version written on `start` and required thereafter.
    pub execution_definition_version: u32,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        ExecutorConfig {
            max_attempts: 2,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        }
    }
}

/// Outcome of delivering one logical operation by id (ports S0c `Delivery`).
#[derive(Clone, Debug, PartialEq)]
pub enum DispatchOutcome {
    Executed {
        operation: OperationId,
        result: Value,
    },
    Reconciled {
        operation: OperationId,
        result: Value,
    },
    Recorded {
        operation: OperationId,
        result: Value,
    },
    /// The operation carries a lease older than the task's current lease: the
    /// claim was superseded by a re-claim, so the effect is refused (M1.6).
    StaleLease {
        operation: OperationId,
        lease: u32,
        current: u32,
    },
    Parked {
        reason: String,
    },
    Blocked,
}

/// Serial execution engine.
pub struct Executor<L: LedgerApi> {
    ledger: L,
    config: ExecutorConfig,
    activities: Vec<(ExecutionPhase, Box<dyn Activity>)>,
}

impl<L: LedgerApi> Executor<L> {
    /// Build an executor over `ledger`.
    pub fn new(ledger: L, config: ExecutorConfig) -> Self {
        Executor {
            ledger,
            config,
            activities: Vec::new(),
        }
    }

    /// Register the activity that performs `phase`.
    pub fn add_activity(&mut self, phase: ExecutionPhase, activity: Box<dyn Activity>) {
        self.activities.retain(|(p, _)| *p != phase);
        self.activities.push((phase, activity));
    }

    /// Borrow the ledger (diagnostics / tests).
    pub fn ledger(&self) -> &L {
        &self.ledger
    }

    /// Mutably borrow the ledger (diagnostics / tests).
    pub fn ledger_mut(&mut self) -> &mut L {
        &mut self.ledger
    }

    /// Whether the kill latch is set.
    pub fn kill_latch(&self) -> Result<bool, ContractError> {
        self.ledger.kill_latch()
    }

    /// Set or clear the kill latch (human `resume` clears it).
    pub fn set_kill_latch(&mut self, latched: bool) -> Result<(), ContractError> {
        self.ledger.set_kill_latch(latched)
    }

    /// Reconstruct `(phase, state, attempt)` from events only; performs no I/O
    /// beyond reading the recorded event log.
    pub fn reconstruct(
        &self,
        execution: &ExecutionId,
    ) -> Result<(ExecutionPhase, ExecutionState, u32), ContractError> {
        let events = self.ledger.events(execution)?;
        Ok(fold_events(&events))
    }

    /// Deliver one logical operation by id. Safe to call more than once: a
    /// completed operation returns its recorded receipt with no effect.
    pub fn deliver_operation(
        &mut self,
        operation: &OperationId,
    ) -> Result<DispatchOutcome, ContractError> {
        if let Some(receipt) = self.ledger.operation_receipt(operation)? {
            return Ok(DispatchOutcome::Recorded {
                operation: operation.clone(),
                result: receipt.result,
            });
        }

        let intent = self
            .ledger
            .pending_intents()?
            .into_iter()
            .find(|i| &i.operation_id == operation)
            .ok_or_else(|| ContractError::KnownFailure(format!("unknown operation {operation}")))?;
        let execution = intent.execution_id.clone();
        let record = self
            .ledger
            .execution(&execution)?
            .ok_or_else(|| ContractError::KnownFailure(format!("unknown execution {execution}")))?;

        if self.ledger.schema_version()? != LEDGER_SCHEMA_VERSION {
            return Ok(DispatchOutcome::Parked {
                reason: "incompatible schema version".to_string(),
            });
        }
        let stored_version = self.definition_version(&execution)?;
        if stored_version != self.config.execution_definition_version {
            let reason = format!(
                "incompatible execution definition version {stored_version} (expected {})",
                self.config.execution_definition_version
            );
            self.park(&execution, &record, Some(operation), &reason)?;
            return Ok(DispatchOutcome::Parked { reason });
        }
        if self.ledger.kill_latch()? {
            return Ok(DispatchOutcome::Blocked);
        }

        let ctx = build_context(&record, &intent)?;
        let probe = self.probe(&ctx)?;
        match probe {
            EffectProbe::Ambiguous => {
                let reason =
                    format!("ambiguous effect for {operation}; parked for manual reconciliation");
                self.park(&execution, &record, Some(operation), &reason)?;
                Ok(DispatchOutcome::Parked { reason })
            }
            EffectProbe::Present => {
                let outcome = self.reconcile(&ctx)?;
                self.complete(&ctx, &outcome, true)?;
                let step = self.advance(&execution, &record, &outcome, operation)?;
                if let StepOutcome::Parked { reason } = step {
                    return Ok(DispatchOutcome::Parked { reason });
                }
                Ok(DispatchOutcome::Reconciled {
                    operation: operation.clone(),
                    result: outcome.result,
                })
            }
            EffectProbe::Absent => {
                // Stale-lease gate: a re-claim superseded this operation's
                // claim. Refuse before any effect runs. A *present* effect is
                // still reconciled above (completing an already-landed merge is
                // not a new effect and is required by expiry recovery).
                let current = self.ledger.lease_generation(&record.task_id)?;
                if ctx.lease_generation < current {
                    return Ok(DispatchOutcome::StaleLease {
                        operation: operation.clone(),
                        lease: ctx.lease_generation,
                        current,
                    });
                }
                let outcome = self.execute(&ctx)?;
                maybe_crash("after_effect_before_completion");
                self.complete(&ctx, &outcome, false)?;
                let step = self.advance(&execution, &record, &outcome, operation)?;
                if let StepOutcome::Parked { reason } = step {
                    return Ok(DispatchOutcome::Parked { reason });
                }
                Ok(DispatchOutcome::Executed {
                    operation: operation.clone(),
                    result: outcome.result,
                })
            }
        }
    }

    // ---- internals ----------------------------------------------------------

    fn activity_mut(&mut self, phase: ExecutionPhase) -> Option<&mut (dyn Activity + 'static)> {
        for (p, activity) in self.activities.iter_mut() {
            if *p == phase {
                return Some(activity.as_mut());
            }
        }
        None
    }

    fn probe(&mut self, ctx: &OperationContext) -> Result<EffectProbe, ContractError> {
        let activity = self
            .activity_mut(ctx.phase)
            .ok_or_else(|| missing_activity(ctx.phase))?;
        Ok(activity.effect_present(ctx))
    }

    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ContractError> {
        let activity = self
            .activity_mut(ctx.phase)
            .ok_or_else(|| missing_activity(ctx.phase))?;
        activity.execute(ctx).map_err(map_activity_err)
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ContractError> {
        let activity = self
            .activity_mut(ctx.phase)
            .ok_or_else(|| missing_activity(ctx.phase))?;
        activity.reconcile(ctx).map_err(map_activity_err)
    }

    fn pending_for(&self, execution: &ExecutionId) -> Result<Option<OperationId>, ContractError> {
        Ok(self
            .ledger
            .pending_intents()?
            .into_iter()
            .find(|i| &i.execution_id == execution)
            .map(|i| i.operation_id))
    }

    /// Read the execution-definition version pinned in the `execution_created`
    /// event (`0` when absent, which never equals the product version).
    fn definition_version(&self, execution: &ExecutionId) -> Result<u32, ContractError> {
        for event in self.ledger.events(execution)? {
            if event.kind == "execution_created" {
                return Ok(event
                    .payload
                    .get("execution_version")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32);
            }
        }
        Ok(0)
    }

    /// Lease generation pinned in the `execution_created` event (fallback: the
    /// execution row's `generation`, so pre-M1.6 records stay fenceable).
    fn lease_for(&self, execution: &ExecutionId) -> Result<u32, ContractError> {
        for event in self.ledger.events(execution)? {
            if event.kind == "execution_created" {
                if let Some(lease) = event
                    .payload
                    .get("lease_generation")
                    .and_then(Value::as_u64)
                {
                    return Ok(lease as u32);
                }
                return Ok(event
                    .payload
                    .get("generation")
                    .and_then(Value::as_u64)
                    .unwrap_or(1) as u32);
            }
        }
        Ok(0)
    }

    fn max_attempts(&self, execution: &ExecutionId) -> Result<u32, ContractError> {
        for event in self.ledger.events(execution)? {
            if event.kind == "execution_created" {
                return Ok(event
                    .payload
                    .get("max_attempts")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
                    .unwrap_or(self.config.max_attempts));
            }
        }
        Ok(self.config.max_attempts)
    }

    fn parked_reason(&self, execution: &ExecutionId) -> Result<Option<String>, ContractError> {
        let mut reason = None;
        for event in self.ledger.events(execution)? {
            if event.kind == "parked" {
                reason = event
                    .payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(reason);
            }
        }
        Ok(reason)
    }

    /// Commit the operation receipt (and its event); this consumes the dispatch
    /// intent. Persisted before [`Executor::advance`].
    fn complete(
        &mut self,
        ctx: &OperationContext,
        outcome: &ActivityOutcome,
        reconciled: bool,
    ) -> Result<(), ContractError> {
        let now = now_ms();
        let payload = ReceiptPayload {
            result: outcome.result.clone(),
            next: outcome.next.clone(),
            lease_generation: ctx.lease_generation,
        };
        let receipt = OperationReceipt {
            operation_id: ctx.operation_id.clone(),
            execution_id: ctx.execution_id.clone(),
            kind: phase_str(ctx.phase).to_string(),
            inputs_hash: inputs_hash(&ctx.inputs),
            result: serde_json::to_value(payload).map_err(json_err)?,
            completed_at_unix_ms: now,
        };
        let event = ExecutionEvent {
            execution_id: ctx.execution_id.clone(),
            sequence: 0,
            kind: if reconciled {
                "operation_reconciled".to_string()
            } else {
                "operation_completed".to_string()
            },
            payload: json!({
                "operation_id": ctx.operation_id,
                "phase": phase_str(ctx.phase),
                "attempt": ctx.attempt,
                "lease_generation": ctx.lease_generation,
                "reconciled": reconciled,
                "next": outcome.next,
            }),
            recorded_at_unix_ms: now,
        };
        self.ledger.commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![event],
            receipts: vec![receipt],
            intents: vec![],
        })?;
        Ok(())
    }

    /// Park an execution (and, when supplied, its operation).
    fn park(
        &mut self,
        execution: &ExecutionId,
        record: &ExecutionRecord,
        operation: Option<&OperationId>,
        reason: &str,
    ) -> Result<(), ContractError> {
        let now = now_ms();
        let state = ExecutionRecord {
            phase: ExecutionPhase::Parked,
            state: ExecutionState::Parked,
            ..record.clone()
        };
        let event = ExecutionEvent {
            execution_id: execution.clone(),
            sequence: 0,
            kind: "parked".to_string(),
            payload: json!({
                "operation_id": operation,
                "phase": phase_str(record.phase),
                "attempt": record.attempt,
                "reason": reason,
            }),
            recorded_at_unix_ms: now,
        };
        self.ledger.commit(CommitBatch {
            expected_revision: Some(record.revision),
            state: Some(state),
            events: vec![event],
            receipts: vec![],
            intents: vec![],
        })?;
        Ok(())
    }

    /// Advance the phase machine after an operation completed. Returns the
    /// step outcome for the transition.
    fn advance(
        &mut self,
        execution: &ExecutionId,
        record: &ExecutionRecord,
        outcome: &ActivityOutcome,
        operation: &OperationId,
    ) -> Result<StepOutcome, ContractError> {
        match &outcome.next {
            ActivityNext::Done => {
                self.finish(execution, record, operation)?;
                maybe_crash("after_transition_commit");
                Ok(StepOutcome::Advanced {
                    phase: ExecutionPhase::Done,
                    operation: operation.clone(),
                })
            }
            ActivityNext::Advance => {
                let to = successor(record.phase);
                if to == ExecutionPhase::Done {
                    self.finish(execution, record, operation)?;
                    maybe_crash("after_transition_commit");
                    return Ok(StepOutcome::Advanced {
                        phase: ExecutionPhase::Done,
                        operation: operation.clone(),
                    });
                }
                let next = self.enter_phase(execution, record, to, record.attempt, json!({}))?;
                maybe_crash("after_transition_commit");
                Ok(StepOutcome::Advanced {
                    phase: to,
                    operation: next,
                })
            }
            ActivityNext::Retry { failure } => {
                let max = self.max_attempts(execution)?;
                if record.attempt < max {
                    let attempt = record.attempt + 1;
                    let inputs = json!({ "attempt": attempt, "failure": failure });
                    let next = self.enter_phase(
                        execution,
                        record,
                        ExecutionPhase::Build,
                        attempt,
                        inputs,
                    )?;
                    maybe_crash("after_transition_commit");
                    Ok(StepOutcome::Advanced {
                        phase: ExecutionPhase::Build,
                        operation: next,
                    })
                } else {
                    let reason = format!(
                        "max_attempts ({max}) exhausted after attempt {}; last failure {failure}",
                        record.attempt
                    );
                    self.park(execution, record, Some(operation), &reason)?;
                    Ok(StepOutcome::Parked { reason })
                }
            }
            ActivityNext::Park { reason } => {
                self.park(execution, record, Some(operation), reason)?;
                Ok(StepOutcome::Parked {
                    reason: reason.clone(),
                })
            }
        }
    }

    /// Atomically move into `phase`: new execution state, the phase event, and
    /// the durable dispatch intent for the phase's operation.
    fn enter_phase(
        &mut self,
        execution: &ExecutionId,
        record: &ExecutionRecord,
        phase: ExecutionPhase,
        attempt: u32,
        inputs: Value,
    ) -> Result<OperationId, ContractError> {
        let now = now_ms();
        let operation = operation_for(execution, phase, attempt);
        let lease_generation = self.lease_for(execution)?;
        let state = ExecutionRecord {
            phase,
            state: ExecutionState::Running,
            attempt,
            ..record.clone()
        };
        let events = if attempt > record.attempt {
            vec![
                ExecutionEvent {
                    execution_id: execution.clone(),
                    sequence: 0,
                    kind: "attempt_started".to_string(),
                    payload: json!({ "attempt": attempt, "inputs": inputs }),
                    recorded_at_unix_ms: now,
                },
                ExecutionEvent {
                    execution_id: execution.clone(),
                    sequence: 0,
                    kind: "phase_entered".to_string(),
                    payload: json!({
                        "from": phase_str(record.phase),
                        "to": phase_str(phase),
                        "operation_id": operation,
                        "attempt": attempt,
                    }),
                    recorded_at_unix_ms: now,
                },
            ]
        } else {
            vec![ExecutionEvent {
                execution_id: execution.clone(),
                sequence: 0,
                kind: "phase_entered".to_string(),
                payload: json!({
                    "from": phase_str(record.phase),
                    "to": phase_str(phase),
                    "operation_id": operation,
                    "attempt": attempt,
                }),
                recorded_at_unix_ms: now,
            }]
        };
        let intent = DispatchIntent {
            operation_id: operation.clone(),
            execution_id: execution.clone(),
            payload: serde_json::to_value(IntentPayload {
                kind: phase_str(phase).to_string(),
                step: step_of(phase, attempt),
                attempt,
                lease_generation,
                inputs,
            })
            .map_err(json_err)?,
            enqueued_at_unix_ms: now,
            consumed: false,
        };
        self.ledger.commit(CommitBatch {
            expected_revision: Some(record.revision),
            state: Some(state),
            events,
            receipts: vec![],
            intents: vec![intent],
        })?;
        Ok(operation)
    }

    /// Atomically finish the execution.
    fn finish(
        &mut self,
        execution: &ExecutionId,
        record: &ExecutionRecord,
        operation: &OperationId,
    ) -> Result<(), ContractError> {
        let now = now_ms();
        let state = ExecutionRecord {
            phase: ExecutionPhase::Done,
            state: ExecutionState::Completed,
            ..record.clone()
        };
        let event = ExecutionEvent {
            execution_id: execution.clone(),
            sequence: 0,
            kind: "done".to_string(),
            payload: json!({ "operation_id": operation, "attempt": record.attempt }),
            recorded_at_unix_ms: now,
        };
        self.ledger.commit(CommitBatch {
            expected_revision: Some(record.revision),
            state: Some(state),
            events: vec![event],
            receipts: vec![],
            intents: vec![],
        })?;
        Ok(())
    }
}

impl<L: LedgerApi> ExecutionApi for Executor<L> {
    fn start(&mut self, task: &TaskId) -> Result<ExecutionId, ContractError> {
        let generation = self.next_generation(task)?;
        // Each claim cycle begins a new, strictly increasing lease generation.
        // It is persisted here (with the execution-created event and first
        // dispatch intent) before any dispatch.
        let lease_generation = self.ledger.begin_lease(task)?;
        let execution = ExecutionId::derive(task, generation);
        let now = now_ms();
        let record = ExecutionRecord {
            execution_id: execution.clone(),
            task_id: task.clone(),
            generation,
            phase: ExecutionPhase::Intake,
            state: ExecutionState::Running,
            attempt: 1,
            revision: 0,
        };
        let created = ExecutionEvent {
            execution_id: execution.clone(),
            sequence: 0,
            kind: "execution_created".to_string(),
            payload: json!({
                "task_id": task,
                "generation": generation,
                "lease_generation": lease_generation,
                "attempt": 1,
                "max_attempts": self.config.max_attempts,
                "execution_version": self.config.execution_definition_version,
            }),
            recorded_at_unix_ms: now,
        };
        let operation = operation_for(&execution, ExecutionPhase::Intake, 1);
        let intent = DispatchIntent {
            operation_id: operation.clone(),
            execution_id: execution.clone(),
            payload: serde_json::to_value(IntentPayload {
                kind: phase_str(ExecutionPhase::Intake).to_string(),
                step: step_of(ExecutionPhase::Intake, 1),
                attempt: 1,
                lease_generation,
                inputs: json!({ "task_id": task, "generation": generation }),
            })
            .map_err(json_err)?,
            enqueued_at_unix_ms: now,
            consumed: false,
        };
        // One atomic commit: execution row + initial event + first dispatch intent.
        self.ledger.commit(CommitBatch {
            expected_revision: None,
            state: Some(record),
            events: vec![created],
            receipts: vec![],
            intents: vec![intent],
        })?;
        maybe_crash("after_transition_commit");
        Ok(execution)
    }

    fn step(&mut self, execution: &ExecutionId) -> Result<StepOutcome, ContractError> {
        let record = self
            .ledger
            .execution(execution)?
            .ok_or_else(|| ContractError::KnownFailure(format!("unknown execution {execution}")))?;

        if record.state == ExecutionState::Parked || record.phase == ExecutionPhase::Parked {
            let reason = self
                .parked_reason(execution)?
                .unwrap_or_else(|| "parked".to_string());
            return Ok(StepOutcome::Parked { reason });
        }
        if record.phase == ExecutionPhase::Done {
            return Ok(StepOutcome::Idle {
                phase: ExecutionPhase::Done,
            });
        }

        if self.ledger.schema_version()? != LEDGER_SCHEMA_VERSION {
            return Ok(StepOutcome::Parked {
                reason: "incompatible schema version".to_string(),
            });
        }
        let stored_version = self.definition_version(execution)?;
        if stored_version != self.config.execution_definition_version {
            let reason = format!(
                "incompatible execution definition version {stored_version} (expected {})",
                self.config.execution_definition_version
            );
            self.park(execution, &record, None, &reason)?;
            return Ok(StepOutcome::Parked { reason });
        }

        // Deliver the single pending operation, if any.
        if let Some(operation) = self.pending_for(execution)? {
            return match self.deliver_operation(&operation)? {
                DispatchOutcome::Executed { operation, .. } => Ok(StepOutcome::Dispatched {
                    operation,
                    reconciled: false,
                    duplicate: false,
                }),
                DispatchOutcome::Reconciled { operation, .. } => Ok(StepOutcome::Dispatched {
                    operation,
                    reconciled: true,
                    duplicate: false,
                }),
                DispatchOutcome::Recorded { operation, .. } => Ok(StepOutcome::Dispatched {
                    operation,
                    reconciled: false,
                    duplicate: true,
                }),
                DispatchOutcome::StaleLease {
                    operation: _,
                    lease,
                    current,
                } => Ok(StepOutcome::Parked {
                    reason: format!(
                        "stale lease {lease} (task lease is {current}); operation refused"
                    ),
                }),
                DispatchOutcome::Parked { reason } => Ok(StepOutcome::Parked { reason }),
                DispatchOutcome::Blocked => Ok(StepOutcome::Blocked {
                    operation: Some(operation),
                }),
            };
        }

        // No pending intent: a crash may have landed between the receipt commit
        // and the transition advance. Re-derive the advance from the receipt.
        let current = operation_for(execution, record.phase, record.attempt);
        if let Some(receipt) = self.ledger.operation_receipt(&current)? {
            let payload: ReceiptPayload =
                serde_json::from_value(receipt.result).map_err(json_err)?;
            let outcome = ActivityOutcome {
                result: payload.result,
                next: payload.next,
            };
            return self.advance(execution, &record, &outcome, &current);
        }

        Ok(StepOutcome::Parked {
            reason: format!(
                "no pending operation and no completion record for phase {}",
                phase_str(record.phase)
            ),
        })
    }

    fn recover(&mut self) -> Result<(), ContractError> {
        if self.ledger.schema_version()? != LEDGER_SCHEMA_VERSION {
            // Cannot persist anything under an incompatible schema; leave
            // dispatch gated (step/status report parked). Never auto-migrate.
            return Ok(());
        }

        // 1. Replay every execution from events only and require agreement with
        //    the projection.
        let executions = self.ledger.executions()?;
        for record in &executions {
            let (phase, state, attempt) = self.reconstruct(&record.execution_id)?;
            if phase != record.phase || state != record.state || attempt != record.attempt {
                return Err(ContractError::Conflict(format!(
                    "reconstructed state disagrees with projection for {}: \
                     derived=({phase:?},{state:?},{attempt}) projected=({:?},{:?},{})",
                    record.execution_id, record.phase, record.state, record.attempt
                )));
            }
        }

        // 2. Reconcile pending intents: complete an effect that survived a crash
        //    (or is ambiguous -> park). Absent effects stay pending for dispatch.
        let pending = self.ledger.pending_intents()?;
        for intent in pending {
            if self
                .ledger
                .operation_receipt(&intent.operation_id)?
                .is_some()
            {
                continue;
            }
            let execution = intent.execution_id.clone();
            let record = match self.ledger.execution(&execution)? {
                Some(record) => record,
                None => continue,
            };
            if self.definition_version(&execution)? != self.config.execution_definition_version {
                continue; // dispatch will park it
            }
            let ctx = build_context(&record, &intent)?;
            match self.probe(&ctx)? {
                EffectProbe::Absent => {}
                EffectProbe::Ambiguous => {
                    let reason = format!(
                        "ambiguous effect for {}; parked for manual reconciliation",
                        intent.operation_id
                    );
                    self.park(&execution, &record, Some(&intent.operation_id), &reason)?;
                }
                EffectProbe::Present => {
                    let outcome = self.reconcile(&ctx)?;
                    self.complete(&ctx, &outcome, true)?;
                    self.advance(&execution, &record, &outcome, &intent.operation_id)?;
                }
            }
        }
        Ok(())
    }

    fn cancel(&mut self, execution: &ExecutionId) -> Result<(), ContractError> {
        if self.ledger.execution(execution)?.is_none() {
            return Err(ContractError::KnownFailure(format!(
                "unknown execution {execution}"
            )));
        }
        // Persist the request as the kill latch (honored before every dispatch)
        // and record it as an event.
        self.ledger.set_kill_latch(true)?;
        self.ledger.commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![ExecutionEvent {
                execution_id: execution.clone(),
                sequence: 0,
                kind: "cancel_requested".to_string(),
                payload: json!({ "reason": "cancel requested" }),
                recorded_at_unix_ms: now_ms(),
            }],
            receipts: vec![],
            intents: vec![],
        })?;
        Ok(())
    }

    fn status(&self, execution: &ExecutionId) -> Result<ExecutionStatus, ContractError> {
        let record = self
            .ledger
            .execution(execution)?
            .ok_or_else(|| ContractError::KnownFailure(format!("unknown execution {execution}")))?;
        let pending_operations = self
            .ledger
            .pending_intents()?
            .into_iter()
            .filter(|i| &i.execution_id == execution)
            .map(|i| i.operation_id)
            .collect();
        if self.ledger.schema_version()? != LEDGER_SCHEMA_VERSION {
            return Ok(ExecutionStatus {
                execution_id: execution.clone(),
                phase: record.phase,
                state: ExecutionState::Parked,
                attempt: record.attempt,
                pending_operations,
                parked_reason: Some("incompatible schema version".to_string()),
            });
        }
        Ok(ExecutionStatus {
            execution_id: execution.clone(),
            phase: record.phase,
            state: record.state,
            attempt: record.attempt,
            pending_operations,
            parked_reason: self.parked_reason(execution)?,
        })
    }
}

impl<L: LedgerApi> Executor<L> {
    fn next_generation(&self, task: &TaskId) -> Result<u32, ContractError> {
        let mut generation = 1u32;
        loop {
            let candidate = ExecutionId::derive(task, generation);
            if self.ledger.execution(&candidate)?.is_none() {
                return Ok(generation);
            }
            generation += 1;
        }
    }
}

/// Recorded-receipt payload: the activity result plus the durable next decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ReceiptPayload {
    result: Value,
    next: ActivityNext,
    #[serde(default)]
    lease_generation: u32,
}

/// Durable dispatch-intent payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct IntentPayload {
    kind: String,
    step: String,
    attempt: u32,
    #[serde(default)]
    lease_generation: u32,
    inputs: Value,
}

fn build_context(
    record: &ExecutionRecord,
    intent: &DispatchIntent,
) -> Result<OperationContext, ContractError> {
    let payload: IntentPayload =
        serde_json::from_value(intent.payload.clone()).map_err(json_err)?;
    let phase = parse_phase(&payload.kind).ok_or_else(|| {
        ContractError::KnownFailure(format!(
            "operation {} has unknown phase kind {:?}",
            intent.operation_id, payload.kind
        ))
    })?;
    Ok(OperationContext {
        execution_id: intent.execution_id.clone(),
        operation_id: intent.operation_id.clone(),
        task_id: record.task_id.clone(),
        phase,
        attempt: payload.attempt,
        lease_generation: if payload.lease_generation == 0 {
            record.generation
        } else {
            payload.lease_generation
        },
        inputs: payload.inputs,
    })
}

/// Fold recorded events into `(phase, state, attempt)` — pure, no I/O.
fn fold_events(events: &[ExecutionEvent]) -> (ExecutionPhase, ExecutionState, u32) {
    let mut phase = ExecutionPhase::Intake;
    let mut state = ExecutionState::Running;
    let mut attempt = 1u32;
    for event in events {
        match event.kind.as_str() {
            "execution_created" => {
                phase = ExecutionPhase::Intake;
                state = ExecutionState::Running;
                attempt = event
                    .payload
                    .get("attempt")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
                    .unwrap_or(1);
            }
            "phase_entered" => {
                if let Some(to) = event.payload.get("to").and_then(Value::as_str) {
                    if let Some(p) = parse_phase(to) {
                        phase = p;
                    }
                }
                state = ExecutionState::Running;
                if let Some(a) = event.payload.get("attempt").and_then(Value::as_u64) {
                    attempt = a as u32;
                }
            }
            "done" => {
                phase = ExecutionPhase::Done;
                state = ExecutionState::Completed;
            }
            "parked" => {
                phase = ExecutionPhase::Parked;
                state = ExecutionState::Parked;
            }
            _ => {}
        }
    }
    (phase, state, attempt)
}

fn operation_for(execution: &ExecutionId, phase: ExecutionPhase, attempt: u32) -> OperationId {
    OperationId::derive(execution, &step_of(phase, attempt))
}

fn step_of(phase: ExecutionPhase, attempt: u32) -> String {
    // Build and verify run once per attempt, so the attempt is part of their
    // stable operation id (a retry is an explicit new operation). Pipeline
    // phases that run once per execution use the bare phase name.
    match phase {
        ExecutionPhase::Build | ExecutionPhase::Verify => format!("{}@{attempt}", phase_str(phase)),
        _ => phase_str(phase).to_string(),
    }
}

/// Next non-terminal phase in the pipeline (`Integrate -> Done`).
fn successor(phase: ExecutionPhase) -> ExecutionPhase {
    match phase {
        ExecutionPhase::Intake => ExecutionPhase::Checkout,
        ExecutionPhase::Checkout => ExecutionPhase::Build,
        ExecutionPhase::Build => ExecutionPhase::Verify,
        ExecutionPhase::Verify => ExecutionPhase::Integrate,
        ExecutionPhase::Integrate => ExecutionPhase::Done,
        ExecutionPhase::Done | ExecutionPhase::Parked => ExecutionPhase::Done,
    }
}

fn phase_str(phase: ExecutionPhase) -> &'static str {
    match phase {
        ExecutionPhase::Intake => "intake",
        ExecutionPhase::Checkout => "checkout",
        ExecutionPhase::Build => "build",
        ExecutionPhase::Verify => "verify",
        ExecutionPhase::Integrate => "integrate",
        ExecutionPhase::Done => "done",
        ExecutionPhase::Parked => "parked",
    }
}

fn parse_phase(raw: &str) -> Option<ExecutionPhase> {
    Some(match raw {
        "intake" => ExecutionPhase::Intake,
        "checkout" => ExecutionPhase::Checkout,
        "build" => ExecutionPhase::Build,
        "verify" => ExecutionPhase::Verify,
        "integrate" => ExecutionPhase::Integrate,
        "done" => ExecutionPhase::Done,
        "parked" => ExecutionPhase::Parked,
        _ => return None,
    })
}

fn missing_activity(phase: ExecutionPhase) -> ContractError {
    ContractError::KnownFailure(format!(
        "no activity registered for phase {}",
        phase_str(phase)
    ))
}

fn map_activity_err(err: ActivityError) -> ContractError {
    match err {
        ActivityError::KnownFailure(message) => ContractError::KnownFailure(message),
        ActivityError::UnknownOutcome(message) => ContractError::UnknownOutcome(message),
    }
}

/// Stable 64-bit FNV-1a over the canonical JSON of the operation inputs.
fn inputs_hash(inputs: &Value) -> String {
    let text = serde_json::to_string(inputs).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Test-support crash injection. Inert unless `AGALMA_EXEC_CRASH_AT` selects
/// `point`; then the process aborts (SIGABRT) so no destructors or buffered
/// writes run after the injection point.
fn maybe_crash(point: &str) {
    if std::env::var(CRASH_ENV).ok().as_deref() == Some(point) {
        // SIGABRT: deliberately abnormal so nothing after this point executes.
        process::abort();
    }
}

fn json_err(err: serde_json::Error) -> ContractError {
    ContractError::KnownFailure(format!("execution json: {err}"))
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
