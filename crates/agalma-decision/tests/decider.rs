//! Co-located decision tests (native Rust, `cargo test -p agalma-decision`).
//!
//! Run directories live under `target/test-runs/decision-*` (never a system
//! temp directory). The decider is exercised end-to-end through the real SQLite
//! ledger so persistence and recovery reuse are covered, not just the pure
//! baselines.

use std::path::{Path, PathBuf};

use agalma_contracts::{
    ContractError, DecisionApi, DecisionAttempt, DecisionKind, DecisionOption, DecisionOutcome,
    DecisionPin, DecisionRequest, DecisionResponse, DecisionScore, FallbackReason, LedgerApi,
    OperationId,
};
use agalma_decision::StaticDecider;
use agalma_ledger::SqliteLedger;

fn run_dir(name: &str) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target.join("test-runs").join(format!("decision-{name}"))
}

fn reset_dir(dir: &Path) {
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("reset run dir");
    }
    std::fs::create_dir_all(dir).expect("create run dir");
}

fn option(id: &str, priority: i64, age_ms: u64) -> DecisionOption {
    DecisionOption {
        option_id: id.to_string(),
        attributes: serde_json::json!({ "priority": priority, "age_ms": age_ms }),
    }
}

fn triage_request(op: &str) -> DecisionRequest {
    DecisionRequest {
        version: 1,
        operation_id: OperationId::new(op),
        kind: DecisionKind::TriagePickNext,
        context: serde_json::json!({}),
        artifacts: vec![],
        options: vec![
            option("task-b", 2, 10),
            option("task-a", 1, 5),
            option("task-c", 1, 50),
        ],
        deadline_unix_ms: 0,
        pin: DecisionPin {
            policy: "triage.pick-next/v1".to_string(),
            genome: "genome/v0".to_string(),
            model: "static".to_string(),
        },
    }
}

fn applied(chosen: &str) -> DecisionOutcome {
    DecisionOutcome::Applied {
        chosen: Some(chosen.to_string()),
        scores: vec![],
    }
}

#[test]
fn baseline_applied_directly_and_reused_on_reopen() {
    let dir = run_dir("baseline");
    reset_dir(&dir);
    let path = dir.join("ledger.sqlite");
    let request = triage_request("op:exec:t1:1:triage");

    let first = {
        let ledger = SqliteLedger::open(&path).unwrap();
        let mut decider = StaticDecider::new(ledger);
        let outcome = decider.decide(&request, DecisionAttempt::Baseline).unwrap();
        // task-c: priority 1, oldest (age 50) beats task-a (priority 1, age 5).
        assert_eq!(outcome, applied("task-c"));
        outcome
    };

    // Reopen (crash/recovery): the recorded outcome is reused, never re-decided.
    let ledger = SqliteLedger::open(&path).unwrap();
    let mut decider = StaticDecider::new(ledger);
    let reused = decider.decide(&request, DecisionAttempt::Timeout).unwrap();
    assert_eq!(reused, first);
    assert_eq!(
        decider
            .ledger()
            .decision_outcome(&request.operation_id)
            .unwrap(),
        Some(first)
    );
}

#[test]
fn fallback_on_timeout_unavailable_malformed_abstention() {
    let cases = [
        (DecisionAttempt::Timeout, FallbackReason::Timeout),
        (DecisionAttempt::Unavailable, FallbackReason::Unavailable),
        (DecisionAttempt::Malformed, FallbackReason::Malformed),
        (
            DecisionAttempt::Response(DecisionResponse {
                chosen: Some("not-offered".to_string()),
                scores: vec![],
                confidence: None,
                confidence_meaning: None,
            }),
            FallbackReason::Malformed,
        ),
        (
            DecisionAttempt::Response(DecisionResponse {
                chosen: None,
                scores: vec![],
                confidence: None,
                confidence_meaning: None,
            }),
            FallbackReason::Abstention,
        ),
        (
            DecisionAttempt::Response(DecisionResponse {
                chosen: None,
                scores: vec![DecisionScore {
                    option_id: "task-a".to_string(),
                    score: f64::NAN,
                }],
                confidence: None,
                confidence_meaning: None,
            }),
            FallbackReason::Malformed,
        ),
    ];

    for (index, (attempt, expected)) in cases.into_iter().enumerate() {
        let dir = run_dir(&format!("fallback-{index}"));
        reset_dir(&dir);
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
        let mut decider = StaticDecider::new(ledger);
        let request = triage_request(&format!("op:exec:t1:{index}:triage"));

        let outcome = decider.decide(&request, attempt).unwrap();
        match outcome {
            DecisionOutcome::Fallback {
                baseline,
                reason,
                chosen,
                ..
            } => {
                assert_eq!(reason, expected, "case {index}");
                assert_eq!(baseline, "triage.pick-next/v1", "case {index}");
                assert_eq!(chosen.as_deref(), Some("task-c"), "case {index}");
            }
            other => panic!("case {index}: expected fallback, got {other:?}"),
        }
    }
}

#[test]
fn valid_candidate_is_applied() {
    let dir = run_dir("candidate");
    reset_dir(&dir);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut decider = StaticDecider::new(ledger);
    let request = triage_request("op:exec:t1:candidate");

    let response = DecisionResponse {
        chosen: Some("task-b".to_string()),
        scores: vec![DecisionScore {
            option_id: "task-b".to_string(),
            score: 0.75,
        }],
        confidence: Some(0.75),
        confidence_meaning: Some("relative preference".to_string()),
    };
    let outcome = decider
        .decide(&request, DecisionAttempt::Response(response))
        .unwrap();
    assert_eq!(
        outcome,
        DecisionOutcome::Applied {
            chosen: Some("task-b".to_string()),
            scores: vec![DecisionScore {
                option_id: "task-b".to_string(),
                score: 0.75,
            }],
        }
    );
}

#[test]
fn late_result_cannot_supersede_applied_outcome() {
    let dir = run_dir("late");
    reset_dir(&dir);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut decider = StaticDecider::new(ledger);
    let request = triage_request("op:exec:t1:late");

    let first = decider.decide(&request, DecisionAttempt::Baseline).unwrap();
    assert_eq!(first, applied("task-c"));

    // A late, different candidate must not supersede the applied outcome.
    let late = DecisionResponse {
        chosen: Some("task-b".to_string()),
        scores: vec![],
        confidence: None,
        confidence_meaning: None,
    };
    let again = decider
        .decide(&request, DecisionAttempt::Response(late))
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(
        decider
            .ledger()
            .decision_outcome(&request.operation_id)
            .unwrap(),
        Some(first)
    );
}

#[test]
fn baseline_cannot_act_parks() {
    let dir = run_dir("park");
    reset_dir(&dir);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut decider = StaticDecider::new(ledger);
    let mut request = triage_request("op:exec:t1:park");
    request.options.clear();

    let outcome = decider.decide(&request, DecisionAttempt::Timeout).unwrap();
    match outcome {
        DecisionOutcome::Parked { reason } => assert!(reason.contains("baseline cannot act")),
        other => panic!("expected park, got {other:?}"),
    }
}

#[test]
fn changed_inputs_require_new_operation_id() {
    let dir = run_dir("changed");
    reset_dir(&dir);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut decider = StaticDecider::new(ledger);
    let request = triage_request("op:exec:t1:changed");
    decider.decide(&request, DecisionAttempt::Baseline).unwrap();

    let mut changed = request.clone();
    changed.options = vec![option("task-d", 0, 1)];
    let err = decider
        .decide(&changed, DecisionAttempt::Baseline)
        .unwrap_err();
    assert!(matches!(err, ContractError::Conflict(_)), "err={err:?}");
}

#[test]
fn retry_escalate_flows_through_decider() {
    let dir = run_dir("retry");
    reset_dir(&dir);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut decider = StaticDecider::new(ledger);

    let request = DecisionRequest {
        version: 1,
        operation_id: OperationId::new("op:exec:t1:retry-1"),
        kind: DecisionKind::RetryEscalate,
        context: serde_json::json!({
            "attempt": 1, "max_attempts": 3, "tier": 0, "max_tier": 1, "verdict": "retryable"
        }),
        artifacts: vec![],
        options: vec![
            DecisionOption {
                option_id: "retry".to_string(),
                attributes: serde_json::json!({}),
            },
            DecisionOption {
                option_id: "escalate".to_string(),
                attributes: serde_json::json!({}),
            },
            DecisionOption {
                option_id: "park".to_string(),
                attributes: serde_json::json!({}),
            },
        ],
        deadline_unix_ms: 0,
        pin: DecisionPin {
            policy: "retry.escalate/v1".to_string(),
            genome: "genome/v0".to_string(),
            model: "static".to_string(),
        },
    };

    assert_eq!(
        decider.decide(&request, DecisionAttempt::Baseline).unwrap(),
        applied("retry")
    );
}
