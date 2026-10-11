//! Terminal run-dir reaping (`agalma-36y`).
//!
//! Unit-level: [`reap_execution`] deletes exactly the heavy scratch set under
//! `runs/<exec>`, keeps the forensic evidence, refuses to touch a run dir with a
//! live `*.worker.json` record, and unlinks (never follows) symlinked scratch
//! entries.
//!
//! End-to-end: a scripted `agalma work --once` green run reaps the checkout on
//! the way out while keeping `artifacts/digest.json` and `stop/` evidence; the
//! boot reap sweep is idempotent on the next run. Scratch lives under
//! `target/test-runs/reap-*` (never `/tmp`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agalma_conductor::phases::run_root;
use agalma_conductor::reap::{reap_execution, REAP_SUBDIRS};
use agalma_contracts::ExecutionId;
use agalma_testkit::ScratchDir;

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

// ---------------------------------------------------------------------------
// Unit-level reaping
// ---------------------------------------------------------------------------

/// Fabricate a run root with every heavy scratch dir plus the kept forensics.
fn fabricate(state: &Path, exec: &ExecutionId) -> PathBuf {
    let root = run_root(state, exec);
    for sub in REAP_SUBDIRS {
        let dir = root.join(sub);
        fs::create_dir_all(&dir).expect("heavy dir");
        fs::write(dir.join("blob.bin"), vec![0u8; 4096]).expect("heavy file");
    }
    // Kept forensics.
    fs::create_dir_all(root.join("artifacts")).expect("artifacts");
    fs::write(root.join("artifacts").join("digest.json"), b"{\"ok\":true}").expect("digest");
    fs::write(root.join("artifacts").join("accept@1-1.txt"), b"green").expect("accept");
    fs::create_dir_all(root.join("stop")).expect("stop");
    fs::write(root.join("stop").join("build-1.json"), b"{\"gone\":true}").expect("stop evidence");
    fs::create_dir_all(root.join("build")).expect("build markers");
    fs::write(root.join("build").join("1.done"), b"{}").expect("build done");
    fs::create_dir_all(root.join("intake")).expect("intake markers");
    fs::write(root.join("checkout.json"), b"{}").expect("checkout handle");
    fs::write(root.join("effects.log"), b"build@1\nverify@1\n").expect("effects");
    fs::create_dir_all(root.join("prompts")).expect("prompts");
    fs::create_dir_all(root.join("opencode-log-abc")).expect("log dir");
    fs::write(root.join("opencode-log-abc").join("opencode.log"), b"log").expect("log file");
    root
}

#[test]
fn reap_removes_exactly_the_heavy_set_and_keeps_forensics() {
    let dir = fresh("reap-unit-heavy");
    let state = dir.join("state");
    fs::create_dir_all(&state).expect("state");
    let exec = ExecutionId::new("exec:reap:1");
    let root = fabricate(&state, &exec);

    let report = reap_execution(&state, &exec);
    assert!(report.skipped.is_none(), "not skipped: {report:?}");
    assert_eq!(
        report.removed.len(),
        REAP_SUBDIRS.len(),
        "every heavy dir removed: {:?}",
        report.removed
    );
    assert!(
        report.bytes >= (REAP_SUBDIRS.len() as u64) * 4096,
        "bytes accounted: {}",
        report.bytes
    );
    for sub in REAP_SUBDIRS {
        assert!(
            !root.join(sub).exists(),
            "heavy dir {sub} must be gone: {}",
            root.join(sub).display()
        );
    }

    // Forensics survive.
    assert!(root.join("artifacts").join("digest.json").exists());
    assert!(root.join("artifacts").join("accept@1-1.txt").exists());
    assert!(root.join("stop").join("build-1.json").exists());
    assert!(root.join("build").join("1.done").exists());
    assert!(root.join("intake").is_dir());
    assert!(root.join("checkout.json").exists());
    assert!(root.join("effects.log").exists());
    assert!(root.join("prompts").is_dir());
    assert!(root.join("opencode-log-abc").join("opencode.log").exists());
}

#[test]
fn reap_is_idempotent_on_an_already_reaped_run_dir() {
    let dir = fresh("reap-unit-idempotent");
    let state = dir.join("state");
    fs::create_dir_all(&state).expect("state");
    let exec = ExecutionId::new("exec:reap:2");
    fabricate(&state, &exec);

    let first = reap_execution(&state, &exec);
    assert_eq!(first.removed.len(), REAP_SUBDIRS.len());

    let second = reap_execution(&state, &exec);
    assert!(
        second.removed.is_empty(),
        "nothing left to reap: {second:?}"
    );
    assert_eq!(second.bytes, 0);
}

#[test]
fn live_worker_record_skips_the_reap() {
    let dir = fresh("reap-unit-worker");
    let state = dir.join("state");
    fs::create_dir_all(&state).expect("state");
    let exec = ExecutionId::new("exec:reap:3");
    let root = fabricate(&state, &exec);

    fs::create_dir_all(root.join("build")).expect("build dir");
    fs::write(root.join("build").join("1.worker.json"), b"{\"pid\":1}").expect("worker record");

    let report = reap_execution(&state, &exec);
    assert!(report.skipped.is_some(), "skipped: {report:?}");
    assert!(report.removed.is_empty(), "nothing removed");
    assert_eq!(report.bytes, 0);
    for sub in REAP_SUBDIRS {
        assert!(
            root.join(sub).exists(),
            "heavy dir {sub} must survive a skip"
        );
    }
}

#[test]
fn symlinked_scratch_is_unlinked_not_followed() {
    let dir = fresh("reap-unit-symlink");
    let state = dir.join("state");
    fs::create_dir_all(&state).expect("state");
    let exec = ExecutionId::new("exec:reap:4");
    let root = run_root(&state, &exec);
    fs::create_dir_all(&root).expect("run root");

    // A real directory outside the run root, reachable only through the link.
    let outside = state.join("outside");
    fs::create_dir_all(&outside).expect("outside");
    fs::write(outside.join("keep.txt"), b"precious").expect("keep file");
    std::os::unix::fs::symlink(&outside, root.join("checkout")).expect("symlink");

    let report = reap_execution(&state, &exec);
    assert!(
        report.removed.contains(&"checkout".to_string()),
        "symlink unlinked: {:?}",
        report.removed
    );
    assert!(
        fs::symlink_metadata(root.join("checkout")).is_err(),
        "the link itself is gone"
    );
    assert!(
        outside.join("keep.txt").exists(),
        "the symlink target must survive"
    );
}

// ---------------------------------------------------------------------------
// End-to-end reaping over a scripted work run
// ---------------------------------------------------------------------------

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

/// A scratch origin repo (git + isolated bd store) with one task issue,
/// mirroring `tests/work.rs`'s `Scratch`.
struct Scratch {
    _dir: ScratchDir,
    origin: PathBuf,
    state_dir: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> (Scratch, String) {
        require_bd();
        let dir = fresh(&format!("reap-{name}"));
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
        let state_dir = dir.join("state");
        let scratch = Scratch {
            _dir: dir,
            origin,
            state_dir,
        };
        let id = scratch.create("Reap task", description);
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
        Command::new(env!("CARGO_BIN_EXE_agalma"))
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
}

#[test]
fn terminal_work_reaps_heavy_scratch_and_boot_sweep_is_idempotent() {
    let (scratch, id) = Scratch::new("e2e");
    let exec = ExecutionId::new(format!("exec:{id}:1"));
    let root = run_root(&scratch.state_dir, &exec);

    let out = scratch.work_once();
    assert!(
        out.status.success(),
        "green work run: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        stderr_of(&out)
    );

    // Heavy scratch is gone; the checkout clone is the heavyweight witness.
    assert!(
        !root.join("checkout").exists(),
        "checkout reaped on terminal: {}",
        root.join("checkout").display()
    );
    // Forensics intact.
    assert!(
        root.join("artifacts").join("digest.json").exists(),
        "digest artifact kept"
    );
    let digest = fs::read_to_string(root.join("artifacts").join("digest.json")).expect("digest");
    assert!(digest.contains("verdict"), "digest is real: {digest}");

    // Fabricate a fresh stop-evidence file plus a heavy dir; a second run with
    // no ready tasks triggers the boot sweep, which must reap the heavy dir
    // again while leaving the evidence.
    fs::create_dir_all(root.join("stop")).expect("stop dir");
    fs::write(root.join("stop").join("build-1.json"), b"{\"gone\":true}").expect("stop evidence");
    fs::create_dir_all(root.join("agent-target")).expect("agent target");
    fs::write(root.join("agent-target").join("blob.bin"), vec![0u8; 1024]).expect("blob");

    let again = scratch.work_once();
    assert!(
        again.status.success(),
        "second (idle) work run: {}",
        stderr_of(&again)
    );
    assert!(
        !root.join("agent-target").exists(),
        "boot sweep reaps leftover heavy scratch"
    );
    assert!(
        root.join("stop").join("build-1.json").exists(),
        "stop evidence survives the sweep"
    );
    assert!(
        root.join("artifacts").join("digest.json").exists(),
        "digest survives the sweep"
    );
}
