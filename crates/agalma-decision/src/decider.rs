//! Durable decision orchestration over [`LedgerApi`].
//!
//! The decider is the Rust-owned apply path from architecture §3.7:
//!
//! 1. A recorded outcome for the operation is reused unchanged (recovery and
//!    late/duplicate results cannot supersede an applied outcome).
//! 2. The request is persisted before any apply (schema v2 `decisions`),
//!    enforcing "changed inputs → new operation ID".
//! 3. The candidate response is validated against the versioned contract
//!    (offered IDs, finite scores); a timeout, unavailable inference, malformed
//!    output, or abstention falls back to the pinned static baseline after
//!    rechecking eligibility. If the baseline cannot act, the outcome parks.
//! 4. The applied outcome is recorded before dependent work is scheduled. The
//!    apply path re-checks the terminal record first.

use agalma_contracts::{
    ContractError, DecisionApi, DecisionAttempt, DecisionOutcome, DecisionRequest,
    DecisionResponse, FallbackReason, LedgerApi,
};

use crate::baseline::{as_outcome, baseline_name, baseline_outcome, BaselineOutcome};

/// Static-policy decider backed by a durable ledger.
///
/// M1 runs the static baseline directly (no model inference); the same type
/// accepts a candidate [`DecisionResponse`] for validation and fallback.
pub struct StaticDecider<L: LedgerApi> {
    ledger: L,
}

impl<L: LedgerApi> StaticDecider<L> {
    /// Wrap a ledger.
    pub fn new(ledger: L) -> Self {
        Self { ledger }
    }

    /// Consume the decider, returning the wrapped ledger.
    pub fn into_ledger(self) -> L {
        self.ledger
    }

    /// Borrow the wrapped ledger.
    pub fn ledger(&self) -> &L {
        &self.ledger
    }

    /// Mutably borrow the wrapped ledger.
    pub fn ledger_mut(&mut self) -> &mut L {
        &mut self.ledger
    }

    /// Resolve a decision without marking it durable (pure, for tests and
    /// previews). Durable callers use [`DecisionApi::decide`].
    pub fn resolve(request: &DecisionRequest, attempt: DecisionAttempt) -> DecisionOutcome {
        match attempt {
            DecisionAttempt::Baseline => as_outcome(baseline_outcome(request)),
            DecisionAttempt::Response(response) => match validate_response(request, &response) {
                Ok(validated) => DecisionOutcome::Applied {
                    chosen: validated.chosen,
                    scores: validated.scores,
                },
                Err(reason) => fallback(request, reason),
            },
            DecisionAttempt::Timeout => fallback(request, FallbackReason::Timeout),
            DecisionAttempt::Unavailable => fallback(request, FallbackReason::Unavailable),
            DecisionAttempt::Malformed => fallback(request, FallbackReason::Malformed),
            DecisionAttempt::Abstention => fallback(request, FallbackReason::Abstention),
        }
    }
}

impl<L: LedgerApi> DecisionApi for StaticDecider<L> {
    fn decide(
        &mut self,
        request: &DecisionRequest,
        attempt: DecisionAttempt,
    ) -> Result<DecisionOutcome, ContractError> {
        // Persist the request before apply; reusing the operation id with
        // changed contents is a conflict (the caller must use a new id).
        self.ledger.record_decision(request)?;

        // Reuse a terminal outcome: recovery must not ask again, and a late or
        // duplicate result cannot supersede an applied outcome.
        if let Some(existing) = self.ledger.decision_outcome(&request.operation_id)? {
            return Ok(existing);
        }

        let outcome = Self::resolve(request, attempt);

        // Apply-path guard: re-check for a terminal record before recording, so
        // a concurrent apply cannot be overwritten.
        if let Some(existing) = self.ledger.decision_outcome(&request.operation_id)? {
            return Ok(existing);
        }
        self.ledger
            .record_decision_outcome(&request.operation_id, &outcome)?;
        Ok(outcome)
    }
}

/// Fallback to the pinned baseline after rechecking eligibility. If the
/// baseline cannot act, the outcome parks.
fn fallback(request: &DecisionRequest, reason: FallbackReason) -> DecisionOutcome {
    match baseline_outcome(request) {
        BaselineOutcome::Action { chosen, scores } => DecisionOutcome::Fallback {
            baseline: baseline_name(request.kind).to_string(),
            reason,
            chosen,
            scores,
        },
        BaselineOutcome::NoAction { reason: park } => DecisionOutcome::Parked {
            reason: format!("baseline cannot act: {park}"),
        },
    }
}

/// Validate a candidate response against the versioned contract: chosen and
/// scored IDs must be offered, scores and confidence must be finite and in
/// `[0.0, 1.0]`, and an empty response is an abstention.
fn validate_response(
    request: &DecisionRequest,
    response: &DecisionResponse,
) -> Result<DecisionResponse, FallbackReason> {
    if let Some(chosen) = &response.chosen {
        if !request.offers(chosen) {
            return Err(FallbackReason::Malformed);
        }
    }
    for score in &response.scores {
        if !request.offers(&score.option_id) || !(0.0..=1.0).contains(&score.score) {
            return Err(FallbackReason::Malformed);
        }
    }
    if let Some(confidence) = response.confidence {
        if !(0.0..=1.0).contains(&confidence) {
            return Err(FallbackReason::Malformed);
        }
    }
    if response.chosen.is_none() && response.scores.is_empty() {
        return Err(FallbackReason::Abstention);
    }
    Ok(response.clone())
}
