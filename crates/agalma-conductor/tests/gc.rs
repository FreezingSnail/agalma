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
use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, ProtectedInputs, PROTECTED_BASELINE_FORMAT,
    PROTECTED_INPUTS_FORMAT,
};
use agalma_conductor::{gc_command, LockGuard};
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::ExecutionId;
use agalma_testkit::ScratchDir;
use serde_json::json;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(out.status.success(), "git {args:?}: {}", stderr_of(&out));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// SF.6 operator protected inputs (gc-local fixture seeding)
//
// Repository work is fail-closed: `agalma work --once` refuses unless the
// operator `ProtectedInputs/v1` manifest (`<state_dir>/protected-inputs.json`)
// carries an entry whose task/repository/base identity matches the fixture and
// freezes a complete `ProtectedEntry` (including a valid baseline). The gc
// fixture seeds that locally, without touching shared `tests/common` helpers.
// ---------------------------------------------------------------------------

/// An immutable protected *behavioral* judge: it independently re-checks the
/// scripted candidate's fixed behavior (`answer() == 42`) against the pinned
/// candidate tree. It is frozen as a `ProtectedBaseline/v1` file and launched
/// through `$AGALMA_PROTECTED_DIR` (outside the candidate writable root), so the
/// candidate cannot substitute its own always-pass tests for the judge.
const PROTECTED_RUNNER: &str = "#!/bin/sh\n\
set -eu\n\
grep -q 42 \"$AGALMA_CANDIDATE_DIR/src/lib.rs\"\n";
/// The frozen protected baseline command (runs the runner through the sandbox).
const PROTECTED_COMMAND: &str = "sh \"$AGALMA_PROTECTED_DIR/runner.sh\"";

/// The frozen `ProtectedBaseline/v1` (one executable file + one command).
fn protected_baseline() -> ProtectedBaseline {
    ProtectedBaseline {
        format: PROTECTED_BASELINE_FORMAT.to_string(),
        files: vec![BaselineFile {
            path: "runner.sh".to_string(),
            mode: "100755".to_string(),
            digest: sha256_digest(PROTECTED_RUNNER.as_bytes()),
            bytes: PROTECTED_RUNNER.len() as u64,
            text: PROTECTED_RUNNER.to_string(),
        }],
        commands: vec![PROTECTED_COMMAND.to_string()],
    }
}

/// The operator `ProtectedEntry` whose identity must match a scratch fixture
/// exactly: the created bd task id, the canonicalized origin path, and the
/// resolved `main` base SHA. `allowed_paths` admits the scripted candidate
/// change (`src/lib.rs`); nothing the candidate touches is protected.
fn protected_entry(task_id: &str, repository_root: &str, base_sha: &str) -> ProtectedEntry {
    ProtectedEntry {
        task_id: task_id.to_string(),
        repository_root: repository_root.to_string(),
        base_sha: base_sha.to_string(),
        allowed_paths: vec!["src".to_string()],
        protected_paths: Vec::new(),
        baseline: protected_baseline(),
    }
}

/// Isolated XDG config base for a scratch fixture.
fn config_home(state_dir: &Path) -> PathBuf {
    state_dir.parent().expect("state parent").join("xdg-config")
}

/// Write an operator `ProtectedInputs/v1` manifest at the isolated XDG config
/// path production reads.
fn write_protected_inputs(state_dir: &Path, entries: Vec<ProtectedEntry>) {
    let inputs = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries,
    };
    let bytes = serde_json::to_vec(&inputs).expect("serialize protected inputs");
    let dir = config_home(state_dir).join("agalma");
    fs::create_dir_all(&dir).expect("config dir");
    fs::write(dir.join("protected-inputs.json"), bytes).expect("write protected inputs");
}

/// Low-level explicit-directory loader fixture (not the production source).
fn write_low_level_inputs(state_dir: &Path, entries: Vec<ProtectedEntry>) {
    let inputs = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries,
    };
    let bytes = serde_json::to_vec(&inputs).expect("serialize protected inputs");
    fs::create_dir_all(state_dir).expect("state dir");
    fs::write(state_dir.join("protected-inputs.json"), bytes).expect("write protected inputs");
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
    /// A scratch fixture with a matching operator manifest seeded (the normal
    /// case: repository work reaches verification).
    fn new(name: &str) -> (Scratch, String) {
        let (scratch, id) = Scratch::new_raw(name);
        scratch.seed_protected_inputs(&id);
        (scratch, id)
    }

    /// A scratch fixture with NO operator manifest (for missing-input refusal
    /// coverage).
    fn new_raw(name: &str) -> (Scratch, String) {
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

    /// The canonical origin path (production canonicalizes `--repo`).
    fn canonical_origin(&self) -> String {
        fs::canonicalize(&self.origin)
            .expect("canonical origin path")
            .to_string_lossy()
            .into_owned()
    }

    /// The resolved `main` base SHA (production resolves `ref: main`).
    fn base_sha(&self) -> String {
        git_out(&self.origin, &["rev-parse", "main"])
    }

    /// The protected entry whose identity matches this scratch fixture.
    fn matching_entry(&self, task_id: &str) -> ProtectedEntry {
        protected_entry(task_id, &self.canonical_origin(), &self.base_sha())
    }

    /// Write an operator `ProtectedInputs/v1` manifest at the isolated XDG
    /// config path production reads.
    fn write_manifest(&self, entries: Vec<ProtectedEntry>) {
        write_protected_inputs(&self.state_dir, entries);
    }

    /// Seed a valid manifest whose identity matches the created task.
    fn seed_protected_inputs(&self, task_id: &str) {
        self.write_manifest(vec![self.matching_entry(task_id)]);
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
            .env("XDG_CONFIG_HOME", config_home(&self.state_dir))
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

/// Missing and identity-mismatched operator protected inputs continue to refuse
/// repository work over the real `agalma work --once` path (SF.6 fail-closed):
/// an absent manifest never authorizes repository work, and a present manifest
/// whose task/repository/base identity does not match the fixture refuses.
#[test]
fn gc_work_refuses_missing_or_mismatched_protected_inputs() {
    // Missing manifest: an absent operator manifest never authorizes work.
    let (missing, _id) = Scratch::new_raw("refuse-missing");
    let out = missing.work_once();
    assert!(
        !out.status.success(),
        "missing manifest must refuse repository work: {}",
        stderr_of(&out)
    );
    let err = stderr_of(&out);
    assert!(
        err.contains("protected-inputs.json"),
        "refusal names the manifest: {err}"
    );
    assert!(err.contains("ScopeDenied"), "refusal is a scope denial: {err}");

    // Identity mismatch: a present manifest whose base_sha does not match the
    // resolved base refuses (task/repository/base identity is validated).
    let (mismatch, id) = Scratch::new_raw("refuse-mismatch");
    let wrong = protected_entry(&id, &mismatch.canonical_origin(), &"0".repeat(40));
    mismatch.write_manifest(vec![wrong]);
    let out = mismatch.work_once();
    assert!(
        !out.status.success(),
        "identity-mismatched manifest must refuse repository work: {}",
        stderr_of(&out)
    );
    assert!(
        stderr_of(&out).contains("ScopeDenied"),
        "mismatch is a scope denial: {}",
        stderr_of(&out)
    );
}

/// Whether `ProtectedEntry::parse` accepts a clone of `good` after `f` mutates
/// it (freeze then re-parse, the production canonical path).
fn entry_accepts(good: &ProtectedEntry, f: impl Fn(&mut ProtectedEntry)) -> bool {
    let mut entry = good.clone();
    f(&mut entry);
    ProtectedEntry::parse(&entry.freeze().expect("freeze entry")).is_ok()
}

/// Deterministic generated-input property (seed `0x5f6_gc`, case budget 64) over
/// the operator fixture identity/manifest contract, derived independently of the
/// implementation:
/// - a generated `ProtectedEntry` freezes and re-parses unchanged;
/// - its `ProtectedInputs/v1` manifest loads from the admission source;
/// - selection with the exact task/repository/base identity returns the frozen
///   entry;
/// - changing any single identity field refuses selection.
/// Boundary/invalid cases (empty identity, empty/broken baseline, bad digest or
/// mode, duplicate files, wrong format, empty manifest, absent file) all refuse.
#[test]
fn gc_protected_fixture_seeding_property_over_generated_cases() {
    // Design seed label `0x5f6_gc`; the non-hex `g`/`c` are the ASCII bytes
    // 0x67, 0x63.
    const SEED: u64 = 0x5f6_6763;
    const CASES: usize = 64;

    const TASKS: [&str; 4] = ["task.a", "task-b", "task.c", "t"];
    const REPOS: [&str; 4] = ["/repo/one", "/repo two", "/r", "/deep/nested/origin"];
    const ALLOWED: [&str; 4] = ["src", "crates", "a/b", "src/lib.rs"];
    const PROTECTED: [&str; 3] = ["tests", "docs/protected.md", "baseline"];

    let dir = ScratchDir::new("gc-fixture-property");
    let state_dir = dir.join("state");

    let mut rng = Rng(SEED);
    let mut checked = 0usize;
    for _ in 0..CASES {
        let task_id = format!(
            "{}.{}",
            TASKS[rng.below(TASKS.len() as u64) as usize],
            rng.below(8)
        );
        let repository_root = REPOS[rng.below(REPOS.len() as u64) as usize].to_string();
        let base_sha = rng_hex(&mut rng, 40);
        let allowed: Vec<String> = (0..1 + rng.below(3))
            .map(|_| ALLOWED[rng.below(ALLOWED.len() as u64) as usize].to_string())
            .collect();
        let protected: Vec<String> = (0..rng.below(2))
            .map(|_| PROTECTED[rng.below(PROTECTED.len() as u64) as usize].to_string())
            .collect();
        let files: Vec<BaselineFile> = (0..1 + rng.below(2))
            .map(|index| {
                let text = format!("#!/bin/sh\necho {}\n", rng.below(1_000_000));
                BaselineFile {
                    path: format!("bin{index}/check.sh"),
                    mode: if rng.below(2) == 0 { "100644" } else { "100755" }.to_string(),
                    digest: sha256_digest(text.as_bytes()),
                    bytes: text.len() as u64,
                    text,
                }
            })
            .collect();
        let commands: Vec<String> = (0..1 + rng.below(2))
            .map(|_| format!("sh \"$AGALMA_PROTECTED_DIR/{}\"", files[0].path))
            .collect();
        let entry = ProtectedEntry {
            task_id,
            repository_root,
            base_sha,
            allowed_paths: allowed,
            protected_paths: protected,
            baseline: ProtectedBaseline {
                format: PROTECTED_BASELINE_FORMAT.to_string(),
                files,
                commands,
            },
        };

        // Freeze then re-parse unchanged (production canonical path).
        let frozen = entry.freeze().expect("freeze generated entry");
        assert_eq!(
            ProtectedEntry::parse(&frozen).expect("re-parse frozen entry"),
            entry,
            "freeze must round-trip"
        );

        // Load the manifest through the low-level explicit-directory loader and
        // select the exact task/repository/base identity; the frozen bytes must
        // match.
        write_low_level_inputs(&state_dir, vec![entry.clone()]);
        let inputs = ProtectedInputs::load(&state_dir).expect("generated manifest loads");
        let selected = inputs
            .select(&entry.task_id, &entry.repository_root, &entry.base_sha)
            .expect("exact identity selects");
        assert_eq!(selected, &entry);
        assert_eq!(selected.freeze().expect("freeze selected"), frozen);

        // Any one changed identity field refuses selection.
        assert!(inputs
            .select(
                &format!("{}-x", entry.task_id),
                &entry.repository_root,
                &entry.base_sha
            )
            .is_err());
        assert!(inputs
            .select(
                &entry.task_id,
                &format!("{}-x", entry.repository_root),
                &entry.base_sha
            )
            .is_err());
        assert!(inputs
            .select(
                &entry.task_id,
                &entry.repository_root,
                &format!("{}-x", entry.base_sha)
            )
            .is_err());

        checked += 1;
    }
    assert_eq!(checked, CASES, "all generated cases executed");

    // Concrete refusal and boundary coverage over the manifest contract.
    let good = protected_entry("task.good", "/repo/origin", &"a".repeat(40));
    let bytes = |entries: Vec<ProtectedEntry>| {
        serde_json::to_vec(&ProtectedInputs {
            format: PROTECTED_INPUTS_FORMAT.to_string(),
            entries,
        })
        .expect("manifest bytes")
    };
    assert!(ProtectedInputs::parse(&bytes(vec![good.clone()])).is_ok());

    // Malformed / wrong-format / empty-entry / duplicate manifests refuse.
    assert!(ProtectedInputs::parse(b"not json").is_err());
    assert!(ProtectedInputs::parse(b"{}").is_err());
    assert!(ProtectedInputs::parse(&bytes(Vec::new())).is_err());
    assert!(ProtectedInputs::parse(&bytes(vec![good.clone(), good.clone()])).is_err());
    let wrong_format = serde_json::to_vec(&json!({
        "format": "ProtectedInputs/v2",
        "entries": [serde_json::to_value(&good).unwrap()],
    }))
    .expect("json");
    assert!(ProtectedInputs::parse(&wrong_format).is_err());

    // Empty identity fields refuse.
    assert!(!entry_accepts(&good, |e| e.task_id = "  ".to_string()));
    assert!(!entry_accepts(&good, |e| e.repository_root = String::new()));
    assert!(!entry_accepts(&good, |e| e.base_sha = String::new()));

    // Baseline boundaries: at least one file and one non-blank command.
    assert!(!entry_accepts(&good, |e| e.baseline.files.clear()));
    assert!(!entry_accepts(&good, |e| e.baseline.commands.clear()));
    assert!(!entry_accepts(&good, |e| e.baseline.commands = vec!["  ".to_string()]));

    // Baseline integrity and mode boundaries.
    assert!(!entry_accepts(&good, |e| e.baseline.files[0].digest = sha256_digest(b"other")));
    assert!(!entry_accepts(&good, |e| e.baseline.files[0].mode = "100600".to_string()));
    assert!(!entry_accepts(&good, |e| {
        let dup = e.baseline.files[0].clone();
        e.baseline.files.push(dup);
    }));

    // An absent admission source refuses load (never authorizes work).
    let absent = ScratchDir::new("gc-fixture-absent");
    assert!(ProtectedInputs::load(&absent).is_err());
}

/// Deterministic xorshift generator (same style as `digest.rs`).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn rng_hex(rng: &mut Rng, len: usize) -> String {
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        out.push(char::from_digit(rng.below(16) as u32, 16).expect("hex digit"));
    }
    out
}
