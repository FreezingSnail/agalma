//! B0.2 history + projection tests: replay agreement, projection-mismatch
//! parking, kernel_meta round-trip, unknown-version parking, and the
//! references-not-inlined contract.
//!
//! Isolated state lives under `target/test-runs/recovery-*` via the frozen
//! harness.

mod support;

use agalma_contracts::{
    ArtifactRef, CommitBatch, ExecutionId, ExecutionPhase, ExecutionState, LedgerApi, OperationId,
    TaskId,
};
use agalma_kernel::{
    Command, ExecState, Inputs, KernelEngine, KernelState, OperationState, Probe, EXEC_DEF_VERSION,
    KERNEL_VERSION,
};

use support::harness;

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

fn announced_inputs() -> Inputs {
    Inputs {
        probe: Probe::Absent,
        now_unix_ms: 1,
        ..Inputs::default()
    }
}

#[test]
fn replay_equals_projection() {
    let (_dir, ledger) = harness::open_isolated_ledger("replay-projection");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let op = OperationId::derive(&exec, "build");

    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();
    engine
        .apply(&exec, &deliver(&op, "h1"), &announced_inputs())
        .unwrap();
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

    // Reconstruct from history alone and compare against the projection.
    let events = engine.ledger().events(&exec).unwrap();
    let replayed = agalma_kernel::replay(&exec, &events).unwrap();
    assert_eq!(replayed.state, ExecState::Running);
    assert_eq!(replayed.operations[&op].state, OperationState::Completed);

    // `load` performs the reducer-vs-projection check internally.
    let loaded = engine.load(&exec).unwrap();
    assert_eq!(loaded, replayed);
    assert_eq!(loaded.revision, events.last().unwrap().sequence);

    let projection = engine.ledger().execution(&exec).unwrap().unwrap();
    assert!(replayed.matches_projection(&projection));
    // One event per committed transition means sequence == revision.
    assert_eq!(projection.revision, replayed.revision);
}

#[test]
fn projection_mismatch_parks() {
    let (_dir, ledger) = harness::open_isolated_ledger("projection-mismatch");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let op = OperationId::derive(&exec, "build");

    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();
    engine
        .apply(&exec, &deliver(&op, "h1"), &announced_inputs())
        .unwrap();

    {
        // Directly tamper the projection so it disagrees with history.
        let ledger = engine.ledger_mut();
        let mut record = ledger.execution(&exec).unwrap().unwrap();
        record.phase = ExecutionPhase::Done;
        record.state = ExecutionState::Completed;
        ledger
            .commit(CommitBatch {
                expected_revision: Some(record.revision),
                state: Some(record),
                events: vec![],
                receipts: vec![],
                intents: vec![],
            })
            .unwrap();
    }

    let state = engine.load(&exec).unwrap();
    assert_eq!(state.state, ExecState::Parked);
    assert!(state
        .parked_reason
        .as_deref()
        .unwrap_or("")
        .contains("projection mismatch"));
}

#[test]
fn kernel_meta_round_trips_through_open() {
    let (_dir, ledger) = harness::open_isolated_ledger("kernel-meta");
    let engine = KernelEngine::open(ledger).unwrap();

    assert_eq!(
        engine
            .ledger()
            .kernel_meta_get("exec_def_version")
            .unwrap()
            .as_deref(),
        Some(EXEC_DEF_VERSION)
    );
    assert_eq!(
        engine
            .ledger()
            .kernel_meta_get("kernel_version")
            .unwrap()
            .as_deref(),
        Some(KERNEL_VERSION)
    );
    assert_eq!(engine.exec_def_version(), Some(EXEC_DEF_VERSION));
}

#[test]
fn unknown_exec_def_version_parks() {
    let (_dir, mut ledger) = harness::open_isolated_ledger("unknown-version");
    ledger
        .kernel_meta_set("exec_def_version", "exec_def/v99")
        .unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    assert_eq!(engine.exec_def_version(), None);

    let exec = exec();
    let loaded = engine.load(&exec).unwrap();
    assert_eq!(loaded.state, ExecState::Parked);

    let outcome = engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();
    assert_eq!(outcome.state.state, ExecState::Parked);
    assert!(outcome
        .state
        .parked_reason
        .as_deref()
        .unwrap_or("")
        .contains("exec_def/v99"));
    // Nothing was written: an unknown version never guesses.
    assert!(engine.ledger().execution(&exec).unwrap().is_none());
}

#[test]
fn large_artifacts_are_referenced_not_inlined() {
    let (dir, ledger) = harness::open_isolated_ledger("large-artifacts");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let op = OperationId::derive(&exec, "build");

    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();
    engine
        .apply(&exec, &deliver(&op, "h1"), &announced_inputs())
        .unwrap();

    // The large artifact body lives outside history; only its reference is recorded.
    let body = "A".repeat(64 * 1024);
    std::fs::write(dir.join("artifact.txt"), &body).unwrap();
    let outcome_ref = ArtifactRef::new("artifact:build-output");
    engine
        .apply(
            &exec,
            &deliver(&op, "h1"),
            &Inputs {
                probe: Probe::Present,
                outcome_ref: Some(outcome_ref.clone()),
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .unwrap();

    let receipt = engine.receipt(&op).unwrap().unwrap();
    assert_eq!(
        receipt.result,
        serde_json::json!({ "artifact": outcome_ref.as_str() })
    );

    let payload = serde_json::to_string(&engine.ledger().events(&exec).unwrap()).unwrap();
    assert!(payload.contains(outcome_ref.as_str()));
    assert!(
        !payload.contains(&body),
        "history must never inline artifacts"
    );

    // Ambiguity without evidence parks rather than risking a duplicate effect.
    let other = OperationId::derive(&exec, "verify");
    let ambiguous = engine
        .apply(
            &exec,
            &deliver(&other, "h2"),
            &Inputs {
                probe: Probe::Unknown,
                now_unix_ms: 3,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert_eq!(ambiguous.state.state, ExecState::Parked);
}

#[test]
fn fresh_engine_loads_unstarted_state() {
    let (_dir, ledger) = harness::open_isolated_ledger("unstarted");
    let engine = KernelEngine::open(ledger).unwrap();
    let state: KernelState = engine.load(&exec()).unwrap();
    assert_eq!(state.state, ExecState::Pending);
    assert!(state.task_id.as_str().is_empty());
}
