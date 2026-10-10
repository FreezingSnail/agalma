//! M1.6 fencing, expiry recovery, and projection reconcile (`agalma-52k.7`).
//!
//! Native Rust tests. The executor-level cases drive the real repo-mode
//! checkout/integrate effects in-process with the scripted build/verify
//! stand-ins; the reconcile case uses a real scratch `bd` store. All scratch
//! state lives under `target/test-runs/` (never `/tmp`).
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{
    CommitBatch, ExecutionApi, ExecutionEvent, ExecutionId, ExecutionPhase, ExecutionRecord,
    ExecutionState, LedgerApi, StepOutcome,
};
use agalma_execution::{DispatchOutcome, Executor, ExecutorConfig, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;
use agalma_taskqueue::BdTaskQueue;
use agalma_workspace::Workspace;
use serde_json::json;

use agalma_conductor::phases::{activities_with_mode, ActivityMode, PhaseDeps, RepoMode, RunState};
use agalma_conductor::reconcile::reconcile_projection;

const TASK: &str = "fence-task";

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
        .join(format!("fencing-{name}-{}-{nanos}", std::process::id()));
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

/// A scratch origin repo (git) plus a state dir; no bd store.
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

    fn commits(&self) -> String {
        git(&self.origin, &["rev-list", "--count", "refs/heads/main"])
    }
}

/// In-process repo-mode harness (real checkout/integrate, scripted build/verify).
struct Harness {
    executor: Executor<SqliteLedger>,
    _workspace: Rc<RefCell<Workspace>>,
}

fn build_harness(origin: &Origin, acceptance: &[&str]) -> Harness {
    std::fs::create_dir_all(&origin.state_dir).expect("state dir");
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
            task_title: "Fence the task".to_string(),
            acceptance: acceptance.iter().map(|s| s.to_string()).collect(),
        }),
        harness: agalma_conductor::harness_binding::harness_handle(
            agalma_conductor::harness_binding::HarnessChoice::OpenCode,
        ),
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
    let mut queue = BdTaskQueue::new(&origin);
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

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for this test but was not found");
}
