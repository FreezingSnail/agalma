//! S0c crash-matrix ported to the M0.3 executor (native tests).
//!
//! Run dirs live under `target/test-runs/execution-*` (never a system temp
//! directory). Crash cases re-exec this test binary as a child with
//! `EXEC_CHILD=<case>`; the child arms `AGALMA_EXEC_CRASH_AT` at the intended
//! injection point and aborts (SIGABRT), after which the parent recovers from
//! the same SQLite database.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::rc::Rc;

use agalma_contracts::{
    ExecutionApi, ExecutionId, ExecutionPhase, ExecutionState, LedgerApi, OperationId, StepOutcome,
    TaskId,
};
use agalma_execution::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, Executor, ExecutorConfig,
    ScriptedActivity, EXECUTION_DEFINITION_VERSION,
};
use agalma_ledger::SqliteLedger;
use serde_json::json;

type SharedActivity = Rc<RefCell<ScriptedActivity>>;

const TASK: &str = "fix-answer";
const PHASES: [ExecutionPhase; 5] = [
    ExecutionPhase::Intake,
    ExecutionPhase::Checkout,
    ExecutionPhase::Build,
    ExecutionPhase::Verify,
    ExecutionPhase::Integrate,
];

fn task() -> TaskId {
    TaskId::new(TASK)
}

fn run_dir(case: &str) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target.join("test-runs").join(format!("execution-{case}"))
}

/// Reset a test run directory.
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

fn effects_path(dir: &Path) -> PathBuf {
    dir.join("effects.log")
}

/// Adapter that lets one scripted activity serve every phase.
struct Shared(Rc<RefCell<ScriptedActivity>>);

impl Activity for Shared {
    fn execute(
        &mut self,
        ctx: &agalma_execution::OperationContext,
    ) -> Result<ActivityOutcome, ActivityError> {
        self.0.borrow_mut().execute(ctx)
    }
    fn effect_present(&mut self, ctx: &agalma_execution::OperationContext) -> EffectProbe {
        self.0.borrow_mut().effect_present(ctx)
    }
    fn reconcile(
        &mut self,
        ctx: &agalma_execution::OperationContext,
    ) -> Result<ActivityOutcome, ActivityError> {
        self.0.borrow_mut().reconcile(ctx)
    }
}

fn setup(dir: &Path, max_attempts: u32, version: u32) -> (Executor<SqliteLedger>, SharedActivity) {
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
    let mut executor = Executor::new(
        ledger,
        ExecutorConfig {
            max_attempts,
            execution_definition_version: version,
        },
    );
    let scripted: SharedActivity = Rc::new(RefCell::new(ScriptedActivity::new(effects_path(dir))));
    for phase in PHASES {
        executor.add_activity(phase, Box::new(Shared(scripted.clone())));
    }
    (executor, scripted)
}

fn effect_count(scripted: &SharedActivity) -> usize {
    scripted.borrow().effect_count()
}

fn outcome(next: ActivityNext) -> ActivityOutcome {
    ActivityOutcome {
        result: json!({ "status": "scripted" }),
        next,
    }
}

fn step_dispatched(outcome: &StepOutcome) -> bool {
    matches!(outcome, StepOutcome::Dispatched { .. })
}

fn spawn_child(test_name: &str, case: &str) -> ExitStatus {
    let exe = std::env::current_exe().expect("current_exe");
    Command::new(exe)
        .args([test_name, "--exact", "--test-threads=1", "--nocapture"])
        .env("EXEC_CHILD", case)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("spawn crash child")
}

fn assert_sigabrt(status: &ExitStatus) {
    assert!(!status.success(), "child should abort, got {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "expected SIGABRT, got {status:?}");
    }
}

/// Arm the crash hook at `point` (only the next matching hook aborts).
fn arm(point: &str) {
    std::env::set_var("AGALMA_EXEC_CRASH_AT", point);
}

// ---------------------------------------------------------------------------
// Case 1 — crash after the transition commit, before dispatch.
// ---------------------------------------------------------------------------

#[test]
fn crash_after_transition_commit_before_dispatch() {
    if std::env::var("EXEC_CHILD").as_deref() == Ok("case1") {
        child_case1();
    }

    let dir = run_dir("case1");
    reset_dir(&dir);
    let status = spawn_child("crash_after_transition_commit_before_dispatch", "case1");
    assert_sigabrt(&status);

    let (mut executor, scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = ExecutionId::derive(&task(), 1);
    executor.recover().expect("recover");

    let (phase, state, attempt) = executor.reconstruct(&exec).unwrap();
    assert_eq!(
        (phase, state, attempt),
        (ExecutionPhase::Intake, ExecutionState::Running, 1)
    );
    assert_eq!(
        executor.ledger().execution(&exec).unwrap().unwrap().phase,
        ExecutionPhase::Intake
    );

    let pending = executor.ledger().pending_intents().unwrap();
    assert_eq!(pending.len(), 1, "one intent recovered: {pending:?}");
    assert_eq!(effect_count(&scripted), 0, "no effect before dispatch");

    let first = executor.step(&exec).unwrap();
    assert!(step_dispatched(&first), "{first:?}");
    assert_eq!(effect_count(&scripted), 1);

    let op = pending[0].operation_id.clone();
    let replay = executor.deliver_operation(&op).unwrap();
    assert!(
        matches!(replay, agalma_execution::DispatchOutcome::Recorded { .. }),
        "duplicate must return recorded result: {replay:?}"
    );
    assert_eq!(effect_count(&scripted), 1, "duplicate repeats no effect");
}

fn child_case1() -> ! {
    let dir = run_dir("case1");
    let (mut executor, _scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    // Arm before the very first transition (start) so the crash lands between
    // the atomic commit and dispatch.
    arm("after_transition_commit");
    let _ = executor.start(&task()).unwrap();
    std::process::abort();
}

// ---------------------------------------------------------------------------
// Case 2 — duplicate delivery after completion returns the recorded result.
// ---------------------------------------------------------------------------

#[test]
fn duplicate_delivery_returns_recorded_receipt() {
    let dir = run_dir("duplicate");
    reset_dir(&dir);
    let (mut executor, scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = executor.start(&task()).unwrap();
    let op = OperationId::derive(&exec, "intake");

    let first = executor.deliver_operation(&op).unwrap();
    assert!(
        matches!(first, agalma_execution::DispatchOutcome::Executed { .. }),
        "{first:?}"
    );
    assert_eq!(effect_count(&scripted), 1);

    let second = executor.deliver_operation(&op).unwrap();
    assert!(
        matches!(second, agalma_execution::DispatchOutcome::Recorded { .. }),
        "{second:?}"
    );
    assert_eq!(effect_count(&scripted), 1, "no second effect");
}

// ---------------------------------------------------------------------------
// Case 3 — restart reconstructs from history; pending reconciled.
// ---------------------------------------------------------------------------

#[test]
fn restart_reconstruction_matches_projection() {
    if std::env::var("EXEC_CHILD").as_deref() == Ok("case3") {
        child_case3();
    }

    let dir = run_dir("case3");
    reset_dir(&dir);
    let status = spawn_child("restart_reconstruction_matches_projection", "case3");
    assert_sigabrt(&status);

    let (mut executor, scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = ExecutionId::derive(&task(), 1);
    executor.recover().expect("recover");

    let (phase, state, attempt) = executor.reconstruct(&exec).unwrap();
    assert_eq!(
        (phase, state, attempt),
        (ExecutionPhase::Verify, ExecutionState::Running, 1)
    );
    let projected = executor.ledger().execution(&exec).unwrap().unwrap();
    assert_eq!(
        (projected.phase, projected.state, projected.attempt),
        (ExecutionPhase::Verify, ExecutionState::Running, 1)
    );

    assert_eq!(effect_count(&scripted), 3, "intake+checkout+build effects");

    let pending = executor.ledger().pending_intents().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].operation_id,
        OperationId::derive(&exec, "verify@1")
    );

    let step = executor.step(&exec).unwrap();
    assert!(step_dispatched(&step), "{step:?}");
    assert_eq!(effect_count(&scripted), 4);
    // Verify completed; the next pending operation is now integrate.
    let pending = executor.ledger().pending_intents().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].operation_id,
        OperationId::derive(&exec, "integrate")
    );
}

fn child_case3() -> ! {
    let dir = run_dir("case3");
    let (mut executor, _scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = executor.start(&task()).unwrap();
    executor.step(&exec).unwrap(); // intake -> checkout
    executor.step(&exec).unwrap(); // checkout -> build
    arm("after_transition_commit");
    let _ = executor.step(&exec).unwrap(); // build -> verify (aborts after commit)
    std::process::abort();
}

// ---------------------------------------------------------------------------
// Case 4 — crash after the external effect, before the completion record.
// ---------------------------------------------------------------------------

#[test]
fn effect_before_completion_reconciled() {
    if std::env::var("EXEC_CHILD").as_deref() == Ok("case4") {
        child_case4();
    }

    let dir = run_dir("case4");
    reset_dir(&dir);
    let status = spawn_child("effect_before_completion_reconciled", "case4");
    assert_sigabrt(&status);

    let (mut executor, scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = ExecutionId::derive(&task(), 1);
    let op = OperationId::derive(&exec, "intake");

    assert_eq!(effect_count(&scripted), 1, "effect survived the crash");
    assert!(executor.ledger().operation_receipt(&op).unwrap().is_none());

    executor.recover().expect("recover");

    let receipt = executor.ledger().operation_receipt(&op).unwrap();
    assert!(receipt.is_some(), "effect reconciled into a receipt");
    assert_eq!(effect_count(&scripted), 1, "effect not repeated");
    assert_eq!(
        executor.ledger().execution(&exec).unwrap().unwrap().phase,
        ExecutionPhase::Checkout,
        "recovery advanced into checkout"
    );
}

fn child_case4() -> ! {
    let dir = run_dir("case4");
    let (mut executor, _scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = executor.start(&task()).unwrap();
    arm("after_effect_before_completion");
    let _ = executor.step(&exec).unwrap(); // executes intake effect then aborts
    std::process::abort();
}

// ---------------------------------------------------------------------------
// Case 5 — kill latch survives restart and blocks recovered dispatch.
// ---------------------------------------------------------------------------

#[test]
fn kill_latch_survives_restart() {
    if std::env::var("EXEC_CHILD").as_deref() == Ok("case5") {
        child_case5();
    }

    let dir = run_dir("case5");
    reset_dir(&dir);
    let status = spawn_child("kill_latch_survives_restart", "case5");
    assert_sigabrt(&status);

    let (mut executor, scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = ExecutionId::derive(&task(), 1);
    executor.recover().expect("recover");
    assert!(executor.kill_latch().unwrap(), "latch survives restart");

    let blocked = executor.step(&exec).unwrap();
    assert!(
        matches!(blocked, StepOutcome::Blocked { .. }),
        "{blocked:?}"
    );
    assert_eq!(effect_count(&scripted), 0, "no effect while latched");

    executor.set_kill_latch(false).unwrap();
    let resumed = executor.step(&exec).unwrap();
    assert!(step_dispatched(&resumed), "{resumed:?}");
    assert_eq!(effect_count(&scripted), 1);
}

fn child_case5() -> ! {
    let dir = run_dir("case5");
    let (mut executor, _scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let _exec = executor.start(&task()).unwrap();
    executor.set_kill_latch(true).unwrap();
    std::process::abort();
}

// ---------------------------------------------------------------------------
// Case 6a — incompatible execution-definition version parks.
// ---------------------------------------------------------------------------

#[test]
fn incompatible_execution_version_parks() {
    let dir = run_dir("version");
    reset_dir(&dir);

    // Write state under version 1, completing intake and leaving checkout pending.
    {
        let (mut executor, _scripted) = setup(&dir, 2, 1);
        let exec = executor.start(&task()).unwrap();
        executor.step(&exec).unwrap(); // intake -> checkout
    }

    // Reopen under version 2: dispatch must park, with no further effect.
    let (mut executor, scripted) = setup(&dir, 2, 2);
    let exec = ExecutionId::derive(&task(), 1);
    executor.recover().expect("recover");
    let before = effect_count(&scripted);

    let parked = executor.step(&exec).unwrap();
    assert!(matches!(parked, StepOutcome::Parked { .. }), "{parked:?}");
    assert_eq!(
        effect_count(&scripted),
        before,
        "no effect under version mismatch"
    );
    let status = executor.status(&exec).unwrap();
    assert_eq!(status.state, ExecutionState::Parked);
    assert!(status.parked_reason.unwrap().contains("definition version"));
}

// ---------------------------------------------------------------------------
// Case 6b — incompatible schema version parks without effect.
// ---------------------------------------------------------------------------

#[test]
fn incompatible_schema_version_parks() {
    let dir = run_dir("schema");
    reset_dir(&dir);
    let db = dir.join("ledger.sqlite");

    {
        let (mut executor, _scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
        let exec = executor.start(&task()).unwrap();
        executor.step(&exec).unwrap(); // intake -> checkout
    }

    // Simulate an incompatible physical schema recorded by a future version.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("UPDATE meta SET value='99' WHERE key='schema_version'", [])
            .unwrap();
    }

    let (mut executor, scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = ExecutionId::derive(&task(), 1);
    executor
        .recover()
        .expect("recover is a no-op under schema mismatch");
    let before = effect_count(&scripted);

    let parked = executor.step(&exec).unwrap();
    assert!(matches!(parked, StepOutcome::Parked { .. }), "{parked:?}");
    assert_eq!(
        effect_count(&scripted),
        before,
        "no effect under schema mismatch"
    );

    let status = executor.status(&exec).unwrap();
    assert_eq!(status.state, ExecutionState::Parked);
    assert!(status.parked_reason.unwrap().contains("schema version"));
}

// ---------------------------------------------------------------------------
// Case 7 — bounded retry, and retry exhaustion parks.
// ---------------------------------------------------------------------------

#[test]
fn verify_red_bounded_retry_then_done() {
    let dir = run_dir("retry");
    reset_dir(&dir);
    let (mut executor, scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    scripted.borrow_mut().script(
        ExecutionPhase::Verify,
        1,
        outcome(ActivityNext::Retry {
            failure: agalma_contracts::ArtifactRef::derive("verify.log"),
        }),
    );

    let exec = executor.start(&task()).unwrap();
    // intake, checkout, build@1, verify@1 -> retry, build@2, verify@2, integrate
    for _ in 0..7 {
        let _ = executor.step(&exec).unwrap();
    }
    // The retry is an explicit new operation (build@2), never an implicit retry.
    assert_eq!(
        executor.ledger().execution(&exec).unwrap().unwrap().attempt,
        2
    );
    assert!(executor
        .ledger()
        .operation_receipt(&OperationId::derive(&exec, "build@2"))
        .unwrap()
        .is_some());

    let status = executor.status(&exec).unwrap();
    assert_eq!(
        (status.phase, status.state),
        (ExecutionPhase::Done, ExecutionState::Completed)
    );
    assert_eq!(effect_count(&scripted), 7);
    // Terminal: a further step is idle.
    let idle = executor.step(&exec).unwrap();
    assert!(matches!(idle, StepOutcome::Idle { .. }), "{idle:?}");
}

#[test]
fn verify_red_exhausts_attempts_parks() {
    let dir = run_dir("retry-park");
    reset_dir(&dir);
    let (mut executor, scripted) = setup(&dir, 1, EXECUTION_DEFINITION_VERSION);
    scripted.borrow_mut().script(
        ExecutionPhase::Verify,
        1,
        outcome(ActivityNext::Retry {
            failure: agalma_contracts::ArtifactRef::derive("verify.log"),
        }),
    );

    let exec = executor.start(&task()).unwrap();
    // intake, checkout, build, verify(1) -> retry exhausted -> parked
    let mut last = None;
    for _ in 0..4 {
        last = Some(executor.step(&exec).unwrap());
    }
    assert!(matches!(last, Some(StepOutcome::Parked { .. })), "{last:?}");
    let status = executor.status(&exec).unwrap();
    assert_eq!(status.state, ExecutionState::Parked);
    assert!(status.parked_reason.unwrap().contains("max_attempts"));
}

// ---------------------------------------------------------------------------
// Ambiguous effect parks.
// ---------------------------------------------------------------------------

#[test]
fn ambiguous_effect_parks() {
    let dir = run_dir("ambiguous");
    reset_dir(&dir);
    let (mut executor, scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    scripted
        .borrow_mut()
        .probe(ExecutionPhase::Intake, 1, EffectProbe::Ambiguous);

    let exec = executor.start(&task()).unwrap();
    let parked = executor.step(&exec).unwrap();
    assert!(matches!(parked, StepOutcome::Parked { .. }), "{parked:?}");
    assert_eq!(
        effect_count(&scripted),
        0,
        "ambiguous effect is never executed"
    );
    let status = executor.status(&exec).unwrap();
    assert_eq!(status.state, ExecutionState::Parked);
    assert!(status.parked_reason.unwrap().contains("ambiguous"));
}

// ---------------------------------------------------------------------------
// Status projection reports pending ops.
// ---------------------------------------------------------------------------

#[test]
fn status_reports_pending_operations() {
    let dir = run_dir("status");
    reset_dir(&dir);
    let (mut executor, _scripted) = setup(&dir, 2, EXECUTION_DEFINITION_VERSION);
    let exec = executor.start(&task()).unwrap();
    let status = executor.status(&exec).unwrap();
    assert_eq!(status.phase, ExecutionPhase::Intake);
    assert_eq!(status.state, ExecutionState::Running);
    assert_eq!(
        status.pending_operations,
        vec![OperationId::derive(&exec, "intake")]
    );
    assert!(status.parked_reason.is_none());
}
