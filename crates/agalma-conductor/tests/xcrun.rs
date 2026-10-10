//! H2 (`agalma-zxi.3`) regression: a git-based scope-guard command runs
//! through the real sandboxed acceptance launch with no `xcrun_db` EPERM noise.
//!
//! Background: on macOS `/usr/bin/git` is a shim over `libxcselect`. It resolves
//! the active developer dir via `xcselect`, whose cache lives at
//! `confstr(_CS_DARWIN_USER_TEMP_DIR)` -> `/var/folders/.../T/xcrun_db` — the
//! *system* per-user temp dir, which ignores `$TMPDIR`. Under the deny-default
//! Seatbelt profile that cache write was denied, so every sandboxed `git`
//! launch printed `couldn't create cache file ... (errno=Operation not
//! permitted)` (M1.8 finding 7). The product profile now grants a narrow
//! `xcrun_db*`-only allowance; this test drives the conductor's real `verify`
//! activity in `Live` mode (Seatbelt acceptance) and asserts the captured
//! output is clean.
//!
//! Scratch state lives under `target/test-runs/xcrun-*` (never `/tmp`); the run
//! is offline and makes no model turn (the guard is green, so no diagnosis
//! session is created).

#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agalma_contracts::ids::{OperationId, TaskId};
use agalma_contracts::{ExecutionId, ExecutionPhase};
use agalma_execution::{ActivityNext, OperationContext};
use agalma_workspace::Workspace;

use agalma_conductor::harness_binding::{harness_handle, HarnessChoice};
use agalma_conductor::phases::{
    activities_with_mode, run_root, ActivityMode, PhaseDeps, RepoMode, RunState,
};

/// The seed scope-guard from the M1 acceptance suite: green iff the candidate
/// diff touches no `Cargo.toml` and no crate `src/**/*.rs`.
const GUARD: &str =
    "sh -c '! git diff --name-only HEAD~1 HEAD | grep -qE \"Cargo\\.toml$|/src/.*\\.rs$\"'";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

fn fresh(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = root()
        .join("target")
        .join("test-runs")
        .join(format!("xcrun-{name}-{}-{nanos}", std::process::id()));
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create run dir");
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

/// A scratch origin with two commits so `HEAD~1` resolves inside the
/// verification checkout. The second commit touches only `docs/`, so the
/// scope-guard is green.
fn seed_origin(dir: &Path) -> PathBuf {
    let origin = dir.join("origin");
    fs::create_dir_all(origin.join("src")).expect("origin src");
    fs::write(
        origin.join("src").join("lib.rs"),
        "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
    )
    .expect("seed source");
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

#[test]
fn sandboxed_git_guard_runs_without_xcrun_noise() {
    let dir = fresh("guard");
    let origin = seed_origin(&dir);
    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).expect("state dir");

    // Render the real product profile (the one the conductor ships) to a file.
    let profile = state_dir.join("worker.sb");
    fs::write(&profile, agalma_sandbox::PRODUCT_PROFILE).expect("write profile");

    let deps = PhaseDeps {
        state_dir: state_dir.clone(),
        fixture: origin.clone(),
        model: "test/offline".to_string(),
        role: "builder".to_string(),
        profile_path: profile,
        builder_prompt: state_dir.join("unused-builder.md"),
        proxy_url: None,
        extra_ro_roots: Vec::new(),
        toolchain_env: BTreeMap::new(),
        turn_timeout: Duration::from_secs(5),
        verify_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_secs(5),
        repo: Some(RepoMode {
            origin,
            base_ref: "main".to_string(),
            task_title: "Sandboxed guard".to_string(),
            acceptance: vec![GUARD.to_string()],
        }),
        harness: harness_handle(HarnessChoice::OpenCode),
    };

    let workspace = Rc::new(RefCell::new(Workspace::new(state_dir.clone())));
    let state = Rc::new(RefCell::new(RunState::default()));
    let mut acts = activities_with_mode(
        deps,
        Rc::clone(&workspace),
        Rc::clone(&state),
        ActivityMode::Live,
    );

    let task_id = TaskId::new("xcrun-guard");
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
        "scope-guard must be green: {}",
        outcome.result
    );

    let artifact = run_root(&state_dir, &exec)
        .join("artifacts")
        .join("accept@1-0.txt");
    let log = fs::read_to_string(&artifact)
        .unwrap_or_else(|e| panic!("read acceptance log {}: {e}", artifact.display()));
    assert!(
        log.contains("exit: 0"),
        "guard must exit 0 in the sandbox:\n{log}"
    );
    assert!(
        !log.contains("xcrun_db"),
        "xcrun cache noise leaked into sandboxed acceptance output:\n{log}"
    );
    assert!(
        !log.contains("Operation not permitted"),
        "sandboxed acceptance saw an EPERM:\n{log}"
    );
}
