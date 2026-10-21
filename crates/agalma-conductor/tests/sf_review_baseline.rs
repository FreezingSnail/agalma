//! `agalma-5fy.18`: the immutable epic review baseline must precede its first
//! integrated child change and survive retry / duplicate / restart.
//!
//! The production defect (found by the SF.10 fresh-goal experiment,
//! `target/test-runs/agalma-5fy.10/20261016-sf10-baseline-defect-01/`): the work
//! loop recorded the epic diff baseline (`review-start:<HEAD>`) *after*
//! `Terminal::Done`, sampling the post-integration HEAD. The admitted review then
//! had `base_sha == candidate_sha` and an empty `diff.patch`, so the first
//! integrated `src/lib.rs` feature change was never independently reviewed.
//!
//! This target drives the **real** production control paths over a fresh bounded
//! scratch source-feature repo (isolated `git` origin + isolated `bd` store):
//!
//! 1. a real `agalma work --once` (scripted activity mode, scripted builder) that
//!    freezes an immutable `TaskPackage/v1`, independently verifies the candidate
//!    against a protected behavioral oracle, and integrates it;
//! 2. the real [`agalma_conductor::review::review_epics`] gate with a scripted
//!    reviewer seam over the merged diff.
//!
//! No network and no paid/live provider inference: the paid planner/reviewer gate
//! stays closed (only a builder-scoped free selector is passed), so the boot
//! sweep/review cannot slip a live session through.
//!
//! Required cases: the first `src/lib.rs` `0 -> 42` change lands inside the
//! baseline and appears in the reviewer diff; a multi-child epic's diff still
//! covers the first change; duplicate/restart never re-integrates or re-reviews
//! and never moves the pin; an unknown/conflicting/missing baseline refuses
//! closure instead of approving an empty/truncated diff; a crash before
//! integration records no baseline and recovery pins the pre-integration base; a
//! stop before claim records no baseline. A deterministic generated
//! operation-sequence property independently checks the baseline-validity
//! decision (fixed seed, bounded case budget).

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;

use agalma_conductor::review::{
    review_epics, ReviewRequest, ReviewerSession, REVIEW_APPROVED_LABEL, REVIEW_DRIFT_LABEL,
    REVIEW_START_PREFIX,
};
use agalma_conductor::scope_guard::PROTECTED_BASELINE_FORMAT;
use agalma_conductor::sweep::PlannerGate;
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::{
    CommitBatch, ContractError, ExecutionEvent, ExecutionId, ExecutionPhase, ExecutionRecord,
    ExecutionState, LedgerApi, TaskId,
};
use agalma_testkit::ScratchDir;
use serde_json::{json, Value};

mod common;

/// The reviewed free builder route used for the builder-only selector (so the
/// builder pin is distinct from the independently resolved verifier route).
const FREE_BUILDER: &str = "opencode/mimo-v2.6-flash-free";
/// The frozen task acceptance the scratch feature must satisfy.
const ACCEPTANCE: &str = "sh -c 'grep -q 42 src/lib.rs'";
/// The scratch origin seed (`answer() == 0`); the scripted build replaces it
/// with `answer() == 42`.
const SEED_SOURCE: &str = "//! Seed source.\npub fn answer() -> u32 { 0 }\n";
/// Repository instruction/shared-design bytes the production admission freezes.
const AGENTS_BYTES: &str = "# Agents\nRepository working rules (frozen at admission).\n";
const SHARED_DESIGN_BYTES: &str =
    "# Self-mutation contracts revision 1\nFrozen shared-design bytes.\n";
/// The epic design the review admission pins.
const EPIC_DESIGN: &str =
    "# Design\n\nDeliver the bounded scratch source feature end to end; the epic is reviewed \
     against this immutable design.\n";
/// The child design (low tier, under the word cap).
const CHILD_DESIGN: &str = "Small, single-file change; no new interface.";

/// The immutable protected *behavioral* judge: it independently compiles the
/// candidate library and asserts `answer() == 42` by execution, so a comment
/// containing the expected text cannot spoof it. Reuses the existing `rustc`
/// (no new dependency, no network).
const PROTECTED_SCRIPT: &str = "#!/bin/sh\n\
set -eu\n\
cand=\"$AGALMA_CANDIDATE_DIR\"\n\
work=\"$TMPDIR/sf18-protected-probe\"\n\
rm -rf \"$work\"\n\
mkdir -p \"$work\"\n\
rustc --edition 2021 --crate-type lib --crate-name cand \"$cand/src/lib.rs\" --out-dir \"$work\"\n\
printf '%s\\n' 'fn main() {' '    if cand::answer() != 42 {' '        eprintln!(\"protected behavior: answer() != 42\");' '        std::process::exit(1);' '    }' '}' > \"$work/main.rs\"\n\
rustc --edition 2021 \"$work/main.rs\" --extern cand=\"$work/libcand.rlib\" -L \"$work\" -o \"$work/probe\"\n\
\"$work/probe\"\n";
const PROTECTED_COMMAND: &str = "sh $AGALMA_PROTECTED_DIR/runner.sh";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

/// Select the in-process ledger binding once, so no `agalma-ledger-server`
/// process is spawned from the test binary (the established `sf_stop` pattern).
fn ensure_in_process_ledger() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| std::env::set_var("AGALMA_LEDGER_TRANSPORT", "in-process"));
}

fn scratch(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(&format!("sf-review-baseline-{name}"));
    dir.keep();
    dir
}

/// Isolated XDG config base for a scratch fixture.
fn config_home(state_dir: &Path) -> PathBuf {
    state_dir.parent().expect("state parent").join("xdg-config")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
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

/// The operator `ProtectedBaseline/v1` JSON frozen as `scope.oracle`.
fn protected_baseline_json() -> Value {
    json!({
        "format": PROTECTED_BASELINE_FORMAT,
        "files": [{
            "path": "runner.sh",
            "mode": "100755",
            "digest": sha256_digest(PROTECTED_SCRIPT.as_bytes()),
            "bytes": PROTECTED_SCRIPT.len() as u64,
            "text": PROTECTED_SCRIPT,
        }],
        "commands": [PROTECTED_COMMAND],
    })
}

/// A low-tier child description that satisfies every CP.1 rule and carries the
/// frozen scratch acceptance in its directive machine block.
fn child_description(index: usize) -> String {
    format!(
        "# Scope\nMake the bounded library surface answer for child {index}.\n\n\
         # Files\nsrc/lib.rs\n\n\
         # Contract\nProvide `pub fn answer() -> u32` returning 42.\n\n\
         # Invariants\nThe crate stays a valid library; only src/ changes.\n\n\
         # Non-goals\nNo refactor and no dependency.\n\n\
         # MACHINE\nprovides:\nconsumes:\nfiles: src/lib.rs\ntier: low\n\n\
         ```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - {ACCEPTANCE}\n\
         budget_usd: 0.00\npriority: normal\nfamily: feature\nmax_attempts: 2\n```\n"
    )
}

fn route_available() -> PlannerGate {
    PlannerGate::new(true, true, None)
}

// ---------------------------------------------------------------------------
// Scratch production harness: a scratch git origin + isolated bd store + state
// ---------------------------------------------------------------------------

struct Prod {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Prod {
    fn new(name: &str) -> Prod {
        ensure_in_process_ledger();
        require_bd();
        let dir = scratch(name);
        let origin = dir.child("origin");
        fs::create_dir_all(origin.join("src")).expect("origin src");
        fs::create_dir_all(origin.join("docs")).expect("origin docs");
        fs::write(origin.join("src").join("lib.rs"), SEED_SOURCE).expect("seed source");
        fs::write(origin.join("AGENTS.md"), AGENTS_BYTES).expect("agents");
        fs::write(
            origin.join("docs").join("self-mutation-contracts.md"),
            SHARED_DESIGN_BYTES,
        )
        .expect("shared design");

        run_ok("git", &origin, &["init", "-q", "-b", "main"]);
        run_ok(
            "git",
            &origin,
            &["config", "user.email", "agalma@localhost"],
        );
        run_ok("git", &origin, &["config", "user.name", "Agalma"]);
        run_ok("git", &origin, &["add", "-A"]);
        run_ok("git", &origin, &["commit", "-qm", "seed"]);

        let prefix = format!("sf18{}", name.replace(['-', '.'], ""));
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

        Prod {
            origin,
            state_dir: dir.child("state"),
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

    fn bd_ok(&self, args: &[&str]) -> String {
        let out = self.bd(args);
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.origin)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "git {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(&self, message: &str) -> String {
        run_ok(
            "git",
            &self.origin,
            &["commit", "--allow-empty", "-q", "-m", message],
        );
        self.head()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    fn commit_count(&self) -> usize {
        self.git(&["rev-list", "--count", "HEAD"])
            .parse()
            .expect("commit count")
    }

    fn show(&self, id: &str) -> Value {
        let stdout = self.bd_ok(&["show", id, "--json", "--include-comments"]);
        let value: Value = serde_json::from_str(&stdout).expect("show json");
        value
            .as_array()
            .and_then(|rows| rows.first().cloned())
            .unwrap_or(value)
    }

    fn status(&self, id: &str) -> String {
        self.show(id)["status"]
            .as_str()
            .unwrap_or("unknown")
            .to_string()
    }

    fn labels(&self, id: &str) -> Vec<String> {
        self.show(id)["labels"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn review_start(&self, epic: &str) -> Option<String> {
        self.labels(epic)
            .iter()
            .find_map(|l| l.strip_prefix(REVIEW_START_PREFIX).map(str::to_string))
    }

    /// Create a plain (un-gated) child of `epic` (recovery-identity fixtures).
    fn create_plain_child(&self, epic: &str, title: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--parent",
            epic,
            "--description",
            "# Scope\nplain child for recovery identity\n",
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        value["id"].as_str().expect("child id").to_string()
    }

    /// Create an epic carrying the immutable fixture design.
    fn create_epic(&self, title: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "epic",
            "-p",
            "2",
            "--description",
            EPIC_DESIGN,
            "--json",
        ]);
        assert!(out.status.success(), "bd create epic: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("epic json");
        value["id"].as_str().expect("epic id").to_string()
    }

    /// Create a low-tier staged child, mark it decomposition-approved, and pin it
    /// with exactly one native human execution-approval gate (the designer's
    /// contract), returning `(child_id, gate_id)`.
    fn create_gated_child(&self, epic: &str, index: usize) -> (String, String) {
        let out = self.bd(&[
            "create",
            "child",
            "-t",
            "task",
            "-p",
            "2",
            "--parent",
            epic,
            "--description",
            &child_description(index),
            "--design",
            CHILD_DESIGN,
            "--labels",
            "staged,difficulty:low,decomp-approved",
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        let child = value["id"].as_str().expect("child id").to_string();
        let out = self.bd(&[
            "gate",
            "create",
            "--type",
            "human",
            "--blocks",
            &child,
            "--reason",
            "execution approval",
            "--json",
        ]);
        assert!(out.status.success(), "bd gate create: {}", stderr_of(&out));
        let gate: Value = serde_json::from_slice(&out.stdout).expect("gate json");
        let gate_id = gate["id"].as_str().expect("gate id").to_string();
        self.bd_ok(&[
            "update",
            &gate_id,
            "--add-label",
            "human,execution-approval",
            "--set-metadata",
            &format!("approval_for={child}"),
        ]);
        (child, gate_id)
    }

    fn resolve_gate(&self, gate: &str) {
        self.bd_ok(&["gate", "resolve", gate]);
        assert_eq!(self.status(gate), "closed", "gate {gate} resolved");
    }

    /// Create a staged child and close it (used by the review-only cases, which
    /// need an Eligible admission: one staged child, all children closed).
    fn create_closed_staged_child(&self, epic: &str, index: usize) -> String {
        let out = self.bd(&[
            "create",
            "child",
            "-t",
            "task",
            "-p",
            "2",
            "--parent",
            epic,
            "--description",
            &child_description(index),
            "--design",
            CHILD_DESIGN,
            "--labels",
            "staged,difficulty:low",
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        let child = value["id"].as_str().expect("child id").to_string();
        self.bd_ok(&["close", &child, "--reason", "landed"]);
        child
    }

    /// Write operator `ProtectedInputs/v1` entries for every `task_id` pinned to
    /// the current origin HEAD as `base_sha`.
    fn protect_all(&self, task_ids: &[String], allowed: &[&str], protected: &[&str]) {
        let root = fs::canonicalize(&self.origin).expect("canonical origin");
        let base = self.head();
        let entries: Vec<Value> = task_ids
            .iter()
            .map(|task_id| {
                json!({
                    "task_id": task_id,
                    "repository_root": root.to_string_lossy(),
                    "base_sha": base,
                    "allowed_paths": allowed,
                    "protected_paths": protected,
                    "baseline": protected_baseline_json(),
                })
            })
            .collect();
        let dir = config_home(&self.state_dir).join("agalma");
        fs::create_dir_all(&dir).expect("config dir");
        let doc = json!({ "format": "ProtectedInputs/v1", "entries": entries });
        fs::write(
            dir.join("protected-inputs.json"),
            serde_json::to_vec_pretty(&doc).expect("json"),
        )
        .expect("write manifest");
    }

    /// Run `agalma work --once` against the scratch origin in scripted mode.
    fn work_once(&self) -> Output {
        self.work_once_with(&[])
    }

    fn work_once_with(&self, extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(bin());
        cmd.args([
            "work",
            "--repo",
            self.origin.to_str().expect("origin utf8"),
            "--state-dir",
            self.state_dir.to_str().expect("state utf8"),
            "--once",
            "--builder-model",
            FREE_BUILDER,
        ])
        .env("AGALMA_ACTIVITY_MODE", "scripted")
        .env("AGALMA_LEDGER_TRANSPORT", "in-process")
        .env("AGALMA_WORKSPACE_TRANSPORT", "in-process")
        .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
        .env("XDG_CONFIG_HOME", config_home(&self.state_dir))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("AGALMA_ALLOW_PAID")
        .env_remove("AGALMA_MODEL")
        .env_remove("AGALMA_BUILDER_MODEL");
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.output().expect("run agalma work")
    }

    /// Latch or clear the durable kill latch read by the shared stop control.
    fn set_kill_latch(&self, latched: bool) {
        let path = self.state_dir.join("ledger.sqlite");
        let mut ledger = agalma_ledger::SqliteLedger::open(path).expect("open ledger");
        ledger.set_kill_latch(latched).expect("set kill latch");
    }

    fn assert_feature(&self, label: &str, rev: &str, present: bool) {
        let src = self.git(&["show", &format!("{rev}:src/lib.rs")]);
        assert_eq!(
            src.contains("42"),
            present,
            "{label} ({rev}) feature presence mismatch: {src:?}"
        );
    }

    /// Remove the retained frozen package document(s) under the state dir (leave
    /// the referenced objects), simulating a missing/corrupt durable recovery
    /// input at boot. Returns the number of documents removed.
    fn corrupt_frozen_packages(&self) -> usize {
        let dir = self.state_dir.join("packages").join("sha256");
        let mut removed = 0;
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
                    fs::remove_file(entry.path()).expect("remove frozen package doc");
                    removed += 1;
                }
            }
        }
        removed
    }
}

// ---------------------------------------------------------------------------
// Scripted reviewer seam (no harness, no network)
// ---------------------------------------------------------------------------

struct ScriptedReviewer {
    output: String,
    calls: Rc<Cell<usize>>,
    captured: Rc<RefCell<Option<PathBuf>>>,
}

impl ScriptedReviewer {
    fn new(output: &str) -> Self {
        ScriptedReviewer {
            output: output.to_string(),
            calls: Rc::new(Cell::new(0)),
            captured: Rc::new(RefCell::new(None)),
        }
    }

    fn calls(&self) -> usize {
        self.calls.get()
    }

    fn attempt_dir(&self) -> Option<PathBuf> {
        self.captured
            .borrow()
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
    }
}

impl ReviewerSession for ScriptedReviewer {
    fn run(&self, request: &ReviewRequest) -> Result<String, ContractError> {
        self.calls.set(self.calls.get() + 1);
        *self.captured.borrow_mut() = Some(request.diff_path.clone());
        Ok(self.output.clone())
    }
}

fn read_dir_json(dir: &Path, name: &str) -> Value {
    let text = fs::read_to_string(dir.join(name))
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.join(name).display()));
    serde_json::from_str(&text).expect("parse json artifact")
}

/// The `package_digest` of the single seeded frozen package document.
fn seeded_package_digest(state_dir: &Path) -> String {
    let dir = state_dir.join("packages").join("sha256");
    let mut digests = Vec::new();
    for entry in fs::read_dir(&dir).expect("package dir") {
        let entry = entry.expect("dir entry");
        if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
            let text = fs::read_to_string(entry.path()).expect("read package doc");
            let value: Value = serde_json::from_str(&text).expect("parse package doc");
            digests.push(
                value["package_digest"]
                    .as_str()
                    .expect("package_digest")
                    .to_string(),
            );
        }
    }
    assert_eq!(digests.len(), 1, "exactly one seeded package: {digests:?}");
    digests.pop().expect("one digest")
}

/// Commit a synthetic terminal *landed* execution whose admission event
/// references `digest`. Recovery reads this exactly like a real crashed
/// execution, so a mismatched `(record.execution_id, record.task_id)` versus the
/// referenced package is exercised through production recovery.
fn commit_recovery_execution(state_dir: &Path, execution_id: &str, task_id: &str, digest: &str) {
    let mut ledger =
        agalma_ledger::SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger");
    let execution = ExecutionId::new(execution_id);
    let record = ExecutionRecord {
        execution_id: execution.clone(),
        task_id: TaskId::new(task_id),
        generation: 1,
        phase: ExecutionPhase::Done,
        state: ExecutionState::Completed,
        attempt: 1,
        revision: 0,
    };
    let event = ExecutionEvent {
        execution_id: execution,
        sequence: 0,
        kind: "task.package_admitted".to_string(),
        payload: json!({ "package_digest": digest }),
        recorded_at_unix_ms: 0,
    };
    ledger
        .commit(CommitBatch {
            expected_revision: None,
            state: Some(record),
            events: vec![event],
            receipts: vec![],
            intents: vec![],
        })
        .expect("commit synthetic recovery execution");
}

// ===========================================================================
// 1. The first integrated change lands inside the review baseline
// ===========================================================================

#[test]
fn first_integrated_change_is_inside_the_review_baseline() {
    let prod = Prod::new("first");
    let epic = prod.create_epic("Baseline epic");
    let (child, gate) = prod.create_gated_child(&epic, 0);
    prod.resolve_gate(&gate);

    // The pre-integration base: the revision before the candidate lands. In the
    // SF.10 defect this and the candidate collapsed to the same (post-land) SHA.
    let base_head = prod.head();
    prod.protect_all(&[child.clone()], &["src"], &["tests"]);

    let out = prod.work_once();
    assert!(
        out.status.success(),
        "work: {}\n{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert_eq!(prod.status(&child), "closed", "the released child landed");
    let candidate = prod.head();
    assert_ne!(candidate, base_head, "the feature advanced the origin");

    // The production baseline must be the pre-integration revision, not HEAD.
    let start = prod
        .review_start(&epic)
        .unwrap_or_else(|| panic!("epic {} has no review baseline", epic));
    assert_eq!(
        start, base_head,
        "the review baseline must be the pre-integration revision, not the post-land HEAD"
    );
    assert_ne!(
        start, candidate,
        "the baseline must precede the integrated candidate"
    );

    // The reviewer's admitted baseline/candidate and diff must cover the first
    // `src/lib.rs` `answer() == 0 -> 42` change (not merely Git metadata).
    let reviewer = ScriptedReviewer::new("review complete\nREVIEW: APPROVED\n");
    let report =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");
    assert_eq!(
        reviewer.calls(),
        1,
        "exactly one reviewer dispatch: {report:?}"
    );
    assert_eq!(report.approved, vec![epic.clone()], "{report:?}");

    let dir = reviewer.attempt_dir().expect("captured attempt dir");
    let admission = read_dir_json(&dir, "admission.json");
    let admitted_base = admission["base_sha"]
        .as_str()
        .expect("base_sha")
        .to_string();
    let admitted_candidate = admission["candidate_sha"]
        .as_str()
        .expect("candidate_sha")
        .to_string();
    assert_eq!(
        admitted_base, base_head,
        "admitted baseline is pre-integration"
    );
    assert_eq!(
        admitted_candidate, candidate,
        "admitted candidate is the landed feature"
    );
    assert_ne!(
        admitted_base, admitted_candidate,
        "never an empty review interval"
    );

    prod.assert_feature("baseline", &admitted_base, false);
    prod.assert_feature("candidate", &admitted_candidate, true);

    let diff = fs::read_to_string(dir.join("diff.patch")).expect("read diff.patch");
    assert!(
        diff.contains("src/lib.rs"),
        "diff covers the feature file: {diff:?}"
    );
    assert!(
        diff.contains("+pub fn answer() -> u32 {") && diff.contains("42"),
        "diff includes the first feature change: {diff:?}"
    );

    assert_eq!(prod.status(&epic), "closed", "the epic is actually closed");
    assert!(prod
        .labels(&epic)
        .iter()
        .any(|l| l == REVIEW_APPROVED_LABEL));
}

// ===========================================================================
// 2. Multi-child review still includes the first change
// ===========================================================================

#[test]
fn multi_child_review_includes_the_first_integrated_change() {
    let prod = Prod::new("multi");
    let epic = prod.create_epic("Multi-child epic");
    let mut children = Vec::new();
    for index in 0..2 {
        let (child, gate) = prod.create_gated_child(&epic, index);
        prod.resolve_gate(&gate);
        children.push(child);
    }

    let base_head = prod.head();
    let base_count = prod.commit_count();
    for _ in 0..children.len() {
        prod.protect_all(&children, &["src"], &["tests"]);
        let out = prod.work_once();
        assert!(out.status.success(), "work: {}", stderr_of(&out));
    }
    for child in &children {
        assert_eq!(prod.status(child), "closed", "child {child} closed");
    }
    // The first child lands the shared fix; later siblings are no-ops, so exactly
    // one candidate commit lands.
    assert_eq!(
        prod.commit_count(),
        base_count + 1,
        "exactly one candidate commit"
    );

    let start = prod.review_start(&epic).expect("review baseline");
    assert_eq!(start, base_head, "first land pins the pre-integration base");
    assert_ne!(start, prod.head(), "baseline precedes the merged candidate");

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");
    assert_eq!(reviewer.calls(), 1, "{report:?}");
    assert_eq!(report.approved, vec![epic.clone()], "{report:?}");

    let dir = reviewer.attempt_dir().expect("attempt dir");
    let diff = fs::read_to_string(dir.join("diff.patch")).expect("diff.patch");
    assert!(
        diff.contains("src/lib.rs") && diff.contains("42"),
        "multi-child diff must include the FIRST change, not only later changes: {diff:?}"
    );
}

// ===========================================================================
// 3. Duplicate / restart preserves the pin and never resends the review
// ===========================================================================

#[test]
fn duplicate_and_restart_preserve_the_pin_without_resending_the_review() {
    let prod = Prod::new("dup");
    let epic = prod.create_epic("Duplicate epic");
    let (child, gate) = prod.create_gated_child(&epic, 0);
    prod.resolve_gate(&gate);

    let base_head = prod.head();
    prod.protect_all(&[child.clone()], &["src"], &["tests"]);
    assert!(prod.work_once().status.success(), "initial work");

    let landed = prod.head();
    let count = prod.commit_count();
    let pin = prod.review_start(&epic).expect("baseline recorded");
    assert_eq!(pin, base_head);

    // Duplicate delivery: a second pass never re-integrates and never moves the
    // pin (the child is closed; the baseline is first-pin-wins).
    let repeat = prod.work_once();
    assert!(
        repeat.status.success(),
        "repeat work: {}",
        stdout_of(&repeat)
    );
    assert_eq!(prod.head(), landed, "no second integration");
    assert_eq!(prod.commit_count(), count, "no extra commit on repeat");
    assert_eq!(
        prod.review_start(&epic).as_deref(),
        Some(pin.as_str()),
        "pin unchanged"
    );

    // First review approves and closes.
    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let first =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");
    assert_eq!(reviewer.calls(), 1, "{first:?}");
    assert_eq!(prod.status(&epic), "closed");

    // Restart: a fresh reviewer pass over the same state never re-dispatches and
    // the terminal pin/closure are preserved.
    let restarted = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let again = review_epics(
        &prod.origin,
        &prod.state_dir,
        &route_available(),
        &restarted,
    )
    .expect("review");
    assert_eq!(
        restarted.calls(),
        0,
        "no duplicate reviewer dispatch: {again:?}"
    );
    assert!(again.approved.is_empty(), "{again:?}");
    assert_eq!(prod.review_start(&epic).as_deref(), Some(pin.as_str()));
    assert_eq!(prod.status(&epic), "closed");
}

// ===========================================================================
// 4. Unknown / conflicting / missing baseline refuses closure
// ===========================================================================

#[test]
fn unknown_conflicting_or_missing_baseline_denies_closure() {
    let prod = Prod::new("deny");
    let head = prod.head();

    // (a) Unknown identity: a baseline that is not a commit in the repository.
    let unknown_epic = prod.create_epic("Unknown baseline epic");
    prod.create_closed_staged_child(&unknown_epic, 0);
    prod.bd_ok(&[
        "update",
        &unknown_epic,
        "--add-label",
        &format!("{REVIEW_START_PREFIX}{}", "f".repeat(40)),
    ]);

    // (b) Conflicting identity: the baseline equals the candidate (the exact
    // SF.10 defect shape — an empty interval).
    let equal_epic = prod.create_epic("Equal baseline epic");
    prod.create_closed_staged_child(&equal_epic, 0);
    prod.bd_ok(&[
        "update",
        &equal_epic,
        "--add-label",
        &format!("{REVIEW_START_PREFIX}{head}"),
    ]);

    // (c) Missing identity: no baseline at all.
    let missing_epic = prod.create_epic("Missing baseline epic");
    prod.create_closed_staged_child(&missing_epic, 0);

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");

    assert_eq!(
        reviewer.calls(),
        0,
        "an unknown/conflicting/missing baseline dispatches no reviewer: {report:?}"
    );
    assert!(report.approved.is_empty(), "{report:?}");
    for epic in [&unknown_epic, &equal_epic, &missing_epic] {
        assert_eq!(prod.status(epic), "open", "epic {epic} must stay open");
        assert!(
            !prod.labels(epic).iter().any(|l| l == REVIEW_APPROVED_LABEL),
            "epic {epic} is never approved"
        );
    }
    assert!(
        prod.labels(&equal_epic)
            .iter()
            .any(|l| l == REVIEW_DRIFT_LABEL),
        "an empty review interval holds dispatch (fail closed)"
    );
}

// ===========================================================================
// 5. Crash before integration records no baseline; recovery pins pre-integration
// ===========================================================================

#[test]
fn crash_before_integration_records_no_baseline_and_recovery_pins_preintegration() {
    let prod = Prod::new("crash");
    let epic = prod.create_epic("Crash epic");
    let (child, gate) = prod.create_gated_child(&epic, 0);
    prod.resolve_gate(&gate);

    let base_head = prod.head();
    let base_count = prod.commit_count();
    prod.protect_all(&[child.clone()], &["src"], &["tests"]);

    // Crash after the frozen package is admitted but before any dispatch: the
    // process aborts (SIGABRT), so no integration and no baseline can exist.
    let out = prod.work_once_with(&[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert!(
        !out.status.success(),
        "the injected crash aborts the work run"
    );
    assert_eq!(prod.head(), base_head, "a crash lands no candidate");
    assert_eq!(prod.commit_count(), base_count, "no commit on crash");
    assert!(
        prod.review_start(&epic).is_none(),
        "a crashed run records no review baseline"
    );

    // Restart: boot recovery drives the frozen execution to completion, which
    // integrates the candidate and must pin the *pre-integration* base — never
    // the post-recovery HEAD.
    let restart = prod.work_once();
    assert!(restart.status.success(), "restart: {}", stderr_of(&restart));
    assert_eq!(
        prod.status(&child),
        "closed",
        "recovery completed the child"
    );
    let candidate = prod.head();
    assert_ne!(candidate, base_head, "recovery landed the feature");
    assert_eq!(
        prod.review_start(&epic).as_deref(),
        Some(base_head.as_str()),
        "restart pins the pre-integration base, not the post-recovery HEAD"
    );

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");
    assert_eq!(reviewer.calls(), 1, "{report:?}");
    let dir = reviewer.attempt_dir().expect("attempt dir");
    let diff = fs::read_to_string(dir.join("diff.patch")).expect("diff.patch");
    assert!(
        diff.contains("src/lib.rs") && diff.contains("42"),
        "diff covers the change: {diff:?}"
    );
}

// ===========================================================================
// 5b. Unreconcilable first pin fails closed; a valid preexisting pin recovers
// ===========================================================================

#[test]
fn unreconcilable_first_pin_refuses_closure_and_ordinary_work() {
    let prod = Prod::new("pin-lost");
    let epic = prod.create_epic("Lost pin epic");
    let (child_a, gate_a) = prod.create_gated_child(&epic, 0);
    prod.resolve_gate(&gate_a);
    let (child_b, gate_b) = prod.create_gated_child(&epic, 1);
    prod.resolve_gate(&gate_b);

    let base_head = prod.head();
    prod.protect_all(&[child_a.clone(), child_b.clone()], &["src"], &["tests"]);

    // Land child A, then crash after integration before the first pin is durable.
    let out = prod.work_once_with(&[("AGALMA_REVIEW_BASELINE_CRASH_AT", "after_integrate")]);
    assert!(
        !out.status.success(),
        "the injected crash aborts after integration"
    );
    let candidate = prod.head();
    assert_ne!(candidate, base_head, "the candidate landed");
    assert!(prod.review_start(&epic).is_none(), "no pin recorded");

    // The retained durable recovery input is now missing/corrupt.
    let removed = prod.corrupt_frozen_packages();
    assert!(removed >= 1, "removed at least one frozen package document");

    // Restart must fail closed: no closure, no ordinary work, no later child.
    let restart = prod.work_once();
    assert!(
        !restart.status.success(),
        "an unreconcilable first pin refuses the run: {}",
        stdout_of(&restart)
    );
    assert!(
        prod.review_start(&epic).is_none(),
        "no masked later pin is ever recorded"
    );
    assert_ne!(
        prod.status(&child_a),
        "closed",
        "the landed child is NOT projected closed without a first pin"
    );
    assert_eq!(
        prod.status(&child_b),
        "open",
        "a later child is never claimed"
    );
    assert_eq!(
        prod.head(),
        candidate,
        "ordinary work integrates nothing further"
    );
    assert_eq!(prod.status(&epic), "open", "the epic stays open");

    // The truncated state is never reviewed, approved, or closed.
    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");
    assert_eq!(reviewer.calls(), 0, "no reviewer dispatch: {report:?}");
    assert!(report.approved.is_empty(), "{report:?}");
    assert_eq!(prod.status(&epic), "open");
    assert!(!prod
        .labels(&epic)
        .iter()
        .any(|l| l == REVIEW_APPROVED_LABEL));
}

#[test]
fn valid_preexisting_pin_remains_recoverable_when_the_package_is_lost() {
    let prod = Prod::new("pin-kept");
    let epic = prod.create_epic("Kept pin epic");
    let (child, gate) = prod.create_gated_child(&epic, 0);
    prod.resolve_gate(&gate);

    let base_head = prod.head();
    // A preexisting valid first pin, as an earlier landed child would have left.
    prod.bd_ok(&[
        "update",
        &epic,
        "--add-label",
        &format!("{REVIEW_START_PREFIX}{base_head}"),
    ]);
    prod.protect_all(&[child.clone()], &["src"], &["tests"]);

    let out = prod.work_once_with(&[("AGALMA_REVIEW_BASELINE_CRASH_AT", "after_integrate")]);
    assert!(
        !out.status.success(),
        "the injected crash aborts after integration"
    );
    let candidate = prod.head();
    assert_ne!(candidate, base_head);

    prod.corrupt_frozen_packages();

    // An already valid pin makes the landed child safe to project closed.
    let restart = prod.work_once();
    assert!(
        restart.status.success(),
        "a valid preexisting pin is recoverable despite a lost package: {}",
        stderr_of(&restart)
    );
    assert_eq!(
        prod.review_start(&epic).as_deref(),
        Some(base_head.as_str()),
        "the valid first pin is preserved (first pin wins)"
    );
    assert_eq!(
        prod.status(&child),
        "closed",
        "the landed child is reconciled closed"
    );

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");
    assert_eq!(reviewer.calls(), 1, "{report:?}");
    let dir = reviewer.attempt_dir().expect("attempt dir");
    let diff = fs::read_to_string(dir.join("diff.patch")).expect("diff.patch");
    assert!(
        diff.contains("src/lib.rs") && diff.contains("42"),
        "{diff:?}"
    );
    assert_eq!(prod.status(&epic), "closed");
}

#[test]
fn mismatched_recovered_execution_identity_refuses_the_first_pin() {
    let prod = Prod::new("id-exec");
    let epic = prod.create_epic("Identity exec epic");
    let child = prod.create_plain_child(&epic, "child");

    // A real, self-consistent package whose execution id (`exec:<child>:1`) does
    // NOT match the recovery record's execution id (`exec:<child>:9`).
    common::seed_recorded_package(
        &prod.state_dir,
        &child,
        1,
        &["sh -c 'true'".to_string()],
        &["src".to_string()],
    );
    let digest = seeded_package_digest(&prod.state_dir);
    commit_recovery_execution(&prod.state_dir, &format!("exec:{child}:9"), &child, &digest);

    let out = prod.work_once();
    assert!(
        !out.status.success(),
        "a mismatched execution identity must fail closed: {}\n{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        stderr_of(&out).contains("does not match recovery record"),
        "the fail-closed reason is the identity check: {}",
        stderr_of(&out)
    );
    assert!(
        prod.review_start(&epic).is_none(),
        "no review baseline is pinned from a mismatched package"
    );
    assert_eq!(
        prod.status(&child),
        "open",
        "the child is never projected closed"
    );
    assert_eq!(prod.status(&epic), "open", "the epic stays open");

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");
    assert_eq!(reviewer.calls(), 0, "no reviewer dispatch: {report:?}");
    assert!(report.approved.is_empty(), "{report:?}");
}

#[test]
fn mismatched_recovered_task_identity_refuses_the_first_pin() {
    let prod = Prod::new("id-task");
    let epic = prod.create_epic("Identity task epic");
    let child = prod.create_plain_child(&epic, "child");

    // A real package for a DIFFERENT task, referenced by this child's record:
    // execution ids match (`exec:other-task:1`) but the task ids differ.
    common::seed_recorded_package(
        &prod.state_dir,
        "other-task",
        1,
        &["sh -c 'true'".to_string()],
        &["src".to_string()],
    );
    let digest = seeded_package_digest(&prod.state_dir);
    commit_recovery_execution(&prod.state_dir, "exec:other-task:1", &child, &digest);

    let out = prod.work_once();
    assert!(
        !out.status.success(),
        "a mismatched task identity must fail closed: {}\n{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(
        stderr_of(&out).contains("does not match recovery record"),
        "the fail-closed reason is the identity check: {}",
        stderr_of(&out)
    );
    assert!(
        prod.review_start(&epic).is_none(),
        "no review baseline is pinned from a mismatched package"
    );
    assert_eq!(
        prod.status(&child),
        "open",
        "the child is never projected closed"
    );
    assert_eq!(prod.status(&epic), "open", "the epic stays open");
}

// ===========================================================================
// 6. Stop before claim records no baseline; resume pins correctly
// ===========================================================================

#[test]
fn crash_after_integration_before_pin_reconciles_preintegration_base_on_restart() {
    let prod = Prod::new("pin-gap");
    let epic = prod.create_epic("Pin-gap epic");
    let (child, gate) = prod.create_gated_child(&epic, 0);
    prod.resolve_gate(&gate);

    let base_head = prod.head();
    prod.protect_all(&[child.clone()], &["src"], &["tests"]);

    // Crash *after* the candidate integrated but *before* the epic review
    // baseline was durably recorded. This is the window the fresh path could not
    // survive: the ledger execution is already terminal (recovery skips it) and
    // the package scratch is reaped.
    let out = prod.work_once_with(&[("AGALMA_REVIEW_BASELINE_CRASH_AT", "after_integrate")]);
    assert!(
        !out.status.success(),
        "the injected crash aborts after integration"
    );
    let candidate = prod.head();
    assert_ne!(candidate, base_head, "the feature landed before the crash");
    assert!(
        prod.review_start(&epic).is_none(),
        "the crash left no review baseline even though the change landed"
    );

    // Restart: boot recovery reconciles the baseline from the retained durable
    // frozen package input, so the next child can never mask the first change.
    let restart = prod.work_once();
    assert!(restart.status.success(), "restart: {}", stderr_of(&restart));
    assert_eq!(
        prod.review_start(&epic).as_deref(),
        Some(base_head.as_str()),
        "recovery pins the exact original pre-integration SHA"
    );
    assert_eq!(
        prod.status(&child),
        "closed",
        "the landed child is reconciled closed"
    );

    // The reviewer diff still covers the full first feature change.
    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report =
        review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer).expect("review");
    assert_eq!(reviewer.calls(), 1, "{report:?}");
    let dir = reviewer.attempt_dir().expect("attempt dir");
    let admission = read_dir_json(&dir, "admission.json");
    assert_eq!(admission["base_sha"].as_str(), Some(base_head.as_str()));
    assert_eq!(
        admission["candidate_sha"].as_str(),
        Some(candidate.as_str())
    );
    let diff = fs::read_to_string(dir.join("diff.patch")).expect("diff.patch");
    assert!(
        diff.contains("src/lib.rs") && diff.contains("42"),
        "the reconciled baseline still yields the full feature diff: {diff:?}"
    );
    assert_eq!(prod.status(&epic), "closed");
}

#[test]
fn stop_before_claim_records_no_baseline_and_resume_pins_correctly() {
    let prod = Prod::new("cancel");
    let epic = prod.create_epic("Cancelled epic");
    let (child, gate) = prod.create_gated_child(&epic, 0);
    prod.resolve_gate(&gate);

    let base_head = prod.head();
    prod.protect_all(&[child.clone()], &["src"], &["tests"]);

    // Cancel: the durable kill latch halts the loop before any claim, so no
    // execution and no review baseline are recorded.
    prod.set_kill_latch(true);
    let stopped = prod.work_once();
    assert!(!stopped.status.success(), "a latched stop refuses the loop");
    assert_eq!(prod.status(&child), "open", "cancel lands nothing");
    assert!(
        prod.review_start(&epic).is_none(),
        "a cancelled run records no baseline"
    );
    prod.set_kill_latch(false);

    // Resume: a clean run lands and pins the pre-integration base (unchanged).
    let resumed = prod.work_once();
    assert!(resumed.status.success(), "resume: {}", stderr_of(&resumed));
    assert_eq!(prod.status(&child), "closed", "resume lands the child");
    assert_eq!(
        prod.review_start(&epic).as_deref(),
        Some(base_head.as_str()),
        "resume pins the same pre-integration base"
    );
}

// ===========================================================================
// 7. Generated operation-sequence property: baseline validity denies truncation
// ===========================================================================

/// A minimal deterministic linear-congruential generator (no external crate).
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed | 1)
    }
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % bound
    }
}

#[test]
fn generated_baseline_states_deny_a_truncated_or_unknown_review() {
    const SEED: u64 = 0x5F18_9001_CAFE_0018;
    const CASES: usize = 32;

    let prod = Prod::new("prop");
    // A linear chain of real commits so genuine ancestors exist.
    let mut chain = vec![prod.head()];
    let mut rng = Lcg::new(SEED);
    let mut approved = 0usize;
    let mut denied = 0usize;

    for case in 0..CASES {
        // Grow the chain so a proper ancestor is always available.
        for _ in 0..(1 + rng.below(2)) {
            chain.push(prod.commit(&format!("chain {case}")));
        }
        let head = prod.head();

        let epic = prod.create_epic(&format!("Generated epic {case}"));
        prod.create_closed_staged_child(&epic, 0);

        // 0 valid proper ancestor, 1 equal (empty interval), 2 unknown,
        // 3 missing.
        let mode = rng.below(4);
        let mut expect_approve = false;
        match mode {
            0 => {
                let idx = rng.below((chain.len() - 1) as u64) as usize;
                let base = chain[idx].clone();
                assert_ne!(base, head, "a proper ancestor precedes the candidate");
                prod.bd_ok(&[
                    "update",
                    &epic,
                    "--add-label",
                    &format!("{REVIEW_START_PREFIX}{base}"),
                ]);
                expect_approve = true;
            }
            1 => {
                prod.bd_ok(&[
                    "update",
                    &epic,
                    "--add-label",
                    &format!("{REVIEW_START_PREFIX}{head}"),
                ]);
            }
            2 => {
                prod.bd_ok(&[
                    "update",
                    &epic,
                    "--add-label",
                    &format!("{REVIEW_START_PREFIX}{:040x}", rng.below(u64::MAX)),
                ]);
            }
            _ => {}
        }

        let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
        let report = review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer)
            .expect("review pass");

        if expect_approve {
            approved += 1;
            assert_eq!(reviewer.calls(), 1, "case={case} mode={mode} {report:?}");
            assert!(
                report.approved.contains(&epic),
                "case={case} mode={mode} a valid pre-candidate baseline approves: {report:?}"
            );
            assert_eq!(prod.status(&epic), "closed", "case={case}");
        } else {
            denied += 1;
            assert_eq!(
                reviewer.calls(),
                0,
                "case={case} mode={mode} an invalid/unknown/missing baseline never approves: {report:?}"
            );
            assert!(
                !prod
                    .labels(&epic)
                    .iter()
                    .any(|l| l == REVIEW_APPROVED_LABEL),
                "case={case} mode={mode}"
            );
            assert_eq!(prod.status(&epic), "open", "case={case}");
            // Keep the open-epic set bounded for the next pass: the terminal
            // state here is "denied", proven above; close it for cleanup only.
            prod.bd_ok(&["close", &epic, "--reason", "property cleanup", "--force"]);
        }
    }

    assert!(approved > 0, "seed/budget produced no approving case");
    assert!(denied > 0, "seed/budget produced no denying case");
    eprintln!("sf18 property: seed={SEED:#x} cases={CASES} approved={approved} denied={denied}");
}
