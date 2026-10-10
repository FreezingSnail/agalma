//! Deterministic integration tests for the M0.7 conductor wiring.
//!
//! These avoid the live model: they exercise the executor wiring that the
//! conductor uses. All scratch state lives under `target/test-runs/` (never
//! `/tmp`).
//!
//! 1. `checkout_activity_prepares_isolated_repo` drives the *real* checkout
//!    activity through the executor and asserts the isolated git checkout.
//! 2. `red_verify_retries_then_parks` uses the scripted activity double to prove
//!    the bounded build/verify retry parks after `max_attempts`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agalma_contracts::ids::{ArtifactRef, TaskId};
use agalma_contracts::{ExecutionApi, ExecutionPhase, StepOutcome};
use agalma_execution::{
    ActivityNext, ActivityOutcome, Executor, ExecutorConfig, ScriptedActivity,
    EXECUTION_DEFINITION_VERSION,
};
use agalma_ledger::SqliteLedger;
use agalma_workspace::Workspace;

use agalma_conductor::phases::{activities, PhaseDeps, RunState};

fn run_dir(test: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("test-runs");
    let dir = base.join(format!("conductor-{test}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("target-template")
}

fn dummy_deps(state_dir: &Path) -> PhaseDeps {
    let fixture = fixture_dir();
    PhaseDeps {
        state_dir: state_dir.to_path_buf(),
        fixture,
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
    }
}

#[test]
fn checkout_activity_prepares_isolated_repo() {
    let dir = run_dir("checkout");
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");

    let deps = dummy_deps(&state_dir);
    let shared_state = Rc::new(RefCell::new(RunState::default()));
    let workspace = Rc::new(RefCell::new(Workspace::new(state_dir.clone())));
    let acts = activities(deps, Rc::clone(&workspace), Rc::clone(&shared_state));

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

    let execution = executor.start(&TaskId::new("fix-answer")).expect("start");
    // Drive until the checkout completes and the pipeline enters build; do not
    // deliver the build operation (that would need the live harness).
    loop {
        match executor.step(&execution).expect("step") {
            StepOutcome::Dispatched { .. } | StepOutcome::Advanced { .. } => {}
            other => panic!("unexpected step outcome: {other:?}"),
        }
        if executor.status(&execution).expect("status").phase == ExecutionPhase::Build {
            break;
        }
    }

    let checkout = shared_state
        .borrow()
        .checkout
        .clone()
        .expect("checkout recorded");
    assert!(Path::new(&checkout.path).is_dir(), "checkout dir exists");
    assert!(!checkout.base_sha.is_empty(), "base sha recorded");
    assert!(
        checkout.candidate_branch.starts_with("candidate/"),
        "candidate branch derived: {}",
        checkout.candidate_branch
    );

    let log = Command::new("git")
        .args(["--no-pager", "log", "--oneline"])
        .current_dir(&checkout.path)
        .output()
        .expect("git log");
    let text = String::from_utf8_lossy(&log.stdout);
    assert!(
        text.contains("baseline"),
        "baseline commit on the checkout: {text}"
    );

    let head = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(&checkout.path)
        .output()
        .expect("git symbolic-ref");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        checkout.candidate_branch
    );
}

fn retry_outcome(attempt: u32) -> ActivityOutcome {
    ActivityOutcome {
        result: serde_json::json!({ "status": "red", "attempt": attempt }),
        next: ActivityNext::Retry {
            failure: ArtifactRef::derive(&format!("verify@{attempt}")),
        },
    }
}

#[test]
fn red_verify_retries_then_parks() {
    let dir = run_dir("red-park");
    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");

    let mut executor = Executor::new(
        SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger"),
        ExecutorConfig {
            max_attempts: 2,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    // Non-verify phases advance automatically; verify is red on both attempts.
    for phase in [
        ExecutionPhase::Intake,
        ExecutionPhase::Checkout,
        ExecutionPhase::Build,
        ExecutionPhase::Integrate,
    ] {
        executor.add_activity(
            phase,
            Box::new(ScriptedActivity::new(
                dir.join(format!("effects-{phase:?}.log")),
            )),
        );
    }
    let mut verify = ScriptedActivity::new(dir.join("effects-verify.log"));
    verify.script(ExecutionPhase::Verify, 1, retry_outcome(1));
    verify.script(ExecutionPhase::Verify, 2, retry_outcome(2));
    executor.add_activity(ExecutionPhase::Verify, Box::new(verify));

    let execution = executor.start(&TaskId::new("scripted")).expect("start");
    loop {
        match executor.step(&execution).expect("step") {
            StepOutcome::Idle { .. } | StepOutcome::Parked { .. } => break,
            StepOutcome::Dispatched { .. } | StepOutcome::Advanced { .. } => continue,
            StepOutcome::Blocked { .. } => panic!("unexpected block"),
        }
    }

    let status = executor.status(&execution).expect("status");
    assert_eq!(status.phase, ExecutionPhase::Parked);
    let reason = status.parked_reason.expect("parked reason");
    assert!(
        reason.contains("max_attempts"),
        "park reason cites attempt bound: {reason}"
    );
}
