//! `agalma-r2l`: stop policy (STOP sentinel + durable kill latch) gates the
//! automatic epic sweep, the ordinary work loop, and the shared session seam.
//!
//! Every test drives the real [`agalma_conductor::sweep::sweep_epics`] (which
//! now applies the production stop policy) or the real `agalma` binary against a
//! scratch `git` + isolated `bd` workspace under `target/test-runs/stop-*`
//! (never `/tmp`). The designer session is a scripted [`DesignerSession`] seam
//! (no harness, no network, no paid call); the gate and all backlog projections
//! are real. Scratch dirs auto-remove on success (kept on failure for forensics).

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_conductor::sweep::{sweep_epics, DesignerRequest, DesignerSession, PlannerGate};
use agalma_contracts::{ContractError, LedgerApi};
use agalma_testkit::ScratchDir;
use serde_json::Value;

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(&format!("stop-{name}"))
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
                "stop",
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

    fn write_stop(&self) {
        fs::write(self.stop_path(), b"stop").expect("write STOP");
    }

    fn set_kill_latch(&self, latched: bool) {
        let mut ledger = agalma_ledger::SqliteLedger::open(self.state_dir.join("ledger.sqlite"))
            .expect("open ledger");
        ledger.set_kill_latch(latched).expect("set latch");
    }

    fn kill_latch(&self) -> bool {
        agalma_ledger::SqliteLedger::open(self.state_dir.join("ledger.sqlite"))
            .expect("open ledger")
            .kill_latch()
            .expect("read latch")
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

/// A scripted designer: records the epics it ran for; in `stops_on_first` mode,
/// the first session creates a single partial staged child and then writes the
/// `STOP` sentinel (simulating a stop that arrives while the session runs).
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
    fn run(&self, request: &DesignerRequest) -> Result<(), ContractError> {
        self.called.borrow_mut().push(request.epic.clone());
        if !self.stop_on_first {
            return Ok(());
        }
        let mut stopped = self.stopped.borrow_mut();
        if *stopped {
            return Ok(());
        }
        *stopped = true;
        // One partial child, then a stop arrives mid-session.
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
        Ok(())
    }
}

fn gate() -> PlannerGate {
    // Paid opt-in + credential present: the planner route is available offline
    // (the static planner route is chosen; no network call is made here).
    PlannerGate::new(true, true, None)
}

// ---------------------------------------------------------------------------
// Acceptance 1 — preexisting STOP → zero designer calls, no ticket mutations
// ---------------------------------------------------------------------------

#[test]
fn preexisting_stop_blocks_sweep_without_mutations() {
    let scratch = Scratch::new("stopped");
    let epic = scratch.create_epic("Candidate epic");
    scratch.write_stop();
    let designer = ScriptedDesigner::inert();

    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "sweep reports stopped: {report:?}");
    assert!(
        designer.called().is_empty(),
        "zero designer calls: {designer:?}"
    );
    assert!(report.epics_seen.is_empty(), "no epic examined: {report:?}");
    assert!(
        scratch.children(&epic).is_empty(),
        "no ticket mutations (no children created)"
    );
    assert!(
        !has_label(&scratch.show(&epic), "decomp-inflight"),
        "epic untouched: no attempt/in-flight labels"
    );
    // Stop state is never implicitly cleared.
    assert!(scratch.stop_path().exists(), "STOP retained");

    // Restart (a second sweep) still sees the stop: state is durable.
    let again = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");
    assert!(again.stopped, "{again:?}");
    assert!(designer.called().is_empty(), "still zero calls");
}

// ---------------------------------------------------------------------------
// Acceptance 2 — persisted kill latch with STOP absent also blocks dispatch
// ---------------------------------------------------------------------------

#[test]
fn persisted_kill_latch_alone_blocks_sweep_without_mutations() {
    let scratch = Scratch::new("latched");
    let epic = scratch.create_epic("Candidate epic");
    scratch.set_kill_latch(true);
    assert!(!scratch.stop_path().exists(), "STOP absent");
    let designer = ScriptedDesigner::inert();

    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    assert!(report.stopped, "kill latch blocks the sweep: {report:?}");
    assert!(designer.called().is_empty(), "zero designer calls");
    assert!(scratch.children(&epic).is_empty(), "no ticket mutations");
    assert!(
        scratch.kill_latch(),
        "latch retained (never implicitly cleared)"
    );

    // Restart still blocked.
    let again = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");
    assert!(again.stopped, "{again:?}");
    assert!(designer.called().is_empty());
}

// ---------------------------------------------------------------------------
// Acceptance 3 — STOP mid-session: no next epic; interrupted state preserved
// ---------------------------------------------------------------------------

#[test]
fn stop_during_session_halts_before_next_epic_and_preserves_state() {
    let scratch = Scratch::new("mid-session");
    let epic1 = scratch.create_epic("First epic");
    let epic2 = scratch.create_epic("Second epic");
    let designer = ScriptedDesigner::stops_on_first();

    let report = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");

    // The stop that arrived mid-session halted the sweep before the next epic.
    assert!(report.stopped, "{report:?}");
    let called = designer.called();
    assert_eq!(called.len(), 1, "exactly one session ran: {called:?}");
    let interrupted = called[0].clone();
    let other = if interrupted == epic1 {
        epic2.clone()
    } else {
        epic1.clone()
    };
    assert_eq!(
        report.epics_seen,
        vec![interrupted.clone()],
        "no next epic examined"
    );
    assert_eq!(report.epics_swept, vec![interrupted.clone()], "{report:?}");

    // The interrupted planning state is preserved for reconciliation: the
    // in-flight marker and attempt counter stay; no gate verdict was projected.
    let interrupted_issue = scratch.show(&interrupted);
    assert!(
        has_label(&interrupted_issue, "decomp-inflight"),
        "in-flight marker retained for reconciliation: {interrupted_issue}"
    );
    assert!(
        has_label(&interrupted_issue, "decomp-attempts:1"),
        "attempt counter retained: {interrupted_issue}"
    );
    let children = scratch.children(&interrupted);
    assert_eq!(children.len(), 1, "the partial child is preserved");
    assert!(has_label(&children[0], "staged"), "child is staged");
    assert!(
        !has_label(&children[0], "decomp-approved") && !has_label(&children[0], "decomp-rejected"),
        "no gate verdict projected on the interrupted child: {}",
        children[0]
    );
    assert!(
        scratch.children(&other).is_empty(),
        "the unlaunched epic is untouched"
    );

    // Restart retains the stop state.
    let restart = ScriptedDesigner::inert();
    let again = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &restart).expect("sweep");
    assert!(again.stopped, "restart retains stop state: {again:?}");
    assert!(
        restart.called().is_empty(),
        "no dispatch after restart while stopped"
    );
}

// ---------------------------------------------------------------------------
// Acceptance 4 — only the human resume flow clears; it permits dispatch again
// ---------------------------------------------------------------------------

#[test]
fn human_resume_is_the_only_clear_and_permits_dispatch() {
    let scratch = Scratch::new("resume");
    let epic1 = scratch.create_epic("First epic");
    scratch.create_epic("Second epic");
    scratch.set_kill_latch(true);
    scratch.write_stop();

    let designer = ScriptedDesigner::inert();
    let blocked =
        sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");
    assert!(blocked.stopped, "{blocked:?}");
    assert!(designer.called().is_empty());
    // Nothing cleared the stop implicitly.
    assert!(scratch.stop_path().exists(), "STOP still present");
    assert!(scratch.kill_latch(), "latched still set");

    // The established human action clears both.
    let resume = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args(["resume", "--state-dir", scratch.state_dir.to_str().unwrap()])
        .output()
        .expect("run agalma resume");
    assert!(resume.status.success(), "resume: {}", stderr_of(&resume));
    assert!(!scratch.stop_path().exists(), "resume clears STOP");
    assert!(!scratch.kill_latch(), "resume clears the latch");

    // Dispatch is permitted again after the human resume.
    let designer2 = ScriptedDesigner::inert();
    let after = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer2).expect("sweep");
    assert!(!after.stopped, "dispatch permitted after resume: {after:?}");
    assert!(
        !designer2.called().is_empty(),
        "the next sweep dispatched the designer: {designer2:?}"
    );
    assert!(
        after.epics_seen.contains(&epic1),
        "candidate epics are swept again: {after:?}"
    );
}

/// Full recovery loop: a stop mid-session, then human resume, then the next
/// sweep reconciles the interrupted decomposition instead of starting a second
/// designer session for the same epic.
#[test]
fn interrupted_decomposition_is_reconciled_after_resume() {
    let scratch = Scratch::new("reconcile");
    let epic1 = scratch.create_epic("Interrupted epic");
    let epic2 = scratch.create_epic("Later epic");
    let designer = ScriptedDesigner::stops_on_first();

    let first = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &designer).expect("sweep");
    assert!(first.stopped, "{first:?}");
    let called = designer.called();
    assert_eq!(called.len(), 1, "one session ran: {called:?}");
    let interrupted = called[0].clone();
    let later = if interrupted == epic1 {
        epic2.clone()
    } else {
        epic1.clone()
    };

    // Human resume clears the stop.
    let resume = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args(["resume", "--state-dir", scratch.state_dir.to_str().unwrap()])
        .output()
        .expect("run agalma resume");
    assert!(resume.status.success(), "resume: {}", stderr_of(&resume));

    // Next sweep recovers the interrupted epic (no new session for it) and
    // dispatches the later epic.
    let restart = ScriptedDesigner::inert();
    let second = sweep_epics(&scratch.repo, &scratch.state_dir, &gate(), &restart).expect("sweep");
    assert!(!second.stopped, "{second:?}");
    assert!(
        second.recovered.contains(&interrupted),
        "interrupted decomposition recovered: {second:?}"
    );
    assert!(
        restart.called().contains(&later),
        "later epic dispatched after resume: {:?}",
        restart.called()
    );
    assert_eq!(
        restart
            .called()
            .iter()
            .filter(|e| **e == interrupted)
            .count(),
        0,
        "no second session for the recovered epic"
    );
    assert!(
        !has_label(&scratch.show(&interrupted), "decomp-inflight"),
        "in-flight marker cleared once reconciled"
    );
}

// ---------------------------------------------------------------------------
// Acceptance 5 — the ordinary work loop's boot path is gated the same way
// ---------------------------------------------------------------------------

#[test]
fn work_boot_stop_blocks_sweep_and_backlog_mutation() {
    let scratch = Scratch::new("work-boot");
    let epic = scratch.create_epic("Candidate epic");
    scratch.write_stop();

    // `--model` unlocks the planner gate (override) so the only thing that can
    // prevent a designer session is the stop policy.
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
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("stop policy active"),
        "boot reports the stop: {stderr}"
    );

    // No planning dispatch and no backlog mutation: no children, and the sweep
    // never even materialized a session scratch dir.
    assert!(scratch.children(&epic).is_empty(), "no children created");
    assert!(
        !scratch.state_dir.join("sweep").exists(),
        "no designer session scratch (session never built)"
    );
    assert!(scratch.stop_path().exists(), "STOP retained after the run");
}
