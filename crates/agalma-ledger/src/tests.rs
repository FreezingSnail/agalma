//! Co-located ledger tests (native Rust, `cargo test -p agalma-ledger`).
//!
//! Run directories live under `target/test-runs/ledger-*` (never a system temp
//! directory). The dispatch-intent recovery test reuses the S0c crash pattern:
//! it re-execs the current test binary as a child with an injection env var and
//! aborts the child (`SIGABRT`) between the atomic commit and dispatch.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use agalma_testkit::ScratchDir;

use agalma_contracts::{
    ArtifactRef, BindingId, BindingRecord, BindingState, CommitBatch, ComponentCommit,
    ComponentEvent, ComponentOperation, ComponentReceipt, ContractError,
    ControlReceipt, DecisionKind, DecisionOutcome, DecisionPin, DecisionRequest, DigestRecord,
    DirectiveRow, DirectiveState, DispatchIntent, ExecutionEvent, ExecutionId, ExecutionPhase,
    ExecutionRecord, ExecutionState, LedgerApi, OperationId, OperationReceipt, TaskId, TimerRecord,
    TimerState, COMPONENT_COMMIT_FORMAT,
};

use crate::{SqliteLedger, SCHEMA_VERSION};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A fresh, self-cleaning run directory under `target/test-runs/ledger-*`.
fn run_dir(name: &str) -> ScratchDir {
    ScratchDir::new(&format!("ledger-{name}"))
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
    let exe = std::env::current_exe().expect("current_exe");
    let status = Command::new(exe)
        .args(["dispatch_intent_recovered_after_crash", "--test-threads=1"])
        .env("LEDGER_CRASH_CHILD", "1")
        .env("LEDGER_CRASH_DIR", dir.path())
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
    let dir = std::env::var("LEDGER_CRASH_DIR").expect("child run dir");
    let mut ledger = SqliteLedger::open(Path::new(&dir).join("ledger.sqlite")).unwrap();
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

#[test]
fn migration_v2_to_v3_when_all_executions_terminal() {
    let dir = run_dir("migrate-v2-terminal");
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
        ledger.downgrade_to_v2().unwrap();
        assert_eq!(ledger.schema_version().unwrap(), 2);
    }

    let mut ledger = SqliteLedger::open(&path).unwrap();
    assert_eq!(
        ledger.schema_version().unwrap(),
        SCHEMA_VERSION,
        "terminal v2 database migrates to v3"
    );
    // v3 surface is live after migration.
    assert_eq!(ledger.kernel_meta_get("exec_def_version").unwrap(), None);
    ledger
        .kernel_meta_set("exec_def_version", "exec_def/v1")
        .unwrap();
    assert_eq!(
        ledger
            .kernel_meta_get("exec_def_version")
            .unwrap()
            .as_deref(),
        Some("exec_def/v1")
    );
}

#[test]
fn migration_v2_to_v3_refused_with_in_flight_execution() {
    let dir = run_dir("migrate-v2-inflight");
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
        ledger.downgrade_to_v2().unwrap();
    }

    let err = SqliteLedger::open(&path).map(|_| ()).unwrap_err();
    assert!(matches!(err, ContractError::KnownFailure(_)), "err={err:?}");
    assert!(
        err.to_string().contains("in-flight"),
        "reason must name in-flight executions: {err}"
    );
}

#[test]
fn kernel_meta_round_trips_and_overwrites() {
    let dir = run_dir("kernel-meta");
    let path = dir.join("ledger.sqlite");
    let exec = fixture_exec();
    let transition = || CommitBatch {
        expected_revision: None,
        state: Some(record(
            &exec,
            ExecutionPhase::Done,
            ExecutionState::Completed,
        )),
        events: vec![],
        receipts: vec![],
        intents: vec![],
    };

    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        assert_eq!(ledger.kernel_meta_get("kernel_version").unwrap(), None);
        ledger.kernel_meta_set("kernel_version", "0.1.0").unwrap();
        ledger
            .kernel_meta_set("exec_def_version", "exec_def/v1")
            .unwrap();
        // Overwrite is an upsert, not an error.
        ledger.kernel_meta_set("kernel_version", "0.1.1").unwrap();
        assert_eq!(
            ledger.kernel_meta_get("kernel_version").unwrap().as_deref(),
            Some("0.1.1")
        );
        // kernel_meta writes are gated by schema compatibility, unlike reads.
        ledger.force_schema_version(99).unwrap();
        assert!(ledger
            .kernel_meta_set("exec_def_version", "exec_def/v2")
            .is_err());
        ledger.force_schema_version(SCHEMA_VERSION).unwrap();
        ledger.commit(transition()).unwrap();
    }

    let ledger = SqliteLedger::open(&path).unwrap();
    assert_eq!(
        ledger.kernel_meta_get("kernel_version").unwrap().as_deref(),
        Some("0.1.1"),
        "kernel_meta must survive reopen"
    );
}

// ---------------------------------------------------------------------------
// Lease generations (M1.6)
// ---------------------------------------------------------------------------

#[test]
fn lease_generation_starts_at_zero_and_increments_per_claim() {
    let dir = run_dir("lease-counter");
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

// ---------------------------------------------------------------------------
// Durable timers (schema v3, B0.4)
// ---------------------------------------------------------------------------

fn timer(id: &str, exec: &ExecutionId, fire_at: u64) -> TimerRecord {
    TimerRecord {
        timer_id: id.to_string(),
        execution_id: exec.clone(),
        kind: "retry".to_string(),
        fire_at_unix_ms: fire_at,
        state: TimerState::Pending,
        payload: json!({ "operation": "build" }),
    }
}

#[test]
fn timers_due_ordering_and_idempotent_insert() {
    let dir = run_dir("timers");
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();

    let later = timer("t-later", &exec, 2_000);
    let sooner = timer("t-sooner", &exec, 1_000);
    assert!(ledger.insert_timer(&later).unwrap(), "new row");
    assert!(ledger.insert_timer(&sooner).unwrap(), "new row");
    // Identical duplicate is a harmless no-op.
    assert!(!ledger.insert_timer(&sooner).unwrap(), "duplicate no-op");
    // Reusing the id with different contents conflicts.
    let mut changed = sooner.clone();
    changed.fire_at_unix_ms = 3_000;
    assert!(matches!(
        ledger.insert_timer(&changed),
        Err(ContractError::Conflict(_))
    ));

    assert_eq!(ledger.next_deadline().unwrap(), Some(1_000));
    let due = ledger.due_timers(1_500).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].timer_id, "t-sooner");

    // Firing marks the row fired exactly once and it stops being due.
    ledger.mark_timer_fired("t-sooner").unwrap();
    assert_eq!(
        ledger.timer("t-sooner").unwrap().unwrap().state,
        TimerState::Fired
    );
    assert!(ledger
        .due_timers(5_000)
        .unwrap()
        .iter()
        .all(|t| t.timer_id != "t-sooner"));
    assert_eq!(ledger.next_deadline().unwrap(), Some(2_000));
}

#[test]
fn cancelling_timers_for_execution_releases_pending() {
    let dir = run_dir("timer-cancel");
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();

    ledger.insert_timer(&timer("t1", &exec, 1_000)).unwrap();
    ledger.insert_timer(&timer("t2", &exec, 2_000)).unwrap();
    let other = ExecutionId::derive(&TaskId::new("other"), 1);
    ledger.insert_timer(&timer("t3", &other, 1_500)).unwrap();

    assert_eq!(ledger.cancel_timers_for_execution(&exec).unwrap(), 2);
    assert_eq!(
        ledger.timer("t1").unwrap().unwrap().state,
        TimerState::Cancelled
    );
    assert_eq!(ledger.next_deadline().unwrap(), Some(1_500), "other intact");
    // Cancelled timers never fire.
    assert_eq!(ledger.due_timers(10_000).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Directives (schema v3, B0.4)
// ---------------------------------------------------------------------------

fn directive(id: &str, version: u64, source: &str, marker: &str) -> DirectiveRow {
    DirectiveRow {
        directive_id: id.to_string(),
        version,
        source: source.to_string(),
        state: DirectiveState::Active,
        payload: json!({ "marker": marker }),
        received_at_unix_ms: version,
    }
}

#[test]
fn directive_upsert_is_idempotent_and_newer_replaces() {
    let dir = run_dir("directives");
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();

    assert!(ledger
        .upsert_directive(&directive("d1", 1, "orch", "v1"))
        .unwrap());
    // Duplicate is a no-op.
    assert!(!ledger
        .upsert_directive(&directive("d1", 1, "orch", "v1"))
        .unwrap());
    // Same version, different contents: conflict.
    assert!(matches!(
        ledger.upsert_directive(&directive("d1", 1, "orch", "changed")),
        Err(ContractError::Conflict(_))
    ));
    // Newer version replaces.
    assert!(ledger
        .upsert_directive(&directive("d1", 2, "orch", "v2"))
        .unwrap());
    let row = ledger.directive("d1").unwrap().unwrap();
    assert_eq!(row.version, 2);
    assert_eq!(row.payload, json!({ "marker": "v2" }));
    // Older version never regresses the row.
    assert!(!ledger
        .upsert_directive(&directive("d1", 1, "orch", "v1"))
        .unwrap());
    assert_eq!(ledger.directive("d1").unwrap().unwrap().version, 2);
}

#[test]
fn directive_survives_reopen() {
    let dir = run_dir("directive-reopen");
    let path = dir.join("ledger.sqlite");
    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        ledger
            .upsert_directive(&directive("d1", 3, "orch", "v3"))
            .unwrap();
    }
    let ledger = SqliteLedger::open(&path).unwrap();
    let row = ledger.directive("d1").unwrap().unwrap();
    assert_eq!(row.version, 3);
    assert_eq!(row.source, "orch");
    assert_eq!(row.state, DirectiveState::Active);
}

// ---------------------------------------------------------------------------
// Control receipts (schema v4, TUI.5 agalma-3kr.6)
// ---------------------------------------------------------------------------

fn control_receipt(action_id: &str, outcome: &str) -> ControlReceipt {
    ControlReceipt {
        action_id: action_id.to_string(),
        action: "pause".to_string(),
        target: String::new(),
        actor: "human".to_string(),
        request: json!({ "outcome": outcome }),
        recorded_at_unix_ms: now_ms(),
    }
}

#[test]
fn control_receipts_round_trip_and_are_idempotent_by_action_id() {
    let dir = run_dir("control-receipts");
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();

    ledger
        .record_control_receipt(&control_receipt("a1", "applied"))
        .unwrap();
    ledger
        .record_control_receipt(&control_receipt("a2", "denied"))
        .unwrap();
    // Identical re-record (ignoring the timestamp) is a harmless no-op.
    ledger
        .record_control_receipt(&control_receipt("a1", "applied"))
        .unwrap();
    // Reusing an action id with different contents is a conflict.
    assert!(matches!(
        ledger.record_control_receipt(&control_receipt("a1", "denied")),
        Err(ContractError::Conflict(_))
    ));

    let receipts = ledger.control_receipts().unwrap();
    assert_eq!(receipts.len(), 2, "no duplicate row for a replay");
    assert_eq!(receipts[0].action_id, "a1");
    assert_eq!(receipts[0].request, json!({ "outcome": "applied" }));
    assert_eq!(receipts[1].action_id, "a2");
}

#[test]
fn control_receipts_survive_reopen() {
    let dir = run_dir("control-reopen");
    let path = dir.join("ledger.sqlite");
    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        ledger
            .record_control_receipt(&control_receipt("a1", "applied"))
            .unwrap();
    }
    let ledger = SqliteLedger::open(&path).unwrap();
    let receipts = ledger.control_receipts().unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].action, "pause");
    assert_eq!(receipts[0].actor, "human");
}

#[test]
fn migration_v3_to_v4_when_all_executions_terminal() {
    let dir = run_dir("migrate-v3-terminal");
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
        ledger.downgrade_to_v3().unwrap();
        assert_eq!(ledger.schema_version().unwrap(), 3);
    }

    let mut ledger = SqliteLedger::open(&path).unwrap();
    assert_eq!(
        ledger.schema_version().unwrap(),
        SCHEMA_VERSION,
        "terminal v3 database migrates to v4"
    );
    // v4 surface is live after migration.
    ledger
        .record_control_receipt(&control_receipt("a1", "applied"))
        .unwrap();
    assert_eq!(ledger.control_receipts().unwrap().len(), 1);
}

#[test]
fn migration_v3_to_v4_refused_with_in_flight_execution() {
    let dir = run_dir("migrate-v3-inflight");
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
        ledger.downgrade_to_v3().unwrap();
    }

    let err = SqliteLedger::open(&path).map(|_| ()).unwrap_err();
    assert!(matches!(err, ContractError::KnownFailure(_)), "err={err:?}");
    assert!(
        err.to_string().contains("in-flight"),
        "reason must name in-flight executions: {err}"
    );
}

// ---------------------------------------------------------------------------
// Explicit fresh initialization vs established open (`agalma-5fy.5`)
// ---------------------------------------------------------------------------

/// `open_existing` refuses a missing database and never creates one.
#[test]
fn open_existing_missing_refuses_without_creating() {
    let dir = run_dir("open-existing-missing");
    let path = dir.join("ledger.sqlite");
    assert!(!path.exists(), "starts absent");

    let err = SqliteLedger::open_existing(&path).map(|_| ()).unwrap_err();
    assert!(
        err.to_string().contains("missing"),
        "clear missing reason: {err}"
    );
    assert!(!path.exists(), "open_existing must never create the database");
}

/// `open_existing` opens an already-initialized ledger (same schema, clear
/// latch) and refuses a corrupt file.
#[test]
fn open_existing_opens_initialized_and_refuses_corrupt() {
    let dir = run_dir("open-existing-initialized");
    let path = dir.join("ledger.sqlite");
    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        ledger.set_kill_latch(true).unwrap();
    }
    let ledger = SqliteLedger::open_existing(&path).expect("open existing initialized");
    assert!(ledger.kill_latch().unwrap(), "latch read through existing open");

    let corrupt = dir.join("corrupt.sqlite");
    std::fs::write(&corrupt, b"not a sqlite database").unwrap();
    let err = SqliteLedger::open_existing(&corrupt).map(|_| ()).unwrap_err();
    assert!(
        matches!(err, ContractError::KnownFailure(_)),
        "corrupt existing refuses: {err:?}"
    );
}

/// `open_existing` refuses an incompatible established schema (no migration) and
/// does not rewrite the recorded version.
#[test]
fn open_existing_incompatible_refuses_without_migrating() {
    let dir = run_dir("open-existing-incompatible");
    let path = dir.join("ledger.sqlite");
    drop(SqliteLedger::open(&path).unwrap());
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value='999' WHERE key='schema_version'", [])
            .unwrap();
    }

    let err = SqliteLedger::open_existing(&path).map(|_| ()).unwrap_err();
    assert!(
        matches!(err, ContractError::Conflict(_)),
        "incompatible existing refuses: {err:?}"
    );
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "999", "open_existing must not migrate/rewrite");
}

// ---------------------------------------------------------------------------
// HP.5 schema v5 component store, explicit migration and complete-state v2
// (`agalma-jmr.5`)
// ---------------------------------------------------------------------------

use agalma_contracts::task_package::BindingPin;

fn fixture_binding(generation: u64) -> BindingPin {
    BindingPin {
        binding_id: "fixture.reporter".to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version: "fixture-1".to_string(),
        generation,
        config_digest: format!("sha256:{}", "a".repeat(64)),
    }
}

fn sealed(mut commit: ComponentCommit) -> ComponentCommit {
    commit.input_digest = commit.computed_input_digest().unwrap();
    commit
}

fn sample_component_commit(revision: u64, value: i64) -> ComponentCommit {
    let binding = fixture_binding(1);
    sealed(ComponentCommit {
        format: COMPONENT_COMMIT_FORMAT.to_string(),
        namespace: "example.action-fixture".to_string(),
        commit_id: format!("commit-{revision}"),
        input_digest: String::new(),
        expected_revision: revision,
        checkpoint: Some(json!({ "value": value })),
        events: vec![ComponentEvent {
            event_id: format!("event-{revision}"),
            kind: "fixture.updated".to_string(),
            payload: json!({ "value": value }),
            recorded_at_unix_ms: 1_791_820_800_000 + revision,
        }],
        operations: vec![ComponentOperation {
            operation_id: format!("example.action-fixture/op-{revision}"),
            input_digest: format!("sha256:{}", "1".repeat(64)),
            binding: binding.clone(),
            intent: json!({ "value": value }),
            state: agalma_contracts::ComponentOperationState::Completed,
            deadline_unix_ms: 1_791_820_810_000,
        }],
        receipts: vec![ComponentReceipt {
            operation_id: format!("example.action-fixture/op-{revision}"),
            input_digest: format!("sha256:{}", "1".repeat(64)),
            binding,
            outcome: json!({ "value": value }),
            completed_at_unix_ms: 1_791_820_800_001 + revision,
        }],
    })
}

#[test]
fn component_commit_is_atomic_and_replay_safe() {
    let dir = run_dir("component-commit");
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();

    let commit = sample_component_commit(0, 1);
    let anchor = ledger.component_commit(&commit).unwrap();
    assert_eq!(anchor.revision, 1);
    assert_eq!(anchor.input_digest, commit.input_digest);

    let state = ledger.component_read(&commit.namespace).unwrap().unwrap();
    assert_eq!(state.revision, 1);
    assert_eq!(state.watermark, 1);
    assert_eq!(state.checkpoint, Some(json!({ "value": 1 })));
    let events = ledger.component_events(&commit.namespace, 0, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.payload, json!({ "value": 1 }));
    let op = ledger
        .component_operation(&commit.namespace, "example.action-fixture/op-0")
        .unwrap()
        .unwrap();
    assert_eq!(op.operation.state, agalma_contracts::ComponentOperationState::Completed);

    // Identical replay returns the original anchor even though the revision has
    // not changed (replay is checked before the revision guard).
    let replay = ledger.component_commit(&commit).unwrap();
    assert_eq!(replay, anchor);

    // Changed content under the same commit id conflicts.
    let mut changed = commit.clone();
    changed.checkpoint = Some(json!({ "value": 99 }));
    changed.input_digest = changed.computed_input_digest().unwrap();
    let err = ledger.component_commit(&changed).unwrap_err();
    assert_eq!(
        agalma_contracts::digest::failure_code(&err),
        Some(agalma_contracts::FailureCode::IdentityConflict)
    );

    // A new commit id with a stale expected revision conflicts and writes
    // nothing.
    let mut stale = sample_component_commit(1, 2);
    stale.commit_id = "commit-stale".to_string();
    stale.expected_revision = 0;
    stale.operations.clear();
    stale.receipts.clear();
    let stale = sealed(stale);
    let err = ledger.component_commit(&stale).unwrap_err();
    assert_eq!(
        agalma_contracts::digest::failure_code(&err),
        Some(agalma_contracts::FailureCode::RevisionConflict)
    );
    assert_eq!(
        ledger.component_read(&commit.namespace).unwrap().unwrap().revision,
        1
    );
    assert_eq!(
        ledger.component_events(&commit.namespace, 0, 10).unwrap().len(),
        1
    );
}

#[test]
fn component_commit_duplicate_operation_content_conflicts_without_partial_rows() {
    let dir = run_dir("component-conflict");
    let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    ledger.component_commit(&sample_component_commit(0, 1)).unwrap();

    // New commit id + new event, but reuses op-0 with changed content.
    let mut conflicting = sample_component_commit(1, 2);
    conflicting.operations[0].operation_id = "example.action-fixture/op-0".to_string();
    conflicting.operations[0].intent = json!({ "value": 999 });
    conflicting.receipts.clear();
    let conflicting = sealed(conflicting);
    let err = ledger.component_commit(&conflicting).unwrap_err();
    assert_eq!(
        agalma_contracts::digest::failure_code(&err),
        Some(agalma_contracts::FailureCode::IdentityConflict)
    );
    // Nothing partially committed: revision/event count unchanged.
    assert_eq!(
        ledger.component_read(&"example.action-fixture").unwrap().unwrap().revision,
        1
    );
    assert_eq!(
        ledger.component_events("example.action-fixture", 0, 10).unwrap().len(),
        1
    );
}

#[test]
fn normal_v4_open_does_not_upgrade_to_v5() {
    let dir = run_dir("v4-no-upgrade");
    let path = dir.join("ledger.sqlite");
    {
        let ledger = SqliteLedger::open(&path).unwrap();
        assert_eq!(ledger.schema_version().unwrap(), SCHEMA_VERSION);
        ledger.downgrade_to_v4().unwrap();
        assert_eq!(ledger.schema_version().unwrap(), 4);
    }

    // A normal open leaves the v4 database untouched (no silent v5 migration).
    let ledger = SqliteLedger::open(&path).unwrap();
    assert_eq!(ledger.schema_version().unwrap(), 4);
    assert!(
        ledger.component_read("ns").is_err(),
        "v4 use refuses v5 component reads"
    );

    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: String = conn
        .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, "4", "normal open must not migrate v4 -> v5");
}

#[test]
fn explicit_v5_migration_refuses_non_quiescent_state() {
    let dir = run_dir("v5-refuse-inflight");
    let path = dir.join("ledger.sqlite");
    let exec = fixture_exec();
    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(record(&exec, ExecutionPhase::Build, ExecutionState::Running)),
                events: vec![],
                receipts: vec![],
                intents: vec![],
            })
            .unwrap();
        ledger.downgrade_to_v4().unwrap();
    }

    let mut ledger = SqliteLedger::open(&path).unwrap();
    let err = ledger.migrate_to_v5().unwrap_err();
    assert!(
        err.to_string().contains("not quiescent"),
        "refusal names quiescence: {err}"
    );
    assert_eq!(ledger.schema_version().unwrap(), 4, "no upgrade on refusal");
    let backups: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("v4-backup"))
        .collect();
    assert!(backups.is_empty(), "refused migration writes no backup");

    // A parked row alone is not quiescence when an unresolved effect remains.
    let dir2 = run_dir("v5-refuse-parked-intent");
    let path2 = dir2.join("ledger.sqlite");
    let exec2 = fixture_exec();
    {
        let mut pending = SqliteLedger::open(&path2).unwrap();
        pending
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(record(
                    &exec2,
                    ExecutionPhase::Parked,
                    ExecutionState::Parked,
                )),
                events: vec![],
                receipts: vec![],
                intents: vec![intent(&exec2, &OperationId::derive(&exec2, "build"))],
            })
            .unwrap();
        pending.downgrade_to_v4().unwrap();
    }
    let err = SqliteLedger::open(&path2).unwrap().migrate_to_v5().unwrap_err();
    assert!(
        err.to_string().contains("unconsumed dispatch intent"),
        "parked row with a pending intent is not quiescent: {err}"
    );
}

#[test]
fn explicit_v5_migration_is_transactional_with_backup_and_receipt() {
    let dir = run_dir("v5-migrate");
    let path = dir.join("ledger.sqlite");
    let exec = fixture_exec();
    let op = OperationId::derive(&exec, "build");
    {
        let mut ledger = SqliteLedger::open(&path).unwrap();
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(record(&exec, ExecutionPhase::Done, ExecutionState::Completed)),
                events: vec![event(&exec, "done", json!({ "ok": true }))],
                receipts: vec![OperationReceipt {
                    operation_id: op.clone(),
                    execution_id: exec.clone(),
                    kind: "build".to_string(),
                    inputs_hash: "h".to_string(),
                    result: json!({ "ok": true }),
                    completed_at_unix_ms: 7,
                }],
                intents: vec![],
            })
            .unwrap();
        ledger
            .record_control_receipt(&ControlReceipt {
                action_id: "pause-1".to_string(),
                action: "pause".to_string(),
                target: String::new(),
                actor: "human".to_string(),
                request: json!({ "outcome": "applied" }),
                recorded_at_unix_ms: 9,
            })
            .unwrap();
        ledger.downgrade_to_v4().unwrap();
    }

    let mut ledger = SqliteLedger::open(&path).unwrap();
    assert_eq!(ledger.schema_version().unwrap(), 4);
    let receipt = ledger.migrate_to_v5().unwrap();
    assert_eq!(receipt.from_version, 4);
    assert_eq!(receipt.to_version, 5);
    assert_eq!(ledger.schema_version().unwrap(), 5);
    assert!(
        Path::new(&receipt.backup_path).is_file(),
        "durable backup exists at {}",
        receipt.backup_path
    );
    let stored = ledger.migration_receipt(&receipt.migration_id).unwrap().unwrap();
    assert_eq!(stored, receipt);

    // Existing v4 data survived and the new component store is live.
    assert!(ledger.operation_receipt(&op).unwrap().is_some());
    assert_eq!(ledger.control_receipts().unwrap().len(), 1);
    ledger
        .component_commit(&sample_component_commit(0, 1))
        .unwrap();
    assert_eq!(
        ledger.component_read("example.action-fixture").unwrap().unwrap().revision,
        1
    );
}

#[test]
fn complete_state_v2_round_trip_preserves_every_section() {
    let source_dir = run_dir("complete-v2-source");
    let mut source = SqliteLedger::open(source_dir.join("ledger.sqlite")).unwrap();
    let exec = fixture_exec();
    let op = OperationId::derive(&exec, "build");

    source
        .commit(CommitBatch {
            expected_revision: None,
            state: Some(record(&exec, ExecutionPhase::Done, ExecutionState::Completed)),
            events: vec![event(&exec, "done", json!({ "ok": true }))],
            receipts: vec![OperationReceipt {
                operation_id: op.clone(),
                execution_id: exec.clone(),
                kind: "build".to_string(),
                inputs_hash: "h".to_string(),
                result: json!({ "ok": true }),
                completed_at_unix_ms: 7,
            }],
            intents: vec![],
        })
        .unwrap();
    let decision = DecisionRequest {
        version: 1,
        operation_id: op.clone(),
        kind: DecisionKind::TriagePickNext,
        context: json!({ "prompt": "p" }),
        artifacts: vec![],
        options: vec![agalma_contracts::DecisionOption {
            option_id: "a".to_string(),
            attributes: json!({}),
        }],
        deadline_unix_ms: 1000,
        pin: agalma_contracts::DecisionPin {
            policy: "policy/v1".to_string(),
            genome: "genome/v1".to_string(),
            model: "static".to_string(),
        },
    };
    source.record_decision(&decision).unwrap();
    source
        .record_decision_outcome(
            &op,
            &DecisionOutcome::Applied {
                chosen: Some("a".to_string()),
                scores: vec![],
            },
        )
        .unwrap();
    source
        .record_digest(&DigestRecord {
            execution_id: exec.clone(),
            version: 1,
            summary: json!({ "cost_nusd": 1 }),
            artifact_ref: Some(ArtifactRef::derive("digest.json")),
            recorded_at_unix_ms: 12,
        })
        .unwrap();
    source
        .record_control_receipt(&ControlReceipt {
            action_id: "cancel-1".to_string(),
            action: "cancel".to_string(),
            target: exec.as_str().to_string(),
            actor: "human".to_string(),
            request: json!({ "outcome": "applied" }),
            recorded_at_unix_ms: 13,
        })
        .unwrap();
    source
        .insert_timer(&TimerRecord {
            timer_id: "timer-1".to_string(),
            execution_id: exec.clone(),
            kind: "retry".to_string(),
            fire_at_unix_ms: 100,
            state: TimerState::Fired,
            payload: json!({ "attempt": 1 }),
        })
        .unwrap();
    source
        .upsert_directive(&DirectiveRow {
            directive_id: "d1".to_string(),
            version: 1,
            source: "human".to_string(),
            state: DirectiveState::Active,
            payload: json!({ "allow": ["prompt"] }),
            received_at_unix_ms: 14,
        })
        .unwrap();
    source
        .upsert_binding(&BindingRecord {
            binding_id: BindingId::new("binding:ledger"),
            component_api: "ledger".to_string(),
            api_version: 1,
            implementation: "sqlite".to_string(),
            implementation_version: "v1".to_string(),
            capabilities: BTreeSet::new(),
            required_capabilities: BTreeSet::new(),
            optional_capabilities: BTreeSet::new(),
            config_hash: "cfg".to_string(),
            generation: 1,
            state: BindingState::Active,
        })
        .unwrap();
    source.begin_lease(&TaskId::new("fix-answer")).unwrap();
    source
        .kernel_meta_set("exec_def_version", "exec_def/v1")
        .unwrap();
    source.component_commit(&sample_component_commit(0, 1)).unwrap();
    source.component_commit(&sample_component_commit(1, 2)).unwrap();

    let exported = source.export_complete_state().unwrap();
    assert_eq!(exported.format, agalma_contracts::COMPLETE_STATE_FORMAT);
    assert_eq!(exported.schema_version, 5);
    assert!(!exported.decisions.is_empty());
    assert!(!exported.decision_outcomes.is_empty());
    assert!(!exported.digests.is_empty());
    assert!(!exported.control_receipts.is_empty());
    assert!(!exported.component_states.is_empty());
    assert_eq!(exported.component_events.len(), 2);
    assert_eq!(exported.component_operations.len(), 2);
    assert_eq!(exported.component_receipts.len(), 2);
    assert_eq!(exported.component_commits.len(), 2);
    assert_eq!(exported.component_states[0].revision, 2);

    let dest_dir = run_dir("complete-v2-dest");
    let mut dest = SqliteLedger::open(dest_dir.join("ledger.sqlite")).unwrap();
    let json = exported.to_canonical_json().unwrap();
    dest.import_complete_state_json(&json).unwrap();

    let reexported = dest.export_complete_state().unwrap();
    assert_eq!(reexported, exported, "complete v2 round trip preserves identity");

    // Durable replay identity survives restore: identical commit replays its
    // original anchor; changed input conflicts.
    let replay = dest.component_commit(&sample_component_commit(0, 1)).unwrap();
    let original = exported
        .component_commits
        .iter()
        .find(|a| a.commit_id == "commit-0")
        .unwrap()
        .clone();
    assert_eq!(replay, original);
    let mut changed = sample_component_commit(0, 1);
    changed.checkpoint = Some(json!({ "value": 999 }));
    changed.input_digest = changed.computed_input_digest().unwrap();
    assert_eq!(
        agalma_contracts::digest::failure_code(&dest.component_commit(&changed).unwrap_err()),
        Some(agalma_contracts::FailureCode::IdentityConflict)
    );
}

#[test]
fn complete_state_import_refuses_nonempty_legacy_unknown_and_incomplete() {
    let dir = run_dir("complete-v2-refusals");
    let source_path = dir.join("source.sqlite");
    let mut source = SqliteLedger::open(&source_path).unwrap();
    source
        .component_commit(&sample_component_commit(0, 1))
        .unwrap();
    let exported = source.export_complete_state().unwrap();

    // Non-empty destination refuses.
    let mut dest = SqliteLedger::open(dir.join("dest.sqlite")).unwrap();
    dest.component_commit(&sample_component_commit(0, 1)).unwrap();
    let err = dest.import_complete_state(&exported).unwrap_err();
    assert!(matches!(err, ContractError::Conflict(_)), "non-empty: {err}");

    // Legacy v1 export alone is incomplete for production restore.
    let legacy = source.export_state().unwrap().to_canonical_json().unwrap();
    let mut empty = SqliteLedger::open(dir.join("empty.sqlite")).unwrap();
    assert_eq!(
        agalma_contracts::digest::failure_code(
            &empty.import_complete_state_json(&legacy).unwrap_err()
        ),
        Some(agalma_contracts::FailureCode::LegacyInputsMissing)
    );

    // Unknown sections refuse.
    let mut unknown: Value = serde_json::to_value(&exported).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("future_section".to_string(), json!([]));
    assert_eq!(
        agalma_contracts::digest::failure_code(
            &empty
                .import_complete_state_json(&unknown.to_string())
                .unwrap_err()
        ),
        Some(agalma_contracts::FailureCode::UnsupportedSchema)
    );

    // Incomplete v2 (missing a required section) refuses.
    let mut incomplete: Value = serde_json::to_value(&exported).unwrap();
    incomplete.as_object_mut().unwrap().remove("component_commits");
    assert!(empty
        .import_complete_state_json(&incomplete.to_string())
        .is_err());
}
