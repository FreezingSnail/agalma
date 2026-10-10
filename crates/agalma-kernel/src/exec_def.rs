//! Versioned execution definition: named states and commands (`exec_def/v1`).
//!
//! The kernel generalizes the M1 fixed phase machine (`docs/b0-kernel.md`
//! §"Execution model and history"): an execution carries one of eight named
//! [`ExecState`]s plus a per-operation [`OperationState`] lifecycle. The
//! definition is versioned as a string (`"exec_def/v1"`); a stored version the
//! kernel does not know parks work rather than being reinterpreted.
//!
//! Commands are the only input to the pure transition ([`crate::transition`]);
//! they are recorded verbatim in history so replay reproduces state.

use agalma_contracts::{ExecutionPhase, OperationId, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activity::ActivityDef;
use crate::binding::BindingPin;

/// Version string of the current execution definition.
pub const EXEC_DEF_VERSION: &str = "exec_def/v1";

/// Kernel implementation version, recorded in `kernel_meta` on first open.
pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Named per-execution lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecState {
    /// Accepted but no work announced yet.
    Pending,
    /// Work can be announced; the execution is not running an effect.
    Ready,
    /// A logical operation is announced/dispatched and in flight.
    Running,
    /// Blocked on a timer, directive, or deadline; no effect in flight.
    Waiting,
    /// Terminal success.
    Completed,
    /// Terminal known failure.
    Failed,
    /// Deliberately halted for a human decision (unknown version, ambiguity,
    /// projection mismatch); never advanced by guessing.
    Parked,
    /// Cancellation requested and terminal.
    Cancelled,
}

/// Per-operation lifecycle state within an execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationState {
    /// Announced: its dispatch intent is durable but the effect has not run.
    Announced,
    /// Handed to an adapter/handler; the effect may be in flight.
    Dispatched,
    /// Receipt recorded (idempotency anchor).
    Completed,
    /// Terminal known failure.
    Failed,
    /// Halted for reconciliation (ambiguous external effect).
    Parked,
    /// Cancelled with the execution.
    Cancelled,
}

/// Recorded input evidence for a transition.
///
/// Every field is *recorded* (read from the ledger or supplied by the caller)
/// so the transition stays pure: no clock, no probe call, no model/tool call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Inputs {
    /// Probe evidence for the external effect of the delivered operation.
    #[serde(default)]
    pub probe: Probe,
    /// Recorded receipt already durable for the operation (duplicate delivery).
    #[serde(default)]
    pub recorded_receipt: Option<agalma_contracts::OperationReceipt>,
    /// Immutable reference to the operation outcome (large artifacts stay
    /// outside history; only this reference is recorded).
    #[serde(default)]
    pub outcome_ref: Option<agalma_contracts::ArtifactRef>,
    /// Recorded decision outcome (for `Command::Decision`).
    #[serde(default)]
    pub decision: Option<agalma_contracts::DecisionOutcome>,
    /// Recorded wall clock used to stamp events/intents in effects. Part of
    /// `inputs` so replay of the same inputs is byte-for-byte deterministic.
    #[serde(default)]
    pub now_unix_ms: u64,
    /// Component/API binding the operation is dispatched under (activity
    /// protocol). Recorded in the dispatch intent so recovery reconciles against
    /// the pinned binding, never the currently configured adapter.
    #[serde(default)]
    pub binding: Option<BindingPin>,
    /// Activity definition that owns this operation (activity protocol). Read
    /// back from the durable dispatch intent on recovery.
    #[serde(default)]
    pub activity: Option<ActivityDef>,
    /// Bounded, opaque operation inputs recorded with the dispatch intent.
    #[serde(default)]
    pub operation_inputs: Option<Value>,
    /// Phase the operation's effect belongs to (activity protocol). Falls back
    /// to the execution's current phase when absent.
    #[serde(default)]
    pub phase: Option<ExecutionPhase>,
}

/// External-effect probe evidence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Probe {
    /// No probe evidence: whether the effect happened is unknown → park.
    #[default]
    Unknown,
    /// The effect is observably absent; safe to dispatch.
    Absent,
    /// The effect is observably present; reconcile without repeating it.
    Present,
}

/// A transition command. Recorded verbatim in history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Begin an execution for a task generation.
    Start { task_id: TaskId, generation: u32 },
    /// Deliver the recorded evidence for a logical operation.
    Deliver {
        operation_id: OperationId,
        /// Content hash of the operation's inputs; reuse under the same id with
        /// a different hash is a conflict.
        inputs_hash: String,
    },
    /// Deliver (or announce) a decider operation.
    Decision {
        operation_id: OperationId,
        inputs_hash: String,
    },
    /// A durable retry/deadline timer fired. Idempotent by `timer_id`.
    TimerFired {
        timer_id: String,
        content_hash: String,
    },
    /// A directive update arrived; bound to `directive_id` + `version`.
    DirectiveUpdate {
        directive_id: String,
        version: u64,
        content_hash: String,
    },
    /// Request cancellation of the execution.
    Cancel,
    /// Record the kill latch state; a latched kernel blocks dispatch.
    KillLatch { latched: bool },
    /// Explicit retry: a *new* operation id (`@attempt`) is announced.
    Retry { operation_id: OperationId },
}

impl Command {
    /// Stable event `kind` recorded in `execution_events`.
    pub fn kind(&self) -> &'static str {
        match self {
            Command::Start { .. } => "start",
            Command::Deliver { .. } => "deliver",
            Command::Decision { .. } => "decision",
            Command::TimerFired { .. } => "timer_fired",
            Command::DirectiveUpdate { .. } => "directive_update",
            Command::Cancel => "cancel",
            Command::KillLatch { .. } => "kill_latch",
            Command::Retry { .. } => "retry",
        }
    }
}

/// The history payload: the command plus the recorded inputs that drove it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KernelEvent {
    pub command: Command,
    pub inputs: Inputs,
}

/// A versioned execution definition: the named states and command kinds a
/// kernel build understands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecDef {
    version: String,
    states: Vec<ExecState>,
    commands: Vec<&'static str>,
}

impl ExecDef {
    /// The current definition (`exec_def/v1`).
    pub fn v1() -> Self {
        ExecDef {
            version: EXEC_DEF_VERSION.to_string(),
            states: vec![
                ExecState::Pending,
                ExecState::Ready,
                ExecState::Running,
                ExecState::Waiting,
                ExecState::Completed,
                ExecState::Failed,
                ExecState::Parked,
                ExecState::Cancelled,
            ],
            commands: vec![
                "start",
                "deliver",
                "decision",
                "timer_fired",
                "directive_update",
                "cancel",
                "kill_latch",
                "retry",
            ],
        }
    }

    /// Resolve a stored version string to a definition, or `None` when unknown
    /// (the caller parks rather than guessing).
    pub fn for_version(version: &str) -> Option<Self> {
        if version == EXEC_DEF_VERSION {
            Some(Self::v1())
        } else {
            None
        }
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn states(&self) -> &[ExecState] {
        &self.states
    }

    pub fn commands(&self) -> &[&'static str] {
        &self.commands
    }
}

/// The coarse projection of an [`ExecState`] onto the shared
/// [`agalma_contracts::ExecutionState`] (the `executions` column). The kernel's
/// canonical state lives in history; this is the durable projection.
pub fn coarse_state(state: ExecState) -> agalma_contracts::ExecutionState {
    use agalma_contracts::ExecutionState;
    match state {
        ExecState::Pending | ExecState::Ready => ExecutionState::Pending,
        ExecState::Running | ExecState::Waiting => ExecutionState::Running,
        ExecState::Completed => ExecutionState::Completed,
        ExecState::Failed | ExecState::Cancelled => ExecutionState::Failed,
        ExecState::Parked => ExecutionState::Parked,
    }
}

/// One recorded operation within a reconstructed [`crate::state::KernelState`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRecord {
    pub state: OperationState,
    /// Content hash of the operation's inputs (conflict anchor).
    pub inputs_hash: String,
    /// Immutable reference to the recorded outcome (never inlined).
    pub outcome_ref: Option<agalma_contracts::ArtifactRef>,
}

/// One recorded directive version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectiveRecord {
    pub version: u64,
    pub content_hash: String,
}

/// Serialize a `Value` into the history payload (never inlines artifacts: the
/// payload only carries identifiers, hashes, and references).
pub(crate) fn encode_event(event: &KernelEvent) -> Value {
    serde_json::to_value(event).unwrap_or(Value::Null)
}

/// Parse a history payload back into a [`KernelEvent`].
pub(crate) fn decode_event(value: &Value) -> Result<KernelEvent, serde_json::Error> {
    serde_json::from_value(value.clone())
}

/// The phase advanced to after a completed operation (M1 pipeline, saturating).
pub fn advance_phase(phase: ExecutionPhase) -> ExecutionPhase {
    match phase {
        ExecutionPhase::Intake => ExecutionPhase::Checkout,
        ExecutionPhase::Checkout => ExecutionPhase::Build,
        ExecutionPhase::Build => ExecutionPhase::Verify,
        ExecutionPhase::Verify => ExecutionPhase::Integrate,
        ExecutionPhase::Integrate => ExecutionPhase::Done,
        ExecutionPhase::Done => ExecutionPhase::Done,
        ExecutionPhase::Parked => ExecutionPhase::Parked,
    }
}

/// Build the operation id for a retry attempt (`op:…@<attempt>`).
pub fn retry_operation_id(operation: &OperationId, attempt: u32) -> OperationId {
    OperationId::new(format!("{}@{}", operation.as_str(), attempt))
}
