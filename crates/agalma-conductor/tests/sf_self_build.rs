//! SF.9 acceptance: scratch self-build evidence, Flash builder routing,
//! protected acceptance, and recovery identity (`agalma-5fy.9`).
//!
//! Focused command: `cargo test --offline --locked -p agalma-conductor --test
//! sf_self_build`.
//!
//! This is the **epic exit** for the SF tranche. It does not add production
//! features; it composes the SF.1–SF.8 surfaces over the real production path
//! and proves the end-to-end behavioral acceptance:
//!
//! - a repository-mode task receives its **complete frozen specification** and
//!   ships a source feature judged by an **independent protected baseline**
//!   (`scripted_self_build_delivers_full_context_and_ships_protected_feature`);
//! - crash/restart **preserves the frozen inputs** and the candidate **merges
//!   exactly once** (`crash_restart_preserves_frozen_inputs_and_merges_once`);
//! - an **outside-scope** candidate parks and never ships
//!   (`out_of_scope_candidate_parks_and_never_ships`);
//! - a **cancelled** execution never ships and cannot be resumed into one
//!   (`cancelled_task_never_ships`);
//! - a **paid Flash builder selection is refused** (never substituted) when the
//!   credential/live gate is closed, and the live run is explicitly skipped
//!   (`flash_builder_selection_is_refused_when_paid_gate_closed`);
//! - the composed invariants (frozen identity, scope ship predicate,
//!   builder-only route) hold over **deterministic generated operation
//!   sequences** with invalid/boundary inputs
//!   (`self_build_invariants_hold_over_generated_operation_sequences`).
//!
//! Everything here is **deterministic scripted transport** (no network, no `bd`
//! daemon, no paid or live provider inference). Scripted evidence is never a
//! model/vendor performance claim; the separately gated paid Flash run is
//! skipped and recorded in `docs/self-build-evidence.md`.
//!
//! Scratch state lives under `target/test-runs/`; the runner redirects and
//! retains it at the bead's persistent evidence root (see the doc).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agalma_conductor::phases::{effects_log_path, run_root, scope_pin_path};
use agalma_conductor::routing::{self, RoleModelOverride, RouteClass};
use agalma_conductor::scope_guard::{
    dummy_artifact, BaselineFile, ProtectedBaseline, ProtectedEntry, ScopePolicy,
    PROTECTED_BASELINE_FORMAT,
};
use agalma_conductor::task_package::{
    build_package, load_package, persist_package, AuthorizationInput, ContextInput, PackageInputs,
    RoleInput,
};
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, ChangeKind, ContextKind, DirectiveRef, ExecutionId, FileChange,
    Role, Scope, TaskFamily, TaskId,
};
use agalma_taskqueue::TaskDefinition;
use agalma_testkit::ScratchDir;
use serde_json::{json, Value};

/// The frozen task definition the test files into the scratch `bd` store.
const FULL_TASK: &str = include_str!("../../../fixtures/self-build/task.md");
/// The reviewed Flash builder route this epic targets (never fabricated).
const FLASH: &str = "opencode-go/deepseek-v4.1-flash";
/// A reviewed free model used for the deterministic builder selector.
const FREE_A: &str = "opencode/mimo-v2.6-flash-free";
/// A different reviewed free model used for the legacy global selector.
const FREE_B: &str = "opencode/big-pickle";
/// Design/notes the production admission freezes from the issue.
const DESIGN: &str = "# Design\n\nKeep the crate a valid library and add the bounded feature.\n";
const NOTES: &str = "# Notes\n\nFrozen before dispatch; recovery never re-reads bd.\n";
/// Repository instruction/shared-design bytes the production admission freezes.
const AGENTS_BYTES: &str = "# Agents\nRepository working rules (frozen at admission).\n";
const SHARED_DESIGN_BYTES: &str =
    "# Self-mutation contracts revision 1\nFrozen shared-design bytes.\n";
/// The scratch origin seed and the scripted build's deterministic result.
const SEED_SOURCE: &str = "//! Seed source.\npub fn answer() -> u32 { 0 }\n";

/// The immutable protected *behavioral* judge: it independently compiles the
/// candidate library and asserts `answer() == 42` by execution, so a comment
/// containing the expected text cannot spoof it and a differently formatted but
/// semantically correct fix still passes. Reuses the existing `rustc` (no new
/// dependency, no network).
const PROTECTED_SCRIPT: &str = "#!/bin/sh\n\
set -eu\n\
cand=\"$AGALMA_CANDIDATE_DIR\"\n\
work=\"$TMPDIR/sf9-protected-probe\"\n\
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

/// Test scratch retained on disk for review evidence (`ScratchDir::keep`).
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

fn assert_aborted(status: &std::process::ExitStatus) {
    assert!(!status.success(), "process should abort: {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "expected SIGABRT: {status:?}");
    }
}

/// The operator `ProtectedBaseline/v1` JSON the production admission freezes as
/// `scope.oracle`. The single behavioral judge lives outside every candidate
/// writable root once materialized.
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

// ---------------------------------------------------------------------------
// Production harness: a scratch git origin + isolated bd store
// ---------------------------------------------------------------------------

struct Prod {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Prod {
    fn new(name: &str) -> Prod {
        require_bd();
        let dir = scratch(&format!("sf-self-build-{name}"));
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

        let prefix = format!("sf9{}", name.replace('-', ""));
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

    /// File the frozen fixture as a repository task with full context.
    fn create_task(&self, title: &str) -> String {
        let stdout = self.bd_ok(&[
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            FULL_TASK,
            "--json",
        ]);
        let value: Value = serde_json::from_str(&stdout).expect("bd create json");
        let id = value["id"].as_str().expect("created id").to_string();
        self.bd_ok(&["update", &id, "--design", DESIGN, "--notes", NOTES]);
        id
    }

    /// Write/merge the operator `ProtectedInputs/v1` entry for `task_id` (SF.6).
    fn protect(&self, task_id: &str, allowed: &[&str], protected: &[&str]) {
        let root = fs::canonicalize(&self.origin).expect("canonical origin");
        let entry = json!({
            "task_id": task_id,
            "repository_root": root.to_string_lossy(),
            "base_sha": self.head(),
            "allowed_paths": allowed,
            "protected_paths": protected,
            "baseline": protected_baseline_json(),
        });
        let path = self.state_dir.join("protected-inputs.json");
        let mut entries: Vec<Value> = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("entries").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        entries.retain(|e| e.get("task_id").and_then(Value::as_str) != Some(task_id));
        entries.push(entry);
        fs::create_dir_all(&self.state_dir).expect("state dir");
        let doc = json!({ "format": "ProtectedInputs/v1", "entries": entries });
        fs::write(&path, serde_json::to_vec_pretty(&doc).expect("json")).expect("write manifest");
    }

    /// Run `agalma work --once` against the scratch origin in scripted mode.
    ///
    /// Route-affecting env vars are cleared first so the runner's environment
    /// cannot leak into a route decision; `flags`/`extra` then set the intended
    /// ones.
    fn work(&self, flags: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut command = Command::new(bin());
        command.args([
            "work",
            "--repo",
            self.origin.to_str().expect("origin utf8"),
            "--state-dir",
            self.state_dir.to_str().expect("state utf8"),
            "--once",
        ]);
        command.args(flags);
        command
            .env("AGALMA_ACTIVITY_MODE", "scripted")
            .env("AGALMA_LEDGER_TRANSPORT", "in-process")
            .env("AGALMA_WORKSPACE_TRANSPORT", "in-process")
            .env("AGALMA_TASKQUEUE_TRANSPORT", "in-process")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("AGALMA_ALLOW_PAID")
            .env_remove("AGALMA_MODEL")
            .env_remove("AGALMA_BUILDER_MODEL");
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().expect("run agalma work")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.origin)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Number of commits reachable from the origin `main`.
    fn commit_count(&self) -> usize {
        self.git(&["rev-list", "--count", "HEAD"])
            .parse()
            .expect("commit count")
    }

    /// The SHA a tag points at, if the tag exists.
    fn tag_sha(&self, tag: &str) -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", &format!("refs/tags/{tag}")])
            .current_dir(&self.origin)
            .output()
            .expect("spawn git");
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            None
        }
    }

    fn ledger(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger")
    }

    /// `(phase, state)` from the durable execution row.
    fn execution_phase(&self, execution: &str) -> (String, String) {
        self.ledger()
            .query_row(
                "SELECT phase, state FROM executions WHERE execution_id=?1",
                [execution],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("execution row")
    }

    /// Whether the execution row exists at all.
    fn has_execution(&self, execution: &str) -> bool {
        self.ledger()
            .query_row("SELECT 1 FROM executions WHERE execution_id=?1", [execution], |_| {
                Ok(())
            })
            .is_ok()
    }

    /// The `task.package_admitted` payload for `execution`, if recorded.
    fn admitted(&self, execution: &str) -> Option<Value> {
        let payload: Option<String> = self
            .ledger()
            .query_row(
                "SELECT payload FROM execution_events \
                 WHERE execution_id=?1 AND kind='task.package_admitted' \
                 ORDER BY rowid DESC LIMIT 1",
                [execution],
                |row| row.get(0),
            )
            .ok();
        payload.map(|text| serde_json::from_str(&text).expect("admission payload json"))
    }

    /// The frozen `TaskPackage/v1` by digest.
    fn frozen(&self, digest: &str) -> agalma_contracts::TaskPackage {
        load_package(&self.state_dir, digest).expect("load frozen package")
    }

    /// The durable `m1/<exec>` scope pin written by a green verify.
    fn scope_pin(&self, execution: &str) -> Value {
        let path = scope_pin_path(&self.state_dir, &ExecutionId::new(execution));
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_slice(&bytes).expect("scope pin json")
    }

    /// The rendered per-attempt builder prompt artifact.
    fn prompt_text(&self, execution: &str, attempt: u32) -> String {
        let path = run_root(&self.state_dir, &ExecutionId::new(execution))
            .join("prompts")
            .join(format!("builder@{attempt}.md"));
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// The captured acceptance output artifact for `attempt`, command `index`.
    fn accept_log(&self, execution: &str, attempt: u32, index: usize) -> String {
        let path = run_root(&self.state_dir, &ExecutionId::new(execution))
            .join("artifacts")
            .join(format!("accept@{attempt}-{index}.txt"));
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Whether the acceptance failure artifact exists for `attempt`.
    fn failure_artifact(&self, execution: &str, attempt: u32) -> Option<String> {
        let path = run_root(&self.state_dir, &ExecutionId::new(execution))
            .join("artifacts")
            .join(format!("failure@{attempt}.txt"));
        fs::read_to_string(path).ok()
    }

    /// The captured acceptance output artifact, or an empty string if absent.
    fn accept_log_or_empty(&self, execution: &str, attempt: u32, index: usize) -> String {
        let path = run_root(&self.state_dir, &ExecutionId::new(execution))
            .join("artifacts")
            .join(format!("accept@{attempt}-{index}.txt"));
        fs::read_to_string(path).unwrap_or_default()
    }

    /// The scripted-mode effect ledger lines recorded for `execution`.
    fn effects(&self, execution: &str) -> Vec<String> {
        let path = effects_log_path(&self.state_dir, &ExecutionId::new(execution));
        fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Completed-operation receipts recorded for `execution`.
    fn receipt_count(&self, execution: &str) -> usize {
        self.ledger()
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE execution_id=?1",
                [execution],
                |row| row.get::<_, i64>(0),
            )
            .expect("receipt count") as usize
    }

    /// Unconsumed dispatch intents for `execution`.
    fn pending_intents(&self, execution: &str) -> Vec<String> {
        let conn = self.ledger();
        let mut stmt = conn
            .prepare("SELECT operation_id FROM dispatch_intents WHERE execution_id=?1 AND consumed=0")
            .expect("prepare intents");
        let rows = stmt
            .query_map([execution], |row| row.get::<_, String>(0))
            .expect("query intents");
        rows.map(|row| row.expect("intent row")).collect()
    }

    /// The backlog status of a task id.
    fn issue_status(&self, id: &str) -> String {
        let id = id.to_string();
        let stdout = self.bd_ok(&["show", &id, "--json"]);
        let value: Value = serde_json::from_str(&stdout).expect("bd show json");
        let row = if value.is_array() {
            value.as_array().and_then(|rows| rows.first()).cloned().unwrap_or(Value::Null)
        } else {
            value
        };
        row["status"].as_str().unwrap_or("unknown").to_string()
    }

    /// The dir-relative candidate acceptance command + index of the protected
    /// judge command in the frozen acceptance list.
    fn scope_pin_candidate_sha(&self, execution: &str) -> String {
        self.scope_pin(execution)["candidate_sha"]
            .as_str()
            .expect("candidate_sha")
            .to_string()
    }

    fn run_cancel(&self, execution: &str) -> Output {
        Command::new(bin())
            .args([
                "cancel",
                "--state-dir",
                self.state_dir.to_str().expect("state utf8"),
                "--execution",
                execution,
                "--confirm",
                "cancel",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run agalma cancel")
    }

    fn run_resume(&self) -> Output {
        Command::new(bin())
            .args([
                "resume",
                "--state-dir",
                self.state_dir.to_str().expect("state utf8"),
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run agalma resume")
    }

    fn run_pause(&self) -> Output {
        Command::new(bin())
            .args([
                "pause",
                "--state-dir",
                self.state_dir.to_str().expect("state utf8"),
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run agalma pause")
    }
}

fn role_model(package: &agalma_contracts::TaskPackage, role: Role) -> String {
    package
        .roles
        .iter()
        .find(|pin| pin.role == role)
        .unwrap_or_else(|| panic!("frozen package has no {role:?} role pin"))
        .model
        .clone()
}

// ===========================================================================
// 1. Full frozen context + independently judged source feature ships
// ===========================================================================

/// The production path freezes the complete specification, renders it into the
/// builder prompt, and ships the feature only after the *independent* protected
/// baseline judges the candidate's behavior; the candidate and integration
/// identities are the same commit and integration happens exactly once.
#[test]
fn scripted_self_build_delivers_full_context_and_ships_protected_feature() {
    let prod = Prod::new("spec");
    let id = prod.create_task("Sf9 frozen spec feature");
    prod.protect(&id, &["src"], &["tests"]);

    let base = prod.head();
    let base_count = prod.commit_count();
    let out = prod.work(&["--builder-model", FREE_A, "--model", FREE_B], &[]);
    assert!(
        out.status.success(),
        "green scripted self-build: {}\n{}",
        stdout_of(&out),
        stderr_of(&out)
    );

    let execution = format!("exec:{id}:1");
    assert_eq!(
        prod.execution_phase(&execution),
        ("done".to_string(), "completed".to_string()),
        "the execution completes"
    );

    // The frozen package carries the complete specification and immutable rules.
    let event = prod.admitted(&execution).expect("admission recorded");
    let digest = event["package_digest"].as_str().expect("digest").to_string();
    let frozen = prod.frozen(&digest);
    let desc = &frozen.definition.description.text;
    for section in [
        "# Scope",
        "# Files",
        "# Contract",
        "# Invariants",
        "# Non-goals",
    ] {
        assert!(
            desc.contains(section),
            "frozen description is missing `{section}`:\n{desc}"
        );
    }
    assert_eq!(frozen.definition.design.text, DESIGN, "design frozen verbatim");
    assert_eq!(frozen.definition.notes.text, NOTES, "notes frozen verbatim");
    assert!(
        frozen.definition.machine_block.contains("family: feature"),
        "the frozen directive block is present"
    );
    assert_eq!(
        frozen.definition.acceptance,
        vec!["sh -c 'grep -q 42 src/lib.rs'".to_string()],
        "the frozen acceptance command is recorded"
    );
    assert!(frozen
        .context
        .iter()
        .any(|item| item.kind == ContextKind::Instruction && item.source == "AGENTS.md"));
    assert!(frozen.context.iter().any(|item| {
        item.kind == ContextKind::SharedDesign && item.source == "docs/self-mutation-contracts.md"
    }));
    assert_eq!(frozen.scope.allowed_paths, vec!["src".to_string()]);
    assert_eq!(frozen.scope.protected_paths, vec!["tests".to_string()]);

    // Builder-only routing: the scoped selector pins the builder; the verifier
    // keeps the independently resolved legacy global route.
    assert_eq!(role_model(&frozen, Role::Builder), FREE_A);
    assert_eq!(role_model(&frozen, Role::Verifier), FREE_B);

    // The rendered builder prompt delivers the frozen sections and the immutable
    // repository rules — not a title-only one-line instruction.
    let prompt = prod.prompt_text(&execution, 1);
    for needle in [
        "# Frozen task context",
        "# Scope",
        "# Contract",
        "# Invariants",
        "# Non-goals",
        "Repository rules and instructions (immutable)",
        "sh -c 'grep -q 42 src/lib.rs'",
        "docs/self-mutation-contracts.md",
    ] {
        assert!(
            prompt.contains(needle),
            "rendered builder prompt is missing `{needle}`:\n{prompt}"
        );
    }

    // Both acceptance commands ran: index 0 is the frozen task acceptance, index
    // 1 is the operator protected baseline (the independent judge).
    let candidate_cmd = prod.accept_log(&execution, 1, 0);
    assert!(
        candidate_cmd.contains("exit: 0") && candidate_cmd.contains("grep -q 42"),
        "frozen task acceptance passed: {candidate_cmd}"
    );
    let protected_cmd = prod.accept_log(&execution, 1, 1);
    assert!(
        protected_cmd.contains("exit: 0") && protected_cmd.contains("AGALMA_PROTECTED_DIR"),
        "protected behavioral judge passed: {protected_cmd}"
    );

    // The independent judge is materialized outside every candidate writable
    // root, at the parent-owned protected root.
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

    // Exact candidate/integration identity: the verified candidate SHA is the
    // integrated origin HEAD, recorded as a distinct commit, tagged once.
    let pin = prod.scope_pin(&execution);
    let candidate_sha = prod.scope_pin_candidate_sha(&execution);
    assert!(
        pin["oracle_digest"].as_str().is_some_and(|s| !s.is_empty()),
        "the pin records the oracle digest: {pin}"
    );
    assert_eq!(pin["package_digest"].as_str(), Some(digest.as_str()));
    assert_ne!(candidate_sha, base, "the candidate advanced from base");
    assert_eq!(
        prod.head(),
        candidate_sha,
        "origin main is exactly the verified candidate"
    );
    assert_eq!(prod.commit_count(), base_count + 1, "seed + one candidate commit");
    let tag = format!(
        "m1/{}",
        agalma_workspace::execution_dir_name(&ExecutionId::new(&execution))
    );
    assert_eq!(
        prod.tag_sha(&tag).as_deref(),
        Some(candidate_sha.as_str()),
        "the integration tag pins the candidate"
    );
}

// ===========================================================================
// 2. Crash/restart preserves frozen inputs; merge happens exactly once
// ===========================================================================

/// A crash after admission preserves the frozen package; recovery reuses it
/// (never re-resolving from a changed environment) and completes on its pins.
/// Re-running the conductor cannot merge a second time.
#[test]
fn crash_restart_preserves_frozen_inputs_and_merges_once() {
    let prod = Prod::new("restart");
    let id = prod.create_task("Sf9 restart");
    prod.protect(&id, &["src"], &["tests"]);
    let base = prod.head();
    let base_count = prod.commit_count();

    // Admit with the scoped selector, then crash before any phase handler runs.
    let crash = prod.work(
        &["--builder-model", FREE_A],
        &[("AGALMA_PACKAGE_CRASH_AT", "after_admit")],
    );
    assert_aborted(&crash.status);
    assert_eq!(prod.head(), base, "the crash did not advance the origin");

    let execution = format!("exec:{id}:1");
    let admitted = prod.admitted(&execution).expect("admission committed");
    let digest = admitted["package_digest"].as_str().expect("digest").to_string();

    // The environment changes before recovery: different builder + global
    // selectors. The frozen pins must win.
    let recovery = prod.work(
        &["--builder-model", FREE_B],
        &[("AGALMA_BUILDER_MODEL", FREE_B), ("AGALMA_MODEL", "custom/other")],
    );
    assert!(
        recovery.status.success(),
        "recovery run: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    // Frozen inputs preserved: the same package digest resolves with the same
    // content and the same builder pin.
    let frozen = prod.frozen(&digest);
    assert_eq!(
        role_model(&frozen, Role::Builder),
        FREE_A,
        "recovery preserves the frozen builder pin despite the changed environment"
    );
    assert_eq!(frozen.definition.description.text, FULL_TASK);
    assert_eq!(
        prod.execution_phase(&execution),
        ("done".to_string(), "completed".to_string())
    );

    let candidate_sha = prod.scope_pin_candidate_sha(&execution);
    assert_eq!(prod.head(), candidate_sha, "one integration landed");
    assert_eq!(
        prod.commit_count(),
        base_count + 1,
        "seed + exactly one candidate commit"
    );

    // A second conductor invocation finds no ready task and cannot re-integrate.
    let again = prod.work(&[], &[]);
    assert!(
        again.status.success(),
        "second run: {}\n{}",
        stdout_of(&again),
        stderr_of(&again)
    );
    assert_eq!(prod.head(), candidate_sha, "origin main did not move twice");
    assert_eq!(prod.commit_count(), base_count + 1, "no second merge");
}

// ===========================================================================
// 3. Outside-scope candidate does not ship
// ===========================================================================

/// A candidate that changes a path outside the frozen allowed scope is refused
/// by the protected guard before integration: it retries to a park and the
/// origin is never advanced.
#[test]
fn out_of_scope_candidate_parks_and_never_ships() {
    let prod = Prod::new("scope");
    let id = prod.create_task("Sf9 outside scope");
    // The scripted build writes `src/lib.rs`; only `docs` is allowed here, so
    // the candidate's change is outside scope and must not ship.
    prod.protect(&id, &["docs"], &["tests"]);
    let base = prod.head();
    let base_count = prod.commit_count();

    let out = prod.work(&[], &[]);
    assert!(
        !out.status.success(),
        "an out-of-scope candidate must not report success: {}",
        stdout_of(&out)
    );

    let execution = format!("exec:{id}:1");
    let (phase, _state) = prod.execution_phase(&execution);
    assert_eq!(phase, "parked", "the execution parks after the denials");
    assert_eq!(
        prod.head(),
        base,
        "origin main is unchanged: the candidate never shipped"
    );
    assert_eq!(prod.commit_count(), base_count, "no candidate commit landed");

    let failure = prod
        .failure_artifact(&execution, 1)
        .expect("a scope failure artifact is recorded");
    assert!(
        failure.contains("src/lib.rs") && failure.contains("not_allowed"),
        "the denial names the outside-scope path: {failure}"
    );
    assert!(
        !prod
            .accept_log_or_empty(&execution, 1, 1)
            .contains("AGALMA_PROTECTED_DIR"),
        "the protected judge never ran for a denied candidate"
    );
}

// ===========================================================================
// 4. Cancelled execution does not ship
// ===========================================================================

/// A cancelled in-flight execution never ships: after cancel + resume + recovery
/// it settles as parked/failed with no effects, no receipts, and an unchanged
/// origin; the backlog issue is blocked.
#[test]
fn cancelled_task_never_ships() {
    let prod = Prod::new("cancel");
    let id = prod.create_task("Sf9 cancel");
    prod.protect(&id, &["src"], &["tests"]);
    let base = prod.head();
    let base_count = prod.commit_count();

    let crash = prod.work(&[], &[("AGALMA_PACKAGE_CRASH_AT", "after_admit")]);
    assert_aborted(&crash.status);
    let execution = format!("exec:{id}:1");
    assert!(prod.admitted(&execution).is_some(), "admission committed");

    // Pause, cancel the execution, resume the factory. Cancellation is durable
    // and distinct from the global pause latch.
    assert!(prod.run_pause().status.success());
    let cancel = prod.run_cancel(&execution);
    assert!(
        cancel.status.success(),
        "cancel applied: {}\n{}",
        stdout_of(&cancel),
        stderr_of(&cancel)
    );
    assert!(prod.run_resume().status.success());

    let recovery = prod.work(&[], &[]);
    assert!(
        recovery.status.success(),
        "recovery run: {}\n{}",
        stdout_of(&recovery),
        stderr_of(&recovery)
    );

    let (phase, state) = prod.execution_phase(&execution);
    assert_eq!((phase.as_str(), state.as_str()), ("parked", "failed"));
    assert!(prod.effects(&execution).is_empty(), "no phase effect shipped");
    assert_eq!(prod.receipt_count(&execution), 0, "no completed operation");
    assert!(prod.pending_intents(&execution).is_empty(), "no outstanding intent");
    assert_eq!(prod.head(), base, "origin main is unchanged: cancelled never ships");
    assert_eq!(prod.commit_count(), base_count, "no candidate commit landed");
    assert_eq!(prod.issue_status(&id), "blocked", "the issue is blocked, not shipped");
}

// ===========================================================================
// 5. Paid Flash builder selection refused; live evidence explicitly skipped
// ===========================================================================

/// The reviewer's intended builder target is the paid Flash route. With the
/// credential/live gate closed, an explicit Flash builder selection is refused
/// *before* any claim — never silently substituted for a free model — and the
/// live paid run is skipped rather than faked.
#[test]
fn flash_builder_selection_is_refused_when_paid_gate_closed() {
    let prod = Prod::new("flash");
    let id = prod.create_task("Sf9 flash"); // task exists but must never be claimed
    prod.protect(&id, &["src"], &["tests"]);

    let out = prod.work(&["--builder-model", FLASH], &[]);
    assert!(
        !out.status.success(),
        "an ineligible paid builder selection is refused: {}",
        stdout_of(&out)
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("builder model selector refused"),
        "refusal is explicit: {stderr}"
    );
    assert!(stderr.contains(FLASH), "refusal names the model: {stderr}");
    assert!(
        stderr.contains("credential") || stderr.contains("AGALMA_ALLOW_PAID"),
        "refusal names the missing gate: {stderr}"
    );
    assert!(
        !prod.has_execution(&format!("exec:{id}:1")),
        "no execution is created for the refused paid selection"
    );

    // The routing surface itself: Flash is selected for the builder *only* when
    // the paid gate is actually open, and is refused (not substituted) when it
    // is closed.
    let routed = routing::resolve_builder_route(
        &prod.state_dir,
        "sf9.flash",
        TaskFamily::Feature,
        true,
        true,
        RoleModelOverride::new(Some(FLASH), None),
    )
    .expect("eligible paid builder selection");
    assert_eq!(routed.class, RouteClass::Builder);
    assert_eq!(routed.model, FLASH);
    assert!(routed.is_paid());

    let denied = routing::resolve_builder_route(
        &prod.state_dir,
        "sf9.flash.closed",
        TaskFamily::Feature,
        false,
        false,
        RoleModelOverride::new(Some(FLASH), None),
    );
    assert!(
        denied.is_err(),
        "a paid builder selection without credential/opt-in is refused"
    );
    assert!(
        !prod.has_execution(&format!("exec:{id}:1")),
        "the refused selection still created no execution"
    );

    // The live paid Flash run is deliberately not executed here: the live gate
    // and credential are closed. See `docs/self-build-evidence.md`.
    eprintln!(
        "sf9: live paid Flash self-build SKIPPED (credential/live gate closed); \
         scripted transport evidence only, not a Flash inference win"
    );
}

// ===========================================================================
// 6. Composed invariants over deterministic generated operation sequences
// ===========================================================================

/// A deterministic linear-congruential generator with a fixed seed and case
/// budget (recorded below) driving generated operation sequences over the
/// composed SF.9 surfaces. It exercises:
///
/// - **frozen identity**: build → persist → load preserves the package digest
///   and content (restart preserves inputs);
/// - **scope ship predicate**: a change is authorized only when every changed
///   path is inside the allowed scope and outside the protected set — including
///   invalid/boundary paths (empty, `.`, `..`, absolute, trailing whitespace,
///   nested `.git`, rename/delete sources). Malformed/unsafe destinations and
///   source-side rename paths are denied under a deliberately permissive
///   allowlist (so the denial is the path itself, not an allowlist gap), while
///   an inside→inside rename and the significant-trailing-whitespace path stay
///   authorized;
/// - **builder-only route**: the builder model is the scoped override, else the
///   global override; the verifier is independent of the builder selector; a
///   paid builder selection without the gate is refused, never substituted.
///
/// Seed `0x5F9_B007`, budget 48 policy cases × up to 6 operations plus 48
/// generated boundary checks (each malformed class and each rename-source class
/// is additionally exercised deterministically). The generation is pure and
/// deterministic, so any failing case is replayable from the seed.
#[test]
fn self_build_invariants_hold_over_generated_operation_sequences() {
    const SEED: u64 = 0x5F9_B007;
    const CASES: usize = 48;

    let dir = scratch("sf-self-build-prop");
    let mut rng = Lcg::new(SEED);

    // The path alphabet: one path that is inside an allowed entry, one outside,
    // one protected, and a set of invalid/boundary paths.
    let inside = "src/lib.rs";
    let outside = "vendor/readme.md";
    let protected_path = "tests/protected_check.sh";
    let boundary = [
        "",
        ".",
        "..",
        "src/../docs/x",
        "/abs/path",
        "src/lib.rs ",   // trailing whitespace is significant
        "src/.git/config",
    ];

    let mut cases_with_denial = 0usize;
    let mut cases_allowed = 0usize;

    for case in 0..CASES {
        // Generate the policy and a mixed change set.
        let mut allowed: Vec<String> = vec!["src".to_string()];
        let mut protected: Vec<String> = vec!["tests".to_string()];
        let mut changes: Vec<FileChange> = Vec::new();

        let ops = 1 + rng.below(6) as usize;
        for _ in 0..ops {
            match rng.below(5) {
                0 => allowed.push(pick(&mut rng, &["docs", "examples", "src/gen"]).to_string()),
                1 => protected.push(pick(&mut rng, &["tests", "fixtures"]).to_string()),
                2 => changes.push(FileChange {
                    kind: ChangeKind::Modified,
                    path: pick(&mut rng, &["src/lib.rs", "vendor/readme.md", "tests/protected_check.sh"])
                        .to_string(),
                    old_path: None,
                    old_mode: None,
                    new_mode: None,
                    symlink_target: None,
                }),
                3 => {
                    let path = boundary[rng.below(boundary.len() as u64) as usize].to_string();
                    changes.push(FileChange {
                        kind: ChangeKind::Added,
                        path,
                        old_path: None,
                        old_mode: None,
                        new_mode: None,
                        symlink_target: None,
                    });
                }
                _ => {
                    // A rename whose source side is the protected/outside path, to
                    // prove both sides are checked.
                    changes.push(FileChange {
                        kind: ChangeKind::Renamed,
                        path: "src/moved.rs".to_string(),
                        old_path: Some(
                            pick(&mut rng, &["tests/protected_check.sh", "vendor/readme.md"])
                                .to_string(),
                        ),
                        old_mode: None,
                        new_mode: None,
                        symlink_target: None,
                    });
                }
            }
        }

        let allowed = dedup(allowed);
        let protected = dedup(protected);

        // --- scope ship predicate (independent monotonic invariants) ---
        let scope = Scope {
            allowed_paths: allowed.clone(),
            protected_paths: protected.clone(),
            readable_artifacts: Vec::new(),
            oracle: dummy_artifact(),
        };
        let policy = match ScopePolicy::from_scope(&scope) {
            Ok(policy) => policy,
            Err(err) => {
                // Invalid generated policy entries must be refused, not dropped.
                assert!(!err.entry.is_empty());
                continue;
            }
        };
        let report = policy.authorize(&changes);

        // An entirely inside-scope change set must be authorized.
        let all_inside = changes.iter().all(|c| {
            c.path == inside
                && c.old_path.is_none()
                && !protected.iter().any(|p| under(&c.path, p))
        });
        if all_inside && !changes.is_empty() {
            assert!(
                !report.is_denied(),
                "in-scope change set must be authorized (case {case})"
            );
            assert_eq!(report.authorized, changes.len());
            cases_allowed += 1;
        }
        // Any change touching a protected path or an outside path must deny.
        let must_deny = changes.iter().any(|c| {
            under(&c.path, protected_path)
                || (c.old_path.as_deref().is_some_and(|p| under(p, protected_path)))
                || c.path == outside
                || c.old_path.as_deref() == Some(outside)
        });
        if must_deny {
            assert!(
                policy.authorize(&changes).is_denied(),
                "protected/outside change must deny (case {case}): {changes:?}"
            );
            cases_with_denial += 1;
        }
        // Deny-wins monotonicity: adding a denied change never authorizes.
        let mut extended = changes.clone();
        extended.push(FileChange {
            kind: ChangeKind::Added,
            path: protected_path.to_string(),
            old_path: None,
            old_mode: None,
            new_mode: None,
            symlink_target: None,
        });
        assert!(
            policy.authorize(&extended).is_denied(),
            "adding a protected-path change must deny (case {case})"
        );

        // --- builder-only route (independent of the builder/verifier split) ---
        let builder_override = [None, Some(FREE_A), Some(FREE_B), Some(FLASH)][rng.below(4) as usize];
        let global_override = [None, Some(FREE_A), Some(FREE_B)][rng.below(3) as usize];
        let overrides = RoleModelOverride::new(builder_override, global_override);
        let effective_builder = builder_override.or(global_override);
        let paid_closed = !matches!(effective_builder, Some(FLASH));

        let routed = routing::resolve_builder_route(
            &dir,
            &format!("sf9.prop.{case}"),
            TaskFamily::Feature,
            false,
            false,
            overrides,
        );
        if let Some(model) = effective_builder {
            if paid_closed {
                let selection = routed.expect("eligible free builder route");
                assert_eq!(selection.model, model, "builder override wins (case {case})");
                assert_eq!(selection.class, RouteClass::Builder);
            } else {
                assert!(
                    routed.is_err(),
                    "paid builder selection without the gate is refused (case {case})"
                );
            }
        } else {
            let selection = routed.expect("static builder route");
            assert_eq!(selection.model, FREE_B, "static free-first builder route");
        }
        let verifier = routing::resolve_route(
            &dir,
            &format!("sf9.prop.v.{case}"),
            "verifier",
            TaskFamily::Feature,
            false,
            false,
            global_override,
        )
        .expect("verifier route");
        assert_eq!(
            verifier.model,
            global_override.unwrap_or(FREE_B),
            "the verifier ignores the builder selector (case {case})"
        );

        // --- frozen identity: build → persist → load is stable ---
        let pkg = build_persisted(
            &dir,
            &format!("sf9.prop.{case}"),
            &allowed,
            &protected,
        );
        let reloaded = load_package(&dir, &pkg.package_digest).expect("reload frozen package");
        reloaded.verify().expect("frozen package integrity");
        assert_eq!(
            reloaded.package_digest, pkg.package_digest,
            "frozen identity is stable across restart (case {case})"
        );
        assert_eq!(reloaded.scope.allowed_paths, allowed);
        assert_eq!(reloaded.scope.protected_paths, protected);
    }

    // --- independent malformed/unsafe path and source-side rename assertions ---
    // A maximally permissive allowlist, so any denial below is caused by the
    // malformed/unsafe path itself (or the rename source side), not by an
    // unrelated allowlist gap. The generator then exercises each class within a
    // fixed budget, deterministically from the same seed stream.
    const BOUNDARY_CASES: usize = 48;
    let permissive = Scope {
        allowed_paths: vec!["src".to_string(), "docs".to_string(), "vendor".to_string()],
        protected_paths: vec!["tests".to_string()],
        readable_artifacts: Vec::new(),
        oracle: dummy_artifact(),
    };
    let permissive_policy = ScopePolicy::from_scope(&permissive).expect("permissive policy");
    // Malformed or unsafe destination paths: invalid/`.`/`..`/absolute/NUL forms
    // and a nested `.git` tree. `src/lib.rs ` (trailing whitespace) is a *valid*
    // path (whitespace is significant) and is asserted separately as authorized.
    let malformed = ["", ".", "..", "src/../docs/x", "/abs/path", "src/.git/config", "\0"];
    // Source-side rename classes: a protected source and an outside-scope source.
    let rename_sources = ["tests/protected_check.sh", "other/readme.md"];

    let mut boundary_checks = 0usize;
    let deny_added = |path: &str, checks: &mut usize| {
        let change = FileChange {
            kind: ChangeKind::Added,
            path: path.to_string(),
            old_path: None,
            old_mode: None,
            new_mode: None,
            symlink_target: None,
        };
        let report = permissive_policy.authorize(&[change]);
        assert!(
            report.is_denied(),
            "malformed/unsafe path must be denied even under a permissive allowlist: {path:?}"
        );
        assert_eq!(
            report.authorized, 0,
            "a malformed/unsafe change authorizes nothing: {path:?}"
        );
        *checks += 1;
    };
    let deny_rename = |source: &str, checks: &mut usize| {
        let change = FileChange {
            kind: ChangeKind::Renamed,
            path: "src/moved.rs".to_string(),
            old_path: Some(source.to_string()),
            old_mode: None,
            new_mode: None,
            symlink_target: None,
        };
        assert!(
            permissive_policy.authorize(&[change]).is_denied(),
            "a rename source side must be checked: {source:?}"
        );
        *checks += 1;
    };
    // Deterministic: every class is exercised at least once.
    for &path in &malformed {
        deny_added(path, &mut boundary_checks);
    }
    for &source in &rename_sources {
        deny_rename(source, &mut boundary_checks);
    }
    // Generated within the fixed budget (same deterministic seed stream).
    for _ in 0..BOUNDARY_CASES {
        if rng.below(2) == 0 {
            deny_added(pick(&mut rng, &malformed), &mut boundary_checks);
        } else {
            deny_rename(pick(&mut rng, &rename_sources), &mut boundary_checks);
        }
    }
    assert_eq!(
        boundary_checks,
        malformed.len() + rename_sources.len() + BOUNDARY_CASES,
        "boundary budget fully exercised"
    );
    // Controls proving the denial is the source/path, not a blanket rename or
    // whitespace rule: an inside→inside rename is authorized, and the
    // significant-trailing-whitespace path stays a valid in-scope path.
    let inside_rename = FileChange {
        kind: ChangeKind::Renamed,
        path: "src/moved.rs".to_string(),
        old_path: Some("src/lib.rs".to_string()),
        old_mode: None,
        new_mode: None,
        symlink_target: None,
    };
    assert!(
        !permissive_policy.authorize(&[inside_rename]).is_denied(),
        "an inside→inside rename is authorized"
    );
    let whitespace = FileChange {
        kind: ChangeKind::Added,
        path: "src/lib.rs ".to_string(),
        old_path: None,
        old_mode: None,
        new_mode: None,
        symlink_target: None,
    };
    assert!(
        !permissive_policy.authorize(&[whitespace]).is_denied(),
        "a trailing-whitespace path stays a valid in-scope path"
    );

    // The generator exercised both branches (guards a degenerate generator).
    assert!(
        cases_allowed > 0,
        "generator produced no in-scope case; seed/budget degenerate"
    );
    assert!(
        cases_with_denial > 0,
        "generator produced no denial case; seed/budget degenerate"
    );
    eprintln!(
        "sf9 property: seed={SEED:#x} cases={CASES} allowed={cases_allowed} \
         denial-cases={cases_with_denial} boundary-checks={boundary_checks} \
         (budget {BOUNDARY_CASES})"
    );
}

/// A minimal deterministic linear-congruential generator (no external crate).
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed | 1)
    }

    /// A `u64` in `0..bound` (bound > 0).
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % bound
    }
}

/// Pick one element from `options` (`options` must be non-empty).
fn pick<'a>(rng: &mut Lcg, options: &[&'a str]) -> &'a str {
    options[rng.below(options.len() as u64) as usize]
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

/// `path == entry || path == "{entry}/..."` (mirrors the scope entry match).
fn under(path: &str, entry: &str) -> bool {
    path == entry || path.strip_prefix(entry).is_some_and(|r| r.starts_with('/'))
}

/// Build and persist a real `TaskPackage/v1` for the generated policy so the
/// identity invariant runs through the production serializer.
fn build_persisted(
    state_dir: &Path,
    task_id: &str,
    allowed: &[String],
    protected: &[String],
) -> agalma_contracts::TaskPackage {
    let definition = TaskDefinition {
        task_id: TaskId::new(task_id),
        title: format!("Property {task_id}"),
        description: "Body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\n\
                      acceptance:\n  - sh -c 'true'\nbudget_usd: 0.00\npriority: normal\n\
                      family: feature\nmax_attempts: 1\n```\n"
            .to_string(),
        design: Some("Design.\n".to_string()),
        notes: Some("Notes.\n".to_string()),
        acceptance_criteria: None,
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    };
    let inputs = PackageInputs {
        execution_id: format!("exec:{task_id}:1"),
        generation: 1,
        lease_generation: 1,
        repository_identity: "agalma".to_string(),
        base_sha: "a".repeat(40),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 0,
            digest: sha256_digest(b"directive"),
        },
        authorization: AuthorizationInput {
            origin: AuthorizationOrigin::HumanBootstrap,
            policy_digest: sha256_digest(b"human-bootstrap/v1"),
            receipt_source: format!("bd:{task_id}/claim"),
            receipt_media_type: "application/json".to_string(),
            receipt_bytes: b"{}".to_vec(),
        },
        roles: vec![RoleInput::new(
            Role::Builder,
            "route.builder".to_string(),
            FREE_A.to_string(),
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
        instructions: vec![ContextInput::new(
            ContextKind::Instruction,
            "AGENTS.md",
            "text/plain",
            AGENTS_BYTES.as_bytes().to_vec(),
        )],
        shared_design: vec![ContextInput::new(
            ContextKind::SharedDesign,
            "docs/self-mutation-contracts.md",
            "text/plain",
            SHARED_DESIGN_BYTES.as_bytes().to_vec(),
        )],
        parent_design: Vec::new(),
        dependency_handoffs: Vec::new(),
        allowed_paths: allowed.to_vec(),
        protected_paths: protected.to_vec(),
        readable_artifacts: Vec::new(),
        oracle_bytes: ProtectedEntry {
            task_id: task_id.to_string(),
            repository_root: "/origin".to_string(),
            base_sha: "a".repeat(40),
            allowed_paths: allowed.to_vec(),
            protected_paths: protected.to_vec(),
            baseline: ProtectedBaseline {
                format: PROTECTED_BASELINE_FORMAT.to_string(),
                files: vec![BaselineFile {
                    path: "runner.sh".to_string(),
                    mode: "100644".to_string(),
                    digest: sha256_digest(b"runner"),
                    bytes: 6,
                    text: "runner".to_string(),
                }],
                commands: vec!["sh -c 'true'".to_string()],
            },
        }
        .freeze()
        .expect("freeze oracle"),
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
