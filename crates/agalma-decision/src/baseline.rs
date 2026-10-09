//! Static decision baselines (architecture §3.7).
//!
//! Each decision kind has a versioned static baseline that is deterministic and
//! needs no inference. The baseline is used directly in M1 and as the fallback
//! whenever a candidate response is unusable. It only ever chooses among the
//! eligible options Rust supplied; it cannot admit an unevaluated option.

use agalma_contracts::{
    DecisionKind, DecisionOption, DecisionOutcome, DecisionRequest, DecisionScore,
};

/// Versioned baseline name for `triage.pick-next`.
pub const TRIAGE_PICK_NEXT_BASELINE: &str = "triage.pick-next/v1";
/// Versioned baseline name for `retry.escalate`.
pub const RETRY_ESCALATE_BASELINE: &str = "retry.escalate/v1";

/// A baseline result: either a concrete choice among the offered options, or a
/// reasoned no-action (wait/park).
#[derive(Clone, Debug, PartialEq)]
pub enum BaselineOutcome {
    /// The baseline chose an offered option.
    Action {
        chosen: Option<String>,
        scores: Vec<DecisionScore>,
    },
    /// The baseline cannot act (no eligible action): wait or park.
    NoAction { reason: String },
}

/// The pinned baseline name for a decision kind.
pub const fn baseline_name(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::TriagePickNext => TRIAGE_PICK_NEXT_BASELINE,
        DecisionKind::RetryEscalate => RETRY_ESCALATE_BASELINE,
    }
}

/// Run the pinned static baseline for `request`.
pub fn baseline_outcome(request: &DecisionRequest) -> BaselineOutcome {
    match request.kind {
        DecisionKind::TriagePickNext => triage_pick_next(request),
        DecisionKind::RetryEscalate => retry_escalate(request),
    }
}

/// `triage.pick-next`: order by priority (lowest value first), then age (oldest
/// first), then option id (ascending). Deterministic over the offered set.
pub fn triage_pick_next(request: &DecisionRequest) -> BaselineOutcome {
    let mut best: Option<(&DecisionOption, i64, u64)> = None;
    for option in &request.options {
        let priority = attr_i64(&option.attributes, "priority").unwrap_or(0);
        let age_ms = attr_u64(&option.attributes, "age_ms").unwrap_or(0);
        let is_better = match &best {
            None => true,
            Some((current, current_priority, current_age)) => {
                priority < *current_priority
                    || (priority == *current_priority && age_ms > *current_age)
                    || (priority == *current_priority
                        && age_ms == *current_age
                        && option.option_id < current.option_id)
            }
        };
        if is_better {
            best = Some((option, priority, age_ms));
        }
    }

    match best {
        Some((option, _, _)) => BaselineOutcome::Action {
            chosen: Some(option.option_id.clone()),
            scores: vec![],
        },
        None => BaselineOutcome::NoAction {
            reason: "no eligible options".to_string(),
        },
    }
}

/// `retry.escalate`: mechanical choice from attempt count and verdict.
///
/// Context fields (all optional, with defaults):
/// `attempt` (default 1), `max_attempts` (default 1), `tier` (default 0),
/// `max_tier` (default 0), `verdict` (`retryable` | `non_retryable` | `fatal`).
///
/// Rules, in order:
/// - `verdict` is a non-retryable verdict → `park`;
/// - `attempt < max_attempts` → `retry`;
/// - `tier < max_tier` → `escalate`;
/// - otherwise → `park`.
///
/// The chosen action must be an offered option (matched by option id or by its
/// `action` attribute); otherwise the baseline cannot act.
pub fn retry_escalate(request: &DecisionRequest) -> BaselineOutcome {
    if request.options.is_empty() {
        return BaselineOutcome::NoAction {
            reason: "no eligible options".to_string(),
        };
    }

    let attempt = context_u64(&request.context, "attempt").unwrap_or(1);
    let max_attempts = context_u64(&request.context, "max_attempts").unwrap_or(1);
    let tier = context_u64(&request.context, "tier").unwrap_or(0);
    let max_tier = context_u64(&request.context, "max_tier").unwrap_or(0);
    let verdict = context_str(&request.context, "verdict").unwrap_or_default();

    let desired = if matches!(
        verdict.as_str(),
        "non_retryable" | "fatal" | "deterministic_failure"
    ) {
        "park"
    } else if attempt < max_attempts {
        "retry"
    } else if tier < max_tier {
        "escalate"
    } else {
        "park"
    };

    match find_action(request, desired) {
        Some(option_id) => BaselineOutcome::Action {
            chosen: Some(option_id),
            scores: vec![],
        },
        None => BaselineOutcome::NoAction {
            reason: format!("action `{desired}` is not an eligible option"),
        },
    }
}

/// Find the offered option that realizes `action`, by option id or `action`
/// attribute.
fn find_action(request: &DecisionRequest, action: &str) -> Option<String> {
    request
        .options
        .iter()
        .find(|o| {
            o.option_id == action || attr_str(&o.attributes, "action").as_deref() == Some(action)
        })
        .map(|o| o.option_id.clone())
}

/// A baseline [`BaselineOutcome`] as a recorded decision outcome.
pub fn as_outcome(outcome: BaselineOutcome) -> DecisionOutcome {
    match outcome {
        BaselineOutcome::Action { chosen, scores } => DecisionOutcome::Applied { chosen, scores },
        BaselineOutcome::NoAction { reason } => DecisionOutcome::Parked { reason },
    }
}

fn attr_i64(value: &serde_json::Value, key: &str) -> Option<i64> {
    value.get(key).and_then(serde_json::Value::as_i64)
}

fn attr_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn attr_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn context_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn context_str(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use agalma_contracts::{
        DecisionKind, DecisionOption, DecisionPin, DecisionRequest, OperationId,
    };

    use super::*;

    fn request(
        kind: DecisionKind,
        context: serde_json::Value,
        options: Vec<DecisionOption>,
    ) -> DecisionRequest {
        DecisionRequest {
            version: 1,
            operation_id: OperationId::new("op:test"),
            kind,
            context,
            artifacts: vec![],
            options,
            deadline_unix_ms: 0,
            pin: DecisionPin {
                policy: "static".to_string(),
                genome: "genome/v0".to_string(),
                model: "static".to_string(),
            },
        }
    }

    fn option(id: &str, attributes: serde_json::Value) -> DecisionOption {
        DecisionOption {
            option_id: id.to_string(),
            attributes,
        }
    }

    #[test]
    fn triage_orders_by_priority_then_age_then_id() {
        let req = request(
            DecisionKind::TriagePickNext,
            json!({}),
            vec![
                option("b", json!({ "priority": 1, "age_ms": 100 })),
                option("a", json!({ "priority": 1, "age_ms": 100 })),
                option("older", json!({ "priority": 1, "age_ms": 500 })),
                option("urgent", json!({ "priority": 0, "age_ms": 1 })),
            ],
        );
        // priority 0 wins before age; then older age; then lexical id.
        assert_eq!(
            triage_pick_next(&req),
            BaselineOutcome::Action {
                chosen: Some("urgent".to_string()),
                scores: vec![]
            }
        );

        let req_no_urgent = request(
            DecisionKind::TriagePickNext,
            json!({}),
            vec![
                option("a", json!({ "priority": 1, "age_ms": 100 })),
                option("b", json!({ "priority": 1, "age_ms": 100 })),
                option("older", json!({ "priority": 1, "age_ms": 500 })),
            ],
        );
        assert_eq!(
            triage_pick_next(&req_no_urgent),
            BaselineOutcome::Action {
                chosen: Some("older".to_string()),
                scores: vec![]
            }
        );

        let req_tie = request(
            DecisionKind::TriagePickNext,
            json!({}),
            vec![
                option("b", json!({ "priority": 2, "age_ms": 10 })),
                option("a", json!({ "priority": 2, "age_ms": 10 })),
            ],
        );
        assert_eq!(
            triage_pick_next(&req_tie),
            BaselineOutcome::Action {
                chosen: Some("a".to_string()),
                scores: vec![]
            }
        );
    }

    #[test]
    fn triage_is_deterministic_under_input_permutation() {
        let opts = vec![
            option("x", json!({ "priority": 3, "age_ms": 5 })),
            option("y", json!({ "priority": 1, "age_ms": 5 })),
            option("z", json!({ "priority": 1, "age_ms": 50 })),
        ];
        let forward = triage_pick_next(&request(
            DecisionKind::TriagePickNext,
            json!({}),
            opts.clone(),
        ));
        let mut reversed = opts;
        reversed.reverse();
        let backward =
            triage_pick_next(&request(DecisionKind::TriagePickNext, json!({}), reversed));
        assert_eq!(forward, backward);
        assert_eq!(
            forward,
            BaselineOutcome::Action {
                chosen: Some("z".to_string()),
                scores: vec![]
            }
        );
    }

    #[test]
    fn triage_without_options_parks() {
        let req = request(DecisionKind::TriagePickNext, json!({}), vec![]);
        assert!(matches!(
            triage_pick_next(&req),
            BaselineOutcome::NoAction { .. }
        ));
    }

    #[test]
    fn retry_escalate_is_mechanical() {
        let actions = vec![
            option("retry", json!({})),
            option("escalate", json!({})),
            option("park", json!({})),
        ];

        // attempt remaining → retry
        assert_eq!(
            retry_escalate(&request(
                DecisionKind::RetryEscalate,
                json!({ "attempt": 1, "max_attempts": 3, "tier": 0, "max_tier": 2, "verdict": "retryable" }),
                actions.clone()
            )),
            BaselineOutcome::Action {
                chosen: Some("retry".to_string()),
                scores: vec![]
            }
        );

        // attempts exhausted, tier room → escalate
        assert_eq!(
            retry_escalate(&request(
                DecisionKind::RetryEscalate,
                json!({ "attempt": 3, "max_attempts": 3, "tier": 0, "max_tier": 2, "verdict": "retryable" }),
                actions.clone()
            )),
            BaselineOutcome::Action {
                chosen: Some("escalate".to_string()),
                scores: vec![]
            }
        );

        // attempts and tiers exhausted → park
        assert_eq!(
            retry_escalate(&request(
                DecisionKind::RetryEscalate,
                json!({ "attempt": 3, "max_attempts": 3, "tier": 2, "max_tier": 2, "verdict": "retryable" }),
                actions.clone()
            )),
            BaselineOutcome::Action {
                chosen: Some("park".to_string()),
                scores: vec![]
            }
        );

        // non-retryable verdict → park even with attempts remaining
        assert_eq!(
            retry_escalate(&request(
                DecisionKind::RetryEscalate,
                json!({ "attempt": 1, "max_attempts": 3, "tier": 0, "max_tier": 2, "verdict": "non_retryable" }),
                actions
            )),
            BaselineOutcome::Action {
                chosen: Some("park".to_string()),
                scores: vec![]
            }
        );
    }

    #[test]
    fn retry_escalate_without_match_parks() {
        let req = request(
            DecisionKind::RetryEscalate,
            json!({ "attempt": 1, "max_attempts": 3 }),
            vec![option("park", json!({}))],
        );
        assert!(matches!(
            retry_escalate(&req),
            BaselineOutcome::NoAction { .. }
        ));
    }
}
