//! Activity (handler) model for pluggable phase effects.
//!
//! Each non-terminal phase of the execution pipeline is performed by an
//! [`Activity`]: run the effect ([`Activity::execute`]), probe whether a
//! previous effect survived a crash ([`Activity::effect_present`]), and complete
//! a surviving effect without repeating it ([`Activity::reconcile`]).
//!
//! M0.3 ships the trait plus a scripted test double ([`crate::ScriptedActivity`]);
//! M0.7 wires the real Git/Seatbelt/OpenCode handlers behind it.

use agalma_contracts::{ArtifactRef, ExecutionId, ExecutionPhase, OperationId, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Everything an activity needs to perform one logical operation.
#[derive(Clone, Debug, PartialEq)]
pub struct OperationContext {
    pub execution_id: ExecutionId,
    pub operation_id: OperationId,
    pub task_id: TaskId,
    /// Phase whose effect this operation performs.
    pub phase: ExecutionPhase,
    /// 1-based attempt number (build/verify retries increment it).
    pub attempt: u32,
    /// Lease generation of the claim that owns this operation (M1.6 fencing).
    /// Persisted with the dispatch intent; stale-leased operations are rejected
    /// before they can produce an effect.
    pub lease_generation: u32,
    /// Phase inputs recorded durably with the dispatch intent.
    pub inputs: Value,
}

/// The outcome of one activity execution: the recorded result and what the
/// executor should do next.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivityOutcome {
    /// Persisted as (part of) the operation receipt.
    pub result: Value,
    /// Durable decision that drives the next phase transition.
    pub next: ActivityNext,
}

/// What the executor does after an activity completes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ActivityNext {
    /// Advance to the next non-terminal phase in the pipeline.
    Advance,
    /// The execution is finished.
    Done,
    /// Verify went red: retry the build with a reference to the failure output.
    Retry { failure: ArtifactRef },
    /// Stop and park the execution with a reason.
    Park { reason: String },
}

/// Result of probing for a prior external effect of an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectProbe {
    /// No effect is observable; it is safe to execute.
    Absent,
    /// The effect is present; complete it as reconciled without repeating.
    Present,
    /// Cannot determine whether the effect occurred; park instead of risking a
    /// duplicate (the S0c "ambiguous" case).
    Ambiguous,
}

/// Errors an activity may return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActivityError {
    /// The effect failed for a known reason.
    #[error("activity known failure: {0}")]
    KnownFailure(String),
    /// The outcome is unknown; the executor parks so the effect can be
    /// reconciled on recovery.
    #[error("activity unknown outcome: {0}")]
    UnknownOutcome(String),
}

/// A pluggable phase effect.
pub trait Activity {
    /// Perform the effect. Must be safe to call only after
    /// [`Activity::effect_present`] returned [`EffectProbe::Absent`].
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError>;

    /// Probe whether the effect already happened (e.g. Git ref, merge, worker).
    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe;

    /// Complete a surviving effect without repeating it, returning the outcome
    /// that would have been recorded had the process not crashed.
    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError>;
}
