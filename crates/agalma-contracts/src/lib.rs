//! Canonical Agalma contracts.
//!
//! This crate is the frozen seam between components. It contains canonical
//! identifiers, normalized events, the error taxonomy, binding records, the
//! canonical hashing helper (`canonical-json/v1` + SHA-256, revision 1), the
//! `TaskPackage/v1` record, and the
//! `LedgerApi`/`ExecutionApi`/`SandboxApi`/`WorkspaceApi`/`HarnessApi` trait
//! signatures. It has no I/O and no vendor types: `serde`, `serde_json`,
//! `thiserror` (for the error taxonomy), and the locked `ring = 0.17.14` for
//! SHA-256.
//!
//! Dependency direction is enforced by the architecture lint in
//! `agalma-conductor`: implementation crates depend on this crate; no
//! implementation crate depends on another implementation crate.

pub mod binding;
pub mod decision;
pub mod digest;
pub mod error;
pub mod execution;
pub mod harness;
pub mod ids;
pub mod ledger;
pub mod sandbox;
pub mod snapshot;
pub mod task_package;
pub mod taskqueue;
pub mod workspace;

pub use binding::{BindingRecord, BindingState};
pub use decision::{
    DecisionApi, DecisionAttempt, DecisionKind, DecisionOption, DecisionOutcome, DecisionPin,
    DecisionRequest, DecisionResponse, DecisionScore, FallbackReason, DECISION_API_VERSION,
};
pub use digest::{
    canonical_digest, canonical_json, failure_code, is_sha256, sha256_digest, sha256_hex,
    validate_digest, FailureCode, CANONICAL_JSON_FORMAT, FAILURE_DETAIL_FORMAT, SHA256_PREFIX,
};
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
    CommitBatch, CommitReceipt, ControlReceipt, DigestRecord, DirectiveRow, DirectiveState,
    DispatchIntent, ExecutionEvent, ExecutionRecord, LedgerApi, OperationReceipt, TimerRecord,
    TimerState,
};
pub use sandbox::{LaunchSpec, SandboxApi, SandboxChild};
pub use snapshot::{StateSnapshot, SNAPSHOT_FORMAT};
pub use task_package::{
    Authorization, AuthorizationOrigin, BindingPin, Budget, ContextItem, ContextKind, Definition,
    DirectiveRef, PackageArtifactRef, RepositoryRef, Role, RolePin, Scope, TaskPackage, TextObject,
    UnknownUsage, TASK_PACKAGE_FAMILY, TASK_PACKAGE_FORMAT,
};
pub use taskqueue::{
    ClaimEvidence, ReadyTasks, SkipReason, SkippedIssue, Task, TaskFamily, TaskPriority,
    TaskQueueApi,
};
pub use workspace::{Checkout, MergeReceipt, WorkspaceApi};
