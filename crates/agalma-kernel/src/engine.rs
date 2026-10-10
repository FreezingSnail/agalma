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
    ContractError, DispatchIntent, ExecutionEvent, ExecutionId, LedgerApi, OperationId,
    OperationReceipt,
};

use crate::crash::{crash_hook, AFTER_TRANSITION_COMMIT};
use crate::exec_def::{decode_event, ExecDef, EXEC_DEF_VERSION, KERNEL_VERSION};
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
