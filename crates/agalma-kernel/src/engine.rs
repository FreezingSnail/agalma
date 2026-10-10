//! Kernel engine: history, projection, deterministic replay, and effect execution.
//!
//! The engine owns no state of its own beyond the ledger handle. `apply` loads
//! the current state by replaying history (no I/O beyond reading the event log),
//! runs the pure transition, and executes its effects: each `Commit` is one
//! atomic [`LedgerApi::commit`] (state projection + event + receipts + intents),
//! then the crash hook fires before the `Dispatch` hand-off. Recovery rebuilds
//! state from events alone and compares it with the projection; a mismatch parks
//! rather than trusting the projection.

use agalma_contracts::{
    ContractError, DirectiveRow, DirectiveState, DispatchIntent, ExecutionEvent, ExecutionId,
    LedgerApi, OperationId, OperationReceipt, TimerRecord,
};

use crate::crash::{
    crash_hook, AFTER_DIRECTIVE_COMMIT, AFTER_TIMER_COMMIT, AFTER_TRANSITION_COMMIT,
};
use crate::exec_def::{
    decode_event, DirectivePlan, ExecDef, ExecState, OperationState, EXEC_DEF_VERSION,
    KERNEL_VERSION,
};
use crate::hash::content_hash;
use crate::state::KernelState;
use crate::transition::{step, Conflict, Effect};

/// Ledger key holding the persisted execution-definition version.
pub const META_EXEC_DEF_VERSION: &str = "exec_def_version";
/// Ledger key holding the persisted kernel implementation version.
pub const META_KERNEL_VERSION: &str = "kernel_version";

/// Outcome of applying one command.
#[derive(Clone, Debug, PartialEq)]
pub struct ApplyOutcome {
    /// The resulting reconstructed state.
    pub state: KernelState,
    /// Post-commit dispatch hand-offs the conductor must perform, in order.
    pub dispatches: Vec<DispatchIntent>,
    /// Set when the command was rejected as a conflict (no effects executed).
    pub conflict: Option<Conflict>,
}

/// Outcome of firing one due timer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimerFire {
    pub timer_id: String,
    /// True when this call recorded the `TimerFired` event; false when it was
    /// already recorded (idempotent no-op) but the row was marked fired.
    pub applied: bool,
}

/// Durable execution kernel over a [`LedgerApi`].
pub struct KernelEngine<L: LedgerApi> {
    ledger: L,
    exec_def: ExecDef,
    /// Set when the persisted `exec_def_version` is unknown: every load/apply
    /// parks rather than guessing.
    unknown_version: Option<String>,
}

impl<L: LedgerApi> KernelEngine<L> {
    /// Open the kernel over `ledger`.
    ///
    /// On first open the kernel records `exec_def_version`/`kernel_version` in
    /// `kernel_meta`. A stored `exec_def_version` the kernel does not know opens
    /// successfully but parks all work (never a guess, never a silent upgrade).
    pub fn open(mut ledger: L) -> Result<Self, ContractError> {
        let stored = ledger.kernel_meta_get(META_EXEC_DEF_VERSION)?;
        match stored {
            None => {
                ledger.kernel_meta_set(META_EXEC_DEF_VERSION, EXEC_DEF_VERSION)?;
                ledger.kernel_meta_set(META_KERNEL_VERSION, KERNEL_VERSION)?;
                Ok(KernelEngine {
                    ledger,
                    exec_def: ExecDef::v1(),
                    unknown_version: None,
                })
            }
            Some(version) => {
                let unknown_version = ExecDef::for_version(&version).is_none().then_some(version);
                Ok(KernelEngine {
                    ledger,
                    exec_def: ExecDef::v1(),
                    unknown_version,
                })
            }
        }
    }

    /// The resolved execution definition.
    pub fn exec_def(&self) -> &ExecDef {
        &self.exec_def
    }

    /// The persisted execution-definition version, when known.
    pub fn exec_def_version(&self) -> Option<&str> {
        self.unknown_version
            .is_none()
            .then_some(self.exec_def.version())
    }

    /// Borrow the ledger (diagnostics / tests).
    pub fn ledger(&self) -> &L {
        &self.ledger
    }

    /// Mutably borrow the ledger (diagnostics / tests).
    pub fn ledger_mut(&mut self) -> &mut L {
        &mut self.ledger
    }

    /// Reconstruct an execution's state from history alone, then require it to
    /// agree with the stored projection. A mismatch (or unknown `exec_def`
    /// version) yields a parked state, never a guess.
    pub fn load(&self, execution_id: &ExecutionId) -> Result<KernelState, ContractError> {
        if let Some(version) = &self.unknown_version {
            return Ok(KernelState::parked(
                execution_id,
                format!("unknown exec_def version {version:?}; parked (no guessing)"),
            ));
        }
        let events = self.ledger.events(execution_id)?;
        let replayed = replay(execution_id, &events)?;
        match self.ledger.execution(execution_id)? {
            Some(projection) => {
                if replayed.matches_projection(&projection) {
                    Ok(replayed)
                } else {
                    Ok(replayed.park(format!(
                        "projection mismatch: history/derived does not match executions row \
                         (derived phase={:?} state={:?} attempt={}, projected phase={:?} state={:?} attempt={})",
                        replayed.phase,
                        crate::exec_def::coarse_state(replayed.state),
                        replayed.attempt,
                        projection.phase,
                        projection.state,
                        projection.attempt,
                    )))
                }
            }
            None => Ok(replayed),
        }
    }

    /// Run one command: load, transition, then commit and dispatch.
    ///
    /// Effects are executed in order; the crash hook fires after each transition
    /// commit and before its dispatch, so a crash there is recovered from the
    /// durable intent. A conflict executes nothing.
    pub fn apply(
        &mut self,
        execution_id: &ExecutionId,
        command: &crate::exec_def::Command,
        inputs: &crate::exec_def::Inputs,
    ) -> Result<ApplyOutcome, ContractError> {
        let state = self.load(execution_id)?;
        if state.state == crate::exec_def::ExecState::Parked {
            return Ok(ApplyOutcome {
                state,
                dispatches: Vec::new(),
                conflict: None,
            });
        }
        let result = step(&state, command, inputs);
        if result.conflict.is_some() {
            return Ok(ApplyOutcome {
                state: result.state,
                dispatches: Vec::new(),
                conflict: result.conflict,
            });
        }
        let mut dispatches = Vec::new();
        for effect in result.effects {
            match effect {
                Effect::Commit(batch) => {
                    self.ledger.commit(batch)?;
                    // Transition commit returned; dispatch has not run.
                    crash_hook(AFTER_TRANSITION_COMMIT);
                }
                Effect::Dispatch(intent) => dispatches.push(intent),
            }
        }

        // Completed/cancelled work cancels its pending timers: a terminal
        // execution must never have a timer fire on its behalf.
        if matches!(
            result.state.state,
            ExecState::Completed | ExecState::Failed | ExecState::Cancelled
        ) {
            self.ledger.cancel_timers_for_execution(execution_id)?;
        }

        // Cancelled operations release their queued dispatch intents, so a
        // replacement directive or cancel does not leave stale work behind.
        let mut cancelled = Vec::new();
        for (operation_id, record) in &result.state.operations {
            if record.state != OperationState::Cancelled {
                continue;
            }
            let was_cancelled = state.operations.get(operation_id).map(|prior| prior.state)
                == Some(OperationState::Cancelled);
            if !was_cancelled {
                cancelled.push(operation_id.clone());
            }
        }
        for operation_id in cancelled {
            self.ledger.consume_dispatch_intent(&operation_id)?;
        }

        Ok(ApplyOutcome {
            state: result.state,
            dispatches,
            conflict: None,
        })
    }

    /// Durable (unconsumed) dispatch intents — the recovery work list.
    pub fn pending_dispatch(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        self.ledger.pending_intents()
    }

    /// Recorded receipt for a completed operation (idempotency anchor).
    pub fn receipt(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<OperationReceipt>, ContractError> {
        self.ledger.operation_receipt(operation_id)
    }

    // -----------------------------------------------------------------------
    // Durable waiting: timers, deadlines, and directives
    // -----------------------------------------------------------------------

    /// Schedule a durable retry timer or deadline, atomically.
    ///
    /// The row is durable before it can fire; a crash immediately after the row
    /// commits is recovered by [`KernelEngine::fire_due_timers`]. Returns `true`
    /// when a new row was written; an identical duplicate is a no-op (`false`)
    /// and reusing an id with different contents is a conflict.
    pub fn schedule_timer(&mut self, timer: &TimerRecord) -> Result<bool, ContractError> {
        let inserted = self.ledger.insert_timer(timer)?;
        if inserted {
            // The row is durable; firing happens on a later wake.
            crash_hook(AFTER_TIMER_COMMIT);
        }
        Ok(inserted)
    }

    /// Pending timers due at or before `now_unix_ms`, ordered by deadline.
    pub fn due_timers(&self, now_unix_ms: u64) -> Result<Vec<TimerRecord>, ContractError> {
        self.ledger.due_timers(now_unix_ms)
    }

    /// Fire every overdue pending timer **exactly once**, in deadline order.
    ///
    /// Each due timer applies a recovered `Command::TimerFired` (idempotent by
    /// timer id: a timer already recorded in history produces no second event
    /// and never advances the phase twice), then the row is marked fired. A
    /// crash between the event commit and the row update is harmless: the next
    /// call re-reads the due row, the transition is a no-op, and the row is
    /// marked fired. A second restart observes the fired row and fires nothing.
    pub fn fire_due_timers(&mut self, now_unix_ms: u64) -> Result<Vec<TimerFire>, ContractError> {
        let due = self.ledger.due_timers(now_unix_ms)?;
        let mut fired = Vec::new();
        for timer in due {
            let content_hash = timer_content_hash(&timer);
            let before = self.load(&timer.execution_id)?;
            let apply = self.apply(
                &timer.execution_id,
                &crate::exec_def::Command::TimerFired {
                    timer_id: timer.timer_id.clone(),
                    content_hash,
                },
                &crate::exec_def::Inputs::default(),
            )?;
            let applied = apply.state.fired_timers.contains_key(&timer.timer_id)
                && !before.fired_timers.contains_key(&timer.timer_id);
            self.ledger.mark_timer_fired(&timer.timer_id)?;
            fired.push(TimerFire {
                timer_id: timer.timer_id,
                applied,
            });
        }
        Ok(fired)
    }

    /// Earliest pending timer deadline (epoch ms), if any.
    ///
    /// The caller's wait contract: when there is no runnable work, sleep until
    /// this deadline (or until external input arrives) and then call
    /// [`KernelEngine::fire_due_timers`]. `None` means no timer is pending.
    pub fn next_deadline(&self) -> Result<Option<u64>, ContractError> {
        self.ledger.next_deadline()
    }

    /// Whether runnable work exists right now: an overdue timer or an
    /// unconsumed dispatch intent.
    ///
    /// A caller with no runnable work must **not** busy-wait; it releases its
    /// agent sessions/workers and sleeps until [`KernelEngine::next_deadline`]
    /// (or external input). The kernel never polls on its own.
    pub fn has_runnable_work(&self, now_unix_ms: u64) -> Result<bool, ContractError> {
        if !self.ledger.due_timers(now_unix_ms)?.is_empty() {
            return Ok(true);
        }
        Ok(!self.ledger.pending_intents()?.is_empty())
    }

    /// Persist a directive work plan, then apply it to `execution_id`.
    ///
    /// Crash-safe: the row is durable before the transition, so a crash after
    /// the row commits but before the transition is recovered by
    /// [`KernelEngine::apply_directive`]. A duplicate plan (same id + version +
    /// contents) is a harmless no-op; a newer version replaces queued work.
    pub fn submit_directive(
        &mut self,
        execution_id: &ExecutionId,
        directive_id: &str,
        version: u64,
        plan: &DirectivePlan,
        now_unix_ms: u64,
    ) -> Result<ApplyOutcome, ContractError> {
        let row = DirectiveRow {
            directive_id: directive_id.to_string(),
            version,
            source: plan.source.clone(),
            state: DirectiveState::Active,
            payload: serde_json::to_value(plan).map_err(json_err)?,
            received_at_unix_ms: now_unix_ms,
        };
        let changed = self.ledger.upsert_directive(&row)?;
        if changed {
            // The directive row is durable; the transition may not be.
            crash_hook(AFTER_DIRECTIVE_COMMIT);
        }
        self.apply_directive(execution_id, directive_id, now_unix_ms)
    }

    /// Apply the durable directive row `directive_id` to `execution_id`.
    ///
    /// Idempotent by `(directive_id, version)`: recovery after a crash between
    /// the row commit and the transition, and a duplicate update, are no-ops. A
    /// missing row is a no-op.
    pub fn apply_directive(
        &mut self,
        execution_id: &ExecutionId,
        directive_id: &str,
        now_unix_ms: u64,
    ) -> Result<ApplyOutcome, ContractError> {
        let Some(row) = self.ledger.directive(directive_id)? else {
            return Ok(ApplyOutcome {
                state: self.load(execution_id)?,
                dispatches: Vec::new(),
                conflict: None,
            });
        };
        let plan: DirectivePlan = serde_json::from_value(row.payload.clone()).map_err(json_err)?;
        let content_hash = content_hash(&row.payload);
        self.apply(
            execution_id,
            &crate::exec_def::Command::DirectiveUpdate {
                directive_id: directive_id.to_string(),
                version: row.version,
                content_hash,
            },
            &crate::exec_def::Inputs {
                directive: Some(plan),
                now_unix_ms,
                ..crate::exec_def::Inputs::default()
            },
        )
    }
}

/// Reconstruct state from history alone by replaying recorded commands through
/// the pure transition. Performs no external effects.
pub fn replay(
    execution_id: &ExecutionId,
    events: &[ExecutionEvent],
) -> Result<KernelState, ContractError> {
    let mut state = KernelState::unstarted(execution_id);
    for event in events {
        let recorded = decode_event(&event.payload).map_err(|err| {
            ContractError::KnownFailure(format!(
                "kernel history: undecodable event #{}-{}: {err}",
                event.sequence, event.kind
            ))
        })?;
        let result = step(&state, &recorded.command, &recorded.inputs);
        state = result.state;
        // The ledger owns the monotonic sequence; the projection revision
        // advances once per recorded event.
        state.revision = event.sequence;
    }
    Ok(state)
}

/// Deterministic content hash pinning a timer's schedule (conflict detector).
fn timer_content_hash(timer: &TimerRecord) -> String {
    content_hash(&serde_json::json!({
        "timer_id": timer.timer_id,
        "execution_id": timer.execution_id.as_str(),
        "kind": timer.kind,
        "fire_at": timer.fire_at_unix_ms,
        "payload": timer.payload,
    }))
}

fn json_err(e: serde_json::Error) -> ContractError {
    ContractError::KnownFailure(format!("kernel json: {e}"))
}
