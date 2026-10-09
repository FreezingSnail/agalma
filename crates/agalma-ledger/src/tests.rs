//! Co-located ledger tests (native Rust, `cargo test -p agalma-ledger`).
//!
//! Run directories live under `target/test-runs/ledger-*` (never a system temp
//! directory). The dispatch-intent recovery test reuses the S0c crash pattern:
//! it re-execs the current test binary as a child with an injection env var and
//! aborts the child (`SIGABRT`) between the atomic commit and dispatch.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use agalma_contracts::{
    ArtifactRef, BindingId, BindingRecord, BindingState, CommitBatch, ContractError, DecisionKind,
    DecisionOutcome, DecisionPin, DecisionRequest, DigestRecord, DispatchIntent, ExecutionEvent,
    ExecutionId, ExecutionPhase, ExecutionRecord, ExecutionState, LedgerApi, OperationId,
    OperationReceipt, TaskId,
};

use crate::{SqliteLedger, SCHEMA_VERSION};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn run_dir(name: &str) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target.join("test-runs").join(format!("ledger-{name}"))
}

/// Delete and recreate a test run directory.
///
/// WARNING: This deletes files. The target is confined to `target/test-runs/`,
/// which holds only disposable test output; it never escapes the workspace
/// target directory and never touches a system temp directory.
fn reset_dir(dir: &Path) {
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("reset run dir");
    }
    std::fs::create_dir_all(dir).expect("create run dir");
}

fn fixture_exec() -> ExecutionId {
    ExecutionId::derive(&TaskId::new("fix-answer"), 1)
}

fn record(exec: &ExecutionId, phase: ExecutionPhase, state: ExecutionState) -> ExecutionRecord {
    ExecutionRecord {
        execution_id: exec.clone(),
        task_id: TaskId::new("fix-answer"),
        generation: 1,
        phase,
        state,
        attempt: 1,
        revision: 0,
    }
}

fn event(exec: &ExecutionId, kind: &str, payload: Value) -> ExecutionEvent {
    ExecutionEvent {
        execution_id: exec.clone(),
        sequence: 0, // ledger-owned; ignored and reassigned
        kind: kind.to_string(),
        payload,
        recorded_at_unix_ms: now_ms(),
    }
}

fn intent(exec: &ExecutionId, op: &OperationId) -> DispatchIntent {
    DispatchIntent {
        operation_id: op.clone(),
        execution_id: exec.clone(),
        payload: json!({ "kind": "build" }),
        enqueued_at_unix_ms: now_ms(),
        consumed: false,
    }
}

#[test]
fn duplicate_delivery_returns_recorded_receipt() {
    let dir = run_dir("duplicate");
    reset_dir(&dir);
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();

    let exec = fixture_exec();
    let op = OperationId::derive(&exec, "build");

    // Transition: state + event + dispatch intent, atomically.
    ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: Some(record(
                &exec,
                ExecutionPhase::Build,
                ExecutionState::Running,
            )),
            events: vec![event(&exec, "phase_advanced", json!({ "to": "build" }))],
            receipts: vec![],
            intents: vec![intent(&exec, &op)],
        })
        .unwrap();
    assert_eq!(ledger.pending_intents().unwrap().len(), 1);

    // Completion: receipt + event, consuming the intent.
    let receipt = OperationReceipt {
        operation_id: op.clone(),
        execution_id: exec.clone(),
        kind: "build".to_string(),
        inputs_hash: "hash-a".to_string(),
        result: json!({ "status": "executed" }),
        completed_at_unix_ms: 100,
    };
    ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![event(
                &exec,
                "operation_completed",
                json!({ "operation_id": op.as_str() }),
            )],
            receipts: vec![receipt.clone()],
            intents: vec![],
        })
        .unwrap();

    let recorded = ledger.operation_receipt(&op).unwrap().unwrap();
    assert_eq!(recorded, receipt);
    assert!(ledger.pending_intents().unwrap().is_empty());
    let events_after_first = ledger.events(&exec).unwrap().len();

    // Duplicate delivery: recorded receipt returned, no repeat.
    ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![event(
                &exec,
                "operation_completed",
                json!({ "operation_id": op.as_str() }),
            )],
            receipts: vec![receipt.clone()],
            intents: vec![],
        })
        .unwrap();
    assert_eq!(ledger.operation_receipt(&op).unwrap().unwrap(), recorded);
    assert_eq!(
        ledger.events(&exec).unwrap().len(),
        events_after_first,
        "duplicate delivery must not append events"
    );

    // Same operation id, different input hash: rejected.
    let conflicting = OperationReceipt {
        inputs_hash: "hash-b".to_string(),
        ..receipt.clone()
    };
    let err = ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![],
            receipts: vec![conflicting],
            intents: vec![],
        })
        .unwrap_err();
    assert!(matches!(err, ContractError::Conflict(_)), "err={err:?}");
    assert_eq!(ledger.operation_receipt(&op).unwrap().unwrap(), recorded);
}

#[test]
fn dispatch_intent_recovered_after_crash() {
    if std::env::var("LEDGER_CRASH_CHILD").is_ok() {
        crash_child_after_transition();
    }

    let dir = run_dir("crash");
    reset_dir(&dir);
    let exe = std::env::current_exe().expect("current_exe");
    let status = Command::new(exe)
        .args(["dispatch_intent_recovered_after_crash", "--test-threads=1"])
        .env("LEDGER_CRASH_CHILD", "1")
        .status()
        .expect("spawn crash child");
    assert!(!status.success(), "child should abort, got {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "expected SIGABRT, got {status:?}");
    }

    // Reopen the same database: the committed intent is still pending.
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();
    let op = OperationId::derive(&exec, "build");
    let pending = ledger.pending_intents().unwrap();
    assert_eq!(pending.len(), 1, "pending={pending:?}");
    assert_eq!(pending[0].operation_id, op);
    assert_eq!(pending[0].execution_id, exec);
    let recovered = ledger.execution(&exec).unwrap().unwrap();
    assert_eq!(recovered.phase, ExecutionPhase::Build);
    assert_eq!(recovered.state, ExecutionState::Running);
}

/// Child role: commit the transition, then abort before dispatch.
fn crash_child_after_transition() -> ! {
    let dir = run_dir("crash");
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();
    let op = OperationId::derive(&exec, "build");
    ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: Some(record(
                &exec,
                ExecutionPhase::Build,
                ExecutionState::Running,
            )),
            events: vec![event(&exec, "phase_advanced", json!({ "to": "build" }))],
            receipts: vec![],
            intents: vec![intent(&exec, &op)],
        })
        .unwrap();
    // Injection point: atomic commit returned, dispatch has not run.
    std::process::abort();
}

#[test]
fn event_reconstruction_matches_projection() {
    let dir = run_dir("reconstruct");
    reset_dir(&dir);
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();

    ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: Some(record(
                &exec,
                ExecutionPhase::Verify,
                ExecutionState::Running,
            )),
            events: vec![
                event(&exec, "execution_created", json!({ "to": "intake" })),
                event(&exec, "phase_advanced", json!({ "to": "build" })),
                event(&exec, "phase_advanced", json!({ "to": "verify" })),
            ],
            receipts: vec![],
            intents: vec![],
        })
        .unwrap();

    let events = ledger.events(&exec).unwrap();
    assert_eq!(
        events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "ledger assigns strictly monotonic sequences"
    );

    // Replay: fold events only, with no I/O.
    let mut derived = ExecutionPhase::Intake;
    for e in &events {
        if e.kind == "execution_created" || e.kind == "phase_advanced" {
            let to = e.payload.get("to").and_then(Value::as_str).unwrap();
            derived = crate::phase_from(to).expect("known phase");
        }
    }
    let projected = ledger.execution(&exec).unwrap().unwrap().phase;
    assert_eq!(derived, projected);
    assert_eq!(projected, ExecutionPhase::Verify);
}

#[test]
fn kill_latch_survives_reopen() {
    let dir = run_dir("latch");
    reset_dir(&dir);
    let path = dir.join("ledger.sqlite");

    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        assert!(!ledger.kill_latch().unwrap());
        ledger.set_kill_latch(true).unwrap();
        assert!(ledger.kill_latch().unwrap());
    }
    {
        let ledger = SqliteLedger::open(&path).unwrap();
        assert!(ledger.kill_latch().unwrap(), "latch must survive reopen");
    }
    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        ledger.set_kill_latch(false).unwrap();
    }
    {
        let ledger = SqliteLedger::open(&path).unwrap();
        assert!(!ledger.kill_latch().unwrap());
    }
}

#[test]
fn schema_mismatch_parks() {
    let dir = run_dir("schema");
    reset_dir(&dir);
    let path = dir.join("ledger.sqlite");
    let exec = fixture_exec();
    let transition = || CommitBatch {
        expected_revision: None,
        state: Some(record(
            &exec,
            ExecutionPhase::Build,
            ExecutionState::Running,
        )),
        events: vec![event(&exec, "phase_advanced", json!({ "to": "build" }))],
        receipts: vec![],
        intents: vec![],
    };

    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        assert_eq!(ledger.schema_version().unwrap(), SCHEMA_VERSION);
        ledger.force_schema_version(99).unwrap();
        assert_eq!(ledger.schema_version().unwrap(), 99);
        let err = ledger.commit(transition()).unwrap_err();
        assert!(matches!(err, ContractError::Conflict(_)), "err={err:?}");
        assert!(
            ledger.execution(&exec).unwrap().is_none(),
            "no write may occur under an incompatible schema"
        );
    }

    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        assert_eq!(
            ledger.schema_version().unwrap(),
            99,
            "schema must never auto-migrate"
        );
        assert!(ledger.commit(transition()).is_err());
    }
}

#[test]
fn wal_mode_active_and_sqlite_version_recorded() {
    let dir = run_dir("wal");
    reset_dir(&dir);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();

    assert_eq!(ledger.journal_mode().unwrap().to_lowercase(), "wal");
    assert_eq!(
        ledger.meta("sqlite_version").unwrap().as_deref(),
        Some(rusqlite::version()),
        "bundled SQLite version recorded in meta"
    );
    assert!(ledger.meta("created_at").unwrap().is_some());
    assert_eq!(
        ledger.meta("schema_version").unwrap().as_deref(),
        Some(SCHEMA_VERSION.to_string().as_str())
    );
}

#[test]
fn component_bindings_round_trip() {
    let dir = run_dir("bindings");
    reset_dir(&dir);
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();

    let mut capabilities = BTreeSet::new();
    capabilities.insert("mvp-baseline".to_string());
    let binding = BindingRecord {
        binding_id: BindingId::new("binding:ledger"),
        component_api: "ledger".to_string(),
        api_version: 1,
        implementation: "agalma-ledger".to_string(),
        implementation_version: "0.1.0".to_string(),
        capabilities,
        required_capabilities: BTreeSet::new(),
        optional_capabilities: BTreeSet::new(),
        config_hash: "cfg-hash".to_string(),
        generation: 1,
        state: BindingState::Active,
    };
    ledger.upsert_binding(&binding).unwrap();
    assert_eq!(ledger.bindings().unwrap(), vec![binding.clone()]);

    let updated = BindingRecord {
        state: BindingState::Draining,
        generation: 2,
        ..binding
    };
    ledger.upsert_binding(&updated).unwrap();
    let loaded = ledger.bindings().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].generation, 2);
    assert_eq!(loaded[0].state, BindingState::Draining);
}

fn decision_request(exec: &ExecutionId) -> DecisionRequest {
    DecisionRequest {
        version: 1,
        operation_id: OperationId::derive(exec, "triage"),
        kind: DecisionKind::TriagePickNext,
        context: json!({}),
        artifacts: vec![],
        options: vec![agalma_contracts::DecisionOption {
            option_id: "task-a".to_string(),
            attributes: json!({ "priority": 1, "age_ms": 10 }),
        }],
        deadline_unix_ms: now_ms() + 60_000,
        pin: DecisionPin {
            policy: "triage.pick-next/v1".to_string(),
            genome: "genome/v0".to_string(),
            model: "static".to_string(),
        },
    }
}

#[test]
fn decision_request_recorded_and_pinned_to_inputs() {
    let dir = run_dir("decisions");
    reset_dir(&dir);
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();
    let request = decision_request(&exec);

    // Idempotent for identical contents.
    ledger.record_decision(&request).unwrap();
    ledger.record_decision(&request).unwrap();

    // Same operation id with changed contents is a conflict.
    let changed = DecisionRequest {
        options: vec![agalma_contracts::DecisionOption {
            option_id: "task-b".to_string(),
            attributes: json!({ "priority": 2, "age_ms": 5 }),
        }],
        ..request.clone()
    };
    let err = ledger.record_decision(&changed).unwrap_err();
    assert!(matches!(err, ContractError::Conflict(_)), "err={err:?}");
}

#[test]
fn decision_outcome_is_terminal_and_recovers() {
    let dir = run_dir("decision-outcome");
    reset_dir(&dir);
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();
    let request = decision_request(&exec);
    ledger.record_decision(&request).unwrap();

    let applied = DecisionOutcome::Applied {
        chosen: Some("task-a".to_string()),
        scores: vec![],
    };
    ledger
        .record_decision_outcome(&request.operation_id, &applied)
        .unwrap();
    assert_eq!(
        ledger.decision_outcome(&request.operation_id).unwrap(),
        Some(applied.clone())
    );

    // A late/different result never supersedes the recorded outcome.
    let late = DecisionOutcome::Parked {
        reason: "late".to_string(),
    };
    ledger
        .record_decision_outcome(&request.operation_id, &late)
        .unwrap();
    assert_eq!(
        ledger.decision_outcome(&request.operation_id).unwrap(),
        Some(applied)
    );

    // Reopen: the recorded outcome survives for recovery reuse.
    drop(ledger);
    let reopened = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    assert_eq!(
        reopened.decision_outcome(&request.operation_id).unwrap(),
        Some(DecisionOutcome::Applied {
            chosen: Some("task-a".to_string()),
            scores: vec![],
        })
    );
}

#[test]
fn digest_rows_round_trip_ordered_by_version() {
    let dir = run_dir("digests");
    reset_dir(&dir);
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();

    for version in [2u32, 1u32] {
        ledger
            .record_digest(&DigestRecord {
                execution_id: exec.clone(),
                version,
                summary: json!({ "cost_usd": 0.0, "files": ["a.md"] }),
                artifact_ref: Some(ArtifactRef::derive("digest")),
                recorded_at_unix_ms: now_ms(),
            })
            .unwrap();
    }

    let digests = ledger.digests_for_execution(&exec).unwrap();
    assert_eq!(
        digests.iter().map(|d| d.version).collect::<Vec<_>>(),
        vec![1, 2],
        "digests are ordered by version"
    );
    assert_eq!(digests[0].artifact_ref, Some(ArtifactRef::derive("digest")));

    // Upsert by (execution, version) replaces in place.
    ledger
        .record_digest(&DigestRecord {
            execution_id: exec.clone(),
            version: 1,
            summary: json!({ "cost_usd": 0.0, "files": ["b.md"] }),
            artifact_ref: None,
            recorded_at_unix_ms: now_ms(),
        })
        .unwrap();
    let digests = ledger.digests_for_execution(&exec).unwrap();
    assert_eq!(digests.len(), 2);
    assert_eq!(digests[0].artifact_ref, None);
}

#[test]
fn migration_v1_to_v2_when_all_executions_terminal() {
    let dir = run_dir("migrate-terminal");
    reset_dir(&dir);
    let path = dir.join("ledger.sqlite");
    let exec = fixture_exec();

    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(record(
                    &exec,
                    ExecutionPhase::Done,
                    ExecutionState::Completed,
                )),
                events: vec![],
                receipts: vec![],
                intents: vec![],
            })
            .unwrap();
        ledger.downgrade_to_v1().unwrap();
        assert_eq!(ledger.schema_version().unwrap(), 1);
    }

    let mut ledger = SqliteLedger::open(&path).unwrap();
    assert_eq!(
        ledger.schema_version().unwrap(),
        SCHEMA_VERSION,
        "terminal v1 database migrates to v2"
    );
    // v2 surface is live after migration.
    let request = decision_request(&exec);
    ledger.record_decision(&request).unwrap();
}

#[test]
fn migration_v1_to_v2_refused_with_in_flight_execution() {
    let dir = run_dir("migrate-inflight");
    reset_dir(&dir);
    let path = dir.join("ledger.sqlite");
    let exec = fixture_exec();

    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(record(
                    &exec,
                    ExecutionPhase::Build,
                    ExecutionState::Running,
                )),
                events: vec![],
                receipts: vec![],
                intents: vec![],
            })
            .unwrap();
        ledger.downgrade_to_v1().unwrap();
    }

    let err = SqliteLedger::open(&path).map(|_| ()).unwrap_err();
    assert!(matches!(err, ContractError::KnownFailure(_)), "err={err:?}");
    assert!(
        err.to_string().contains("in-flight"),
        "reason must name in-flight executions: {err}"
    );
}

// ---------------------------------------------------------------------------
// Lease generations (M1.6)
// ---------------------------------------------------------------------------

#[test]
fn lease_generation_starts_at_zero_and_increments_per_claim() {
    let dir = run_dir("lease-counter");
    reset_dir(&dir);
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let task = TaskId::new("fix-answer");

    assert_eq!(ledger.lease_generation(&task).unwrap(), 0, "unclaimed");
    assert_eq!(ledger.begin_lease(&task).unwrap(), 1);
    assert_eq!(ledger.begin_lease(&task).unwrap(), 2);
    assert_eq!(ledger.lease_generation(&task).unwrap(), 2, "latest wins");

    // A different task has an independent counter.
    let other = TaskId::new("other-task");
    assert_eq!(ledger.lease_generation(&other).unwrap(), 0);
    assert_eq!(ledger.begin_lease(&other).unwrap(), 1);
    assert_eq!(ledger.lease_generation(&task).unwrap(), 2, "isolated");
}

#[test]
fn lease_generation_persists_across_reopen() {
    let dir = run_dir("lease-persist");
    reset_dir(&dir);
    let path = dir.join("ledger.sqlite");
    let task = TaskId::new("fix-answer");
    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        assert_eq!(ledger.begin_lease(&task).unwrap(), 1);
    }
    let mut ledger = SqliteLedger::open(&path).unwrap();
    assert_eq!(ledger.lease_generation(&task).unwrap(), 1);
    assert_eq!(ledger.begin_lease(&task).unwrap(), 2);
}
