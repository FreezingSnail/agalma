//! `agalma-5fy.5`: fail-closed stop admission (`Running` / `Stopped` /
//! `Unavailable`) for the epic sweep, the scoped designer, the review gate, the
//! concierge intake, and the shared confined session seam.
//!
//! Every case drives a real production entry point ([`sweep_epics`],
//! [`sweep_epic`], [`review_epics`], [`run_concierge`], [`run_session`]) against a
//! scratch `git` + isolated `bd` workspace under `target/test-runs/` (never
//! `/tmp`). The designer/reviewer/concierge sessions are scripted seams (no
//! harness, no network, no paid call).
//!
//! Failure paths inject an *established but unreadable* ledger — a genuinely
//! corrupt SQLite file, a schema-incompatible one, and a **failing bound
//! `LedgerApi`** whose reads error even though a readable database file is
//! present — with `STOP` absent, and assert **no** session/action launch and
//! **no** backlog mutation. Fresh absent state is explicitly initialized before
//! admission and is distinct from unreadable established state. A successful
//! review verdict that arrives after the stop observation flips to
//! `Stopped`/`Unavailable` is never projected. Mid-session stop / explicit human
//! resume / bounded session cleanup are covered alongside.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Once;

use agalma_conductor::concierge::{run_concierge, ConciergeRequest, ConciergeSession};
use agalma_conductor::ledger_process::LedgerHandle;
use agalma_conductor::review::{
    review_epics, review_epics_with_stop, ReviewRequest, ReviewerSession,
};
use agalma_conductor::session::{run_session, LiveSession, SessionSpec};
use agalma_conductor::ledger_process::LedgerTransport;
use agalma_conductor::stop::{
    bind_stop_control, bind_stop_control_with, BoundStopControl, StopControl, StopDecision,
};
use agalma_conductor::sweep::{
    sweep_epic, sweep_epics, sweep_epics_with_stop, DesignerOutcome, DesignerRequest,
    DesignerSession, PlannerGate,
};
use agalma_contracts::ids::ArtifactRef;
use agalma_contracts::{
    BindingRecord, CommitBatch, CommitReceipt, ContractError, ControlReceipt, DecisionOutcome,
    DecisionRequest, DigestRecord, DirectiveRow, DispatchIntent, ExecutionEvent, ExecutionId,
    ExecutionRecord, LedgerApi, OperationId, OperationReceipt, StateSnapshot, TaskId, TimerRecord,
};use agalma_testkit::ScratchDir;
use serde_json::Value;

/// The focused runner must be hermetic: select the in-process ledger binding
/// once so the production `bind_stop_control` does not spawn a server per call.
fn ensure_in_process_ledger() {
    static INIT: Once = Once::new();
    INIT.call_once(|| std::env::set_var("AGALMA_LEDGER_TRANSPORT", "in-process"));
}

fn fresh(name: &str) -> ScratchDir {
    ensure_in_process_ledger();
    ScratchDir::new(&format!("sf-stop-{name}"))
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
    assert!(out.status.success(), "{program} {args:?}: {}", stderr_of(&out));
}

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for these tests but was not found");
}

/// An isolated git + bd workspace plus a conductor state dir.
struct Scratch {
    repo: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Scratch {
    fn new(name: &str) -> Self {
        require_bd();
        let dir = fresh(name);
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        run_ok("git", &repo, &["init", "-q"]);
        run_ok("git", &repo, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &repo, &["config", "user.name", "Agalma"]);

        let out = Command::new("bd")
            .args([
                "init",
                "--prefix",
                "sfstop",
                "--non-interactive",
                "--skip-agents",
                "--skip-hooks",
            ])
            .current_dir(&repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd init");
        assert!(out.status.success(), "bd init: {}", stderr_of(&out));

        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir).expect("state dir");
        Scratch {
            repo,
            state_dir,
            _dir: dir,
        }
    }

    fn bd(&self, args: &[&str]) -> std::process::Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.repo.join(".beads"))
            .args(args)
            .current_dir(&self.repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn bd_ok(&self, args: &[&str]) -> String {
        let out = self.bd(args);
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "git {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(&self, message: &str) -> String {
        run_ok(
            "git",
            &self.repo,
            &["commit", "--allow-empty", "-q", "-m", message],
        );
        self.git(&["rev-parse", "HEAD"])
    }

    fn create_epic(&self, title: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "epic",
            "-p",
            "2",
            "--description",
            "epic design",
            "--json",
        ]);
        assert!(out.status.success(), "bd create epic: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("epic json");
        value["id"].as_str().expect("epic id").to_string()
    }

    fn create_closed_child(&self, parent: &str, title: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--parent",
            parent,
            "--description",
            &low_desc(title),
            "--labels",
            "staged,difficulty:low",
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        let id = value["id"].as_str().expect("child id").to_string();
        self.bd_ok(&["close", &id, "--reason", "landed"]);
        id
    }

    fn show(&self, id: &str) -> Value {
        let stdout = self.bd_ok(&["show", id, "--json", "--include-comments"]);
        let value: Value = serde_json::from_str(&stdout).expect("show json");
        value
            .as_array()
            .expect("show array")
            .first()
            .expect("one issue")
            .clone()
    }

    fn children(&self, epic: &str) -> Vec<Value> {
        let stdout = self.bd_ok(&["list", "--parent", epic, "--all", "--json", "--limit", "0"]);
        serde_json::from_str(&stdout).expect("children json")
    }

    fn stop_path(&self) -> PathBuf {
        self.state_dir.join("STOP")
    }

    fn ledger_path(&self) -> PathBuf {
        self.state_dir.join("ledger.sqlite")
    }

    fn set_kill_latch(&self, latched: bool) {
        let mut ledger = agalma_ledger::SqliteLedger::open(self.ledger_path()).expect("open ledger");
        ledger.set_kill_latch(latched).expect("set latch");
    }

    fn kill_latch(&self) -> bool {
        agalma_ledger::SqliteLedger::open(self.ledger_path())
            .expect("open ledger")
            .kill_latch()
            .expect("read latch")
    }

    /// Establish a *readable* valid ledger, then force an incompatible version.
    fn force_schema_version(&self, version: u32) {
        drop(agalma_ledger::SqliteLedger::open(self.ledger_path()).expect("open ledger"));
        let conn = rusqlite::Connection::open(self.ledger_path()).expect("reopen raw");
        conn.execute(
            "UPDATE meta SET value=?1 WHERE key='schema_version'",
            [version.to_string()],
        )
        .expect("force schema version");
    }

    /// Establish an unreadable ledger: a non-SQLite file at the ledger path.
    fn corrupt_ledger(&self) {
        fs::write(self.ledger_path(), b"not a sqlite database").expect("write corrupt ledger");
    }

    /// Establish a readable valid ledger (used to prove reads fail even though
    /// the database file is present and readable).
    fn establish_readable_ledger(&self) {
        drop(agalma_ledger::SqliteLedger::open(self.ledger_path()).expect("open ledger"));
    }

    fn labels(issue: &Value) -> Vec<String> {
        issue["labels"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn has_label(issue: &Value, label: &str) -> bool {
    Scratch::labels(issue).contains(&label.to_string())
}

/// Build an epic with one closed staged child and a recorded review baseline.
fn landed_epic(scratch: &Scratch, name: &str) -> String {
    let base = scratch.commit("base");
    let epic = scratch.create_epic(name);
    scratch.create_closed_child(&epic, "child 0");
    scratch.commit("work");
    scratch.bd_ok(&[
        "update",
        &epic,
        "--add-label",
        &format!("review-start:{base}"),
    ]);
    epic
}

fn low_desc(name: &str) -> String {
    format!(
        "# Scope\nDo exactly one small thing for {name}.\n\n\
         # Files\ncrates/foo/src/{name}.rs\n\n\
         # Contract\nRun the acceptance command.\n\n\
         # Invariants\nNo behavior change outside scope.\n\n\
         # Non-goals\nNo refactor.\n\n\
         # MACHINE\nprovides:\nconsumes:\nfiles: crates/foo/src/{name}.rs\ntier: low\n"
    )
}

fn gate() -> PlannerGate {
    PlannerGate::new(true, true, None)
}

// ---------------------------------------------------------------------------
// Scripted session seams (no harness, no network, no paid call)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ScriptedDesigner {
    called: RefCell<Vec<String>>,
    stop_on_first: bool,
    stopped: RefCell<bool>,
}

impl ScriptedDesigner {
    fn inert() -> Self {
        ScriptedDesigner {
            called: RefCell::new(Vec::new()),
            stop_on_first: false,
            stopped: RefCell::new(false),
        }
    }

    fn stops_on_first() -> Self {
        ScriptedDesigner {
            called: RefCell::new(Vec::new()),
            stop_on_first: true,
            stopped: RefCell::new(false),
        }
    }

    fn called(&self) -> Vec<String> {
        self.called.borrow().clone()
    }
}

impl DesignerSession for ScriptedDesigner {
    fn run(&self, request: &DesignerRequest) -> Result<DesignerOutcome, ContractError> {
        self.called.borrow_mut().push(request.epic.clone());
        if !self.stop_on_first {
            return Ok(DesignerOutcome::Completed);
        }
        let mut stopped = self.stopped.borrow_mut();
        if *stopped {
            return Ok(DesignerOutcome::Completed);
        }
        *stopped = true;
        let out = Command::new("bd")
            .arg("--db")
            .arg(request.repo.join(".beads"))
            .args([
                "create",
                "partial",
                "-t",
                "task",
                "-p",
                "2",
                "--parent",
                &request.epic,
                "--description",
                "partial child",
                "--design",
                "small change",
                "--labels",
                "staged,difficulty:low",
                "--defer",
                "2099-01-01",
            ])
            .current_dir(&request.repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd in designer");
        assert!(out.status.success(), "designer child: {}", stderr_of(&out));
        fs::write(request.state_dir.join("STOP"), b"stop").expect("write STOP in-session");
        Ok(DesignerOutcome::Completed)
    }
}

#[derive(Default)]
struct ScriptedReviewer {
    called: RefCell<Vec<String>>,
}

impl ScriptedReviewer {
    fn new() -> Self {
        ScriptedReviewer::default()
    }
    fn called(&self) -> Vec<String> {
        self.called.borrow().clone()
    }
}

impl ReviewerSession for ScriptedReviewer {
    fn run(&self, request: &ReviewRequest) -> Result<String, ContractError> {
        self.called.borrow_mut().push(request.epic.clone());
        Ok("REVIEW: APPROVED".to_string())
    }
}

#[derive(Default)]
struct ScriptedConcierge {
    called: RefCell<Vec<String>>,
}

impl ScriptedConcierge {
    fn new() -> Self {
        ScriptedConcierge::default()
    }
    fn called(&self) -> Vec<String> {
        self.called.borrow().clone()
    }
}

impl ConciergeSession for ScriptedConcierge {
    fn run(&self, request: &ConciergeRequest) -> Result<String, ContractError> {
        self.called.borrow_mut().push(request.goal.clone());
        Ok("epic: sfstop-1\ndesign: design\n".to_string())
    }
}

/// A fixed stop decision for the session seam.
struct ScriptedStop(StopDecision);

impl StopControl for ScriptedStop {
    fn stopped(&self) -> bool {
        self.0.is_denied()
    }
    fn decision(&self) -> StopDecision {
        self.0.clone()
    }
}

/// A control whose observation flips when the reviewer session runs: `Running`
/// for the pre-session guards, then the injected denied decision.
#[derive(Clone)]
struct LateStop {
    flip: Rc<RefCell<Option<StopDecision>>>,
}

impl LateStop {
    fn running() -> Self {
        LateStop {
            flip: Rc::new(RefCell::new(None)),
        }
    }
    fn trigger(&self, decision: StopDecision) {
        *self.flip.borrow_mut() = Some(decision);
    }
}

impl StopControl for LateStop {
    fn stopped(&self) -> bool {
        !self.decision().permits_admission()
    }
    fn decision(&self) -> StopDecision {
        self.flip
            .borrow()
            .clone()
            .unwrap_or(StopDecision::Running)
    }
}

/// A reviewer that flips the stop observation when it runs, then returns a valid
/// approval verdict.
struct StoppingReviewer {
    control: LateStop,
    decision: StopDecision,
}

impl ReviewerSession for StoppingReviewer {
    fn run(&self, _request: &ReviewRequest) -> Result<String, ContractError> {
        self.control.trigger(self.decision.clone());
        Ok("REVIEW: APPROVED".to_string())
    }
}

/// A bound `LedgerApi` whose reads fail while a readable database file exists.
/// Records the reads it observes; every other method is unused by the control.
struct FailingLedger {
    calls: RefCell<Vec<&'static str>>,
}

impl FailingLedger {
    fn new() -> Self {
        FailingLedger {
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl LedgerApi for FailingLedger {
    fn schema_version(&self) -> Result<u32, ContractError> {
        self.calls.borrow_mut().push("schema_version");
        Err(ContractError::KnownFailure("ledger service unavailable".to_string()))
    }

    fn kill_latch(&self) -> Result<bool, ContractError> {
        self.calls.borrow_mut().push("kill_latch");
        Err(ContractError::KnownFailure("ledger service unavailable".to_string()))
    }

    fn commit(&mut self, _: CommitBatch) -> Result<CommitReceipt, ContractError> {
        unimplemented!("unused")
    }
    fn execution(&self, _: &ExecutionId) -> Result<Option<ExecutionRecord>, ContractError> {
        unimplemented!("unused")
    }
    fn executions(&self) -> Result<Vec<ExecutionRecord>, ContractError> {
        unimplemented!("unused")
    }
    fn pending_intents(&self) -> Result<Vec<DispatchIntent>, ContractError> {
        unimplemented!("unused")
    }
    fn events(&self, _: &ExecutionId) -> Result<Vec<ExecutionEvent>, ContractError> {
        unimplemented!("unused")
    }
    fn operation_receipt(&self, _: &OperationId) -> Result<Option<OperationReceipt>, ContractError> {
        unimplemented!("unused")
    }
    fn lease_generation(&self, _: &TaskId) -> Result<u32, ContractError> {
        unimplemented!("unused")
    }
    fn begin_lease(&mut self, _: &TaskId) -> Result<u32, ContractError> {
        unimplemented!("unused")
    }
    fn set_kill_latch(&mut self, _: bool) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn upsert_binding(&mut self, _: &BindingRecord) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn bindings(&self) -> Result<Vec<BindingRecord>, ContractError> {
        unimplemented!("unused")
    }
    fn record_decision(&mut self, _: &DecisionRequest) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn record_decision_outcome(
        &mut self,
        _: &OperationId,
        _: &DecisionOutcome,
    ) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn decision_outcome(&self, _: &OperationId) -> Result<Option<DecisionOutcome>, ContractError> {
        unimplemented!("unused")
    }
    fn record_digest(&mut self, _: &DigestRecord) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn digests_for_execution(&self, _: &ExecutionId) -> Result<Vec<DigestRecord>, ContractError> {
        unimplemented!("unused")
    }
    fn kernel_meta_get(&self, _: &str) -> Result<Option<String>, ContractError> {
        unimplemented!("unused")
    }
    fn kernel_meta_set(&mut self, _: &str, _: &str) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn insert_timer(&mut self, _: &TimerRecord) -> Result<bool, ContractError> {
        unimplemented!("unused")
    }
    fn timer(&self, _: &str) -> Result<Option<TimerRecord>, ContractError> {
        unimplemented!("unused")
    }
    fn due_timers(&self, _: u64) -> Result<Vec<TimerRecord>, ContractError> {
        unimplemented!("unused")
    }
    fn mark_timer_fired(&mut self, _: &str) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn cancel_timers_for_execution(&mut self, _: &ExecutionId) -> Result<u64, ContractError> {
        unimplemented!("unused")
    }
    fn next_deadline(&self) -> Result<Option<u64>, ContractError> {
        unimplemented!("unused")
    }
    fn upsert_directive(&mut self, _: &DirectiveRow) -> Result<bool, ContractError> {
        unimplemented!("unused")
    }
    fn directive(&self, _: &str) -> Result<Option<DirectiveRow>, ContractError> {
        unimplemented!("unused")
    }
    fn consume_dispatch_intent(&mut self, _: &OperationId) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn record_control_receipt(&mut self, _: &ControlReceipt) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
    fn control_receipts(&self) -> Result<Vec<ControlReceipt>, ContractError> {
        unimplemented!("unused")
    }
    fn export_state(&self) -> Result<StateSnapshot, ContractError> {
        unimplemented!("unused")
    }
    fn import_state(&mut self, _: &StateSnapshot) -> Result<(), ContractError> {
        unimplemented!("unused")
    }
}

fn session_spec() -> SessionSpec {
    SessionSpec {
        role: "designer".to_string(),
        model: "test/offline".to_string(),
        repo: PathBuf::from("."),
        dir_key: "sweep/stopped".to_string(),
        socket_name: "harness-sf-stop.sock".to_string(),
        operation_id: OperationId::new("op:sf-stop"),
        prompt_path: PathBuf::from("prompt.md"),
        input_ref: ArtifactRef::derive("designer-turn"),
        tool_policy_ref: ArtifactRef::derive("tool-policy-designer"),
        genome_ref: ArtifactRef::derive("genome"),
        constraints_ref: ArtifactRef::derive("platform-constraints"),
        output_path: None,
        error_label: "designer session".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Unavailable established state (STOP absent) suppresses dispatch everywhere
// ---------------------------------------------------------------------------

#[test]
fn unreadable_established_ledger_suppresses_fleet_sweep() {
    let scratch = Scratch::new("corrupt-fleet");
    let epic = scratch.create_epic("Candidate epic");
    scratch.corrupt_ledger();
    assert!(!scratch.stop_path().exists(), "STOP absent; only the ledger is unreadable");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "unreadable ledger suppresses the sweep: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(report.epics_seen.is_empty(), "no epic examined");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
    assert!(
        !has_label(&scratch.show(&epic), "decomp-inflight"),
        "no attempt marker projected"
    );
}

#[test]
fn incompatible_ledger_suppresses_scoped_designer() {
    let scratch = Scratch::new("incompatible-designer");
    let epic = scratch.create_epic("Candidate epic");
    scratch.force_schema_version(999);

    let designer = ScriptedDesigner::inert();
    let report = sweep_epic(&scratch.repo, &scratch.state_dir, &epic, &gate(), &designer)
        .expect("scoped designer");

    assert!(report.stopped, "incompatible ledger suppresses: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

#[test]
fn unreadable_ledger_suppresses_review_gate() {
    let scratch = Scratch::new("corrupt-review");
    let reviewer = ScriptedReviewer::new();
    scratch.corrupt_ledger();

    let report =
        review_epics(&scratch.repo, &scratch.state_dir, &gate(), &reviewer).expect("review");

    assert!(report.stopped, "unreadable ledger suppresses review: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(reviewer.called().is_empty(), "zero reviewer calls");
}

#[test]
fn unreadable_ledger_suppresses_concierge_intake() {
    let scratch = Scratch::new("corrupt-concierge");
    scratch.corrupt_ledger();
    let session = ScriptedConcierge::new();

    let err = run_concierge(&scratch.repo, &scratch.state_dir, "vague goal", &gate(), &session)
        .expect_err("unreadable ledger must suppress the intake");

    assert!(
        err.to_string().contains("stop state unavailable"),
        "distinct reason: {err}"
    );
    assert!(session.called().is_empty(), "no session spawned");
}

/// A **failing bound LedgerApi** (reads error) with a readable database file
/// present suppresses the fleet sweep without launching or mutating.
#[test]
fn failing_bound_service_read_suppresses_fleet_sweep() {
    let scratch = Scratch::new("failing-bound-fleet");
    let epic = scratch.create_epic("Candidate epic");
    scratch.establish_readable_ledger();
    assert!(scratch.ledger_path().is_file(), "readable database file present");

    let failing = FailingLedger::new();
    let control: BoundStopControl<FailingLedger> = BoundStopControl::new(&scratch.state_dir, failing);
    assert!(matches!(control.decision(), StopDecision::Unavailable(_)));
    let designer = ScriptedDesigner::inert();
    let report = sweep_epics_with_stop(
        &scratch.repo,
        &scratch.state_dir,
        &gate(),
        &designer,
        &control,
    )
    .expect("sweep");

    assert!(report.stopped, "failing service read suppresses: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(report.epics_seen.is_empty(), "no epic examined");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

/// Fresh absent state is explicitly **initialized** before admission: the
/// selected binding creates the database, records a durable establishment marker
/// outside it, reads the clear latch, then the sweep dispatches.
#[test]
fn fresh_virgin_state_is_explicitly_initialized_then_running() {
    let scratch = Scratch::new("fresh");
    let _epic = scratch.create_epic("Candidate epic");
    assert!(!scratch.ledger_path().exists(), "no ledger yet");
    assert!(
        !scratch.state_dir.join("ledger.established").exists(),
        "no establishment marker yet"
    );

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(!report.stopped, "fresh state is running: {report:?}");
    assert!(
        scratch.ledger_path().is_file(),
        "fresh state is explicitly initialized before admission"
    );
    assert!(
        scratch.state_dir.join("ledger.established").is_file(),
        "a durable establishment marker is recorded outside the database"
    );
    assert!(
        !designer.called().is_empty(),
        "a fresh initialized ledger permits dispatch: {designer:?}"
    );
}

/// Losing a previously initialized/latched database (marker durable, database
/// gone, STOP absent) is `Unavailable`: it must never become Running through a
/// create-if-missing path, and no launch or queue mutation occurs.
#[test]
fn missing_established_latched_ledger_is_unavailable() {
    let scratch = Scratch::new("lost-established");
    let epic = scratch.create_epic("Candidate epic");

    // Explicitly initialize (records the marker), then latch and lose the db.
    let control = bind_stop_control(&scratch.state_dir);
    assert_eq!(control.decision(), StopDecision::Running, "fresh init is running");
    drop(control);
    scratch.set_kill_latch(true);
    assert!(scratch.kill_latch(), "latch persisted before loss");
    fs::remove_file(scratch.ledger_path()).expect("lose the database");
    assert!(
        scratch.state_dir.join("ledger.established").is_file(),
        "the durable establishment marker survives database loss"
    );
    assert!(!scratch.stop_path().exists(), "STOP absent");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "lost established state suppresses: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
    assert!(
        !scratch.ledger_path().exists(),
        "no replacement database is created for lost established state"
    );
}

/// A partial/ambiguous initialization (leftover temporary marker) refuses.
#[test]
fn incomplete_initialization_is_unavailable() {
    let scratch = Scratch::new("partial-init");
    let epic = scratch.create_epic("Candidate epic");
    fs::write(
        scratch.state_dir.join("ledger.established.tmp"),
        b"{\"version\":1}",
    )
    .expect("write partial marker");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "partial initialization suppresses: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

/// A failing establishment durability barrier (directory sync injection) is
/// `Unavailable` with no launch or queue effect, even though the database was
/// initialized.
#[test]
fn establishment_durability_failure_is_unavailable() {
    let scratch = Scratch::new("establish-durability-fail");
    let epic = scratch.create_epic("Candidate epic");
    fs::write(
        scratch.state_dir.join("ledger.establish.fail"),
        b"inject",
    )
    .expect("write durability injection");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "durability failure suppresses: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(report.epics_seen.is_empty(), "no epic examined");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

/// A durability failure that published the marker but not the barrier must stay
/// `Unavailable` across repeated binds and sweeps (still zero launch/queue
/// effects): the retry may not treat the visible-but-unconfirmed marker as
/// Running.
#[test]
fn establishment_durability_failure_persists_across_repeated_binds() {
    let scratch = Scratch::new("establish-durability-retry");
    let epic = scratch.create_epic("Candidate epic");
    fs::write(
        scratch.state_dir.join("ledger.establish.fail"),
        b"inject",
    )
    .expect("write durability injection");

    // First bind: fresh initialization fails at the durability barrier, but the
    // marker was already renamed into place (visible, unconfirmed).
    let first = bind_stop_control(&scratch.state_dir);
    assert!(
        matches!(first.decision(), StopDecision::Unavailable(_)),
        "first bind is unavailable"
    );
    drop(first);
    assert!(
        scratch.state_dir.join("ledger.established").is_file(),
        "the marker was published before the barrier failed"
    );

    // Second bind: classify sees Established; the same barrier must still refuse.
    let second = bind_stop_control(&scratch.state_dir);
    assert!(
        matches!(second.decision(), StopDecision::Unavailable(_)),
        "the retry must not become Running on the unconfirmed marker"
    );
    drop(second);

    // A sweep stays suppressed with zero launch/queue effects.
    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");
    assert!(report.stopped, "retry sweep stays suppressed: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(report.epics_seen.is_empty(), "no epic examined");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

/// A fresh state-directory creation error is `Unavailable` with no launch or
/// queue effect.
#[test]
fn fresh_state_directory_creation_error_is_unavailable() {
    let scratch = Scratch::new("ro-parent");
    let epic = scratch.create_epic("Candidate epic");

    // A read-only parent: create_dir_all(<ro>/state) fails, and `<ro>/state`
    // never exists, so admission classifies it fresh then refuses on creation.
    let ro = scratch.state_dir.parent().expect("scratch dir").join("ro");
    fs::create_dir_all(&ro).expect("ro parent");
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).expect("chmod ro");
    let missing_state = ro.join("state");
    assert!(!missing_state.exists(), "state must not exist under the ro parent");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &missing_state, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "creation error suppresses: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
    assert!(!missing_state.exists(), "no state dir created");

    // Restore permissions so the scratch dir cleans up.
    let _ = fs::set_permissions(&ro, fs::Permissions::from_mode(0o755));
}

/// A present-but-unresolved `STOP` entry (dangling/self-referential symlink)
/// still denies: it is a present entry, not absence (`agalma-5fy.5`).
#[test]
fn present_stop_entry_denies_even_when_unresolved() {
    let scratch = Scratch::new("present-stop");
    let epic = scratch.create_epic("Candidate epic");
    std::os::unix::fs::symlink(scratch.stop_path(), scratch.stop_path())
        .expect("self-referential STOP symlink");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "a present STOP entry denies: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("stop policy active"), "stop reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
    assert!(!scratch.ledger_path().exists(), "no ledger initialized under a stop");
}

/// A `STOP` metadata observation error (not `NotFound`, not a present entry) is
/// `Unavailable`, never false.
#[test]
fn sentinel_metadata_error_is_unavailable() {
    let scratch = Scratch::new("sentinel-meta");
    let epic = scratch.create_epic("Candidate epic");
    // A non-directory intermediate path makes observing <path>/STOP an ENOTDIR
    // error — a genuine metadata IO failure distinct from both a real NotFound
    // and a present entry.
    let blocker = scratch
        .state_dir
        .parent()
        .expect("scratch dir")
        .join("blocker-file");
    fs::write(&blocker, b"not a directory").expect("blocker file");
    let blocked = blocker.join("state");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &blocked, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "sentinel metadata error suppresses: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

/// The selected **process** binding (default Connect) enforces the same policy:
/// fresh explicit initialization, then established open-existing reads the latch.
#[test]
fn process_binding_initializes_fresh_and_opens_existing() {
    let dir = fresh("process-binding");
    let control = bind_stop_control_with(&dir, LedgerTransport::Process);
    assert_eq!(
        control.decision(),
        StopDecision::Running,
        "fresh process binding initializes and reads a clear latch"
    );
    assert!(dir.join("ledger.sqlite").is_file(), "process binding created the database");
    assert!(
        dir.join("ledger.established").is_file(),
        "establishment recorded for the process binding"
    );
    drop(control);

    {
        let mut ledger =
            agalma_ledger::SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger.set_kill_latch(true).expect("set latch");
    }
    let control = bind_stop_control_with(&dir, LedgerTransport::Process);
    assert!(
        matches!(control.decision(), StopDecision::Stopped(_)),
        "established process open reads the durable latch"
    );
}

// ---------------------------------------------------------------------------
// Explicit stop (sentinel / persisted latch) still suppresses, with the stop reason
// ---------------------------------------------------------------------------

#[test]
fn persisted_latch_alone_suppresses_with_stopped_reason() {
    let scratch = Scratch::new("latch");
    let epic = scratch.create_epic("Candidate epic");
    scratch.set_kill_latch(true);
    assert!(!scratch.stop_path().exists(), "STOP absent");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "{report:?}");
    let reason = report.skipped_reason.expect("reason");
    assert!(reason.contains("stop policy active"), "{reason}");
    assert!(designer.called().is_empty());
    assert!(scratch.children(&epic).is_empty());
    assert!(scratch.kill_latch(), "latch retained (never implicitly cleared)");
}

// ---------------------------------------------------------------------------
// STOP arriving during a session: bounded cancel + preserved interrupted state
// ---------------------------------------------------------------------------

#[test]
fn stop_during_session_halts_and_preserves_interrupted_state() {
    let scratch = Scratch::new("mid-session");
    let epic1 = scratch.create_epic("First epic");
    let epic2 = scratch.create_epic("Second epic");
    let designer = ScriptedDesigner::stops_on_first();

    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "{report:?}");
    let called = designer.called();
    assert_eq!(called.len(), 1, "exactly one session ran: {called:?}");
    let interrupted = called[0].clone();
    let other = if interrupted == epic1 { epic2.clone() } else { epic1.clone() };
    assert_eq!(report.epics_seen, vec![interrupted.clone()], "no next epic");
    assert_eq!(report.epics_swept, vec![interrupted.clone()], "{report:?}");

    let issue = scratch.show(&interrupted);
    assert!(has_label(&issue, "decomp-inflight"), "in-flight marker retained: {issue}");
    assert!(has_label(&issue, "decomp-attempts:1"), "attempt counter retained: {issue}");
    let children = scratch.children(&interrupted);
    assert_eq!(children.len(), 1, "the partial child is preserved");
    assert!(has_label(&children[0], "staged"), "child is staged");
    assert!(
        !has_label(&children[0], "decomp-approved") && !has_label(&children[0], "decomp-rejected"),
        "no gate verdict projected: {}",
        children[0]
    );
    assert!(scratch.children(&other).is_empty(), "unlaunched epic untouched");

    let restart = ScriptedDesigner::inert();
    let again = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &restart).expect("sweep");
    assert!(again.stopped, "restart retains stop state: {again:?}");
    assert!(restart.called().is_empty(), "no dispatch after restart while stopped");
}

// ---------------------------------------------------------------------------
// Explicit human resume is the only clear, and permits dispatch again
// ---------------------------------------------------------------------------

#[test]
fn explicit_human_resume_permits_dispatch_again() {
    let scratch = Scratch::new("resume");
    let epic = scratch.create_epic("Candidate epic");
    scratch.set_kill_latch(true);
    fs::write(scratch.stop_path(), b"stop").expect("write STOP");

    let before = ScriptedDesigner::inert();
    let blocked = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &before).expect("sweep");
    assert!(blocked.stopped, "{blocked:?}");
    assert!(before.called().is_empty());
    assert!(scratch.stop_path().exists(), "STOP still present");
    assert!(scratch.kill_latch(), "latch still set");

    let resume = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args(["resume", "--state-dir", scratch.state_dir.to_str().unwrap()])
        .output()
        .expect("run agalma resume");
    assert!(resume.status.success(), "resume: {}", stderr_of(&resume));
    assert!(!scratch.stop_path().exists(), "resume clears STOP");
    assert!(!scratch.kill_latch(), "resume clears the latch");

    let after = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &after).expect("sweep");
    assert!(!report.stopped, "dispatch permitted after resume: {report:?}");
    assert!(!after.called().is_empty(), "the next sweep dispatched: {after:?}");
    assert!(report.epics_seen.contains(&epic), "{report:?}");
}

// ---------------------------------------------------------------------------
// The shared session seam: a denied decision refuses before any scratch
// ---------------------------------------------------------------------------

#[test]
fn session_seam_refuses_unavailable_without_creating_scratch() {
    let dir = fresh("unavailable-session");
    let stop: Rc<dyn StopControl> = Rc::new(ScriptedStop(StopDecision::Unavailable(
        "stop state unavailable".to_string(),
    )));
    let live = LiveSession::build(&dir, None, false, stop).expect("build live session");

    let err = run_session(&live, &session_spec()).expect_err("unavailable must refuse");
    assert!(err.to_string().contains("stop state unavailable"), "{err}");
    assert!(
        !dir.join("sweep").exists(),
        "no session scratch created while the stop state is unavailable"
    );
}

#[test]
fn session_seam_refuses_explicit_stop_without_creating_scratch() {
    let dir = fresh("stopped-session");
    let stop: Rc<dyn StopControl> = Rc::new(ScriptedStop(StopDecision::Stopped(
        "stop policy active (STOP sentinel or kill latch set)".to_string(),
    )));
    let live = LiveSession::build(&dir, None, false, stop).expect("build live session");

    let err = run_session(&live, &session_spec()).expect_err("stopped must refuse");
    assert!(err.to_string().contains("stop policy active"), "{err}");
    assert!(!dir.join("sweep").exists(), "no session scratch created while stopped");
}

/// The session seam uses the **same** bound control: a failing service read
/// refuses even though a readable database file is present.
#[test]
fn session_seam_refuses_failing_bound_service_read() {
    let dir = fresh("failing-bound-session");
    drop(agalma_ledger::SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger"));
    let stop: Rc<dyn StopControl> =
        Rc::new(BoundStopControl::new(&dir, FailingLedger::new()));
    let live = LiveSession::build(&dir, None, false, stop).expect("build live session");

    let err = run_session(&live, &session_spec()).expect_err("failing service read must refuse");
    assert!(err.to_string().contains("stop state unavailable"), "{err}");
    assert!(!dir.join("sweep").exists(), "no session scratch created");
}

// ---------------------------------------------------------------------------
// Review: a successful verdict after the stop observation flips is not projected
// ---------------------------------------------------------------------------

#[test]
fn review_success_is_not_projected_when_stop_arrives_mid_session() {
    let scratch = Scratch::new("review-late-stop");
    let epic = landed_epic(&scratch, "Reviewable epic");

    let control = LateStop::running();
    let reviewer = StoppingReviewer {
        control: control.clone(),
        decision: StopDecision::Stopped(
            "stop policy active (STOP sentinel or kill latch set)".to_string(),
        ),
    };
    let report = review_epics_with_stop(
        &scratch.repo,
        &scratch.state_dir,
        &gate(),
        &reviewer,
        &control,
    )
    .expect("review");

    assert!(report.stopped, "mid-session stop halts the gate: {report:?}");
    assert!(report.approved.is_empty(), "no approval projected: {report:?}");
    assert!(report.drift.is_empty(), "no drift projected: {report:?}");

    // No verdict was projected: neither label landed and no drift-fix task was
    // created. The attempt label recorded before the session is preserved.
    let labels = Scratch::labels(&scratch.show(&epic));
    assert!(!labels.iter().any(|l| l == "review-approved"), "no approval label: {labels:?}");
    assert!(!labels.iter().any(|l| l == "review-drift"), "no drift label: {labels:?}");
    assert!(labels.iter().any(|l| l == "review-attempts:1"), "attempt evidence preserved: {labels:?}");
    let children = scratch.children(&epic);
    assert!(
        !children.iter().any(|c| Scratch::labels(c).iter().any(|l| l == "drift-fix")),
        "no drift-fix task created: {children:?}"
    );
}

#[test]
fn review_success_is_not_projected_when_ledger_becomes_unavailable_mid_session() {
    let scratch = Scratch::new("review-late-unavailable");
    let epic = landed_epic(&scratch, "Reviewable epic");

    let control = LateStop::running();
    let reviewer = StoppingReviewer {
        control: control.clone(),
        decision: StopDecision::Unavailable("stop state unavailable".to_string()),
    };
    let report = review_epics_with_stop(
        &scratch.repo,
        &scratch.state_dir,
        &gate(),
        &reviewer,
        &control,
    )
    .expect("review");

    assert!(report.stopped, "unavailability halts the gate: {report:?}");
    assert!(report.approved.is_empty(), "no approval projected: {report:?}");
    let labels = Scratch::labels(&scratch.show(&epic));
    assert!(!labels.iter().any(|l| l == "review-approved"), "no approval label: {labels:?}");
    assert!(labels.iter().any(|l| l == "review-attempts:1"), "attempt evidence preserved: {labels:?}");
}

// ---------------------------------------------------------------------------
// Production work/run refusal: a present STOP never initializes a database
// ---------------------------------------------------------------------------

#[test]
fn work_binary_stop_on_fresh_state_leaves_no_ledger_or_marker() {
    let scratch = Scratch::new("work-stop-fresh");
    let epic = scratch.create_epic("Candidate epic");
    fs::write(scratch.stop_path(), b"stop").expect("write STOP");

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "work",
            "--repo",
            scratch.repo.to_str().unwrap(),
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--once",
            "--model",
            "test/offline",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .output()
        .expect("run agalma work");
    assert!(!out.status.success(), "a stop must suppress the work loop");
    let stderr = stderr_of(&out);
    assert!(stderr.contains("stop admission refused"), "clear reason: {stderr}");

    // No ledger, no establishment marker, no backlog mutation, no session scratch.
    assert!(!scratch.ledger_path().exists(), "no ledger created under STOP");
    assert!(
        !scratch.state_dir.join("ledger.established").exists(),
        "no establishment marker under STOP"
    );
    assert!(scratch.children(&epic).is_empty(), "no children created");
    assert!(!scratch.state_dir.join("sweep").exists(), "no designer scratch");
    assert!(scratch.stop_path().exists(), "STOP retained");
}

#[test]
fn work_binary_dangling_stop_symlink_denies_without_ledger() {
    let scratch = Scratch::new("work-dangling-stop");
    let epic = scratch.create_epic("Candidate epic");
    // A present-but-unresolved STOP entry must still deny (never treated absent).
    std::os::unix::fs::symlink(scratch.stop_path(), scratch.stop_path())
        .expect("self-referential STOP symlink");

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "work",
            "--repo",
            scratch.repo.to_str().unwrap(),
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--once",
            "--model",
            "test/offline",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .output()
        .expect("run agalma work");
    assert!(!out.status.success(), "a dangling STOP must deny");
    assert!(!scratch.ledger_path().exists(), "no ledger created");
    assert!(
        !scratch.state_dir.join("ledger.established").exists(),
        "no establishment marker created"
    );
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

#[test]
fn run_binary_stop_on_fresh_state_leaves_no_ledger_or_marker() {
    let scratch = Scratch::new("run-stop-fresh");
    fs::write(scratch.stop_path(), b"stop").expect("write STOP");
    let fixture = scratch.repo.join("nonexistent-fixture");

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "run",
            "--fixture",
            fixture.to_str().unwrap(),
            "--once",
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--model",
            "test/offline",
        ])
        .output()
        .expect("run agalma run");
    assert!(!out.status.success(), "a stop must suppress the run path");
    let stderr = stderr_of(&out);
    assert!(stderr.contains("stop admission refused"), "clear reason: {stderr}");
    assert!(!scratch.ledger_path().exists(), "no ledger created under STOP");
    assert!(
        !scratch.state_dir.join("ledger.established").exists(),
        "no establishment marker under STOP"
    );
}

// ---------------------------------------------------------------------------
// Production work/run refusal of lost/corrupt/incompatible established state
// ---------------------------------------------------------------------------

#[test]
fn work_binary_lost_established_ledger_is_refused() {
    let scratch = Scratch::new("work-lost-established");
    let epic = scratch.create_epic("Candidate epic");
    // Explicitly initialize (ledger + marker), latch, then lose the database.
    let control = bind_stop_control(&scratch.state_dir);
    assert_eq!(control.decision(), StopDecision::Running);
    drop(control);
    scratch.set_kill_latch(true);
    fs::remove_file(scratch.ledger_path()).expect("lose the database");
    assert!(scratch.state_dir.join("ledger.established").is_file(), "marker durable");

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "work",
            "--repo",
            scratch.repo.to_str().unwrap(),
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--once",
            "--model",
            "test/offline",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .output()
        .expect("run agalma work");
    assert!(!out.status.success(), "lost established state must refuse the work loop");
    let stderr = stderr_of(&out);
    assert!(stderr.contains("refusing to recreate"), "clear reason: {stderr}");
    assert!(!scratch.ledger_path().exists(), "no replacement ledger created");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

#[test]
fn work_binary_corrupt_established_ledger_is_refused() {
    let scratch = Scratch::new("work-corrupt-established");
    let epic = scratch.create_epic("Candidate epic");
    drop(bind_stop_control(&scratch.state_dir));
    fs::write(scratch.ledger_path(), b"not a sqlite database").expect("corrupt ledger");

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "work",
            "--repo",
            scratch.repo.to_str().unwrap(),
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--once",
            "--model",
            "test/offline",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .output()
        .expect("run agalma work");
    assert!(!out.status.success(), "corrupt established ledger must refuse the work loop");
    assert_eq!(
        fs::read(scratch.ledger_path()).expect("read ledger"),
        b"not a sqlite database",
        "the corrupt ledger is not replaced"
    );
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
}

#[test]
fn run_binary_incompatible_ledger_is_refused() {
    let scratch = Scratch::new("run-incompatible");
    drop(bind_stop_control(&scratch.state_dir));
    scratch.force_schema_version(999);
    let fixture = scratch.repo.join("nonexistent-fixture");

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "run",
            "--fixture",
            fixture.to_str().unwrap(),
            "--once",
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--model",
            "test/offline",
        ])
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .output()
        .expect("run agalma run");
    assert!(!out.status.success(), "incompatible established ledger must refuse the run path");
    let stderr = stderr_of(&out);
    // The refusal must be the selected-binding schema refusal, not a fixture-load
    // failure (the fixture is deliberately absent and loads only after admission).
    assert!(
        stderr.contains("incompatible ledger schema"),
        "the schema refusal must be visible: {stderr}"
    );
    // The recorded schema is unchanged and no execution artifact was produced.
    let conn = rusqlite::Connection::open(scratch.ledger_path()).expect("reopen ledger");
    let recorded: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema row");
    assert_eq!(recorded, "999", "open-existing must not migrate/rewrite");
    assert!(
        !scratch.state_dir.join("runs").exists(),
        "no execution/proxy artifacts under refusal"
    );
}

// ---------------------------------------------------------------------------
// Production latched refusal without proxy/profile/prompts artifacts
// ---------------------------------------------------------------------------

#[test]
fn run_binary_latched_ledger_refuses_without_proxy_or_prompts() {
    let scratch = Scratch::new("run-latched");
    drop(bind_stop_control(&scratch.state_dir));
    scratch.set_kill_latch(true);
    let fixture = scratch.repo.join("nonexistent-fixture");

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "run",
            "--fixture",
            fixture.to_str().unwrap(),
            "--once",
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--model",
            "test/offline",
        ])
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .output()
        .expect("run agalma run");
    assert!(!out.status.success(), "a latched run must refuse");
    let stderr = stderr_of(&out);
    assert!(stderr.contains("stop policy active"), "clear reason: {stderr}");
    // No normal composition artifacts: no proxy/profile/prompts/execution.
    assert!(!scratch.state_dir.join("worker.sb").exists(), "no sandbox profile");
    assert!(!scratch.state_dir.join("prompts").exists(), "no prompts materialized");
    assert!(!scratch.state_dir.join("runs").exists(), "no execution artifacts");
}

#[test]
fn work_binary_latched_ledger_refuses_without_proxy_or_prompts() {
    let scratch = Scratch::new("work-latched");
    let epic = scratch.create_epic("Candidate epic");
    drop(bind_stop_control(&scratch.state_dir));
    scratch.set_kill_latch(true);

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "work",
            "--repo",
            scratch.repo.to_str().unwrap(),
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--once",
            "--model",
            "test/offline",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .output()
        .expect("run agalma work");
    assert!(!out.status.success(), "a latched work loop must refuse");
    let stderr = stderr_of(&out);
    assert!(stderr.contains("stop policy active"), "clear reason: {stderr}");
    assert!(!scratch.state_dir.join("worker.sb").exists(), "no sandbox profile");
    assert!(!scratch.state_dir.join("prompts").exists(), "no prompts materialized");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
    assert!(!scratch.state_dir.join("sweep").exists(), "no designer scratch");
    assert!(scratch.kill_latch(), "latch retained (never implicitly cleared)");
}

// ---------------------------------------------------------------------------
// Bounded real-worker cleanup during a latched work boot
// ---------------------------------------------------------------------------

/// A latched `work` boot still reconciles a recorded live survivor through the
/// shared bounded cleanup (stop evidence written, worker record cleared), then
/// refuses without new effects (`agalma-5fy.5`).
#[test]
fn work_binary_latched_boot_reconciles_recorded_survivor() {
    use std::os::unix::process::CommandExt;

    let scratch = Scratch::new("work-latched-survivor");
    let epic = scratch.create_epic("Candidate epic");
    drop(bind_stop_control(&scratch.state_dir));
    scratch.set_kill_latch(true);

    // A real long-lived worker in its own process group, recorded for a build.
    let mut child = Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .expect("spawn survivor");
    let pid = child.id();
    let exec = ExecutionId::new("exec:sf-stop-survivor:1");
    agalma_conductor::workers::record_worker(
        &scratch.state_dir,
        &exec,
        "build",
        1,
        "op:sf-stop-survivor",
        1,
        pid,
        pid,
    )
    .expect("record worker");

    // The authoritative bound pending intent for that build operation.
    {
        let mut ledger =
            agalma_ledger::SqliteLedger::open(scratch.ledger_path()).expect("open ledger");
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: None,
                events: vec![],
                receipts: vec![],
                intents: vec![DispatchIntent {
                    operation_id: OperationId::new("op:sf-stop-survivor"),
                    execution_id: exec.clone(),
                    payload: serde_json::json!({
                        "kind": "build",
                        "attempt": 1,
                        "lease_generation": 1,
                        "inputs": {}
                    }),
                    enqueued_at_unix_ms: 0,
                    consumed: false,
                }],
            })
            .expect("commit intent");
    }

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "work",
            "--repo",
            scratch.repo.to_str().unwrap(),
            "--state-dir",
            scratch.state_dir.to_str().unwrap(),
            "--once",
            "--model",
            "test/offline",
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .output()
        .expect("run agalma work");
    assert!(!out.status.success(), "latched work must refuse");

    // The recorded survivor was reconciled: durable stop evidence exists and the
    // worker record was cleared. No new backlog effect.
    assert!(
        agalma_conductor::workers::stop_path(&scratch.state_dir, &exec, "build", 1).exists(),
        "stop evidence written for the reconciled survivor"
    );
    assert!(
        !agalma_conductor::workers::worker_path(&scratch.state_dir, &exec, "build", 1).exists(),
        "worker record cleared after termination"
    );
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");

    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Genuine absence vs present-but-unresolved establishment entries
// ---------------------------------------------------------------------------

#[test]
fn dangling_established_marker_without_db_is_unavailable() {
    let scratch = Scratch::new("dangling-marker");
    let epic = scratch.create_epic("Candidate epic");
    // A dangling marker symlink with no database must be Ambiguous, never Fresh.
    std::os::unix::fs::symlink(
        scratch.state_dir.join("missing-target"),
        scratch.state_dir.join("ledger.established"),
    )
    .expect("dangling marker symlink");

    let designer = ScriptedDesigner::inert();
    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");
    assert!(report.stopped, "dangling marker suppresses: {report:?}");
    let reason = report.skipped_reason.expect("visible reason");
    assert!(reason.contains("unavailable"), "distinct reason: {reason}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(scratch.children(&epic).is_empty(), "no backlog mutation");
    assert!(!scratch.ledger_path().exists(), "no clear ledger recreated");
}

#[test]
fn dangling_db_entry_is_unavailable_not_fresh() {
    let dir = fresh("dangling-db");
    std::os::unix::fs::symlink(dir.join("missing-target"), dir.join("ledger.sqlite"))
        .expect("dangling db symlink");
    let control = bind_stop_control(&dir);
    assert!(
        matches!(control.decision(), StopDecision::Unavailable(_)),
        "a dangling db entry is not fresh"
    );
    assert!(
        std::fs::symlink_metadata(dir.join("ledger.sqlite"))
            .expect("db entry present")
            .file_type()
            .is_symlink(),
        "the dangling entry is preserved (no replacement created)"
    );
}

#[test]
fn dangling_temp_marker_is_unavailable() {
    let dir = fresh("dangling-tmp");
    std::os::unix::fs::symlink(dir.join("missing-target"), dir.join("ledger.established.tmp"))
        .expect("dangling temp symlink");
    let control = bind_stop_control(&dir);
    assert!(
        matches!(control.decision(), StopDecision::Unavailable(_)),
        "a dangling temp marker is ambiguous"
    );
    assert!(!dir.join("ledger.sqlite").exists(), "no ledger created");
}

// ---------------------------------------------------------------------------
// Bound LedgerApi read: Running / Stopped / Unavailable through the trait
// ---------------------------------------------------------------------------

#[test]
fn bound_stop_control_distinguishes_the_three_states() {
    let dir = fresh("bound");
    let mut handle = LedgerHandle::in_process(&dir).expect("bind ledger");

    let running = BoundStopControl::new(&dir, handle.clone());
    assert_eq!(running.decision(), StopDecision::Running);
    drop(running);

    handle.set_kill_latch(true).expect("set latch");
    let stopped = BoundStopControl::new(&dir, handle.clone());
    assert!(matches!(stopped.decision(), StopDecision::Stopped(_)));
    drop(stopped);
    handle.set_kill_latch(false).expect("clear latch");
    drop(handle);

    {
        let conn = rusqlite::Connection::open(dir.join("ledger.sqlite")).expect("reopen raw");
        conn.execute("UPDATE meta SET value='999' WHERE key='schema_version'", [])
            .expect("force schema version");
    }
    let handle = LedgerHandle::in_process(&dir).expect("bind ledger");
    let unavailable = BoundStopControl::new(&dir, handle);
    match unavailable.decision() {
        StopDecision::Unavailable(reason) => assert!(reason.contains("unavailable"), "{reason}"),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
