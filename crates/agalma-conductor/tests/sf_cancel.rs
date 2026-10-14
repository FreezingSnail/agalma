//! SF.4 acceptance: durable per-execution cancellation that survives resume.
//!
//! Cancellation is per-execution and monotonic (`none -> requested ->
//! settling -> settled`), **distinct** from the global pause latch. Resume
//! clears only the global latch; it never removes cancellation. A settled
//! cancellation projects to `ExecutionState::Failed` / `ExecutionPhase::Parked`
//! with reason `Cancelled`, matching the kernel-native `ExecState::Cancelled`.
//!
//! Evidence is layered:
//!
//! - executor-seam cases drive a real `SqliteLedger` with task-scoped fake
//!   activities that panic on cross-task access (reused from
//!   `sf_recovery_gate.rs`), covering: request/duplicate/settle with no new
//!   effect, an unconfirmed (ambiguous / missing-activity) probe that stays
//!   pending and never redispatches, crash-before-settlement recovery, and a
//!   no-op cancel of already-completed work;
//! - kernel-adapter cases drive a real `KernelEngine` with a fake `SandboxApi`
//!   that reports the process group gone (settled) versus no sandbox
//!   (settling), asserting the idempotent cancel, the `Cancelled` projection,
//!   and that a cancelled pending operation cannot dispatch;
//! - a production case drives the real `agalma work --once` + `agalma cancel`
//!   CLI over a scratch git + `bd` store in scripted mode: pause, cancel an
//!   in-flight task, resume, run work, and confirm the cancelled task parks as
//!   `Cancelled` with no effect/merge while a second authorized task completes.
//!
//! Scratch state lives under `target/test-runs/`; no `/tmp`, no network, no
//! paid inference.

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;

use agalma_contracts::{
    CancellationState, ContractError, ExecutionApi, ExecutionId, ExecutionPhase, ExecutionState,
    LaunchSpec, LedgerApi, OperationId, SandboxApi, SandboxChild, StepOutcome, StopEvidence, TaskId,
};
use agalma_execution::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, DispatchOutcome, EffectProbe, Executor,
    ExecutorConfig, OperationContext, EXECUTION_DEFINITION_VERSION,
};
use agalma_kernel::{
    Command as KernelCommand, ExecState, Inputs, KernelEngine, Probe, WorkerRecord,
};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;
use serde_json::{json, Value};

const PHASES: [ExecutionPhase; 5] = [
    ExecutionPhase::Intake,
    ExecutionPhase::Checkout,
    ExecutionPhase::Build,
    ExecutionPhase::Verify,
    ExecutionPhase::Integrate,
];

fn advance_outcome() -> ActivityOutcome {
    ActivityOutcome {
        result: json!({ "status": "ok" }),
        next: ActivityNext::Advance,
    }
}

fn open(db: &Path) -> SqliteLedger {
    SqliteLedger::open(db).expect("open ledger")
}

fn executor_config() -> ExecutorConfig {
    ExecutorConfig {
        max_attempts: 2,
        execution_definition_version: EXECUTION_DEFINITION_VERSION,
    }
}

// ===========================================================================
// Executor-seam harness (task-scoped fake activities)
// ===========================================================================

/// How the activity answers a survivor probe.
#[derive(Clone, Copy, Debug)]
enum ProbeMode {
    /// Read the task's own effect log: a recorded effect is present.
    File,
    /// Force a result (models a confirmed or ambiguous survivor).
    Force(EffectProbe),
}

#[derive(Default, Debug)]
struct Observed {
    probes: Vec<(TaskId, ExecutionPhase, u32)>,
    executed: Vec<String>,
    reconciled: Vec<String>,
}

/// A task-scoped activity: it appends effects to its task's own workspace and
/// **panics** if the executor probes/executes another task's operation.
struct TaskActivity {
    task: TaskId,
    effects: PathBuf,
    probe: ProbeMode,
    outcome: ActivityOutcome,
    observed: Rc<RefCell<Observed>>,
}

impl TaskActivity {
    fn assert_owned(&self, ctx: &OperationContext, call: &str) {
        assert_eq!(
            ctx.task_id, self.task,
            "cross-task {call}: activity bound to task {} was asked about {} ({:?})",
            self.task, ctx.task_id, ctx.phase
        );
    }
}

impl Activity for TaskActivity {
    fn execute(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        self.assert_owned(ctx, "execute");
        self.observed
            .borrow_mut()
            .executed
            .push(ctx.operation_id.to_string());
        append_effect(&self.effects, ctx.operation_id.as_str())?;
        Ok(self.outcome.clone())
    }

    fn effect_present(&mut self, ctx: &OperationContext) -> EffectProbe {
        self.assert_owned(ctx, "probe");
        let probe = match self.probe {
            ProbeMode::Force(result) => result,
            ProbeMode::File => {
                if effect_recorded(&self.effects, ctx.operation_id.as_str()) {
                    EffectProbe::Present
                } else {
                    EffectProbe::Absent
                }
            }
        };
        self.observed
            .borrow_mut()
            .probes
            .push((ctx.task_id.clone(), ctx.phase, ctx.attempt));
        probe
    }

    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        self.assert_owned(ctx, "reconcile");
        self.observed
            .borrow_mut()
            .reconciled
            .push(ctx.operation_id.to_string());
        Ok(self.outcome.clone())
    }
}

fn build_executor(
    ledger: SqliteLedger,
    task: &TaskId,
    workspace: &Path,
    observed: &Rc<RefCell<Observed>>,
    build_probe: ProbeMode,
) -> Executor<SqliteLedger> {
    let mut executor = Executor::new(ledger, executor_config());
    for phase in PHASES {
        let probe = if phase == ExecutionPhase::Build {
            build_probe
        } else {
            ProbeMode::File
        };
        executor.add_activity(
            phase,
            Box::new(TaskActivity {
                task: task.clone(),
                effects: effects_log(workspace),
                probe,
                outcome: advance_outcome(),
                observed: Rc::clone(observed),
            }),
        );
    }
    executor
}

fn drive_to(executor: &mut Executor<SqliteLedger>, exec: &ExecutionId, target: ExecutionPhase) {
    for _ in 0..16 {
        let status = executor.status(exec).expect("status");
        if status.phase == target {
            return;
        }
        match executor.step(exec).expect("step") {
            StepOutcome::Dispatched { .. } | StepOutcome::Advanced { .. } => {}
            other => panic!("unexpected step outcome before {target:?}: {other:?}"),
        }
    }
    panic!("did not reach {target:?}");
}

fn effects_log(workspace: &Path) -> PathBuf {
    workspace.join("effects.log")
}

fn append_effect(path: &Path, operation_id: &str) -> Result<(), ActivityError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ActivityError::KnownFailure(format!("create effects dir: {e}")))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| ActivityError::KnownFailure(format!("open effects log: {e}")))?;
    writeln!(file, "{operation_id}")
        .map_err(|e| ActivityError::KnownFailure(format!("append effect: {e}")))?;
    file.sync_all()
        .map_err(|e| ActivityError::KnownFailure(format!("sync effect: {e}")))?;
    Ok(())
}

fn effect_recorded(path: &Path, operation_id: &str) -> bool {
    fs::read_to_string(path)
        .map(|text| text.lines().any(|line| line.trim() == operation_id))
        .unwrap_or(false)
}

fn effects(workspace: &Path) -> Vec<String> {
    fs::read_to_string(effects_log(workspace))
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ===========================================================================
// Case 1 — request/duplicate/settle with no new effect; second task runs
// ===========================================================================

#[test]
fn executor_cancel_denies_then_settles_without_effect_and_other_task_runs() {
    let scratch = ScratchDir::new("sf-cancel-exec");
    let state = scratch.child("state");
    let ws_a = scratch.child("ws-a");
    let ws_b = scratch.child("ws-b");
    let db = state.join("ledger.sqlite");
    let task_a = TaskId::new("task-a");
    let task_b = TaskId::new("task-b");

    let obs_a = Rc::new(RefCell::new(Observed::default()));
    let mut ex = build_executor(open(&db), &task_a, &ws_a, &obs_a, ProbeMode::File);
    let exec_a = ex.start(&task_a).expect("start A");
    drive_to(&mut ex, &exec_a, ExecutionPhase::Build);

    let before = effects(&ws_a);
    assert_eq!(before.len(), 2, "intake + checkout effects: {before:?}");
    let build_op = OperationId::derive(&exec_a, "build@1");
    assert_eq!(
        ex.status(&exec_a).expect("status").pending_operations,
        vec![build_op.clone()]
    );

    // Cancel: durable per-execution request; duplicate is idempotent.
    ex.cancel(&exec_a).expect("cancel A");
    assert_eq!(
        ex.cancellation_state(&exec_a).expect("state"),
        CancellationState::Requested
    );
    assert_eq!(cancel_request_count(ex.ledger(), &exec_a), 1);
    ex.cancel(&exec_a).expect("duplicate cancel A");
    assert_eq!(
        cancel_request_count(ex.ledger(), &exec_a),
        1,
        "duplicate cancel records no second request"
    );
    assert!(
        !ex.kill_latch().expect("latch"),
        "per-execution cancel never sets the global pause latch"
    );

    // Requested denies advancement and dispatch, with zero effect.
    match ex.step(&exec_a).expect("step") {
        StepOutcome::Blocked { operation } => assert_eq!(operation, Some(build_op.clone())),
        other => panic!("expected Blocked while requested, got {other:?}"),
    }
    assert_eq!(
        ex.deliver_operation(&build_op).expect("deliver"),
        DispatchOutcome::Blocked,
        "a cancelled pending operation cannot merge/dispatch"
    );
    assert_eq!(effects(&ws_a), before, "no effect while requested");

    // Settle: the absent intent is consumed, the execution parks.
    let settled = ex.settle_cancellation(&exec_a).expect("settle A");
    assert_eq!(settled, CancellationState::Settled);
    let status = ex.status(&exec_a).expect("status A");
    assert_eq!(
        (status.phase, status.state),
        (ExecutionPhase::Parked, ExecutionState::Failed),
        "settled projection is Failed/Parked"
    );
    assert_eq!(status.parked_reason.as_deref(), Some("Cancelled"));
    assert_eq!(status.cancellation, CancellationState::Settled);
    assert!(status.pending_operations.is_empty(), "intent consumed on settle");
    assert_eq!(effects(&ws_a), before, "cancelled operation produced no effect");
    assert!(
        !obs_a.borrow().executed.iter().any(|op| op == build_op.as_str()),
        "the cancelled operation never executed"
    );

    // A second authorized task still runs to completion.
    let obs_b = Rc::new(RefCell::new(Observed::default()));
    let mut ex_b = build_executor(open(&db), &task_b, &ws_b, &obs_b, ProbeMode::File);
    let exec_b = ex_b.start(&task_b).expect("start B");
    drive_to(&mut ex_b, &exec_b, ExecutionPhase::Done);
    let status_b = ex_b.status(&exec_b).expect("status B");
    assert_eq!(status_b.state, ExecutionState::Completed);
    assert_eq!(status_b.cancellation, CancellationState::None);
    assert!(
        effects(&ws_b).len() >= 5,
        "B ran every phase: {:?}",
        effects(&ws_b)
    );
}

// ===========================================================================
// Case 2 — unconfirmed termination stays Settling and never redispatches
// ===========================================================================

#[test]
fn unconfirmed_probe_stays_settling_and_never_redispatches() {
    let scratch = ScratchDir::new("sf-cancel-unconfirmed");
    let state = scratch.child("state");
    let ws = scratch.child("ws-a");
    let db = state.join("ledger.sqlite");
    let task = TaskId::new("task-a");

    let obs = Rc::new(RefCell::new(Observed::default()));
    let mut ex = build_executor(
        open(&db),
        &task,
        &ws,
        &obs,
        ProbeMode::Force(EffectProbe::Ambiguous),
    );
    let exec = ex.start(&task).expect("start");
    drive_to(&mut ex, &exec, ExecutionPhase::Build);
    let before = effects(&ws);
    let build_op = OperationId::derive(&exec, "build@1");

    ex.cancel(&exec).expect("cancel");
    let settled = ex.settle_cancellation(&exec).expect("settle");
    assert_eq!(
        settled,
        CancellationState::Settling,
        "an unconfirmed probe keeps the settlement pending"
    );

    let status = ex.status(&exec).expect("status");
    assert_eq!((status.phase, status.state), (ExecutionPhase::Parked, ExecutionState::Failed));
    assert_eq!(status.parked_reason.as_deref(), Some("Cancelled"));
    assert_eq!(status.cancellation, CancellationState::Settling);
    assert_eq!(
        status.pending_operations,
        vec![build_op.clone()],
        "the unconfirmed intent stays pending (never consumed as success)"
    );
    assert_eq!(effects(&ws), before, "no effect under ambiguity");

    // Cannot redispatch, even after the park.
    match ex.step(&exec).expect("step") {
        StepOutcome::Parked { .. } | StepOutcome::Blocked { .. } => {}
        other => panic!("cancelled execution must not redispatch, got {other:?}"),
    }
    assert_eq!(
        ex.deliver_operation(&build_op).expect("deliver"),
        DispatchOutcome::Blocked
    );
    assert_eq!(effects(&ws), before, "still no effect after refused redispatch");
}

// ===========================================================================
// Case 2b — a missing activity probe is also unconfirmed (conservative)
// ===========================================================================

#[test]
fn missing_activity_probe_is_unconfirmed() {
    let scratch = ScratchDir::new("sf-cancel-missing");
    let state = scratch.child("state");
    let ws = scratch.child("ws-a");
    let db = state.join("ledger.sqlite");
    let task = TaskId::new("task-a");

    {
        let obs = Rc::new(RefCell::new(Observed::default()));
        let mut ex = build_executor(open(&db), &task, &ws, &obs, ProbeMode::File);
        let exec = ex.start(&task).expect("start");
        drive_to(&mut ex, &exec, ExecutionPhase::Build);
        ex.cancel(&exec).expect("cancel");
    }
    let exec = ExecutionId::derive(&task, 1);
    let build_op = OperationId::derive(&exec, "build@1");

    // A bare executor has no activities: probing the pending build errors, which
    // must be treated as an unconfirmed outcome (pending, settling), never a
    // silent success.
    let mut bare = Executor::new(open(&db), executor_config());
    let settled = bare.settle_cancellation(&exec).expect("settle");
    assert_eq!(settled, CancellationState::Settling);
    let status = bare.status(&exec).expect("status");
    assert_eq!(status.cancellation, CancellationState::Settling);
    assert_eq!(
        status.pending_operations,
        vec![build_op],
        "a missing-activity probe leaves the intent pending"
    );
}

// ===========================================================================
// Case 3 — crash before settlement is recovered; completed cancel is a no-op
// ===========================================================================

#[test]
fn crash_before_settlement_recovers_and_completed_cancel_is_noop() {
    // Part A: a requested-but-unsent settlement (simulated crash) is recovered
    // on a later `settle_cancellation` call.
    let scratch = ScratchDir::new("sf-cancel-crash");
    let state = scratch.child("state");
    let ws = scratch.child("ws-a");
    let db = state.join("ledger.sqlite");
    let task = TaskId::new("task-a");

    let exec;
    {
        let obs = Rc::new(RefCell::new(Observed::default()));
        let mut ex = build_executor(open(&db), &task, &ws, &obs, ProbeMode::File);
        exec = ex.start(&task).expect("start");
        drive_to(&mut ex, &exec, ExecutionPhase::Build);
        // The seed `cancel` journals the request only; settlement is separate.
        ex.cancel(&exec).expect("request cancel");
    }

    let obs = Rc::new(RefCell::new(Observed::default()));
    let mut reopened = build_executor(open(&db), &task, &ws, &obs, ProbeMode::File);
    assert_eq!(
        reopened.cancellation_state(&exec).expect("state"),
        CancellationState::Requested,
        "the request survived the reopen"
    );
    assert_eq!(
        reopened.settle_cancellation(&exec).expect("settle"),
        CancellationState::Settled
    );
    let status = reopened.status(&exec).expect("status");
    assert_eq!((status.phase, status.state), (ExecutionPhase::Parked, ExecutionState::Failed));
    assert_eq!(status.cancellation, CancellationState::Settled);

    // Part B: cancelling already-completed work is a no-op that keeps Completed.
    let task_c = TaskId::new("task-c");
    let ws_c = scratch.child("ws-c");
    let obs_c = Rc::new(RefCell::new(Observed::default()));
    let mut ex_c = build_executor(open(&db), &task_c, &ws_c, &obs_c, ProbeMode::File);
    let exec_c = ex_c.start(&task_c).expect("start C");
    drive_to(&mut ex_c, &exec_c, ExecutionPhase::Done);

    ex_c.cancel(&exec_c).expect("noop cancel");
    assert_eq!(
        ex_c.cancellation_state(&exec_c).expect("state"),
        CancellationState::None,
        "a no-op cancel records no cancellation state"
    );
    let status_c = ex_c.status(&exec_c).expect("status C");
    assert_eq!(status_c.state, ExecutionState::Completed, "completed work keeps its outcome");
    assert_eq!(status_c.cancellation, CancellationState::None);
    assert_eq!(
        event_count(ex_c.ledger(), &exec_c, "cancel_noop"),
        1,
        "exactly one no-op cancel receipt"
    );
    ex_c.cancel(&exec_c).expect("duplicate noop cancel");
    assert_eq!(
        event_count(ex_c.ledger(), &exec_c, "cancel_noop"),
        1,
        "duplicate no-op cancel records nothing new"
    );
}

// ===========================================================================
// Case 4 — kernel adapter: idempotent cancel, projection, settled vs settling
// ===========================================================================

/// A fake sandbox whose processes are always already gone.
struct GoneSandbox;

impl SandboxApi for GoneSandbox {
    fn launch(&mut self, _spec: LaunchSpec) -> Result<SandboxChild, ContractError> {
        Err(ContractError::UnsupportedCapability("gone sandbox cannot launch".into()))
    }

    fn alive(&self, _child: &SandboxChild) -> Result<bool, ContractError> {
        Ok(false)
    }

    fn terminate(&mut self, _child: &SandboxChild) -> Result<StopEvidence, ContractError> {
        Ok(StopEvidence {
            process_group_gone: true,
            detail: "already gone".to_string(),
        })
    }
}

fn start_and_deliver(engine: &mut KernelEngine<SqliteLedger>, task: &TaskId, exec: &ExecutionId) {
    let lease = engine.begin_lease(task).expect("lease");
    engine
        .apply(
            exec,
            &KernelCommand::Start {
                task_id: task.clone(),
                generation: 1,
            },
            &Inputs {
                lease_generation: lease,
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .expect("start");
    let build = OperationId::derive(exec, "build");
    engine
        .apply(
            exec,
            &KernelCommand::Deliver {
                operation_id: build,
                inputs_hash: "h1".to_string(),
            },
            &Inputs {
                probe: Probe::Absent,
                lease_generation: lease,
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .expect("deliver");
}

#[test]
fn kernel_cancel_is_idempotent_and_projects_per_settlement() {
    let scratch = ScratchDir::new("sf-cancel-kernel-gone");
    let db = scratch.child("state").join("ledger.sqlite");
    let task = TaskId::new("k-cancel");
    let exec = ExecutionId::derive(&task, 1);

    // (a) Sandbox reports the process group gone -> Settled.
    {
        let ledger = SqliteLedger::open(&db).expect("ledger");
        let mut engine = KernelEngine::open(ledger)
            .expect("kernel")
            .with_sandbox(Box::new(GoneSandbox));
        start_and_deliver(&mut engine, &task, &exec);
        let build = OperationId::derive(&exec, "build");
        assert_eq!(engine.pending_dispatch().expect("pending").len(), 1);
        engine
            .record_worker(&WorkerRecord {
                execution_id: exec.clone(),
                operation_id: build,
                phase: ExecutionPhase::Build,
                attempt: 0,
                lease_generation: 1,
                pid: 4242,
                pgid: 4242,
                started_at_unix_ms: 1,
            })
            .expect("record worker");

        let first = engine.cancel(&exec, 5).expect("cancel");
        assert_eq!(first.state.state, ExecState::Cancelled);
        let settlement = engine
            .cancellation_settlement(&exec)
            .expect("settlement")
            .expect("some settlement");
        assert_eq!(settlement.state, CancellationState::Settled);
        assert!(settlement.unconfirmed_operations.is_empty(), "{settlement:?}");

        // Idempotent duplicate: no new history/effect.
        let revision = engine.load(&exec).expect("load").revision;
        let second = engine.cancel(&exec, 6).expect("duplicate cancel");
        assert_eq!(second.state.state, ExecState::Cancelled);
        assert_eq!(engine.load(&exec).expect("load").revision, revision);

        let status = ExecutionApi::status(&engine, &exec).expect("status");
        assert_eq!(
            (status.phase, status.state),
            (ExecutionPhase::Parked, ExecutionState::Failed),
            "native Cancelled maps to Failed/Parked"
        );
        assert_eq!(status.parked_reason.as_deref(), Some("Cancelled"));
        assert_eq!(status.cancellation, CancellationState::Settled);
        assert!(
            engine.pending_dispatch().expect("pending").is_empty(),
            "a settled cancellation releases the pending intent"
        );
    }

    // (b) No sandbox -> the recorded worker cannot be confirmed gone -> Settling.
    {
        let scratch = ScratchDir::new("sf-cancel-kernel-nosandbox");
        let db = scratch.child("state").join("ledger.sqlite");
        let task = TaskId::new("k-cancel2");
        let exec = ExecutionId::derive(&task, 1);
        let ledger = SqliteLedger::open(&db).expect("ledger");
        let mut engine = KernelEngine::open(ledger).expect("kernel");
        start_and_deliver(&mut engine, &task, &exec);
        let build = OperationId::derive(&exec, "build");
        engine
            .record_worker(&WorkerRecord {
                execution_id: exec.clone(),
                operation_id: build.clone(),
                phase: ExecutionPhase::Build,
                attempt: 0,
                lease_generation: 1,
                pid: 4242,
                pgid: 4242,
                started_at_unix_ms: 1,
            })
            .expect("record worker");

        engine.cancel(&exec, 5).expect("cancel");
        let settlement = engine
            .cancellation_settlement(&exec)
            .expect("settlement")
            .expect("some settlement");
        assert_eq!(
            settlement.state,
            CancellationState::Settling,
            "no sandbox makes termination unconfirmed"
        );
        assert_eq!(settlement.unconfirmed_operations, vec![build]);

        let status = ExecutionApi::status(&engine, &exec).expect("status");
        assert_eq!((status.phase, status.state), (ExecutionPhase::Parked, ExecutionState::Failed));
        assert_eq!(status.cancellation, CancellationState::Settling);
        assert!(engine.pending_dispatch().expect("pending").is_empty());

        // Retrying settlement without a sandbox stays Settling (never a guess).
        let retried = engine
            .settle_cancellation(&exec, 9)
            .expect("retry")
            .expect("some settlement");
        assert_eq!(retried.state, CancellationState::Settling);
    }
}

// ===========================================================================
// Production harness — real `agalma work --once` + `agalma cancel`
// ===========================================================================

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for these tests but was not found");
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_ok(program: &str, dir: &Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(out.status.success(), "{program} {args:?}: {}", stderr_of(&out));
}

fn assert_aborted(status: &std::process::ExitStatus) {
    assert!(!status.success(), "process should abort: {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "expected SIGABRT: {status:?}");
    }
}

struct Prod {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Prod {
    fn new(name: &str) -> Prod {
        require_bd();
        let dir = ScratchDir::new(&format!("sf-cancel-{name}"));
        let origin = dir.child("origin");
        fs::create_dir_all(origin.join("src")).expect("origin src");
        fs::write(
            origin.join("src").join("lib.rs"),
            "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
        )
        .expect("seed source");

        run_ok("git", &origin, &["init", "-q", "-b", "main"]);
        run_ok("git", &origin, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &origin, &["config", "user.name", "Agalma"]);
        run_ok("git", &origin, &["add", "-A"]);
        run_ok("git", &origin, &["commit", "-qm", "seed"]);

        let prefix = format!("sfcancel{}", name.replace('-', ""));
        let out = Command::new("bd")
            .args([
                "init",
                "--prefix",
                &prefix,
                "--non-interactive",
                "--skip-agents",
                "--skip-hooks",
            ])
            .current_dir(&origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd init");
        assert!(out.status.success(), "bd init: {}", stderr_of(&out));

        Prod {
            origin,
            state_dir: dir.child("state"),
            _dir: dir,
        }
    }

    fn bd(&self, args: &[&str]) -> Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.origin.join(".beads"))
            .args(args)
            .current_dir(&self.origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn bd_ok(&self, args: &[&str]) -> String {
        let out = self.bd(args);
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn create_task(&self, title: &str, marker: &str) -> String {
        let description = format!(
            "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\n\
             acceptance:\n  - sh -c 'echo {marker}'\npriority: normal\nfamily: fix\n\
             max_attempts: 2\n```\n"
        );
        let stdout = self.bd_ok(&[
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            &description,
            "--json",
        ]);
        let value: Value = serde_json::from_str(&stdout).expect("bd create json");
        value["id"].as_str().expect("created id").to_string()
    }

    fn run_cli(&self, args: &[&str]) -> Output {
        let mut command = Command::new(bin());
        command.args(args);
        command.args(["--state-dir", self.state_dir.to_str().expect("state utf8")]);
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("AGALMA_LEDGER_TRANSPORT", "in-process");
        command.output().expect("run agalma cli")
    }

    fn pause(&self) -> Output {
        self.run_cli(&["pause"])
    }

    fn resume(&self) -> Output {
        self.run_cli(&["resume"])
    }

    fn cancel(&self, execution: &str) -> Output {
        self.run_cli(&["cancel", "--execution", execution, "--confirm", "cancel"])
    }

    /// Run `agalma work --once` against the scratch origin in scripted mode.
    fn work(&self, extra: &[(&str, &str)]) -> Output {
        let mut command = Command::new(bin());
        command.args([
            "work",
            "--repo",
            self.origin.to_str().expect("origin utf8"),
            "--state-dir",
            self.state_dir.to_str().expect("state utf8"),
            "--once",
        ]);
        command
            .env("AGALMA_ACTIVITY_MODE", "scripted")
            .env("AGALMA_LEDGER_TRANSPORT", "in-process")
            .env("AGALMA_WORKSPACE_TRANSPORT", "in-process")
            .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
            .env("GIT_TERMINAL_PROMPT", "0");
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().expect("run agalma work")
    }

    fn ledger(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger")
    }

    fn latch(&self) -> bool {
        self.ledger()
            .query_row("SELECT active FROM kill_latch WHERE id=1", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("latch row")
            != 0
    }

    fn has_event_kind(&self, execution: &str, kind: &str) -> bool {
        self.ledger()
            .query_row(
                "SELECT COUNT(*) FROM execution_events WHERE execution_id=?1 AND kind=?2",
                rusqlite::params![execution, kind],
                |row| row.get::<_, i64>(0),
            )
            .expect("event count")
            > 0
    }

    fn execution_phase(&self, execution: &str) -> (String, String) {
        self.ledger()
            .query_row(
                "SELECT phase, state FROM executions WHERE execution_id=?1",
                [execution],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("execution row")
    }

    fn pending_intents(&self, execution: &str) -> Vec<String> {
        let conn = self.ledger();
        let mut stmt = conn
            .prepare(
                "SELECT operation_id FROM dispatch_intents WHERE execution_id=?1 AND consumed=0 \
                 ORDER BY operation_id",
            )
            .expect("prepare intents");
        let rows = stmt
            .query_map([execution], |row| row.get::<_, String>(0))
            .expect("query intents");
        rows.map(|row| row.expect("intent row")).collect()
    }

    fn admitted(&self, execution: &str) -> bool {
        self.has_event_kind(execution, "task.package_admitted")
    }

    fn receipt_count(&self, execution: &str) -> usize {
        self.ledger()
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE execution_id=?1",
                [execution],
                |row| row.get::<_, i64>(0),
            )
            .expect("receipt count") as usize
    }

    fn effects(&self, execution: &str) -> Vec<String> {
        fs::read_to_string(agalma_conductor::phases::effects_log_path(
            &self.state_dir,
            &ExecutionId::new(execution),
        ))
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
    }

    fn issue_status(&self, id: &str) -> String {
        let stdout = self.bd_ok(&["show", id, "--json"]);
        let value: Value = serde_json::from_str(&stdout).expect("bd show json");
        value["status"]
            .as_str()
            .or_else(|| {
                value
                    .as_array()
                    .and_then(|items| items.first())
                    .and_then(|item| item["status"].as_str())
            })
            .unwrap_or_default()
            .to_string()
    }
}

// ===========================================================================
// Case 5 — production cancel survives resume; B still completes green
// ===========================================================================

#[test]
fn production_cancel_survives_resume_and_second_task_completes() {
    let prod = Prod::new("resume");

    // A: crash after admission -> a genuinely in-flight, non-terminal execution.
    let a = prod.create_task("Cancel target A", "ACCEPT_A");
    let crash = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&crash.status);
    let exec_a = format!("exec:{a}:1");
    assert!(prod.admitted(&exec_a), "A frozen package admitted before the crash");
    assert_eq!(
        prod.execution_phase(&exec_a),
        ("intake".to_string(), "running".to_string())
    );
    let intake = OperationId::derive(&ExecutionId::new(&exec_a), "intake").to_string();
    assert_eq!(prod.pending_intents(&exec_a), vec![intake]);

    // Pause, cancel the in-flight execution, then resume.
    let pause = prod.pause();
    assert!(pause.status.success(), "pause: {}", stderr_of(&pause));
    assert!(prod.latch(), "pause sets the global latch");

    let cancel = prod.cancel(&exec_a);
    assert!(
        cancel.status.success(),
        "cancel: {}\n{}",
        stdout_of(&cancel),
        stderr_of(&cancel)
    );
    assert!(
        prod.has_event_kind(&exec_a, "cancel_requested"),
        "cancel records a durable per-execution request"
    );
    assert!(prod.latch(), "cancel leaves the pause latch unchanged");

    let resume = prod.resume();
    assert!(resume.status.success(), "resume: {}", stderr_of(&resume));
    assert!(!prod.latch(), "resume clears the latch");

    // A second authorized task becomes available after resume.
    let b = prod.create_task("Authorized B", "ACCEPT_B");

    let recovery = prod.work(&[]);
    assert!(
        recovery.status.success(),
        "work: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    // A is durably settled as Failed/Parked/Cancelled with no effect and no merge.
    assert_eq!(
        prod.execution_phase(&exec_a),
        ("parked".to_string(), "failed".to_string()),
        "cancelled A parks as Failed/Parked"
    );
    assert!(prod.has_event_kind(&exec_a, "cancel_parked"), "durable cancel_parked recorded");
    assert!(prod.has_event_kind(&exec_a, "cancel_settled"), "settlement reached Settled");
    assert!(
        prod.effects(&exec_a).is_empty(),
        "cancelled A produced no effect/merge: {:?}",
        prod.effects(&exec_a)
    );
    assert_eq!(prod.receipt_count(&exec_a), 0, "cancelled A completed no operation");
    assert!(
        prod.pending_intents(&exec_a).is_empty(),
        "cancelled A's intent is consumed"
    );
    assert_eq!(
        prod.issue_status(&a),
        "blocked",
        "cancelled A is projected as blocked in the backlog"
    );

    // B completed green.
    let exec_b = format!("exec:{b}:1");
    assert_eq!(
        prod.execution_phase(&exec_b),
        ("done".to_string(), "completed".to_string()),
        "the second authorized task completed"
    );
}

// ===========================================================================
// Small ledger helpers for the executor cases
// ===========================================================================

fn event_count(ledger: &SqliteLedger, execution: &ExecutionId, kind: &str) -> usize {
    ledger
        .events(execution)
        .expect("events")
        .iter()
        .filter(|event| event.kind == kind)
        .count()
}

fn cancel_request_count(ledger: &SqliteLedger, execution: &ExecutionId) -> usize {
    event_count(ledger, execution, "cancel_requested")
}
