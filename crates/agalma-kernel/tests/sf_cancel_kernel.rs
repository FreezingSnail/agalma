//! SF.4 kernel cancellation repair coverage (`agalma-5fy.4` review finding).
//!
//! The kernel's `settle_cancellation` used to retry worker termination but never
//! replayed the `Cancel` transition or consumed the execution's pending dispatch
//! intents. A crash after the durable request (`Requested`) committed — before
//! the transition, or between its commit and its post-commit cleanup — could
//! therefore settle as `Settled` while a dispatchable intent survived, letting
//! recovery advance the cancelled execution. This file proves the repair:
//!
//! - [`crash_after_cancel_request_recovers_transition_and_intents`] — real
//!   crash/restart child aborts **after the durable request commits, before the
//!   `Cancel` transition**; recovery replays the transition, consumes the intent,
//!   and produces no effect.
//! - [`crash_after_cancel_commit_recovers_cleanup`] — real crash/restart child
//!   aborts **after the `Cancel` transition commit, before its cleanup**;
//!   recovery consumes the surviving intent and never dispatches it.
//! - [`property_cancellation_invariants`] — generated operation sequences
//!   (deterministic SplitMix64, fixed seed, bounded case/step budget) assert,
//!   independently of the implementation: duplicate cancel is idempotent, a
//!   pending cancellation blocks advancement/effect, settlement is monotonic,
//!   and a `Settled` record never leaves a dispatchable pending intent.
//! - [`boundary_sequences_are_harmless`] — invalid/boundary sequences: settle
//!   without a request, cancel of completed work, and a stray settle.
//!
//! Run artifacts live under `target/test-runs/`; no `/tmp`, no network, no paid
//! inference.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use agalma_contracts::{
    CancellationSettlement, CancellationState, ExecutionId, ExecutionPhase, OperationId, TaskId,
};
use agalma_kernel::{
    recover_pending, run_activity, ActivityDef, ActivityRegistry, Command, ExecState, Inputs,
    KernelEngine, OperationRequest, OperationResult, OperationState, Probe, WorkerRecord,
};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;
use serde_json::json;

use support::harness::{self, EffectJournal, RecordingActivity};
use support::ops::JournalHandler;
use support::process::{GoneLauncher, StubbornLauncher};

/// Environment variable passing the parent's run directory to the crash child.
const DIR_ENV: &str = "KERNEL_SF_CANCEL_DIR";

const CASE_REQUEST: &str = "cancel-request-before-transition";
const CASE_TRANSITION: &str = "cancel-transition-before-cleanup";

fn exec() -> ExecutionId {
    ExecutionId::derive(&TaskId::new("sf-cancel-task"), 1)
}

fn build_def() -> ActivityDef {
    ActivityDef::new("fake/build", "v1")
}

fn start(engine: &mut KernelEngine<SqliteLedger>, exec: &ExecutionId, task: &TaskId) {
    engine
        .apply(
            exec,
            &Command::Start {
                task_id: task.clone(),
                generation: 1,
            },
            &Inputs {
                now_unix_ms: 1,
                ..Inputs::default()
            },
        )
        .expect("start");
}

/// Announce one pending operation (a durable dispatch intent), without an effect.
fn announce(engine: &mut KernelEngine<SqliteLedger>, exec: &ExecutionId) -> OperationId {
    let operation = OperationId::derive(exec, "build");
    engine
        .apply(
            exec,
            &Command::Deliver {
                operation_id: operation.clone(),
                inputs_hash: "h1".to_string(),
            },
            &Inputs {
                probe: Probe::Absent,
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .expect("announce");
    operation
}

fn pending_of(engine: &KernelEngine<SqliteLedger>, exec: &ExecutionId) -> Vec<OperationId> {
    engine
        .pending_dispatch()
        .expect("pending")
        .into_iter()
        .filter(|intent| &intent.execution_id == exec)
        .map(|intent| intent.operation_id)
        .collect()
}

fn registry(journal: &EffectJournal) -> ActivityRegistry {
    let mut registry = ActivityRegistry::new();
    registry
        .register(Box::new(JournalHandler::new(
            build_def(),
            RecordingActivity::new(journal.clone()),
        )))
        .expect("register fake/build");
    registry
}

// ===========================================================================
// Crash / restart regressions
// ===========================================================================

/// Spawn this test binary as the named crash child.
fn spawn_child(test_name: &str, case: &str, dir: &Path) -> std::process::ExitStatus {
    let exe = std::env::current_exe().expect("current_exe");
    StdCommand::new(exe)
        .args([test_name, "--exact", "--test-threads=1", "--nocapture"])
        .env(harness::CHILD_ENV, case)
        .env(DIR_ENV, dir)
        .status()
        .expect("spawn crash child")
}

/// Child role for both windows: start, announce a pending intent, arm the
/// requested hook, then request cancellation (which must abort).
fn run_crash_child() {
    let case = harness::child_case().expect("child case");
    let dir = std::env::var(DIR_ENV).expect("child run dir");
    let ledger = SqliteLedger::open(Path::new(&dir).join("ledger.sqlite")).expect("child ledger");
    let mut engine = KernelEngine::open(ledger).expect("child kernel");
    let exec = exec();

    harness::disarm();
    start(&mut engine, &exec, &TaskId::new("sf-cancel-task"));
    announce(&mut engine, &exec);
    match case.as_str() {
        CASE_REQUEST => harness::arm(harness::AFTER_CANCEL_REQUEST),
        CASE_TRANSITION => harness::arm(harness::AFTER_TRANSITION_COMMIT),
        other => panic!("unknown child case {other}"),
    }
    let _ = engine.cancel(&exec, 7);
    eprintln!("child {case} did not crash as expected");
    std::process::exit(1);
}

#[test]
fn crash_after_cancel_request_recovers_transition_and_intents() {
    if harness::child_case().is_some() {
        run_crash_child();
        return;
    }

    let dir = harness::recovery_dir(CASE_REQUEST);
    let status = spawn_child(
        "crash_after_cancel_request_recovers_transition_and_intents",
        CASE_REQUEST,
        &dir,
    );
    harness::assert_sigabrt(&status);

    let journal = EffectJournal::in_run_dir(&dir, CASE_REQUEST);
    let mut engine =
        KernelEngine::open(SqliteLedger::open(dir.join("ledger.sqlite")).expect("ledger"))
            .expect("kernel")
            .with_launcher(Box::new(GoneLauncher));
    let exec = exec();
    let build = OperationId::derive(&exec, "build");

    // The durable request survived; the transition did not run: the execution is
    // still advancing-capable with a pending dispatchable intent.
    let settlement = engine
        .cancellation_settlement(&exec)
        .expect("settlement")
        .expect("requested settlement");
    assert_eq!(settlement.state, CancellationState::Requested, "{settlement:?}");
    let state = engine.load(&exec).expect("load");
    assert_eq!(state.state, ExecState::Running);
    assert_eq!(state.operations[&build].state, OperationState::Dispatched);
    assert_eq!(pending_of(&engine, &exec), vec![build.clone()]);

    // Before settlement, the requested cancellation already dominates recovery:
    // the dispatchable intent is not re-dispatched.
    let mut early_reg = registry(&journal);
    let early = recover_pending(&mut engine, &mut early_reg, &exec, 8).expect("pre-settle recover");
    assert!(early.is_empty(), "requested cancellation dispatched: {early:?}");
    assert_eq!(journal.effect_count(), 0);

    // Recovery replays the cancellation transition, consumes the intent, and
    // settles: the cancelled execution can never advance again.
    let settled = engine
        .settle_cancellation(&exec, 9)
        .expect("settle")
        .expect("settlement");
    assert_eq!(settled.state, CancellationState::Settled, "{settled:?}");
    let state = engine.load(&exec).expect("load after settle");
    assert_eq!(state.state, ExecState::Cancelled);
    assert_eq!(state.operations[&build].state, OperationState::Cancelled);
    assert!(
        pending_of(&engine, &exec).is_empty(),
        "recovery consumed the pending intent"
    );

    // Independent behavioral check: recovery cannot dispatch an effect.
    let mut reg = registry(&journal);
    let results = recover_pending(&mut engine, &mut reg, &exec, 10).expect("recover");
    assert!(results.is_empty(), "recovery dispatched: {results:?}");
    assert_eq!(journal.effect_count(), 0, "no effect after cancellation");

    // Even a fresh activity attempt is rejected.
    let blocked = run_activity(
        &mut engine,
        &mut reg,
        &exec,
        &OperationRequest {
            activity: build_def(),
            step: "verify".to_string(),
            phase: ExecutionPhase::Verify,
            component_api: "harness".to_string(),
            inputs: json!({}),
            now_unix_ms: 11,
        },
    )
    .expect("run_activity");
    assert!(
        matches!(blocked, OperationResult::Blocked { .. }),
        "cancelled execution accepted new work: {blocked:?}"
    );
    assert_eq!(journal.effect_count(), 0);
}

#[test]
fn crash_after_cancel_commit_recovers_cleanup() {
    if harness::child_case().is_some() {
        run_crash_child();
        return;
    }

    let dir = harness::recovery_dir(CASE_TRANSITION);
    let status = spawn_child(
        "crash_after_cancel_commit_recovers_cleanup",
        CASE_TRANSITION,
        &dir,
    );
    harness::assert_sigabrt(&status);

    let journal = EffectJournal::in_run_dir(&dir, CASE_TRANSITION);
    let mut engine =
        KernelEngine::open(SqliteLedger::open(dir.join("ledger.sqlite")).expect("ledger"))
            .expect("kernel")
            .with_launcher(Box::new(GoneLauncher));
    let exec = exec();
    let build = OperationId::derive(&exec, "build");

    // The `Cancel` transition committed (execution + operation Cancelled) but the
    // post-commit cleanup did not run: the intent is still pending.
    let settlement = engine
        .cancellation_settlement(&exec)
        .expect("settlement")
        .expect("requested settlement");
    assert_eq!(settlement.state, CancellationState::Requested, "{settlement:?}");
    let state = engine.load(&exec).expect("load");
    assert_eq!(state.state, ExecState::Cancelled);
    assert_eq!(state.operations[&build].state, OperationState::Cancelled);
    assert_eq!(
        pending_of(&engine, &exec),
        vec![build.clone()],
        "the crashed cleanup left the intent unconsumed"
    );

    // The requested cancellation blocks recovery of the surviving intent even
    // before settlement runs.
    let mut early_reg = registry(&journal);
    let early = recover_pending(&mut engine, &mut early_reg, &exec, 8).expect("pre-settle recover");
    assert!(early.is_empty(), "requested cancellation dispatched: {early:?}");
    assert_eq!(journal.effect_count(), 0);

    // Recovery accounts for the surviving intent before settling.
    let settled = engine
        .settle_cancellation(&exec, 9)
        .expect("settle")
        .expect("settlement");
    assert_eq!(settled.state, CancellationState::Settled, "{settled:?}");
    assert!(
        pending_of(&engine, &exec).is_empty(),
        "recovery consumed the surviving intent"
    );

    let mut reg = registry(&journal);
    let results = recover_pending(&mut engine, &mut reg, &exec, 10).expect("recover");
    assert!(results.is_empty(), "recovery dispatched: {results:?}");
    assert_eq!(journal.effect_count(), 0);
}

// ===========================================================================
// Property test — generated cancellation operation sequences
// ===========================================================================

/// Deterministic SplitMix64 generator: a fixed seed plus a per-case constant, so
/// a failing sequence is replayable from `(SEED, case, step)`.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// The fixed, recorded sequence seed and bounded case/step budget.
const PROP_SEED: u64 = 0x5F4C_0100_0000_0001;
const PROP_CASES: u64 = 160;
const PROP_STEPS: usize = 24;

struct Fixture {
    /// Held only to keep the scratch directory alive for the case duration.
    _dir: ScratchDir,
    db: PathBuf,
    engine: KernelEngine<SqliteLedger>,
    registry: ActivityRegistry,
    journal: EffectJournal,
    exec: ExecutionId,
    task: TaskId,
    last_rank: u8,
    /// Coverage counters: how often each invariant was actually exercised.
    duplicate_checks: u64,
    pending_blocks_checks: u64,
    settled_checks: u64,
}

fn rank(state: CancellationState) -> u8 {
    match state {
        CancellationState::None => 0,
        CancellationState::Requested => 1,
        CancellationState::Settling => 2,
        CancellationState::Settled => 3,
    }
}

impl Fixture {
    fn new(case: u64) -> Self {
        let dir = harness::recovery_dir(&format!("sf-cancel-prop-{case}"));
        let db = dir.join("ledger.sqlite");
        let journal = EffectJournal::in_run_dir(&dir, "sf-cancel-prop");
        let engine =
            KernelEngine::open(SqliteLedger::open(&db).expect("ledger"))
                .expect("kernel")
                .with_launcher(Box::new(StubbornLauncher));
        let registry = registry(&journal);
        let task = TaskId::new("prop-task");
        let exec = ExecutionId::derive(&task, 1);
        Fixture {
            _dir: dir,
            db,
            engine,
            registry,
            journal,
            exec,
            task,
            last_rank: 0,
            duplicate_checks: 0,
            pending_blocks_checks: 0,
            settled_checks: 0,
        }
    }

    fn revision(&self) -> u64 {
        self.engine.load(&self.exec).expect("load").revision
    }

    fn settlement(&self) -> Option<CancellationSettlement> {
        self.engine
            .cancellation_settlement(&self.exec)
            .expect("settlement")
    }

    fn cancellation_state(&self) -> CancellationState {
        self.settlement().map_or(CancellationState::None, |s| s.state)
    }

    fn pending(&self) -> Vec<OperationId> {
        pending_of(&self.engine, &self.exec)
    }

    fn effects(&self) -> usize {
        self.journal.effect_count()
    }

    fn reopen(&mut self) {
        let db = self.db.clone();
        let journal = self.journal.clone();
        self.engine = KernelEngine::open(SqliteLedger::open(&db).expect("reopen ledger"))
            .expect("reopen kernel")
            .with_launcher(Box::new(StubbornLauncher));
        self.registry = registry(&journal);
    }

    fn activity(&mut self, step: &str) -> OperationResult {
        let request = OperationRequest {
            activity: build_def(),
            step: step.to_string(),
            phase: ExecutionPhase::Build,
            component_api: "harness".to_string(),
            inputs: json!({ "step": step }),
            now_unix_ms: 3,
        };
        run_activity(&mut self.engine, &mut self.registry, &self.exec, &request).expect("run_activity")
    }

    fn recover(&mut self) {
        let _ = recover_pending(&mut self.engine, &mut self.registry, &self.exec, 4)
            .expect("recover_pending");
    }

    fn cancel(&mut self) {
        self.engine.cancel(&self.exec, 5).expect("cancel");
    }

    fn settle(&mut self) {
        let _ = self.engine.settle_cancellation(&self.exec, 6).expect("settle");
    }

    /// Apply one generated operation.
    fn apply_op(&mut self, op: u64) {
        match op {
            0 => start(&mut self.engine, &self.exec, &self.task),
            1 => {
                let _ = self.activity("build");
            }
            2 => {
                let _ = self.activity("verify");
            }
            3 => self.cancel(),
            4 => self.settle(),
            5 => self.reopen(),
            6 => {
                let _ = self.engine.apply(
                    &self.exec,
                    &Command::KillLatch { latched: false },
                    &Inputs {
                        now_unix_ms: 8,
                        ..Inputs::default()
                    },
                );
            }
            7 => self.recover(),
            8 => {
                // Record a confined worker for the build operation so a
                // cancellation has termination evidence to (fail to) confirm.
                self.engine
                    .record_worker(&WorkerRecord {
                        execution_id: self.exec.clone(),
                        operation_id: OperationId::derive(&self.exec, "build"),
                        phase: ExecutionPhase::Build,
                        attempt: 1,
                        lease_generation: 1,
                        pid: 4242,
                        pgid: 4242,
                        started_at_unix_ms: 1,
                    })
                    .expect("record worker");
            }
            _ => unreachable!(),
        }
    }

    /// Assert the cancellation invariants after every operation. These are
    /// implementation-independent checks over observable state.
    fn check_invariants(&mut self, case: u64, step: usize) {
        let settlement = self.settlement();
        let state = settlement
            .as_ref()
            .map_or(CancellationState::None, |s| s.state);

        // Monotonic: cancellation never regresses.
        assert!(
            rank(state) >= self.last_rank,
            "case {case} step {step}: cancellation regressed {} -> {:?}",
            self.last_rank,
            state
        );
        self.last_rank = rank(state);

        let revision = self.revision();
        let pending = self.pending();
        let effects = self.effects();

        // (1) Duplicate cancel is idempotent (only meaningful once requested).
        if settlement.is_some() {
            self.duplicate_checks += 1;
            self.cancel();
            assert_eq!(
                self.revision(),
                revision,
                "case {case} step {step}: duplicate cancel bumped revision"
            );
            assert_eq!(
                self.settlement(),
                settlement,
                "case {case} step {step}: duplicate cancel changed the settlement"
            );
            assert_eq!(
                self.pending(),
                pending,
                "case {case} step {step}: duplicate cancel changed pending intents"
            );
        }

        // (2) A pending cancellation blocks advancement and any effect.
        if matches!(state, CancellationState::Requested | CancellationState::Settling) {
            self.pending_blocks_checks += 1;
            let result = self.activity("integrate");
            assert!(
                matches!(result, OperationResult::Blocked { .. })
                    || matches!(result, OperationResult::Completed { duplicate: true, .. }),
                "case {case} step {step}: pending cancellation did not block advancement: {result:?}"
            );
            assert_eq!(
                self.effects(),
                effects,
                "case {case} step {step}: an effect ran under pending cancellation"
            );
            self.recover();
            assert_eq!(
                self.effects(),
                effects,
                "case {case} step {step}: recovery dispatched under pending cancellation"
            );
        }

        // (3) A settled cancellation never leaves a dispatchable pending intent.
        if state == CancellationState::Settled {
            self.settled_checks += 1;
            assert!(
                self.pending().is_empty(),
                "case {case} step {step}: settled cancellation left pending intents {:?}",
                self.pending()
            );
            self.recover();
            assert_eq!(
                self.effects(),
                effects,
                "case {case} step {step}: recovery dispatched after settlement"
            );
        }
    }
}

#[test]
fn property_cancellation_invariants() {
    let mut duplicate_checks = 0u64;
    let mut pending_blocks_checks = 0u64;
    let mut settled_checks = 0u64;
    for case in 0..PROP_CASES {
        let mut rng = Rng::new(PROP_SEED ^ case.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut fixture = Fixture::new(case);
        // A fraction of cases start with real work so cancellation lands on an
        // in-flight execution.
        let starts = rng.below(4);
        for _ in 0..starts {
            fixture.apply_op(0);
        }
        for step in 0..PROP_STEPS {
            let op = rng.below(9);
            fixture.apply_op(op);
            fixture.check_invariants(case, step);
        }
        duplicate_checks += fixture.duplicate_checks;
        pending_blocks_checks += fixture.pending_blocks_checks;
        settled_checks += fixture.settled_checks;
    }
    // Non-vacuous: the generated budget must actually exercise each invariant.
    // Recorded budget: PROP_CASES cases x up to PROP_STEPS ops from seed
    // PROP_SEED; observed coverage below.
    eprintln!(
        "property coverage: duplicate_cancel={duplicate_checks} \
         pending_blocks={pending_blocks_checks} settled={settled_checks}"
    );
    assert!(
        duplicate_checks > 0,
        "generated sequences never exercised duplicate cancel"
    );
    assert!(
        pending_blocks_checks > 0,
        "generated sequences never observed a pending cancellation"
    );
    assert!(
        settled_checks > 0,
        "generated sequences never reached a settled cancellation"
    );
}

// ===========================================================================
// Invalid / boundary sequences
// ===========================================================================

#[test]
fn boundary_sequences_are_harmless() {
    // Settle without any request: no settlement, no effect.
    let mut fixture = Fixture::new(0);
    assert_eq!(
        fixture
            .engine
            .settle_cancellation(&fixture.exec, 1)
            .expect("settle"),
        None,
        "no cancellation was requested"
    );
    assert_eq!(fixture.cancellation_state(), CancellationState::None);

    // Cancel of already-terminal (parked) work keeps its outcome: the frozen
    // `Cancelled` projection never rewrites a terminal execution.
    start(&mut fixture.engine, &fixture.exec, &fixture.task);
    fixture.engine
        .apply(
            &fixture.exec,
            &Command::Deliver {
                operation_id: OperationId::derive(&fixture.exec, "build"),
                inputs_hash: "h1".to_string(),
            },
            &Inputs {
                probe: Probe::Unknown,
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .expect("park via unknown probe");
    let state = fixture.engine.load(&fixture.exec).expect("load");
    assert_eq!(state.state, ExecState::Parked, "unknown probe parks");
    let effects = fixture.effects();
    fixture.cancel();
    let state = fixture.engine.load(&fixture.exec).expect("load after cancel");
    assert_eq!(state.state, ExecState::Parked, "terminal work keeps its outcome");
    assert_eq!(
        fixture.cancellation_state(),
        CancellationState::Settled,
        "a cancel of terminal work is reconciled as a no-op settlement"
    );
    // Settling an already-settled no-op changes nothing.
    let revision = fixture.revision();
    fixture.settle();
    assert_eq!(fixture.revision(), revision);
    assert_eq!(fixture.effects(), effects, "no effect from a no-op cancel");

    // A stray kill-latch toggle (resume) never clears a cancellation.
    let mut fixture2 = Fixture::new(1);
    let task2 = fixture2.task.clone();
    start(&mut fixture2.engine, &fixture2.exec, &task2);
    let _ = fixture2.activity("build");
    fixture2.cancel();
    fixture2
        .engine
        .apply(
            &fixture2.exec,
            &Command::KillLatch { latched: false },
            &Inputs {
                now_unix_ms: 2,
                ..Inputs::default()
            },
        )
        .expect("resume");
    assert_ne!(
        fixture2.cancellation_state(),
        CancellationState::None,
        "resume must not clear a per-execution cancellation"
    );
    fixture2.settle();
    assert_eq!(fixture2.cancellation_state(), CancellationState::Settled);
    assert!(fixture2.pending().is_empty());
}
