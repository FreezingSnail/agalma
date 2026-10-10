//! B0.2 recovery tests against the protected fixture pack.
//!
//! - Case 1 `transition-commit-before-dispatch`: a child aborts after the
//!   transition commit (`after_transition_commit`) but before dispatch; the
//!   parent reopens the same isolated ledger and proves the durable intent is
//!   recovered and dispatched exactly once.
//! - Case 2 `duplicate-delivery-after-completion`: a completed operation
//!   delivered again returns the recorded receipt and repeats no effect.

mod support;

use std::path::Path;
use std::process::Command as StdCommand;

use agalma_contracts::{ArtifactRef, ExecutionId, OperationId, TaskId};
use agalma_execution::EffectProbe;
use agalma_kernel::{Command, ExecState, Inputs, KernelEngine, OperationState, Probe};
use agalma_ledger::SqliteLedger;

use support::harness::{self, EffectJournal, FakeActivity, RecordingActivity};

/// Environment variable passing the parent's run directory to the crash child.
const DIR_ENV: &str = "KERNEL_RECOVERY_DIR";

const CASE_1: &str = "transition-commit-before-dispatch";
const CASE_2: &str = "duplicate-delivery-after-completion";

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
    let exe = std::env::current_exe().expect("current_exe");
    let status = StdCommand::new(exe)
        .args([
            "crash_after_transition_commit_before_dispatch",
            "--exact",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(harness::CHILD_ENV, CASE_1)
        .env(DIR_ENV, &dir)
        .status()
        .expect("spawn crash child");
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
// Case 2 — duplicate delivery after completion
// ---------------------------------------------------------------------------

#[test]
fn duplicate_delivery_returns_recorded_receipt() {
    let (dir, ledger) = harness::open_isolated_ledger(CASE_2);
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let op = OperationId::derive(&exec, "build");
    let journal = EffectJournal::in_run_dir(&dir, CASE_2);

    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();
    engine
        .apply(
            &exec,
            &deliver(&op, "h1"),
            &Inputs {
                probe: Probe::Absent,
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .unwrap();

    // The activity performs the effect once.
    let mut activity = RecordingActivity::new(journal.clone());
    activity.execute(&op).unwrap();
    assert_eq!(journal.effect_count(), 1);

    // Completion records the receipt (reference only).
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
    let recorded = engine.receipt(&op).unwrap().expect("recorded receipt");

    // Duplicate delivery returns the recorded receipt and appends no effect.
    let duplicate = engine
        .apply(
            &exec,
            &deliver(&op, "h1"),
            &Inputs {
                recorded_receipt: Some(recorded.clone()),
                now_unix_ms: 3,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert!(duplicate.conflict.is_none());
    assert!(duplicate.dispatches.is_empty());
    assert_eq!(journal.effect_count(), 1, "duplicate repeats no effect");
    assert_eq!(
        engine.receipt(&op).unwrap().unwrap().result,
        recorded.result
    );

    // Reusing the id with different inputs is rejected.
    let conflict = engine
        .apply(
            &exec,
            &deliver(&op, "h2"),
            &Inputs {
                now_unix_ms: 4,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert!(conflict.conflict.is_some());
    assert_eq!(journal.effect_count(), 1);
}
