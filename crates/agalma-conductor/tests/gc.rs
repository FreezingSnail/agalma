//! `agalma gc` end-to-end (`agalma-xme`).
//!
//! Exercises the manual reap CLI over a scripted terminal execution: reclaim a
//! recreated heavy scratch dir while keeping the forensic evidence (exit 0),
//! the idempotent `nothing to reap` second run, and the state-dir lock refusal
//! while a conductor holds the lock. Scratch lives under
//! `target/test-runs/gc-*` via [`ScratchDir`] (never `/tmp`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

use agalma_conductor::cli::GcArgs;
use agalma_conductor::phases::run_root;
use agalma_conductor::{gc_command, LockGuard};
use agalma_contracts::ExecutionId;
use agalma_testkit::ScratchDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

/// A scratch origin repo (git + isolated bd store) with one green task issue,
/// mirroring `tests/reap.rs`/`tests/work.rs`.
struct Scratch {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Scratch {
    fn new(name: &str) -> (Scratch, String) {
        require_bd();
        let dir = ScratchDir::new(&format!("gc-{name}"));
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

        let description = "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n  - sh -c 'grep -q 42 src/lib.rs'\npriority: normal\nfamily: fix\nmax_attempts: 2\n```\n";
        let scratch = Scratch {
            origin,
            state_dir: dir.join("state"),
            _dir: dir,
        };
        let id = scratch.create("Gc task", description);
        (scratch, id)
    }

    fn beads(&self) -> PathBuf {
        self.origin.join(".beads")
    }

    fn create(&self, title: &str, description: &str) -> String {
        let out = Command::new("bd")
            .arg("--db")
            .arg(self.beads())
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
            .current_dir(&self.origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd create");
        assert!(out.status.success(), "bd create: {}", stderr_of(&out));
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("bd create json");
        value["id"].as_str().expect("created id").to_string()
    }

    fn work_once(&self) -> Output {
        Command::new(bin())
            .args([
                "work",
                "--repo",
                self.origin.to_str().expect("origin utf8"),
                "--state-dir",
                self.state_dir.to_str().expect("state utf8"),
                "--once",
            ])
            .env("AGALMA_ACTIVITY_MODE", "scripted")
            .output()
            .expect("run agalma work")
    }

    fn gc(&self) -> Output {
        Command::new(bin())
            .args([
                "gc",
                "--state-dir",
                self.state_dir.to_str().expect("state utf8"),
            ])
            .output()
            .expect("run agalma gc")
    }
}

/// Recreate a heavy scratch subdir (deleted by the terminal run) with a blob.
fn recreate_heavy(root: &Path, sub: &str) {
    fs::create_dir_all(root.join(sub)).expect("heavy dir");
    fs::write(root.join(sub).join("blob.bin"), vec![0u8; 4096]).expect("heavy file");
}

#[test]
fn gc_reaps_terminal_leftovers_and_keeps_forensics() {
    let (scratch, id) = Scratch::new("reap");
    let exec = ExecutionId::new(format!("exec:{id}:1"));
    let root = run_root(&scratch.state_dir, &exec);

    let out = scratch.work_once();
    assert!(
        out.status.success(),
        "green work run: {}\n{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    // The terminal run reaps on the way out; recreate heavy scratch to sweep.
    assert!(!root.join("checkout").exists(), "work reaped the checkout");
    recreate_heavy(&root, "checkout");
    recreate_heavy(&root, "agent-target");

    let gc = scratch.gc();
    assert!(gc.status.success(), "gc exits 0: {}", stderr_of(&gc));
    assert!(
        !root.join("checkout").exists() && !root.join("agent-target").exists(),
        "gc reaped the recreated heavy scratch"
    );
    assert!(
        root.join("artifacts").join("digest.json").exists(),
        "forensic digest survives the sweep"
    );
    assert!(
        stdout_of(&gc).contains("agalma gc: reaped"),
        "total line printed: {}",
        stdout_of(&gc)
    );
}

#[test]
fn gc_second_run_reports_nothing_to_reap() {
    let (scratch, id) = Scratch::new("idempotent");
    let exec = ExecutionId::new(format!("exec:{id}:1"));
    let root = run_root(&scratch.state_dir, &exec);

    let out = scratch.work_once();
    assert!(out.status.success(), "work: {}", stderr_of(&out));
    recreate_heavy(&root, "agent-target");

    let first = scratch.gc();
    assert!(first.status.success(), "first gc: {}", stderr_of(&first));
    assert!(
        stdout_of(&first).contains("agalma gc: reaped"),
        "first run reaps: {}",
        stdout_of(&first)
    );
    assert!(!root.join("agent-target").exists(), "heavy dir gone");

    let second = scratch.gc();
    assert!(second.status.success(), "second gc: {}", stderr_of(&second));
    assert!(
        stdout_of(&second).contains("agalma gc: nothing to reap"),
        "idempotent second run: {}",
        stdout_of(&second)
    );
    assert!(
        root.join("artifacts").join("digest.json").exists(),
        "forensics still kept on the idempotent run"
    );
}

#[test]
fn gc_refuses_while_a_conductor_holds_the_lock() {
    let dir = ScratchDir::new("gc-lock");
    let state = dir.path().join("state");
    fs::create_dir_all(&state).expect("state dir");
    let state = fs::canonicalize(&state).expect("canonical state");

    // Hold the state-dir lock in this process; a live pid refuses gc.
    let _lock = LockGuard::acquire(&state).expect("acquire lock");

    let args = GcArgs {
        state_dir: Some(state.clone()),
    };
    assert_eq!(
        gc_command(&args),
        ExitCode::FAILURE,
        "gc must refuse while the lock is held"
    );
}
