//! SF.10 acceptance: fresh goal -> design -> staged/native-human-approved
//! children -> execution/protected verification -> independent review -> actual
//! epic closure (`agalma-5fy.10`).
//!
//! Focused command (unified runner): `make test-focused SELECTION='-p
//! agalma-conductor --test sf_pipeline_proof'`.
//!
//! This is an **integrated pipeline proof**. It does not add production
//! features; it drives the real production control paths over a fresh bounded
//! scratch source-feature goal (`fixtures/self-build/pipeline-goal.md`) in an
//! isolated scratch git origin + scratch `bd` store:
//!
//! 1. **goal** — `concierge::run_concierge` turns the fixture goal into a
//!    designed epic through the shared planner-route seam (the concierge
//!    *session* is scripted via the established `ConciergeSession` seam; the
//!    output-contract parse, route decision, and `bd` epic/design verification
//!    are real).
//! 2. **design** — `sweep::sweep_epic` runs one scoped designer attempt through
//!    the `DesignerSession` seam; the CP.1 decomposition gate and the native
//!    human execution-approval reconciliation are real.
//! 3. **staged/native-human-approved children** — open `staged` children
//!    each carry exactly one well-formed native human gate; a human resolves each
//!    gate with `bd gate resolve` (the explicit bootstrap authorization). The
//!    release is *not* a cleared date or a label.
//! 4. **execution** — the real `agalma work` production path (scripted
//!    activity mode) freezes an immutable `TaskPackage/v1`, ships the candidate
//!    only after the operator **protected baseline** independently judges its
//!    behavior, and integrates it. The paid planner/reviewer gate stays closed,
//!    so the work-loop boot sweep/review cannot slip a live session through.
//! 5. **review/closure** — `review::review_epics` runs prerequisite-aware
//!    admission and one independent reviewer session (scripted seam) over the
//!    merged diff; approval is rechecked against the live graph before the epic
//!    is actually closed in `bd`. In the same fresh lineage, an open blocking
//!    prerequisite first waits with zero reviewer calls and no new drift hold,
//!    then the completed prerequisite admits exactly one review and closure.
//!
//! Named negative cases: invalid decomposition cannot silently close; a missing
//! human release leaves children open and the epic open; a designer that
//! times out *after authoring* still reconciles its committed decomposition; an
//! open blocking prerequisite waits with **zero reviewer calls** and no new
//! drift hold; a drift verdict withholds closure; a graph/design edit between
//! dispatch and approval withholds stale closure. A deterministic generated
//! operation-sequence property re-derives the decomposition-gate and
//! approval-association verdicts from an independent model.
//!
//! Everything is deterministic scripted transport (no network, no paid/live
//! provider inference). Scripted evidence is never a model/vendor performance
//! claim. Scratch state lives under `target/test-runs/`; the runner retains it
//! at the bead's persistent evidence root (see `docs/self-build-evidence.md`).

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;

use agalma_conductor::concierge::{run_concierge, ConciergeRequest, ConciergeSession};
use agalma_conductor::phases::{effects_log_path, run_root, scope_pin_path};
use agalma_conductor::review::{
    approved_snapshot_path, dispatch_hold, review_epics, ReviewRequest, ReviewerSession,
    REVIEW_APPROVED_LABEL, REVIEW_DRIFT_LABEL, REVIEW_START_PREFIX,
};
use agalma_conductor::routing::{resolve_builder_route, RoleModelOverride};
use agalma_conductor::scope_guard::PROTECTED_BASELINE_FORMAT;
use agalma_conductor::sweep::{
    sweep_epic, DesignerOutcome, DesignerRequest, DesignerSession, PlannerGate,
};
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::{ContractError, ExecutionId, LedgerApi, Role, TaskFamily};
use agalma_taskqueue::{
    approval_state, decomp, ApprovalGate, ApprovalState, BdTaskQueue, DECOMP_APPROVED_LABEL,
    DECOMP_FIX_LABEL, EXECUTION_APPROVAL_LABEL, HUMAN_LABEL, STAGED_LABEL,
};
use agalma_testkit::ScratchDir;
use serde_json::{json, Value};

/// The frozen goal fixture the test feeds to the concierge seam.
const GOAL: &str = include_str!("../../../fixtures/self-build/pipeline-goal.md");
/// The reviewed Flash builder route this epic targets (never fabricated).
const FLASH: &str = "opencode-go/deepseek-v4.1-flash";
/// A reviewed free model used for the builder-only selector (so the builder pin
/// is distinct from the independently resolved verifier route).
const FREE_BUILDER: &str = "opencode/mimo-v2.6-flash-free";
/// The frozen task acceptance the scratch feature must satisfy.
const ACCEPTANCE: &str = "sh -c 'grep -q 42 src/lib.rs'";
/// The scratch origin seed and the scripted build's deterministic result.
const SEED_SOURCE: &str = "//! Seed source.\npub fn answer() -> u32 { 0 }\n";
/// Repository instruction/shared-design bytes the production admission freezes.
const AGENTS_BYTES: &str = "# Agents\nRepository working rules (frozen at admission).\n";
const SHARED_DESIGN_BYTES: &str =
    "# Self-mutation contracts revision 1\nFrozen shared-design bytes.\n";
/// The epic design the scripted concierge authors (pinned by review admission).
const EPIC_DESIGN: &str = "# Design\n\nDeliver the bounded scratch source feature end to end; \
     the epic is reviewed against this immutable design.\n";

/// The immutable protected *behavioral* judge: it independently compiles the
/// candidate library and asserts `answer() == 42` by execution, so a comment
/// containing the expected text cannot spoof it. Reuses the existing `rustc`
/// (no new dependency, no network).
const PROTECTED_SCRIPT: &str = "#!/bin/sh\n\
set -eu\n\
cand=\"$AGALMA_CANDIDATE_DIR\"\n\
work=\"$TMPDIR/sf10-protected-probe\"\n\
rm -rf \"$work\"\n\
mkdir -p \"$work\"\n\
rustc --edition 2021 --crate-type lib --crate-name cand \"$cand/src/lib.rs\" --out-dir \"$work\"\n\
printf '%s\\n' 'fn main() {' '    if cand::answer() != 42 {' '        eprintln!(\"protected behavior: answer() != 42\");' '        std::process::exit(1);' '    }' '}' > \"$work/main.rs\"\n\
rustc --edition 2021 \"$work/main.rs\" --extern cand=\"$work/libcand.rlib\" -L \"$work\" -o \"$work/probe\"\n\
\"$work/probe\"\n";
const PROTECTED_COMMAND: &str = "sh $AGALMA_PROTECTED_DIR/runner.sh";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

/// Select the in-process ledger binding for `bind_stop_control` once, so no
/// `agalma-ledger-server` process is spawned from a test binary.
fn ensure_in_process_ledger() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| std::env::set_var("AGALMA_LEDGER_TRANSPORT", "in-process"));
}

fn scratch(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(name);
    dir.keep();
    dir
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

/// A low-tier child description that satisfies every CP.1 rule: the six required
/// headings in order, a fenced task block, and the frozen scratch acceptance.
/// Prose deliberately avoids the difficulty markers (`derive_tier` scans the
/// whole description) so the `difficulty:low` label matches.
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

/// The child design (low tier, under the 150-word cap).
const CHILD_DESIGN: &str = "Small, single-file change; no new interface.";

fn gate(allow_paid: bool, paid_available: bool) -> PlannerGate {
    PlannerGate::new(allow_paid, paid_available, None)
}

/// A planner gate with an eligible (paid) route: used only with *injected*
/// scripted sessions, so the route decision is recorded but no live session runs.
fn route_available() -> PlannerGate {
    gate(true, true)
}

/// The frozen package's role pin for `role`.
fn role_model(package: &agalma_contracts::TaskPackage, role: Role) -> String {
    package
        .roles
        .iter()
        .find(|pin| pin.role == role)
        .unwrap_or_else(|| panic!("frozen package has no {role:?} role pin"))
        .model
        .clone()
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
        let dir = scratch(&format!("sf-pipeline-{name}"));
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
        run_ok("git", &origin, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &origin, &["config", "user.name", "Agalma"]);
        run_ok("git", &origin, &["add", "-A"]);
        run_ok("git", &origin, &["commit", "-qm", "seed"]);

        let prefix = format!("sfa{}", name.replace(['-', '.'], ""));
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
        self.show(id)["status"].as_str().unwrap_or("unknown").to_string()
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

    /// The epic's children (all statuses) as raw `bd list` rows.
    fn children(&self, epic: &str) -> Vec<Value> {
        let stdout = self.bd_ok(&["list", "--parent", epic, "--all", "--json", "--limit", "0"]);
        serde_json::from_str(&stdout).expect("children json")
    }

    /// The ids of the epic's `staged` decomposition children.
    fn staged_children(&self, epic: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .children(epic)
            .iter()
            .filter(|child| {
                child["labels"]
                    .as_array()
                    .map(|l| l.iter().any(|v| v.as_str() == Some(STAGED_LABEL)))
                    .unwrap_or(false)
            })
            .filter_map(|child| child["id"].as_str().map(str::to_string))
            .collect();
        ids.sort();
        ids
    }

    /// The id of the single native gate directly blocking `child`.
    fn native_gate_id(&self, child: &str) -> String {
        let issue = self.show(child);
        issue["dependencies"]
            .as_array()
            .and_then(|deps| {
                deps.iter()
                    .find(|d| {
                        d["dependency_type"] == "blocks" && d["issue_type"] == "gate"
                    })
                    .and_then(|d| d["id"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| panic!("child {child} has no native gate: {issue}"))
    }

    /// Whether `child` is blocked by exactly one native human gate and its
    /// approval association is well formed (unresolved or resolved).
    fn child_approval_state(&self, child: &str) -> ApprovalState {
        let queue = BdTaskQueue::new(&self.origin);
        let issue = self.show(child);
        let blockers: Vec<String> = issue["dependencies"]
            .as_array()
            .map(|deps| {
                deps.iter()
                    .filter(|d| d["dependency_type"] == "blocks")
                    .filter_map(|d| d["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let gates = queue.approval_gates().expect("approval gates");
        approval_state(child, &blockers, &gates)
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
        fs::create_dir_all(&self.state_dir).expect("state dir");
        let doc = json!({ "format": "ProtectedInputs/v1", "entries": entries });
        let path = self.state_dir.join("protected-inputs.json");
        fs::write(&path, serde_json::to_vec_pretty(&doc).expect("json"))
            .expect("write manifest");
    }

    /// Run `agalma work --once` against the scratch origin in scripted mode.
    ///
    /// Only the *builder-scoped* selector is passed (a free model): it never sets
    /// the legacy global `--model`, so the paid planner/reviewer gate stays
    /// unavailable to the boot sweep/review and cannot dispatch a live
    /// designer/reviewer. Ordinary dispatch proceeds on the static free routes;
    /// the builder pin is distinct from the independently resolved verifier.
    fn work_once(&self) -> Output {
        Command::new(bin())
            .args([
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
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("AGALMA_ALLOW_PAID")
            .env_remove("AGALMA_MODEL")
            .env_remove("AGALMA_BUILDER_MODEL")
            .output()
            .expect("run agalma work")
    }

    /// Bind a read-only in-process ledger handle over the existing state
    /// directory. Reads go through the canonical `LedgerApi`; the test never
    /// opens the database or issues SQL itself.
    fn ledger(&self) -> agalma_conductor::ledger_process::LedgerHandle {
        agalma_conductor::ledger_process::LedgerHandle::from_transport_existing(
            &self.state_dir,
            agalma_conductor::ledger_process::LedgerTransport::InProcess,
        )
        .expect("open existing in-process ledger")
    }

    /// Whether an execution row exists. A read error is distinct from an absent
    /// execution (the read is `expect`-ed, so an unreadable ledger fails loudly).
    fn has_execution(&self, execution: &str) -> bool {
        self.ledger()
            .execution(&ExecutionId::new(execution))
            .expect("read execution from the bound ledger")
            .is_some()
    }

    fn execution_phase(&self, execution: &str) -> (String, String) {
        let record = self
            .ledger()
            .execution(&ExecutionId::new(execution))
            .expect("read execution from the bound ledger")
            .expect("execution row");
        (
            format!("{:?}", record.phase).to_lowercase(),
            format!("{:?}", record.state).to_lowercase(),
        )
    }

    /// The `task.package_admitted` payload for `execution`, if recorded.
    fn admitted(&self, execution: &str) -> Option<Value> {
        let events = self
            .ledger()
            .events(&ExecutionId::new(execution))
            .expect("read execution events from the bound ledger");
        events
            .iter()
            .rev()
            .find(|event| event.kind == "task.package_admitted")
            .map(|event| event.payload.clone())
    }

    fn frozen(&self, digest: &str) -> agalma_contracts::TaskPackage {
        agalma_conductor::task_package::load_package(&self.state_dir, digest)
            .expect("load frozen package")
    }

    fn scope_pin(&self, execution: &str) -> Value {
        let path = scope_pin_path(&self.state_dir, &ExecutionId::new(execution));
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_slice(&bytes).expect("scope pin json")
    }

    fn accept_log(&self, execution: &str, attempt: u32, index: usize) -> String {
        let path = run_root(&self.state_dir, &ExecutionId::new(execution))
            .join("artifacts")
            .join(format!("accept@{attempt}-{index}.txt"));
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn effects(&self, execution: &str) -> Vec<String> {
        let path = effects_log_path(&self.state_dir, &ExecutionId::new(execution));
        fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A base commit, an epic with the fixture design, one closed `staged` child,
    /// a work commit, and the recorded review baseline. Mirrors the production
    /// `record_start_on_land` baseline shape (diff = the work commit).
    fn landed_epic(&self, title: &str) -> String {
        let base = self.commit("base");
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
        let epic = value["id"].as_str().expect("epic id").to_string();

        let out = self.bd(&[
            "create",
            "child 0",
            "-t",
            "task",
            "-p",
            "2",
            "--parent",
            &epic,
            "--description",
            "child 0",
            "--labels",
            "staged,difficulty:low",
            "--json",
        ]);
        assert!(out.status.success(), "bd create child: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("child json");
        let child = value["id"].as_str().expect("child id").to_string();
        self.bd_ok(&["close", &child, "--reason", "landed"]);
        self.commit("work");
        self.bd_ok(&[
            "update",
            &epic,
            "--add-label",
            &format!("{REVIEW_START_PREFIX}{base}"),
        ]);
        epic
    }
}

// ---------------------------------------------------------------------------
// Scripted model seams (injected; no harness, no network)
// ---------------------------------------------------------------------------

/// A scripted concierge: authors an epic with the fixture design and returns the
/// output contract the conductor parses.
struct ScriptedConcierge {
    calls: RefCell<Vec<String>>,
}

impl ScriptedConcierge {
    fn new() -> Self {
        ScriptedConcierge {
            calls: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl ConciergeSession for ScriptedConcierge {
    fn run(&self, request: &ConciergeRequest) -> Result<String, ContractError> {
        self.calls.borrow_mut().push(request.goal.clone());
        let repo = &request.repo;
        let out = Command::new("bd")
            .arg("--db")
            .arg(repo.join(".beads"))
            .args([
                "create",
                "Pipeline goal epic",
                "-t",
                "epic",
                "-p",
                "2",
                "--description",
                &request.goal,
                "--design",
                EPIC_DESIGN,
                "--json",
            ])
            .current_dir(repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd in concierge");
        assert!(
            out.status.success(),
            "concierge bd create: {}",
            stderr_of(&out)
        );
        let value: Value = serde_json::from_slice(&out.stdout).expect("epic json");
        let id = value["id"].as_str().expect("epic id").to_string();
        Ok(format!(
            "epic: {id}\ndesign: written to {id} --design\nhandoff: bd show {id}\n"
        ))
    }
}

/// How the scripted designer materializes its decomposition.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DesignerMode {
    /// Eight valid staged children, each with a native gate; Completed.
    Valid,
    /// Two rule-violating children; Completed (the gate must reject).
    Invalid,
    /// Eight valid staged children, then the turn bound (timeout after authoring).
    TimeoutAfterAuthoring,
}

/// A scripted designer: authors children (and gates) via `bd` itself, exactly as
/// the live session would, and reports its terminal outcome.
struct ScriptedDesigner {
    mode: DesignerMode,
    calls: RefCell<Vec<String>>,
}

impl ScriptedDesigner {
    fn new(mode: DesignerMode) -> Self {
        ScriptedDesigner {
            mode,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl DesignerSession for ScriptedDesigner {
    fn run(&self, request: &DesignerRequest) -> Result<DesignerOutcome, ContractError> {
        self.calls.borrow_mut().push(request.epic.clone());
        let repo = &request.repo;
        let bd = |args: &[&str]| {
            let out = Command::new("bd")
                .arg("--db")
                .arg(repo.join(".beads"))
                .args(args)
                .current_dir(repo)
                .env("BD_NON_INTERACTIVE", "1")
                .output()
                .expect("spawn bd in designer");
            assert!(
                out.status.success(),
                "designer bd {args:?}: {}",
                stderr_of(&out)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        // Create a staged child (with the optional labels/design overrides) and
        // pin it with exactly one native, unresolved human execution-approval
        // gate — the designer's contract.
        let create = |description: &str, labels: &str, design: &str| {
            let out = bd(&[
                "create",
                "child",
                "-t",
                "task",
                "-p",
                "2",
                "--parent",
                &request.epic,
                "--description",
                description,
                "--design",
                design,
                "--labels",
                labels,
                "--json",
            ]);
            let child: Value = serde_json::from_str(&out).expect("created child json");
            let child_id = child["id"].as_str().expect("child id").to_string();
            let out = bd(&[
                "gate",
                "create",
                "--type",
                "human",
                "--blocks",
                &child_id,
                "--reason",
                "execution approval",
                "--json",
            ]);
            let gate: Value = serde_json::from_str(&out).expect("created gate json");
            let gate_id = gate["id"].as_str().expect("gate id");
            bd(&[
                "update",
                gate_id,
                "--add-label",
                "human,execution-approval",
                "--set-metadata",
                &format!("approval_for={child_id}"),
            ]);
        };

        match self.mode {
            DesignerMode::Valid | DesignerMode::TimeoutAfterAuthoring => {
                for i in 0..8 {
                    create(&child_description(i), "staged,difficulty:low", CHILD_DESIGN);
                }
                Ok(if self.mode == DesignerMode::Valid {
                    DesignerOutcome::Completed
                } else {
                    DesignerOutcome::TimedOut
                })
            }
            DesignerMode::Invalid => {
                // A low shape labelled high (tier-mismatch) and a child missing a
                // required heading (schema-section); below the minimum too
                // (count-budget).
                create(&child_description(0), "staged,difficulty:high", CHILD_DESIGN);
                let missing = child_description(1).replace("# Non-goals\n", "");
                create(&missing, "staged,difficulty:low", CHILD_DESIGN);
                Ok(DesignerOutcome::Completed)
            }
        }
    }
}

/// A scripted reviewer that counts calls, captures the assignment, and may
/// mutate the backlog mid-review to prove the closure recheck denies a stale
/// approval.
struct ScriptedReviewer {
    output: String,
    calls: Rc<Cell<usize>>,
    mutation: Option<Box<dyn Fn()>>,
    captured: Rc<RefCell<Option<PathBuf>>>,
}

impl ScriptedReviewer {
    fn new(output: &str) -> Self {
        ScriptedReviewer {
            output: output.to_string(),
            calls: Rc::new(Cell::new(0)),
            mutation: None,
            captured: Rc::new(RefCell::new(None)),
        }
    }

    fn with_mutation(output: &str, mutation: Box<dyn Fn()>) -> Self {
        ScriptedReviewer {
            output: output.to_string(),
            calls: Rc::new(Cell::new(0)),
            mutation: Some(mutation),
            captured: Rc::new(RefCell::new(None)),
        }
    }

    fn calls(&self) -> usize {
        self.calls.get()
    }

    fn captured_diff(&self) -> Option<PathBuf> {
        self.captured.borrow().clone()
    }
}

impl ReviewerSession for ScriptedReviewer {
    fn run(&self, request: &ReviewRequest) -> Result<String, ContractError> {
        self.calls.set(self.calls.get() + 1);
        *self.captured.borrow_mut() = Some(request.diff_path.clone());
        if let Some(mutation) = &self.mutation {
            mutation();
        }
        Ok(self.output.clone())
    }
}

// ===========================================================================
// 1. Fresh goal -> design -> staged children -> release -> execution ->
//    protected behavior -> review -> actual epic closure
// ===========================================================================

#[test]
fn fresh_goal_reaches_staged_children_then_execution_review_and_epic_closure() {
    let prod = Prod::new("full");

    // --- 1. goal -> designed epic (real output-contract + bd verification) ---
    let concierge = ScriptedConcierge::new();
    let intake = run_concierge(&prod.origin, &prod.state_dir, GOAL, &route_available(), &concierge)
        .expect("concierge intake");
    assert_eq!(intake.epics.len(), 1, "one designed epic: {intake:?}");
    assert_eq!(
        concierge.calls(),
        vec![GOAL.to_string()],
        "the session was handed the exact fixture goal"
    );
    let epic = intake.epics[0].clone();
    let epic_issue = prod.show(&epic);
    assert_eq!(
        epic_issue["design"].as_str().unwrap_or(""),
        EPIC_DESIGN,
        "the epic carries the pinned immutable design"
    );

    // --- 2/3. designer -> staged children, each gated (gate must approve) ---
    let designer = ScriptedDesigner::new(DesignerMode::Valid);
    let sweep =
        sweep_epic(&prod.origin, &prod.state_dir, &epic, &route_available(), &designer)
            .expect("designer sweep");
    assert_eq!(designer.calls(), vec![epic.clone()]);
    assert_eq!(sweep.approved, vec![epic.clone()], "gate approved: {sweep:?}");
    assert!(!sweep.rejected.contains(&epic), "not rejected: {sweep:?}");

    let children = prod.staged_children(&epic);
    assert_eq!(children.len(), 8, "eight staged children: {children:?}");
    for child in &children {
        assert_eq!(
            prod.child_approval_state(child),
            ApprovalState::Unresolved,
            "each child has exactly one well-formed *unresolved* native gate"
        );
        assert_eq!(
            prod.status(child),
            "open",
            "children stay open and approval-held until the human releases them"
        );
        let labels = prod.labels(child);
        assert!(labels.iter().any(|l| l == DECOMP_APPROVED_LABEL), "{labels:?}");
    }

    // --- a missing release cannot dispatch: no gate resolved yet ---
    prod.protect_all(&children, &["src"], &["tests"]);
    let unreleased = prod.work_once();
    assert!(
        unreleased.status.success(),
        "an unreleased decomposition has no ready work: {}",
        stdout_of(&unreleased)
    );
    for child in &children {
        assert!(
            !prod.has_execution(&format!("exec:{child}:1")),
            "no execution is created for the unreleased child {child}"
        );
    }
    assert_eq!(prod.status(&epic), "open", "the epic is still open");

    // --- 3. human release: resolve each child's native gate ---
    for child in &children {
        let g = prod.native_gate_id(child);
        prod.bd_ok(&["gate", "resolve", &g]);
        assert_eq!(prod.status(&g), "closed", "gate {g} resolved");
    }

    // --- 4. authorized execution of every child through the real work path ---
    let base_head = prod.head();
    let base_count = prod.commit_count();
    for _ in 0..children.len() {
        prod.protect_all(&children, &["src"], &["tests"]);
        let out = prod.work_once();
        assert!(
            out.status.success(),
            "authorized work run: {}\n{}",
            stdout_of(&out),
            stderr_of(&out)
        );
    }
    for child in &children {
        let execution = format!("exec:{child}:1");
        assert!(prod.has_execution(&execution), "execution {execution} exists");
        assert_eq!(
            prod.execution_phase(&execution),
            ("done".to_string(), "completed".to_string()),
            "every released child completed"
        );
        assert_eq!(prod.status(child), "closed", "child {child} closed by execution");
    }

    // The executed feature ships: origin HEAD advanced by the real candidate and
    // every sibling's no-op integration did not add a spurious commit.
    assert_ne!(prod.head(), base_head, "the feature advanced the origin");
    assert_eq!(
        prod.commit_count(),
        base_count + 1,
        "exactly one candidate commit landed (independent children with a shared fix)"
    );

    // --- pinned package / oracle / protected judge artifacts (child 0) ---
    let execution = format!("exec:{}:1", children[0]);
    let admitted = prod.admitted(&execution).expect("admission recorded");
    let digest = admitted["package_digest"]
        .as_str()
        .expect("package digest")
        .to_string();
    let frozen = prod.frozen(&digest);
    assert_eq!(
        frozen.definition.acceptance,
        vec![ACCEPTANCE.to_string()],
        "the frozen acceptance is the scratch feature contract"
    );
    assert!(
        frozen.definition.description.text.contains("# Scope"),
        "frozen description is the complete specification"
    );
    assert_eq!(frozen.scope.allowed_paths, vec!["src".to_string()]);
    assert_eq!(frozen.scope.protected_paths, vec!["tests".to_string()]);

    // Builder-only routing: the scoped selector pins the builder; the verifier
    // is independently resolved, so the role pins stay distinct.
    let builder_pin = role_model(&frozen, Role::Builder);
    let verifier_pin = role_model(&frozen, Role::Verifier);
    assert_eq!(builder_pin, FREE_BUILDER, "the builder-only selector wins");
    assert_ne!(
        builder_pin, verifier_pin,
        "the verifier is not selected by the builder selector"
    );

    // Flash is the intended paid builder target and is refused (never
    // substituted) while the paid gate is closed, so this proof contains no
    // live/paid model calls: scripted mechanics stay distinguishable from live
    // inference and no live session can slip through the work path.
    assert!(
        resolve_builder_route(
            &prod.state_dir,
            "sf10.flash",
            TaskFamily::Feature,
            false,
            false,
            RoleModelOverride::new(Some(FLASH), None),
        )
        .is_err(),
        "a paid Flash builder selection is refused, never substituted"
    );

    let pin = prod.scope_pin(&execution);
    let oracle_digest = pin["oracle_digest"]
        .as_str()
        .expect("oracle digest")
        .to_string();
    assert!(!oracle_digest.is_empty(), "the pin records the oracle digest");
    let candidate_sha = pin["candidate_sha"].as_str().expect("candidate_sha");
    assert_eq!(
        prod.head(),
        candidate_sha,
        "origin main is exactly the verified candidate"
    );

    // The independent protected judge actually ran and passed (index 1), after
    // the frozen task acceptance (index 0).
    let candidate_cmd = prod.accept_log(&execution, 1, 0);
    assert!(
        candidate_cmd.contains("exit: 0") && candidate_cmd.contains("grep -q 42"),
        "frozen task acceptance passed: {candidate_cmd}"
    );
    let protected_cmd = prod.accept_log(&execution, 1, 1);
    assert!(
        protected_cmd.contains("exit: 0") && protected_cmd.contains("AGALMA_PROTECTED_DIR"),
        "independent protected behavioral judge passed: {protected_cmd}"
    );
    let protected_runner = prod
        .state_dir
        .join("protected")
        .join(agalma_workspace::execution_dir_name(&ExecutionId::new(&execution)))
        .join("runner.sh");
    assert!(
        protected_runner.exists(),
        "the protected judge lives at {}",
        protected_runner.display()
    );
    assert!(
        !prod.effects(&execution).is_empty(),
        "the scripted build recorded a phase effect"
    );

    // Review baseline recorded by the production `record_start_on_land` path.
    assert!(
        prod.labels(&epic)
            .iter()
            .any(|l| l.starts_with(REVIEW_START_PREFIX)),
        "the epic carries a production review baseline"
    );

    // --- 4b. SF.11 waiting in the same fresh lineage: an open blocking
    //         prerequisite waits with zero reviewer calls and no new drift hold.
    let out = prod.bd(&[
        "create",
        "Pipeline prerequisite",
        "-t",
        "epic",
        "-p",
        "2",
        "--description",
        "prerequisite for the fresh-goal epic",
        "--json",
    ]);
    assert!(out.status.success(), "prereq epic: {}", stderr_of(&out));
    let value: Value = serde_json::from_slice(&out.stdout).expect("prereq json");
    let prereq = value["id"].as_str().expect("prereq id").to_string();
    prod.bd_ok(&["dep", "add", &epic, &prereq, "--type", "blocks"]);

    let reviewer = ScriptedReviewer::new("review complete\nREVIEW: APPROVED\n");
    let waiting = review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer)
        .expect("waiting review pass");
    assert_eq!(
        reviewer.calls(),
        0,
        "no reviewer dispatched while the blocking prerequisite is open"
    );
    assert!(
        waiting.review_waiting.iter().any(|e| e == &epic),
        "the eligible epic waits on its prerequisite: {waiting:?}"
    );
    assert!(!waiting.approved.contains(&epic), "{waiting:?}");
    assert!(
        !prod.labels(&epic).iter().any(|l| l == REVIEW_DRIFT_LABEL),
        "a waiting prerequisite is not a drift hold"
    );
    assert_eq!(prod.status(&epic), "open", "waiting leaves the epic open");

    // The modeled operator completes the prerequisite; the very next pass admits
    // exactly one review and closes the epic.
    prod.bd_ok(&["close", &prereq, "--reason", "prerequisite complete"]);

    // --- 5. independent review -> actual epic closure ---
    let review = review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer)
        .expect("review pass");
    assert_eq!(reviewer.calls(), 1, "exactly one reviewer dispatch");
    assert_eq!(review.approved, vec![epic.clone()], "{review:?}");
    assert!(review.stale.is_empty(), "no stale admission: {review:?}");
    assert_eq!(
        prod.show(&epic)["status"].as_str(),
        Some("closed"),
        "the epic is actually closed in bd"
    );
    assert!(prod
        .labels(&epic)
        .iter()
        .any(|l| l == REVIEW_APPROVED_LABEL));

    // The admitted review artifacts are pinned on disk.
    let attempt_dir = reviewer
        .captured_diff()
        .expect("captured diff")
        .parent()
        .expect("attempt dir")
        .to_path_buf();
    for name in ["admission.json", "definition.json", "design.txt", "diff.patch"] {
        assert!(
            attempt_dir.join(name).exists(),
            "admitted review artifact {name} at {}",
            attempt_dir.display()
        );
    }
    assert!(
        approved_snapshot_path(&prod.state_dir, &epic).exists(),
        "the durable approved admission identity persists"
    );

    // --- REQUIRED (SF.11 + SF.10): independent review of the *integrated
    // feature* against the design. The admitted baseline must precede the
    // candidate, and the reviewer's diff must cover the actual first
    // `src/lib.rs` feature change (seed `answer() == 0` -> candidate
    // `answer() == 42`) — not merely nonempty Git metadata. This is the
    // production `record_start_on_land` ordering under test; the test does not
    // hand-set review-start, add a pre-landing commit, or vary sibling content.
    let admission: Value = serde_json::from_str(
        &fs::read_to_string(attempt_dir.join("admission.json")).expect("read admission.json"),
    )
    .expect("parse admission.json");
    let review_base = admission["base_sha"]
        .as_str()
        .expect("admitted base_sha")
        .to_string();
    let review_candidate = admission["candidate_sha"]
        .as_str()
        .expect("admitted candidate_sha")
        .to_string();
    assert_ne!(
        review_base, review_candidate,
        "the admitted review baseline must precede the integrated candidate; production \
         sampled it after the first feature landed, excluding that change from the review"
    );
    // The baseline tree predates the feature; the candidate carries it.
    let base_src = prod.git(&["show", &format!("{review_base}:src/lib.rs")]);
    let candidate_src = prod.git(&["show", &format!("{review_candidate}:src/lib.rs")]);
    assert!(
        !base_src.contains("42"),
        "the review baseline must be pre-feature (`answer() == 0`): {base_src:?}"
    );
    assert!(
        candidate_src.contains("42"),
        "the integrated candidate must carry the feature (`answer() == 42`): {candidate_src:?}"
    );
    // The reviewer's actual diff must cover the file and the feature change.
    let diff = fs::read_to_string(attempt_dir.join("diff.patch")).expect("read diff.patch");
    assert!(
        diff.contains("src/lib.rs"),
        "the reviewer diff must cover the feature file src/lib.rs: {diff:?}"
    );
    assert!(
        diff.contains("+pub fn answer() -> u32 {") && diff.contains("42"),
        "the reviewer diff must include the actual feature change (answer() -> 42): {diff:?}"
    );

    // --- repeat: no duplicate integration and no duplicate review ---
    let head_after = prod.head();
    let count_after = prod.commit_count();
    let repeat_work = prod.work_once();
    assert!(
        repeat_work.status.success(),
        "repeat work: {}",
        stdout_of(&repeat_work)
    );
    assert_eq!(prod.head(), head_after, "no second integration");
    assert_eq!(prod.commit_count(), count_after, "no extra commit on repeat");
    let repeat_review = review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer)
        .expect("repeat review");
    assert_eq!(reviewer.calls(), 1, "no duplicate reviewer dispatch");
    assert!(repeat_review.approved.is_empty(), "{repeat_review:?}");
    assert_eq!(prod.status(&epic), "closed", "the epic stays closed");
}

// ===========================================================================
// 2. Invalid decomposition cannot silently close
// ===========================================================================

#[test]
fn invalid_decomposition_cannot_close_the_epic() {
    let prod = Prod::new("invalid");
    let concierge = ScriptedConcierge::new();
    let intake = run_concierge(&prod.origin, &prod.state_dir, GOAL, &route_available(), &concierge)
        .expect("intake");
    let epic = intake.epics[0].clone();

    let designer = ScriptedDesigner::new(DesignerMode::Invalid);
    let sweep =
        sweep_epic(&prod.origin, &prod.state_dir, &epic, &route_available(), &designer)
            .expect("sweep");

    assert_eq!(sweep.rejected, vec![epic.clone()], "gate rejected: {sweep:?}");
    assert!(sweep.approved.is_empty(), "{sweep:?}");
    assert_eq!(prod.status(&epic), "open", "a rejected decomposition never closes");

    // Every staged child is marked rejected; ONE rework task is filed.
    for child in prod.staged_children(&epic) {
        assert!(
            prod.labels(&child)
                .iter()
                .any(|l| l == agalma_taskqueue::DECOMP_REJECTED_LABEL),
            "child {child} is decomp-rejected"
        );
    }
    let fixes: Vec<Value> = prod
        .children(&epic)
        .into_iter()
        .filter(|c| {
            c["labels"]
                .as_array()
                .map(|l| l.iter().any(|v| v.as_str() == Some(DECOMP_FIX_LABEL)))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(fixes.len(), 1, "exactly one decomp-fix rework task");
    assert_eq!(
        fixes[0]["status"].as_str(),
        Some("open"),
        "the rework task is open for repair"
    );
    // No child is ready to run: none is decomp-approved.
    for child in prod.staged_children(&epic) {
        assert!(
            !prod
                .labels(&child)
                .iter()
                .any(|l| l == DECOMP_APPROVED_LABEL),
            "no child is approved"
        );
    }
}

// ===========================================================================
// 3. A missing human release leaves approval-held children open and the epic open
// ===========================================================================

#[test]
fn missing_human_release_leaves_children_approval_held_and_the_epic_open() {
    let prod = Prod::new("release");
    let concierge = ScriptedConcierge::new();
    let intake = run_concierge(&prod.origin, &prod.state_dir, GOAL, &route_available(), &concierge)
        .expect("intake");
    let epic = intake.epics[0].clone();

    let designer = ScriptedDesigner::new(DesignerMode::Valid);
    let sweep =
        sweep_epic(&prod.origin, &prod.state_dir, &epic, &route_available(), &designer)
            .expect("sweep");
    assert_eq!(sweep.approved, vec![epic.clone()]);

    let children = prod.staged_children(&epic);
    assert_eq!(children.len(), 8);
    // The decomposition is approved, but the human has *not* resolved any gate:
    // each child stays open and approval-held by its own native gate. No sentinel
    // defer is recreated; the unresolved gate is what withholds readiness.
    for child in &children {
        assert_eq!(prod.status(child), "open", "child {child} stays open");
        assert_eq!(
            prod.child_approval_state(child),
            ApprovalState::Unresolved,
            "child {child} is approval-held by exactly one well-formed native gate"
        );
        let labels = prod.labels(child);
        assert!(
            !labels.iter().any(|l| l == "deferred"),
            "no sentinel defer is recreated: {labels:?}"
        );
        assert!(labels.iter().any(|l| l == DECOMP_APPROVED_LABEL));
        // The native gate association exists and is unresolved.
        let gate = prod.native_gate_id(child);
        assert_eq!(prod.status(&gate), "open", "gate {gate} awaits the human");
    }

    let base_head = prod.head();
    let base_count = prod.commit_count();
    prod.protect_all(&children, &["src"], &["tests"]);
    let out = prod.work_once();
    assert!(
        out.status.success(),
        "nothing is ready without release: {}",
        stdout_of(&out)
    );
    for child in &children {
        assert!(
            !prod.has_execution(&format!("exec:{child}:1")),
            "no execution without a human release ({child})"
        );
    }
    assert_eq!(prod.head(), base_head, "the origin never advanced");
    assert_eq!(prod.commit_count(), base_count, "no candidate commit landed");
    assert_eq!(prod.status(&epic), "open", "the epic never silently closes");
}

// ===========================================================================
// 4. A designer timeout after authoring still reconciles its decomposition
// ===========================================================================

#[test]
fn timeout_after_authoring_reconciles_the_committed_decomposition() {
    let prod = Prod::new("timeout");
    let concierge = ScriptedConcierge::new();
    let intake = run_concierge(&prod.origin, &prod.state_dir, GOAL, &route_available(), &concierge)
        .expect("intake");
    let epic = intake.epics[0].clone();

    let designer = ScriptedDesigner::new(DesignerMode::TimeoutAfterAuthoring);
    let sweep =
        sweep_epic(&prod.origin, &prod.state_dir, &epic, &route_available(), &designer)
            .expect("sweep");

    assert_eq!(sweep.timed_out, vec![epic.clone()], "the turn bound was hit: {sweep:?}");
    assert_eq!(
        sweep.approved,
        vec![epic.clone()],
        "the committed decomposition is reconciled in the same pass: {sweep:?}"
    );
    assert_eq!(prod.staged_children(&epic).len(), 8);
    for child in prod.staged_children(&epic) {
        assert!(prod
            .labels(&child)
            .iter()
            .any(|l| l == DECOMP_APPROVED_LABEL));
    }
    assert_eq!(prod.status(&epic), "open", "timeout does not close the epic");
}

// ===========================================================================
// 5. An open blocking prerequisite waits: zero reviewer calls, no drift hold
// ===========================================================================

#[test]
fn open_prerequisite_waits_with_zero_reviewer_calls_and_no_new_drift_hold() {
    let prod = Prod::new("prereq");
    let epic = prod.landed_epic("Blocked epic");

    // A second, still-open epic blocks review of `epic`.
    let out = prod.bd(&[
        "create",
        "Prerequisite epic",
        "-t",
        "epic",
        "-p",
        "2",
        "--description",
        "prerequisite design",
        "--json",
    ]);
    assert!(out.status.success(), "prereq epic: {}", stderr_of(&out));
    let value: Value = serde_json::from_slice(&out.stdout).expect("prereq json");
    let prereq = value["id"].as_str().expect("prereq id").to_string();
    prod.bd_ok(&["dep", "add", &epic, &prereq, "--type", "blocks"]);

    let reviewer = ScriptedReviewer::new("REVIEW: APPROVED\n");
    let report = review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer)
        .expect("review pass");

    assert_eq!(reviewer.calls(), 0, "no reviewer dispatched while waiting");
    assert!(report.epics_seen.is_empty(), "{report:?}");
    assert!(report.approved.is_empty(), "{report:?}");
    assert!(report.review_waiting.iter().any(|e| e == &epic), "{report:?}");
    let labels = prod.labels(&epic);
    assert!(!labels.iter().any(|l| l == REVIEW_DRIFT_LABEL), "no new drift hold: {labels:?}");
    assert!(!labels.iter().any(|l| l == REVIEW_APPROVED_LABEL));
    assert_eq!(
        prod.children(&epic)
            .into_iter()
            .filter(|c| c["labels"]
                .as_array()
                .map(|l| l.iter().any(|v| v.as_str() == Some("drift-fix")))
                .unwrap_or(false))
            .count(),
        0,
        "no drift-fix rework filed"
    );
    assert_eq!(
        dispatch_hold(&BdTaskQueue::new(&prod.origin)).expect("hold"),
        None,
        "a waiting prerequisite is not a global drift hold"
    );
    assert_eq!(prod.status(&epic), "open");
}

// ===========================================================================
// 6. A drift verdict withholds closure
// ===========================================================================

#[test]
fn drift_withholds_epic_closure() {
    let prod = Prod::new("drift");
    let epic = prod.landed_epic("Drifting epic");

    let reviewer = ScriptedReviewer::new("REVIEW: DRIFT the merged diff omits the protected check\n");
    let report = review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer)
        .expect("review pass");

    assert_eq!(reviewer.calls(), 1, "one review happened");
    assert_eq!(report.drift, vec![epic.clone()], "{report:?}");
    assert!(report.approved.is_empty(), "drift never approves: {report:?}");
    assert_eq!(prod.status(&epic), "open", "drift withholds closure");
    assert!(prod.labels(&epic).iter().any(|l| l == REVIEW_DRIFT_LABEL));
    assert!(
        prod.children(&epic)
            .into_iter()
            .any(|c| c["labels"]
                .as_array()
                .map(|l| l.iter().any(|v| v.as_str() == Some("drift-fix")))
                .unwrap_or(false)),
        "a drift-fix rework task is filed"
    );
    assert_eq!(
        dispatch_hold(&BdTaskQueue::new(&prod.origin)).expect("hold"),
        Some(epic),
        "the drift hold blocks ordinary dispatch"
    );
}

// ===========================================================================
// 7. A graph/design edit between dispatch and approval withholds closure
// ===========================================================================

#[test]
fn graph_or_design_edit_withholds_stale_closure() {
    // --- a graph edit (a new open blocking prerequisite) ---
    let prod = Prod::new("graph-edit");
    let epic = prod.landed_epic("Graph edited epic");
    let repo = prod.origin.clone();
    let target = epic.clone();
    let mutation: Box<dyn Fn()> = Box::new(move || {
        let out = Command::new("bd")
            .arg("--db")
            .arg(repo.join(".beads"))
            .args([
                "create",
                "Late prerequisite",
                "-t",
                "epic",
                "-p",
                "2",
                "--description",
                "late",
                "--json",
            ])
            .current_dir(&repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd late");
        let value: Value = serde_json::from_slice(&out.stdout).expect("late epic json");
        let late = value["id"].as_str().expect("late id").to_string();
        let out = Command::new("bd")
            .arg("--db")
            .arg(repo.join(".beads"))
            .args(["dep", "add", &target, &late, "--type", "blocks"])
            .current_dir(&repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd dep");
        assert!(out.status.success(), "late dep: {}", stderr_of(&out));
    });
    let reviewer = ScriptedReviewer::with_mutation("REVIEW: APPROVED\n", mutation);
    let report = review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer)
        .expect("review pass");
    assert_eq!(reviewer.calls(), 1, "one review happened");
    assert!(report.approved.is_empty(), "a graph edit denies closure: {report:?}");
    assert!(report.stale.iter().any(|e| e == &epic), "{report:?}");
    assert_eq!(prod.status(&epic), "open");
    assert!(!prod.labels(&epic).iter().any(|l| l == REVIEW_APPROVED_LABEL));

    // --- a design edit ---
    let prod = Prod::new("design-edit");
    let epic = prod.landed_epic("Design edited epic");
    let repo = prod.origin.clone();
    let target = epic.clone();
    let mutation: Box<dyn Fn()> = Box::new(move || {
        let out = Command::new("bd")
            .arg("--db")
            .arg(repo.join(".beads"))
            .args([
                "update",
                &target,
                "--design",
                "the design changed after the reviewer was dispatched",
            ])
            .current_dir(&repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd design edit");
        assert!(out.status.success(), "design edit: {}", stderr_of(&out));
    });
    let reviewer = ScriptedReviewer::with_mutation("REVIEW: APPROVED\n", mutation);
    let report = review_epics(&prod.origin, &prod.state_dir, &route_available(), &reviewer)
        .expect("review pass");
    assert!(report.approved.is_empty(), "a design edit denies closure: {report:?}");
    assert!(report.stale.iter().any(|e| e == &epic), "{report:?}");
    assert_eq!(prod.status(&epic), "open");
    assert!(!prod.labels(&epic).iter().any(|l| l == REVIEW_APPROVED_LABEL));
}

// ===========================================================================
// 8. Composed invariants over deterministic generated operation sequences
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

/// The composed decomposition-gate verdict the pipeline projects: a child set is
/// accepted only when the CP.1 validator finds no rule violations *and* every
/// staged child carries a well-formed native human approval association.
fn composed_gate_accepts(
    children: &[decomp::ChildTask],
    approvals: &[ApprovalState],
) -> bool {
    let violations = decomp::validate(
        children,
        &decomp::ValidateParams {
            allow_below_min: false,
            min_children: decomp::MIN_CHILDREN,
            min_width: 1,
        },
    );
    violations.is_empty() && approvals.iter().all(|s| s.is_well_formed())
}

/// The independent model of the same verdict, computed from the *generated
/// intent* (never the production validator): a bad count, a missing heading, a
/// missing fenced block, or a child without a well-formed approval association
/// rejects the decomposition.
#[derive(Clone, Copy, Debug)]
struct ChildPlan {
    good_headings: bool,
    has_block: bool,
    approval: ApprovalState,
}

impl ChildPlan {
    fn expected_rejection_rule(&self) -> bool {
        !self.good_headings || !self.has_block || !self.approval.is_well_formed()
    }
}

#[test]
fn pipeline_gate_invariants_hold_over_generated_operation_sequences() {
    // Fixed seed + bounded case budget so any failure is replayable.
    const SEED: u64 = 0x5F10_9001_CAFE_0007;
    const CASES: usize = 240;

    let mut rng = Lcg::new(SEED);
    let mut accepted = 0usize;
    let mut rejected = 0usize;

    for case in 0..CASES {
        // One in three cases is a deliberately valid decomposition, so the
        // generator exercises the accepted branch as well as the rejected one.
        let force_valid = rng.below(3) == 0;
        let n = if force_valid {
            8 + rng.below(5) as usize // 8..=12
        } else {
            1 + rng.below(12) as usize // 1..=12
        };
        let mut children = Vec::with_capacity(n);
        let mut plans = Vec::with_capacity(n);
        let mut blocker_sets: Vec<Vec<String>> = Vec::with_capacity(n);
        let mut gates = Vec::new();

        for i in 0..n {
            // Defect kind: 0 clean, 1 missing heading, 2 missing block,
            // 3 approval missing, 4 approval malformed, 5 foreign gate.
            let kind = if force_valid { 0 } else { rng.below(6) };
            let good_headings = kind != 1;
            let has_block = kind != 2;
            let mut description = child_description(i);
            if !good_headings {
                description = description.replace("# Non-goals\n", "");
            }
            if !has_block {
                // Drop the fenced task block (still a schema-complete heading set).
                description = description.split("```yaml").next().unwrap().to_string();
            }
            let id = format!("child.{i}");
            let gate_id = format!("gate.{i}");
            let (approval, gate) = match kind {
                3 => (ApprovalState::Missing, None),
                4 => (
                    ApprovalState::Malformed,
                    Some(well_formed_gate(&gate_id, "someone-else", false)),
                ),
                5 => (ApprovalState::Missing, Some(foreign_gate(&gate_id))),
                // Clean and the two description defects get a well-formed,
                // unresolved native gate.
                _ => (
                    ApprovalState::Unresolved,
                    Some(well_formed_gate(&gate_id, &id, false)),
                ),
            };
            let blockers: Vec<String> = gate
                .as_ref()
                .map(|g| vec![g.id.clone()])
                .unwrap_or_default();
            blocker_sets.push(blockers);
            if let Some(g) = gate {
                gates.push(g);
            }
            plans.push(ChildPlan {
                good_headings,
                has_block,
                approval,
            });
            children.push(decomp::ChildTask {
                id: id.clone(),
                title: format!("child {i}"),
                description,
                design: Some(CHILD_DESIGN.to_string()),
                labels: vec!["staged".to_string(), "difficulty:low".to_string()],
                depends_on: Vec::new(),
            });
        }

        // Re-derive each child's approval association from its own blocker set
        // and compare with the generated intent.
        for (i, plan) in plans.iter().enumerate() {
            let child_id = format!("child.{i}");
            let observed = approval_state(&child_id, &blocker_sets[i], &gates);
            assert_eq!(
                observed, plan.approval,
                "seed={SEED:#x} case={case} child={i} plan={plan:?}"
            );
        }

        // Composed gate verdict vs the generated intent.
        let approvals: Vec<ApprovalState> = plans.iter().map(|p| p.approval).collect();
        let verdict = composed_gate_accepts(&children, &approvals);
        let expected_reject =
            n < decomp::MIN_CHILDREN || n > decomp::MAX_CHILDREN || plans.iter().any(ChildPlan::expected_rejection_rule);
        assert_eq!(
            verdict,
            !expected_reject,
            "seed={SEED:#x} case={case} n={n} plans={plans:?}"
        );

        // Independent rule-level check for the two heading/block defects.
        let violations = decomp::validate(
            &children,
            &decomp::ValidateParams {
                allow_below_min: false,
                min_children: decomp::MIN_CHILDREN,
                min_width: 1,
            },
        );
        let has_rule = |rule: &str| violations.iter().any(|v| v.rule == rule);
        assert_eq!(
            has_rule(decomp::RULE_SCHEMA_SECTION),
            plans.iter().any(|p| !p.good_headings),
            "schema-section matches the missing-heading intent (case {case})"
        );
        assert_eq!(
            has_rule(decomp::RULE_INTAKE_BLOCK),
            plans.iter().any(|p| !p.has_block),
            "intake-block matches the missing-block intent (case {case})"
        );
        assert_eq!(
            has_rule(decomp::RULE_COUNT_BUDGET),
            n < decomp::MIN_CHILDREN,
            "count-budget matches the count intent (case {case})"
        );

        if verdict {
            accepted += 1;
            // An accepted decomposition is non-empty, validates cleanly, and has
            // a well-formed association for every child.
            assert!(!children.is_empty());
            assert!(violations.is_empty());
            assert!(approvals.iter().all(|s| s.is_well_formed()));
        } else {
            rejected += 1;
        }
    }

    // The generator exercised both branches (guards a degenerate generator).
    assert!(accepted > 0, "seed/budget produced no accepted decomposition");
    assert!(rejected > 0, "seed/budget produced no rejected decomposition");
    eprintln!(
        "sf10 property: seed={SEED:#x} cases={CASES} accepted={accepted} rejected={rejected}"
    );
}

fn well_formed_gate(id: &str, approval_for: &str, resolved: bool) -> ApprovalGate {
    ApprovalGate {
        id: id.to_string(),
        approval_for: Some(approval_for.to_string()),
        labels: vec![
            HUMAN_LABEL.to_string(),
            EXECUTION_APPROVAL_LABEL.to_string(),
        ],
        issue_type: Some("gate".to_string()),
        await_type: Some("human".to_string()),
        resolved,
    }
}

/// A native gate that claims no execution approval (no `execution-approval`
/// label, no `approval_for`): an ordinary prerequisite gate, ignored by the
/// approval association.
fn foreign_gate(id: &str) -> ApprovalGate {
    ApprovalGate {
        id: id.to_string(),
        approval_for: None,
        labels: vec![],
        issue_type: Some("gate".to_string()),
        await_type: Some("human".to_string()),
        resolved: false,
    }
}
