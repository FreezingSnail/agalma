//! B0.3 decision-integration tests: the request is persisted before dispatch,
//! a recorded outcome is reused on recovery without another inference, a late
//! result cannot overwrite an applied fallback, and changed inputs require a new
//! operation id.
//!
//! State lives under `target/test-runs/recovery-*` via the frozen harness.

mod support;

use agalma_contracts::{
    DecisionApi, DecisionAttempt, DecisionKind, DecisionOption, DecisionOutcome, DecisionPin,
    DecisionRequest, DecisionResponse, ExecutionId, FallbackReason, OperationId, TaskId,
    DECISION_API_VERSION,
};
use agalma_kernel::{run_decision, Command, Inputs, KernelEngine};
use agalma_ledger::SqliteLedger;
use serde_json::json;

use support::harness;
use support::ops::CountingDecider;

fn exec() -> ExecutionId {
    ExecutionId::derive(&TaskId::new("fix-answer"), 1)
}

fn start(engine: &mut KernelEngine<SqliteLedger>) {
    engine
        .apply(
            &exec(),
            &Command::Start {
                task_id: TaskId::new("fix-answer"),
                generation: 1,
            },
            &Inputs::default(),
        )
        .unwrap();
}

fn request(exec: &ExecutionId) -> DecisionRequest {
    DecisionRequest {
        version: DECISION_API_VERSION,
        operation_id: OperationId::derive(exec, "triage"),
        kind: DecisionKind::TriagePickNext,
        context: json!({ "eligible": ["a", "b"] }),
        artifacts: vec![],
        options: vec![
            DecisionOption {
                option_id: "a".to_string(),
                attributes: json!({ "priority": 1, "age_ms": 5 }),
            },
            DecisionOption {
                option_id: "b".to_string(),
                attributes: json!({ "priority": 2, "age_ms": 1 }),
            },
        ],
        deadline_unix_ms: 0,
        pin: DecisionPin {
            policy: "triage.pick-next/v1".to_string(),
            genome: "genome/v0".to_string(),
            model: "static".to_string(),
        },
    }
}

#[test]
fn decision_request_is_persisted_and_duplicate_reuses_it() {
    let (dir, ledger) = harness::open_isolated_ledger("decision-reuse");
    let mut engine = KernelEngine::open(ledger).unwrap();
    start(&mut engine);
    let req = request(&exec());
    let (mut decider, decide_calls, inference_calls) =
        CountingDecider::open(&dir.join("ledger.sqlite"));

    let first = run_decision(
        &mut engine,
        &mut decider,
        &exec(),
        &req,
        DecisionAttempt::Baseline,
        1,
    )
    .unwrap();
    assert!(!first.reused && !first.duplicate && first.conflict.is_none());
    assert!(matches!(first.outcome, DecisionOutcome::Applied { .. }));
    assert_eq!(inference_calls.get(), 1, "one resolution");
    assert_eq!(decide_calls.get(), 1);
    assert!(engine.receipt(&req.operation_id).unwrap().is_some());

    // Duplicate delivery returns the stored outcome without calling the decider.
    let second = run_decision(
        &mut engine,
        &mut decider,
        &exec(),
        &req,
        DecisionAttempt::Baseline,
        2,
    )
    .unwrap();
    assert!(second.duplicate);
    assert_eq!(second.outcome, first.outcome);
    assert_eq!(decide_calls.get(), 1, "duplicate does not call the decider");
    assert_eq!(inference_calls.get(), 1);
}

#[test]
fn recovery_reuses_recorded_outcome_without_inference() {
    let (dir, ledger) = harness::open_isolated_ledger("decision-recovery-reuse");
    let mut engine = KernelEngine::open(ledger).unwrap();
    start(&mut engine);
    let req = request(&exec());

    // Simulate a crash after the outcome committed but before the apply: the
    // decider records the outcome, then the process dies.
    {
        let (mut decider, _, _) = CountingDecider::open(&dir.join("ledger.sqlite"));
        decider.decide(&req, DecisionAttempt::Baseline).unwrap();
    }

    // Recovery opens a fresh decider; it must never be called.
    let (mut fresh, decide_calls, inference_calls) =
        CountingDecider::open(&dir.join("ledger.sqlite"));
    let run = run_decision(
        &mut engine,
        &mut fresh,
        &exec(),
        &req,
        DecisionAttempt::Baseline,
        2,
    )
    .unwrap();
    assert!(run.reused, "recorded outcome is reused");
    assert!(!run.duplicate);
    assert_eq!(decide_calls.get(), 0, "no decider call on recovery");
    assert_eq!(inference_calls.get(), 0, "no second inference call");
    assert!(engine.receipt(&req.operation_id).unwrap().is_some());
}

#[test]
fn late_result_cannot_overwrite_an_applied_fallback() {
    let (dir, ledger) = harness::open_isolated_ledger("decision-late-fallback");
    let mut engine = KernelEngine::open(ledger).unwrap();
    start(&mut engine);
    let req = request(&exec());
    let (mut decider, _, inference_calls) = CountingDecider::open(&dir.join("ledger.sqlite"));

    // Unavailable inference: the pinned baseline is applied and recorded.
    let applied = run_decision(
        &mut engine,
        &mut decider,
        &exec(),
        &req,
        DecisionAttempt::Unavailable,
        1,
    )
    .unwrap();
    let chosen = match &applied.outcome {
        DecisionOutcome::Fallback { chosen, reason, .. } => {
            assert_eq!(*reason, FallbackReason::Unavailable);
            chosen.clone()
        }
        other => panic!("expected a fallback, got {other:?}"),
    };
    assert_eq!(chosen.as_deref(), Some("a"));
    assert_eq!(inference_calls.get(), 1);

    // A late, valid inference result arrives with the same operation id.
    let late = DecisionResponse {
        chosen: Some("b".to_string()),
        scores: vec![],
        confidence: None,
        confidence_meaning: None,
    };
    let after = run_decision(
        &mut engine,
        &mut decider,
        &exec(),
        &req,
        DecisionAttempt::Response(late),
        2,
    )
    .unwrap();
    assert!(after.duplicate);
    assert_eq!(
        after.outcome, applied.outcome,
        "the late result cannot overwrite the applied fallback"
    );
    assert_eq!(inference_calls.get(), 1, "no new inference");
}

#[test]
fn changed_inputs_require_a_new_operation_id() {
    let (dir, ledger) = harness::open_isolated_ledger("decision-changed-inputs");
    let mut engine = KernelEngine::open(ledger).unwrap();
    start(&mut engine);
    let req = request(&exec());
    let (mut decider, _, _) = CountingDecider::open(&dir.join("ledger.sqlite"));

    let applied = run_decision(
        &mut engine,
        &mut decider,
        &exec(),
        &req,
        DecisionAttempt::Baseline,
        1,
    )
    .unwrap();
    assert!(applied.conflict.is_none());

    // The same operation id with changed contents is rejected.
    let mut changed = request(&exec());
    changed.context = json!({ "eligible": ["a"] });
    let rejected = run_decision(
        &mut engine,
        &mut decider,
        &exec(),
        &changed,
        DecisionAttempt::Baseline,
        2,
    )
    .unwrap();
    assert!(rejected.conflict.is_some(), "changed inputs must conflict");
}
