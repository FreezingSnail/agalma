//! H1 (`agalma-zxi.2`) watchdog, provider-health, and park-projection tests.
//!
//! Deterministic and native: a test-local [`HarnessApi`] fake drives the
//! progress watchdog directly (no vendor/network); the park and reconcile cases
//! run the real `agalma work` / `agalma reconcile` paths against a scratch
//! `git` + isolated `bd` store under `target/test-runs/watchdog-*` (never
//! `/tmp`). Covers:
//!
//! - silent session → idle `stalled` verdict with a tiny idle override plus a
//!   `provider_health` event;
//! - session-gone (404) fast-fail with no idle wait;
//! - a transient poll error does not fast-fail;
//! - digest stall count;
//! - park → bd status `blocked` + label `parked`;
//! - reconcile maps/repairs a parked projection idempotently.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::harness::{
    AttemptHandle, CancelAck, CreateSessionRequest, Describe, Event, HarnessApi, OperationHandle,
    OperationState, RunTurnRequest, SessionHandle, StartAttemptRequest, StopEvidence, API_VERSION,
    HARNESS_API,
};
use agalma_contracts::ids::{AttemptId, OperationId, SessionId, TaskId};
use agalma_contracts::{
    CommitBatch, ContractError, ExecutionEvent, ExecutionId, ExecutionPhase, ExecutionRecord,
    ExecutionState, LedgerApi, Task, TaskFamily, TaskPriority,
};
use agalma_ledger::SqliteLedger;

use agalma_conductor::digest::{self, Verdict};
use agalma_conductor::phases::{
    poll_terminal, record_provider_health, PollOutcome, PollStallReason,
};

// ---------------------------------------------------------------------------
// Test-local HarnessApi fake
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behavior {
    /// Never progresses: always running with a fixed event set.
    Silent,
    /// The remote session is gone (404 on inspect).
    SessionGone,
    /// A transient poll error that must not fast-fail.
    Transient,
}

struct FakeHarness {
    behavior: Behavior,
    op: OperationHandle,
}

impl FakeHarness {
    fn new(behavior: Behavior) -> Self {
        FakeHarness {
            behavior,
            op: OperationHandle {
                id: OperationId::new("op:fake:turn/1"),
            },
        }
    }
}

impl HarnessApi for FakeHarness {
    fn describe(&self) -> Describe {
        Describe {
            api: HARNESS_API.to_string(),
            api_version: API_VERSION,
            impl_name: "fake".to_string(),
            impl_version: "fake/v1".to_string(),
            capabilities: Default::default(),
        }
    }

    fn start_attempt(&mut self, _req: StartAttemptRequest) -> Result<AttemptHandle, ContractError> {
        Ok(AttemptHandle {
            id: AttemptId::derive(1),
        })
    }

    fn create_session(
        &mut self,
        _req: CreateSessionRequest,
    ) -> Result<SessionHandle, ContractError> {
        Ok(SessionHandle {
            id: SessionId::derive(&AttemptId::derive(1), 1),
        })
    }

    fn run_turn(
        &mut self,
        _session: &SessionHandle,
        _req: RunTurnRequest,
    ) -> Result<OperationHandle, ContractError> {
        Ok(self.op.clone())
    }

    fn inspect_operation(
        &mut self,
        _op: &OperationHandle,
    ) -> Result<OperationState, ContractError> {
        match self.behavior {
            Behavior::Silent | Behavior::Transient => Ok(OperationState::Running),
            Behavior::SessionGone => Err(ContractError::UnknownOutcome(
                "GET /api/session/fake -> 404".to_string(),
            )),
        }
    }

    fn read_events(&mut self, _op: &OperationHandle) -> Result<Vec<Event>, ContractError> {
        if self.behavior == Behavior::Transient {
            return Err(ContractError::KnownFailure(
                "transient read blip".to_string(),
            ));
        }
        Ok(vec![Event::TurnStarted])
    }

    fn cancel_operation(&mut self, _op: &OperationHandle) -> Result<CancelAck, ContractError> {
        Ok(CancelAck {
            acknowledged: true,
            terminated: true,
            detail: "fake: cancelled".to_string(),
        })
    }

    fn close_session(&mut self, _session: &SessionHandle) -> Result<(), ContractError> {
        Ok(())
    }

    fn stop_attempt(&mut self, _attempt: &AttemptHandle) -> Result<StopEvidence, ContractError> {
        Ok(StopEvidence {
            process_group_gone: true,
            detail: "fake: stopped".to_string(),
        })
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("test-runs")
        .join(format!("watchdog-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create run dir");
    dir
}

// ---------------------------------------------------------------------------
// Watchdog: idle verdict + provider_health event
// ---------------------------------------------------------------------------

#[test]
fn silent_session_is_stalled_and_records_provider_health() {
    let dir = scratch_dir("idle");
    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let execution = ExecutionId::new("exec:watchdog-idle:1");

    let mut harness = FakeHarness::new(Behavior::Silent);
    let op = OperationHandle {
        id: OperationId::new("op:fake:turn/1"),
    };
    let outcome = poll_terminal(
        &mut harness,
        &op,
        Duration::from_millis(300),
        Duration::from_secs(30),
    );

    let stall = match outcome {
        PollOutcome::Stalled(stall) => stall,
        other => panic!("expected idle stall, got {other:?}"),
    };
    assert_eq!(stall.reason, PollStallReason::Idle, "{stall:?}");
    assert!(
        stall.idle_ms >= 300 && stall.idle_ms < 10_000,
        "idle bound respected: {stall:?}"
    );
    assert_eq!(stall.tokens, 0, "silent session has no tokens: {stall:?}");

    // The production helper emits the provider_health event for the stall.
    record_provider_health(&state_dir, &execution, "test/silent", "opencode", 1, &stall)
        .expect("record provider_health");

    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger");
    let events = ledger.events(&execution).expect("events");
    let health = events
        .iter()
        .find(|e| e.kind == "provider_health")
        .expect("provider_health event");
    assert_eq!(health.payload["model"], "test/silent");
    assert_eq!(health.payload["binding"], "opencode");
    assert_eq!(health.payload["attempt"], 1);
    assert_eq!(health.payload["reason"], "idle");
    assert!(health.payload["idle_ms"].as_u64().is_some(), "{health:?}");
    assert_eq!(health.payload["tokens"], 0);
    assert!(health.payload["wall_ms"].as_u64().is_some(), "{health:?}");
}

// ---------------------------------------------------------------------------
// Watchdog: session-gone fast-fail
// ---------------------------------------------------------------------------

#[test]
fn session_gone_fast_fails_without_waiting_for_idle() {
    let mut harness = FakeHarness::new(Behavior::SessionGone);
    let op = OperationHandle {
        id: OperationId::new("op:fake:turn/1"),
    };
    let idle = Duration::from_secs(60);
    let started = Instant::now();
    let outcome = poll_terminal(&mut harness, &op, idle, Duration::from_secs(600));
    let elapsed = started.elapsed();

    let stall = match outcome {
        PollOutcome::Stalled(stall) => stall,
        other => panic!("expected session-gone stall, got {other:?}"),
    };
    assert_eq!(stall.reason, PollStallReason::SessionGone, "{stall:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "fast-fail must not wait the idle bound: elapsed {elapsed:?}"
    );
    assert!(
        stall.wall_ms < idle.as_millis() as u64,
        "wall_ms {} << idle override {:?}",
        stall.wall_ms,
        idle
    );
}

#[test]
fn transient_poll_error_does_not_fast_fail() {
    let mut harness = FakeHarness::new(Behavior::Transient);
    let op = OperationHandle {
        id: OperationId::new("op:fake:turn/1"),
    };
    let outcome = poll_terminal(
        &mut harness,
        &op,
        Duration::from_millis(300),
        Duration::from_secs(30),
    );
    match outcome {
        PollOutcome::Stalled(stall) => {
            assert_eq!(
                stall.reason,
                PollStallReason::Idle,
                "transient errors keep waiting until idle: {stall:?}"
            );
        }
        other => panic!("expected idle stall, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Digest stall count
// ---------------------------------------------------------------------------

fn minimal_task(id: &str) -> Task {
    Task {
        task_id: TaskId::new(id),
        title: "Watchdog task".to_string(),
        directive: "directive/v0".to_string(),
        target: "agalma".to_string(),
        base_ref: "main".to_string(),
        acceptance: vec![],
        budget_usd: None,
        priority: TaskPriority::Normal,
        family: TaskFamily::Fix,
        max_attempts: 2,
        raw_block: String::new(),
    }
}

fn commit_event(state_dir: &Path, execution: &ExecutionId, kind: &str, payload: serde_json::Value) {
    let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger");
    ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: None,
            events: vec![ExecutionEvent {
                execution_id: execution.clone(),
                sequence: 0,
                kind: kind.to_string(),
                payload,
                recorded_at_unix_ms: 1_700_000_000_000,
            }],
            receipts: vec![],
            intents: vec![],
        })
        .expect("commit event");
}

#[test]
fn digest_counts_provider_health_stalls() {
    let dir = scratch_dir("digest");
    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let execution = ExecutionId::new("exec:watchdog-digest:1");

    for attempt in 1..=2u32 {
        commit_event(
            &state_dir,
            &execution,
            "provider_health",
            serde_json::json!({
                "model": "test/silent",
                "binding": "opencode",
                "attempt": attempt,
                "reason": "idle",
                "idle_ms": 120_000,
                "tokens": 0,
                "wall_ms": 120_000,
            }),
        );
    }

    let task = minimal_task("watchdog-digest");
    let digest = digest::record(&state_dir, &execution, &task, "test/silent", Verdict::Red)
        .expect("record digest");
    assert_eq!(
        digest.stalls, 2,
        "stall count derived from events: {digest:?}"
    );
    assert!(
        digest.summary_line().contains("stalls=2"),
        "summary line carries the stall count: {}",
        digest.summary_line()
    );
}

// ---------------------------------------------------------------------------
// Park projection: blocked + label (real bd)
// ---------------------------------------------------------------------------

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn stderr_of(out: &Output) -> String {
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

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for these tests but was not found");
}

/// Scratch `git` origin with a real isolated `bd` store and one task issue.
struct Scratch {
    origin: PathBuf,
    state_dir: PathBuf,
    id: String,
}

impl Scratch {
    fn new(name: &str, acceptance: &str) -> Scratch {
        require_bd();
        let dir = root().join("target").join("test-runs").join(name);
        // WARNING: deletes files, but only under `target/test-runs/`.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create run dir");
        let origin = dir.join("origin");
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

        let prefix = name.replace('-', "");
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

        let description = format!(
            "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n{acceptance}\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n"
        );
        let id = create_task(&origin, "Watchdog park", &description);
        Scratch {
            state_dir: dir.join("state"),
            origin,
            id,
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

    fn show(&self) -> serde_json::Value {
        let out = self.bd(&["show", &self.id, "--json", "--include-comments"]);
        assert!(out.status.success(), "bd show: {}", stderr_of(&out));
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("bd show json");
        value
            .as_array()
            .expect("show array")
            .first()
            .expect("one issue")
            .clone()
    }
}

fn create_task(origin: &Path, title: &str, description: &str) -> String {
    let out = Command::new("bd")
        .arg("--db")
        .arg(origin.join(".beads"))
        .args([
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            description,
            "--json",
        ])
        .current_dir(origin)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("spawn bd create");
    assert!(out.status.success(), "bd create: {}", stderr_of(&out));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("create json");
    value["id"].as_str().expect("created id").to_string()
}

#[test]
fn parked_task_projects_blocked_status_and_label() {
    let scratch = Scratch::new("watchdog-park", "  - sh -c 'grep -q MAGIC src/lib.rs'");
    let out = Command::new(bin())
        .args([
            "work",
            "--repo",
            scratch.origin.to_str().expect("origin utf8"),
            "--state-dir",
            scratch.state_dir.to_str().expect("state utf8"),
            "--once",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .output()
        .expect("run agalma work");
    assert!(
        !out.status.success(),
        "--once exits nonzero on park: {}",
        stderr_of(&out)
    );

    let issue = scratch.show();
    assert_eq!(
        issue["status"], "blocked",
        "parked task projects blocked (not in_progress): {issue}"
    );
    let labels: Vec<&str> = issue["labels"]
        .as_array()
        .map(|l| l.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(labels.contains(&"parked"), "parked label: {issue}");
    let comments = issue["comments"]
        .as_array()
        .map(|cs| {
            cs.iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    assert!(
        comments.contains("parked:"),
        "park reason comment present: {comments}"
    );
    assert!(
        comments.contains("stalls="),
        "digest stall count rides the terminal comment: {comments}"
    );
}

// ---------------------------------------------------------------------------
// Parked projection reconcile (idempotent), no work run
// ---------------------------------------------------------------------------

#[test]
fn parked_projection_reconcile_maps_and_is_idempotent() {
    require_bd();
    let dir = scratch_dir("reconcile");
    let origin = dir.join("origin");
    fs::create_dir_all(origin.join("src")).expect("origin src");
    fs::write(origin.join("src").join("lib.rs"), "//! seed\n").expect("seed");
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
            "wdpark",
            "--non-interactive",
            "--skip-agents",
            "--skip-hooks",
        ])
        .current_dir(&origin)
        .env("BD_NON_INTERACTIVE", "1")
        .output()
        .expect("bd init");
    assert!(init.status.success(), "bd init: {}", stderr_of(&init));
    let id = create_task(&origin, "Parked drift", "no block");
    let task_id = TaskId::new(id.clone());

    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let exec = ExecutionId::derive(&task_id, 1);
    {
        let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger");
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(ExecutionRecord {
                    execution_id: exec.clone(),
                    task_id: task_id.clone(),
                    generation: 1,
                    phase: ExecutionPhase::Parked,
                    state: ExecutionState::Parked,
                    attempt: 2,
                    revision: 0,
                }),
                events: vec![
                    ExecutionEvent {
                        execution_id: exec.clone(),
                        sequence: 0,
                        kind: "execution_created".to_string(),
                        payload: serde_json::json!({
                            "task_id": task_id,
                            "generation": 1,
                            "lease_generation": 1,
                            "attempt": 1,
                        }),
                        recorded_at_unix_ms: 1_700_000_000_000,
                    },
                    ExecutionEvent {
                        execution_id: exec.clone(),
                        sequence: 0,
                        kind: "parked".to_string(),
                        payload: serde_json::json!({
                            "reason": "max_attempts (2) exhausted",
                            "attempt": 2,
                        }),
                        recorded_at_unix_ms: 1_700_000_000_000,
                    },
                ],
                receipts: vec![],
                intents: vec![],
            })
            .expect("commit parked record");
    }

    // Crash drift: the board still shows a claimed in-progress issue.
    let bd = |args: &[&str]| {
        let out = Command::new("bd")
            .arg("--db")
            .arg(origin.join(".beads"))
            .args(args)
            .current_dir(&origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd");
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        out
    };
    bd(&["update", &id, "--status", "in_progress"]);

    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("reopen ledger");
    let mut queue = agalma_taskqueue::BdTaskQueue::new(&origin);

    let first = agalma_conductor::reconcile::reconcile_projection(&ledger, &mut queue)
        .expect("first reconcile");
    assert_eq!(first.checked, 1, "{first:?}");
    assert!(first.fixed >= 1, "parked projection repaired: {first:?}");

    let projection = queue.projection(&task_id).expect("projection");
    assert_eq!(projection.status, "blocked", "ledger wins: blocked");
    assert!(projection.has_label("parked"), "parked label repaired");
    assert!(
        projection.has_comment(&format!("phase=parked execution={exec} lease=1")),
        "parked phase comment written: {:?}",
        projection.comments
    );

    // Idempotent: a second reconcile makes no writes.
    let second = agalma_conductor::reconcile::reconcile_projection(&ledger, &mut queue)
        .expect("second reconcile");
    assert_eq!(second.checked, 1);
    assert_eq!(second.fixed, 0, "no drift on the second pass: {second:?}");
    assert_eq!(
        second.stale, 0,
        "no corrections on the second pass: {second:?}"
    );
}
