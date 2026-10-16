//! M1.7 harness rebind drill (`agalma-52k.8`).
//!
//! Deterministic, in-process tests over the real repo-mode checkout/integrate
//! effects and the deterministic reference `HarnessApi` adapters. No vendor, no
//! network, no bd. Scratch state lives under `target/test-runs/rebind-*`;
//! scratch dirs are retained for review evidence (`ScratchDir::keep`).
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
use std::time::Duration;

use agalma_conductor::workspace_process::WorkspaceHandle;
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::harness::CAP_SYNTHETIC_FEEDBACK;
use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, BindingRecord, ContextKind, ContractError, DirectiveRef,
    ExecutionApi, ExecutionPhase, ExecutionState, LedgerApi, Role, StepOutcome,
};
use agalma_execution::{Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;
use agalma_taskqueue::TaskDefinition;

use agalma_conductor::harness_binding::{
    describe_for, harness_handle, persist_binding, plan_binding, plan_from_describe,
    read_pinned_binding, HarnessChoice, HarnessHandle, HarnessKind,
};
use agalma_conductor::phases::{activities_with_mode, ActivityMode, PhaseDeps, RepoMode, RunState};
use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, ProtectedInputs, PROTECTED_BASELINE_FORMAT,
    PROTECTED_INPUTS_FORMAT,
};
use agalma_conductor::task_package::{
    build_package, persist_package, AuthorizationInput, ContextInput, PackageInputs, RoleInput,
};
use agalma_testkit::ScratchDir;

const TASK: &str = "rebind-task";
const ACCEPTANCE: &str = "sh -c 'test -f .agalma/reference-ok'";

fn task() -> TaskId {
    TaskId::new(TASK)
}

fn fresh(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(&format!("rebind-{name}"));
    // Retain fixtures for review evidence (`ScratchDir::keep`); not auto-removed.
    dir.keep();
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

// ---------------------------------------------------------------------------
// SF.6 operator protected inputs (rebind-local fixture seeding)
//
// Repository verification is fail-closed: the in-process executor cases need the
// recorded package's frozen `scope.oracle` to be the selected operator protected
// entry, and the binary `agalma work --once` cases need the operator
// `ProtectedInputs/v1` manifest (`<state_dir>/protected-inputs.json`). The shared
// `tests/common` helper seeds an intentionally opaque oracle, which makes
// repo-mode verification refuse; the rebind fixture seeds directionally correct
// inputs locally instead, without touching `tests/common`.
// ---------------------------------------------------------------------------

/// An immutable protected *behavioral* judge: it independently re-checks that
/// the deterministic reference candidate applied its scripted change (the
/// `.agalma/reference-ok` marker) against the pinned candidate tree. Frozen as a
/// `ProtectedBaseline/v1` file and launched through `$AGALMA_PROTECTED_DIR`
/// (outside the candidate writable root), so the candidate cannot substitute its
/// own always-pass tests for the judge.
const PROTECTED_RUNNER: &str = "#!/bin/sh\n\
set -eu\n\
test -f \"$AGALMA_CANDIDATE_DIR/.agalma/reference-ok\"\n";
/// The frozen protected baseline command (runs the runner through the sandbox).
const PROTECTED_COMMAND: &str = "sh \"$AGALMA_PROTECTED_DIR/runner.sh\"";

/// The frozen `ProtectedBaseline/v1` (one executable file + one command).
fn protected_baseline() -> ProtectedBaseline {
    ProtectedBaseline {
        format: PROTECTED_BASELINE_FORMAT.to_string(),
        files: vec![BaselineFile {
            path: "runner.sh".to_string(),
            mode: "100755".to_string(),
            digest: sha256_digest(PROTECTED_RUNNER.as_bytes()),
            bytes: PROTECTED_RUNNER.len() as u64,
            text: PROTECTED_RUNNER.to_string(),
        }],
        commands: vec![PROTECTED_COMMAND.to_string()],
    }
}

/// The operator `ProtectedEntry` whose identity must match a scratch fixture:
/// the created bd/package task id, the canonicalized origin path, and the
/// resolved `main` base SHA. `allowed_paths` admits the deterministic reference
/// candidate change (`.agalma/reference-ok`); nothing it touches is protected.
fn protected_entry(task_id: &str, repository_root: &str, base_sha: &str) -> ProtectedEntry {
    ProtectedEntry {
        task_id: task_id.to_string(),
        repository_root: repository_root.to_string(),
        base_sha: base_sha.to_string(),
        allowed_paths: vec!["src".to_string(), ".agalma".to_string()],
        protected_paths: Vec::new(),
        baseline: protected_baseline(),
    }
}

/// Write an operator `ProtectedInputs/v1` manifest into `state_dir`
/// (canonicalized like `WorkArgs::state_dir`), the admission source production
/// reads at `<state_dir>/protected-inputs.json`.
fn write_protected_inputs(state_dir: &Path, entries: Vec<ProtectedEntry>) {
    let inputs = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries,
    };
    let bytes = serde_json::to_vec(&inputs).expect("serialize protected inputs");
    std::fs::create_dir_all(state_dir).expect("state dir");
    let state_dir = std::fs::canonicalize(state_dir).expect("canonical state dir");
    std::fs::write(state_dir.join("protected-inputs.json"), bytes).expect("write protected inputs");
}

/// Persist a recorded `TaskPackage/v1` for `execution_id` with explicit package
/// scope, acceptance, and frozen oracle bytes; return its package digest.
fn persist_rebind_package(
    state_dir: &Path,
    execution_id: &str,
    base_sha: &str,
    acceptance: &str,
    generation: u32,
    allowed_paths: Vec<String>,
    protected_paths: Vec<String>,
    oracle_bytes: Vec<u8>,
) -> String {
    let definition = TaskDefinition {
        task_id: TaskId::new(TASK),
        title: "Rebind the harness".to_string(),
        description: "Rebind fixture.\n".to_string(),
        design: Some("Design.\n".to_string()),
        notes: None,
        acceptance_criteria: Some(acceptance.to_string()),
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    };
    let inputs = PackageInputs {
        execution_id: execution_id.to_string(),
        generation: generation as u64,
        lease_generation: 1,
        repository_identity: "rebind-fixture".to_string(),
        base_sha: base_sha.to_string(),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 0,
            digest: sha256_digest(b"directive/v0"),
        },
        authorization: AuthorizationInput {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: sha256_digest(b"human-bootstrap/v1"),
            receipt_source: "bd:rebind-task/claim".to_string(),
            receipt_media_type: "application/json".to_string(),
            receipt_bytes: b"{}".to_vec(),
        },
        roles: vec![RoleInput::new(
            Role::Builder,
            "builder:fix".to_string(),
            "test/offline".to_string(),
            BindingPin {
                binding_id: "harness.opencode".to_string(),
                api: "harness".to_string(),
                api_version: 1,
                implementation_version: "harness/v1".to_string(),
                generation: 1,
                config_digest: sha256_digest(b"{}"),
            },
            b"builder prompt".to_vec(),
            "text/plain".to_string(),
        )],
        instructions: vec![ContextInput::new(
            ContextKind::Instruction,
            "AGENTS.md",
            "text/plain",
            b"frozen repository instruction\n".to_vec(),
        )],
        shared_design: Vec::new(),
        parent_design: Vec::new(),
        dependency_handoffs: Vec::new(),
        allowed_paths,
        protected_paths,
        readable_artifacts: Vec::new(),
        oracle_bytes,
        oracle_media_type: "application/json".to_string(),
        max_attempts: 2,
        allowance_nusd: 0,
        quota_policy_digest: sha256_digest(b"quota"),
        recorded_at_unix_ms: 1_791_820_800_000,
    };
    let built = build_package(&definition, inputs).expect("build rebind package");
    persist_package(state_dir, &built).expect("persist rebind package");
    built.package.package_digest
}

/// Seed a valid SF.6-fixture package for generations `1..=max_generation` of
/// `TASK`, matching the scratch origin's repository root and base SHA.
fn seed_rebind_packages(
    state_dir: &Path,
    repository_root: &str,
    base_sha: &str,
    acceptance: &str,
    max_generation: u32,
) {
    let entry = protected_entry(TASK, repository_root, base_sha);
    let oracle = entry.freeze().expect("freeze rebind entry");
    for generation in 1..=max_generation {
        persist_rebind_package(
            state_dir,
            &format!("exec:{TASK}:{generation}"),
            base_sha,
            acceptance,
            generation,
            entry.allowed_paths.clone(),
            entry.protected_paths.clone(),
            oracle.clone(),
        );
    }
}

/// A scratch origin repo (git) plus a state dir.
struct Origin {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
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
            state_dir: dir.join("state"),
            origin,
            _dir: dir,
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
    // SF.1/SF.6: repository mode requires the recorded package *and* a valid
    // frozen protected oracle entry. Seed a rebind-local valid fixture for every
    // generation this drill can dispatch (gen1, gen2); the shared
    // `tests/common` helper intentionally seeds an opaque oracle that refuses.
    let base_sha = git(&origin.origin, &["rev-parse", "refs/heads/main"]);
    seed_rebind_packages(
        &origin.state_dir,
        origin.origin.to_str().expect("origin utf8"),
        &base_sha,
        ACCEPTANCE,
        2,
    );
    let handle = harness_handle(choice);
    let deps = PhaseDeps {
        state_dir: origin.state_dir.clone(),
        fixture: origin.origin.clone(),
        model: "test/offline".to_string(),
        verifier_model: "test/offline".to_string(),
        route: "builder:fix".to_string(),
        verifier_route: "verifier:fix".to_string(),
        role: "builder".to_string(),
        builder_prompt: origin.state_dir.join("unused-builder.md"),
        proxy_url: None,
        verifier_proxy_url: None,
        paid_proxy: false,
        toolchain_env: BTreeMap::new(),
        turn_timeout: Duration::from_secs(5),
        verify_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        repo: Some(RepoMode {
            origin: origin.origin.clone(),
            base_ref: "main".to_string(),
            task_title: "Rebind the harness".to_string(),
            acceptance: vec![ACCEPTANCE.to_string()],
        }),
        harness: handle.clone(),
        stop: std::rc::Rc::new(agalma_conductor::stop::FileStopControl::new(
            &origin.state_dir,
        )),
    };
    let workspace = Rc::new(RefCell::new(WorkspaceHandle::in_process(&origin.state_dir)));
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
/// success marker, and (unless `seed` is false) a matching operator protected
/// manifest seeded into its state dir.
fn binary_scratch(name: &str) -> (ScratchDir, PathBuf, PathBuf, PathBuf) {
    binary_scratch_inner(name, true)
}

/// The binary scratch origin without a seeded manifest (missing-input refusal
/// coverage).
fn binary_scratch_raw(name: &str) -> (ScratchDir, PathBuf, PathBuf, PathBuf) {
    binary_scratch_inner(name, false)
}

fn binary_scratch_inner(name: &str, seed: bool) -> (ScratchDir, PathBuf, PathBuf, PathBuf) {
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
    let id = value["id"].as_str().expect("created id").to_string();
    let state = dir.join("state");
    // Seed the operator protected-inputs manifest matching this task's identity
    // (task id, canonical origin, resolved `main` base) so the binary
    // repository-mode work reaches verification.
    let canonical = std::fs::canonicalize(&origin)
        .expect("canonical origin")
        .to_string_lossy()
        .into_owned();
    let base_sha = git(&origin, &["rev-parse", "refs/heads/main"]);
    if seed {
        write_protected_inputs(&state, vec![protected_entry(&id, &canonical, &base_sha)]);
    }
    (dir, origin, state, beads)
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
    let (_dir, origin, state, _beads) = binary_scratch("binary-reference");
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
    let (_dir2, origin2, state2, _beads2) = binary_scratch("binary-reference-fail");
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

/// Missing and identity-mismatched operator protected inputs continue to refuse
/// repository work over the real `agalma work --once` path (SF.6 fail-closed):
/// an absent manifest never authorizes repository work, and a present manifest
/// whose task/repository/base identity does not match the fixture refuses.
#[test]
fn binary_work_refuses_missing_or_mismatched_protected_inputs() {
    // Missing manifest: an absent operator manifest never authorizes work.
    let (_dir, origin, state, _beads) = binary_scratch_raw("binary-refuse-missing");
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
        !out.status.success(),
        "missing manifest must refuse repository work: {}",
        stderr_of(&out)
    );
    let err = stderr_of(&out);
    assert!(
        err.contains("protected-inputs.json"),
        "refusal names the manifest: {err}"
    );
    assert!(err.contains("ScopeDenied"), "refusal is a scope denial: {err}");

    // Identity mismatch: a present manifest whose base_sha does not match the
    // resolved base refuses (task/repository/base identity is validated).
    let (_dir2, origin2, state2, _beads2) = binary_scratch("binary-refuse-mismatch");
    let text = std::fs::read_to_string(state2.join("protected-inputs.json")).expect("manifest");
    let inputs = ProtectedInputs::parse(text.as_bytes()).expect("parse manifest");
    let id = inputs.entries[0].task_id.clone();
    let canonical = std::fs::canonicalize(&origin2)
        .expect("canonical origin")
        .to_string_lossy()
        .into_owned();
    write_protected_inputs(&state2, vec![protected_entry(&id, &canonical, &"0".repeat(40))]);
    let out = agalma(
        &[
            "work",
            "--repo",
            origin2.to_str().expect("origin utf8"),
            "--state-dir",
            state2.to_str().expect("state utf8"),
            "--once",
            "--harness",
            "reference",
        ],
        true,
    );
    assert!(
        !out.status.success(),
        "identity-mismatched manifest must refuse repository work: {}",
        stderr_of(&out)
    );
    assert!(
        stderr_of(&out).contains("ScopeDenied"),
        "mismatch is a scope denial: {}",
        stderr_of(&out)
    );
}

/// Whether `ProtectedEntry::parse` accepts a clone of `good` after `f` mutates
/// it (freeze then re-parse, the production canonical path).
fn entry_accepts(good: &ProtectedEntry, f: impl Fn(&mut ProtectedEntry)) -> bool {
    let mut entry = good.clone();
    f(&mut entry);
    ProtectedEntry::parse(&entry.freeze().expect("freeze entry")).is_ok()
}

/// Deterministic xorshift generator (same style as `digest.rs`).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn rng_hex(rng: &mut Rng, len: usize) -> String {
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        out.push(char::from_digit(rng.below(16) as u32, 16).expect("hex digit"));
    }
    out
}

/// Deterministic generated-input property (seed `0x5f6_rebn`, case budget 48)
/// over the operator fixture identity/manifest contract, derived independently
/// of the implementation:
/// - a generated `ProtectedEntry` freezes and re-parses unchanged;
/// - its `ProtectedInputs/v1` manifest loads from the admission source;
/// - selection with the exact task/repository/base identity returns the frozen
///   entry;
/// - changing any single identity field refuses selection.
/// Boundary/invalid cases (empty identity, empty/broken baseline, bad digest or
/// mode, duplicate files, wrong format, empty manifest, absent file) all refuse.
#[test]
fn rebind_protected_fixture_seeding_property_over_generated_cases() {
    // Design seed label `0x5f6_rebn`; the non-hex letters are their ASCII bytes.
    const SEED: u64 = 0x5f6_7265_626e;
    const CASES: usize = 48;

    const TASKS: [&str; 4] = ["task.a", "task-b", "task.c", "t"];
    const REPOS: [&str; 4] = ["/repo/one", "/repo two", "/r", "/deep/nested/origin"];
    const ALLOWED: [&str; 4] = ["src", "crates", "a/b", "src/lib.rs"];
    const PROTECTED: [&str; 3] = ["tests", "docs/protected.md", "baseline"];

    let dir = fresh("rebind-fixture-property");
    let state_dir = dir.join("state");

    let mut rng = Rng(SEED);
    let mut checked = 0usize;
    for _ in 0..CASES {
        let task_id = format!(
            "{}.{}",
            TASKS[rng.below(TASKS.len() as u64) as usize],
            rng.below(8)
        );
        let repository_root = REPOS[rng.below(REPOS.len() as u64) as usize].to_string();
        let base_sha = rng_hex(&mut rng, 40);
        let allowed: Vec<String> = (0..1 + rng.below(3))
            .map(|_| ALLOWED[rng.below(ALLOWED.len() as u64) as usize].to_string())
            .collect();
        let protected: Vec<String> = (0..rng.below(2))
            .map(|_| PROTECTED[rng.below(PROTECTED.len() as u64) as usize].to_string())
            .collect();
        let files: Vec<BaselineFile> = (0..1 + rng.below(2))
            .map(|index| {
                let text = format!("#!/bin/sh\necho {}\n", rng.below(1_000_000));
                BaselineFile {
                    path: format!("bin{index}/check.sh"),
                    mode: if rng.below(2) == 0 { "100644" } else { "100755" }.to_string(),
                    digest: sha256_digest(text.as_bytes()),
                    bytes: text.len() as u64,
                    text,
                }
            })
            .collect();
        let commands: Vec<String> = (0..1 + rng.below(2))
            .map(|_| format!("sh \"$AGALMA_PROTECTED_DIR/{}\"", files[0].path))
            .collect();
        let entry = ProtectedEntry {
            task_id,
            repository_root,
            base_sha,
            allowed_paths: allowed,
            protected_paths: protected,
            baseline: ProtectedBaseline {
                format: PROTECTED_BASELINE_FORMAT.to_string(),
                files,
                commands,
            },
        };

        let frozen = entry.freeze().expect("freeze generated entry");
        assert_eq!(
            ProtectedEntry::parse(&frozen).expect("re-parse frozen entry"),
            entry,
            "freeze must round-trip"
        );

        write_protected_inputs(&state_dir, vec![entry.clone()]);
        let inputs = ProtectedInputs::load(&state_dir).expect("generated manifest loads");
        let selected = inputs
            .select(&entry.task_id, &entry.repository_root, &entry.base_sha)
            .expect("exact identity selects");
        assert_eq!(selected, &entry);
        assert_eq!(selected.freeze().expect("freeze selected"), frozen);

        assert!(inputs
            .select(
                &format!("{}-x", entry.task_id),
                &entry.repository_root,
                &entry.base_sha
            )
            .is_err());
        assert!(inputs
            .select(
                &entry.task_id,
                &format!("{}-x", entry.repository_root),
                &entry.base_sha
            )
            .is_err());
        assert!(inputs
            .select(
                &entry.task_id,
                &entry.repository_root,
                &format!("{}-x", entry.base_sha)
            )
            .is_err());

        checked += 1;
    }
    assert_eq!(checked, CASES, "all generated cases executed");

    // Concrete refusal and boundary coverage over the manifest contract.
    let good = protected_entry("task.good", "/repo/origin", &"a".repeat(40));
    let bytes = |entries: Vec<ProtectedEntry>| {
        serde_json::to_vec(&ProtectedInputs {
            format: PROTECTED_INPUTS_FORMAT.to_string(),
            entries,
        })
        .expect("manifest bytes")
    };
    assert!(ProtectedInputs::parse(&bytes(vec![good.clone()])).is_ok());

    assert!(ProtectedInputs::parse(b"not json").is_err());
    assert!(ProtectedInputs::parse(b"{}").is_err());
    assert!(ProtectedInputs::parse(&bytes(Vec::new())).is_err());
    assert!(ProtectedInputs::parse(&bytes(vec![good.clone(), good.clone()])).is_err());
    let wrong_format = serde_json::to_vec(&serde_json::json!({
        "format": "ProtectedInputs/v2",
        "entries": [serde_json::to_value(&good).unwrap()],
    }))
    .expect("json");
    assert!(ProtectedInputs::parse(&wrong_format).is_err());

    assert!(!entry_accepts(&good, |e| e.task_id = "  ".to_string()));
    assert!(!entry_accepts(&good, |e| e.repository_root = String::new()));
    assert!(!entry_accepts(&good, |e| e.base_sha = String::new()));

    assert!(!entry_accepts(&good, |e| e.baseline.files.clear()));
    assert!(!entry_accepts(&good, |e| e.baseline.commands.clear()));
    assert!(!entry_accepts(&good, |e| e.baseline.commands = vec!["  ".to_string()]));

    assert!(!entry_accepts(&good, |e| e.baseline.files[0].digest = sha256_digest(b"other")));
    assert!(!entry_accepts(&good, |e| e.baseline.files[0].mode = "100600".to_string()));
    assert!(!entry_accepts(&good, |e| {
        let dup = e.baseline.files[0].clone();
        e.baseline.files.push(dup);
    }));

    let absent = fresh("rebind-fixture-absent");
    assert!(ProtectedInputs::load(&absent).is_err());
}
