//! Decision integration: budgeted model/inference attempts driven through the
//! frozen [`DecisionApi`].
//!
//! `docs/b0-kernel.md` §"Activity protocol" and `docs/architecture.md` §3.7: a
//! model or inference attempt is an explicit *decision* operation. The kernel
//! drives it as follows:
//!
//! 1. The [`DecisionApi`] implementation persists the request before any
//!    dispatch (enforcing "changed inputs → new operation ID").
//! 2. A recorded outcome is reused and applied **without calling the decider
//!    again** — recovery and late/duplicate results never trigger a second
//!    inference.
//! 3. The validated choice/fallback is applied before dependent work is
//!    scheduled, via the pure [`Command::Decision`] transition (receipt + event
//!    in one atomic commit).
//!
//! The kernel depends only on `agalma-contracts`; the concrete decider is
//! injected by the composition root (tests use a counting decider double).

use agalma_contracts::{
    ContractError, DecisionApi, DecisionAttempt, DecisionOutcome, DecisionRequest, ExecutionId,
    LedgerApi, OperationId,
};

use crate::crash::{crash_hook, AFTER_DECISION_COMMIT};
use crate::engine::KernelEngine;
use crate::exec_def::{Command, ExecState, Inputs};
use crate::hash::content_hash;
use crate::transition::Conflict;

/// Deterministic content hash pinning a decision request's inputs.
pub fn decision_inputs_hash(request: &DecisionRequest) -> String {
    content_hash(request)
}

/// Outcome of driving one decision operation.
#[derive(Clone, Debug, PartialEq)]
pub struct DecisionRun {
    pub operation_id: OperationId,
    /// The applied outcome (recorded choice/fallback, or a park).
    pub outcome: DecisionOutcome,
    /// True when the outcome was read from durable state without calling the
    /// decider (recovery / duplicate).
    pub reused: bool,
    /// True when the operation was already fully applied (receipt existed).
    pub duplicate: bool,
    /// Set when the operation id was reused with different contents.
    pub conflict: Option<Conflict>,
}

/// Drive one decision operation for `request` through `decider`, then apply it
/// through the transition.
///
/// A recorded outcome is reused without calling the decider (no second
/// inference); otherwise the decider resolves and durably records the outcome
/// before dependent work is scheduled.
pub fn run_decision<L: LedgerApi>(
    engine: &mut KernelEngine<L>,
    decider: &mut dyn DecisionApi,
    execution_id: &ExecutionId,
    request: &DecisionRequest,
    attempt: DecisionAttempt,
    now_unix_ms: u64,
) -> Result<DecisionRun, ContractError> {
    let operation_id = request.operation_id.clone();
    let inputs_hash = decision_inputs_hash(request);

    // Fully applied already: the receipt is the idempotency anchor.
    if let Some(receipt) = engine.receipt(&operation_id)? {
        if receipt.inputs_hash != inputs_hash {
            let conflict = Conflict {
                id: operation_id.to_string(),
                reason: format!(
                    "decision reused with different inputs (existing={}, new={inputs_hash})",
                    receipt.inputs_hash
                ),
            };
            return Ok(DecisionRun {
                operation_id,
                outcome: DecisionOutcome::Parked {
                    reason: "decision id reused with different inputs".to_string(),
                },
                reused: true,
                duplicate: true,
                conflict: Some(conflict),
            });
        }
        let outcome = serde_json::from_value(receipt.result.clone()).unwrap_or_else(|_| {
            DecisionOutcome::Parked {
                reason: "recorded decision result is undecodable".to_string(),
            }
        });
        return Ok(DecisionRun {
            operation_id,
            outcome,
            reused: true,
            duplicate: true,
            conflict: None,
        });
    }

    let state = engine.load(execution_id)?;
    if state.state == ExecState::Parked {
        return Ok(DecisionRun {
            operation_id,
            outcome: DecisionOutcome::Parked {
                reason: state.parked_reason.unwrap_or_default(),
            },
            reused: false,
            duplicate: false,
            conflict: None,
        });
    }

    if let Some(record) = state.operations.get(&operation_id) {
        if record.inputs_hash != inputs_hash {
            let conflict = Conflict {
                id: operation_id.to_string(),
                reason: format!(
                    "decision reused with different inputs (existing={}, new={inputs_hash})",
                    record.inputs_hash
                ),
            };
            return Ok(DecisionRun {
                operation_id,
                outcome: DecisionOutcome::Parked {
                    reason: "decision id reused with different inputs".to_string(),
                },
                reused: false,
                duplicate: false,
                conflict: Some(conflict),
            });
        }
    }

    // Recovery reuse: a recorded outcome is applied without another inference.
    let (outcome, reused) = match engine.ledger().decision_outcome(&operation_id)? {
        Some(recorded) => (recorded, true),
        None => {
            let resolved = decider.decide(request, attempt)?;
            crash_hook(AFTER_DECISION_COMMIT);
            (resolved, false)
        }
    };

    let apply = engine.apply(
        execution_id,
        &Command::Decision {
            operation_id: operation_id.clone(),
            inputs_hash,
        },
        &Inputs {
            decision: Some(outcome.clone()),
            now_unix_ms,
            ..Inputs::default()
        },
    )?;

    Ok(DecisionRun {
        operation_id,
        outcome,
        reused,
        duplicate: false,
        conflict: apply.conflict,
    })
}
