//! SF.6 acceptance: candidate scope and immutable protected verification.
//!
//! Focused command: `cargo test --offline -p agalma-conductor --test sf_scope_guard`.
//!
//! The suite drives the **production seams** — the real `VerifyActivity` and
//! `IntegrateActivity` from `activities_with_mode`, over a real git origin and a
//! real immutable package store — plus the operator `ProtectedInputs/v1`
//! manifest and the protected-root placement:
//!
//! - an always-pass candidate test loses to the independently executed frozen
//!   protected command;
//! - a permitted real source fix passes and integrates;
//! - committed and dirty candidate changes after verify are refused at the real
//!   merge boundary;
//! - missing / invalid scope, manifest, and baseline **fail closed**;
//! - the protected runner executes and the frozen judge judges behavior
//!   (read-only protection of the protected root is convention-only after
//!   sandbox removal, design §7.1);
//! - deterministic generated-input invariants cover the policy.
//!
//! Scratch lives under `target/test-runs/`; no `/tmp`, no network, no paid
//! inference.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use agalma_conductor::harness_binding::{harness_handle, HarnessChoice};
use agalma_conductor::phases::{
    activities_with_mode, builder_config_init, builder_runtime_dir, protected_root, scope_pin_path,
    verifier_config_init, verify_runtime_dir, verify_verdict_matches_pin, ActivityMode, PhaseDeps,
    RepoMode, RunState,
};
use agalma_conductor::scope_guard::{
    self, load_config_from, resolve_config_path_from, BaselineFile, CandidatePin, ProtectedBaseline,
    ProtectedEntry, ProtectedInputs, ScopePolicy, PROTECTED_BASELINE_FORMAT, PROTECTED_CONFIG_DIR,
    PROTECTED_INPUTS_FILE, PROTECTED_INPUTS_FORMAT,
};
use agalma_conductor::stop::FileStopControl;
use agalma_conductor::task_package::{
    build_package, persist_package, AuthorizationInput, ContextInput, PackageInputs, RoleInput,
};
use agalma_conductor::workspace_process::WorkspaceHandle;
use agalma_contracts::digest::{failure_code, sha256_digest, FailureCode};
use agalma_contracts::ids::{ArtifactRef, AttemptId};
use agalma_contracts::process::ProcessSpec;
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, ChangeKind, Checkout, ContextKind, DirectiveRef, ExecutionApi,
    ExecutionId, ExecutionPhase, ExecutionState, FileChange, Role, Scope, StepOutcome, TaskId,
    WorkspaceApi,
};
use agalma_execution::{
    Activity, ActivityError, ActivityNext, ActivityOutcome, EffectProbe, Executor, ExecutorConfig,
    OperationContext, EXECUTION_DEFINITION_VERSION,
};
use agalma_ipc::WorkspaceOps;
use agalma_ledger::SqliteLedger;
use agalma_process::LocalLauncher;
use agalma_taskqueue::TaskDefinition;
use agalma_testkit::ScratchDir;
use serde_json::json;

const TASK: &str = "sf-scope-guard.task";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn run_ok(program: &str, dir: &Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The immutable protected *behavioral* runner: it independently compiles the
/// candidate library and asserts `answer() == 42` by execution, so a comment
/// containing the expected text cannot spoof it and a semantically equivalent
/// but differently formatted fix still passes. No new dependency (existing
/// `rustc`).
const PROTECTED_SCRIPT: &str = "#!/bin/sh\n\
set -eu\n\
cand=\"$AGALMA_CANDIDATE_DIR\"\n\
work=\"$TMPDIR/protected-probe\"\n\
rm -rf \"$work\"\n\
mkdir -p \"$work\"\n\
rustc --edition 2021 --crate-type lib --crate-name cand \"$cand/src/lib.rs\" --out-dir \"$work\"\n\
printf '%s\\n' 'fn main() {' '    if cand::answer() != 42 {' '        eprintln!(\"protected behavior: answer() != 42\");' '        std::process::exit(1);' '    }' '}' > \"$work/main.rs\"\n\
rustc --edition 2021 \"$work/main.rs\" --extern cand=\"$work/libcand.rlib\" -L \"$work\" -o \"$work/probe\"\n\
\"$work/probe\"\n";
const PROTECTED_COMMAND: &str = "sh $AGALMA_PROTECTED_DIR/runner.sh";
const BROKEN_SOURCE: &str = "pub fn answer() -> u32 { 0 }\n";
const FIXED_SOURCE: &str = "pub fn answer() -> u32 { 42 }\n";
const BASE_SHA: &str = "0000000000000000000000000000000000000000";

fn baseline() -> ProtectedBaseline {
    ProtectedBaseline {
        format: PROTECTED_BASELINE_FORMAT.to_string(),
        files: vec![BaselineFile {
            path: "runner.sh".to_string(),
            mode: "100755".to_string(),
            digest: sha256_digest(PROTECTED_SCRIPT.as_bytes()),
            bytes: PROTECTED_SCRIPT.len() as u64,
            text: PROTECTED_SCRIPT.to_string(),
        }],
        commands: vec![PROTECTED_COMMAND.to_string()],
    }
}

fn entry() -> ProtectedEntry {
    ProtectedEntry {
        task_id: TASK.to_string(),
        repository_root: "/origin".to_string(),
        base_sha: BASE_SHA.to_string(),
        allowed_paths: vec!["src".to_string(), "tests".to_string()],
        protected_paths: vec!["tests/protected_check.sh".to_string()],
        baseline: baseline(),
    }
}

/// A real git origin with the protected baseline file present at base.
fn seed_origin(name: &str) -> (ScratchDir, PathBuf, PathBuf) {
    let dir = ScratchDir::new(name);
    let origin = dir.child("origin");
    fs::create_dir_all(origin.join("src")).expect("src");
    fs::create_dir_all(origin.join("tests")).expect("tests");
    fs::write(origin.join("src/lib.rs"), BROKEN_SOURCE).expect("lib");
    fs::write(origin.join("tests/protected_check.sh"), PROTECTED_SCRIPT).expect("protected");
    run_ok("git", &origin, &["init", "-q", "-b", "main"]);
    run_ok(
        "git",
        &origin,
        &["config", "user.email", "agalma@localhost"],
    );
    run_ok("git", &origin, &["config", "user.name", "Agalma"]);
    run_ok("git", &origin, &["add", "-A"]);
    run_ok("git", &origin, &["commit", "-qm", "seed"]);
    let state = dir.child("state");
    (dir, origin, state)
}

struct CandidateBuild {
    source: String,
    own_test: Option<String>,
    state: Rc<RefCell<RunState>>,
}

impl Activity for CandidateBuild {
    fn execute(&mut self, _ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        let checkout =
            self.state.borrow().checkout.clone().ok_or_else(|| {
                ActivityError::KnownFailure("no checkout for candidate build".into())
            })?;
        let co = Path::new(&checkout.path);
        fs::write(co.join("src/lib.rs"), &self.source)
            .map_err(|e| ActivityError::KnownFailure(format!("write source: {e}")))?;
        if let Some(test) = &self.own_test {
            fs::create_dir_all(co.join("tests"))
                .map_err(|e| ActivityError::KnownFailure(format!("mkdir tests: {e}")))?;
            fs::write(co.join("tests/own_test.sh"), test)
                .map_err(|e| ActivityError::KnownFailure(format!("write own test: {e}")))?;
        }
        Ok(ActivityOutcome {
            result: json!({ "status": "candidate" }),
            next: ActivityNext::Advance,
        })
    }
    fn effect_present(&mut self, _ctx: &OperationContext) -> EffectProbe {
        EffectProbe::Absent
    }
    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        self.execute(ctx)
    }
}

struct Noop;
impl Activity for Noop {
    fn execute(&mut self, _ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        Ok(ActivityOutcome {
            result: json!({ "phase": "intake" }),
            next: ActivityNext::Advance,
        })
    }
    fn effect_present(&mut self, _ctx: &OperationContext) -> EffectProbe {
        EffectProbe::Absent
    }
    fn reconcile(&mut self, ctx: &OperationContext) -> Result<ActivityOutcome, ActivityError> {
        self.execute(ctx)
    }
}

fn drive_to(executor: &mut Executor<SqliteLedger>, exec: &ExecutionId, target: ExecutionPhase) {
    for _ in 0..24 {
        let status = executor.status(exec).expect("status");
        if status.phase == target
            || matches!(status.phase, ExecutionPhase::Parked | ExecutionPhase::Done)
        {
            return;
        }
        match executor.step(exec) {
            Ok(StepOutcome::Dispatched { .. }) | Ok(StepOutcome::Advanced { .. }) => {}
            _ => return,
        }
    }
}

fn drain(executor: &mut Executor<SqliteLedger>, exec: &ExecutionId) {
    for _ in 0..32 {
        let status = executor.status(exec).expect("status");
        if matches!(status.phase, ExecutionPhase::Parked | ExecutionPhase::Done) {
            return;
        }
        match executor.step(exec) {
            Ok(StepOutcome::Dispatched { .. }) | Ok(StepOutcome::Advanced { .. }) => {}
            _ => return,
        }
    }
}

/// Build and persist the frozen package whose `scope.oracle` is `entry`.
fn build_frozen_package(
    state_dir: &Path,
    exec: &ExecutionId,
    entry: &ProtectedEntry,
) -> agalma_contracts::TaskPackage {
    build_frozen_package_with(state_dir, exec, entry, "sh -c 'true'")
}

fn build_frozen_package_with(
    state_dir: &Path,
    exec: &ExecutionId,
    entry: &ProtectedEntry,
    acceptance: &str,
) -> agalma_contracts::TaskPackage {
    let definition = TaskDefinition {
        task_id: TaskId::new(TASK),
        title: "SF.6 fixture".to_string(),
        description: "Body.\n".to_string(),
        design: Some("Design.\n".to_string()),
        notes: None,
        // The task acceptance is separate from the protected judge; both run.
        acceptance_criteria: Some(acceptance.to_string()),
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    };
    let inputs = PackageInputs {
        execution_id: exec.as_str().to_string(),
        generation: 1,
        lease_generation: 1,
        repository_identity: "agalma".to_string(),
        base_sha: entry.base_sha.clone(),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 0,
            digest: sha256_digest(b"directive"),
        },
        authorization: AuthorizationInput {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: sha256_digest(b"human-bootstrap/v1"),
            receipt_source: "bd:sf-scope-guard/claim".to_string(),
            receipt_media_type: "application/json".to_string(),
            receipt_bytes: b"{}".to_vec(),
        },
        roles: vec![RoleInput::new(
            Role::Builder,
            "route.builder".to_string(),
            "model-builder".to_string(),
            BindingPin {
                binding_id: "harness.opencode".to_string(),
                api: "harness".to_string(),
                api_version: 1,
                implementation_version: "opencode/v1".to_string(),
                generation: 1,
                config_digest: sha256_digest(b"{}"),
            },
            b"builder prompt".to_vec(),
            "text/plain",
        )],
        instructions: Vec::new(),
        shared_design: vec![ContextInput::new(
            ContextKind::SharedDesign,
            "docs/self-mutation-contracts.md",
            "text/plain",
            b"shared design".to_vec(),
        )],
        parent_design: Vec::new(),
        dependency_handoffs: Vec::new(),
        allowed_paths: entry.allowed_paths.clone(),
        protected_paths: entry.protected_paths.clone(),
        readable_artifacts: Vec::new(),
        oracle_bytes: entry.freeze().expect("freeze entry"),
        oracle_media_type: "application/json".to_string(),
        max_attempts: 1,
        allowance_nusd: 0,
        quota_policy_digest: sha256_digest(b"quota"),
        recorded_at_unix_ms: 1_791_820_800_000,
    };
    let built = build_package(&definition, inputs).expect("build package");
    persist_package(state_dir, &built).expect("persist package");
    built.package
}

/// Host toolchain env so the behavioral protected runner's `rustc` resolves.
fn host_toolchain() -> BTreeMap<String, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let pick = |var: &str, suffix: &str| {
        std::env::var(var)
            .ok()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(suffix))
    };
    let rustup = pick("RUSTUP_HOME", ".rustup");
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    );
    env.insert(
        "RUSTUP_HOME".to_string(),
        rustup.to_string_lossy().into_owned(),
    );
    env
}

struct Harness {
    executor: Executor<SqliteLedger>,
    _workspace: Rc<RefCell<WorkspaceHandle>>,
    _state: Rc<RefCell<RunState>>,
}

fn build_harness(
    state_dir: &Path,
    origin: &Path,
    source: &str,
    own_test: Option<String>,
) -> Harness {
    fs::create_dir_all(state_dir).expect("state dir");
    let deps = PhaseDeps {
        state_dir: state_dir.to_path_buf(),
        fixture: origin.to_path_buf(),
        model: "test/offline".to_string(),
        verifier_model: "test/offline".to_string(),
        route: "builder:fix".to_string(),
        verifier_route: "verifier:fix".to_string(),
        role: "builder".to_string(),
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        verifier_proxy_url: None,
        paid_proxy: false,
        toolchain_env: host_toolchain(),
        turn_timeout: Duration::from_secs(2),
        verify_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(1),
        repo: Some(RepoMode {
            origin: origin.to_path_buf(),
            base_ref: "main".to_string(),
            task_title: "SF.6 candidate".to_string(),
            acceptance: vec!["sh -c 'true'".to_string()],
        }),
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
            max_attempts: 1,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    executor.add_activity(ExecutionPhase::Intake, Box::new(Noop));
    executor.add_activity(ExecutionPhase::Checkout, acts.checkout);
    executor.add_activity(
        ExecutionPhase::Build,
        Box::new(CandidateBuild {
            source: source.to_string(),
            own_test,
            state: Rc::clone(&state),
        }),
    );
    executor.add_activity(ExecutionPhase::Verify, acts.verify);
    executor.add_activity(ExecutionPhase::Integrate, acts.integrate);
    Harness {
        executor,
        _workspace: workspace,
        _state: state,
    }
}

/// A Live-mode harness: the real `VerifyActivity` runs the acceptance through
/// the production launcher so the malicious-command path is exercised at the
/// production seam. Read-only protection of the protected root is no longer
/// OS-enforced (design §7.1); the behavioral judge still runs.
fn build_harness_live(state_dir: &Path, origin: &Path, source: &str) -> Harness {
    fs::create_dir_all(state_dir).expect("state dir");
    let deps = PhaseDeps {
        state_dir: state_dir.to_path_buf(),
        fixture: origin.to_path_buf(),
        model: "test/offline".to_string(),
        verifier_model: "test/offline".to_string(),
        route: "builder:fix".to_string(),
        verifier_route: "verifier:fix".to_string(),
        role: "builder".to_string(),
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        verifier_proxy_url: None,
        paid_proxy: false,
        toolchain_env: host_toolchain(),
        turn_timeout: Duration::from_secs(2),
        verify_timeout: Duration::from_secs(30),
        idle_timeout: Duration::from_secs(1),
        repo: Some(RepoMode {
            origin: origin.to_path_buf(),
            base_ref: "main".to_string(),
            task_title: "SF.6 candidate".to_string(),
            acceptance: vec!["sh -c 'true'".to_string()],
        }),
        harness: harness_handle(HarnessChoice::OpenCode),
        stop: Rc::new(FileStopControl::new(state_dir)),
    };
    let workspace = Rc::new(RefCell::new(WorkspaceHandle::in_process(state_dir)));
    let state = Rc::new(RefCell::new(RunState::default()));
    let acts = activities_with_mode(
        deps,
        Rc::clone(&workspace),
        Rc::clone(&state),
        ActivityMode::Live,
    );
    let mut executor = Executor::new(
        SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger"),
        ExecutorConfig {
            max_attempts: 1,
            execution_definition_version: EXECUTION_DEFINITION_VERSION,
        },
    );
    executor.add_activity(ExecutionPhase::Intake, Box::new(Noop));
    executor.add_activity(ExecutionPhase::Checkout, acts.checkout);
    executor.add_activity(
        ExecutionPhase::Build,
        Box::new(CandidateBuild {
            source: source.to_string(),
            own_test: None,
            state: Rc::clone(&state),
        }),
    );
    executor.add_activity(ExecutionPhase::Verify, acts.verify);
    executor.add_activity(ExecutionPhase::Integrate, acts.integrate);
    Harness {
        executor,
        _workspace: workspace,
        _state: state,
    }
}

fn origin_head(origin: &Path) -> String {
    git_out(origin, &["rev-parse", "refs/heads/main"])
}

fn read_checkout(state_dir: &Path, exec: &ExecutionId) -> Checkout {
    let path = agalma_conductor::phases::run_root(state_dir, exec).join("checkout.json");
    serde_json::from_slice(&fs::read(&path).expect("checkout artifact")).expect("parse checkout")
}

// ---------------------------------------------------------------------------
// Production seam — protected judge
// ---------------------------------------------------------------------------

#[test]
fn always_pass_candidate_loses_to_protected_baseline() {
    let (_dir, origin, state) = seed_origin("sf-scope-always-pass");
    let mut harness = build_harness(&state, &origin, BROKEN_SOURCE, Some("exit 0\n".to_string()));
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drain(&mut harness.executor, &exec);
    let status = harness.executor.status(&exec).expect("status");
    assert_eq!(
        (status.phase, status.state),
        (ExecutionPhase::Parked, ExecutionState::Parked),
        "an always-pass candidate test must lose to the protected command: {status:?}"
    );
    assert_eq!(
        origin_head(&origin),
        git_out(&origin, &["rev-parse", "HEAD"]),
        "nothing integrated"
    );
}

/// The protected runner executes behavior: a source whose *comment* contains
/// the expected text (but whose `answer()` returns 0) is rejected.
#[test]
fn comment_spoof_wrong_behavior_is_rejected() {
    let (_dir, origin, state) = seed_origin("sf-scope-spoof");
    let before = origin_head(&origin);
    let spoof = "// pub fn answer() -> u32 { 42 }\npub fn answer() -> u32 { 0 }\n";
    let mut harness = build_harness(&state, &origin, spoof, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drain(&mut harness.executor, &exec);
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked,
        "a comment spoof of the expected text must be rejected by behavior"
    );
    assert_eq!(origin_head(&origin), before);
}

/// A semantically equivalent but differently formatted fix still passes the
/// behavioral judge (execution, not text).
#[test]
fn differently_formatted_correct_behavior_is_accepted() {
    let (_dir, origin, state) = seed_origin("sf-scope-reformat");
    let before = origin_head(&origin);
    let reformatted = "pub fn answer() -> u32\n{\n    42\n}\n";
    let mut harness = build_harness(&state, &origin, reformatted, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drain(&mut harness.executor, &exec);
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Done,
        "a correct but differently formatted fix must pass"
    );
    assert_ne!(origin_head(&origin), before, "integrated");
}

#[test]
fn permitted_source_fix_passes_and_integrates_outside_child_roots() {
    let (_dir, origin, state) = seed_origin("sf-scope-fix");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drain(&mut harness.executor, &exec);
    let status = harness.executor.status(&exec).expect("status");
    assert_eq!(status.phase, ExecutionPhase::Done, "{status:?}");
    assert_ne!(origin_head(&origin), before, "integrated");

    // The protected judge lives outside `<run>` (ATTEMPT) and outside the
    // candidate tree.
    let pj = protected_root(&state, &exec);
    let attempt = agalma_conductor::phases::run_root(&state, &exec);
    assert!(pj.starts_with(&state), "protected root under state dir");
    assert!(
        !pj.starts_with(&attempt),
        "protected root must NOT lie inside the child ATTEMPT root"
    );
    assert_eq!(
        fs::read_to_string(pj.join("runner.sh")).expect("materialized"),
        PROTECTED_SCRIPT
    );

    // The verification pin also lives outside the child ATTEMPT root.
    let pin = scope_pin_path(&state, &exec);
    assert!(
        !pin.starts_with(&attempt),
        "pin outside child writable roots"
    );
    let pin: CandidatePin =
        serde_json::from_slice(&fs::read(&pin).expect("pin")).expect("pin json");
    assert_eq!(pin.candidate_sha, origin_head(&origin));
}

// ---------------------------------------------------------------------------
// Fail-closed seams
// ---------------------------------------------------------------------------

#[test]
fn missing_policy_fails_closed() {
    let (_dir, origin, state) = seed_origin("sf-scope-nopolicy");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    let mut e = entry();
    e.allowed_paths.clear();
    e.protected_paths.clear();
    build_frozen_package(&state, &exec, &e);
    drain(&mut harness.executor, &exec);
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked
    );
    assert_eq!(origin_head(&origin), before);
}

#[test]
fn invalid_policy_entry_fails_closed() {
    let (_dir, origin, state) = seed_origin("sf-scope-badpolicy");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    let mut e = entry();
    // An invalid allowlist entry refuses the whole policy on load / verify.
    e.allowed_paths.push("/absolute".to_string());
    build_frozen_package(&state, &exec, &e);
    drain(&mut harness.executor, &exec);
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked,
        "an invalid policy entry must refuse, never fail open"
    );
    assert_eq!(origin_head(&origin), before);
}

#[test]
fn invalid_frozen_entry_fails_closed_at_load() {
    // A frozen oracle that is not a valid protected entry refuses on load.
    let (_dir, origin, state) = seed_origin("sf-scope-badoracle");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package_raw(&state, &exec, b"opaque-not-an-entry".to_vec(), &entry());
    drain(&mut harness.executor, &exec);
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked
    );
    assert_eq!(origin_head(&origin), before);
}

#[test]
fn missing_protected_baseline_fails_closed() {
    let (_dir, origin, state) = seed_origin("sf-scope-nobaseline");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    // Build a package whose oracle is NOT a ProtectedEntry (opaque): refuse.
    build_frozen_package_raw(&state, &exec, b"not-a-protected-entry".to_vec(), &entry());
    drain(&mut harness.executor, &exec);
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked
    );
    assert_eq!(origin_head(&origin), before);
}

/// Persist a package with explicit oracle bytes but the entry's scope.
fn build_frozen_package_raw(
    state_dir: &Path,
    exec: &ExecutionId,
    oracle: Vec<u8>,
    entry: &ProtectedEntry,
) {
    let definition = TaskDefinition {
        task_id: TaskId::new(TASK),
        title: "SF.6 fixture".to_string(),
        description: "Body.\n".to_string(),
        design: Some("Design.".to_string()),
        notes: None,
        acceptance_criteria: Some("sh -c 'true'".to_string()),
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    };
    let inputs = PackageInputs {
        execution_id: exec.as_str().to_string(),
        generation: 1,
        lease_generation: 1,
        repository_identity: "agalma".to_string(),
        base_sha: entry.base_sha.clone(),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 0,
            digest: sha256_digest(b"directive"),
        },
        authorization: AuthorizationInput {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: sha256_digest(b"p"),
            receipt_source: "bd:x".to_string(),
            receipt_media_type: "application/json".to_string(),
            receipt_bytes: b"{}".to_vec(),
        },
        roles: vec![RoleInput::new(
            Role::Builder,
            "route.builder".to_string(),
            "model".to_string(),
            BindingPin {
                binding_id: "h".to_string(),
                api: "harness".to_string(),
                api_version: 1,
                implementation_version: "v".to_string(),
                generation: 1,
                config_digest: sha256_digest(b"{}"),
            },
            b"prompt".to_vec(),
            "text/plain",
        )],
        instructions: Vec::new(),
        shared_design: vec![ContextInput::new(
            ContextKind::SharedDesign,
            "docs/x.md",
            "text/plain",
            b"d".to_vec(),
        )],
        parent_design: Vec::new(),
        dependency_handoffs: Vec::new(),
        allowed_paths: entry.allowed_paths.clone(),
        protected_paths: entry.protected_paths.clone(),
        readable_artifacts: Vec::new(),
        oracle_bytes: oracle,
        oracle_media_type: "text/plain".to_string(),
        max_attempts: 1,
        allowance_nusd: 0,
        quota_policy_digest: sha256_digest(b"q"),
        recorded_at_unix_ms: 1,
    };
    let built = build_package(&definition, inputs).expect("build");
    persist_package(state_dir, &built).expect("persist");
}

// ---------------------------------------------------------------------------
// Integration identity at the real merge boundary
// ---------------------------------------------------------------------------

#[test]
fn committed_change_after_verify_is_refused_at_integrate() {
    let (_dir, origin, state) = seed_origin("sf-scope-committed");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drive_to(&mut harness.executor, &exec, ExecutionPhase::Integrate);
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Integrate
    );
    let checkout = read_checkout(&state, &exec);
    fs::write(Path::new(&checkout.path).join("src/extra.rs"), "// later\n").expect("write");
    run_ok("git", Path::new(&checkout.path), &["add", "-A"]);
    run_ok(
        "git",
        Path::new(&checkout.path),
        &["commit", "-qm", "post-verify"],
    );
    let _ = harness.executor.step(&exec).expect("step");
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked,
        "a committed change after verify must not ship"
    );
    assert_eq!(origin_head(&origin), before);
}

#[test]
fn dirty_worktree_after_verify_is_refused_at_integrate() {
    let (_dir, origin, state) = seed_origin("sf-scope-dirty");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drive_to(&mut harness.executor, &exec, ExecutionPhase::Integrate);
    let checkout = read_checkout(&state, &exec);
    fs::write(
        Path::new(&checkout.path).join("src/lib.rs"),
        "pub fn answer() -> u32 { 1 }\n",
    )
    .expect("write");
    let _ = harness.executor.step(&exec).expect("step");
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked
    );
    assert_eq!(origin_head(&origin), before);
}

#[test]
fn stale_or_missing_pin_is_refused_at_integrate() {
    let (_dir, origin, state) = seed_origin("sf-scope-nopin");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drive_to(&mut harness.executor, &exec, ExecutionPhase::Integrate);
    // Remove the pin after verification: integration must refuse.
    let pin = scope_pin_path(&state, &exec);
    if pin.exists() {
        fs::remove_file(&pin).expect("remove pin");
    }
    let _ = harness.executor.step(&exec).expect("step");
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked
    );
    assert_eq!(origin_head(&origin), before);
}

// ---------------------------------------------------------------------------
// Protected runner isolation (real sandbox)
// ---------------------------------------------------------------------------

#[test]
fn protected_runner_runs_in_attempt_root() {
    // Sandbox removal (design §7.1): the protected root is no longer
    // OS-enforced read-only. The runner still executes and the frozen judge
    // still judges behavior; the read-only property is convention-only now.
    let dir = ScratchDir::new("sf-scope-isolation");
    // Canonicalize the runtime root: production `canonical_dir` does the same.
    let attempt = fs::canonicalize(dir.child("attempt")).expect("attempt dir");
    let protected = fs::canonicalize(dir.child("protected")).expect("protected dir");
    fs::write(protected.join("judge.sh"), PROTECTED_SCRIPT).expect("judge");

    let script = format!(
        "echo inside > {attempt}/inside.txt; echo done > {attempt}/marker",
        attempt = attempt.display(),
    );
    let spec = ProcessSpec {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), script],
        cwd: attempt.to_string_lossy().into_owned(),
        env: BTreeMap::new(),
        attempt: AttemptId::derive(1),
    };
    let mut launcher = LocalLauncher::new();
    let output = launcher
        .run(&spec, Duration::from_secs(20))
        .expect("run");
    assert_eq!(output.exit_code, 0, "runner exit: {}", output.stderr);

    assert!(
        attempt.join("inside.txt").exists(),
        "the candidate/attempt root is writable"
    );
    assert!(
        attempt.join("marker").exists(),
        "the run completed inside its runtime root"
    );
}

// ---------------------------------------------------------------------------
// Operator manifest + pure policy
// ---------------------------------------------------------------------------

#[test]
fn protected_inputs_manifest_validation() {
    let manifest = serde_json::to_vec(&json!({
        "format": PROTECTED_INPUTS_FORMAT,
        "entries": [serde_json::to_value(entry()).unwrap()],
    }))
    .expect("json");
    let parsed = ProtectedInputs::parse(&manifest).expect("valid manifest");
    parsed
        .select(TASK, "/origin", BASE_SHA)
        .expect("select matches");
    assert!(parsed.select("other.task", "/origin", BASE_SHA).is_err());
    assert!(parsed.select(TASK, "/other", BASE_SHA).is_err());
    assert!(parsed.select(TASK, "/origin", "different").is_err());

    // Unknown field refuses.
    let bad = serde_json::to_vec(&json!({
        "format": PROTECTED_INPUTS_FORMAT,
        "entries": [serde_json::to_value(entry()).unwrap()],
        "extra": 1,
    }))
    .expect("json");
    assert!(ProtectedInputs::parse(&bad).is_err());

    // Duplicate task entries refuse.
    let dup = serde_json::to_vec(&json!({
        "format": PROTECTED_INPUTS_FORMAT,
        "entries": [serde_json::to_value(entry()).unwrap(), serde_json::to_value(entry()).unwrap()],
    }))
    .expect("json");
    assert!(ProtectedInputs::parse(&dup).is_err());
}

#[test]
fn missing_manifest_refuses_load() {
    let dir = ScratchDir::new("sf-scope-nomanifest");
    assert!(
        ProtectedInputs::load(&dir).is_err(),
        "an absent operator manifest must refuse repository work"
    );
}

#[test]
fn policy_refuses_invalid_entries_and_preserves_whitespace() {
    assert_eq!(scope_guard::normalize_path("a/b ").unwrap(), "a/b ");
    assert!(scope_guard::normalize_path("/abs").is_err());
    assert!(scope_guard::normalize_path("a/../b").is_err());
    assert!(scope_guard::normalize_path("a//b").is_err());
    let scope = Scope {
        allowed_paths: vec!["src".to_string()],
        protected_paths: vec!["/bad".to_string()],
        readable_artifacts: Vec::new(),
        oracle: scope_guard::dummy_artifact(),
    };
    assert!(ScopePolicy::from_scope(&scope).is_err());
}

#[test]
fn real_symlink_escape_is_refused() {
    let dir = ScratchDir::new("sf-scope-symlink");
    let root = dir.child("candidate");
    fs::create_dir_all(root.join("src")).expect("src");
    std::os::unix::fs::symlink("/etc", root.join("escape")).expect("symlink");
    std::os::unix::fs::symlink("../../..", root.join("up")).expect("symlink up");
    let scope = Scope {
        allowed_paths: vec!["src".to_string(), "escape".to_string(), "up".to_string()],
        protected_paths: Vec::new(),
        readable_artifacts: Vec::new(),
        oracle: scope_guard::dummy_artifact(),
    };
    let policy = ScopePolicy::from_scope(&scope).expect("policy");
    for path in ["escape/passwd", "up/outside.rs"] {
        let escape = FileChange {
            kind: ChangeKind::Added,
            path: path.to_string(),
            old_path: None,
            old_mode: None,
            new_mode: None,
            symlink_target: None,
        };
        let report = policy.authorize_in_tree(&[escape], &root);
        assert!(report.is_denied(), "`{path}` must be refused");
        assert_eq!(report.denied[0].code, "symlink_escape");
    }
}

/// Deterministic symlink-authorization property (seed `0x5EED_F6_2026`,
/// budget 256 cases) over fixture-known classifications:
/// absolute external dangling, relative external dangling, internal safe,
/// chained external, loop, legitimate deleted internal, and safe new internal.
/// Expected authorization is derived from the classification, not the
/// implementation. Meaningful filenames (spaces, non-ASCII) are exercised.
#[test]
fn symlink_authorization_property_over_generated_cases() {
    #[derive(Clone, Copy)]
    enum Kind {
        AbsExt,
        RelExt,
        IntOk,
        ChainExt,
        Loop,
        DeletedInternal,
        SafeNew,
    }
    const CASES: usize = 256;
    const SEED: u64 = 0x5EED_F6_2026;

    let dir = ScratchDir::new("sf-scope-symprop");
    let root = dir.child("candidate");
    fs::create_dir_all(root.join("src")).expect("src");
    // Absolute external dangling target (the case that drove this repair).
    std::os::unix::fs::symlink("/nonexistent-f6-abs-target", root.join("abs_ext")).expect("abs");
    // Relative external dangling target.
    std::os::unix::fs::symlink("../../nonexistent-f6-rel", root.join("rel_ext")).expect("rel");
    // Internal safe directory link.
    std::os::unix::fs::symlink("src", root.join("int_ok")).expect("int");
    // Chained external (link -> external link).
    std::os::unix::fs::symlink("abs_ext", root.join("chain_ext")).expect("chain");
    // Loop.
    std::os::unix::fs::symlink("loop_b", root.join("loop_a")).expect("loop_a");
    std::os::unix::fs::symlink("loop_a", root.join("loop_b")).expect("loop_b");

    let scope = Scope {
        allowed_paths: vec![
            "src".to_string(),
            "abs_ext".to_string(),
            "rel_ext".to_string(),
            "int_ok".to_string(),
            "chain_ext".to_string(),
            "loop_a".to_string(),
            "loop_b".to_string(),
        ],
        protected_paths: Vec::new(),
        readable_artifacts: Vec::new(),
        oracle: scope_guard::dummy_artifact(),
    };
    let policy = ScopePolicy::from_scope(&scope).expect("policy");

    let kinds = [
        Kind::AbsExt,
        Kind::RelExt,
        Kind::IntOk,
        Kind::ChainExt,
        Kind::Loop,
        Kind::DeletedInternal,
        Kind::SafeNew,
    ];
    // Bounded path/depth variation with meaningful filenames.
    let tails = [
        "",
        "a.rs",
        "d/e.rs",
        "file name.rs",
        "ünicode.rs",
        "x/y/z.rs",
    ];
    let mut rng = Rng(SEED);
    let mut counts = [0usize; 7];
    for _ in 0..CASES {
        let k = rng.below(kinds.len() as u64) as usize;
        counts[k] += 1;
        let tail = tails[rng.below(tails.len() as u64) as usize];
        let join = |base: &str| {
            if tail.is_empty() {
                base.to_string()
            } else {
                format!("{base}/{tail}")
            }
        };
        let (path, kind, refuse) = match kinds[k] {
            Kind::AbsExt => (join("abs_ext"), ChangeKind::Added, true),
            Kind::RelExt => (join("rel_ext"), ChangeKind::Added, true),
            Kind::ChainExt => (join("chain_ext"), ChangeKind::Added, true),
            Kind::Loop => (join("loop_a"), ChangeKind::Added, true),
            Kind::IntOk => (join("int_ok"), ChangeKind::Added, false),
            Kind::SafeNew => (join("src/new_file"), ChangeKind::Added, false),
            Kind::DeletedInternal => (join("src/deleted_file"), ChangeKind::Deleted, false),
        };
        let change = FileChange {
            kind,
            path: path.clone(),
            old_path: if matches!(kind, ChangeKind::Deleted) {
                Some(path.clone())
            } else {
                None
            },
            old_mode: None,
            new_mode: None,
            symlink_target: None,
        };
        let report = policy.authorize_in_tree(&[change], &root);
        assert_eq!(
            report.is_denied(),
            refuse,
            "case {path:?} (seed {SEED}) expected refuse={refuse}"
        );
        if refuse {
            assert_eq!(
                report.denied[0].code, "symlink_escape",
                "refusal for {path:?} must be symlink_escape"
            );
        }
    }
    assert!(
        counts.iter().all(|&c| c > 0),
        "every classification must be exercised: {counts:?}"
    );
}

#[test]
fn candidate_pin_revalidates_every_identity_field() {
    let pin = CandidatePin {
        execution_id: "exec:x:1".to_string(),
        generation: 1,
        package_digest: sha256_digest(b"pkg"),
        attempt: 1,
        candidate_sha: "aaa".to_string(),
        oracle_digest: sha256_digest(b"oracle"),
    };
    let ok = scope_guard::PinIdentity {
        execution_id: "exec:x:1",
        generation: 1,
        package_digest: &pin.package_digest,
        candidate_sha: "aaa",
        oracle_digest: &pin.oracle_digest,
    };
    pin.revalidate(&ok).expect("unchanged");
    let bad_sha = scope_guard::PinIdentity {
        candidate_sha: "bbb",
        ..ok
    };
    assert_eq!(
        failure_code(&pin.revalidate(&bad_sha).unwrap_err()),
        Some(FailureCode::BaselineChanged)
    );
}

#[test]
fn invalid_baseline_refuses() {
    // No commands.
    let mut b = baseline();
    b.commands.clear();
    assert!(b.validate().is_err());
    // Unsupported mode.
    let mut b = baseline();
    b.files[0].mode = "100777".to_string();
    assert!(b.validate().is_err());
    // Integrity mismatch.
    let mut b = baseline();
    b.files[0].text = "tampered".to_string();
    assert!(b.validate().is_err());
}

#[test]
fn production_verifier_command_runs_with_writable_candidate_root() {
    let (_dir, origin, state) = seed_origin("sf-scope-malicious");
    let mut harness = build_harness_live(&state, &origin, FIXED_SOURCE);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    // The frozen command writes a witness into the candidate-root and exits 0,
    // so verify stays green and no live verifier session is needed.
    //
    // Sandbox removal (design §7.1): the verifier's ability to overwrite parent
    // receipt markers is no longer OS-enforced, so this test no longer asserts
    // marker immutability — an accepted guarantee reduction. It still asserts
    // the behavioral judge runs with a writable candidate root.
    let acceptance =
        "sh -c 'echo ok > $AGALMA_CANDIDATE_DIR/witness.txt; exit 0'".to_string();
    build_frozen_package_with(&state, &exec, &entry(), &acceptance);
    drain(&mut harness.executor, &exec);
    let witness = agalma_conductor::phases::run_root(&state, &exec)
        .join("verify-checkout")
        .join("witness.txt");
    assert!(
        witness.exists(),
        "the verifier command must have run with a writable candidate root"
    );
}

#[test]
fn altered_pin_attempt_is_refused_at_integrate() {
    let (_dir, origin, state) = seed_origin("sf-scope-attempt");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drive_to(&mut harness.executor, &exec, ExecutionPhase::Integrate);
    // Rewrite the pin's attempt to one with no durable green verdict.
    let pin_path = scope_pin_path(&state, &exec);
    let mut pin: CandidatePin =
        serde_json::from_slice(&fs::read(&pin_path).expect("pin")).expect("pin json");
    pin.attempt = 99;
    fs::write(&pin_path, serde_json::to_vec(&pin).expect("json")).expect("rewrite pin");
    let _ = harness.executor.step(&exec).expect("step");
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked,
        "a pin attempt with no durable green verdict must be refused"
    );
    assert_eq!(origin_head(&origin), before);
}

#[test]
fn baseline_rejects_conflicting_inventory() {
    // File/directory (ancestor) conflict.
    let mut b = baseline();
    b.files.push(BaselineFile {
        path: "runner.sh/nested".to_string(),
        mode: "100644".to_string(),
        digest: sha256_digest(b"x"),
        bytes: 1,
        text: "x".to_string(),
    });
    assert!(b.validate().is_err(), "ancestor conflict must be rejected");

    // Duplicate normalized destination.
    let mut b = baseline();
    b.files.push(b.files[0].clone());
    assert!(
        b.validate().is_err(),
        "duplicate normalized path must be rejected"
    );
}

#[test]
fn protected_executable_runner_is_materialized_executable() {
    let dir = ScratchDir::new("sf-scope-mode");
    let target = dir.child("protected");
    scope_guard::materialize_baseline(&target, &baseline()).expect("materialize");
    let meta = fs::metadata(target.join("runner.sh")).expect("meta");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        meta.permissions().mode() & 0o111,
        0o111,
        "the declared 100755 mode must be applied"
    );
}

// ---------------------------------------------------------------------------
// Review findings: verdict binding, gitlink, modes, child-root separation
// ---------------------------------------------------------------------------

fn green_verdict(
    candidate: &str,
    oracle: &str,
    attempt: u32,
    next: ActivityNext,
) -> ActivityOutcome {
    ActivityOutcome {
        result: json!({
            "status": "green", "exit_code": 0, "attempt": attempt,
            "candidate_sha": candidate, "oracle_digest": oracle,
        }),
        next,
    }
}

fn verdict_pin() -> CandidatePin {
    CandidatePin {
        execution_id: "exec:x:1".to_string(),
        generation: 1,
        package_digest: sha256_digest(b"pkg"),
        attempt: 1,
        candidate_sha: "candidate-a".to_string(),
        oracle_digest: format!("sha256:{}", "a".repeat(64)),
    }
}

#[test]
fn verify_verdict_binding_requires_matching_identity() {
    let pin = verdict_pin();
    let ok = green_verdict(
        &pin.candidate_sha,
        &pin.oracle_digest,
        pin.attempt,
        ActivityNext::Advance,
    );
    verify_verdict_matches_pin(&pin, &ok).expect("matching green verdict");

    for (label, v) in [
        (
            "red status",
            ActivityOutcome {
                result: json!({"status":"red","exit_code":1,"attempt":1,"candidate_sha":pin.candidate_sha,"oracle_digest":pin.oracle_digest}),
                next: ActivityNext::Retry {
                    failure: ArtifactRef::derive("x"),
                },
            },
        ),
        (
            "missing status",
            ActivityOutcome {
                result: json!({"exit_code":0,"attempt":1,"candidate_sha":pin.candidate_sha,"oracle_digest":pin.oracle_digest}),
                next: ActivityNext::Advance,
            },
        ),
        (
            "nonzero exit",
            ActivityOutcome {
                result: json!({"status":"green","exit_code":1,"attempt":1,"candidate_sha":pin.candidate_sha,"oracle_digest":pin.oracle_digest}),
                next: ActivityNext::Advance,
            },
        ),
        (
            "wrong attempt",
            green_verdict(
                &pin.candidate_sha,
                &pin.oracle_digest,
                9,
                ActivityNext::Advance,
            ),
        ),
        (
            "wrong candidate",
            green_verdict("other", &pin.oracle_digest, 1, ActivityNext::Advance),
        ),
        (
            "wrong oracle",
            green_verdict(
                &pin.candidate_sha,
                &format!("sha256:{}", "b".repeat(64)),
                1,
                ActivityNext::Advance,
            ),
        ),
        (
            "non-advancing",
            green_verdict(
                &pin.candidate_sha,
                &pin.oracle_digest,
                1,
                ActivityNext::Retry {
                    failure: ArtifactRef::derive("x"),
                },
            ),
        ),
    ] {
        assert!(
            verify_verdict_matches_pin(&pin, &v).is_err(),
            "{label} must be refused"
        );
    }
}

#[test]
fn verify_verdict_binding_property() {
    let pin = verdict_pin();
    let mut rng = Rng(0xC0FFEE);
    let mut ok_seen = 0usize;
    for _ in 0..256 {
        let status = if rng.below(2) == 0 { "green" } else { "red" };
        let exit = rng.below(2) as i64;
        let attempt = 1 + rng.below(3) as u64;
        let candidate = if rng.below(2) == 0 {
            "candidate-a"
        } else {
            "candidate-b"
        };
        let oracle = if rng.below(2) == 0 {
            pin.oracle_digest.clone()
        } else {
            format!("sha256:{}", "b".repeat(64))
        };
        let next = if rng.below(4) == 0 {
            ActivityNext::Retry {
                failure: ArtifactRef::derive("x"),
            }
        } else {
            ActivityNext::Advance
        };
        let advancing = matches!(next, ActivityNext::Advance);
        let outcome = ActivityOutcome {
            result: json!({ "status": status, "exit_code": exit, "attempt": attempt,
                            "candidate_sha": candidate, "oracle_digest": oracle }),
            next,
        };
        let expected = advancing
            && status == "green"
            && exit == 0
            && attempt == pin.attempt as u64
            && candidate == pin.candidate_sha
            && oracle == pin.oracle_digest;
        assert_eq!(
            verify_verdict_matches_pin(&pin, &outcome).is_ok(),
            expected,
            "predicate must match the contract for {outcome:?}"
        );
        if expected {
            ok_seen += 1;
        }
    }
    assert!(
        ok_seen > 0,
        "generated cases must include a matching verdict"
    );
}

#[test]
fn green_but_mismatched_verdict_is_refused_at_integrate() {
    let (_dir, origin, state) = seed_origin("sf-scope-verdict");
    let before = origin_head(&origin);
    let mut harness = build_harness(&state, &origin, FIXED_SOURCE, None);
    let exec = harness.executor.start(&TaskId::new(TASK)).expect("start");
    build_frozen_package(&state, &exec, &entry());
    drive_to(&mut harness.executor, &exec, ExecutionPhase::Integrate);
    // Overwrite the durable verify verdict with a *green* outcome for a different
    // candidate: only the real successful verdict may satisfy the pin.
    let tampered = green_verdict(
        "different-candidate",
        &format!("sha256:{}", "b".repeat(64)),
        1,
        ActivityNext::Advance,
    );
    agalma_conductor::workers::write_done(&state, &exec, "verify", 1, &tampered)
        .expect("tamper verdict");
    let _ = harness.executor.step(&exec).expect("step");
    assert_eq!(
        harness.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Parked,
        "a green verdict for a different candidate/oracle must not satisfy the pin"
    );
    assert_eq!(origin_head(&origin), before);
}

/// A real Git gitlink (nested repo/submodule) at an allowed path is refused as
/// nested_repository — the `.git` segment test alone would miss it.
#[test]
fn git_gitlink_change_is_nested_repository() {
    let (_dir, origin, state) = seed_origin("sf-scope-gitlink");
    let mut workspace = agalma_conductor::workspace_process::WorkspaceHandle::in_process(&state);
    // Prepare an ordinary checkout, then add a nested repository and commit it
    // so Git records a gitlink (mode 160000).
    let checkout = workspace
        .prepare_repo(&origin, "main", &ExecutionId::new("exec:gitlink:1"), &state)
        .expect("prepare");
    let co = Path::new(&checkout.path);
    let sub = co.join("subrepo");
    fs::create_dir_all(&sub).expect("subrepo");
    run_ok("git", &sub, &["init", "-q", "-b", "main"]);
    fs::write(sub.join("f"), b"x").expect("f");
    run_ok("git", &sub, &["add", "-A"]);
    run_ok(
        "git",
        &sub,
        &[
            "-c",
            "user.email=a@b",
            "-c",
            "user.name=a",
            "commit",
            "-qm",
            "x",
        ],
    );
    run_ok("git", co, &["add", "-A"]);
    let sha = workspace.commit_candidate(&checkout).expect("commit");
    assert!(!sha.is_empty());

    let changes = workspace.changes(&checkout).expect("changes");
    let subrepo = changes
        .iter()
        .find(|c| c.path == "subrepo")
        .expect("gitlink change reported");
    assert_eq!(subrepo.new_mode.as_deref(), Some("160000"), "gitlink mode");

    let scope = Scope {
        allowed_paths: entry().allowed_paths,
        protected_paths: entry().protected_paths,
        readable_artifacts: Vec::new(),
        oracle: scope_guard::dummy_artifact(),
    };
    let policy = ScopePolicy::from_scope(&scope).expect("policy");
    let report = policy.authorize(&[subrepo.clone()]);
    assert!(report.is_denied(), "a gitlink must be refused");
    assert_eq!(report.denied[0].code, "nested_repository");
}

#[test]
fn protected_executable_runner_executes_directly() {
    let dir = ScratchDir::new("sf-scope-exec");
    let target = dir.child("protected");
    let script = "#!/bin/sh\nexit 0\n";
    let b = ProtectedBaseline {
        format: PROTECTED_BASELINE_FORMAT.to_string(),
        files: vec![BaselineFile {
            path: "runner.sh".to_string(),
            mode: "100755".to_string(),
            digest: sha256_digest(script.as_bytes()),
            bytes: script.len() as u64,
            text: script.to_string(),
        }],
        commands: vec!["sh $AGALMA_PROTECTED_DIR/runner.sh".to_string()],
    };
    scope_guard::materialize_baseline(&target, &b).expect("materialize");
    let runner = target.join("runner.sh");
    let status = Command::new(&runner)
        .status()
        .expect("spawn protected runner");
    assert!(
        status.success(),
        "the materialized 100755 runner must execute directly"
    );
}

/// The builder and diagnosis child roots are narrow runtime roots; no parent
/// phase marker or receipt lives inside any granted child root.
#[test]
fn child_runtime_roots_exclude_parent_markers() {
    let dir = ScratchDir::new("sf-scope-roots");
    let state = dir.child("state");
    let exec = ExecutionId::new("exec:roots:1");
    let build_rt = builder_runtime_dir(&state, &exec);
    let verify_rt = verify_runtime_dir(&state, &exec);
    // SF.6 narrowed the builder and verifier ATTEMPT roots so NO parent phase
    // marker/receipt lies inside them.
    for phase in ["intake", "checkout", "build", "verify", "integrate"] {
        for marker in [
            agalma_conductor::workers::started_path(&state, &exec, phase, 1),
            agalma_conductor::workers::done_path(&state, &exec, phase, 1),
            agalma_conductor::workers::worker_path(&state, &exec, phase, 1),
        ] {
            assert!(
                !marker.starts_with(&build_rt) && !marker.starts_with(&verify_rt),
                "parent marker {} must not be inside a narrowed child ATTEMPT root",
                marker.display()
            );
        }
    }
    // The narrowed roots are distinct from each other and outside the run root's
    // phase-marker dirs by name.
    assert_ne!(build_rt, verify_rt);
}

/// A minimal `PhaseDeps` for config-construction tests (no repo/live session).
fn test_deps(state_dir: &Path, origin: &Path) -> PhaseDeps {
    PhaseDeps {
        state_dir: state_dir.to_path_buf(),
        fixture: origin.to_path_buf(),
        model: "test/offline".to_string(),
        verifier_model: "test/offline".to_string(),
        route: "builder:fix".to_string(),
        verifier_route: "verifier:fix".to_string(),
        role: "builder".to_string(),
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        verifier_proxy_url: None,
        paid_proxy: false,
        toolchain_env: BTreeMap::new(),
        turn_timeout: Duration::from_secs(1),
        verify_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(1),
        repo: None,
        harness: harness_handle(HarnessChoice::OpenCode),
        stop: Rc::new(FileStopControl::new(state_dir)),
    }
}

fn config_checkout(state: &Path, exec: &ExecutionId) -> Checkout {
    let dir = agalma_conductor::phases::run_root(state, exec)
        .join("checkout")
        .to_string_lossy()
        .into_owned();
    Checkout {
        path: dir,
        base_sha: "b".repeat(40),
        candidate_branch: format!("candidate/{}", agalma_workspace::execution_dir_name(exec)),
    }
}

/// SF.6: the ACTUAL production builder and diagnosis configs are narrow (each
/// gets its own runtime root), and no parent marker lives inside a child
/// runtime.
#[test]
fn production_child_configs_are_narrow() {
    let dir = ScratchDir::new("sf-scope-cfg");
    let state = dir.child("state");
    let origin = dir.child("origin");
    fs::create_dir_all(&origin).expect("origin");
    let deps = test_deps(&state, &origin);
    let exec = ExecutionId::new("exec:cfg:1");
    let checkout = config_checkout(&state, &exec);

    let b = builder_config_init(&deps, &checkout, &exec).expect("builder config");
    let v = verifier_config_init(&deps, &checkout, &exec).expect("verifier config");
    let brt = fs::canonicalize(builder_runtime_dir(&state, &exec)).expect("build rt");
    let vrt = fs::canonicalize(verify_runtime_dir(&state, &exec)).expect("verify rt");

    assert_eq!(b.attempt_dir, brt, "builder runtime is the narrow runtime");
    assert_eq!(
        v.attempt_dir, vrt,
        "diagnosis runtime is the narrow runtime"
    );
    assert_ne!(brt, vrt, "build and verify runtimes are distinct");

    for phase in ["build", "verify"] {
        let marker = agalma_conductor::workers::done_path(&state, &exec, phase, 1);
        assert!(!marker.starts_with(&brt) && !marker.starts_with(&vrt));
    }
}

/// SF.6: the production diagnosis config's runtime root is writable and the
/// frozen diagnosis inputs (under the run root) are readable. Read-only
/// protection of the protected root is convention-only after sandbox removal
/// (design §7.1).
#[test]
fn production_verifier_runtime_reads_diagnosis_inputs() {
    let dir = ScratchDir::new("sf-scope-cfgprobe");
    let state = dir.child("state");
    let origin = dir.child("origin");
    fs::create_dir_all(&origin).expect("origin");
    let deps = test_deps(&state, &origin);
    let exec = ExecutionId::new("exec:cfgprobe:1");
    let checkout = config_checkout(&state, &exec);
    let v = verifier_config_init(&deps, &checkout, &exec).expect("verifier config");

    let run = agalma_conductor::phases::run_root(&state, &exec);
    fs::create_dir_all(run.join("artifacts")).expect("artifacts");
    fs::write(run.join("artifacts").join("diff@1.patch"), b"DIFFCONTENT").expect("diff");

    let attempt = &v.attempt_dir;
    let script = format!(
        "cat {p}/artifacts/diff@1.patch > {a}/read_proof; \
         echo ok > {a}/write_proof; exit 0",
        p = run.display(),
        a = attempt.display(),
    );
    let spec = ProcessSpec {
        program: "sh".to_string(),
        args: vec!["-c".to_string(), script],
        cwd: attempt.to_string_lossy().into_owned(),
        env: BTreeMap::new(),
        attempt: AttemptId::derive(1),
    };
    let mut launcher = LocalLauncher::new();
    let output = launcher
        .run(&spec, Duration::from_secs(20))
        .expect("run");
    assert_eq!(output.exit_code, 0, "runner exit: {}", output.stderr);

    assert!(
        attempt.join("read_proof").exists(),
        "the diagnosis input must be readable"
    );
    assert_eq!(
        fs::read_to_string(attempt.join("read_proof")).expect("read proof"),
        "DIFFCONTENT"
    );
    assert!(attempt.join("write_proof").exists(), "runtime root is writable");
}

// Generated-input invariants (>= 128 cases).
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

const PATHS: [&str; 6] = [
    "src/a.rs",
    "src/b.rs",
    "tests/t.rs",
    "tests/protected_check.sh",
    "docs/d.md",
    "vendor/x.rs",
];

fn under(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

#[cfg(unix)]
mod xdg_config {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn non_unicode_path() -> OsString {
        OsString::from_vec(b"/config/\xff".to_vec())
    }

    fn xdg_states() -> Vec<Option<OsString>> {
        vec![
            None,
            Some(OsString::new()),
            Some(OsString::from("relative/config")),
            Some(OsString::from("/xdg/config")),
            Some(OsString::from("/xdg/config with space")),
            Some(non_unicode_path()),
        ]
    }

    fn home_states() -> Vec<Option<OsString>> {
        vec![
            None,
            Some(OsString::new()),
            Some(OsString::from("relative/home")),
            Some(OsString::from("/home/operator")),
            Some(OsString::from("/home/operator with space")),
            Some(non_unicode_path()),
        ]
    }

    /// Contract predicate, not a path oracle: a base is usable only when it is
    /// non-empty, valid Unicode, and absolute.
    fn usable_abs(value: Option<&OsStr>) -> Option<&str> {
        let text = value?.to_str()?;
        if text.is_empty() || !Path::new(text).is_absolute() {
            return None;
        }
        Some(text)
    }

    /// A non-empty, non-Unicode XDG value is a hard refusal even when HOME is
    /// usable; it is never guessed or silently ignored.
    fn xdg_non_unicode(value: Option<&OsStr>) -> bool {
        value
            .filter(|raw| !raw.is_empty())
            .map(|raw| raw.to_str().is_none())
            .unwrap_or(false)
    }

    fn config_file_under(base: &Path) -> PathBuf {
        base.join(PROTECTED_CONFIG_DIR)
            .join(PROTECTED_INPUTS_FILE)
    }

    fn manifest_bytes() -> Vec<u8> {
        serde_json::to_vec(&ProtectedInputs {
            format: PROTECTED_INPUTS_FORMAT.to_string(),
            entries: vec![entry()],
        })
        .expect("manifest bytes")
    }

    /// Explicit 6x6 contract truth table over the representative XDG/HOME
    /// states. Expectations are literals; this is not a reimplementation of the
    /// resolver branches.
    #[test]
    fn config_path_contract_truth_table() {
        let xdg = xdg_states();
        let home = home_states();
        let xdg_path = "/xdg/config/agalma/protected-inputs.json";
        let xdg_space = "/xdg/config with space/agalma/protected-inputs.json";
        let home_path = "/home/operator/.config/agalma/protected-inputs.json";
        let home_space = "/home/operator with space/.config/agalma/protected-inputs.json";

        // (xdg index, home index, expected literal or None for refusal)
        let table: &[(usize, usize, Option<&str>)] = &[
            (0, 0, None),
            (0, 1, None),
            (0, 2, None),
            (0, 3, Some(home_path)),
            (0, 4, Some(home_space)),
            (0, 5, None),
            (1, 0, None),
            (1, 1, None),
            (1, 2, None),
            (1, 3, Some(home_path)),
            (1, 4, Some(home_space)),
            (1, 5, None),
            (2, 0, None),
            (2, 1, None),
            (2, 2, None),
            (2, 3, Some(home_path)),
            (2, 4, Some(home_space)),
            (2, 5, None),
            (3, 0, Some(xdg_path)),
            (3, 1, Some(xdg_path)),
            (3, 2, Some(xdg_path)),
            (3, 3, Some(xdg_path)),
            (3, 4, Some(xdg_path)),
            (3, 5, Some(xdg_path)),
            (4, 0, Some(xdg_space)),
            (4, 1, Some(xdg_space)),
            (4, 2, Some(xdg_space)),
            (4, 3, Some(xdg_space)),
            (4, 4, Some(xdg_space)),
            (4, 5, Some(xdg_space)),
            (5, 0, None),
            (5, 1, None),
            (5, 2, None),
            (5, 3, None),
            (5, 4, None),
            (5, 5, None),
        ];
        assert_eq!(table.len(), 36, "complete 6x6 state table");
        for &(xi, hi, want) in table {
            let got = resolve_config_path_from(xdg[xi].as_deref(), home[hi].as_deref());
            match (want, got) {
                (Some(want), Ok(got)) => assert_eq!(got, PathBuf::from(want)),
                (None, Err(err)) => {
                    assert_eq!(failure_code(&err), Some(FailureCode::ScopeDenied));
                    assert!(format!("{err}").contains(PROTECTED_INPUTS_FILE));
                }
                (want, got) => {
                    panic!("truth-table mismatch xdg={xi} home={hi}: want={want:?} got={got:?}")
                }
            }
        }
    }

    /// Independent generated metamorphic checks over 160 fixed-seed states.
    /// No expected-path clone: success is classified by the contract predicates
    /// (XDG dominance, HOME fallback) and each class is checked for invariance.
    #[test]
    fn config_path_metamorphic_generated() {
        const CASES: usize = 160;
        let xdg = xdg_states();
        let home = home_states();
        let invalid_xdg: [Option<&OsStr>; 3] =
            [None, Some(OsStr::new("")), Some(OsStr::new("relative"))];
        let mut rng = Rng(0x5f6_7864_6700);
        let mut xdg_hits = 0usize;
        let mut home_hits = 0usize;
        let mut refused = 0usize;

        for case in 0..CASES {
            let x = &xdg[rng.below(xdg.len() as u64) as usize];
            let h = &home[rng.below(home.len() as u64) as usize];
            let got = resolve_config_path_from(x.as_deref(), h.as_deref());
            match got {
                Ok(path) => {
                    assert!(path.is_absolute(), "case {case}: resolved path is absolute");
                    assert_eq!(path.file_name(), Some(OsStr::new(PROTECTED_INPUTS_FILE)));
                    assert_eq!(
                        path.parent().and_then(|parent| parent.file_name()),
                        Some(OsStr::new(PROTECTED_CONFIG_DIR))
                    );

                    if let Some(base) = usable_abs(x.as_deref()) {
                        // XDG dominance: the result is exactly the XDG config
                        // path and cannot depend on any HOME value.
                        assert_eq!(path, config_file_under(Path::new(base)));
                        for alt in &home {
                            assert_eq!(
                                resolve_config_path_from(x.as_deref(), alt.as_deref())
                                    .expect("XDG dominance keeps resolution valid"),
                                path,
                                "case {case}: HOME must not affect a valid XDG"
                            );
                        }
                        xdg_hits += 1;
                    } else {
                        // HOME fallback: only an absolute HOME can reach here.
                        let base = usable_abs(h.as_deref()).expect("fallback has usable HOME");
                        assert_eq!(
                            path,
                            Path::new(base)
                                .join(".config")
                                .join(PROTECTED_CONFIG_DIR)
                                .join(PROTECTED_INPUTS_FILE)
                        );
                        for alt in invalid_xdg {
                            assert_eq!(
                                resolve_config_path_from(alt, h.as_deref())
                                    .expect("invalid XDG falls back to usable HOME"),
                                path,
                                "case {case}: invalid XDG cannot change HOME fallback"
                            );
                        }
                        home_hits += 1;
                    }
                }
                Err(err) => {
                    assert_eq!(failure_code(&err), Some(FailureCode::ScopeDenied));
                    assert!(
                        format!("{err}").contains(PROTECTED_INPUTS_FILE),
                        "case {case}: refusal names the config file"
                    );
                    assert!(
                        xdg_non_unicode(x.as_deref())
                            || (usable_abs(x.as_deref()).is_none()
                                && usable_abs(h.as_deref()).is_none()),
                        "case {case}: only unusable bases refuse (xdg={x:?}, home={h:?})"
                    );
                    refused += 1;
                }
            }
        }
        assert!(xdg_hits > 0, "generated cases exercise XDG dominance");
        assert!(home_hits > 0, "generated cases exercise HOME fallback");
        assert!(refused > 0, "generated cases exercise fail-closed");
    }

    #[test]
    fn explicit_config_path_cases() {
        let abs = OsStr::new("/xdg/config with space");
        let home = OsStr::new("/home/operator");
        assert_eq!(
            resolve_config_path_from(Some(abs), None).unwrap(),
            PathBuf::from("/xdg/config with space/agalma/protected-inputs.json")
        );
        assert_eq!(
            resolve_config_path_from(Some(OsStr::new("")), Some(home)).unwrap(),
            PathBuf::from("/home/operator/.config/agalma/protected-inputs.json")
        );
        assert_eq!(
            resolve_config_path_from(Some(OsStr::new("relative")), Some(home)).unwrap(),
            PathBuf::from("/home/operator/.config/agalma/protected-inputs.json")
        );
        assert!(resolve_config_path_from(None, None).is_err());
        assert!(resolve_config_path_from(Some(OsStr::new("relative")), None).is_err());
        assert!(resolve_config_path_from(None, Some(OsStr::new(""))).is_err());
        assert!(resolve_config_path_from(None, Some(OsStr::new("relative"))).is_err());
        assert!(
            resolve_config_path_from(Some(non_unicode_path().as_os_str()), Some(home)).is_err()
        );
        assert!(resolve_config_path_from(Some(abs), Some(non_unicode_path().as_os_str())).is_ok());
    }

    #[test]
    fn config_policy_never_falls_back_or_merges_generated() {
        const CASES: usize = 144;
        let dir = ScratchDir::new("sf6-xdg-policy");
        let good = manifest_bytes();
        let bad = b"{ not a protected manifest".to_vec();
        let mut rng = Rng(0x5f6_7864_706f);

        for case in 0..CASES {
            let root = dir.child(&format!("case-{case}"));
            let config_home = root.join("xdg");
            let state_dir = root.join("state");
            fs::create_dir_all(&config_home).expect("config home");
            fs::create_dir_all(&state_dir).expect("state dir");
            let config_file = config_home
                .join(PROTECTED_CONFIG_DIR)
                .join(PROTECTED_INPUTS_FILE);
            let legacy_file = state_dir.join(PROTECTED_INPUTS_FILE);

            // Independently choose config/legacy states; a valid config must
            // win, and the legacy file must never change that outcome.
            let config_valid = rng.below(3) == 0;
            let config_malformed = rng.below(3) == 0;
            let legacy_valid = rng.below(3) != 0;
            let legacy_malformed = !legacy_valid && rng.below(2) == 0;
            if config_valid {
                fs::create_dir_all(config_file.parent().unwrap()).expect("config dir");
                fs::write(&config_file, &good).expect("write config");
            } else if config_malformed {
                fs::create_dir_all(config_file.parent().unwrap()).expect("config dir");
                fs::write(&config_file, &bad).expect("write malformed config");
            }
            if legacy_valid {
                fs::write(&legacy_file, &good).expect("write legacy");
            } else if legacy_malformed {
                fs::write(&legacy_file, &bad).expect("write malformed legacy");
            }

            let loaded = load_config_from(
                Some(config_home.as_os_str()),
                Some(OsStr::new("/unused-home")),
            );
            if config_valid {
                assert_eq!(
                    loaded.as_ref().expect("valid config loads").entries,
                    vec![entry()]
                );
            } else {
                assert!(
                    loaded.is_err(),
                    "missing/malformed selected config must refuse (legacy_valid={legacy_valid})"
                );
            }

            // Independent no-state-fallback metamorph: removing the legacy
            // state manifest cannot change the selected-config outcome.
            let _ = fs::remove_file(&legacy_file);
            let after = load_config_from(
                Some(config_home.as_os_str()),
                Some(OsStr::new("/unused-home")),
            );
            assert_eq!(
                loaded.is_ok(),
                after.is_ok(),
                "case {case}: legacy state file is inert"
            );
            if let Ok(before) = loaded {
                assert_eq!(before.entries, after.expect("same config still loads").entries);
            }
        }
    }

    #[test]
    fn legacy_state_manifest_is_context_only() {
        let dir = ScratchDir::new("sf6-xdg-legacy");
        let config_home = dir.child("xdg");
        let state_dir = dir.child("state");
        fs::create_dir_all(&config_home).expect("config home");
        fs::create_dir_all(&state_dir).expect("state dir");
        fs::write(state_dir.join(PROTECTED_INPUTS_FILE), manifest_bytes())
            .expect("legacy manifest");

        // A valid legacy state manifest alone does not admit work.
        assert!(load_config_from(Some(config_home.as_os_str()), None).is_err());

        // A malformed selected config refuses even with a valid legacy file.
        let config_file = config_home
            .join(PROTECTED_CONFIG_DIR)
            .join(PROTECTED_INPUTS_FILE);
        fs::create_dir_all(config_file.parent().unwrap()).expect("config dir");
        fs::write(&config_file, b"not json").expect("malformed config");
        assert!(load_config_from(Some(config_home.as_os_str()), None).is_err());

        // Replacing it with a valid selected config admits exactly that policy.
        fs::write(&config_file, manifest_bytes()).expect("valid config");
        let loaded = load_config_from(Some(config_home.as_os_str()), None).expect("config loads");
        assert_eq!(loaded.entries.len(), 1);
    }
}

/// Validate the generated `agalma-dns` bootstrap draft through the existing
/// Rust contract. The artifact intentionally lives under the persistent primary
/// `target/test-runs/<bead>/` root, outside any worktree, so the path is passed
/// explicitly and the case is opt-in.
#[test]
#[ignore = "requires AGALMA_DNS_DRAFT_MANIFEST, AGALMA_DNS_MANIFEST_REPO, AGALMA_DNS_MANIFEST_BASE"]
fn dns_draft_manifest_parses_and_selects_pending_main() {
    let path = std::env::var("AGALMA_DNS_DRAFT_MANIFEST")
        .expect("AGALMA_DNS_DRAFT_MANIFEST must name the generated draft");
    let repository = std::env::var("AGALMA_DNS_MANIFEST_REPO")
        .expect("AGALMA_DNS_MANIFEST_REPO must name the canonical repository root");
    let base = std::env::var("AGALMA_DNS_MANIFEST_BASE")
        .expect("AGALMA_DNS_MANIFEST_BASE must name the pinned base SHA");
    let bytes = fs::read(&path).expect("read generated draft");
    let inputs = ProtectedInputs::parse(&bytes).expect("generated draft parses");
    let entry = inputs
        .select("agalma-dns", &repository, &base)
        .expect("generated draft selects the pinned identity");
    assert_eq!(entry.task_id, "agalma-dns");
    assert_eq!(
        entry.allowed_paths,
        vec![
            "scripts/test.sh".to_string(),
            "scripts/tests/runner-properties.sh".to_string(),
            "scripts/tests/runner-focused.sh".to_string()
        ]
    );
    assert_eq!(
        entry.protected_paths,
        vec!["fixtures/protected/agalma-dns".to_string()]
    );
    assert_eq!(entry.baseline.files.len(), 1);
    assert_eq!(entry.baseline.files[0].path, "agalma-dns/judge.sh");
    assert_eq!(entry.baseline.files[0].mode, "100755");
    assert_eq!(entry.baseline.commands.len(), 1);
    let command = &entry.baseline.commands[0];
    let evidence_base = Path::new(&repository).join("target/test-runs/agalma-dns");
    let quoted_evidence = format!(
        "'{}'",
        evidence_base
            .to_string_lossy()
            .replace('\'', "'\\''")
    );
    let expected = format!(
        "AGALMA_JUDGE_EVIDENCE_BASE={quoted_evidence} bash \"$AGALMA_PROTECTED_DIR/agalma-dns/judge.sh\""
    );
    assert_eq!(
        command, &expected,
        "emitted command must shell-quote the evidence base and runtime protected path"
    );
}

#[test]
fn authorization_properties_hold_over_generated_change_sets() {
    const CASES: usize = 256;
    const KINDS: [ChangeKind; 5] = [
        ChangeKind::Added,
        ChangeKind::Modified,
        ChangeKind::Deleted,
        ChangeKind::Renamed,
        ChangeKind::ModeChanged,
    ];
    let scope = Scope {
        allowed_paths: vec!["src".to_string(), "tests".to_string()],
        protected_paths: vec!["tests/protected_check.sh".to_string()],
        readable_artifacts: Vec::new(),
        oracle: scope_guard::dummy_artifact(),
    };
    let policy = ScopePolicy::from_scope(&scope).expect("policy");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut denied_seen = 0usize;
    for _ in 0..CASES {
        let count = 1 + rng.below(6) as usize;
        let mut changes = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = KINDS[rng.below(KINDS.len() as u64) as usize];
            let path = PATHS[rng.below(PATHS.len() as u64) as usize].to_string();
            let old_path = if kind == ChangeKind::Renamed {
                Some(PATHS[rng.below(PATHS.len() as u64) as usize].to_string())
            } else {
                None
            };
            changes.push(FileChange {
                kind,
                path,
                old_path,
                old_mode: None,
                new_mode: None,
                symlink_target: None,
            });
        }
        let report = policy.authorize(&changes);
        assert_eq!(report.authorized + report.denied.len(), changes.len());
        assert_eq!(report, policy.authorize(&changes), "deterministic");
        for denial in &report.denied {
            let affected = changes.iter().any(|c| {
                c.path == denial.path || c.old_path.as_deref() == Some(denial.path.as_str())
            });
            assert!(affected, "denial references an affected path: {denial:?}");
        }
        for c in &changes {
            if under(&c.path, "tests/protected_check.sh") {
                assert!(report.is_denied(), "protected destination must deny");
            }
        }
        if report.is_denied() {
            denied_seen += 1;
        }
    }
    assert!(denied_seen > 0, "generated cases must exercise denials");
}
