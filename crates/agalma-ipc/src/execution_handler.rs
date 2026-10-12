//! Caller-side handler-registry protocol for the `ExecutionApi` process
//! boundary (B1-F1, `agalma-5de`).
//!
//! ## Why a protocol
//!
//! The kernel separates the **pure transition** (`KernelEngine::apply`) from the
//! **external effect dispatch** (`run_activity`/`recover_pending`), which invokes
//! a caller-registered [`agalma_kernel::ActivityHandler`]. Those handlers are
//! Rust closures built from consumer code and cannot cross a process boundary
//! generically, so the B1.7 kernel server returned `UnsupportedCapability` for
//! `ExecutionApi::step`/`recover`.
//!
//! This module defines the **handler-registry protocol**: the engine (durable
//! owner) *begins* an operation and hands the caller a serializable
//! [`EffectContext`]; the caller performs the effect through its local
//! [`EffectDispatch`] registry and *completes* the operation with a serializable
//! [`EffectReport`]. The durable announce/commit half stays server-side; only the
//! nondeterministic effect half crosses the boundary — exactly the split the
//! kernel already draws between transition and handler.
//!
//! The protocol is transport-agnostic: `agalma-ipc` carries it over the framed
//! socket, but `KernelEngine<L>` implements the same three operations in-process
//! (`begin_step`/`begin_pending_step`/`complete_step`), so an in-process binding
//! and a process binding expose one surface.

use agalma_contracts::{
    ArtifactRef, ExecutionId, ExecutionPhase, OperationId, StepOutcome, TaskId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The versioned activity identity a handler is registered under (`name@version`).
///
/// Mirrors the kernel's `ActivityDef`; kept here because `agalma-ipc` is
/// infrastructure and must not depend on `agalma-kernel`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HandlerKey {
    pub name: String,
    pub version: String,
}

impl HandlerKey {
    /// Build a key.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        HandlerKey {
            name: name.into(),
            version: version.into(),
        }
    }

    /// Registry key `name@version`.
    pub fn key(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// Where an operation stands when it is handed to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectMode {
    /// A brand-new operation: its dispatch intent is already durable; the caller
    /// executes the effect.
    Dispatch,
    /// A pending (in-flight) operation: the caller probes the external effect,
    /// then executes it only when the probe reports it absent.
    Reconcile,
}

/// Everything a caller-side handler needs to perform one operation.
///
/// This is the wire-safe projection of the kernel's `ActivityContext`; the
/// `binding` pin and opaque `inputs` cross as JSON so a recovery under a new
/// process binding reconciles against the *pinned* binding, never the currently
/// configured adapter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectContext {
    pub execution_id: ExecutionId,
    pub operation_id: OperationId,
    pub task_id: TaskId,
    pub phase: ExecutionPhase,
    pub attempt: u32,
    pub activity: HandlerKey,
    pub binding: Option<Value>,
    pub inputs: Value,
    pub now_unix_ms: u64,
    pub mode: EffectMode,
}

/// A caller-side probe of a pending operation's external effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EffectProbe {
    /// The effect is present; the carried outcome completes it without repeating.
    Present(ArtifactRef),
    /// The effect is observably absent; it is safe to (re-)execute.
    Absent,
    /// Cannot determine whether the effect happened; park rather than duplicate.
    Ambiguous,
}

/// Errors a caller-side handler may return.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandlerError {
    /// The effect failed for a known reason.
    KnownFailure(String),
    /// The outcome is unknown; leave the operation pending for reconciliation.
    UnknownOutcome(String),
}

/// The caller's report after performing (or probing) an effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EffectReport {
    /// The probe found the effect already present with this outcome.
    Present { outcome_ref: ArtifactRef },
    /// The effect was executed now (a fresh dispatch, or a reconcile whose probe
    /// reported absent).
    Executed { outcome_ref: ArtifactRef },
    /// Ambiguous effect; the engine parks the execution.
    Ambiguous,
    /// The handler failed for a known reason.
    KnownFailure { message: String },
    /// The handler's outcome is unknown; the execution parks for reconciliation.
    UnknownOutcome { message: String },
}

/// Caller-hosted handler registry: the half of the protocol that cannot cross the
/// boundary because it holds real effect code.
///
/// The conductor implements this over its phase activities; tests implement it
/// with fakes. It must be `Send` so a [`crate::ExecutionClient`] carrying one
/// stays `Send`.
pub trait EffectDispatch: Send {
    /// Whether a handler is registered for `activity`.
    fn contains(&self, activity: &HandlerKey) -> bool;

    /// Probe a pending operation's external effect (reconcile mode).
    fn probe(&mut self, ctx: &EffectContext) -> Result<EffectProbe, HandlerError>;

    /// Perform the effect, returning the outcome reference to record.
    fn execute(&mut self, ctx: &EffectContext) -> Result<ArtifactRef, HandlerError>;
}

/// The engine's answer to a begin call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StepBegin {
    /// The caller must perform this effect, then call `complete_step`.
    Effect(EffectContext),
    /// No effect is needed: the operation is already settled, or the execution is
    /// parked/blocked/idle. The carried [`StepOutcome`] is the step result.
    Done(StepOutcome),
}
