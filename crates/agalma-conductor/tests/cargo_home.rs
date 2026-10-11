//! B1-F6 (`agalma-dbv`) regression: sandboxed offline cargo resolution needs a
//! seeded `CARGO_HOME` that exposes the real cargo registry read-only.
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
//! to the resolved real registry (`$CARGO_HOME` or `$HOME/.cargo`; already
//! exposed read-only via `extra_ro_roots`). This test drives the conductor's
//! real `verify` activity in `Live` mode (Seatbelt acceptance) over a fresh
//! checkout whose acceptance is a resolution-only `cargo metadata --offline`,
//! asserting exit 0 and emitted metadata. A focused unit covers the helper's
//! idempotency and link target directly.
//!
//! Scratch state lives under `target/test-runs/cargo-home-*` (never `/tmp`); the
//! run is offline and makes no model turn (acceptance is green). Scratch dirs
//! auto-remove on success (kept on failure for forensics).

#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use agalma_conductor::workspace_process::WorkspaceHandle;
use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{ExecutionId, ExecutionPhase};
use agalma_execution::{ActivityNext, OperationContext};
use agalma_testkit::ScratchDir;

use agalma_conductor::harness_binding::{harness_handle, HarnessChoice};
use agalma_conductor::phases::{
    activities_with_mode, ensure_cargo_home, run_root, ActivityMode, PhaseDeps, RepoMode, RunState,
};

/// Resolution-only acceptance command: must exit 0 and print metadata JSON when
/// the seeded registry link works offline.
const ACCEPT: &str = "cargo metadata --offline --format-version 1";

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(&format!("cargo-home-{name}"))
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

/// End-to-end: the real Seatbelt acceptance launch resolves offline against the
/// seeded CARGO_HOME in a fresh checkout.
#[test]
fn sandboxed_acceptance_resolves_cargo_offline() {
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

    // Render the real product profile (the one the conductor ships) to a file.
    let profile = state_dir.join("worker.sb");
    fs::write(&profile, agalma_sandbox::PRODUCT_PROFILE).expect("write profile");

    // Toolchain env/roots mirroring `composition::toolchain` so the confined
    // cargo finds the rustup shim and the real (read-only) homes.
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
    let extra_ro_roots = vec![
        fs::canonicalize(&cargo_home).unwrap_or(cargo_home.clone()),
        fs::canonicalize(&rustup_home).unwrap_or(rustup_home.clone()),
    ];

    let deps = PhaseDeps {
        state_dir: state_dir.clone(),
        fixture: origin.clone(),
        model: "test/offline".to_string(),
        verifier_model: "test/offline".to_string(),
        route: "builder:fix".to_string(),
        verifier_route: "verifier:fix".to_string(),
        role: "builder".to_string(),
        profile_path: profile,
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        paid_proxy: false,
        extra_ro_roots,
        toolchain_env,
        turn_timeout: Duration::from_secs(5),
        verify_timeout: Duration::from_secs(120),
        idle_timeout: Duration::from_secs(5),
        repo: Some(RepoMode {
            origin,
            base_ref: "main".to_string(),
            task_title: "Sandboxed offline resolution".to_string(),
            acceptance: vec![ACCEPT.to_string()],
        }),
        harness: harness_handle(HarnessChoice::OpenCode),
    };

    let workspace = Rc::new(RefCell::new(WorkspaceHandle::in_process(&state_dir)));
    let state = Rc::new(RefCell::new(RunState::default()));
    let mut acts = activities_with_mode(
        deps,
        Rc::clone(&workspace),
        Rc::clone(&state),
        ActivityMode::Live,
    );

    let task_id = TaskId::new("cargo-home-accept");
    let exec = ExecutionId::derive(&task_id, 1);
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
        "cargo metadata must exit 0 in the sandbox:\n{log}"
    );
    assert!(
        log.contains("\"packages\""),
        "cargo metadata must emit metadata JSON:\n{log}"
    );

    // The seeded link backs the run-local CARGO_HOME.
    let seeded = run_root(&state_dir, &exec)
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
