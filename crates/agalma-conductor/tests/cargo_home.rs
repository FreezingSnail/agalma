//! B1-F6 (`agalma-dbv`) regression: offline cargo resolution needs a
//! seeded `CARGO_HOME` that exposes the real cargo registry.
//!
//! Live factory run 2 acceptance (`cargo build --offline -p agalma-conductor`)
//! failed exit 101 `no matching package named serde found ... offline mode`.
//! Root cause: the acceptance env copied `toolchain_env` (no `CARGO_HOME`) and
//! pointed `HOME` at an empty run-local `verify-home`, so cargo defaulted to an
//! empty `CARGO_HOME` and could not resolve dependencies offline. The builder
//! and `cargo test` verify envs set `CARGO_HOME=<run>/cargo-home`, but that dir
//! was empty too.
//!
//! Fix: `phases::ensure_cargo_home` seeds `<run>/cargo-home/registry` as a link
//! to the resolved real registry (`$CARGO_HOME` or `$HOME/.cargo`). This test
//! drives the conductor's real `verify` activity in `Live` mode over a fresh
//! checkout whose acceptance is a resolution-only `cargo metadata --offline`,
//! asserting exit 0 and emitted metadata. A focused unit covers the helper's
//! idempotency and link target directly.
//!
//! Scratch state lives under `target/test-runs/cargo-home-*` (never `/tmp`); the
//! run is offline and makes no model turn (acceptance is green). Scratch dirs
//! are retained for review evidence (`ScratchDir::keep`), not auto-removed.
//!
//! SF.6 (`agalma-5fy.6`): repository verification is **fail-closed** and reads
//! the operator `ProtectedInputs/v1` manifest (`<state_dir>/protected-inputs.json`)
//! frozen whole as the package's `scope.oracle`. This fixture therefore seeds a
//! valid operator entry, freezes the complete `ProtectedEntry/v1` as the oracle,
//! and populates the package scope from the same entry so the recorded package
//! loads and the protected judge materializes. A legacy opaque/absent oracle
//! refuses (see the regression below); production validation is unchanged.

#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, ProtectedInputs, PROTECTED_BASELINE_FORMAT,
    PROTECTED_INPUTS_FILE, PROTECTED_INPUTS_FORMAT,
};
use agalma_conductor::task_package::{
    build_package, persist_package, AuthorizationInput, ContextInput, PackageInputs, RoleInput,
};
use agalma_conductor::workspace_process::WorkspaceHandle;
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{
    AuthorizationOrigin, BindingPin, ContextKind, DirectiveRef, ExecutionId, ExecutionPhase, Role,
};
use agalma_execution::{ActivityNext, OperationContext};
use agalma_taskqueue::TaskDefinition;
use agalma_testkit::ScratchDir;

use agalma_conductor::harness_binding::{harness_handle, HarnessChoice};
use agalma_conductor::phases::{
    activities_with_mode, ensure_cargo_home, resolve_recorded_package, run_root, verify_runtime_dir,
    ActivityMode, PhaseDeps, RepoMode, RunState,
};

/// Resolution-only acceptance command: must exit 0 and print metadata JSON when
/// the seeded registry link works offline.
const ACCEPT: &str = "cargo metadata --offline --format-version 1";

fn fresh(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(&format!("cargo-home-{name}"));
    // Retain fixtures for review evidence (`ScratchDir::keep`); not auto-removed.
    dir.keep();
    dir
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The real cargo home cargo itself would use (mirrors `composition::toolchain`).
fn real_cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cargo"))
}

fn real_rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".rustup"))
}

fn cargo_bin() -> PathBuf {
    std::env::var_os("CARGO")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .unwrap_or_else(|| real_cargo_home().join("bin").join("cargo"))
}

/// A scratch origin with a detached cargo package (depends on `serde`, present
/// in the real offline cache) plus a second `docs/` commit so `HEAD~1` resolves
/// in the verification checkout. The lockfile is generated offline at seed time.
fn seed_origin(dir: &Path) -> PathBuf {
    let origin = dir.join("origin");
    fs::create_dir_all(origin.join("src")).expect("origin src");
    fs::write(
        origin.join("Cargo.toml"),
        "[workspace]\n\n[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nserde = { version = \"=1.0.229\", features = [\"derive\"] }\n",
    )
    .expect("origin manifest");
    fs::write(
        origin.join("src").join("lib.rs"),
        "//! Seed source.\npub fn answer() -> u32 { 42 }\n",
    )
    .expect("seed source");

    let lock = Command::new(cargo_bin())
        .args(["generate-lockfile", "--offline"])
        .current_dir(&origin)
        .output()
        .expect("spawn cargo generate-lockfile");
    assert!(
        lock.status.success(),
        "generate-lockfile: {}",
        String::from_utf8_lossy(&lock.stderr)
    );

    git_ok(&origin, &["init", "-q", "-b", "main"]);
    git_ok(&origin, &["config", "user.email", "agalma@localhost"]);
    git_ok(&origin, &["config", "user.name", "Agalma"]);
    git_ok(&origin, &["add", "-A"]);
    git_ok(&origin, &["commit", "-qm", "seed"]);

    fs::create_dir_all(origin.join("docs")).expect("origin docs");
    fs::write(origin.join("docs").join("note.md"), "# note\n").expect("docs note");
    git_ok(&origin, &["add", "-A"]);
    git_ok(&origin, &["commit", "-qm", "docs note"]);
    origin
}

/// Task id this fixture admits (mirrors `ctx.task_id`).
const TASK: &str = "cargo-home-accept";

/// The resolved `HEAD` of the scratch origin (the base the operator pins).
fn head(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("git rev-parse");
    assert!(out.status.success(), "git rev-parse HEAD");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A minimal harness [`BindingPin`] for the frozen builder role.
fn binding_pin() -> BindingPin {
    BindingPin {
        binding_id: "harness.opencode".to_string(),
        api: "harness".to_string(),
        api_version: 1,
        implementation_version: "opencode/v1".to_string(),
        generation: 1,
        config_digest: sha256_digest(b"{}"),
    }
}

/// A valid operator `ProtectedInputs/v1` entry for `task_id` at the scratch
/// origin's `HEAD`: a nonempty allow/protect scope and a `ProtectedBaseline/v1`
/// with one regular file plus a nonempty command.
fn protected_entry(task_id: &str, origin: &Path, base_sha: &str) -> ProtectedEntry {
    let root = fs::canonicalize(origin).expect("canonical origin");
    let marker = "protected fixture marker\n";
    ProtectedEntry {
        task_id: task_id.to_string(),
        repository_root: root.to_string_lossy().into_owned(),
        base_sha: base_sha.to_string(),
        allowed_paths: vec!["src".to_string(), "docs".to_string()],
        protected_paths: vec!["tests".to_string()],
        baseline: ProtectedBaseline {
            format: PROTECTED_BASELINE_FORMAT.to_string(),
            files: vec![BaselineFile {
                path: "protected/marker.txt".to_string(),
                mode: "100644".to_string(),
                digest: sha256_digest(marker.as_bytes()),
                bytes: marker.len() as u64,
                text: marker.to_string(),
            }],
            commands: vec!["sh -c 'true'".to_string()],
        },
    }
}

/// Write the operator `ProtectedInputs/v1` manifest (`<state_dir>/protected-inputs.json`),
/// validating it through the production contract before persisting.
fn seed_operator_manifest(state_dir: &Path, entry: &ProtectedEntry) {
    fs::create_dir_all(state_dir).expect("state dir");
    let manifest = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries: vec![entry.clone()],
    };
    let bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
    ProtectedInputs::parse(&bytes).expect("seeded operator manifest must be valid");
    fs::write(state_dir.join(PROTECTED_INPUTS_FILE), bytes).expect("write manifest");
}

/// Seed a *valid* SF.6 recorded `TaskPackage/v1` for `execution_id`: the selected
/// operator entry is frozen whole as `scope.oracle`, the scope is populated from
/// the same entry, and the frozen acceptance is the resolution-only command. A
/// strict repository-mode harness resolves it and the protected judge loads.
fn seed_protected_package(
    state_dir: &Path,
    task_id: &str,
    execution_id: &str,
    entry: &ProtectedEntry,
) {
    seed_package_with(
        state_dir,
        task_id,
        execution_id,
        &entry.base_sha,
        entry.allowed_paths.clone(),
        entry.protected_paths.clone(),
        entry.freeze().expect("freeze entry"),
    );
}

/// Low-level fixture: record a package with explicit scope fields and raw oracle
/// bytes (used by the valid fixture and by the malformed/identity-mismatch
/// refusal regressions).
fn seed_package_with(
    state_dir: &Path,
    task_id: &str,
    execution_id: &str,
    base_sha: &str,
    allowed_paths: Vec<String>,
    protected_paths: Vec<String>,
    oracle_bytes: Vec<u8>,
) {
    let definition = TaskDefinition {
        task_id: TaskId::new(task_id),
        title: "Offline resolution".to_string(),
        description: "Offline cargo resolution acceptance.\n".to_string(),
        design: Some(String::new()),
        notes: None,
        acceptance_criteria: Some(ACCEPT.to_string()),
        parent: None,
        dependencies: Vec::new(),
        comments: Vec::new(),
    };
    let inputs = PackageInputs {
        execution_id: execution_id.to_string(),
        generation: 1,
        lease_generation: 1,
        repository_identity: "cargo-home-fixture".to_string(),
        base_sha: base_sha.to_string(),
        directive: DirectiveRef {
            id: "directive/v0".to_string(),
            version: 0,
            digest: sha256_digest(b"directive/v0"),
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
            "builder:fix".to_string(),
            "test/offline".to_string(),
            binding_pin(),
            b"builder prompt".to_vec(),
            "text/plain",
        )],
        instructions: vec![ContextInput::new(
            ContextKind::Instruction,
            "AGENTS.md",
            "text/plain",
            b"repo rules".to_vec(),
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
    let built = build_package(&definition, inputs).expect("build package");
    persist_package(state_dir, &built).expect("persist package");
}

/// Focused helper check: seeds the registry link, is idempotent, points at the
/// resolved real registry, and replaces a wrong/self-referential link.
#[test]
fn ensure_cargo_home_seeds_idempotent_registry_link() {
    let real_registry = real_cargo_home().join("registry");
    if !real_registry.is_dir() {
        eprintln!(
            "skipping: no real cargo registry at {}",
            real_registry.display()
        );
        return;
    }
    let expected = fs::canonicalize(&real_registry).expect("canonical real registry");

    let dir = fresh("helper");
    let run = dir.join("run");

    // First call creates the link.
    let cargo_home = ensure_cargo_home(&run).expect("first seed");
    let link = cargo_home.join("registry");
    let meta = fs::symlink_metadata(&link).expect("link metadata");
    assert!(meta.file_type().is_symlink(), "registry must be a symlink");
    assert_eq!(fs::canonicalize(&link).expect("canonical link"), expected);

    // Second call is idempotent (no error, target unchanged).
    ensure_cargo_home(&run).expect("second seed");
    assert_eq!(fs::canonicalize(&link).expect("canonical link"), expected);

    // A wrong link is replaced.
    fs::remove_file(&link).expect("remove link");
    std::os::unix::fs::symlink(&dir, &link).expect("wrong link");
    ensure_cargo_home(&run).expect("replace wrong link");
    assert_eq!(fs::canonicalize(&link).expect("canonical link"), expected);

    // A self-referential link is replaced.
    fs::remove_file(&link).expect("remove link");
    std::os::unix::fs::symlink("registry", &link).expect("self link");
    ensure_cargo_home(&run).expect("replace self link");
    assert_eq!(fs::canonicalize(&link).expect("canonical link"), expected);
}

/// End-to-end: the real acceptance launch resolves offline against the seeded
/// CARGO_HOME in a fresh checkout.
#[test]
fn acceptance_resolves_cargo_offline() {
    let cargo_home = real_cargo_home();
    if !cargo_home.join("registry").is_dir() {
        eprintln!(
            "skipping: no real cargo registry under {}",
            cargo_home.display()
        );
        return;
    }

    let dir = fresh("accept");
    let origin = seed_origin(&dir);
    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    // SF.1 + SF.6: repository mode requires the recorded package, and repository
    // verification is fail-closed on the operator protected inputs. Seed the
    // valid operator `ProtectedInputs/v1` entry, freeze the selected complete
    // `ProtectedEntry/v1` as `scope.oracle`, and record the package that agrees
    // with it, so the strict harness and the protected judge both load.
    let task_id = TaskId::new(TASK);
    let exec = ExecutionId::derive(&task_id, 1);
    let entry = protected_entry(TASK, &origin, &head(&origin));
    seed_operator_manifest(&state_dir, &entry);
    let manifest = ProtectedInputs::load(&state_dir).expect("load operator manifest");
    let selected = manifest
        .select(TASK, &entry.repository_root, &entry.base_sha)
        .expect("selected operator entry")
        .clone();
    seed_protected_package(&state_dir, TASK, exec.as_str(), &selected);

    // Toolchain env mirroring `composition::toolchain` so the launched cargo
    // finds the rustup shim and the real homes via PATH.
    let rustup_home = real_rustup_home();
    let cargo_bin_dir = cargo_home.join("bin");
    let mut toolchain_env = BTreeMap::new();
    toolchain_env.insert(
        "RUSTUP_HOME".to_string(),
        rustup_home.to_string_lossy().into_owned(),
    );
    toolchain_env.insert(
        "RUSTUP_TOOLCHAIN".to_string(),
        std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "stable".to_string()),
    );
    toolchain_env.insert(
        "PATH".to_string(),
        format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", cargo_bin_dir.display()),
    );

    let deps = PhaseDeps {
        state_dir: state_dir.clone(),
        fixture: origin.clone(),
        model: "test/offline".to_string(),
        verifier_model: "test/offline".to_string(),
        route: "builder:fix".to_string(),
        verifier_route: "verifier:fix".to_string(),
        role: "builder".to_string(),
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        verifier_proxy_url: None,
        paid_proxy: false,
        toolchain_env,
        turn_timeout: Duration::from_secs(5),
        verify_timeout: Duration::from_secs(120),
        idle_timeout: Duration::from_secs(5),
        repo: Some(RepoMode {
            origin,
            base_ref: "main".to_string(),
            task_title: "Offline resolution".to_string(),
            acceptance: vec![ACCEPT.to_string()],
        }),
        harness: harness_handle(HarnessChoice::OpenCode),
        stop: std::rc::Rc::new(agalma_conductor::stop::FileStopControl::new(&state_dir)),
    };

    let workspace = Rc::new(RefCell::new(WorkspaceHandle::in_process(&state_dir)));
    let state = Rc::new(RefCell::new(RunState::default()));
    let mut acts = activities_with_mode(
        deps,
        Rc::clone(&workspace),
        Rc::clone(&state),
        ActivityMode::Live,
    );

    let ctx = OperationContext {
        execution_id: exec.clone(),
        operation_id: OperationId::derive(&exec, "verify@1"),
        task_id,
        phase: ExecutionPhase::Verify,
        attempt: 1,
        lease_generation: 1,
        inputs: serde_json::json!({}),
    };

    // Real repo-mode checkout, then the real Seatbelt acceptance launch.
    acts.checkout.execute(&ctx).expect("checkout");
    let outcome = acts.verify.execute(&ctx).expect("verify execute");
    assert_eq!(
        outcome.next,
        ActivityNext::Advance,
        "offline resolution must be green: {}",
        outcome.result
    );

    let artifact = run_root(&state_dir, &exec)
        .join("artifacts")
        .join("accept@1-0.txt");
    let log = fs::read_to_string(&artifact)
        .unwrap_or_else(|e| panic!("read acceptance log {}: {e}", artifact.display()));
    assert!(
        log.contains("exit: 0"),
        "cargo metadata must exit 0:\n{log}"
    );
    assert!(
        log.contains("\"packages\""),
        "cargo metadata must emit metadata JSON:\n{log}"
    );

    // The seeded link backs the run-local CARGO_HOME under the SF.6 narrow
    // verifier runtime root (outside the parent receipt markers/artifacts).
    let seeded = verify_runtime_dir(&state_dir, &exec)
        .join("cargo-home")
        .join("registry");
    assert!(
        fs::symlink_metadata(&seeded)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "run-local cargo-home/registry must be seeded: {}",
        seeded.display()
    );
}

// ---------------------------------------------------------------------------
// Fixture contract: deterministic protected-input invariants + legacy refusals
// ---------------------------------------------------------------------------

/// A template valid operator entry rooted at a synthetic origin (used by the
/// pure property below; the acceptance fixture canonicalizes a real origin).
fn template_entry() -> ProtectedEntry {
    let marker = "protected fixture marker\n";
    ProtectedEntry {
        task_id: TASK.to_string(),
        repository_root: "/origin".to_string(),
        base_sha: "0".repeat(40),
        allowed_paths: vec!["src".to_string()],
        protected_paths: vec!["tests".to_string()],
        baseline: ProtectedBaseline {
            format: PROTECTED_BASELINE_FORMAT.to_string(),
            files: vec![BaselineFile {
                path: "protected/marker.txt".to_string(),
                mode: "100644".to_string(),
                digest: sha256_digest(marker.as_bytes()),
                bytes: marker.len() as u64,
                text: marker.to_string(),
            }],
            commands: vec!["sh -c 'true'".to_string()],
        },
    }
}

fn frozen_bytes(entry: &ProtectedEntry) -> Vec<u8> {
    entry.freeze().expect("serialize entry")
}

/// Deterministic property (fixed budget: 12 entry cases × 1 assertion each, plus
/// 3 `select` identity cases, 4 manifest cases, and 1 missing-manifest case = 20
/// assertions; no randomness) over the SF.6 protected-input contract these
/// fixtures consume: a valid entry round-trips through `freeze`/`parse`, a valid
/// manifest selects the frozen identity, and every malformed/missing/duplicate/
/// identity-mismatched variant refuses. Expected validity is derived from the
/// mutation kind, not from the implementation. Seed: none (enumerated), replay
/// with `cargo test --offline --locked -p agalma-conductor --test cargo_home`.
#[test]
fn protected_entry_and_manifest_round_trip_and_refuse_invalid() {
    let base = template_entry();
    let mut entry_cases: Vec<(&str, ProtectedEntry, bool)> = Vec::new();
    entry_cases.push(("valid", base.clone(), true));

    let mut empty_task = base.clone();
    empty_task.task_id = String::new();
    entry_cases.push(("empty_task_id", empty_task, false));

    let mut empty_root = base.clone();
    empty_root.repository_root = String::new();
    entry_cases.push(("empty_repository_root", empty_root, false));

    let mut empty_base = base.clone();
    empty_base.base_sha = String::new();
    entry_cases.push(("empty_base_sha", empty_base, false));

    let mut abs_path = base.clone();
    abs_path.allowed_paths = vec!["/abs".to_string()];
    entry_cases.push(("absolute_allowed_path", abs_path, false));

    let mut escape_path = base.clone();
    escape_path.protected_paths = vec!["a/../b".to_string()];
    entry_cases.push(("escaping_protected_path", escape_path, false));

    let mut bad_format = base.clone();
    bad_format.baseline.format = "ProtectedBaseline/v2".to_string();
    entry_cases.push(("unsupported_baseline_format", bad_format, false));

    let mut no_files = base.clone();
    no_files.baseline.files = Vec::new();
    entry_cases.push(("empty_baseline_files", no_files, false));

    let mut no_commands = base.clone();
    no_commands.baseline.commands = Vec::new();
    entry_cases.push(("empty_baseline_commands", no_commands, false));

    let mut bad_digest = base.clone();
    bad_digest.baseline.files[0].digest = sha256_digest(b"different");
    entry_cases.push(("baseline_digest_mismatch", bad_digest, false));

    let mut bad_mode = base.clone();
    bad_mode.baseline.files[0].mode = "120000".to_string();
    entry_cases.push(("unsupported_file_mode", bad_mode, false));

    let mut dup_file = base.clone();
    dup_file
        .baseline
        .files
        .push(dup_file.baseline.files[0].clone());
    entry_cases.push(("duplicate_baseline_file", dup_file, false));

    let mut cases = 0usize;
    for (label, entry, expect_valid) in entry_cases {
        cases += 1;
        let parsed = ProtectedEntry::parse(&frozen_bytes(&entry));
        assert_eq!(
            parsed.is_ok(),
            expect_valid,
            "entry case {label:?} validity must be {expect_valid}"
        );
    }

    // A valid manifest selects the frozen identity; invalid variants refuse.
    let manifest = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries: vec![base.clone()],
    };
    let good = serde_json::to_vec(&manifest).expect("json");
    let parsed = ProtectedInputs::parse(&good).expect("valid manifest parses");
    parsed
        .select(TASK, "/origin", &base.base_sha)
        .expect("identity match selects");
    assert!(
        parsed.select("other.task", "/origin", &base.base_sha).is_err(),
        "missing task entry refuses"
    );
    assert!(
        parsed.select(TASK, "/other", &base.base_sha).is_err(),
        "repository identity mismatch refuses"
    );
    assert!(
        parsed.select(TASK, "/origin", "different").is_err(),
        "base identity mismatch refuses"
    );
    cases += 3;

    let wrong_format = serde_json::json!({
        "format": "ProtectedInputs/v2",
        "entries": [serde_json::to_value(&base).unwrap()],
    });
    assert!(
        ProtectedInputs::parse(&serde_json::to_vec(&wrong_format).unwrap()).is_err(),
        "unsupported manifest format refuses"
    );
    cases += 1;

    let empty_entries = serde_json::json!({ "format": PROTECTED_INPUTS_FORMAT, "entries": [] });
    assert!(
        ProtectedInputs::parse(&serde_json::to_vec(&empty_entries).unwrap()).is_err(),
        "an empty manifest refuses"
    );
    cases += 1;

    let duplicate = serde_json::json!({
        "format": PROTECTED_INPUTS_FORMAT,
        "entries": [serde_json::to_value(&base).unwrap(), serde_json::to_value(&base).unwrap()],
    });
    assert!(
        ProtectedInputs::parse(&serde_json::to_vec(&duplicate).unwrap()).is_err(),
        "duplicate task entries refuse"
    );
    cases += 1;

    let unknown = serde_json::json!({
        "format": PROTECTED_INPUTS_FORMAT,
        "entries": [serde_json::to_value(&base).unwrap()],
        "extra": 1,
    });
    assert!(
        ProtectedInputs::parse(&serde_json::to_vec(&unknown).unwrap()).is_err(),
        "unknown manifest fields refuse"
    );
    cases += 1;

    // A missing operator manifest refuses (never authorizes repository work).
    let empty = fresh("manifest-missing");
    assert!(
        ProtectedInputs::load(&empty).is_err(),
        "a missing operator manifest refuses"
    );
    cases += 1;

    assert_eq!(cases, 20, "fixed deterministic case budget is exercised");
}

/// Regression: the legacy fixtures' opaque oracle and any identity-mismatched
/// oracle refuse at the recorded-package boundary (production fail-closed path
/// unchanged). A concrete form of the previous `harness-oracle` seed is the
/// malformed case.
#[test]
fn legacy_opaque_and_mismatched_oracles_refuse_resolution() {
    let dir = fresh("oracle-refuse");
    let origin = seed_origin(&dir);
    let base_sha = head(&origin);

    // (a) Opaque legacy oracle: not a `ProtectedEntry` at all.
    let opaque = dir.join("opaque");
    seed_package_with(
        &opaque,
        TASK,
        "exec:cargo-home-opaque:1",
        &base_sha,
        vec!["src".to_string()],
        vec!["tests".to_string()],
        b"harness-oracle".to_vec(),
    );
    let exec = ExecutionId::derive(&TaskId::new("cargo-home-opaque"), 1);
    assert!(
        matches!(
            resolve_recorded_package(&opaque, &exec),
            Err(_)
        ),
        "an opaque (non-ProtectedEntry) oracle must refuse resolution"
    );

    // (b) Identity-mismatched oracle: valid entry, wrong task id.
    let mismatch = dir.join("mismatch");
    let mut mismatched = protected_entry(TASK, &origin, &base_sha);
    mismatched.task_id = "some.other.task".to_string();
    seed_package_with(
        &mismatch,
        TASK,
        "exec:cargo-home-mismatch:1",
        &base_sha,
        mismatched.allowed_paths.clone(),
        mismatched.protected_paths.clone(),
        frozen_bytes(&mismatched),
    );
    let exec = ExecutionId::derive(&TaskId::new("cargo-home-mismatch"), 1);
    assert!(
        matches!(resolve_recorded_package(&mismatch, &exec), Err(_)),
        "an identity-mismatched oracle must refuse resolution"
    );

    // (c) Scope disagreement: the entry's paths disagree with the package scope.
    let disagree = dir.join("disagree");
    let entry = protected_entry(TASK, &origin, &base_sha);
    seed_package_with(
        &disagree,
        TASK,
        "exec:cargo-home-disagree:1",
        &base_sha,
        vec!["different".to_string()],
        entry.protected_paths.clone(),
        frozen_bytes(&entry),
    );
    let exec = ExecutionId::derive(&TaskId::new("cargo-home-disagree"), 1);
    assert!(
        matches!(resolve_recorded_package(&disagree, &exec), Err(_)),
        "a frozen scope that disagrees with its oracle must refuse resolution"
    );
}

// ---------------------------------------------------------------------------
// Deterministic generation over the protected-input contract (fixed seed)
// ---------------------------------------------------------------------------

/// SplitMix64 generator (same algorithm as `agalma-testkit`; deterministic,
/// no runtime randomness). Replay: fixed seeds below, fixed case budgets.
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

/// Independent property: for generated `ProtectedEntry` mutations, a case is
/// valid **iff** its mutation class is the valid class; every valid case
/// round-trips `freeze`/`parse` to an identical entry, and every malformed /
/// missing-identity / bad-path / baseline-boundary case refuses. Expected
/// validity is derived from the class, not the implementation. Deterministic
/// seed `0x5F6A_0000_0012`, fixed budget 256; every class is asserted generated.
#[test]
fn generated_protected_entry_mutations_round_trip_or_refuse() {
    const SEED: u64 = 0x5F6A_0000_0012;
    const BUDGET: u64 = 256;
    let mut rng = Rng::new(SEED);
    let mut class_hits = [0u64; 8];
    for case in 0..BUDGET {
        let mut entry = template_entry();
        let class = rng.below(class_hits.len() as u64) as usize;
        class_hits[class] += 1;
        let expected_valid = match class {
            0 => true,
            1 => {
                // Blank one identity field.
                match rng.below(3) {
                    0 => entry.task_id.clear(),
                    1 => entry.repository_root.clear(),
                    _ => entry.base_sha.clear(),
                }
                false
            }
            2 => {
                // Malformed scope path (absolute / escape / empty / dot).
                let bad = ["/abs", "..", "a/../b", "a//b", "."][rng.below(5) as usize].to_string();
                if rng.next_u64() & 1 == 0 {
                    entry.allowed_paths = vec![bad];
                } else {
                    entry.protected_paths = vec![bad];
                }
                false
            }
            3 => {
                entry.baseline.format = "ProtectedBaseline/v0".to_string();
                false
            }
            4 => {
                entry.baseline.files.clear();
                false
            }
            5 => {
                entry.baseline.commands.clear();
                false
            }
            6 => {
                // Baseline file boundary: digest mismatch or unsupported mode.
                if rng.next_u64() & 1 == 0 {
                    entry.baseline.files[0].digest = sha256_digest(b"other");
                } else {
                    entry.baseline.files[0].mode = "120000".to_string();
                }
                false
            }
            _ => {
                // Duplicate (normalized) protected file path.
                entry.baseline.files.push(entry.baseline.files[0].clone());
                false
            }
        };
        let bytes = frozen_bytes(&entry);
        let parsed = ProtectedEntry::parse(&bytes);
        assert_eq!(
            parsed.is_ok(),
            expected_valid,
            "case {case}: class {class} validity must be {expected_valid}"
        );
        if expected_valid {
            assert_eq!(
                parsed.expect("valid entry parses"),
                entry,
                "case {case}: a valid entry round-trips identically"
            );
        }
    }
    let total: u64 = class_hits.iter().sum();
    assert_eq!(total, BUDGET, "fixed generated case budget is exercised");
    for (class, hits) in class_hits.iter().enumerate() {
        assert!(*hits > 0, "mutation class {class} was generated");
    }
}

/// Independent property over generated `ProtectedInputs/v1` manifest
/// mutations: a valid manifest parses and selects the frozen identity; a
/// duplicate-entry, wrong-format, empty, unknown-field, or identity-mismatched
/// manifest refuses. Deterministic seed `0x5F6A_0000_0013`, fixed budget 128;
/// every class is asserted generated.
#[test]
fn generated_protected_manifest_mutations_select_or_refuse() {
    const SEED: u64 = 0x5F6A_0000_0013;
    const BUDGET: u64 = 128;
    let base = template_entry();
    let mut rng = Rng::new(SEED);
    let mut class_hits = [0u64; 6];
    for case in 0..BUDGET {
        let class = rng.below(class_hits.len() as u64) as usize;
        class_hits[class] += 1;
        let expect_parse_ok = class == 0 || class == 5;
        let refuse_selection = class == 5;
        let doc = match class {
            0 => serde_json::json!({
                "format": PROTECTED_INPUTS_FORMAT,
                "entries": [serde_json::to_value(&base).unwrap()],
            }),
            1 => serde_json::json!({
                "format": PROTECTED_INPUTS_FORMAT,
                "entries": [serde_json::to_value(&base).unwrap(), serde_json::to_value(&base).unwrap()],
            }),
            2 => serde_json::json!({
                "format": "ProtectedInputs/v2",
                "entries": [serde_json::to_value(&base).unwrap()],
            }),
            3 => serde_json::json!({ "format": PROTECTED_INPUTS_FORMAT, "entries": [] }),
            4 => serde_json::json!({
                "format": PROTECTED_INPUTS_FORMAT,
                "entries": [serde_json::to_value(&base).unwrap()],
                "extra": 1,
            }),
            _ => serde_json::json!({
                "format": PROTECTED_INPUTS_FORMAT,
                "entries": [serde_json::to_value(&base).unwrap()],
            }),
        };
        let bytes = serde_json::to_vec(&doc).expect("json");
        match ProtectedInputs::parse(&bytes) {
            Ok(parsed) => {
                assert!(expect_parse_ok, "case {case}: class {class} must refuse parsing");
                if refuse_selection {
                    assert!(
                        parsed.select("missing.task", "/origin", &base.base_sha).is_err(),
                        "case {case}: a missing task entry refuses selection"
                    );
                    assert!(
                        parsed.select(&base.task_id, "/other", &base.base_sha).is_err(),
                        "case {case}: a repository identity mismatch refuses selection"
                    );
                } else {
                    assert!(
                        parsed.select(TASK, "/origin", &base.base_sha).is_ok(),
                        "case {case}: the frozen identity selects"
                    );
                }
            }
            Err(_) => assert!(!expect_parse_ok, "case {case}: class {class} must parse"),
        }
    }
    let total: u64 = class_hits.iter().sum();
    assert_eq!(total, BUDGET, "fixed generated case budget is exercised");
    for (class, hits) in class_hits.iter().enumerate() {
        assert!(*hits > 0, "manifest mutation class {class} was generated");
    }
}
