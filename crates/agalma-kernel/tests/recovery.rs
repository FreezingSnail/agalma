//! B0 recovery tests against the protected fixture pack.
//!
//! - Case 1 `transition-commit-before-dispatch`: a child aborts after the
//!   transition commit (`after_transition_commit`) but before dispatch; the
//!   parent reopens the same isolated ledger and proves the durable intent is
//!   recovered and dispatched exactly once.
//! - Case 2 `duplicate-delivery-after-completion`: a completed operation is
//!   delivered again through the activity path; the recorded receipt is returned
//!   and the external effect is not repeated.
//! - Case 3 `restart-after-decision-commit`: a child aborts after the decision
//!   outcome commits (`after_decision_commit`); recovery reuses the recorded
//!   outcome with no second inference call and schedules dependent work.
//! - Case 4 `merge-before-completion`: a child aborts after an integrate effect
//!   lands (`after_merge_before_completion`); recovery reconciles the surviving
//!   effect and completes without re-merging.

mod support;

use std::path::Path;
use std::process::Command as StdCommand;

use agalma_contracts::{
    ArtifactRef, DecisionApi, DecisionAttempt, DecisionKind, DecisionOption, DecisionPin,
    DecisionRequest, ExecutionId, ExecutionPhase, OperationId, TaskId, DECISION_API_VERSION,
};
use agalma_execution::EffectProbe;
use agalma_kernel::{
    recover_pending, run_activity, run_decision, ActivityDef, ActivityRegistry, Command, ExecState,
    Inputs, KernelEngine, OperationRequest, OperationResult, OperationState, Probe,
};
use agalma_ledger::SqliteLedger;
use serde_json::json;

use support::harness::{self, EffectJournal, FakeActivity, RecordingActivity};
use support::ops::{CountingDecider, JournalHandler};

/// Environment variable passing the parent's run directory to the crash child.
const DIR_ENV: &str = "KERNEL_RECOVERY_DIR";

const CASE_1: &str = "transition-commit-before-dispatch";
const CASE_2: &str = "duplicate-delivery-after-completion";
const CASE_3: &str = "restart-after-decision-commit";
const CASE_4: &str = "merge-before-completion";

fn exec() -> ExecutionId {
    ExecutionId::derive(&TaskId::new("fix-answer"), 1)
}

fn start_command() -> Command {
    Command::Start {
        task_id: TaskId::new("fix-answer"),
        generation: 1,
    }
}

fn deliver(op: &OperationId, hash: &str) -> Command {
    Command::Deliver {
        operation_id: op.clone(),
        inputs_hash: hash.to_string(),
    }
}

/// Spawn this test binary as a crash child running `test_name`.
fn spawn_child(test_name: &str, case: &str, dir: &Path) -> std::process::ExitStatus {
    let exe = std::env::current_exe().expect("current_exe");
    StdCommand::new(exe)
        .args([test_name, "--exact", "--test-threads=1", "--nocapture"])
        .env(harness::CHILD_ENV, case)
        .env(DIR_ENV, dir)
        .status()
        .expect("spawn crash child")
}

// ---------------------------------------------------------------------------
// Case 1 — crash after transition commit, before dispatch
// ---------------------------------------------------------------------------

#[test]
fn crash_after_transition_commit_before_dispatch() {
    if harness::child_case().is_some() {
        run_crash_child();
        return;
    }

    let dir = harness::recovery_dir(CASE_1);
    let status = spawn_child(
        "crash_after_transition_commit_before_dispatch",
        CASE_1,
        &dir,
    );
    harness::assert_sigabrt(&status);

    let journal = EffectJournal::in_run_dir(&dir, CASE_1);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let op = OperationId::derive(&exec, "build");

    // Recovery: the committed intent survived; the execution is Running with
    // exactly one pending dispatch and zero recorded effects.
    let state = engine.load(&exec).unwrap();
    assert_eq!(state.state, ExecState::Running);
    assert_eq!(state.operations[&op].state, OperationState::Dispatched);
    let pending = engine.pending_dispatch().unwrap();
    assert_eq!(pending.len(), 1, "pending={pending:?}");
    assert_eq!(pending[0].operation_id, op);
    assert_eq!(journal.effect_count(), 0, "no effect before recovery");

    // The first step after recovery dispatches exactly once.
    let mut activity = RecordingActivity::new(journal.clone());
    assert_eq!(activity.effect_present(&op), EffectProbe::Absent);
    activity.execute(&op).unwrap();
    assert_eq!(
        journal.effect_count(),
        1,
        "exactly one effect after recovery"
    );

    // Deliver the completion: the intent is consumed; no second effect.
    engine
        .apply(
            &exec,
            &deliver(&op, "h1"),
            &Inputs {
                probe: Probe::Present,
                outcome_ref: Some(ArtifactRef::new("artifact:build-output")),
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert!(engine.pending_dispatch().unwrap().is_empty());
    assert_eq!(journal.effect_count(), 1, "completion repeats no effect");
    assert!(engine.receipt(&op).unwrap().is_some());
}

/// Child role: start, then commit the dispatch transition and abort at the hook.
fn run_crash_child() {
    let dir = std::env::var(DIR_ENV).expect("child run dir");
    let ledger = SqliteLedger::open(Path::new(&dir).join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();

    harness::disarm();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    harness::arm(harness::AFTER_TRANSITION_COMMIT);
    let op = OperationId::derive(&exec, "build");
    // Commits the intent + event, then aborts at the hook before dispatch.
    let _ = engine.apply(
        &exec,
        &deliver(&op, "h1"),
        &Inputs {
            probe: Probe::Absent,
            now_unix_ms: 1,
            ..Inputs::default()
        },
    );
    harness::disarm();
    panic!("crash child should have aborted after the transition commit");
}

// ---------------------------------------------------------------------------
// Case 2 — duplicate delivery after completion (activity path)
// ---------------------------------------------------------------------------

#[test]
fn duplicate_delivery_returns_recorded_receipt() {
    let (dir, ledger) = harness::open_isolated_ledger(CASE_2);
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let journal = EffectJournal::in_run_dir(&dir, CASE_2);
    let def = ActivityDef::new("fake/build", "v1");
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    let request = OperationRequest {
        activity: def,
        step: "build".to_string(),
        phase: ExecutionPhase::Build,
        component_api: "harness".to_string(),
        inputs: json!({ "task": "fix-answer" }),
        now_unix_ms: 1,
    };

    // The activity performs the effect once and the receipt is recorded.
    let first = run_activity(&mut engine, &mut registry, &exec, &request).unwrap();
    let receipt = match first {
        OperationResult::Completed {
            receipt,
            reconciled,
            duplicate,
            ..
        } => {
            assert!(!reconciled && !duplicate);
            receipt
        }
        other => panic!("expected Completed, got {other:?}"),
    };
    assert_eq!(journal.effect_count(), 1);

    // Duplicate delivery returns the recorded receipt and repeats no effect.
    let second = run_activity(&mut engine, &mut registry, &exec, &request).unwrap();
    match second {
        OperationResult::Completed {
            receipt: again,
            duplicate,
            ..
        } => {
            assert!(duplicate, "second delivery is a duplicate");
            assert_eq!(again.result, receipt.result);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(journal.effect_count(), 1, "duplicate repeats no effect");

    // Reusing the id with different inputs is rejected.
    let mut changed = request.clone();
    changed.inputs = json!({ "task": "other" });
    let conflict = run_activity(&mut engine, &mut registry, &exec, &changed).unwrap();
    assert!(matches!(conflict, OperationResult::Conflict(_)));
    assert_eq!(journal.effect_count(), 1);
}

// ---------------------------------------------------------------------------
// Case 3 — restart after decision commit
// ---------------------------------------------------------------------------

fn decision_request(exec: &ExecutionId) -> DecisionRequest {
    DecisionRequest {
        version: DECISION_API_VERSION,
        operation_id: OperationId::derive(exec, "triage"),
        kind: DecisionKind::TriagePickNext,
        context: json!({ "eligible": ["fix-answer"] }),
        artifacts: vec![],
        options: vec![DecisionOption {
            option_id: "fix-answer".to_string(),
            attributes: json!({ "priority": 1, "age_ms": 10 }),
        }],
        deadline_unix_ms: 0,
        pin: DecisionPin {
            policy: "triage.pick-next/v1".to_string(),
            genome: "genome/v0".to_string(),
            model: "static".to_string(),
        },
    }
}

#[test]
fn restart_after_decision_commit_reuses_recorded_outcome() {
    if harness::child_case().is_some() {
        run_decision_child();
        return;
    }

    let dir = harness::recovery_dir(CASE_3);
    let status = spawn_child(
        "restart_after_decision_commit_reuses_recorded_outcome",
        CASE_3,
        &dir,
    );
    harness::assert_sigabrt(&status);

    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let request = decision_request(&exec);

    // Recovery uses a fresh decider: the recorded outcome is reused with zero
    // decider calls and zero inference.
    let (mut decider, decide_calls, inference_calls) =
        CountingDecider::open(&dir.join("ledger.sqlite"));
    let run = run_decision(
        &mut engine,
        &mut decider,
        &exec,
        &request,
        DecisionAttempt::Baseline,
        2,
    )
    .unwrap();
    assert!(run.reused, "recovery reuses the recorded decision");
    assert_eq!(decide_calls.get(), 0, "no decider call on recovery");
    assert_eq!(inference_calls.get(), 0, "no second inference call");
    assert!(engine.receipt(&request.operation_id).unwrap().is_some());

    // Exactly one recorded decision: re-resolving the same request through the
    // decider returns the recorded outcome without a second inference.
    let via_decider = decider.decide(&request, DecisionAttempt::Baseline).unwrap();
    assert_eq!(via_decider, run.outcome);
    assert_eq!(
        inference_calls.get(),
        0,
        "the single recorded decision is reused, never re-inferred"
    );

    // The recorded outcome schedules dependent work: the build step runs.
    let journal = EffectJournal::in_run_dir(&dir, CASE_3);
    let def = ActivityDef::new("fake/build", "v1");
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    let build = OperationRequest {
        activity: def,
        step: "build".to_string(),
        phase: ExecutionPhase::Build,
        component_api: "harness".to_string(),
        inputs: json!({ "task": "fix-answer" }),
        now_unix_ms: 3,
    };
    let result = run_activity(&mut engine, &mut registry, &exec, &build).unwrap();
    assert!(matches!(result, OperationResult::Completed { .. }));
    assert_eq!(journal.effect_count(), 1);
}

/// Child role: start, commit the decision outcome, and abort at the hook.
fn run_decision_child() {
    let dir = std::env::var(DIR_ENV).expect("child run dir");
    let ledger = SqliteLedger::open(Path::new(&dir).join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();

    harness::disarm();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    let request = decision_request(&exec);
    let (mut decider, _, _) = CountingDecider::open(&Path::new(&dir).join("ledger.sqlite"));
    harness::arm(harness::AFTER_DECISION_COMMIT);
    // Resolves and commits the outcome, then aborts before the apply.
    let _ = run_decision(
        &mut engine,
        &mut decider,
        &exec,
        &request,
        DecisionAttempt::Baseline,
        1,
    );
    harness::disarm();
    panic!("crash child should have aborted after the decision commit");
}

// ---------------------------------------------------------------------------
// Case 4 — merge before completion
// ---------------------------------------------------------------------------

#[test]
fn merge_before_completion_reconciles_without_remerge() {
    if harness::child_case().is_some() {
        run_merge_child();
        return;
    }

    let dir = harness::recovery_dir(CASE_4);
    let status = spawn_child(
        "merge_before_completion_reconciles_without_remerge",
        CASE_4,
        &dir,
    );
    harness::assert_sigabrt(&status);

    let journal = EffectJournal::in_run_dir(&dir, CASE_4);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let op = OperationId::derive(&exec, "integrate");
    let def = ActivityDef::new("fake/integrate", "v1");
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();

    // The merge effect survived the crash: exactly one execute entry.
    assert_eq!(journal.effect_count(), 1, "exactly one merge effect");

    let results = recover_pending(&mut engine, &mut registry, &exec, 2).unwrap();
    assert_eq!(results.len(), 1);
    match &results[0] {
        OperationResult::Completed { reconciled, .. } => {
            assert!(reconciled, "present merge is reconciled, not re-run");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(journal.effect_count(), 1, "the merge is not repeated");
    assert!(engine.receipt(&op).unwrap().is_some());
    assert_eq!(
        engine.load(&exec).unwrap().operations[&op].state,
        OperationState::Completed
    );
}

/// Child role: announce + execute the integrate effect, abort at the hook.
fn run_merge_child() {
    let dir = std::env::var(DIR_ENV).expect("child run dir");
    let ledger = SqliteLedger::open(Path::new(&dir).join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let def = ActivityDef::new("fake/integrate", "v1");
    let journal = EffectJournal::in_run_dir(Path::new(&dir), CASE_4);
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();

    harness::disarm();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    harness::arm(harness::AFTER_MERGE_BEFORE_COMPLETION);
    let request = OperationRequest {
        activity: def,
        step: "integrate".to_string(),
        phase: ExecutionPhase::Integrate,
        component_api: "git".to_string(),
        inputs: json!({ "candidate": "candidate/exec:fix-answer:1" }),
        now_unix_ms: 1,
    };
    // Announces, performs the merge, then aborts before the completion record.
    let _ = run_activity(&mut engine, &mut registry, &exec, &request);
    harness::disarm();
    panic!("crash child should have aborted after the merge");
}
