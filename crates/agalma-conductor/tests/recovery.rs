//! M0.8 recovery + kill-switch exit matrix (`agalma-4ui.10`).
//!
//! Native Rust tests. Cases 1, 4, and 6 spawn the real `agalma` binary against a
//! fresh state dir; cases 2, 3, and 5 drive the executor in-process with the
//! scripted activity set and the real checkout/integrate effects. All scratch
//! state lives under `target/test-runs/` (never a system temp directory).
//!
//! | Case | Scenario |
//! |------|----------|
//! | 1 | crash (SIGKILL) mid-build; survivor terminated with evidence before redispatch |
//! | 2 | crash after integrate, before completion record; completes without re-merge |
//! | 3 | duplicate delivery of a completed operation returns the recorded receipt |
//! | 4 | kill latch (SIGTERM) survives restart; `resume` clears; work continues |
//! | 5 | surviving worker with a stale lease is terminated before redispatch |
//! | 6 | schema mismatch refuses to run and records no effect |

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{
    ExecutionApi, ExecutionId, ExecutionPhase, ExecutionState, LedgerApi, StepOutcome,
};
use agalma_execution::{DispatchOutcome, Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;
use agalma_workspace::Workspace;
use serde_json::Value;

use agalma_conductor::phases::{activities_with_mode, ActivityMode, PhaseDeps, RunState};
use agalma_conductor::workers;

const TASK: &str = "fix-answer";

fn task() -> TaskId {
    TaskId::new(TASK)
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("target-template")
}

fn fresh_dir(case: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("test-runs");
    let dir = base.join(format!("recovery-{case}-{}-{nanos}", std::process::id()));
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("reset run dir");
    }
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn exec_id() -> ExecutionId {
    ExecutionId::new("exec:fix-answer:1")
}

fn state_of(dir: &Path) -> PathBuf {
    dir.join("state")
}

// ---------------------------------------------------------------------------
// In-process executor harness (scripted activities + real checkout/integrate)
// ---------------------------------------------------------------------------

struct Harness {
    executor: Executor<SqliteLedger>,
    _workspace: Rc<RefCell<Workspace>>,
    _state: Rc<RefCell<RunState>>,
}

fn build_harness(state_dir: &Path) -> Harness {
    std::fs::create_dir_all(state_dir).expect("state dir");
    let deps = PhaseDeps {
        state_dir: state_dir.to_path_buf(),
        fixture: fixture_dir(),
        model: "test/offline".to_string(),
        role: "builder".to_string(),
        profile_path: state_dir.join("unused-worker.sb"),
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        extra_ro_roots: Vec::new(),
        toolchain_env: BTreeMap::new(),
        turn_timeout: Duration::from_secs(1),
        verify_timeout: Duration::from_secs(1),
        repo: None,
        harness: agalma_conductor::harness_binding::harness_handle(
            agalma_conductor::harness_binding::HarnessChoice::OpenCode,
        ),
    };
    let workspace = Rc::new(RefCell::new(Workspace::new(state_dir.to_path_buf())));
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
    Harness {
        executor,
        _workspace: workspace,
        _state: state,
    }
}

/// Drive an execution until it is waiting on `phase` (a pending intent exists).
fn drive_to(executor: &mut Executor<SqliteLedger>, exec: &ExecutionId, phase: ExecutionPhase) {
    for _ in 0..16 {
        let status = executor.status(exec).expect("status");
        if status.phase == phase {
            return;
        }
        if let StepOutcome::Parked { reason } = executor.step(exec).expect("step") {
            panic!("parked before {phase:?}: {reason}");
        }
    }
    panic!("did not reach phase {phase:?}");
}

fn effects_count(state_dir: &Path, exec: &ExecutionId) -> usize {
    let path = agalma_conductor::phases::effects_log_path(state_dir, exec);
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Count effect lines for one phase (e.g. `build@1`).
fn phase_effects(state_dir: &Path, exec: &ExecutionId, phase: &str) -> usize {
    let path = agalma_conductor::phases::effects_log_path(state_dir, exec);
    let prefix = format!("{phase}@");
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter(|l| l.trim().starts_with(&prefix))
                .count()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Binary helpers
// ---------------------------------------------------------------------------

fn agalma_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn run_command_output(state_dir: &Path, sleep_ms: u64) -> Output {
    Command::new(agalma_bin())
        .args([
            "run",
            "--fixture",
            fixture_dir().to_str().expect("fixture utf8"),
            "--once",
            "--state-dir",
            state_dir.to_str().expect("state utf8"),
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_SCRIPTED_BUILD_SLEEP_MS", sleep_ms.to_string())
        .output()
        .expect("run agalma")
}

fn spawn_run(state_dir: &Path, sleep_ms: u64) -> Child {
    Command::new(agalma_bin())
        .args([
            "run",
            "--fixture",
            fixture_dir().to_str().expect("fixture utf8"),
            "--once",
            "--state-dir",
            state_dir.to_str().expect("state utf8"),
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_SCRIPTED_BUILD_SLEEP_MS", sleep_ms.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agalma")
}

fn run_subcommand(subcommand: &str, state_dir: &Path, extra: &[&str]) -> Output {
    let mut args = vec![subcommand];
    args.extend_from_slice(extra);
    args.push("--state-dir");
    args.push(state_dir.to_str().expect("state utf8"));
    Command::new(agalma_bin())
        .args(&args)
        .output()
        .expect("run agalma subcommand")
}

fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

fn signal(pid: u32, sig: &str) {
    let _ = Command::new("/bin/kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status();
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn read_worker_pid(state_dir: &Path, exec: &ExecutionId) -> u32 {
    let path = workers::worker_path(state_dir, exec, "build", 1);
    let text = std::fs::read_to_string(&path).expect("worker record");
    let value: Value = serde_json::from_str(&text).expect("worker json");
    value["pid"].as_u64().expect("worker pid") as u32
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("read json");
    serde_json::from_str(&text).expect("parse json")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ===========================================================================
// Case 1 — crash mid-build; survivor terminated with evidence before redispatch
// ===========================================================================

#[test]
fn crash_mid_build_terminates_survivor_before_redispatch() {
    let dir = fresh_dir("case1");
    let state = state_of(&dir);
    let exec = exec_id();

    let mut child = spawn_run(&state, 60_000);
    let started = workers::started_path(&state, &exec, "build", 1);
    assert!(
        wait_for_path(&started, Duration::from_secs(60)),
        "scripted build did not start"
    );
    let survivor = read_worker_pid(&state, &exec);
    assert!(pid_alive(survivor), "worker should be alive before SIGKILL");

    signal(child.id(), "KILL");
    let _ = child.wait();
    assert!(
        pid_alive(survivor),
        "orphaned worker survives the conductor crash"
    );

    // Restart against the same state dir: recovery must terminate the survivor
    // with evidence before the build is redispatched.
    let out = run_command_output(&state, 0);
    assert!(
        out.status.success(),
        "restart should complete: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!pid_alive(survivor), "survivor must be terminated on boot");

    let stop = workers::stop_path(&state, &exec, "build", 1);
    assert!(stop.exists(), "stop evidence recorded: {}", stop.display());
    let evidence = read_json(&stop);
    assert_eq!(
        evidence["process_group_gone"], true,
        "termination evidence: {evidence}"
    );

    // No duplicate side effects: main is the candidate exactly once.
    let checkout = state.join("runs/exec-fix-answer-1/checkout");
    assert_eq!(
        git(&checkout, &["rev-parse", "refs/heads/main"]),
        git(
            &checkout,
            &["rev-parse", "refs/heads/candidate/exec-fix-answer-1"]
        ),
        "main == candidate"
    );
    assert_eq!(git(&checkout, &["rev-list", "--count", "main"]), "2");
    assert_eq!(
        git(&checkout, &["tag", "-l", "m0/exec-fix-answer-1"]),
        "m0/exec-fix-answer-1"
    );

    let status = run_subcommand("status", &state, &[]);
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("phase=Done"), "status reports done: {text}");
}

// ===========================================================================
// Case 2 — crash after integrate, before completion record
// ===========================================================================

#[test]
fn crash_after_integrate_completes_without_remerge() {
    if std::env::var("M08_CHILD").as_deref() == Ok("case2") {
        child_case2();
    }
    let dir = fresh_dir("case2");
    let state = state_of(&dir);
    let status = spawn_child(
        "crash_after_integrate_completes_without_remerge",
        "case2",
        &state,
    );
    assert_sigabrt(&status);

    let exec = exec_id();
    let checkout = state.join("runs/exec-fix-answer-1/checkout");
    // The merge already landed on disk before the crash.
    assert_eq!(
        git(&checkout, &["rev-parse", "refs/heads/main"]),
        git(
            &checkout,
            &["rev-parse", "refs/heads/candidate/exec-fix-answer-1"]
        ),
        "merge landed before crash"
    );

    // Restart: recovery detects the landed merge and completes without re-merging.
    let mut harness = build_harness(&state);
    harness.executor.recover().expect("recover");

    let status = harness.executor.status(&exec).expect("status");
    assert_eq!(
        (status.phase, status.state),
        (ExecutionPhase::Done, ExecutionState::Completed),
        "recovery completes the execution"
    );

    let receipt = harness
        .executor
        .ledger()
        .operation_receipt(&OperationId::derive(&exec, "integrate"))
        .expect("receipt lookup")
        .expect("integrate receipt reconciled");
    assert!(
        receipt.result["result"]["result_sha"].as_str().is_some(),
        "receipt carries the result sha: {}",
        receipt.result
    );

    // Exactly one candidate commit: the merge was not repeated.
    assert_eq!(git(&checkout, &["rev-list", "--count", "main"]), "2");
    assert_eq!(
        git(&checkout, &["tag", "-l", "m0/exec-fix-answer-1"]),
        "m0/exec-fix-answer-1"
    );

    let events = harness.executor.ledger().events(&exec).expect("events");
    assert!(
        events.iter().any(|e| e.kind == "operation_reconciled"),
        "recovery records a reconciled operation"
    );
}

fn child_case2() -> ! {
    let state = PathBuf::from(std::env::var("M08_STATE_DIR").expect("M08_STATE_DIR"));
    let mut harness = build_harness(&state);
    let exec = harness.executor.start(&task()).expect("start");
    drive_to(&mut harness.executor, &exec, ExecutionPhase::Integrate);

    // Arm the crash hook so the integrate effect runs, then SIGABRT before the
    // completion record is written.
    std::env::set_var("AGALMA_EXEC_CRASH_AT", "after_effect_before_completion");
    let _ = harness.executor.step(&exec);
    std::process::abort();
}

// ===========================================================================
// Case 3 — duplicate delivery returns the recorded receipt
// ===========================================================================

#[test]
fn duplicate_delivery_returns_recorded_receipt() {
    let dir = fresh_dir("case3");
    let state = state_of(&dir);
    let mut harness = build_harness(&state);
    let exec = harness.executor.start(&task()).expect("start");
    drive_to(&mut harness.executor, &exec, ExecutionPhase::Build);

    let op = OperationId::derive(&exec, "build@1");
    let before = effects_count(&state, &exec);
    let first = harness.executor.deliver_operation(&op).expect("first");
    assert!(
        matches!(first, DispatchOutcome::Executed { .. }),
        "first delivery executes: {first:?}"
    );
    assert_eq!(effects_count(&state, &exec), before + 1);

    let second = harness.executor.deliver_operation(&op).expect("second");
    assert!(
        matches!(second, DispatchOutcome::Recorded { .. }),
        "duplicate returns recorded: {second:?}"
    );
    assert_eq!(
        effects_count(&state, &exec),
        before + 1,
        "duplicate repeats no effect"
    );
}

// ===========================================================================
// Case 4 — kill latch survives restart; resume clears; work continues
// ===========================================================================

#[test]
fn kill_latch_survives_restart_then_resume_completes() {
    let dir = fresh_dir("case4");
    let state = state_of(&dir);
    let exec = exec_id();

    let mut child = spawn_run(&state, 60_000);
    let started = workers::started_path(&state, &exec, "build", 1);
    assert!(
        wait_for_path(&started, Duration::from_secs(60)),
        "scripted build did not start"
    );
    let survivor = read_worker_pid(&state, &exec);

    // SIGTERM: the handler persists the latch and exits cleanly (code 0).
    signal(child.id(), "TERM");
    let status = child.wait().expect("wait");
    assert!(status.success(), "SIGTERM exits cleanly: {status:?}");

    let status_out = run_subcommand("status", &state, &[]);
    let text = String::from_utf8_lossy(&status_out.stdout);
    assert!(text.contains("kill_latch=true"), "latch persisted: {text}");

    // Restart stays blocked: the survivor is reconciled but nothing dispatches.
    let blocked = run_command_output(&state, 0);
    assert!(
        !blocked.status.success(),
        "latched restart must not complete"
    );
    assert_eq!(
        phase_effects(&state, &exec, "build"),
        1,
        "no build effect while latched"
    );
    assert!(
        !pid_alive(survivor),
        "survivor reconciled during latched boot"
    );
    assert!(workers::stop_path(&state, &exec, "build", 1).exists());

    // `resume` clears the latch; the next run finishes the task.
    let resumed = run_subcommand("resume", &state, &[]);
    assert!(resumed.status.success());

    let done = run_command_output(&state, 0);
    assert!(
        done.status.success(),
        "resumed run completes: {}",
        String::from_utf8_lossy(&done.stderr)
    );
    assert_eq!(
        phase_effects(&state, &exec, "build"),
        2,
        "build redispatched once"
    );
    let final_status = run_subcommand("status", &state, &[]);
    assert!(String::from_utf8_lossy(&final_status.stdout).contains("phase=Done"));
}

// ===========================================================================
// Case 5 — surviving worker with a stale lease is terminated before redispatch
// ===========================================================================

#[test]
fn stale_lease_survivor_terminated_before_redispatch() {
    let dir = fresh_dir("case5");
    let state = state_of(&dir);
    let exec = exec_id();

    // Drive a fresh execution to the build intent (real checkout runs).
    {
        let mut harness = build_harness(&state);
        let started_exec = harness.executor.start(&task()).expect("start");
        assert_eq!(started_exec, exec);
        drive_to(&mut harness.executor, &exec, ExecutionPhase::Build);
    }

    // Plant a stale worker lease: a live process-group leader recorded with an
    // ancient start time, plus a started marker and no completion marker.
    let (pid, mut child) = spawn_grouped_sleeper();
    let record = serde_json::json!({
        "pid": pid,
        "pgid": pid,
        "attempt": 1,
        "phase": "build",
        "operation_id": OperationId::derive(&exec, "build@1").to_string(),
        "started_at_unix_ms": 1,
    });
    workers::write_json(&workers::worker_path(&state, &exec, "build", 1), &record)
        .expect("write worker record");
    workers::write_json(
        &workers::started_path(&state, &exec, "build", 1),
        &serde_json::json!({ "phase": "build", "attempt": 1 }),
    )
    .expect("write started marker");

    // Reopen (simulated restart) and recover.
    let mut harness = build_harness(&state);
    harness.executor.recover().expect("recover");

    // Reap the terminated child (the test process is its parent).
    let _ = child.wait();
    assert!(!pid_alive(pid), "stale-lease survivor terminated");
    let evidence = read_json(&workers::stop_path(&state, &exec, "build", 1));
    assert_eq!(evidence["process_group_gone"], true);
    assert_eq!(
        effects_count(&state, &exec),
        0,
        "recovery does not dispatch the build"
    );

    // Only now does the executor dispatch the build.
    let step = harness.executor.step(&exec).expect("step");
    assert!(
        matches!(step, StepOutcome::Dispatched { .. }),
        "build dispatched after termination: {step:?}"
    );
    assert_eq!(effects_count(&state, &exec), 1);
}

#[cfg(unix)]
fn spawn_grouped_sleeper() -> (u32, std::process::Child) {
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

// ===========================================================================
// Case 6 — schema mismatch refuses to run, records no effect
// ===========================================================================

#[test]
fn schema_mismatch_refuses_to_run() {
    let dir = fresh_dir("case6");
    let state = state_of(&dir);
    std::fs::create_dir_all(&state).expect("state dir");

    // Create a valid ledger, then record an incompatible physical schema.
    {
        let _ledger = SqliteLedger::open(state.join("ledger.sqlite")).expect("open ledger");
    }
    {
        let conn = rusqlite::Connection::open(state.join("ledger.sqlite")).expect("raw open");
        conn.execute("UPDATE meta SET value='99' WHERE key='schema_version'", [])
            .expect("corrupt schema version");
    }

    let out = run_command_output(&state, 0);
    assert!(!out.status.success(), "schema mismatch must refuse to run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("incompatible ledger schema"),
        "stderr names the mismatch: {stderr}"
    );
    assert!(
        !state.join("runs").exists(),
        "no effect recorded under schema mismatch"
    );
}

// ---------------------------------------------------------------------------
// child re-exec helpers (case 2)
// ---------------------------------------------------------------------------

fn spawn_child(test_name: &str, case: &str, state_dir: &Path) -> ExitStatus {
    let exe = std::env::current_exe().expect("current_exe");
    Command::new(exe)
        .args([test_name, "--exact", "--test-threads=1", "--nocapture"])
        .env("M08_CHILD", case)
        .env("M08_STATE_DIR", state_dir)
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
