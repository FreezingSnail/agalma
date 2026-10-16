//! M1.6 fencing, expiry recovery, and projection reconcile (`agalma-52k.7`).
//!
//! Native Rust tests. The executor-level cases drive the real repo-mode
//! checkout/integrate effects in-process with the scripted build/verify
//! stand-ins; the reconcile case uses a real scratch `bd` store. All scratch
//! state lives under `target/test-runs/` (never `/tmp`); scratch dirs are
//! retained for review evidence (`ScratchDir::keep`), not auto-removed.
//!
//! | Case | Scenario |
//! |------|----------|
//! | 1 | duplicate delivery of a completed integrate merges exactly once (two conductors) |
//! | 2 | a stale-lease operation is rejected before it can produce an effect |
//! | 3 | a re-claim supersedes the old lease; an already-landed merge completes without re-merge |
//! | 4 | projection reconcile after a simulated crash rewrites the drift and is idempotent |

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use agalma_contracts::digest::{failure_code, sha256_digest, FailureCode};
use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, CommitBatch, ContextKind, DirectiveRef, ExecutionApi,
    ExecutionEvent, ExecutionId, ExecutionPhase, ExecutionRecord, ExecutionState, LedgerApi, Role,
    StepOutcome,
};
use agalma_execution::{DispatchOutcome, Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ipc::TaskQueueOps;
use agalma_ledger::SqliteLedger;
use agalma_taskqueue::TaskDefinition;
use agalma_testkit::ScratchDir;
use agalma_workspace::Workspace;

use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, PROTECTED_BASELINE_FORMAT,
};
use agalma_conductor::task_package::{
    build_package, load_admitted_package, object_path, persist_package, AuthorizationInput,
    ContextInput, PackageInputs, RoleInput,
};
use agalma_conductor::workspace_process::WorkspaceHandle;
use serde_json::json;

use agalma_conductor::phases::{activities_with_mode, ActivityMode, PhaseDeps, RepoMode, RunState};
use agalma_conductor::reconcile::reconcile_projection;
use agalma_conductor::taskqueue_process::TaskQueueHandle;

mod common;

const TASK: &str = "fence-task";

fn task() -> TaskId {
    TaskId::new(TASK)
}

fn fresh(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(&format!("fencing-{name}"));
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

/// A scratch origin repo (git) plus a state dir; no bd store.
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

    fn commits(&self) -> String {
        git(&self.origin, &["rev-list", "--count", "refs/heads/main"])
    }
}

/// In-process repo-mode harness (real checkout/integrate, scripted build/verify).
struct Harness {
    executor: Executor<SqliteLedger>,
    _workspace: Rc<RefCell<WorkspaceHandle>>,
}

fn build_harness(origin: &Origin, acceptance: &[&str]) -> Harness {
    std::fs::create_dir_all(&origin.state_dir).expect("state dir");
    // SF.1/SF.6: repository mode requires the recorded package *and* a valid
    // frozen protected oracle entry. Seed a fencing-local valid fixture for
    // every generation this drill can dispatch (gen1, gen2); the shared
    // `tests/common` helper intentionally seeds an opaque oracle.
    let base_sha = git(&origin.origin, &["rev-parse", "refs/heads/main"]);
    seed_fenced_packages(
        &origin.state_dir,
        origin.origin.to_str().expect("origin utf8"),
        &base_sha,
        &acceptance.join("\n"),
        2,
    );
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
            task_title: "Fence the task".to_string(),
            acceptance: acceptance.iter().map(|s| s.to_string()).collect(),
        }),
        harness: agalma_conductor::harness_binding::harness_handle(
            agalma_conductor::harness_binding::HarnessChoice::OpenCode,
        ),
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
    Harness {
        executor,
        _workspace: workspace,
    }
}

/// Drive until the execution is waiting on `phase` (before delivering it).
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

fn drive_to_terminal(executor: &mut Executor<SqliteLedger>, exec: &ExecutionId) {
    for _ in 0..32 {
        match executor.step(exec).expect("step") {
            StepOutcome::Idle { .. } | StepOutcome::Parked { .. } => return,
            StepOutcome::Dispatched { .. } | StepOutcome::Advanced { .. } => {}
            StepOutcome::Blocked { .. } => panic!("unexpected block"),
        }
    }
    panic!("execution did not reach a terminal state");
}

fn checkout_of(origin: &Origin, exec: &ExecutionId) -> agalma_contracts::Checkout {
    let path = origin
        .state_dir
        .join("runs")
        .join(agalma_workspace::execution_dir_name(exec))
        .join("checkout.json");
    let text = std::fs::read_to_string(&path).expect("checkout artifact");
    serde_json::from_str(&text).expect("checkout json")
}

// ===========================================================================
// Local valid SF.6 protected oracle fixture
// ===========================================================================
//
// SF.6 makes repository verification fail-closed: the frozen `scope.oracle` is
// the selected operator protected entry, and a package whose oracle is missing
// or malformed refuses at load. The shared `tests/common` helper seeds an
// intentionally opaque oracle (`b"harness-oracle"`), which is correct for the
// fixture-mode drills that never reach verification but makes these repo-mode
// fencing cases refuse before their fencing/merge effects. This fencing-local
// seeder builds the same minimal recorded package with a *valid* frozen entry
// (task/base/scope identities matched to the scratch origin) instead.

/// The frozen protected judge a valid baseline must carry (nonempty files +
/// nonempty commands). A trivial passing command keeps the drilling effect: the
/// fencing cases assert merge/fencing semantics, not protected behavior.
const PROTECTED_JUDGE_TEXT: &str = "#!/bin/sh\nexit 0\n";
const PROTECTED_JUDGE_COMMAND: &str = "sh -c 'true'";

fn protected_baseline() -> ProtectedBaseline {
    ProtectedBaseline {
        format: PROTECTED_BASELINE_FORMAT.to_string(),
        files: vec![BaselineFile {
            path: "judge.sh".to_string(),
            mode: "100644".to_string(),
            digest: sha256_digest(PROTECTED_JUDGE_TEXT.as_bytes()),
            bytes: PROTECTED_JUDGE_TEXT.len() as u64,
            text: PROTECTED_JUDGE_TEXT.to_string(),
        }],
        commands: vec![PROTECTED_JUDGE_COMMAND.to_string()],
    }
}

/// A validated protected entry bound to `repository_root`/`base_sha` with the
/// fencing candidate's scope (`src` writable, nothing protected).
fn fenced_entry(repository_root: &str, base_sha: &str) -> ProtectedEntry {
    ProtectedEntry {
        task_id: TASK.to_string(),
        repository_root: repository_root.to_string(),
        base_sha: base_sha.to_string(),
        allowed_paths: vec!["src".to_string()],
        protected_paths: Vec::new(),
        baseline: protected_baseline(),
    }
}

/// Persist a recorded `TaskPackage/v1` for `execution_id` with explicit package
/// scope, acceptance, and frozen oracle bytes; return its package digest.
fn persist_fenced_package(
    state_dir: &Path,
    execution_id: &str,
    package_task: &str,
    base_sha: &str,
    acceptance: &str,
    allowed_paths: Vec<String>,
    protected_paths: Vec<String>,
    oracle_bytes: Vec<u8>,
) -> String {
    let definition = TaskDefinition {
        task_id: TaskId::new(package_task),
        title: "Fence the task".to_string(),
        description: "Fenced fixture.\n".to_string(),
        design: Some("Design.\n".to_string()),
        notes: None,
        acceptance_criteria: Some(acceptance.to_string()),
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    };
    let inputs = PackageInputs {
        execution_id: execution_id.to_string(),
        generation: 1,
        lease_generation: 1,
        repository_identity: "fence-fixture".to_string(),
        base_sha: base_sha.to_string(),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 0,
            digest: sha256_digest(b"directive/v0"),
        },
        authorization: AuthorizationInput {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: sha256_digest(b"human-bootstrap/v1"),
            receipt_source: "bd:fence-task/claim".to_string(),
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
    let built = build_package(&definition, inputs).expect("build fenced package");
    persist_package(state_dir, &built).expect("persist fenced package");
    built.package.package_digest
}

/// Seed a valid SF.6-fixture package for generations `1..=max_generation` of
/// `TASK`, matching the scratch origin's repository root and base SHA.
fn seed_fenced_packages(
    state_dir: &Path,
    repository_root: &str,
    base_sha: &str,
    acceptance: &str,
    max_generation: u32,
) {
    let entry = fenced_entry(repository_root, base_sha);
    let oracle = entry.freeze().expect("freeze fencing entry");
    for generation in 1..=max_generation {
        persist_fenced_package(
            state_dir,
            &format!("exec:{TASK}:{generation}"),
            TASK,
            base_sha,
            acceptance,
            entry.allowed_paths.clone(),
            entry.protected_paths.clone(),
            oracle.clone(),
        );
    }
}


// ===========================================================================
// Case 1 — duplicate dispatch cannot merge twice
// ===========================================================================

#[test]
fn duplicate_integrate_delivery_merges_once() {
    let origin = Origin::new("dup-merge");
    let mut h = build_harness(&origin, &["sh -c 'grep -q 42 src/lib.rs'"]);
    let exec = h.executor.start(&task()).expect("start");
    drive_to_terminal(&mut h.executor, &exec);

    assert_eq!(
        h.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Done
    );
    assert_eq!(origin.commits(), "2", "seed + one candidate merge");

    let integrate = OperationId::derive(&exec, "integrate");

    // Replay against the same conductor: returns the recorded receipt.
    let replay = h.executor.deliver_operation(&integrate).expect("replay");
    assert!(
        matches!(replay, DispatchOutcome::Recorded { .. }),
        "replay returns recorded: {replay:?}"
    );

    // "Two conductors": a second executor over the same durable state also
    // returns the recorded receipt and repeats no merge.
    let mut second = build_harness(&origin, &["sh -c 'grep -q 42 src/lib.rs'"]);
    let other = second
        .executor
        .deliver_operation(&integrate)
        .expect("second");
    assert!(
        matches!(other, DispatchOutcome::Recorded { .. }),
        "second conductor returns recorded: {other:?}"
    );

    assert_eq!(origin.commits(), "2", "exactly one merge commit");
    let tag = format!("m1/exec-{TASK}-1");
    assert_eq!(
        git(&origin.origin, &["rev-parse", &format!("refs/tags/{tag}")]),
        git(&origin.origin, &["rev-parse", "refs/heads/main"]),
        "single integration tag at merged main"
    );
}

// ===========================================================================
// Case 2 — stale-lease operation is rejected before it can produce an effect
// ===========================================================================

#[test]
fn stale_lease_operation_is_rejected() {
    let origin = Origin::new("stale-reject");
    let mut h = build_harness(&origin, &["sh -c 'grep -q 42 src/lib.rs'"]);
    let exec = h.executor.start(&task()).expect("start");
    assert_eq!(
        h.executor
            .ledger()
            .lease_generation(&task())
            .expect("lease"),
        1
    );
    drive_to(&mut h.executor, &exec, ExecutionPhase::Build);

    let effects = agalma_conductor::phases::effects_log_path(&origin.state_dir, &exec);
    let before = std::fs::read_to_string(&effects)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);

    // A re-claim (expired lease) begins lease 2, superseding the first claim.
    let reclaimed = h.executor.start(&task()).expect("re-claim");
    assert_eq!(reclaimed, ExecutionId::new(format!("exec:{TASK}:2")));
    assert_eq!(
        h.executor
            .ledger()
            .lease_generation(&task())
            .expect("lease"),
        2
    );

    // The old claim's pending build operation must be refused: no effect runs.
    let build_op = OperationId::derive(&exec, "build@1");
    let outcome = h.executor.deliver_operation(&build_op).expect("deliver");
    assert!(
        matches!(
            outcome,
            DispatchOutcome::StaleLease {
                lease: 1,
                current: 2,
                ..
            }
        ),
        "stale lease refused: {outcome:?}"
    );
    let after = std::fs::read_to_string(&effects)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    assert_eq!(after, before, "stale operation produced no effect");
}

// ===========================================================================
// Case 3 — re-claim single winner; an already-landed merge completes without
// re-merge
// ===========================================================================

#[test]
fn reclaim_single_winner_and_merged_effect_completes() {
    let origin = Origin::new("reclaim-merged");
    let mut h = build_harness(&origin, &["sh -c 'grep -q 42 src/lib.rs'"]);
    let exec = h.executor.start(&task()).expect("start");
    drive_to(&mut h.executor, &exec, ExecutionPhase::Integrate);

    // Land the merge outside the executor, exactly as a crash-after-CAS would.
    let checkout = checkout_of(&origin, &exec);
    let workspace = Workspace::new(origin.state_dir.clone());
    workspace
        .integrate_origin(&checkout, &origin.origin, &checkout.base_sha)
        .expect("land merge");
    assert_eq!(origin.commits(), "2", "merge landed before re-claim");

    // Re-claim: lease 2 is the single winner; the old claim (lease 1) is stale.
    let reclaimed = h.executor.start(&task()).expect("re-claim");
    assert_eq!(reclaimed, ExecutionId::new(format!("exec:{TASK}:2")));
    assert_eq!(
        h.executor
            .ledger()
            .lease_generation(&task())
            .expect("lease"),
        2,
        "only the latest lease is current"
    );

    // The stale claim's integrate effect is already present: it reconciles and
    // completes without repeating the merge.
    let integrate = OperationId::derive(&exec, "integrate");
    let outcome = h.executor.deliver_operation(&integrate).expect("deliver");
    assert!(
        matches!(outcome, DispatchOutcome::Reconciled { .. }),
        "already-landed merge reconciled: {outcome:?}"
    );
    assert_eq!(
        h.executor.status(&exec).expect("status").phase,
        ExecutionPhase::Done,
        "stale claim completed from the landed merge"
    );
    assert_eq!(origin.commits(), "2", "no re-merge");
}

// ===========================================================================
// Case 4 — projection reconcile after a simulated crash rewrites drift and is
// idempotent
// ===========================================================================

#[test]
fn projection_reconcile_after_crash_is_idempotent() {
    require_bd();
    let dir = fresh("reconcile");
    let origin = dir.join("origin");
    std::fs::create_dir_all(origin.join("src")).expect("origin src");
    std::fs::write(
        origin.join("src").join("lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
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
            "fence",
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
    let create = Command::new("bd")
        .arg("--db")
        .arg(&beads)
        .args([
            "create",
            "Fence task",
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'true'\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n",
            "--json",
        ])
        .current_dir(&origin)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd create");
    assert!(create.status.success(), "bd create: {}", stderr_of(&create));
    let value: serde_json::Value = serde_json::from_slice(&create.stdout).expect("create json");
    let id = value["id"].as_str().expect("created id").to_string();
    let task_id = TaskId::new(id.clone());

    let state_dir = dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let ledger_path = state_dir.join("ledger.sqlite");
    let exec = ExecutionId::derive(&task_id, 1);
    {
        let mut ledger = SqliteLedger::open(&ledger_path).expect("open ledger");
        let record = ExecutionRecord {
            execution_id: exec.clone(),
            task_id: task_id.clone(),
            generation: 1,
            phase: ExecutionPhase::Done,
            state: ExecutionState::Completed,
            attempt: 1,
            revision: 0,
        };
        let now = 1_700_000_000_000u64;
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(record),
                events: vec![
                    ExecutionEvent {
                        execution_id: exec.clone(),
                        sequence: 0,
                        kind: "execution_created".to_string(),
                        payload: json!({
                            "task_id": task_id,
                            "generation": 1,
                            "lease_generation": 1,
                            "attempt": 1,
                        }),
                        recorded_at_unix_ms: now,
                    },
                    ExecutionEvent {
                        execution_id: exec.clone(),
                        sequence: 0,
                        kind: "done".to_string(),
                        payload: json!({ "attempt": 1 }),
                        recorded_at_unix_ms: now,
                    },
                ],
                receipts: vec![],
                intents: vec![],
            })
            .expect("commit ledger state");
    }

    // Simulated crash drift: bd shows an in-progress, parked issue with a stale
    // phase comment instead of the ledger's terminal done projection.
    let bd = |args: &[&str]| {
        let out = Command::new("bd")
            .arg("--db")
            .arg(&beads)
            .args(args)
            .current_dir(&origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd");
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        out
    };
    bd(&["update", &id, "--status", "in_progress"]);
    bd(&["update", &id, "--add-label", "parked"]);
    bd(&[
        "comment",
        &id,
        &format!("phase=build execution={exec} lease=1"),
    ]);

    let bin = env!("CARGO_BIN_EXE_agalma");
    let first = Command::new(bin)
        .args([
            "reconcile",
            "--repo",
            origin.to_str().expect("origin utf8"),
            "--state-dir",
            state_dir.to_str().expect("state utf8"),
        ])
        .output()
        .expect("run reconcile");
    assert!(first.status.success(), "reconcile: {}", stderr_of(&first));
    let summary = String::from_utf8_lossy(&first.stdout);
    assert!(summary.contains("fixed=1"), "fixed once: {summary}");

    let ledger = SqliteLedger::open(&ledger_path).expect("reopen ledger");
    let mut queue = TaskQueueHandle::in_process(&origin);
    let projection = queue.projection(&task_id).expect("projection");
    assert_eq!(projection.status, "closed", "ledger wins: closed");
    assert!(!projection.has_label("parked"), "parked label removed");
    assert!(
        projection.has_comment(&format!("phase=done execution={exec} lease=1")),
        "canonical done comment written: {:?}",
        projection.comments
    );
    let comments_after_first = projection.comments.len();

    // Idempotent: a second reconcile makes no writes.
    let second = reconcile_projection(&ledger, &mut queue).expect("second reconcile");
    assert_eq!(second.checked, 1);
    assert_eq!(second.fixed, 0, "no writes when the projection matches");
    assert_eq!(second.stale, 0);
    let projection = queue.projection(&task_id).expect("projection again");
    assert_eq!(
        projection.comments.len(),
        comments_after_first,
        "idempotent: no extra comment"
    );
}

// ===========================================================================
// Protected oracle fixture identity — deterministic property + fail-closed
// ===========================================================================

/// Deterministic xorshift generator (same construction as the SF.6 property).
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

/// Order-insensitive path-set equality (the scope-agreement contract).
fn same_set(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<&String> = a.iter().collect();
    let mut b: Vec<&String> = b.iter().collect();
    a.sort();
    b.sort();
    a == b
}

/// Deterministic property over the frozen protected-entry identity contract
/// (seed `0x5f6_fence`, budget 64 cases).
///
/// The observable seam is `load_admitted_package`, the exact fail-closed path
/// the repo-mode fencing cases exercise (oracle parse + scope agreement). The
/// expected outcome is derived from the generated **identities**, never from
/// the loader: a package loads only when `task_id`, `base_sha`, and the
/// allowed/protected path *sets* agree; every mismatch refuses with the
/// matching failure code. Boundary paths include the empty set and an
/// order-permuted set that must still agree.
#[test]
fn protected_entry_identity_fail_closed_property() {
    const CASES: usize = 64;
    // The bead's mnemonic seed `0x5f6_fence`; `n` is not a hex digit, so the
    // seed is the same digit string with `n` dropped (`0x5f6_fece`).
    const SEED: u64 = 0x5f6_fece;
    const PACKAGE_BASE: &str = "1111111111111111111111111111111111111111";
    const ALTERNATE_BASE: &str = "2222222222222222222222222222222222222222";

    // Boundary path sets: empty, singleton, an order permutation of the same
    // set (must agree), and a differing set (must disagree).
    let empty: Vec<String> = Vec::new();
    let src_only: Vec<String> = vec!["src".to_string()];
    let docs_src: Vec<String> = vec!["docs".to_string(), "src".to_string()];
    let src_docs: Vec<String> = vec!["src".to_string(), "docs".to_string()];

    let dir = fresh("oracle-prop");
    let mut rng = Rng(SEED);
    let mut ok_seen = 0usize;
    let mut task_mismatch_seen = 0usize;
    let mut base_mismatch_seen = 0usize;
    let mut path_mismatch_seen = 0usize;

    for case in 0..CASES {
        // The first four cases pin the required classes; the rest vary from the
        // seeded stream. Both the forcing and the stream are deterministic.
        let (task_match, base_match, paths_match) = match case {
            0 => (true, true, true),
            1 => (false, true, true),
            2 => (true, false, true),
            3 => (true, true, false),
            _ => (rng.below(2) == 1, rng.below(2) == 1, rng.below(2) == 1),
        };

        // A "path match" uses an order permutation (boundary) so order
        // insensitivity is exercised; a mismatch is always a genuinely
        // different set.
        let (pkg_allowed, entry_allowed) = if paths_match {
            let pick = rng.below(3);
            match pick {
                0 => (empty.clone(), empty.clone()),
                1 => (src_only.clone(), src_only.clone()),
                _ => (docs_src.clone(), src_docs.clone()),
            }
        } else {
            (src_only.clone(), empty.clone())
        };
        let pkg_protected: Vec<String> = Vec::new();
        let entry_protected: Vec<String> = Vec::new();

        let entry = ProtectedEntry {
            task_id: if task_match {
                TASK.to_string()
            } else {
                "other.task".to_string()
            },
            repository_root: "/origin".to_string(),
            base_sha: if base_match { PACKAGE_BASE } else { ALTERNATE_BASE }.to_string(),
            allowed_paths: entry_allowed.clone(),
            protected_paths: entry_protected.clone(),
            baseline: protected_baseline(),
        };
        let oracle = entry.freeze().expect("freeze generated entry");
        // The generated frozen bytes must be a *valid* entry: any refusal below
        // is an identity disagreement, never malformedness.
        ProtectedEntry::parse(&oracle).expect("generated entry is well-formed");

        let digest = persist_fenced_package(
            &dir,
            &format!("exec:prop:{case}"),
            TASK,
            PACKAGE_BASE,
            "sh -c 'true'",
            pkg_allowed.clone(),
            pkg_protected.clone(),
            oracle,
        );

        let expect_ok = task_match
            && base_match
            && same_set(&entry.allowed_paths, &pkg_allowed)
            && same_set(&entry.protected_paths, &pkg_protected);

        match load_admitted_package(&dir, &digest) {
            Ok(_) => {
                assert!(
                    expect_ok,
                    "case {case}: valid fixture must load (seed {SEED:#x})"
                );
                ok_seen += 1;
            }
            Err(err) => {
                assert!(
                    !expect_ok,
                    "case {case}: mismatched fixture must refuse, loaded anyway: {err}"
                );
                let code = failure_code(&err);
                if !task_match || !base_match {
                    assert_eq!(
                        code,
                        Some(FailureCode::IdentityConflict),
                        "case {case}: task/base mismatch refuses as IdentityConflict"
                    );
                    if !task_match {
                        task_mismatch_seen += 1;
                    }
                    if !base_match {
                        base_mismatch_seen += 1;
                    }
                } else {
                    assert_eq!(
                        code,
                        Some(FailureCode::ScopeDenied),
                        "case {case}: path disagreement refuses as ScopeDenied"
                    );
                    path_mismatch_seen += 1;
                }
            }
        }
    }

    assert!(ok_seen > 0, "valid identity must be exercised");
    assert!(task_mismatch_seen > 0, "task mismatch must be exercised");
    assert!(base_mismatch_seen > 0, "base mismatch must be exercised");
    assert!(path_mismatch_seen > 0, "path mismatch must be exercised");
    // A permutation of the same set agrees; a different set does not.
    assert!(same_set(&docs_src, &src_docs), "order permutation agrees");
    assert!(!same_set(&src_only, &empty), "different sets disagree");
}

/// A frozen oracle that is not a valid protected entry refuses on load.
#[test]
fn malformed_frozen_oracle_refuses_on_load() {
    let dir = fresh("oracle-malformed");
    let digest = persist_fenced_package(
        &dir,
        &format!("exec:{TASK}:1"),
        TASK,
        "a".repeat(40).as_str(),
        "sh -c 'true'",
        vec!["src".to_string()],
        Vec::new(),
        b"opaque-not-a-protected-entry".to_vec(),
    );
    let err = load_admitted_package(&dir, &digest).expect_err("malformed oracle must refuse");
    assert_eq!(failure_code(&err), Some(FailureCode::InvalidRecord));
}

/// A valid fixture whose referenced oracle object is missing refuses on load.
#[test]
fn missing_frozen_oracle_object_refuses_on_load() {
    let dir = fresh("oracle-missing");
    let entry = fenced_entry("/origin", &"a".repeat(40));
    let oracle = entry.freeze().expect("freeze entry");
    let digest = persist_fenced_package(
        &dir,
        &format!("exec:{TASK}:1"),
        TASK,
        &"a".repeat(40),
        "sh -c 'true'",
        entry.allowed_paths.clone(),
        entry.protected_paths.clone(),
        oracle.clone(),
    );
    // Positive control: the fixture is valid while its oracle object is present.
    load_admitted_package(&dir, &digest).expect("valid fixture loads");
    // Remove the content-addressed oracle object: the load must refuse.
    let oracle_digest = sha256_digest(&oracle);
    std::fs::remove_file(object_path(&dir, &oracle_digest).expect("oracle path"))
        .expect("remove oracle object");
    let err = load_admitted_package(&dir, &digest).expect_err("missing oracle must refuse");
    assert_eq!(failure_code(&err), Some(FailureCode::LegacyInputsMissing));
}

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for this test but was not found");
}
