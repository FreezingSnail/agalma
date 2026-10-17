//! Complete-state/v2 canonical contract tests (`agalma-jmr.5`).
//!
//! These exercise `crates/agalma-contracts/src/snapshot.rs` directly: the
//! `agalma-state/v2` format/schema gate, section ordering, duplicate identity
//! and execution foreign-key validation, unknown-section refusal and canonical
//! JSON round trips. They are independent of the ledger implementation.

use std::collections::BTreeMap;

use agalma_contracts::digest::failure_code;
use agalma_contracts::{
    CompleteStateV2, ContractError, ExecutionEvent, ExecutionId, ExecutionPhase, ExecutionRecord,
    ExecutionState, FailureCode, TaskId, COMPLETE_STATE_FORMAT, COMPLETE_STATE_SCHEMA_VERSION,
};

fn empty() -> CompleteStateV2 {
    CompleteStateV2 {
        format: COMPLETE_STATE_FORMAT.to_string(),
        schema_version: COMPLETE_STATE_SCHEMA_VERSION,
        exec_def_version: String::new(),
        kernel_version: String::new(),
        kill_latch: false,
        kernel_meta: BTreeMap::new(),
        lease_generations: BTreeMap::new(),
        executions: Vec::new(),
        events: Vec::new(),
        receipts: Vec::new(),
        intents: Vec::new(),
        timers: Vec::new(),
        directives: Vec::new(),
        bindings: Vec::new(),
        decisions: Vec::new(),
        decision_outcomes: Vec::new(),
        digests: Vec::new(),
        control_receipts: Vec::new(),
        component_states: Vec::new(),
        component_events: Vec::new(),
        component_operations: Vec::new(),
        component_receipts: Vec::new(),
        component_commits: Vec::new(),
    }
}

fn execution(id: &str) -> ExecutionRecord {
    ExecutionRecord {
        execution_id: ExecutionId::new(id),
        task_id: TaskId::new("fix-answer"),
        generation: 1,
        phase: ExecutionPhase::Done,
        state: ExecutionState::Completed,
        attempt: 1,
        revision: 1,
    }
}

#[test]
fn complete_v2_round_trips_canonically() {
    let mut state = empty();
    state
        .kernel_meta
        .insert("exec_def_version".to_string(), "exec_def/v1".to_string());
    state.executions.push(execution("exec:a:1"));
    state.events.push(ExecutionEvent {
        execution_id: ExecutionId::new("exec:a:1"),
        sequence: 1,
        kind: "phase_advanced".to_string(),
        payload: serde_json::json!({ "to": "done" }),
        recorded_at_unix_ms: 7,
    });
    state.validate_complete().unwrap();

    let json = state.to_canonical_json().unwrap();
    let parsed = CompleteStateV2::from_json(&json).unwrap();
    assert_eq!(parsed, state);
    assert_eq!(parsed.to_canonical_json().unwrap(), json, "stable canonical JSON");
}

#[test]
fn version_gate_and_schema_mismatch_refuse() {
    let mut legacy = empty();
    legacy.format = "agalma-state/v1".to_string();
    assert_eq!(
        failure_code(&legacy.version_gate().unwrap_err()),
        Some(FailureCode::UnsupportedVersion),
        "legacy v1 is not a complete-state format"
    );

    let mut future = empty();
    future.format = "agalma-state/v9".to_string();
    assert_eq!(
        failure_code(&future.version_gate().unwrap_err()),
        Some(FailureCode::UnsupportedVersion)
    );

    let mut foreign = empty();
    foreign.format = "OtherFormat/v2".to_string();
    assert_eq!(
        failure_code(&foreign.version_gate().unwrap_err()),
        Some(FailureCode::UnsupportedSchema)
    );

    let mut wrong_schema = empty();
    wrong_schema.schema_version = 4;
    assert_eq!(
        failure_code(&wrong_schema.validate_complete().unwrap_err()),
        Some(FailureCode::UnsupportedSchema)
    );
}

#[test]
fn ordering_duplicates_and_foreign_keys_are_validated() {
    // Unsorted executions refuse.
    let mut unsorted = empty();
    unsorted.executions.push(execution("exec:b:1"));
    unsorted.executions.push(execution("exec:a:1"));
    assert_eq!(
        failure_code(&unsorted.validate_complete().unwrap_err()),
        Some(FailureCode::InvalidRecord)
    );

    // Duplicate execution identity refuses.
    let mut duplicates = empty();
    duplicates.executions.push(execution("exec:a:1"));
    duplicates.executions.push(execution("exec:a:1"));
    assert_eq!(
        failure_code(&duplicates.validate_complete().unwrap_err()),
        Some(FailureCode::IdentityConflict)
    );

    // An event referencing an absent execution refuses.
    let mut orphan = empty();
    orphan.events.push(ExecutionEvent {
        execution_id: ExecutionId::new("exec:missing:1"),
        sequence: 1,
        kind: "x".to_string(),
        payload: serde_json::json!({}),
        recorded_at_unix_ms: 1,
    });
    assert_eq!(
        failure_code(&orphan.validate_complete().unwrap_err()),
        Some(FailureCode::InvalidRecord)
    );
}

#[test]
fn unknown_sections_are_refused_at_parse() {
    let json = empty().to_canonical_json().unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("future_section".to_string(), serde_json::json!([]));
    let err = CompleteStateV2::from_json(&value.to_string()).unwrap_err();
    assert!(
        matches!(err, ContractError::KnownFailure(_)),
        "unknown section refuses parse: {err:?}"
    );

    // A missing required section also refuses parse.
    value.as_object_mut().unwrap().remove("future_section");
    value.as_object_mut().unwrap().remove("component_commits");
    assert!(CompleteStateV2::from_json(&value.to_string()).is_err());
}
