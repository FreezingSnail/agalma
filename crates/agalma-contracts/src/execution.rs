//! `ExecutionApi` — durable execution state machine.
//!
//! The conductor drives phase transitions through this contract; the
//! implementation persists state through `LedgerApi`. Recovery reconstructs
//! state from recorded events without I/O, then reconciles pending operations.

use serde::{Deserialize, Serialize};

use crate::error::ContractError;
use crate::ids::{ExecutionId, OperationId, TaskId};

/// Conductor phases for a single execution (M0 fixed pipeline).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionPhase {
    Intake,
    Checkout,
    Build,
    Verify,
    Integrate,
    Done,
    Parked,
}

/// Durable execution state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionState {
    Pending,
    Running,
    Completed,
    Failed,
    Parked,
}

/// Projection of an execution's durable status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionStatus {
    pub execution_id: ExecutionId,
    pub phase: ExecutionPhase,
    pub state: ExecutionState,
    pub attempt: u32,
    /// Unconsumed dispatch intents for this execution (logical operations that
    /// have been durably announced but not yet completed).
    pub pending_operations: Vec<OperationId>,
    /// Reason recorded when the execution parked, if any.
    pub parked_reason: Option<String>,
}

/// Outcome of one logical step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StepOutcome {
    /// A pending operation was delivered and its receipt recorded. `duplicate`
    /// is true when a recorded receipt was returned instead of repeating the
    /// effect; `reconciled` is true when an already-present effect was
    /// completed without repeating it.
    Dispatched {
        operation: OperationId,
        reconciled: bool,
        duplicate: bool,
    },
    /// The execution advanced into `phase`; `operation` is the newly ready
    /// logical operation (its dispatch intent is durable).
    Advanced {
        phase: ExecutionPhase,
        operation: OperationId,
    },
    /// Nothing to do (terminal phase with no pending work).
    Idle { phase: ExecutionPhase },
    /// Dispatch was blocked (kill latch / cancellation); no effect ran.
    Blocked { operation: Option<OperationId> },
    /// The execution parked with a reason.
    Parked { reason: String },
}

/// Serial executor contract: one logical operation at a time.
pub trait ExecutionApi {
    /// Start (or reconcile) an execution for a task.
    fn start(&mut self, task: &TaskId) -> Result<ExecutionId, ContractError>;

    /// Advance one logical operation. Must be safe to deliver more than once:
    /// completed operations return their recorded receipt.
    fn step(&mut self, execution: &ExecutionId) -> Result<StepOutcome, ContractError>;

    /// Boot: reconstruct state from events, reconcile pending effects, and
    /// resume dispatch unless the kill latch is set.
    fn recover(&mut self) -> Result<(), ContractError>;

    /// Request cancellation; termination requires separate evidence.
    fn cancel(&mut self, execution: &ExecutionId) -> Result<(), ContractError>;

    /// Current status projection.
    fn status(&self, execution: &ExecutionId) -> Result<ExecutionStatus, ContractError>;
}
