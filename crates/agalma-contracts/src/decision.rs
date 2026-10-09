//! `DecisionApi` — versioned bounded decision contract (architecture §3.7).
//!
//! Rust constructs a versioned [`DecisionRequest`] with a stable operation ID,
//! decision kind, bounded context/artifact references, the eligible option set
//! (filtered by Rust before any inference), a deadline, and the pinned
//! policy/genome/model versions. A bound implementation returns a normalized
//! [`DecisionResponse`] (chosen ID and/or finite scores) or a
//! [`DecisionOutcome`] when it owns fallback semantics.
//!
//! Eligibility filtering, validation, authorization, spending limits,
//! cancellation, acceptance, and integration stay with Rust. This module has no
//! I/O and no vendor types.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ContractError;
use crate::ids::{ArtifactRef, OperationId};

/// Decision contract version carried by requests and responses.
pub const DECISION_API_VERSION: u32 = 1;

/// Bounded decision kinds for M1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionKind {
    /// `triage.pick-next`: pick one ready task from eligible ready tasks.
    TriagePickNext,
    /// `retry.escalate`: mechanical retry / escalation / park choice.
    RetryEscalate,
}

impl DecisionKind {
    /// Stable name used in requests, ledger rows, and baselines.
    pub const fn as_str(self) -> &'static str {
        match self {
            DecisionKind::TriagePickNext => "triage.pick-next",
            DecisionKind::RetryEscalate => "retry.escalate",
        }
    }
}

/// Parse error for [`DecisionKind::from_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownDecisionKind;

impl std::fmt::Display for UnknownDecisionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unknown decision kind")
    }
}

impl std::error::Error for UnknownDecisionKind {}

impl FromStr for DecisionKind {
    type Err = UnknownDecisionKind;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "triage.pick-next" => Ok(DecisionKind::TriagePickNext),
            "retry.escalate" => Ok(DecisionKind::RetryEscalate),
            _ => Err(UnknownDecisionKind),
        }
    }
}

/// Pinned versions that bound one decision: policy, genome, and model/adapter.
///
/// A decision is only valid under exactly these versions; a changed pin is a
/// changed input and requires a new operation ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionPin {
    /// Static policy or decision-template version.
    pub policy: String,
    /// Genome version the decision is evaluated under.
    pub genome: String,
    /// Model/decider adapter version (`static` when no inference ran).
    pub model: String,
}

/// One eligible option offered to a decision.
///
/// `attributes` is bounded, opaque data for the static policy (for example
/// `priority`, `age_ms`, `attempt`, `tier`, `verdict`). It is never a channel
/// for authority.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionOption {
    /// Stable option identifier; the response must echo an offered ID.
    pub option_id: String,
    /// Bounded attributes consumed by the pinned static policy.
    #[serde(default)]
    pub attributes: Value,
}

/// Versioned, bounded decision request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    /// Request schema version.
    pub version: u32,
    /// Stable operation ID; reusing it with changed contents is a conflict.
    pub operation_id: OperationId,
    /// Decision kind.
    pub kind: DecisionKind,
    /// Bounded context (never credentials or authority).
    #[serde(default)]
    pub context: Value,
    /// Immutable artifact references the decision may cite.
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    /// Eligible options, already filtered by Rust. A single option needs no
    /// inference; an empty set waits or parks.
    pub options: Vec<DecisionOption>,
    /// Deadline (unix ms) after which the decision must fall back.
    pub deadline_unix_ms: u64,
    /// Pinned policy/genome/model versions.
    pub pin: DecisionPin,
}

impl DecisionRequest {
    /// Whether `option_id` is one of the offered eligible options.
    pub fn offers(&self, option_id: &str) -> bool {
        self.options.iter().any(|o| o.option_id == option_id)
    }

    /// The offered option with `option_id`, if any.
    pub fn option(&self, option_id: &str) -> Option<&DecisionOption> {
        self.options.iter().find(|o| o.option_id == option_id)
    }
}

/// One finite score over a supplied option ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionScore {
    /// Option the score applies to.
    pub option_id: String,
    /// Finite score (no NaN/infinity).
    pub score: f64,
}

/// Normalized decision response: a chosen ID and/or finite scores, with
/// optional confidence and its declared meaning.
///
/// Confidence is optional; raw scores and generated self-confidence are not
/// assumed calibrated (see §3.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    /// Chosen option ID, if the decider made a choice.
    pub chosen: Option<String>,
    /// Finite scores over supplied option IDs.
    #[serde(default)]
    pub scores: Vec<DecisionScore>,
    /// Optional confidence in `[0.0, 1.0]`.
    pub confidence: Option<f64>,
    /// Declared meaning of `confidence` (required when confidence is set).
    pub confidence_meaning: Option<String>,
}

/// Why a candidate response was not applied and the baseline was used instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FallbackReason {
    /// Inference exceeded the deadline.
    Timeout,
    /// Inference was unavailable (no adapter, refused, crashed).
    Unavailable,
    /// Output failed schema/ID/score validation.
    Malformed,
    /// The decider abstained (no choice, no scores).
    Abstention,
}

impl FallbackReason {
    /// Stable label for ledger rows and evidence.
    pub const fn as_str(self) -> &'static str {
        match self {
            FallbackReason::Timeout => "timeout",
            FallbackReason::Unavailable => "unavailable",
            FallbackReason::Malformed => "malformed",
            FallbackReason::Abstention => "abstention",
        }
    }
}

/// The candidate supplied to a decision, or the signal that none is usable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DecisionAttempt {
    /// No candidate: apply the pinned static baseline directly (M1 default).
    Baseline,
    /// A candidate response to validate before applying.
    Response(DecisionResponse),
    /// Inference timed out.
    Timeout,
    /// Inference was unavailable.
    Unavailable,
    /// Output could not be parsed at all.
    Malformed,
    /// The decider abstained.
    Abstention,
}

/// Applied outcome: a validated choice (candidate, or the static baseline when
/// it acts as the primary policy).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DecisionOutcome {
    /// A validated choice was applied.
    Applied {
        /// Chosen option ID, if any.
        chosen: Option<String>,
        /// Finite scores, if any.
        #[serde(default)]
        scores: Vec<DecisionScore>,
    },
    /// The candidate was unusable; the named baseline was applied after
    /// rechecking eligibility.
    Fallback {
        /// Baseline policy name (with version).
        baseline: String,
        /// Why the candidate was rejected.
        reason: FallbackReason,
        /// Choice the baseline produced.
        chosen: Option<String>,
        /// Finite scores, if any.
        #[serde(default)]
        scores: Vec<DecisionScore>,
    },
    /// No eligible action exists (or the baseline cannot act): wait or park.
    Parked {
        /// Human-readable reason, recorded as evidence.
        reason: String,
    },
}

/// Decision contract. Implementations own fallback semantics; Rust rechecks
/// eligibility and validates before applying any result.
pub trait DecisionApi {
    /// Resolve `request` durably. A recorded outcome is reused and never
    /// superseded (recovery and late/duplicate responses return it unchanged).
    fn decide(
        &mut self,
        request: &DecisionRequest,
        attempt: DecisionAttempt,
    ) -> Result<DecisionOutcome, ContractError>;
}
