//! H1 (`agalma-zxi.2`) watchdog, provider-health, and park-projection tests.
//!
//! Deterministic and native: a test-local [`HarnessApi`] fake drives the
//! progress watchdog directly (no vendor/network); the park and reconcile cases
//! run the real `agalma work` / `agalma reconcile` paths against a scratch
//! `git` + isolated `bd` store under `target/test-runs/watchdog-*` (never
//! `/tmp`; scratch dirs auto-remove on success, kept on failure). Covers:
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
use std::time::{Duration, Instant};

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
use agalma_ipc::TaskQueueOps;
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;

use agalma_conductor::digest::{self, Verdict};
use agalma_conductor::phases::{
    poll_terminal, record_provider_health, PollOutcome, PollStallReason,
};
use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, ProtectedInputs, PROTECTED_BASELINE_FORMAT,
    PROTECTED_INPUTS_FORMAT,
};
use agalma_contracts::digest::sha256_digest;

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

fn scratch_dir(name: &str) -> ScratchDir {
    ScratchDir::new(&format!("watchdog-{name}"))
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
        None,
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
    let outcome = poll_terminal(&mut harness, &op, idle, Duration::from_secs(600), None);
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
        None,
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

// ---------------------------------------------------------------------------
// SF.6 operator protected inputs (watchdog-local fixture seeding)
//
// Repository work is fail-closed: `agalma work --once` refuses unless the
// operator `ProtectedInputs/v1` manifest (`<state_dir>/protected-inputs.json`)
// carries an entry whose task/repository/base identity matches the fixture and
// freezes a complete `ProtectedEntry` (including a valid baseline). The
// watchdog fixture seeds that locally, without touching the shared
// `tests/common` helpers.
// ---------------------------------------------------------------------------

/// An immutable protected *behavioral* judge: it independently re-checks the
/// scripted candidate's fixed behavior (`answer() == 42`) against the pinned
/// candidate tree. Frozen as a `ProtectedBaseline/v1` file and launched through
/// `$AGALMA_PROTECTED_DIR` (outside the candidate writable root), so the
/// candidate cannot substitute its own always-pass tests for the judge.
const PROTECTED_RUNNER: &str = "#!/bin/sh\n\
set -eu\n\
grep -q 42 \"$AGALMA_CANDIDATE_DIR/src/lib.rs\"\n";
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

/// The operator `ProtectedEntry` whose identity must match a scratch fixture
/// exactly: the created bd task id, the canonicalized origin path, and the
/// resolved `main` base SHA. `allowed_paths` admits the scripted candidate
/// change (`src/lib.rs`); nothing the candidate touches is protected.
fn protected_entry(task_id: &str, repository_root: &str, base_sha: &str) -> ProtectedEntry {
    ProtectedEntry {
        task_id: task_id.to_string(),
        repository_root: repository_root.to_string(),
        base_sha: base_sha.to_string(),
        allowed_paths: vec!["src".to_string()],
        protected_paths: Vec::new(),
        baseline: protected_baseline(),
    }
}

/// Isolated XDG config base for a scratch fixture.
fn config_home(state_dir: &Path) -> PathBuf {
    state_dir.parent().expect("state parent").join("xdg-config")
}

/// Write an operator `ProtectedInputs/v1` manifest at the isolated XDG config
/// path production reads.
fn write_protected_inputs(state_dir: &Path, entries: Vec<ProtectedEntry>) {
    let inputs = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries,
    };
    let bytes = serde_json::to_vec(&inputs).expect("serialize protected inputs");
    let dir = config_home(state_dir).join("agalma");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("protected-inputs.json"), bytes).expect("write protected inputs");
}

/// Low-level explicit-directory loader fixture (not the production source).
fn write_low_level_inputs(state_dir: &Path, entries: Vec<ProtectedEntry>) {
    let inputs = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries,
    };
    let bytes = serde_json::to_vec(&inputs).expect("serialize protected inputs");
    fs::create_dir_all(state_dir).expect("state dir");
    fs::write(state_dir.join("protected-inputs.json"), bytes).expect("write protected inputs");
}

/// Scratch `git` origin with a real isolated `bd` store and one task issue.
struct Scratch {
    origin: PathBuf,
    state_dir: PathBuf,
    id: String,
    _dir: ScratchDir,
}

impl Scratch {
    /// A scratch fixture with a matching operator manifest seeded (the normal
    /// case: repository work reaches verification).
    fn new(name: &str, acceptance: &str) -> Scratch {
        let scratch = Scratch::new_raw(name, acceptance);
        scratch.seed_protected_inputs(&scratch.id);
        scratch
    }

    /// A scratch fixture with NO operator manifest (for missing-input refusal
    /// coverage).
    fn new_raw(name: &str, acceptance: &str) -> Scratch {
        require_bd();
        let dir = ScratchDir::new(name);
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

    /// Write an operator `ProtectedInputs/v1` manifest into the isolated state
    /// directory production reads.
    fn write_manifest(&self, entries: Vec<ProtectedEntry>) {
        write_protected_inputs(&self.state_dir, entries);
    }

    /// Seed a valid manifest whose identity matches the created task.
    fn seed_protected_inputs(&self, task_id: &str) {
        let origin = fs::canonicalize(&self.origin)
            .expect("canonical origin")
            .to_string_lossy()
            .into_owned();
        let out = Command::new("git")
            .args(["rev-parse", "main"])
            .current_dir(&self.origin)
            .output()
            .expect("git rev-parse");
        assert!(out.status.success(), "git rev-parse main: {}", stderr_of(&out));
        let base_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        self.write_manifest(vec![protected_entry(task_id, &origin, &base_sha)]);
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
        .env("XDG_CONFIG_HOME", config_home(&scratch.state_dir))
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
    let mut queue = agalma_conductor::taskqueue_process::TaskQueueHandle::in_process(&origin);

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

/// Missing and identity-mismatched operator protected inputs continue to refuse
/// repository work over the real `agalma work --once` path (SF.6 fail-closed):
/// an absent manifest never authorizes repository work, and a present manifest
/// whose task/repository/base identity does not match the fixture refuses.
#[test]
fn watchdog_work_refuses_missing_or_mismatched_protected_inputs() {
    fn work_once(scratch: &Scratch) -> Output {
        Command::new(bin())
            .args([
                "work",
                "--repo",
                scratch.origin.to_str().expect("origin utf8"),
                "--state-dir",
                scratch.state_dir.to_str().expect("state utf8"),
                "--once",
            ])
            .env("AGALMA_ACTIVITY_MODE", "scripted")
            .env("XDG_CONFIG_HOME", config_home(&scratch.state_dir))
            .output()
            .expect("run agalma work")
    }

    // Missing manifest: an absent operator manifest never authorizes work.
    let missing = Scratch::new_raw("watchdog-refuse-missing", "  - sh -c 'true'");
    let out = work_once(&missing);
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
    let mismatch = Scratch::new_raw("watchdog-refuse-mismatch", "  - sh -c 'true'");
    let origin = fs::canonicalize(&mismatch.origin)
        .expect("canonical origin")
        .to_string_lossy()
        .into_owned();
    mismatch.write_manifest(vec![protected_entry(&mismatch.id, &origin, &"0".repeat(40))]);
    let out = work_once(&mismatch);
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

/// Deterministic generated-input property (seed `0x5f6_wdch`, case budget 48)
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
fn watchdog_protected_fixture_seeding_property_over_generated_cases() {
    // Design seed label `0x5f6_wdch`; the non-hex letters are their ASCII bytes.
    const SEED: u64 = 0x5f6_7764_6368;
    const CASES: usize = 48;

    const TASKS: [&str; 4] = ["task.a", "task-b", "task.c", "t"];
    const REPOS: [&str; 4] = ["/repo/one", "/repo two", "/r", "/deep/nested/origin"];
    const ALLOWED: [&str; 4] = ["src", "crates", "a/b", "src/lib.rs"];
    const PROTECTED: [&str; 3] = ["tests", "docs/protected.md", "baseline"];

    let dir = scratch_dir("watchdog-fixture-property");
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

        write_low_level_inputs(&state_dir, vec![entry.clone()]);
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

    let absent = scratch_dir("watchdog-fixture-absent");
    assert!(ProtectedInputs::load(&absent).is_err());
}
