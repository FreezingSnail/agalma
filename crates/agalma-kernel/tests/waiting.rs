//! B0.4 durable-waiting tests: timers, deadlines, and directive updates.
//!
//! - `restart_during_retry_delay_fires_timer_once` (fixture case 6): a child
//!   aborts after the retry timer row commits (`after_timer_commit`); the parent
//!   reopens the isolated ledger and proves the overdue timer fires exactly once
//!   after restart, a second restart re-fires nothing, and the retry proceeds
//!   once.
//! - `directive_update_survives_restart_and_applies_once` (fixture case 7): a
//!   child aborts after the directive row commits (`after_directive_commit`);
//!   recovery applies the directive once, a duplicate is a no-op, a newer
//!   directive replaces queued work, and a persisted kill latch still blocks
//!   dispatch after restart.
//! - Focused unit-style cases: duplicate `TimerFired` no-op, timer cancellation
//!   with terminal work, duplicate directive no-op, newer-directive replacement,
//!   and next-deadline computation.
//!
//! Isolated state lives under `target/test-runs/recovery-*` via the frozen
//! harness; the protected fixture pack is never modified.

mod support;

use std::path::Path;
use std::process::Command as StdCommand;

use agalma_contracts::{
    DirectiveState, ExecutionId, LedgerApi, OperationId, TaskId, TimerRecord, TimerState,
};
use agalma_kernel::{
    ActivityDef, Command, DirectivePlan, DirectiveStep, ExecState, Inputs, KernelEngine,
    OperationState, Probe,
};
use agalma_ledger::SqliteLedger;
use serde_json::json;

use support::harness;

/// Environment variable passing the parent's run directory to the crash child.
const DIR_ENV: &str = "KERNEL_RECOVERY_DIR";

const CASE_6: &str = "restart-during-retry-delay";
const CASE_7: &str = "directive-update-restart";

fn exec() -> ExecutionId {
    ExecutionId::derive(&TaskId::new("fix-answer"), 1)
}

fn start_command() -> Command {
    Command::Start {
        task_id: TaskId::new("fix-answer"),
        generation: 1,
    }
}

fn retry_timer(exec: &ExecutionId) -> TimerRecord {
    TimerRecord {
        timer_id: format!("timer:retry:{}", exec.as_str()),
        execution_id: exec.clone(),
        kind: "retry".to_string(),
        fire_at_unix_ms: 1_000,
        state: TimerState::Pending,
        payload: json!({ "operation": "build", "attempt": 1 }),
    }
}

fn directive_step(name: &str) -> DirectiveStep {
    DirectiveStep {
        step: name.to_string(),
        activity: ActivityDef::new("fake/build", "v1"),
        phase: agalma_contracts::ExecutionPhase::Build,
        inputs: json!({ "task": "fix-answer" }),
    }
}

fn plan(source: &str, steps: &[&str]) -> DirectivePlan {
    DirectivePlan {
        source: source.to_string(),
        steps: steps.iter().map(|s| directive_step(s)).collect(),
    }
}

fn timer_event_count(engine: &KernelEngine<SqliteLedger>, exec: &ExecutionId) -> usize {
    engine
        .ledger()
        .events(exec)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "timer_fired")
        .count()
}

fn directive_event_count(engine: &KernelEngine<SqliteLedger>, exec: &ExecutionId) -> usize {
    engine
        .ledger()
        .events(exec)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "directive_update")
        .count()
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
// Case 6 — restart during retry delay fires the timer once
// ---------------------------------------------------------------------------

#[test]
fn restart_during_retry_delay_fires_timer_once() {
    if harness::child_case().is_some() {
        run_retry_child();
        return;
    }

    let dir = harness::recovery_dir(CASE_6);
    let status = spawn_child("restart_during_retry_delay_fires_timer_once", CASE_6, &dir);
    harness::assert_sigabrt(&status);

    let exec = exec();
    let timer_id = retry_timer(&exec).timer_id;

    // The timer row survived the crash, still pending, before its deadline.
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let row = engine.ledger().timer(&timer_id).unwrap().unwrap();
    assert_eq!(row.state, TimerState::Pending);
    assert_eq!(row.fire_at_unix_ms, 1_000);
    assert_eq!(engine.next_deadline().unwrap(), Some(1_000));
    assert!(!engine.has_runnable_work(999).unwrap());
    assert!(engine.has_runnable_work(1_000).unwrap());

    // Overdue recovery fires exactly one TimerFired and marks the row fired.
    let fired = engine.fire_due_timers(1_000).unwrap();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].timer_id, timer_id);
    assert!(fired[0].applied, "the overdue timer fired");
    assert_eq!(
        engine.ledger().timer(&timer_id).unwrap().unwrap().state,
        TimerState::Fired
    );
    assert_eq!(timer_event_count(&engine, &exec), 1);

    // Idempotent: firing again records nothing new.
    assert!(engine.fire_due_timers(1_000).unwrap().is_empty());
    assert_eq!(timer_event_count(&engine, &exec), 1);
    assert_eq!(engine.next_deadline().unwrap(), None);

    // A second restart does not re-fire the same timer.
    drop(engine);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    assert_eq!(
        engine.ledger().timer(&timer_id).unwrap().unwrap().state,
        TimerState::Fired
    );
    assert!(engine.fire_due_timers(1_000).unwrap().is_empty());
    assert_eq!(timer_event_count(&engine, &exec), 1);
    assert!(engine
        .load(&exec)
        .unwrap()
        .fired_timers
        .contains_key(&timer_id));

    // The retry proceeds exactly once (the caller derives it from the fired
    // timer; the timer itself never advances the phase).
    let build = OperationId::derive(&exec, "build");
    engine
        .apply(
            &exec,
            &Command::Retry {
                operation_id: build,
            },
            &Inputs::default(),
        )
        .unwrap();
    assert_eq!(engine.load(&exec).unwrap().attempt, 1);
}

/// Child role: start, commit the retry timer row, and abort at the hook.
fn run_retry_child() {
    let dir = std::env::var(DIR_ENV).expect("child run dir");
    let ledger = SqliteLedger::open(Path::new(&dir).join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();

    harness::disarm();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    harness::arm(harness::AFTER_TIMER_COMMIT);
    // Commits the timer row, then aborts at the hook before it can fire.
    let _ = engine.schedule_timer(&retry_timer(&exec));
    harness::disarm();
    panic!("crash child should have aborted after the timer commit");
}

// ---------------------------------------------------------------------------
// Case 7 — directive update survives restart and applies once
// ---------------------------------------------------------------------------

#[test]
fn directive_update_survives_restart_and_applies_once() {
    if harness::child_case().is_some() {
        run_directive_child();
        return;
    }

    let dir = harness::recovery_dir(CASE_7);
    let status = spawn_child(
        "directive_update_survives_restart_and_applies_once",
        CASE_7,
        &dir,
    );
    harness::assert_sigabrt(&status);

    let exec = exec();
    let build = OperationId::derive(&exec, "build");

    // The directive row survived; the transition did not run before the crash.
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let row = engine.ledger().directive("dir-1").unwrap().unwrap();
    assert_eq!(row.version, 1);
    assert_eq!(row.state, DirectiveState::Active);
    assert_eq!(row.source, "orchestrator");
    assert!(!engine.load(&exec).unwrap().directives.contains_key("dir-1"));

    // Recovery applies the directive exactly once: build is queued + dispatched.
    let applied = engine.apply_directive(&exec, "dir-1", 5).unwrap();
    assert_eq!(applied.state.directives["dir-1"].version, 1);
    let state = engine.load(&exec).unwrap();
    assert_eq!(state.operations[&build].state, OperationState::Dispatched);
    assert_eq!(engine.pending_dispatch().unwrap().len(), 1);
    assert_eq!(directive_event_count(&engine, &exec), 1);

    // A duplicate application is a harmless no-op.
    let revision = engine.load(&exec).unwrap().revision;
    engine.apply_directive(&exec, "dir-1", 6).unwrap();
    assert_eq!(engine.load(&exec).unwrap().revision, revision);
    assert_eq!(directive_event_count(&engine, &exec), 1);

    // A newer directive replaces queued work derived from the prior version.
    engine
        .submit_directive(&exec, "dir-1", 2, &plan("orchestrator", &["test"]), 7)
        .unwrap();
    let state = engine.load(&exec).unwrap();
    assert_eq!(state.directives["dir-1"].version, 2);
    assert_eq!(state.operations[&build].state, OperationState::Cancelled);
    let test = OperationId::derive(&exec, "test");
    assert_eq!(state.operations[&test].state, OperationState::Dispatched);
    let pending = engine.pending_dispatch().unwrap();
    assert_eq!(pending.len(), 1, "the replaced work's intent was released");
    assert_eq!(pending[0].operation_id, test);

    // The kill latch survives restart and still blocks dispatch.
    engine
        .apply(
            &exec,
            &Command::KillLatch { latched: true },
            &Inputs::default(),
        )
        .unwrap();
    drop(engine);

    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let state = engine.load(&exec).unwrap();
    assert!(state.kill_latched, "kill latch survived restart");
    assert_eq!(
        state.directives["dir-1"].version, 2,
        "directive state survived"
    );

    let verify = OperationId::derive(&exec, "verify");
    engine
        .apply(
            &exec,
            &Command::Deliver {
                operation_id: verify.clone(),
                inputs_hash: "h1".to_string(),
            },
            &Inputs {
                probe: Probe::Absent,
                now_unix_ms: 8,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert!(
        !engine.load(&exec).unwrap().operations.contains_key(&verify),
        "latched kernel blocks recovered dispatch"
    );
}

/// Child role: start, commit the directive row, and abort at the hook.
fn run_directive_child() {
    let dir = std::env::var(DIR_ENV).expect("child run dir");
    let ledger = SqliteLedger::open(Path::new(&dir).join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();

    harness::disarm();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    harness::arm(harness::AFTER_DIRECTIVE_COMMIT);
    // Commits the directive row, then aborts at the hook before the transition.
    let _ = engine.submit_directive(&exec, "dir-1", 1, &plan("orchestrator", &["build"]), 1);
    harness::disarm();
    panic!("crash child should have aborted after the directive commit");
}

// ---------------------------------------------------------------------------
// Focused timer cases
// ---------------------------------------------------------------------------

#[test]
fn duplicate_timer_fired_is_a_noop() {
    let (_dir, ledger) = harness::open_isolated_ledger("duplicate-timer");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    let command = Command::TimerFired {
        timer_id: "t1".to_string(),
        content_hash: "c1".to_string(),
    };
    engine.apply(&exec, &command, &Inputs::default()).unwrap();
    let revision = engine.load(&exec).unwrap().revision;

    // A second firing of the same timer id is a no-op (no event, no revision).
    engine.apply(&exec, &command, &Inputs::default()).unwrap();
    assert_eq!(engine.load(&exec).unwrap().revision, revision);
    assert_eq!(timer_event_count(&engine, &exec), 1);
}

#[test]
fn timer_cancelled_with_work() {
    let (_dir, ledger) = harness::open_isolated_ledger("cancel-timer");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    let timer = retry_timer(&exec);
    engine.schedule_timer(&timer).unwrap();
    assert_eq!(
        engine
            .ledger()
            .timer(&timer.timer_id)
            .unwrap()
            .unwrap()
            .state,
        TimerState::Pending
    );

    // Cancelling the work cancels its pending timer; nothing fires.
    engine
        .apply(&exec, &Command::Cancel, &Inputs::default())
        .unwrap();
    assert_eq!(engine.load(&exec).unwrap().state, ExecState::Cancelled);
    assert_eq!(
        engine
            .ledger()
            .timer(&timer.timer_id)
            .unwrap()
            .unwrap()
            .state,
        TimerState::Cancelled
    );
    assert!(engine.fire_due_timers(u64::MAX).unwrap().is_empty());
    assert_eq!(engine.next_deadline().unwrap(), None);
}

#[test]
fn next_deadline_computation() {
    let (_dir, ledger) = harness::open_isolated_ledger("next-deadline");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    assert_eq!(engine.next_deadline().unwrap(), None);

    let later = TimerRecord {
        timer_id: "t-later".to_string(),
        execution_id: exec.clone(),
        kind: "deadline".to_string(),
        fire_at_unix_ms: 2_000,
        state: TimerState::Pending,
        payload: json!({ "phase": "verify" }),
    };
    let sooner = TimerRecord {
        timer_id: "t-sooner".to_string(),
        execution_id: exec.clone(),
        kind: "deadline".to_string(),
        fire_at_unix_ms: 1_000,
        state: TimerState::Pending,
        payload: json!({ "phase": "build" }),
    };
    engine.schedule_timer(&later).unwrap();
    engine.schedule_timer(&sooner).unwrap();

    assert_eq!(engine.next_deadline().unwrap(), Some(1_000));
    assert!(!engine.has_runnable_work(500).unwrap());
    assert!(engine.has_runnable_work(1_000).unwrap());

    engine.fire_due_timers(1_500).unwrap();
    assert_eq!(engine.next_deadline().unwrap(), Some(2_000));
    engine.fire_due_timers(3_000).unwrap();
    assert_eq!(engine.next_deadline().unwrap(), None);
}

// ---------------------------------------------------------------------------
// Focused directive cases
// ---------------------------------------------------------------------------

#[test]
fn duplicate_directive_is_a_noop() {
    let (_dir, ledger) = harness::open_isolated_ledger("duplicate-directive");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    let v1 = plan("orchestrator", &["build"]);
    engine.submit_directive(&exec, "dir-1", 1, &v1, 1).unwrap();
    let revision = engine.load(&exec).unwrap().revision;

    // Same id + version + contents: harmless no-op (no new event/revision).
    engine.submit_directive(&exec, "dir-1", 1, &v1, 2).unwrap();
    assert_eq!(engine.load(&exec).unwrap().revision, revision);
    assert_eq!(directive_event_count(&engine, &exec), 1);
    assert_eq!(
        engine.ledger().directive("dir-1").unwrap().unwrap().version,
        1
    );
}

#[test]
fn newer_directive_replaces_queued_work() {
    let (_dir, ledger) = harness::open_isolated_ledger("newer-directive");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();

    let build = OperationId::derive(&exec, "build");
    let test = OperationId::derive(&exec, "test");

    engine
        .submit_directive(
            &exec,
            "dir-1",
            1,
            &plan("orchestrator", &["build", "test"]),
            1,
        )
        .unwrap();
    let state = engine.load(&exec).unwrap();
    assert_eq!(state.operations[&build].state, OperationState::Dispatched);
    assert_eq!(state.operations[&test].state, OperationState::Dispatched);
    assert_eq!(engine.pending_dispatch().unwrap().len(), 2);

    // The newer version drops `test`: it is cancelled and its intent released.
    engine
        .submit_directive(&exec, "dir-1", 2, &plan("orchestrator", &["build"]), 2)
        .unwrap();
    let state = engine.load(&exec).unwrap();
    assert_eq!(state.directives["dir-1"].version, 2);
    assert_eq!(state.operations[&build].state, OperationState::Dispatched);
    assert_eq!(state.operations[&test].state, OperationState::Cancelled);
    let pending = engine.pending_dispatch().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].operation_id, build);
}
