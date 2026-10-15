//! SF.3 acceptance: execution-scoped recovery gate before recovered dispatch.
//!
//! The gate (`Executor::recover_execution`) replays **one** execution's recorded
//! history, requires it to agree with the durable projection, and reconciles
//! only that execution's pending intents/survivors on the registered binding.
//!
//! Evidence is layered:
//!
//! - executor-seam cases drive a real `SqliteLedger` with task-scoped fake
//!   activities that carry a distinct acceptance/workspace and refuse to probe
//!   another task's context;
//! - production cases drive the real `agalma work --once` CLI over a scratch
//!   git + `bd` store in scripted mode, exercising `recover_inflight` — the
//!   frozen `TaskPackage/v1` admission precedes the crash, and recovery reads it;
//! - a recorded-survivor case uses the production scripted activity set
//!   (`activities_with_mode`) so `build_probe` terminates a real process-group
//!   leader through `workers::reconcile_survivor` and writes durable stop
//!   evidence.
//!
//! Scratch state lives under `target/test-runs/`; no `/tmp`, no network, no
//! paid inference.
//!
//! | Case | Scenario |
//! |------|----------|
//! | 1 | tampered projection with preserved history refuses with zero effects |
//! | 2 | two executions with distinct acceptance/workspaces; each uses its own |
//! | 3 | crash-after-effect reconciles without repetition |
//! | 4 | ambiguous survivor parks and prevents redispatch |
//! | 5 | production `work` recovery parks a tampered projection (no effects) |
//! | 6 | production `work` recovery uses each task's frozen acceptance |
//! | 7 | recorded survivor terminated with stop evidence before redispatch |

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use agalma_conductor::harness_binding::{harness_handle, HarnessChoice};
use agalma_conductor::phases::{
    activities_with_mode, effects_log_path, run_root, ActivityMode, PhaseDeps, RunState,
};
use agalma_conductor::stop::FileStopControl;
use agalma_conductor::workers;
use agalma_conductor::workspace_process::WorkspaceHandle;
use agalma_contracts::{
    ContractError, ExecutionApi, ExecutionId, ExecutionPhase, ExecutionState, LedgerApi,
    OperationId, StepOutcome, TaskId,
};
use agalma_execution::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, Executor, ExecutorConfig,
    OperationContext, EXECUTION_DEFINITION_VERSION,
};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;
use serde_json::{json, Value};

/// The non-terminal phases the pipeline exercises.
const PHASES: [ExecutionPhase; 5] = [
    ExecutionPhase::Intake,
    ExecutionPhase::Checkout,
    ExecutionPhase::Build,
    ExecutionPhase::Verify,
    ExecutionPhase::Integrate,
];

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

/// Every observation the activity set makes, shared across its phase handlers.
#[derive(Default, Debug)]
struct Observed {
    /// `(task, phase, attempt)` for each survivor probe.
    probes: Vec<(TaskId, ExecutionPhase, u32)>,
    /// `(task, phase, attempt)` where a confirmed survivor was terminated.
    terminated: Vec<(TaskId, ExecutionPhase, u32)>,
    /// Operation ids for which a fresh effect ran.
    executed: Vec<String>,
    /// Operation ids for which a surviving effect was completed.
    reconciled: Vec<String>,
    /// `(task, acceptance)` for each executed operation: the task-scoped
    /// acceptance each recovered context actually ran.
    acceptance_ran: Vec<(TaskId, String)>,
}

/// A task-scoped activity: it appends effects to its task's workspace log, runs
/// its own acceptance, and **panics** if the executor ever probes/executes an
/// operation belonging to a different task. This is the mechanism that proves
/// the recovery gate is execution-scoped: a global probe against the wrong
/// activity set fails here instead of silently producing a wrong-context effect.
struct TaskActivity {
    task: TaskId,
    /// The task's frozen acceptance context (distinct per task).
    acceptance: String,
    effects: PathBuf,
    context: PathBuf,
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
        {
            let mut observed = self.observed.borrow_mut();
            observed.executed.push(ctx.operation_id.to_string());
            observed
                .acceptance_ran
                .push((ctx.task_id.clone(), self.acceptance.clone()));
        }
        append_effect(&self.effects, ctx.operation_id.as_str())?;
        // Record the acceptance this context ran, in its own workspace.
        write_file(&self.context, &self.acceptance)?;
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
        let mut observed = self.observed.borrow_mut();
        observed
            .probes
            .push((ctx.task_id.clone(), ctx.phase, ctx.attempt));
        if probe == EffectProbe::Present {
            // A confirmed survivor is terminated here (stand-in for the real
            // `workers::reconcile_survivor`) before it is completed.
            observed
                .terminated
                .push((ctx.task_id.clone(), ctx.phase, ctx.attempt));
        }
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

fn advance_outcome() -> ActivityOutcome {
    ActivityOutcome {
        result: json!({ "status": "ok" }),
        next: ActivityNext::Advance,
    }
}

/// Build an executor whose activities are bound to `task` with `acceptance` and
/// log to `workspace`. Only the build phase honours `build_probe`.
fn build_executor(
    ledger: SqliteLedger,
    task: &TaskId,
    acceptance: &str,
    workspace: &Path,
    observed: &Rc<RefCell<Observed>>,
    build_probe: ProbeMode,
) -> Executor<SqliteLedger> {
    let mut executor = Executor::new(
        ledger,
        ExecutorConfig {
            max_attempts: 2,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
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
                acceptance: acceptance.to_string(),
                effects: effects_log(workspace),
                context: context_artifact(workspace),
                probe,
                outcome: advance_outcome(),
                observed: Rc::clone(observed),
            }),
        );
    }
    executor
}

fn open(db: &Path) -> SqliteLedger {
    SqliteLedger::open(db).expect("open ledger")
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

fn context_artifact(workspace: &Path) -> PathBuf {
    workspace.join("context.txt")
}

fn write_file(path: &Path, text: &str) -> Result<(), ActivityError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ActivityError::KnownFailure(format!("create dir: {e}")))?;
    }
    fs::write(path, text).map_err(|e| ActivityError::KnownFailure(format!("write file: {e}")))
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

/// The ordered effect log of a workspace.
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

/// The ordered effect log of a production run root.
fn run_effects(state_dir: &Path, execution: &str) -> Vec<String> {
    fs::read_to_string(effects_log_path(state_dir, &ExecutionId::new(execution)))
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Tamper the durable projection of one execution while preserving its events.
fn tamper_phase(db: &Path, exec: &ExecutionId, phase: &str) {
    let conn = rusqlite::Connection::open(db).expect("open ledger raw");
    let updated = conn
        .execute(
            "UPDATE executions SET phase=?1 WHERE execution_id=?2",
            rusqlite::params![phase, exec.as_str()],
        )
        .expect("tamper projection");
    assert_eq!(updated, 1, "tampered exactly one execution row");
}

// ===========================================================================
// Case 1 — tampered projection with preserved history refuses, zero effects
// ===========================================================================

#[test]
fn projection_tamper_with_preserved_history_refuses_with_zero_effects() {
    let scratch = ScratchDir::new("sf-gate-tamper");
    let state = scratch.child("state");
    let ws = scratch.child("ws-a");
    let db = state.join("ledger.sqlite");
    let task = TaskId::new("task-a");
    let exec;

    {
        let observed = Rc::new(RefCell::new(Observed::default()));
        let mut executor = build_executor(
            open(&db),
            &task,
            "ACCEPT-A",
            &ws,
            &observed,
            ProbeMode::File,
        );
        exec = executor.start(&task).expect("start");
        drive_to(&mut executor, &exec, ExecutionPhase::Build);
    }
    let before = effects(&ws);
    assert_eq!(before.len(), 2, "intake + checkout effects: {before:?}");

    // The projection now claims `verify`; the events still fold to `build`, so
    // history is preserved and only the projection is tampered.
    tamper_phase(&db, &exec, "verify");

    let observed = Rc::new(RefCell::new(Observed::default()));
    let mut executor = build_executor(
        open(&db),
        &task,
        "ACCEPT-A",
        &ws,
        &observed,
        ProbeMode::File,
    );
    let err = executor
        .recover_execution(&exec)
        .expect_err("tampered projection must refuse the gate");
    assert!(
        matches!(err, ContractError::Conflict(_)),
        "projection disagreement is a Conflict: {err:?}"
    );
    let err2 = executor
        .recover_execution(&exec)
        .expect_err("still refuses on retry");
    assert!(matches!(err2, ContractError::Conflict(_)));

    assert_eq!(effects(&ws), before, "zero subsequent effects after tamper");
    assert!(observed.borrow().executed.is_empty(), "no fresh effect ran");

    let pending = executor
        .ledger()
        .pending_intents()
        .expect("pending intents");
    assert_eq!(pending.len(), 1, "only the build intent is pending");
    assert_eq!(
        pending[0].operation_id,
        OperationId::derive(&exec, "build@1"),
        "the preserved build intent is unchanged"
    );
    assert!(
        executor
            .ledger()
            .operation_receipt(&OperationId::derive(&exec, "build@1"))
            .expect("receipt lookup")
            .is_none(),
        "no completion recorded for the refused operation"
    );
}

// ===========================================================================
// Case 2 — two executions, distinct acceptance/workspaces, each uses its own
// ===========================================================================

#[test]
fn scoped_recovery_uses_each_tasks_own_context() {
    let scratch = ScratchDir::new("sf-gate-scope");
    let state = scratch.child("state");
    let ws_a = scratch.child("ws-a");
    let ws_b = scratch.child("ws-b");
    let db = state.join("ledger.sqlite");
    let task_a = TaskId::new("task-a");
    let task_b = TaskId::new("task-b");
    let exec_a;
    let exec_b;

    {
        let observed = Rc::new(RefCell::new(Observed::default()));
        let mut executor = build_executor(
            open(&db),
            &task_a,
            "ACCEPT-A",
            &ws_a,
            &observed,
            ProbeMode::File,
        );
        exec_a = executor.start(&task_a).expect("start A");
        drive_to(&mut executor, &exec_a, ExecutionPhase::Build);
    }
    {
        let observed = Rc::new(RefCell::new(Observed::default()));
        let mut executor = build_executor(
            open(&db),
            &task_b,
            "ACCEPT-B",
            &ws_b,
            &observed,
            ProbeMode::File,
        );
        exec_b = executor.start(&task_b).expect("start B");
        drive_to(&mut executor, &exec_b, ExecutionPhase::Build);
    }
    assert_ne!(ws_a, ws_b, "the two tasks have distinct workspaces");

    // Recover only execution A with an activity set bound to task A (acceptance
    // A). If the gate were the global `recover()`, it would probe B's intent
    // through A's activity and the task-scoped assertion would panic.
    let obs_rec = Rc::new(RefCell::new(Observed::default()));
    let mut executor_a = build_executor(
        open(&db),
        &task_a,
        "ACCEPT-A",
        &ws_a,
        &obs_rec,
        ProbeMode::File,
    );
    executor_a
        .recover_execution(&exec_a)
        .expect("scoped recovery of A");
    assert_eq!(
        obs_rec.borrow().probes,
        vec![(task_a.clone(), ExecutionPhase::Build, 1)],
        "A's activity probed only A's pending build"
    );

    // B is untouched by A's scoped recovery.
    let pending = executor_a
        .ledger()
        .pending_intents()
        .expect("pending intents");
    assert!(
        pending.iter().any(|intent| intent.execution_id == exec_b
            && intent.operation_id == OperationId::derive(&exec_b, "build@1")),
        "B's build intent was not consumed by A's scoped recovery"
    );

    // Each recovered context runs its own acceptance in its own workspace.
    assert!(matches!(
        executor_a.step(&exec_a).expect("step A"),
        StepOutcome::Dispatched { .. }
    ));

    let obs_rec_b = Rc::new(RefCell::new(Observed::default()));
    let mut executor_b = build_executor(
        open(&db),
        &task_b,
        "ACCEPT-B",
        &ws_b,
        &obs_rec_b,
        ProbeMode::File,
    );
    executor_b
        .recover_execution(&exec_b)
        .expect("scoped recovery of B");
    assert_eq!(
        obs_rec_b.borrow().probes,
        vec![(task_b.clone(), ExecutionPhase::Build, 1)],
        "B's activity probed only B's pending build"
    );
    assert!(matches!(
        executor_b.step(&exec_b).expect("step B"),
        StepOutcome::Dispatched { .. }
    ));

    assert_eq!(
        obs_rec.borrow().acceptance_ran,
        vec![(task_a.clone(), "ACCEPT-A".to_string())],
        "A's recovered context ran A's acceptance"
    );
    assert_eq!(
        obs_rec_b.borrow().acceptance_ran,
        vec![(task_b.clone(), "ACCEPT-B".to_string())],
        "B's recovered context ran B's acceptance"
    );
    assert_eq!(
        fs::read_to_string(context_artifact(&ws_a)).expect("A context"),
        "ACCEPT-A"
    );
    assert_eq!(
        fs::read_to_string(context_artifact(&ws_b)).expect("B context"),
        "ACCEPT-B"
    );
    // Distinct workspaces/acceptance, not shared paths/IDs.
    assert_ne!(
        fs::read_to_string(context_artifact(&ws_a)).unwrap(),
        fs::read_to_string(context_artifact(&ws_b)).unwrap()
    );
}

// ===========================================================================
// Case 3 — crash after effect reconciles without repetition
// ===========================================================================

#[test]
fn crash_after_effect_reconciles_without_repetition() {
    let scratch = ScratchDir::new("sf-gate-crash");
    let state = scratch.child("state");
    let ws = scratch.child("ws-a");
    let db = state.join("ledger.sqlite");
    let task = TaskId::new("task-a");
    let exec;

    {
        let observed = Rc::new(RefCell::new(Observed::default()));
        let mut executor = build_executor(
            open(&db),
            &task,
            "ACCEPT-A",
            &ws,
            &observed,
            ProbeMode::File,
        );
        exec = executor.start(&task).expect("start");
        drive_to(&mut executor, &exec, ExecutionPhase::Build);
    }

    let build_op = OperationId::derive(&exec, "build@1");
    append_effect(&effects_log(&ws), build_op.as_str()).expect("plant landed effect");
    let before = effects(&ws);
    assert_eq!(before.len(), 3, "intake + checkout + landed build effect");

    let observed = Rc::new(RefCell::new(Observed::default()));
    let mut executor = build_executor(
        open(&db),
        &task,
        "ACCEPT-A",
        &ws,
        &observed,
        ProbeMode::File,
    );
    executor
        .recover_execution(&exec)
        .expect("recovery reconciles the landed effect");

    assert_eq!(effects(&ws), before, "the effect is not repeated");
    assert!(
        observed.borrow().executed.is_empty(),
        "recovery dispatches no fresh effect"
    );
    assert_eq!(
        observed.borrow().reconciled,
        vec![build_op.to_string()],
        "the surviving effect is completed via reconcile"
    );
    assert_eq!(
        observed.borrow().terminated,
        vec![(task.clone(), ExecutionPhase::Build, 1)],
        "the confirmed survivor is terminated before completion"
    );

    let receipt = executor
        .ledger()
        .operation_receipt(&build_op)
        .expect("receipt lookup")
        .expect("build receipt recorded");
    assert_eq!(receipt.operation_id, build_op);
    let events = executor.ledger().events(&exec).expect("events");
    assert!(
        events
            .iter()
            .any(|event| event.kind == "operation_reconciled"),
        "a reconciled operation event is recorded"
    );
    assert_eq!(
        executor.status(&exec).expect("status").phase,
        ExecutionPhase::Verify,
        "reconciled build advanced the pipeline without repetition"
    );
}

// ===========================================================================
// Case 4 — ambiguous survivor parks and prevents redispatch
// ===========================================================================

#[test]
fn ambiguous_survivor_parks_and_prevents_redispatch() {
    let scratch = ScratchDir::new("sf-gate-ambiguous");
    let state = scratch.child("state");
    let ws = scratch.child("ws-a");
    let db = state.join("ledger.sqlite");
    let task = TaskId::new("task-a");
    let exec;

    {
        let observed = Rc::new(RefCell::new(Observed::default()));
        let mut executor = build_executor(
            open(&db),
            &task,
            "ACCEPT-A",
            &ws,
            &observed,
            ProbeMode::File,
        );
        exec = executor.start(&task).expect("start");
        drive_to(&mut executor, &exec, ExecutionPhase::Build);
    }
    let before = effects(&ws);
    assert_eq!(before.len(), 2);

    let observed = Rc::new(RefCell::new(Observed::default()));
    let mut executor = build_executor(
        open(&db),
        &task,
        "ACCEPT-A",
        &ws,
        &observed,
        ProbeMode::Force(EffectProbe::Ambiguous),
    );
    executor
        .recover_execution(&exec)
        .expect("ambiguous survivor parks without an error");

    let status = executor.status(&exec).expect("status");
    assert_eq!(
        (status.phase, status.state),
        (ExecutionPhase::Parked, ExecutionState::Parked),
        "ambiguous evidence parks the execution"
    );
    assert!(
        status
            .parked_reason
            .as_deref()
            .unwrap_or_default()
            .contains("ambiguous"),
        "parked reason names the ambiguity: {:?}",
        status.parked_reason
    );
    assert_eq!(effects(&ws), before, "ambiguity produced no effect");
    assert!(
        observed.borrow().executed.is_empty(),
        "no dispatch under ambiguity"
    );
    assert!(
        observed.borrow().terminated.is_empty(),
        "no survivor terminated"
    );

    match executor.step(&exec).expect("step") {
        StepOutcome::Parked { .. } => {}
        other => panic!("ambiguous survivor must prevent redispatch, got {other:?}"),
    }
    assert_eq!(
        effects(&ws),
        before,
        "no effect after the refused redispatch"
    );
    assert!(
        executor
            .ledger()
            .operation_receipt(&OperationId::derive(&exec, "build@1"))
            .expect("receipt lookup")
            .is_none(),
        "the ambiguous operation is never completed"
    );
}

// ===========================================================================
// Production harness — real `agalma work --once` over scratch git + bd
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
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        stderr_of(&out)
    );
}

fn assert_aborted(status: &std::process::ExitStatus) {
    assert!(!status.success(), "process should abort: {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "expected SIGABRT: {status:?}");
    }
}

/// A scratch origin repository with a real isolated `bd` store.
struct Prod {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Prod {
    fn new(name: &str) -> Prod {
        require_bd();
        let dir = ScratchDir::new(&format!("sf-gate-{name}"));
        let origin = dir.child("origin");
        fs::create_dir_all(origin.join("src")).expect("origin src");
        fs::write(
            origin.join("src").join("lib.rs"),
            "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
        )
        .expect("seed source");

        run_ok("git", &origin, &["init", "-q", "-b", "main"]);
        run_ok(
            "git",
            &origin,
            &["config", "user.email", "agalma@localhost"],
        );
        run_ok("git", &origin, &["config", "user.name", "Agalma"]);
        run_ok("git", &origin, &["add", "-A"]);
        run_ok("git", &origin, &["commit", "-qm", "seed"]);

        let prefix = format!("sfgate{}", name.replace('-', ""));
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

    /// Create a green task whose frozen acceptance echoes `marker`.
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
        let id = value["id"].as_str().expect("created id").to_string();
        self.protect(&id);
        id
    }

    /// Write/merge the operator `ProtectedInputs/v1` entry for `task_id` (SF.6).
    fn protect(&self, task_id: &str) {
        let root = fs::canonicalize(&self.origin).expect("canonical origin");
        let marker = "ok\n";
        let entry = json!({
            "task_id": task_id,
            "repository_root": root.to_string_lossy(),
            "base_sha": self.head(),
            "allowed_paths": ["src"],
            "protected_paths": ["tests"],
            "baseline": {
                "format": "ProtectedBaseline/v1",
                "files": [{
                    "path": "protected/marker.txt",
                    "mode": "100644",
                    "digest": agalma_contracts::digest::sha256_digest(marker.as_bytes()),
                    "bytes": marker.len() as u64,
                    "text": marker,
                }],
                "commands": ["sh -c 'true'"],
            },
        });
        let path = self.state_dir.join("protected-inputs.json");
        let mut entries: Vec<Value> = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("entries").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        entries.retain(|e| e.get("task_id").and_then(Value::as_str) != Some(task_id));
        entries.push(entry);
        fs::create_dir_all(&self.state_dir).expect("state dir");
        let doc = json!({ "format": "ProtectedInputs/v1", "entries": entries });
        fs::write(&path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write manifest");
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

    fn head(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.origin)
            .output()
            .expect("git rev-parse");
        assert!(out.status.success(), "git rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `(phase, state)` from the durable execution row.
    fn execution_phase(&self, execution: &str) -> (String, String) {
        let conn =
            rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger");
        conn.query_row(
            "SELECT phase, state FROM executions WHERE execution_id=?1",
            [execution],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("execution row")
    }

    /// Unconsumed dispatch intents for `execution`.
    fn pending_intents(&self, execution: &str) -> Vec<String> {
        let conn =
            rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger");
        let mut stmt = conn
            .prepare(
                "SELECT operation_id FROM dispatch_intents WHERE execution_id=?1 AND consumed=0",
            )
            .expect("prepare intents");
        let rows = stmt
            .query_map([execution], |row| row.get::<_, String>(0))
            .expect("query intents");
        rows.map(|row| row.expect("intent row")).collect()
    }

    /// The frozen-package admission reference recorded for `execution`, if any.
    fn admitted(&self, execution: &str) -> Option<Value> {
        let conn =
            rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger");
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM execution_events \
                 WHERE execution_id=?1 AND kind='task.package_admitted' \
                 ORDER BY rowid DESC LIMIT 1",
                [execution],
                |row| row.get(0),
            )
            .ok();
        payload.map(|text| serde_json::from_str(&text).expect("admission payload json"))
    }

    /// The captured acceptance output artifact for attempt 1, command `index`.
    fn acceptance_log(&self, execution: &str, index: usize) -> String {
        let root = run_root(&self.state_dir, &ExecutionId::new(execution));
        let path = root.join("artifacts").join(format!("accept@1-{index}.txt"));
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn ledger(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger")
    }

    /// The reason recorded in the durable `parked` event for `execution`.
    fn park_reason(&self, execution: &str) -> Option<String> {
        let conn = self.ledger();
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM execution_events \
                 WHERE execution_id=?1 AND kind='parked' ORDER BY rowid DESC LIMIT 1",
                [execution],
                |row| row.get(0),
            )
            .ok();
        payload
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| {
                value
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
    }

    /// Completed-operation receipts recorded for `execution`.
    fn receipt_count(&self, execution: &str) -> usize {
        self.ledger()
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE execution_id=?1",
                [execution],
                |row| row.get::<_, i64>(0),
            )
            .expect("receipt count") as usize
    }

    /// Every dispatch-intent operation id announced for `execution`, consumed or
    /// not (a phase `enter_phase` announces one per transition).
    fn announced_intents(&self, execution: &str) -> Vec<String> {
        let conn = self.ledger();
        let mut stmt = conn
            .prepare(
                "SELECT operation_id FROM dispatch_intents WHERE execution_id=?1 \
                 ORDER BY operation_id",
            )
            .expect("prepare intents");
        let rows = stmt
            .query_map([execution], |row| row.get::<_, String>(0))
            .expect("query intents");
        rows.map(|row| row.expect("intent row")).collect()
    }

    fn effects(&self, execution: &str) -> Vec<String> {
        run_effects(&self.state_dir, execution)
    }

    fn tamper_phase(&self, execution: &str, phase: &str) {
        tamper_phase(
            &self.state_dir.join("ledger.sqlite"),
            &ExecutionId::new(execution),
            phase,
        );
    }
}

// ===========================================================================
// Case 5 — production work recovery parks a tampered projection (no effects)
// ===========================================================================

#[test]
fn work_gate_parks_tampered_projection_without_effects() {
    let prod = Prod::new("tamper");
    let id = prod.create_task("Gate tamper task", "true");
    let before = prod.head();

    // Crash after the frozen package is admitted but before any phase runs, so
    // the execution is genuinely non-terminal with a durable package reference.
    let crash = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&crash.status);

    let exec = format!("exec:{id}:1");
    assert!(
        prod.admitted(&exec).is_some(),
        "frozen package admitted before the crash"
    );
    assert_eq!(
        prod.execution_phase(&exec),
        ("intake".to_string(), "running".to_string()),
        "activated, non-terminal execution"
    );
    let intake = OperationId::derive(&ExecutionId::new(&exec), "intake").to_string();
    assert_eq!(prod.pending_intents(&exec), vec![intake.clone()]);

    // Tamper the projection while preserving the recorded history (the events
    // still fold to `intake`).
    prod.tamper_phase(&exec, "verify");

    let recovery = prod.work(&[]);
    assert!(
        recovery.status.success(),
        "recovery tolerates the park: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    // The gate — not an unrelated verifier/workspace failure — refused: the
    // stderr names the gate and the projection/history disagreement.
    let stderr = stderr_of(&recovery);
    assert!(
        stderr.contains("recovery gate parks")
            && stderr.contains("reconstructed state disagrees with projection"),
        "the gate reports the projection disagreement: {stderr}"
    );
    // The same reason is persisted durably by the recovery park event.
    let reason = prod
        .park_reason(&exec)
        .expect("a persisted recovery park event");
    assert!(
        reason.contains("reconstructed state disagrees with projection"),
        "persisted park reason names the disagreement: {reason}"
    );

    assert_eq!(
        prod.execution_phase(&exec),
        ("parked".to_string(), "parked".to_string()),
        "the gate parks a projection that disagrees with history"
    );
    assert_eq!(
        prod.head(),
        before,
        "no integration without history agreement"
    );
    assert!(
        prod.effects(&exec).is_empty(),
        "the recovery gate produced no effects: {:?}",
        prod.effects(&exec)
    );
    // No phase work was driven: the only announced intent is the intake admitted
    // at activation, no later phase intent exists, and no operation completed.
    assert_eq!(
        prod.announced_intents(&exec),
        vec![intake.clone()],
        "the gate announced no phase intent and advanced nothing"
    );
    assert_eq!(
        prod.receipt_count(&exec),
        0,
        "the gate completed no operation"
    );
    assert_eq!(
        prod.pending_intents(&exec),
        vec![intake],
        "the intake intent is untouched (unconsumed)"
    );
}

// ===========================================================================
// Case 6 — production work recovery uses each task's frozen acceptance
// ===========================================================================

#[test]
fn work_recovery_uses_each_tasks_frozen_acceptance() {
    let prod = Prod::new("twocontext");

    // Freeze task A, then crash its admission: A is non-terminal.
    let a = prod.create_task("Frozen A", "ACCEPT_A");
    let crash_a = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&crash_a.status);
    let exec_a = format!("exec:{a}:1");
    assert!(prod.admitted(&exec_a).is_some());

    // A boot with no other task recovers A from its own frozen package to done
    // (integrating A moves `main`).
    let boot_a = prod.work(&[]);
    assert!(
        boot_a.status.success(),
        "recover A: {}\n{}",
        stdout_of(&boot_a),
        stderr_of(&boot_a)
    );
    assert_eq!(
        prod.execution_phase(&exec_a),
        ("done".to_string(), "completed".to_string()),
        "A was recovered and completed"
    );

    // The operator registers B against the *post-A* origin SHA (fail-closed base
    // matching is preserved), then crashes B's admission: B non-terminal.
    let b = prod.create_task("Frozen B", "ACCEPT_B");
    let crash_b = prod.work(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&crash_b.status);
    let exec_b = format!("exec:{b}:1");
    assert_eq!(
        prod.execution_phase(&exec_b),
        ("intake".to_string(), "running".to_string()),
    );

    // Recover B from its own frozen package.
    let recovery = prod.work(&[]);
    assert!(
        recovery.status.success(),
        "recover B: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );
    assert_eq!(
        prod.execution_phase(&exec_b),
        ("done".to_string(), "completed".to_string())
    );

    // Each recovered execution ran its own frozen acceptance, in its own
    // workspace — distinct context, not shared paths/IDs.
    let a_log = prod.acceptance_log(&exec_a, 0);
    let b_log = prod.acceptance_log(&exec_b, 0);
    assert!(
        a_log.contains("ACCEPT_A") && !a_log.contains("ACCEPT_B"),
        "A used A's frozen acceptance: {a_log}"
    );
    assert!(
        b_log.contains("ACCEPT_B") && !b_log.contains("ACCEPT_A"),
        "B used B's frozen acceptance: {b_log}"
    );
    assert_ne!(exec_a, exec_b);
    assert_ne!(
        run_root(&prod.state_dir, &ExecutionId::new(&exec_a)),
        run_root(&prod.state_dir, &ExecutionId::new(&exec_b)),
        "distinct execution workspaces"
    );
}

// ===========================================================================
// Case 7 — recorded survivor terminated with stop evidence before redispatch
// ===========================================================================

struct SurvivorHarness {
    executor: Executor<SqliteLedger>,
    _workspace: Rc<RefCell<WorkspaceHandle>>,
    _state: Rc<RefCell<RunState>>,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("target-template")
}

/// The production scripted activity set (same wiring recovery.rs uses) so
/// `build_probe` terminates recorded survivors through the real sandbox.
fn build_survivor_harness(state_dir: &Path) -> SurvivorHarness {
    fs::create_dir_all(state_dir).expect("state dir");
    let deps = PhaseDeps {
        state_dir: state_dir.to_path_buf(),
        fixture: fixture_dir(),
        model: "test/offline".to_string(),
        verifier_model: "test/offline".to_string(),
        route: "builder:fix".to_string(),
        verifier_route: "verifier:fix".to_string(),
        role: "builder".to_string(),
        profile_path: state_dir.join("unused-worker.sb"),
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        verifier_proxy_url: None,
        paid_proxy: false,
        extra_ro_roots: Vec::new(),
        toolchain_env: BTreeMap::new(),
        turn_timeout: Duration::from_secs(1),
        verify_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(1),
        repo: None,
        harness: harness_handle(HarnessChoice::OpenCode),
        stop: Rc::new(FileStopControl::new(state_dir)),
    };
    let workspace = Rc::new(RefCell::new(WorkspaceHandle::in_process(state_dir)));
    let state = Rc::new(RefCell::new(RunState::default()));
    let acts = activities_with_mode(
        deps,
        Rc::clone(&workspace),
        Rc::clone(&state),
        ActivityMode::Scripted,
    );
    let mut executor = Executor::new(
        SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger"),
        ExecutorConfig {
            max_attempts: 2,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    executor.add_activity(ExecutionPhase::Intake, acts.intake);
    executor.add_activity(ExecutionPhase::Checkout, acts.checkout);
    executor.add_activity(ExecutionPhase::Build, acts.build);
    executor.add_activity(ExecutionPhase::Verify, acts.verify);
    executor.add_activity(ExecutionPhase::Integrate, acts.integrate);
    SurvivorHarness {
        executor,
        _workspace: workspace,
        _state: state,
    }
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).expect("read json");
    serde_json::from_str(&text).expect("parse json")
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn spawn_grouped_sleeper() -> (u32, Child) {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("/bin/sleep");
    command.arg("600");
    command.process_group(0);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("spawn sleeper");
    let pid = child.id();
    (pid, child)
}

#[test]
fn recover_execution_terminates_recorded_survivor_before_redispatch() {
    let scratch = ScratchDir::new("sf-gate-survivor");
    let state = scratch.child("state");
    let task = TaskId::new("fix-answer");
    let exec;

    // Drive a real execution to the build intent (real checkout runs).
    {
        let mut harness = build_survivor_harness(&state);
        exec = harness.executor.start(&task).expect("start");
        drive_to(&mut harness.executor, &exec, ExecutionPhase::Build);
    }

    // Plant a live process-group leader recorded for the build, plus a started
    // marker and no completion marker: a genuine surviving worker.
    let (pid, mut child) = spawn_grouped_sleeper();
    workers::write_json(
        &workers::worker_path(&state, &exec, "build", 1),
        &json!({
            "pid": pid,
            "pgid": pid,
            "attempt": 1,
            "phase": "build",
            "operation_id": OperationId::derive(&exec, "build@1").to_string(),
            "started_at_unix_ms": 1,
        }),
    )
    .expect("write worker record");
    workers::write_json(
        &workers::started_path(&state, &exec, "build", 1),
        &json!({ "phase": "build", "attempt": 1 }),
    )
    .expect("write started marker");
    assert!(pid_alive(pid), "survivor is live before recovery");

    // Reopen (simulated restart) and run the scoped recovery gate only.
    let mut harness = build_survivor_harness(&state);
    harness
        .executor
        .recover_execution(&exec)
        .expect("scoped recovery of the build");

    // The recorded survivor must be terminated promptly — assert death before
    // any blocking reap. Poll `try_wait` (non-blocking) bounded, not a 600s
    // natural-exit wait; clean the process group up if termination was missed.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut dead = false;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait survivor") {
            Some(_) => {
                dead = true;
                break;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    if !dead {
        let _ = Command::new("/bin/kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .status();
        let _ = child.kill();
        let _ = child.wait();
        panic!("recorded survivor {pid} was not terminated promptly by the gate");
    }
    assert!(
        !pid_alive(pid),
        "recorded survivor terminated by the gate before redispatch"
    );
    let evidence = read_json(&workers::stop_path(&state, &exec, "build", 1));
    assert_eq!(
        evidence["process_group_gone"], true,
        "durable stop evidence: {evidence}"
    );
    assert!(
        !workers::worker_path(&state, &exec, "build", 1).exists(),
        "worker record cleared after termination"
    );
    assert_eq!(
        run_effects(&state, exec.as_str()),
        Vec::<String>::new(),
        "no build effect before redispatch"
    );

    // Only now does the executor dispatch the build, exactly once.
    assert!(matches!(
        harness.executor.step(&exec).expect("step"),
        StepOutcome::Dispatched { .. }
    ));
    assert_eq!(
        run_effects(&state, exec.as_str()).len(),
        1,
        "build dispatched once after confirmed termination"
    );
}
