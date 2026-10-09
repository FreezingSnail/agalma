//! Frozen `HarnessApi` contract for the S0d seam-freeze spike.
//!
//! This module is the seam. It contains only canonical Agalma types: requests,
//! results, events, errors, capability names, and canonical ID helpers. It has
//! no implementation dependencies and — critically for the privacy check — no
//! vendor names, endpoints, transports, or native identifier shapes. Consumers
//! (the scenario driver) import this module and nothing else.
//!
//! Scope: spike-only Rust freeze. Types here are the candidate for the product
//! contract package; see `docs/spikes/s0d-seam-freeze.md` Results.

use std::collections::BTreeSet;
use std::fmt;

/// Canonical API name reported by `Describe`.
pub const HARNESS_API: &str = "HarnessApi";
/// Contract API major version. `0` during the spike.
pub const API_VERSION: u32 = 0;

// Optional capability strings (the six from the contract). A binding may
// advertise any subset; absence means the conductor must choose a declared
// supported plan instead of the capability.
pub const CAP_SESSION_FORK: &str = "session_fork";
pub const CAP_SYNTHETIC_FEEDBACK: &str = "synthetic_feedback";
pub const CAP_MODEL_SWITCH: &str = "model_switch";
pub const CAP_CONSTRAINED_GENERATION: &str = "constrained_generation";
pub const CAP_COMPACTION: &str = "compaction";
pub const CAP_CONTEXT_HOOKS: &str = "context_hooks";

/// All six optional capability strings, in a stable order.
pub fn optional_capabilities() -> [&'static str; 6] {
    [
        CAP_SESSION_FORK,
        CAP_SYNTHETIC_FEEDBACK,
        CAP_MODEL_SWITCH,
        CAP_CONSTRAINED_GENERATION,
        CAP_COMPACTION,
        CAP_CONTEXT_HOOKS,
    ]
}

/// The `mvp-baseline` profile requires only the baseline operations
/// (`StartAttempt`, `CreateSession`, `RunTurn`, `InspectOperation`, `ReadEvents`,
/// `CancelOperation`, `CloseSession`, `StopAttempt`) and no optional capability.
/// The returned set is the required optional-capability set: empty.
pub fn mvp_baseline_required() -> Vec<&'static str> {
    Vec::new()
}

/// Canonical attempt ID: `attempt:<n>`.
pub fn attempt_id(n: u32) -> String {
    format!("attempt:{n}")
}

/// Canonical session ID: `session:<attempt>:<n>`.
pub fn session_id(attempt: u32, n: u32) -> String {
    format!("session:{attempt}:{n}")
}

/// Canonical operation ID: `op:<attempt>:turn/<n>`.
pub fn operation_id(attempt: u32, turn: u32) -> String {
    format!("op:{attempt}:turn/{turn}")
}

/// Canonical artifact reference: `artifact:<name>`.
pub fn artifact_ref(name: &str) -> String {
    format!("artifact:{name}")
}

/// `Describe` result: implementation identity and advertised capabilities.
#[derive(Clone, Debug, PartialEq)]
pub struct Describe {
    pub api: String,
    pub api_version: u32,
    pub impl_name: String,
    pub impl_version: String,
    pub capabilities: BTreeSet<String>,
}

/// Resource limits attached to an attempt. Values are advisory for the spike.
#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    pub wall_ms: u64,
    pub tokens: u64,
    pub cost_micros: u64,
}

/// `StartAttempt` request. `workspace` is the attempt-owned fixture directory.
#[derive(Clone, Debug, PartialEq)]
pub struct StartAttemptRequest {
    pub workspace: String,
    pub role: String,
    pub genome_ref: String,
    pub constraints_ref: String,
    pub limits: Limits,
    pub operation_id: String,
}

/// `CreateSession` request: a fresh phase context from canonical inputs.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateSessionRequest {
    pub role: String,
    pub model: String,
    pub prompts_ref: String,
    pub handoff_refs: Vec<String>,
    pub tool_policy_ref: String,
}

/// `RunTurn` request: a bounded turn over an input artifact.
#[derive(Clone, Debug, PartialEq)]
pub struct RunTurnRequest {
    pub input_ref: String,
    pub bounded_turns: u32,
    pub deadline_ms: u64,
}

/// Usage completeness is always declared, never implied by a silent zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completeness {
    Complete,
    Partial,
    Unknown,
}

impl Completeness {
    pub fn label(self) -> &'static str {
        match self {
            Completeness::Complete => "Complete",
            Completeness::Partial => "Partial",
            Completeness::Unknown => "Unknown",
        }
    }
}

/// Normalized usage snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub completeness: Completeness,
}

/// Operation state. `Unknown` is explicit: it is not a substitute for failure.
#[derive(Clone, Debug, PartialEq)]
pub enum OperationState {
    Running,
    Completed { result_ref: String, usage: Usage },
    Failed { reason: String },
    Unknown,
}

impl OperationState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, OperationState::Completed { .. } | OperationState::Failed { .. })
    }

    pub fn label(&self) -> String {
        match self {
            OperationState::Running => "Running".to_string(),
            OperationState::Completed { result_ref, .. } => format!("Completed {result_ref}"),
            OperationState::Failed { reason } => format!("Failed {reason}"),
            OperationState::Unknown => "Unknown".to_string(),
        }
    }
}

/// Tool outcome status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolResult {
    Ok,
    Error,
}

impl ToolResult {
    pub fn label(self) -> &'static str {
        match self {
            ToolResult::Ok => "ok",
            ToolResult::Error => "error",
        }
    }
}

/// Normalized harness event.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    TurnStarted,
    ToolOutcome { tool: String, outcome: ToolResult, detail: String },
    UsageSnapshot { usage: Usage },
    TurnCompleted { result_ref: String },
    TurnFailed { reason: String },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::TurnStarted => "TurnStarted",
            Event::ToolOutcome { .. } => "ToolOutcome",
            Event::UsageSnapshot { .. } => "UsageSnapshot",
            Event::TurnCompleted { .. } => "TurnCompleted",
            Event::TurnFailed { .. } => "TurnFailed",
        }
    }

    /// Stable one-line rendering used in transcripts and comparability checks.
    pub fn render(&self) -> String {
        match self {
            Event::TurnStarted => "TurnStarted".to_string(),
            Event::ToolOutcome { tool, outcome, detail } => {
                format!("ToolOutcome {} {} {}", outcome.label(), tool, detail)
            }
            Event::UsageSnapshot { usage } => format!(
                "UsageSnapshot in={} out={} cost={:.6} completeness={}",
                usage.tokens_in,
                usage.tokens_out,
                usage.cost_usd,
                usage.completeness.label()
            ),
            Event::TurnCompleted { result_ref } => format!("TurnCompleted {result_ref}"),
            Event::TurnFailed { reason } => format!("TurnFailed {reason}"),
        }
    }
}

/// Contract error taxonomy. A retryable error never by itself authorizes
/// repeating an effect (documented here; not exercised by the spike).
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessError {
    KnownFailure(String),
    UnsupportedCapability(String),
    Conflict(String),
    UnknownOutcome(String),
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HarnessError::KnownFailure(m) => write!(f, "KnownFailure: {m}"),
            HarnessError::UnsupportedCapability(m) => write!(f, "UnsupportedCapability: {m}"),
            HarnessError::Conflict(m) => write!(f, "Conflict: {m}"),
            HarnessError::UnknownOutcome(m) => write!(f, "UnknownOutcome: {m}"),
        }
    }
}

/// Opaque attempt handle. `id` is canonical (`attempt:<n>`).
#[derive(Clone, Debug, PartialEq)]
pub struct AttemptHandle {
    pub id: String,
}

/// Opaque session handle. `id` is canonical (`session:<attempt>:<n>`).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionHandle {
    pub id: String,
}

/// Opaque operation handle. `id` is canonical (`op:<attempt>:turn/<n>`).
#[derive(Clone, Debug, PartialEq)]
pub struct OperationHandle {
    pub id: String,
}

/// Cancellation acknowledgment, deliberately distinct from termination
/// evidence: `acknowledged` means the request was accepted, `terminated` means
/// the adapter has confirmed the effect stopped.
#[derive(Clone, Debug, PartialEq)]
pub struct CancelAck {
    pub acknowledged: bool,
    pub terminated: bool,
    pub detail: String,
}

/// Evidence returned by `StopAttempt`. `process_group_gone` is the cleanup
/// assertion; `detail` is human-readable provenance (no secrets).
#[derive(Clone, Debug, PartialEq)]
pub struct StopEvidence {
    pub process_group_gone: bool,
    pub detail: String,
}

/// The frozen harness seam. A binding selects one implementation; consumers use
/// only this trait plus the types above.
pub trait HarnessAdapter {
    fn describe(&self) -> Describe;

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, HarnessError>;

    fn create_session(&mut self, req: CreateSessionRequest) -> Result<SessionHandle, HarnessError>;

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, HarnessError>;

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, HarnessError>;

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, HarnessError>;

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, HarnessError>;

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), HarnessError>;

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, HarnessError>;
}
