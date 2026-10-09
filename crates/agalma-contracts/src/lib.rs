//! Canonical Agalma contracts.
//!
//! This crate is the frozen seam between components. It contains canonical
//! identifiers, normalized events, the error taxonomy, binding records, and the
//! `LedgerApi`/`ExecutionApi`/`SandboxApi`/`WorkspaceApi`/`HarnessApi` trait
//! signatures. It has no I/O, no vendor types, and no implementation
//! dependencies: only `serde`, `serde_json`, and `thiserror` (for the error
//! taxonomy).
//!
//! Dependency direction is enforced by the architecture lint in
//! `agalma-conductor`: implementation crates depend on this crate; no
//! implementation crate depends on another implementation crate.

pub mod binding;
pub mod error;
pub mod execution;
pub mod harness;
pub mod ids;
pub mod ledger;
pub mod sandbox;
pub mod workspace;

pub use binding::{BindingRecord, BindingState};
pub use error::ContractError;
pub use execution::{ExecutionApi, ExecutionPhase, ExecutionState, ExecutionStatus, StepOutcome};
pub use harness::{
    mvp_baseline_required, optional_capabilities, AttemptHandle, CancelAck, Completeness,
    CreateSessionRequest, Describe, Event, HarnessApi, Limits, OperationHandle, OperationState,
    RunTurnRequest, SessionHandle, StartAttemptRequest, StopEvidence, ToolResult, Usage,
    API_VERSION, CAP_COMPACTION, CAP_CONSTRAINED_GENERATION, CAP_CONTEXT_HOOKS, CAP_MODEL_SWITCH,
    CAP_SESSION_FORK, CAP_SYNTHETIC_FEEDBACK, HARNESS_API,
};
pub use ids::{ArtifactRef, AttemptId, BindingId, ExecutionId, OperationId, SessionId, TaskId};
pub use ledger::{
    CommitBatch, CommitReceipt, DispatchIntent, ExecutionEvent, ExecutionRecord, LedgerApi,
    OperationReceipt,
};
pub use sandbox::{LaunchSpec, SandboxApi, SandboxChild};
pub use workspace::{Checkout, MergeReceipt, WorkspaceApi};
