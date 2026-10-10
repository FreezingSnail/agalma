//! B0.2 pure-transition tests: determinism, conflict rejection, and the
//! fail-closed parks (ambiguous effect). No ledger, no I/O.
//!
//! `transition` is pure, so identical `(state, command, inputs)` must yield
//! byte-identical effects. `inputs` carries the recorded wall clock, so replay
//! is deterministic without reading a real clock.

use agalma_contracts::{ArtifactRef, ExecutionId, OperationId, TaskId};
use agalma_kernel::{
    detect_conflict, step, transition, Command, Effect, ExecState, Inputs, KernelState,
    OperationState, Probe,
};

fn exec() -> ExecutionId {
    ExecutionId::derive(&TaskId::new("fix-answer"), 1)
}

fn started() -> KernelState {
    let state = KernelState::unstarted(&exec());
    step(
        &state,
        &Command::Start {
            task_id: TaskId::new("fix-answer"),
            generation: 1,
        },
        &Inputs::default(),
    )
    .state
}

fn build_op(state: &KernelState) -> OperationId {
    OperationId::derive(&state.execution_id, "build")
}

fn deliver(op: &OperationId, hash: &str) -> Command {
    Command::Deliver {
        operation_id: op.clone(),
        inputs_hash: hash.to_string(),
    }
}

#[test]
fn transition_is_deterministic() {
    let state = started();
    let op = build_op(&state);
    let command = deliver(&op, "h1");
    let inputs = Inputs {
        probe: Probe::Absent,
        now_unix_ms: 1_700_000_000_000,
        ..Inputs::default()
    };

    // Identical inputs ⇒ identical effects, across repeated calls.
    assert_eq!(
        transition(&state, &command, &inputs),
        transition(&state, &command, &inputs)
    );
    assert_eq!(
        step(&state, &command, &inputs),
        step(&state, &command, &inputs)
    );

    // A different clock changes nothing structural (it is a recorded input).
    let later = Inputs {
        now_unix_ms: inputs.now_unix_ms + 1,
        ..inputs.clone()
    };
    let a = step(&state, &command, &inputs);
    let b = step(&state, &command, &later);
    assert_eq!(a.state, b.state);
    assert_eq!(a.effects.len(), b.effects.len());
}

#[test]
fn deliver_absent_announces_then_dispatches() {
    let state = started();
    let op = build_op(&state);
    let inputs = Inputs {
        probe: Probe::Absent,
        now_unix_ms: 10,
        ..Inputs::default()
    };
    let result = step(&state, &deliver(&op, "h1"), &inputs);

    assert!(result.conflict.is_none());
    assert_eq!(result.state.state, ExecState::Running);
    assert_eq!(
        result.state.operations[&op].state,
        OperationState::Dispatched
    );
    // Commit(state+event+intent) first, then the post-commit dispatch hand-off.
    assert_eq!(result.effects.len(), 2);
    assert!(matches!(result.effects[0], Effect::Commit(_)));
    assert!(matches!(result.effects[1], Effect::Dispatch(_)));
}

#[test]
fn conflict_on_same_operation_id_with_different_inputs() {
    let state = started();
    let op = build_op(&state);
    let inputs = Inputs {
        probe: Probe::Absent,
        now_unix_ms: 10,
        ..Inputs::default()
    };
    let announced = step(&state, &deliver(&op, "h1"), &inputs);

    // The recorded operation's hash is `h1`; reuse under `h2` is a conflict.
    let conflict = detect_conflict(&announced.state, &deliver(&op, "h2"), &inputs)
        .expect("reused id with different contents must conflict");
    assert_eq!(conflict.id, op.to_string());

    // A rejected command produces no effects and does not mutate state.
    let rejected = step(&announced.state, &deliver(&op, "h2"), &inputs);
    assert!(rejected.conflict.is_some());
    assert!(rejected.effects.is_empty());
    assert_eq!(rejected.state, announced.state);
}

#[test]
fn directive_reused_at_same_version_with_different_contents_conflicts() {
    let state = started();
    let v1 = Command::DirectiveUpdate {
        directive_id: "dir-1".to_string(),
        version: 1,
        content_hash: "c1".to_string(),
    };
    let applied = step(&state, &v1, &Inputs::default());
    assert_eq!(applied.state.directives["dir-1"].version, 1);

    // Newer version replaces without conflict.
    let v2 = Command::DirectiveUpdate {
        directive_id: "dir-1".to_string(),
        version: 2,
        content_hash: "c2".to_string(),
    };
    assert!(step(&applied.state, &v2, &Inputs::default())
        .conflict
        .is_none());

    // Same id + same version + different contents is a conflict.
    let clashing = Command::DirectiveUpdate {
        directive_id: "dir-1".to_string(),
        version: 1,
        content_hash: "other".to_string(),
    };
    let rejected = step(&applied.state, &clashing, &Inputs::default());
    assert!(rejected.conflict.is_some());
    assert!(rejected.effects.is_empty());
}

#[test]
fn duplicate_delivery_with_recorded_receipt_is_a_no_op() {
    let state = started();
    let op = build_op(&state);
    let recorded = agalma_contracts::OperationReceipt {
        operation_id: op.clone(),
        execution_id: state.execution_id.clone(),
        kind: "operation".to_string(),
        inputs_hash: "h1".to_string(),
        result: serde_json::json!({ "artifact": "artifact:build-output" }),
        completed_at_unix_ms: 5,
    };
    let inputs = Inputs {
        recorded_receipt: Some(recorded),
        now_unix_ms: 20,
        ..Inputs::default()
    };
    let result = step(&state, &deliver(&op, "h1"), &inputs);
    assert!(result.conflict.is_none());
    assert!(
        result.effects.is_empty(),
        "duplicate delivery repeats no effect"
    );
    assert_eq!(result.state, state);
}

#[test]
fn ambiguous_effect_parks() {
    let state = started();
    let op = build_op(&state);
    // Unknown probe: no evidence whether the effect happened.
    let inputs = Inputs {
        probe: Probe::Unknown,
        ..Inputs::default()
    };
    let result = step(&state, &deliver(&op, "h1"), &inputs);
    assert_eq!(result.state.state, ExecState::Parked);
    assert!(result
        .state
        .parked_reason
        .as_deref()
        .unwrap_or("")
        .contains("ambiguous"));
    // Parking is durable: a commit records the park, but nothing dispatches.
    assert_eq!(result.effects.len(), 1);
    assert!(matches!(result.effects[0], Effect::Commit(_)));
}

#[test]
fn kill_latch_blocks_dispatch() {
    let state = started();
    let latched = step(
        &state,
        &Command::KillLatch { latched: true },
        &Inputs::default(),
    )
    .state;
    assert!(latched.kill_latched);

    let op = build_op(&latched);
    let inputs = Inputs {
        probe: Probe::Absent,
        ..Inputs::default()
    };
    let blocked = step(&latched, &deliver(&op, "h1"), &inputs);
    assert!(
        blocked.effects.is_empty(),
        "latched kernel must not dispatch"
    );
    assert_eq!(blocked.state, latched);
}

#[test]
fn complete_present_effect_records_a_reference_only() {
    let state = started();
    let op = build_op(&state);
    let outcome_ref = ArtifactRef::new("artifact:build-output");
    let inputs = Inputs {
        probe: Probe::Present,
        outcome_ref: Some(outcome_ref),
        now_unix_ms: 30,
        ..Inputs::default()
    };
    let result = step(&state, &deliver(&op, "h1"), &inputs);
    assert_eq!(result.state.state, ExecState::Running);
    assert_eq!(
        result.state.operations[&op].state,
        OperationState::Completed
    );
    // The recorded receipt holds the immutable reference, never the artifact.
    let Effect::Commit(batch) = &result.effects[0] else {
        panic!("expected a commit");
    };
    assert_eq!(batch.receipts.len(), 1);
    assert_eq!(
        batch.receipts[0].result,
        serde_json::json!({ "artifact": "artifact:build-output" })
    );
}
