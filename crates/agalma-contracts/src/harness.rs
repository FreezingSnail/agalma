//! `HarnessApi` — canonical agent-harness contract (S0d surface, folded).
//!
//! Ported from `spikes/s0d/src/contract.rs`. Consumers (the conductor) use only
//! this module plus the rest of `agalma-contracts`; vendor names, endpoints,
//! transports, and native identifiers never appear here. `CancelAck` separates
//! acknowledgment from confirmed termination; `StopEvidence` carries cleanup
//! evidence. A binding may advertise a subset of the optional capabilities; a
//! missing capability forces the conductor to choose a declared supported plan.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::ContractError;
use crate::ids::{ArtifactRef, AttemptId, OperationId, SessionId};

/// Canonical API name reported by `Describe`.
pub const HARNESS_API: &str = "HarnessApi";
/// Contract API major version. `0` matches the frozen S0d surface; a breaking
/// semantic change requires a new major version.
pub const API_VERSION: u32 = 0;

/// Optional capability: explore alternative approaches in parallel.
pub const CAP_SESSION_FORK: &str = "session_fork";
/// Optional capability: inject verifier feedback or interventions mid-run.
pub const CAP_SYNTHETIC_FEEDBACK: &str = "synthetic_feedback";
/// Optional capability: mid-session model/role escalation.
pub const CAP_MODEL_SWITCH: &str = "model_switch";
/// Optional capability: cheap one-shot constrained generation.
pub const CAP_CONSTRAINED_GENERATION: &str = "constrained_generation";
/// Optional capability: context compaction.
pub const CAP_COMPACTION: &str = "compaction";
/// Optional capability: in-turn context/tool hooks.
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

/// The `mvp-baseline` profile requires only the baseline operations and no
/// optional capability. The returned set is the required optional-capability
/// set: empty.
pub fn mvp_baseline_required() -> Vec<&'static str> {
    Vec::new()
}

/// `Describe` result: implementation identity and advertised capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Describe {
    pub api: String,
    pub api_version: u32,
    pub impl_name: String,
    pub impl_version: String,
    pub capabilities: BTreeSet<String>,
}

/// Resource limits attached to an attempt. Values are advisory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub wall_ms: u64,
    pub tokens: u64,
    pub cost_micros: u64,
}

/// `StartAttempt` request. `workspace` is the attempt-owned checkout directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartAttemptRequest {
    pub workspace: String,
    pub role: String,
    pub genome_ref: String,
    pub constraints_ref: String,
    pub limits: Limits,
    pub operation_id: OperationId,
}

/// `CreateSession` request: a fresh phase context from canonical inputs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub role: String,
    pub model: String,
    pub prompts_ref: String,
    pub handoff_refs: Vec<ArtifactRef>,
    pub tool_policy_ref: String,
}

/// `RunTurn` request: a bounded turn over an input artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTurnRequest {
    pub input_ref: ArtifactRef,
    pub bounded_turns: u32,
    pub deadline_ms: u64,
}

/// Usage completeness is always declared, never implied by a silent zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub completeness: Completeness,
}

/// Operation state. `Unknown` is explicit: it is not a substitute for failure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OperationState {
    Running,
    Completed {
        result_ref: ArtifactRef,
        usage: Usage,
    },
    Failed {
        reason: String,
    },
    Unknown,
}

impl OperationState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OperationState::Completed { .. } | OperationState::Failed { .. }
        )
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// Normalized harness event (the S0d set).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    TurnStarted,
    ToolOutcome {
        tool: String,
        outcome: ToolResult,
        detail: String,
    },
    UsageSnapshot {
        usage: Usage,
    },
    TurnCompleted {
        result_ref: ArtifactRef,
    },
    TurnFailed {
        reason: String,
    },
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
            Event::ToolOutcome {
                tool,
                outcome,
                detail,
            } => format!("ToolOutcome {} {} {}", outcome.label(), tool, detail),
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

/// Opaque attempt handle. `id` is canonical (`attempt:<n>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptHandle {
    pub id: AttemptId,
}

/// Opaque session handle. `id` is canonical (`session:<attempt>:<n>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    pub id: SessionId,
}

/// Opaque operation handle. `id` is canonical (`op:<execution_id>:<step>`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationHandle {
    pub id: OperationId,
}

/// Cancellation acknowledgment, deliberately distinct from termination
/// evidence: `acknowledged` means the request was accepted, `terminated` means
/// the adapter has confirmed the effect stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelAck {
    pub acknowledged: bool,
    pub terminated: bool,
    pub detail: String,
}

/// Evidence returned by `StopAttempt`. `process_group_gone` is the cleanup
/// assertion; `detail` is human-readable provenance (no secrets).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopEvidence {
    pub process_group_gone: bool,
    pub detail: String,
}

/// The frozen harness seam. A binding selects one implementation; consumers use
/// only this trait plus the types above.
pub trait HarnessApi {
    fn describe(&self) -> Describe;

    fn start_attempt(&mut self, req: StartAttemptRequest) -> Result<AttemptHandle, ContractError>;

    fn create_session(&mut self, req: CreateSessionRequest)
        -> Result<SessionHandle, ContractError>;

    fn run_turn(
        &mut self,
        session: &SessionHandle,
        req: RunTurnRequest,
    ) -> Result<OperationHandle, ContractError>;

    fn inspect_operation(&mut self, op: &OperationHandle) -> Result<OperationState, ContractError>;

    fn read_events(&mut self, op: &OperationHandle) -> Result<Vec<Event>, ContractError>;

    fn cancel_operation(&mut self, op: &OperationHandle) -> Result<CancelAck, ContractError>;

    fn close_session(&mut self, session: &SessionHandle) -> Result<(), ContractError>;

    fn stop_attempt(&mut self, attempt: &AttemptHandle) -> Result<StopEvidence, ContractError>;
}
