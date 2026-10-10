//! M1.7 harness rebind drill (`agalma-52k.8`).
//!
//! Deterministic, in-process tests over the real repo-mode checkout/integrate
//! effects and the deterministic reference `HarnessApi` adapters. No vendor, no
//! network, no bd. Scratch state lives under `target/test-runs/rebind-*`.
//!
//! | Case | Scenario |
//! |------|----------|
//! | a | red attempt on `reference-fail`; rebind to `reference`; next attempt green; task done; binding records differ |
//! | b | in-flight operation's binding pinned; restart with flipped config recovers on the pinned implementation |
//! | c | a new execution generation after completion binds the current (flipped) implementation |
//! | d | bind gate: a missing required capability is rejected; a missing optional capability falls back |
//! | e | binary end-to-end: `agalma work --harness reference` succeeds and `agalma status` reports the binding (bd-gated) |

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agalma_contracts::harness::CAP_SYNTHETIC_FEEDBACK;
use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{
    BindingRecord, ContractError, ExecutionApi, ExecutionPhase, ExecutionState, LedgerApi,
    StepOutcome,
};
use agalma_execution::{Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;
use agalma_workspace::Workspace;

use agalma_conductor::harness_binding::{
    describe_for, harness_handle, persist_binding, plan_binding, plan_from_describe,
    read_pinned_binding, HarnessChoice, HarnessHandle, HarnessKind,
};
use agalma_conductor::phases::{activities_with_mode, ActivityMode, PhaseDeps, RepoMode, RunState};

const TASK: &str = "rebind-task";
const ACCEPTANCE: &str = "sh -c 'test -f .agalma/reference-ok'";

fn task() -> TaskId {
    TaskId::new(TASK)
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

fn fresh(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = root()
        .join("target")
        .join("test-runs")
        .join(format!("rebind-{name}-{}-{nanos}", std::process::id()));
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
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

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A scratch origin repo (git) plus a state dir.
struct Origin {
    origin: PathBuf,
    state_dir: PathBuf,
}

impl Origin {
    fn new(name: &str) -> Origin {
        let dir = fresh(name);
        let origin = dir.join("origin");
        std::fs::create_dir_all(origin.join("src")).expect("origin src");
        std::fs::write(
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
        Origin {
            origin,
            state_dir: dir.join("state"),
        }
    }
}

/// Build an in-process repo-mode executor over the scripted verify runner and
/// the deterministic reference harness, returning the shared binding handle.
fn build_harness(
    origin: &Origin,
    choice: HarnessChoice,
) -> (Executor<SqliteLedger>, HarnessHandle) {
    std::fs::create_dir_all(&origin.state_dir).expect("state dir");
    let handle = harness_handle(choice);
    let deps = PhaseDeps {
        state_dir: origin.state_dir.clone(),
        fixture: origin.origin.clone(),
        model: "test/offline".to_string(),
        role: "builder".to_string(),
        profile_path: origin.state_dir.join("unused-worker.sb"),
        builder_prompt: origin.state_dir.join("unused-builder.md"),
        proxy_url: None,
        extra_ro_roots: Vec::new(),
        toolchain_env: BTreeMap::new(),
        turn_timeout: Duration::from_secs(5),
        verify_timeout: Duration::from_secs(5),
        repo: Some(RepoMode {
            origin: origin.origin.clone(),
            base_ref: "main".to_string(),
            task_title: "Rebind the harness".to_string(),
            acceptance: vec![ACCEPTANCE.to_string()],
        }),
        harness: handle.clone(),
    };
    let workspace = Rc::new(RefCell::new(Workspace::new(origin.state_dir.clone())));
    let state = Rc::new(RefCell::new(RunState::default()));
    let acts = activities_with_mode(
        deps,
        Rc::clone(&workspace),
        Rc::clone(&state),
        ActivityMode::Scripted,
    );
    let mut executor = Executor::new(
        SqliteLedger::open(origin.state_dir.join("ledger.sqlite")).expect("open ledger"),
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
    (executor, handle)
}

/// Drive until the execution is waiting on `phase` (before delivering it).
fn drive_to(
    executor: &mut Executor<SqliteLedger>,
    exec: &agalma_contracts::ExecutionId,
    phase: ExecutionPhase,
) {
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

fn drive_to_terminal(executor: &mut Executor<SqliteLedger>, exec: &agalma_contracts::ExecutionId) {
    for _ in 0..32 {
        match executor.step(exec).expect("step") {
            StepOutcome::Idle { .. } | StepOutcome::Parked { .. } => return,
            StepOutcome::Dispatched { .. } | StepOutcome::Advanced { .. } => {}
            StepOutcome::Blocked { .. } => panic!("unexpected block"),
        }
    }
    panic!("execution did not reach a terminal state");
}

fn pinned(
    state_dir: &Path,
    exec: &agalma_contracts::ExecutionId,
    step: &str,
) -> Option<BindingRecord> {
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger");
    read_pinned_binding(&ledger, exec, &OperationId::derive(exec, step)).expect("read pin")
}

/// Durable verify verdict for an attempt (from the recorded operation receipt).
fn verify_status(state_dir: &Path, exec: &agalma_contracts::ExecutionId, attempt: u32) -> String {
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger");
    let receipt = ledger
        .operation_receipt(&OperationId::derive(exec, &format!("verify@{attempt}")))
        .expect("receipt lookup")
        .expect("verify receipt recorded");
    receipt.result["result"]["status"]
        .as_str()
        .unwrap_or("?")
        .to_string()
}

// ===========================================================================
// Case a — red reference-fail, rebind, green reference, done
// ===========================================================================

#[test]
fn red_reference_fail_then_rebind_reference_completes() {
    let origin = Origin::new("red-rebind-green");
    let (mut executor, handle) = build_harness(&origin, HarnessChoice::ReferenceFail);
    let exec = executor.start(&task()).expect("start");

    // Drive until the executor enters build attempt 2 (attempt 1 went red).
    loop {
        let status = executor.status(&exec).expect("status");
        if status.phase == ExecutionPhase::Build && status.attempt == 2 {
            break;
        }
        match executor.step(&exec).expect("step") {
            StepOutcome::Idle { .. } | StepOutcome::Parked { .. } => {
                panic!("unexpected terminal before attempt 2")
            }
            _ => {}
        }
    }

    // Attempt 1 was red; the binding records differ.
    let build1 = pinned(&origin.state_dir, &exec, "build@1").expect("build@1 pin");
    assert_eq!(build1.implementation, "reference-fail");
    assert_eq!(verify_status(&origin.state_dir, &exec, 1), "red");

    // Rebind for the *new* attempt, then finish.
    *handle.borrow_mut() = HarnessChoice::Reference;
    drive_to_terminal(&mut executor, &exec);

    let status = executor.status(&exec).expect("status");
    assert_eq!(
        (status.phase, status.state),
        (ExecutionPhase::Done, ExecutionState::Completed),
        "green on the rebound harness"
    );
    let build2 = pinned(&origin.state_dir, &exec, "build@2").expect("build@2 pin");
    assert_eq!(build2.implementation, "reference");
    assert_ne!(
        build1.binding_id, build2.binding_id,
        "rebind changes the binding identity"
    );
    assert_eq!(build1.implementation_version, "reference-fail/v1");
    assert_eq!(build2.implementation_version, "reference/v1");
    let tag = format!("m1/exec-{TASK}-1");
    assert_eq!(
        git(&origin.origin, &["rev-parse", &format!("refs/tags/{tag}")]),
        git(&origin.origin, &["rev-parse", "refs/heads/main"]),
        "integration tag at merged main"
    );
}

// ===========================================================================
// Case b — in-flight operation recovers on its pinned binding
// ===========================================================================

#[test]
fn recovery_uses_pinned_binding_not_flipped_config() {
    let origin = Origin::new("recovery-pin");
    let exec;

    // First conductor: reach the build intent for attempt 1, then "start" the
    // operation by pinning its binding (the process dies before the effect).
    {
        let (mut first, _h) = build_harness(&origin, HarnessChoice::ReferenceFail);
        exec = first.start(&task()).expect("start");
        drive_to(&mut first, &exec, ExecutionPhase::Build);
        let mut ledger =
            SqliteLedger::open(origin.state_dir.join("ledger.sqlite")).expect("ledger");
        let plan = plan_binding(HarnessKind::ReferenceFail, 1, "test/offline").expect("plan");
        persist_binding(
            &mut ledger,
            &exec,
            &OperationId::derive(&exec, "build@1"),
            1,
            &plan,
        )
        .expect("pin");
    }

    // Restart with the *flipped* config: recovery must use the pinned binding.
    let (mut second, _h2) = build_harness(&origin, HarnessChoice::Reference);
    second.recover().expect("recover");
    drive_to_terminal(&mut second, &exec);

    let build1 = pinned(&origin.state_dir, &exec, "build@1").expect("build@1 pin");
    assert_eq!(
        build1.implementation, "reference-fail",
        "recovery stays on the pinned implementation"
    );
    assert_eq!(
        verify_status(&origin.state_dir, &exec, 1),
        "red",
        "pinned reference-fail leaves attempt 1 red"
    );

    // Attempt 2 resolves the current (flipped) config.
    let build2 = pinned(&origin.state_dir, &exec, "build@2").expect("build@2 pin");
    assert_eq!(build2.implementation, "reference");
    let status = second.status(&exec).expect("status");
    assert_eq!(status.phase, ExecutionPhase::Done);
    let tag = format!("m1/exec-{TASK}-1");
    assert_eq!(
        git(&origin.origin, &["rev-parse", &format!("refs/tags/{tag}")]),
        git(&origin.origin, &["rev-parse", "refs/heads/main"]),
        "integration tag at merged main"
    );
}

// ===========================================================================
// Case c — a new attempt after completion uses the current binding
// ===========================================================================

#[test]
fn new_generation_after_completion_uses_new_binding() {
    let origin = Origin::new("new-generation");
    let (mut executor, handle) = build_harness(&origin, HarnessChoice::Reference);
    let first = executor.start(&task()).expect("start gen1");
    drive_to_terminal(&mut executor, &first);
    assert_eq!(
        executor.status(&first).expect("status").phase,
        ExecutionPhase::Done
    );

    // Flip the config; a new claim cycle is a new generation.
    *handle.borrow_mut() = HarnessChoice::ReferenceFail;
    let second = executor.start(&task()).expect("start gen2");
    assert_eq!(
        second,
        agalma_contracts::ExecutionId::new(format!("exec:{TASK}:2"))
    );
    drive_to(&mut executor, &second, ExecutionPhase::Build);
    // Dispatch build@1 so its binding is resolved and recorded.
    executor.step(&second).expect("dispatch build@1");

    let build = pinned(&origin.state_dir, &second, "build@1").expect("gen2 build@1 pin");
    assert_eq!(
        build.implementation, "reference-fail",
        "new generation binds the current config"
    );
    assert_eq!(build.generation, 2);
}

// ===========================================================================
// Case d — capability bind gate (S0d static policy)
// ===========================================================================

#[test]
fn bind_gate_rejects_missing_required_and_falls_back_for_optional() {
    let red = describe_for(HarnessKind::ReferenceFail);
    let mut required = std::collections::BTreeSet::new();
    required.insert(CAP_SYNTHETIC_FEEDBACK.to_string());

    // Required capability missing -> refuse to bind.
    let err = plan_from_describe(
        &red,
        1,
        "test/offline",
        &required,
        &std::collections::BTreeSet::new(),
    )
    .expect_err("missing required capability must be rejected");
    assert!(
        matches!(err, ContractError::UnsupportedCapability(_)),
        "unsupported capability error: {err:?}"
    );

    // Optional capability missing -> declared supported fallback plan.
    let mut optional = std::collections::BTreeSet::new();
    optional.insert(CAP_SYNTHETIC_FEEDBACK.to_string());
    let plan = plan_from_describe(
        &red,
        1,
        "test/offline",
        &std::collections::BTreeSet::new(),
        &optional,
    )
    .expect("optional capability falls back");
    assert_eq!(plan.fallback, vec![CAP_SYNTHETIC_FEEDBACK.to_string()]);
    assert_eq!(plan.fallback_plan(), Some("fresh_session+handoff"));

    // Required capability present -> binds.
    let green = describe_for(HarnessKind::Reference);
    let bound = plan_from_describe(
        &green,
        1,
        "test/offline",
        &required,
        &std::collections::BTreeSet::new(),
    )
    .expect("advertised required capability binds");
    assert!(bound.binding.capabilities.contains(CAP_SYNTHETIC_FEEDBACK));
    assert_eq!(bound.binding.implementation, "reference");
}

// ===========================================================================
// Case e — binary end-to-end: `--harness` selection + `status` binding display
// ===========================================================================

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for this test but was not found");
}

fn bd(origin: &Path, beads: &Path, args: &[&str]) -> std::process::Output {
    Command::new("bd")
        .arg("--db")
        .arg(beads)
        .args(args)
        .current_dir(origin)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("spawn bd")
}

/// A scratch origin repo with one bd task whose acceptance checks the reference
/// success marker.
fn binary_scratch(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    require_bd();
    let dir = fresh(name);
    let origin = dir.join("origin");
    std::fs::create_dir_all(origin.join("src")).expect("origin src");
    std::fs::write(
        origin.join("src").join("lib.rs"),
        "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
    )
    .expect("seed");
    run_ok("git", &origin, &["init", "-q", "-b", "main"]);
    run_ok(
        "git",
        &origin,
        &["config", "user.email", "agalma@localhost"],
    );
    run_ok("git", &origin, &["config", "user.name", "Agalma"]);
    run_ok("git", &origin, &["add", "-A"]);
    run_ok("git", &origin, &["commit", "-qm", "seed"]);

    let init = Command::new("bd")
        .args([
            "init",
            "--prefix",
            "rebind",
            "--non-interactive",
            "--skip-agents",
            "--skip-hooks",
        ])
        .current_dir(&origin)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd init");
    assert!(init.status.success(), "bd init: {}", stderr_of(&init));

    let beads = origin.join(".beads");
    let description = "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'test -f .agalma/reference-ok'\npriority: normal\nfamily: fix\nmax_attempts: 1\n```\n";
    let create = bd(
        &origin,
        &beads,
        &[
            "create",
            "Rebind binary task",
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            description,
            "--json",
        ],
    );
    assert!(create.status.success(), "bd create: {}", stderr_of(&create));
    let value: serde_json::Value = serde_json::from_slice(&create.stdout).expect("create json");
    assert!(value["id"].as_str().is_some());
    (origin, dir.join("state"), beads)
}

fn agalma(args: &[&str], scripted: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agalma"));
    command.args(args);
    if scripted {
        command.env("AGALMA_ACTIVITY_MODE", "scripted");
    }
    command.output().expect("spawn agalma")
}

#[test]
fn binary_work_harness_flag_and_status_binding() {
    let (origin, state, _beads) = binary_scratch("binary-reference");
    let out = agalma(
        &[
            "work",
            "--repo",
            origin.to_str().expect("origin utf8"),
            "--state-dir",
            state.to_str().expect("state utf8"),
            "--once",
            "--harness",
            "reference",
        ],
        true,
    );
    assert!(
        out.status.success(),
        "work --harness reference should succeed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        stderr_of(&out)
    );

    let status = agalma(
        &["status", "--state-dir", state.to_str().expect("state utf8")],
        false,
    );
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        text.contains("harness=reference@reference/v1"),
        "status reports the bound harness: {text}"
    );
    assert!(text.contains("phase=Done"), "status reports done: {text}");

    // A red binding parks and is visible in status.
    let (origin2, state2, _beads2) = binary_scratch("binary-reference-fail");
    let red = agalma(
        &[
            "work",
            "--repo",
            origin2.to_str().expect("origin2 utf8"),
            "--state-dir",
            state2.to_str().expect("state2 utf8"),
            "--once",
            "--harness",
            "reference-fail",
        ],
        true,
    );
    assert!(!red.status.success(), "reference-fail parks the task");
    let status = agalma(
        &[
            "status",
            "--state-dir",
            state2.to_str().expect("state utf8"),
        ],
        false,
    );
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        text.contains("harness=reference-fail@reference-fail/v1"),
        "status reports the red binding: {text}"
    );
    assert!(text.contains("phase=Parked"), "task parked: {text}");
}
