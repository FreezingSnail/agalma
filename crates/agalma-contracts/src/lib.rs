//! Canonical Agalma contracts.
//!
//! This crate is the frozen seam between components. It contains canonical
//! identifiers, normalized events, the error taxonomy, binding records, the
//! canonical hashing helper (`canonical-json/v1` + SHA-256, revision 1), the
//! `TaskPackage/v1` record, and the
//! `LedgerApi`/`ExecutionApi`/`ProcessApi`/`WorkspaceApi`/`HarnessApi` trait
//! signatures. It has no I/O and no vendor types: `serde`, `serde_json`,
//! `thiserror` (for the error taxonomy), and the locked `ring = 0.17.14` for
//! SHA-256.
//!
//! Dependency direction is enforced by the architecture lint in
//! `agalma-conductor`: implementation crates depend on this crate; no
//! implementation crate depends on another implementation crate.

pub mod binding;
pub mod budget;
pub mod control_plane;
pub mod decision;
pub mod digest;
pub mod error;
pub mod execution;
pub mod harness;
pub mod ids;
pub mod ledger;
pub mod plugin;
pub mod process;
pub mod snapshot;
pub mod task_package;
pub mod taskqueue;
pub mod workspace;

pub use binding::{BindingRecord, BindingState};
pub use budget::{
    BillablePath, BudgetError, Nusd, PathClass, ProviderRequestProfile, RequestKind, Reservation,
    ReservationState, SpendState, UsageObservation, NUSD_PER_USD, TOKENS_PER_PRICE_UNIT,
};
pub use control_plane::{
    check_cursor, resolve_field_selector, BrowserWire, ComponentSummary, ExactAmount, ExactU64,
    ExecutionSummary, FactoryStop, FieldResolution, Freshness, ListComponents, ReadArtifactChunk,
    ReadArtifactChunkRequest, ReadComponent, ReadCursor, ReadMetadata, ReadOverview, ReadPage,
    ReadPageRequest, SourceAvailability, SourceState, TimelineEntry, UsageCost,
    ARTIFACT_CHUNK_MAX_BYTES, ARTIFACT_TOTAL_MAX_BYTES, CONTROL_PLANE_READ_CAPABILITY,
    CONTROL_PLANE_READ_FAMILY, CONTROL_PLANE_READ_FORMAT, READ_CURSOR_PREFIX,
    READ_PAGE_DEFAULT_LIMIT, READ_PAGE_MAX_BYTES, READ_PAGE_MAX_ENTRIES, READ_PAGE_MAX_LIMIT,
    READ_PAGE_MIN_LIMIT,
};
pub use decision::{
    DecisionApi, DecisionAttempt, DecisionKind, DecisionOption, DecisionOutcome, DecisionPin,
    DecisionRequest, DecisionResponse, DecisionScore, FallbackReason, DECISION_API_VERSION,
};
pub use digest::{
    canonical_digest, canonical_json, failure_code, is_sha256, sha256_digest, sha256_hex,
    validate_digest, FailureCode, CANONICAL_JSON_FORMAT, FAILURE_DETAIL_FORMAT, SHA256_PREFIX,
};
pub use error::ContractError;
pub use execution::{
    CancellationSettlement, CancellationState, ExecutionApi, ExecutionPhase, ExecutionState,
    ExecutionStatus, StepOutcome,
};
pub use harness::{
    mvp_baseline_required, optional_capabilities, AttemptHandle, CancelAck, Completeness,
    CreateSessionRequest, Describe, Event, HarnessApi, Limits, OperationHandle, OperationState,
    RunTurnRequest, SessionHandle, StartAttemptRequest, StopEvidence, ToolResult, Usage,
    API_VERSION, CAP_COMPACTION, CAP_CONSTRAINED_GENERATION, CAP_CONTEXT_HOOKS, CAP_MODEL_SWITCH,
    CAP_SESSION_FORK, CAP_SYNTHETIC_FEEDBACK, HARNESS_API,
};
pub use ids::{ArtifactRef, AttemptId, BindingId, ExecutionId, OperationId, SessionId, TaskId};
pub use ledger::{
    CommitBatch, CommitReceipt, ComponentCommit, ComponentCommitAnchor, ComponentEvent,
    ComponentEventRow, ComponentOperation, ComponentOperationRow, ComponentReceipt,
    ComponentReceiptRow, ComponentStateRow, ControlReceipt, DigestRecord, DirectiveRow,
    DirectiveState, DispatchIntent, ExecutionEvent, ExecutionRecord, LedgerApi, OperationReceipt,
    TimerRecord, TimerState, COMPONENT_COMMIT_FAMILY, COMPONENT_COMMIT_FORMAT,
};
/// The component-store operation lifecycle (distinct from
/// [`harness::OperationState`], re-exported here under a component-qualified
/// name to avoid a root-name collision).
pub use ledger::OperationState as ComponentOperationState;
pub use plugin::{
    missing_approved_capabilities, ActionDescriptor, ActionIntent, ActionScope, Actor, DomainApi,
    Durability, HealthState, ManifestEntrypoint, PluginCancelRequest, PluginHealth,
    PluginInstallation, PluginManifest, PluginOperationStatus, PluginSnapshot,
    PluginSnapshotEntry, ReportEnvelope, ReportPublication, ReportSchemaRef, StateCompatibility,
    View, ViewKind, ACTION_INTENT_FAMILY, ACTION_INTENT_FORMAT, COMPONENT_CHECKPOINT_LIMIT_BYTES,
    COMPONENT_COMMIT_MAX_ROWS, COMPONENT_STORE_CAPABILITY, MANIFEST_DOCUMENT_LIMIT_BYTES,
    PLUGIN_CANCEL_REQUEST_FAMILY, PLUGIN_CANCEL_REQUEST_FORMAT, PLUGIN_HEALTH_FAMILY,
    PLUGIN_HEALTH_FORMAT, PLUGIN_INSTALLATION_FAMILY, PLUGIN_INSTALLATION_FORMAT,
    PLUGIN_MANIFEST_FAMILY, PLUGIN_MANIFEST_FORMAT, PLUGIN_OPERATION_STATUS_FAMILY,
    PLUGIN_OPERATION_STATUS_FORMAT, PLUGIN_SNAPSHOT_FAMILY, PLUGIN_SNAPSHOT_FORMAT,
    PLUGIN_SNAPSHOT_PAYLOAD_LIMIT_BYTES, PROTOCOL_MAJOR, REPORT_ENVELOPE_FAMILY,
    REPORT_ENVELOPE_FORMAT, REPORT_PAYLOAD_LIMIT_BYTES, REPORT_PUBLICATION_FAMILY,
    REPORT_PUBLICATION_FORMAT,
};
pub use process::{ProcessApi, ProcessChild, ProcessSpec};
pub use snapshot::{
    CompleteStateV2, SnapshotDecisionOutcomeRow, SnapshotDecisionRow, COMPLETE_STATE_FORMAT,
    COMPLETE_STATE_SCHEMA_VERSION, StateSnapshot, SNAPSHOT_FORMAT,
};
pub use task_package::{
    Authorization, AuthorizationOrigin, BindingPin, Budget, ContextItem, ContextKind, Definition,
    DirectiveRef, PackageArtifactRef, RepositoryRef, Role, RolePin, Scope, TaskPackage, TextObject,
    UnknownUsage, TASK_PACKAGE_FAMILY, TASK_PACKAGE_FORMAT,
};
pub use taskqueue::{
    admission_definition_canonical, admission_definition_digest, blocking_prerequisite_ids,
    decide_review_admission, normalize_children, normalize_edges, AdmissionChild, AdmissionEdge,
    AdmissionEdgeType, ClaimEvidence, EpicReviewAdmission, ReadyTasks, ReviewAdmissionContext,
    ReviewAdmissionDecision, SkipReason, SkippedIssue, Task, TaskFamily, TaskPriority,
    TaskQueueApi, ADMISSION_STAGED_LABEL, EPIC_REVIEW_ADMISSION_FORMAT,
    REVIEW_ADMISSION_CAPABILITY,
};
pub use workspace::{ChangeKind, Checkout, FileChange, MergeReceipt, WorkspaceApi};
