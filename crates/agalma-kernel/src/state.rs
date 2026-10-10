//! Reconstructed kernel state and its coarse projection.
//!
//! [`KernelState`] is the kernel's canonical per-execution state. It is *not*
//! persisted directly: it is reconstructed from history by the reducer
//! ([`crate::engine::replay`]). The durable `executions` row is its coarse
//! projection ([`KernelState::projection`]); recovery compares the two and parks
//! on any mismatch rather than trusting the projection.

use std::collections::BTreeMap;

use agalma_contracts::{
    ArtifactRef, ExecutionId, ExecutionPhase, ExecutionRecord, OperationId, TaskId,
};
use serde::{Deserialize, Serialize};

use crate::exec_def::{
    coarse_state, DirectiveRecord, ExecState, OperationRecord, OperationState, EXEC_DEF_VERSION,
};

/// Canonical per-execution kernel state.
///
/// `Serialize`/`Deserialize` (B1.7) let the reconstructed state cross the
/// `ExecutionApi` process boundary as JSON without an implementation-crate edge
/// into `agalma-ipc`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelState {
    pub execution_id: ExecutionId,
    pub task_id: TaskId,
    pub generation: u32,
    pub exec_def_version: String,
    pub state: ExecState,
    pub phase: ExecutionPhase,
    pub attempt: u32,
    /// Sequence of the last recorded event; equals the projection revision.
    pub revision: u64,
    pub operations: BTreeMap<OperationId, OperationRecord>,
    pub directives: BTreeMap<String, DirectiveRecord>,
    /// Fired timers: `timer_id -> content_hash` (idempotency by timer id).
    pub fired_timers: BTreeMap<String, String>,
    pub kill_latched: bool,
    pub parked_reason: Option<String>,
}

impl KernelState {
    /// The state before `Start` (no task bound yet).
    pub fn unstarted(execution_id: &ExecutionId) -> Self {
        KernelState {
            execution_id: execution_id.clone(),
            task_id: TaskId::new(""),
            generation: 0,
            exec_def_version: EXEC_DEF_VERSION.to_string(),
            state: ExecState::Pending,
            phase: ExecutionPhase::Intake,
            attempt: 0,
            revision: 0,
            operations: BTreeMap::new(),
            directives: BTreeMap::new(),
            fired_timers: BTreeMap::new(),
            kill_latched: false,
            parked_reason: None,
        }
    }

    /// A parked state for `execution_id` with `reason`.
    pub fn parked(execution_id: &ExecutionId, reason: impl Into<String>) -> Self {
        let mut state = KernelState::unstarted(execution_id);
        state.state = ExecState::Parked;
        state.parked_reason = Some(reason.into());
        state
    }

    /// A copy of this state parked with `reason`.
    pub fn park(&self, reason: impl Into<String>) -> Self {
        let mut state = self.clone();
        state.state = ExecState::Parked;
        state.parked_reason = Some(reason.into());
        state
    }

    /// Whether the state is terminal (no further work).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            ExecState::Completed | ExecState::Failed | ExecState::Parked | ExecState::Cancelled
        )
    }

    /// The coarse projection of this state.
    pub fn projection(&self) -> ExecutionRecord {
        ExecutionRecord {
            execution_id: self.execution_id.clone(),
            task_id: self.task_id.clone(),
            generation: self.generation,
            phase: self.phase,
            state: coarse_state(self.state),
            attempt: self.attempt,
            revision: self.revision,
        }
    }

    /// Whether a stored projection agrees with this reconstructed state.
    ///
    /// Compares the durable projection fields (not the ledger-internal
    /// `revision`, which the ledger owns).
    pub fn matches_projection(&self, record: &ExecutionRecord) -> bool {
        record.execution_id == self.execution_id
            && record.task_id == self.task_id
            && record.generation == self.generation
            && record.phase == self.phase
            && record.attempt == self.attempt
            && record.state == coarse_state(self.state)
    }

    /// The recorded receipt result for an outcome reference: a reference only,
    /// never the artifact body.
    pub fn outcome_result(outcome_ref: &ArtifactRef) -> serde_json::Value {
        serde_json::json!({ "artifact": outcome_ref.as_str() })
    }

    /// Record a completed operation and advance the phase; returns the state.
    pub(crate) fn complete_operation(
        &mut self,
        operation_id: &OperationId,
        inputs_hash: &str,
        outcome_ref: &ArtifactRef,
    ) {
        self.operations.insert(
            operation_id.clone(),
            OperationRecord {
                state: OperationState::Completed,
                inputs_hash: inputs_hash.to_string(),
                outcome_ref: Some(outcome_ref.clone()),
            },
        );
        self.phase = crate::exec_def::advance_phase(self.phase);
        if self.state == ExecState::Ready {
            self.state = ExecState::Running;
        }
        self.bump_revision();
    }

    /// Insert/replace an announced operation record; returns the state.
    pub(crate) fn announce_operation(&mut self, operation_id: &OperationId, inputs_hash: &str) {
        self.operations.insert(
            operation_id.clone(),
            OperationRecord {
                state: OperationState::Dispatched,
                inputs_hash: inputs_hash.to_string(),
                outcome_ref: None,
            },
        );
        if self.state == ExecState::Ready {
            self.state = ExecState::Running;
        }
        self.bump_revision();
    }

    pub(crate) fn bump_revision(&mut self) {
        self.revision += 1;
    }
}
