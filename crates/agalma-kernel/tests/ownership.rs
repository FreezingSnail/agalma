//! B0.5 cancellation + ownership tests.
//!
//! - Cancellation: `cancel` persists a `Cancel` command; running/waiting work
//!   settles (pending intents released, timers cancelled, workers terminated
//!   with evidence); cancelling terminal work, and a duplicate cancel, are
//!   harmless no-ops.
//! - Ownership/fencing: a delivery carrying a superseded lease generation is
//!   fenced (recorded and dropped); reclaiming reconciles prior processes with
//!   recorded termination evidence before a fresh lease is usable.
//! - Kill latch: blocks recovered dispatch as well as new work.
//! - Case 5 `stale-lease-worker`: a worker survives a conductor failure; recovery
//!   confirms termination (or parks); reports from its stale lease cannot
//!   advance the task; only after reconciliation does a fresh claim dispatch the
//!   work, exactly once.
//!
//! Isolated state lives under `target/test-runs/recovery-*` via the frozen
//! harness; the protected fixture pack is never modified.

mod support;

use std::path::Path;
use std::process::Command as StdCommand;

use agalma_contracts::{
    ArtifactRef, ExecutionId, ExecutionPhase, LedgerApi, OperationId, TaskId, TimerRecord,
    TimerState,
};
use agalma_kernel::{
    operation_input_hash, recover_pending, run_activity, step, ActivityDef, ActivityRegistry,
    Command, ExecState, Inputs, KernelEngine, KernelState, OperationRequest, OperationResult,
    Probe, WorkerRecord,
};
use agalma_ledger::SqliteLedger;
use serde_json::json;

use support::harness::{self, EffectJournal, RecordingActivity};
use support::ops::JournalHandler;
use support::process::{self as fake, FakeLauncher};

/// Environment variable passing the parent's run directory to the crash child.
const DIR_ENV: &str = "KERNEL_RECOVERY_DIR";

const CASE_5: &str = "stale-lease-worker";

fn task() -> TaskId {
    TaskId::new("fix-answer")
}

fn exec() -> ExecutionId {
    ExecutionId::derive(&task(), 1)
}

fn start_command() -> Command {
    Command::Start {
        task_id: task(),
        generation: 1,
    }
}

fn deliver(op: &OperationId, hash: &str) -> Command {
    Command::Deliver {
        operation_id: op.clone(),
        inputs_hash: hash.to_string(),
    }
}

fn retry_timer(exec: &ExecutionId) -> TimerRecord {
    TimerRecord {
        timer_id: format!("timer:retry:{}", exec.as_str()),
        execution_id: exec.clone(),
        kind: "retry".to_string(),
        fire_at_unix_ms: 1_000,
        state: TimerState::Pending,
        payload: json!({ "operation": "build" }),
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
// Cancellation
// ---------------------------------------------------------------------------

#[test]
fn cancel_running_releases_intents_timers_and_workers() {
    let (dir, ledger) = harness::open_isolated_ledger("cancel-running");
    let mut engine = KernelEngine::open(ledger)
        .unwrap()
        .with_launcher(Box::new(FakeLauncher::new(&dir)));
    let exec = exec();
    let task = task();
    let lease = engine.begin_lease(&task).unwrap();
    assert_eq!(lease, 1);

    engine
        .apply(
            &exec,
            &start_command(),
            &Inputs {
                lease_generation: lease,
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .unwrap();
    let build = OperationId::derive(&exec, "build");
    engine
        .apply(
            &exec,
            &deliver(&build, "h1"),
            &Inputs {
                probe: Probe::Absent,
                lease_generation: lease,
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert_eq!(engine.pending_dispatch().unwrap().len(), 1);

    let timer = retry_timer(&exec);
    engine.schedule_timer(&timer).unwrap();

    let child = engine
        .launcher_mut()
        .unwrap()
        .launch(fake::spec(&dir))
        .unwrap();
    engine
        .record_worker(&WorkerRecord {
            execution_id: exec.clone(),
            operation_id: build.clone(),
            phase: ExecutionPhase::Build,
            attempt: 0,
            lease_generation: lease,
            pid: child.pid,
            pgid: child.pgid,
            started_at_unix_ms: child.started_at_unix_ms,
        })
        .unwrap();
    assert!(engine.launcher_mut().unwrap().alive(&child).unwrap());

    // Cancel: the execution settles — intent released, timer cancelled, worker
    // terminated with evidence.
    let outcome = engine.cancel(&exec, 5).unwrap();
    assert_eq!(outcome.state.state, ExecState::Cancelled);
    assert!(
        engine.pending_dispatch().unwrap().is_empty(),
        "cancellation releases the queued intent"
    );
    assert_eq!(
        engine
            .ledger()
            .timer(&timer.timer_id)
            .unwrap()
            .unwrap()
            .state,
        TimerState::Cancelled
    );
    assert!(
        !engine.launcher_mut().unwrap().alive(&child).unwrap(),
        "the worker was terminated"
    );
    let evidence = engine.termination_evidence(&exec).unwrap();
    assert_eq!(evidence.len(), 1);
    assert!(evidence[0].evidence.process_group_gone);
    assert!(engine.workers(&exec).unwrap().is_empty());
}

#[test]
fn cancel_waiting_execution_is_terminal() {
    let mut state = KernelState::unstarted(&exec());
    state.state = ExecState::Waiting;
    let result = step(&state, &Command::Cancel, &Inputs::default());
    assert_eq!(result.state.state, ExecState::Cancelled);
    assert_eq!(result.effects.len(), 1, "the cancellation is persisted");
}

#[test]
fn duplicate_and_terminal_cancel_are_noops() {
    let (_dir, ledger) = harness::open_isolated_ledger("cancel-duplicate");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let execution = exec();
    engine
        .apply(&execution, &start_command(), &Inputs::default())
        .unwrap();

    let first = engine.cancel(&execution, 1).unwrap();
    assert_eq!(first.state.state, ExecState::Cancelled);
    let revision = engine.load(&execution).unwrap().revision;

    // A duplicate cancel of the (now terminal) execution is a no-op.
    let second = engine.cancel(&execution, 2).unwrap();
    assert_eq!(second.state.state, ExecState::Cancelled);
    assert_eq!(engine.load(&execution).unwrap().revision, revision);

    // Cancelling already-terminal work produces no effects at all.
    let mut completed = KernelState::unstarted(&execution);
    completed.state = ExecState::Completed;
    let result = step(&completed, &Command::Cancel, &Inputs::default());
    assert!(result.effects.is_empty());
    assert_eq!(result.state, completed);
}

// ---------------------------------------------------------------------------
// Ownership / fencing
// ---------------------------------------------------------------------------

#[test]
fn stale_lease_result_is_rejected_and_recorded() {
    let (_dir, ledger) = harness::open_isolated_ledger("stale-lease");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let task = task();

    let lease1 = engine.begin_lease(&task).unwrap();
    engine
        .apply(
            &exec,
            &start_command(),
            &Inputs {
                lease_generation: lease1,
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .unwrap();
    let build = OperationId::derive(&exec, "build");
    engine
        .apply(
            &exec,
            &deliver(&build, "h1"),
            &Inputs {
                probe: Probe::Absent,
                lease_generation: lease1,
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .unwrap();
    let before = engine.load(&exec).unwrap();

    // Reclaim (no workers yet): a fresh lease supersedes the worker's lease.
    let reclaim = engine.reclaim(&task, &exec, 3).unwrap();
    assert!(reclaim.is_reclaimed());
    assert_eq!((reclaim.previous_lease, reclaim.new_lease), (1, 2));

    // A result from the superseded lease generation advances nothing.
    let stale = engine
        .apply(
            &exec,
            &deliver(&build, "h1"),
            &Inputs {
                probe: Probe::Present,
                outcome_ref: Some(ArtifactRef::new("artifact:stale")),
                lease_generation: lease1,
                now_unix_ms: 4,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert!(stale.stale_lease.is_some());
    assert_eq!(engine.load(&exec).unwrap(), before, "state is unchanged");
    assert!(
        engine.receipt(&build).unwrap().is_none(),
        "a stale result never records an effect"
    );
    let log = engine.lease_fence_log(&task).unwrap();
    assert_eq!(log.len(), 1, "the rejection is recorded");
    assert_eq!((log[0].lease, log[0].current), (1, 2));

    // A result under the current lease is applied normally.
    let fresh = engine
        .apply(
            &exec,
            &deliver(&build, "h1"),
            &Inputs {
                probe: Probe::Present,
                outcome_ref: Some(ArtifactRef::new("artifact:fresh")),
                lease_generation: 2,
                now_unix_ms: 5,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert!(fresh.stale_lease.is_none());
    assert!(engine.receipt(&build).unwrap().is_some());
    assert_eq!(engine.lease_fence_log(&task).unwrap().len(), 1);
}

#[test]
fn reclaim_terminates_a_live_survivor_with_evidence() {
    let (dir, ledger) = harness::open_isolated_ledger("reclaim-evidence");
    let mut engine = KernelEngine::open(ledger)
        .unwrap()
        .with_launcher(Box::new(FakeLauncher::new(&dir)));
    let exec = exec();
    let task = task();

    let lease1 = engine.begin_lease(&task).unwrap();
    engine
        .apply(
            &exec,
            &start_command(),
            &Inputs {
                lease_generation: lease1,
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .unwrap();
    let build = OperationId::derive(&exec, "build");
    engine
        .apply(
            &exec,
            &deliver(&build, "h1"),
            &Inputs {
                probe: Probe::Absent,
                lease_generation: lease1,
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .unwrap();
    let child = engine
        .launcher_mut()
        .unwrap()
        .launch(fake::spec(&dir))
        .unwrap();
    engine
        .record_worker(&WorkerRecord {
            execution_id: exec.clone(),
            operation_id: build.clone(),
            phase: ExecutionPhase::Build,
            attempt: 0,
            lease_generation: lease1,
            pid: child.pid,
            pgid: child.pgid,
            started_at_unix_ms: child.started_at_unix_ms,
        })
        .unwrap();
    assert!(FakeLauncher::new(&dir).pid_live(child.pid));

    let reclaim = engine.reclaim(&task, &exec, 5).unwrap();
    assert!(reclaim.is_reclaimed());
    assert_eq!((reclaim.previous_lease, reclaim.new_lease), (1, 2));
    assert_eq!(reclaim.terminations.len(), 1);
    assert!(reclaim.terminations[0].evidence.process_group_gone);
    assert_eq!(reclaim.settled_intents, vec![build.clone()]);
    assert!(engine.pending_dispatch().unwrap().is_empty());

    let evidence = engine.termination_evidence(&exec).unwrap();
    assert_eq!(evidence.len(), 1);
    assert!(evidence[0].evidence.process_group_gone);
    assert!(engine.workers(&exec).unwrap().is_empty());
    assert!(!FakeLauncher::new(&dir).pid_live(child.pid));
    assert!(
        FakeLauncher::new(&dir)
            .stop_evidence(child.pid)
            .expect("recorded stop evidence")
            .process_group_gone
    );
}

#[test]
fn reclaim_without_launcher_parks_instead_of_guessing() {
    let (_dir, ledger) = harness::open_isolated_ledger("reclaim-no-launcher");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let exec = exec();
    let task = task();

    let lease1 = engine.begin_lease(&task).unwrap();
    engine
        .apply(
            &exec,
            &start_command(),
            &Inputs {
                lease_generation: lease1,
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .unwrap();
    let build = OperationId::derive(&exec, "build");
    engine
        .apply(
            &exec,
            &deliver(&build, "h1"),
            &Inputs {
                probe: Probe::Absent,
                lease_generation: lease1,
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .unwrap();
    engine
        .record_worker(&WorkerRecord {
            execution_id: exec.clone(),
            operation_id: build,
            phase: ExecutionPhase::Build,
            attempt: 0,
            lease_generation: lease1,
            pid: 4242,
            pgid: 4242,
            started_at_unix_ms: 1,
        })
        .unwrap();

    // No launcher: termination cannot be confirmed, so reclaim blocks and parks.
    let reclaim = engine.reclaim(&task, &exec, 9).unwrap();
    assert!(!reclaim.is_reclaimed());
    assert_eq!(reclaim.new_lease, lease1, "no fresh lease is issued");
    assert!(reclaim.blocked_reason.is_some());
    assert_eq!(engine.load(&exec).unwrap().state, ExecState::Parked);
}

// ---------------------------------------------------------------------------
// Kill latch blocks recovered dispatch
// ---------------------------------------------------------------------------

#[test]
fn kill_latch_blocks_recovered_dispatch() {
    let (dir, ledger) = harness::open_isolated_ledger("kill-latch-recovery");
    let mut engine = KernelEngine::open(ledger).unwrap();
    let journal = EffectJournal::in_run_dir(&dir, "kill-latch-recovery");
    let def = ActivityDef::new("fake/build", "v1");
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    let exec = exec();

    engine
        .apply(&exec, &start_command(), &Inputs::default())
        .unwrap();
    let build = OperationId::derive(&exec, "build");
    let op_inputs = json!({ "task": "fix-answer" });
    let hash = operation_input_hash(&def, &op_inputs);
    engine
        .apply(
            &exec,
            &deliver(&build, &hash),
            &Inputs {
                probe: Probe::Absent,
                activity: Some(def.clone()),
                operation_inputs: Some(op_inputs),
                phase: Some(ExecutionPhase::Build),
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert_eq!(engine.pending_dispatch().unwrap().len(), 1);

    // Latch, then recover: the pending effect must not be dispatched.
    engine
        .apply(
            &exec,
            &Command::KillLatch { latched: true },
            &Inputs::default(),
        )
        .unwrap();
    let results = recover_pending(&mut engine, &mut registry, &exec, 2).unwrap();
    assert!(results.is_empty(), "latched kernel recovers nothing");
    assert_eq!(journal.effect_count(), 0, "no recovered effect runs");

    // Clearing the latch lets the same recovered work proceed exactly once.
    engine
        .apply(
            &exec,
            &Command::KillLatch { latched: false },
            &Inputs::default(),
        )
        .unwrap();
    let results = recover_pending(&mut engine, &mut registry, &exec, 3).unwrap();
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0], OperationResult::Completed { .. }));
    assert_eq!(journal.effect_count(), 1);
}

// ---------------------------------------------------------------------------
// Case 5 — stale-lease worker survives the conductor and cannot advance
// ---------------------------------------------------------------------------

#[test]
fn stale_lease_worker_cannot_advance_after_conductor_failure() {
    if harness::child_case().is_some() {
        run_stale_worker_child();
        return;
    }

    let dir = harness::recovery_dir(CASE_5);
    let status = spawn_child(
        "stale_lease_worker_cannot_advance_after_conductor_failure",
        CASE_5,
        &dir,
    );
    harness::assert_sigabrt(&status);

    let exec = exec();
    let task = task();
    let build = OperationId::derive(&exec, "build");

    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger)
        .unwrap()
        .with_launcher(Box::new(FakeLauncher::new(&dir)));

    // The child's worker survived the crash: a live, stale-lease record.
    let workers = engine.workers(&exec).unwrap();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].lease_generation, 1);
    let pid = workers[0].pid;
    assert!(
        FakeLauncher::new(&dir).pid_live(pid),
        "the survivor is still live before recovery"
    );
    assert_eq!(engine.pending_dispatch().unwrap().len(), 1);

    // Recovery confirms termination of the prior process and records evidence.
    let reclaim = engine.reclaim(&task, &exec, 10).unwrap();
    assert!(reclaim.is_reclaimed());
    assert_eq!((reclaim.previous_lease, reclaim.new_lease), (1, 2));
    assert_eq!(reclaim.terminations.len(), 1);
    assert!(reclaim.terminations[0].evidence.process_group_gone);
    assert!(!FakeLauncher::new(&dir).pid_live(pid), "survivor terminated");
    assert!(
        FakeLauncher::new(&dir)
            .stop_evidence(pid)
            .expect("recorded termination evidence")
            .process_group_gone
    );
    assert_eq!(engine.termination_evidence(&exec).unwrap().len(), 1);
    assert!(
        engine.pending_dispatch().unwrap().is_empty(),
        "the stale-lease intent is settled"
    );

    // A report from the stale lease generation cannot advance the task.
    let before = engine.load(&exec).unwrap();
    let stale = engine
        .apply(
            &exec,
            &deliver(&build, "h1"),
            &Inputs {
                probe: Probe::Present,
                outcome_ref: Some(ArtifactRef::new("artifact:stale-worker")),
                lease_generation: 1,
                now_unix_ms: 11,
                ..Inputs::default()
            },
        )
        .unwrap();
    assert!(stale.stale_lease.is_some());
    assert_eq!(engine.load(&exec).unwrap(), before);
    assert!(engine.receipt(&build).unwrap().is_none());
    assert_eq!(engine.lease_fence_log(&task).unwrap().len(), 1);

    // Only after reconciliation does a fresh claim dispatch the work, once.
    let fresh = OperationId::derive(&exec, "build@1");
    let journal = EffectJournal::in_run_dir(&dir, CASE_5);
    let def = ActivityDef::new("fake/build", "v1");
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            def.clone(),
            RecordingActivity::new(journal.clone()),
        )))
        .unwrap();
    let request = OperationRequest {
        activity: def,
        step: "build@1".to_string(),
        phase: ExecutionPhase::Build,
        component_api: "harness".to_string(),
        inputs: json!({ "task": "fix-answer" }),
        now_unix_ms: 13,
    };
    let result = run_activity(&mut engine, &mut registry, &exec, &request).unwrap();
    assert!(matches!(result, OperationResult::Completed { .. }));
    assert_eq!(
        journal.effect_count(),
        1,
        "the fresh claim runs exactly once"
    );
    assert!(engine.receipt(&fresh).unwrap().is_some());
    assert!(engine.pending_dispatch().unwrap().is_empty());
}

/// Child role: start under lease G1, dispatch, launch + record the worker, then
/// abort after dispatch (before completion).
fn run_stale_worker_child() {
    let dir = std::env::var(DIR_ENV).expect("child run dir");
    let ledger = SqliteLedger::open(Path::new(&dir).join("ledger.sqlite")).unwrap();
    let mut engine = KernelEngine::open(ledger)
        .unwrap()
        .with_launcher(Box::new(FakeLauncher::new(&dir)));
    let exec = exec();
    let task = task();

    harness::disarm();
    let lease = engine.begin_lease(&task).unwrap();
    engine
        .apply(
            &exec,
            &start_command(),
            &Inputs {
                lease_generation: lease,
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .unwrap();
    let build = OperationId::derive(&exec, "build");
    engine
        .apply(
            &exec,
            &deliver(&build, "h1"),
            &Inputs {
                probe: Probe::Absent,
                lease_generation: lease,
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .unwrap();

    // Adapter hand-off: the confined worker is launched under G1 and recorded.
    let child = engine
        .launcher_mut()
        .unwrap()
        .launch(fake::spec(Path::new(&dir)))
        .unwrap();
    engine
        .record_worker(&WorkerRecord {
            execution_id: exec.clone(),
            operation_id: build,
            phase: ExecutionPhase::Build,
            attempt: 0,
            lease_generation: lease,
            pid: child.pid,
            pgid: child.pgid,
            started_at_unix_ms: child.started_at_unix_ms,
        })
        .unwrap();

    // Crash after dispatch, before completion.
    harness::arm(harness::AFTER_DISPATCH);
    harness::crash_hook(harness::AFTER_DISPATCH);
    harness::disarm();
    panic!("crash child should have aborted after dispatch");
}
