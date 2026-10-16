//! Deterministic integration tests for the M1 queue-driven `agalma work` loop.
//!
//! Each test runs against its own scratch origin repository under
//! `target/test-runs/work-*`: a real `git` origin seeded with a small source
//! file and a real isolated `bd` store. Model calls are replaced by the
//! conductor's scripted activity mode (`AGALMA_ACTIVITY_MODE=scripted`); the
//! acceptance command list still runs, deterministically, via the in-process
//! runner. No `/tmp`, no network. Scratch dirs auto-remove on success (kept on
//! failure for forensics).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, ProtectedInputs, PROTECTED_BASELINE_FORMAT,
    PROTECTED_INPUTS_FORMAT,
};
use agalma_contracts::digest::sha256_digest;
use agalma_testkit::ScratchDir;
use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

fn stderr_of(out: &Output) -> String {
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

// ---------------------------------------------------------------------------
// SF.6 operator protected inputs (work-local fixture seeding)
//
// Repository work is fail-closed: `agalma work --once` refuses unless the
// operator `ProtectedInputs/v1` manifest (`<state_dir>/protected-inputs.json`)
// carries an entry whose task/repository/base identity matches the fixture and
// freezes a complete `ProtectedEntry` (including a valid baseline). The work
// fixture seeds that locally, without touching the shared `tests/common`
// helpers.
// ---------------------------------------------------------------------------

/// An immutable protected *behavioral* judge: it independently re-checks the
/// scripted candidate's fixed behavior (`answer() == 42`) against the pinned
/// candidate tree. Frozen as a `ProtectedBaseline/v1` file and launched through
/// `$AGALMA_PROTECTED_DIR` (outside the candidate writable root), so the
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

/// Write an operator `ProtectedInputs/v1` manifest into `state_dir`
/// (canonicalized like `WorkArgs::state_dir`), the admission source production
/// reads at `<state_dir>/protected-inputs.json`.
fn write_protected_inputs(state_dir: &Path, entries: Vec<ProtectedEntry>) {
    let inputs = ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries,
    };
    let bytes = serde_json::to_vec(&inputs).expect("serialize protected inputs");
    fs::create_dir_all(state_dir).expect("state dir");
    let state_dir = fs::canonicalize(state_dir).expect("canonical state dir");
    fs::write(state_dir.join("protected-inputs.json"), bytes).expect("write protected inputs");
}

/// A scratch origin repo (git + isolated bd store) with one task issue.
struct Scratch {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Scratch {
    /// A scratch fixture with a matching operator manifest seeded (the normal
    /// case: repository work reaches verification).
    ///
    /// `acceptance` is the fenced-block acceptance list body (already indented).
    fn new(name: &str, acceptance: &str, max_attempts: u32) -> (Scratch, String) {
        let (scratch, id) = Scratch::new_raw(name, acceptance, max_attempts);
        scratch.seed_protected_inputs(&[&id]);
        (scratch, id)
    }

    /// A scratch fixture with NO operator manifest (for missing-input refusal
    /// coverage).
    fn new_raw(name: &str, acceptance: &str, max_attempts: u32) -> (Scratch, String) {
        require_bd();
        let dir = fresh(&format!("work-{name}"));
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

        // bd initializes a local store because cwd is the scratch repo.
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

        let description = format!(
            "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n{acceptance}\npriority: normal\nfamily: fix\nmax_attempts: {max_attempts}\n```\n"
        );
        let state_dir = dir.join("state");
        let scratch = Scratch {
            origin,
            state_dir,
            _dir: dir,
        };
        let id = scratch.create("Scratch task", &description);
        (scratch, id)
    }

    fn beads(&self) -> PathBuf {
        self.origin.join(".beads")
    }

    fn bd(&self, args: &[&str]) -> Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.beads())
            .args(args)
            .current_dir(&self.origin)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn create(&self, title: &str, description: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "task",
            "-p",
            "2",
            "--description",
            description,
            "--json",
        ]);
        assert!(out.status.success(), "bd create: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("bd create json");
        value["id"].as_str().expect("created id").to_string()
    }

    fn show(&self, id: &str) -> Value {
        let out = self.bd(&["show", id, "--json", "--include-comments"]);
        assert!(out.status.success(), "bd show: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("bd show json");
        value
            .as_array()
            .expect("show array")
            .first()
            .expect("one issue")
            .clone()
    }

    fn work(&self, once: bool) -> Output {
        self.work_with(once, None)
    }

    fn work_with(&self, once: bool, max_tasks: Option<u32>) -> Output {
        let mut command = Command::new(bin());
        command.args([
            "work",
            "--repo",
            self.origin.to_str().expect("origin utf8"),
            "--state-dir",
            self.state_dir.to_str().expect("state utf8"),
        ]);
        if once {
            command.arg("--once");
        }
        if let Some(max) = max_tasks {
            command.arg("--max-tasks").arg(max.to_string());
        }
        command
            .env("AGALMA_ACTIVITY_MODE", "scripted")
            .output()
            .expect("run agalma work")
    }

    /// Create another task issue with an explicit priority and acceptance.
    fn create_task(&self, title: &str, priority: &str, acceptance: &str) -> String {
        let description = format!(
            "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n{acceptance}\npriority: {priority}\nfamily: fix\nmax_attempts: 2\n```\n"
        );
        self.create(title, &description)
    }

    fn git_origin(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.origin)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
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
        self.git_origin(&["rev-parse", "main"])
    }

    /// The protected entry whose identity matches this scratch fixture.
    fn matching_entry(&self, task_id: &str) -> ProtectedEntry {
        protected_entry(task_id, &self.canonical_origin(), &self.base_sha())
    }

    /// Write an operator `ProtectedInputs/v1` manifest into the isolated state
    /// directory production reads.
    fn write_manifest(&self, entries: Vec<ProtectedEntry>) {
        write_protected_inputs(&self.state_dir, entries);
    }

    /// Seed a valid manifest covering every task id that may be admitted.
    fn seed_protected_inputs(&self, task_ids: &[&str]) {
        let entries = task_ids
            .iter()
            .map(|id| self.matching_entry(id))
            .collect();
        self.write_manifest(entries);
    }
}

fn comments_text(issue: &Value) -> String {
    issue["comments"]
        .as_array()
        .map(|comments| {
            comments
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------

#[test]
fn happy_path_closes_task_and_merges_origin() {
    let (scratch, id) = Scratch::new(
        "happy",
        "  - sh -c 'test -f src/lib.rs'\n  - sh -c 'grep -q 42 src/lib.rs'",
        2,
    );

    let out = scratch.work(true);
    assert!(
        out.status.success(),
        "work --once should succeed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        stderr_of(&out)
    );

    // Projection: phase comments with the execution id, and the task closed.
    let issue = scratch.show(&id);
    assert_eq!(issue["status"], "closed", "issue closed on done: {issue}");
    let text = comments_text(&issue);
    let execution = format!("exec:{id}:1");
    for phase in ["intake", "checkout", "build", "verify", "integrate", "done"] {
        assert!(
            text.contains(&format!("phase={phase}")),
            "missing phase={phase} comment: {text}"
        );
    }
    assert!(
        text.contains(&format!("execution={execution}")),
        "execution id projected: {text}"
    );

    // Integration: origin main advanced to the candidate and is tagged m1/<exec>.
    let main = scratch.git_origin(&["rev-parse", "refs/heads/main"]);
    let seed = scratch.git_origin(&["rev-parse", "refs/heads/main~1"]);
    assert_ne!(main, seed, "origin main advanced");
    let tag = format!("m1/exec-{id}-1");
    assert_eq!(
        scratch.git_origin(&["rev-parse", &format!("refs/tags/{tag}")]),
        main,
        "m1 tag points at merged main"
    );
}

#[test]
fn red_acceptance_retries_then_parks() {
    let (scratch, id) = Scratch::new("red", "  - sh -c 'grep -q MAGIC src/lib.rs'", 2);
    let before = scratch.git_origin(&["rev-parse", "refs/heads/main"]);

    let out = scratch.work(true);
    assert!(
        !out.status.success(),
        "--once exits nonzero on park: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let issue = scratch.show(&id);
    let labels: Vec<&str> = issue["labels"]
        .as_array()
        .map(|l| l.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(labels.contains(&"parked"), "parked label: {issue}");
    let text = comments_text(&issue);
    assert!(
        text.contains("max_attempts"),
        "park reason cites the attempt bound: {text}"
    );

    // Two attempts ran (bounded retry), and the origin was never merged.
    assert_eq!(
        scratch.git_origin(&["rev-parse", "refs/heads/main"]),
        before,
        "origin main unchanged on red"
    );
    assert!(
        scratch
            .git_origin(&["tag", "-l", &format!("m1/exec-{id}-1")])
            .is_empty(),
        "no integration tag on red"
    );

    // The failed acceptance output is captured as an artifact.
    let run_root = scratch.state_dir.join("runs").join(format!("exec-{id}-1"));
    let failure = run_root.join("artifacts").join("failure@1.txt");
    assert!(
        failure.exists(),
        "failure artifact captured: {}",
        failure.display()
    );
    let captured = fs::read_to_string(&failure).expect("read failure");
    assert!(
        captured.contains("MAGIC"),
        "captured failing command: {captured}"
    );
}

#[test]
fn triage_picks_higher_priority_and_max_tasks_bounds() {
    let (scratch, normal_id) = Scratch::new("triage", "  - sh -c 'grep -q 42 src/lib.rs'", 2);
    let critical_id =
        scratch.create_task("Critical", "critical", "  - sh -c 'grep -q 42 src/lib.rs'");

    // Seed the operator manifest for both eligible tasks (triage may admit
    // either; the picked one needs a matching protected entry).
    scratch.seed_protected_inputs(&[&normal_id, &critical_id]);

    // Two eligible tasks → static `triage.pick-next` picks the critical one;
    // `--max-tasks 1` bounds the loop to a single task.
    let out = scratch.work_with(false, Some(1));
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(
        scratch.show(&critical_id)["status"],
        "closed",
        "critical task chosen by triage"
    );
    assert_eq!(
        scratch.show(&normal_id)["status"],
        "open",
        "--max-tasks bounds the loop"
    );
}

#[test]
fn routing_pins_model_and_records_route_decision_and_digest() {
    let (scratch, id) = Scratch::new("routing", "  - sh -c 'grep -q 42 src/lib.rs'", 2);
    let out = scratch.work(true);
    assert!(out.status.success(), "green run: {}", stderr_of(&out));

    // The resolved free route is recorded as a `route.model` decision and the
    // builder attempt pins that model in the usage event.
    let execution = format!("exec:{id}:1");
    let conn =
        rusqlite::Connection::open(scratch.state_dir.join("ledger.sqlite")).expect("open ledger");
    let route_decisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM decisions WHERE kind='route.model'",
            [],
            |row| row.get(0),
        )
        .expect("count route decisions");
    assert!(route_decisions >= 1, "route.model decision recorded");

    let builder_model: String = conn
        .query_row(
            "SELECT json_extract(payload,'$.model') FROM execution_events \
             WHERE execution_id=?1 AND kind='usage' AND json_extract(payload,'$.role')='builder' \
             LIMIT 1",
            [&execution],
            |row| row.get(0),
        )
        .expect("builder usage model");
    assert_eq!(
        builder_model, "opencode/big-pickle",
        "attempt pinned the route"
    );

    // Digest records the model and the route label.
    let digest_path = scratch
        .state_dir
        .join("runs")
        .join(format!("exec-{id}-1"))
        .join("artifacts")
        .join("digest.json");
    let digest: Value =
        serde_json::from_str(&fs::read_to_string(&digest_path).expect("read digest"))
            .expect("digest json");
    assert_eq!(digest["model"], "opencode/big-pickle");
    assert_eq!(digest["route"], "builder:fix");
    assert!(digest["cost"]["usd"].as_f64().unwrap() == 0.0);
}

#[test]
fn generation_increments_on_rerun() {
    let (scratch, id) = Scratch::new("generation", "  - sh -c 'grep -q 42 src/lib.rs'", 2);

    let first = scratch.work(true);
    assert!(first.status.success(), "first run: {}", stderr_of(&first));

    // Reopen the closed issue and run again: the execution generation advances.
    let reopened = scratch.bd(&["reopen", &id]);
    assert!(
        reopened.status.success(),
        "reopen: {}",
        stderr_of(&reopened)
    );
    // The first run merged the candidate, advancing `main`; the rerun resolves
    // the new base, so re-seed the operator manifest against it.
    scratch.seed_protected_inputs(&[&id]);
    let second = scratch.work(true);
    assert!(
        second.status.success(),
        "second run: {}",
        stderr_of(&second)
    );

    let issue = scratch.show(&id);
    let text = comments_text(&issue);
    assert!(
        text.contains(&format!("execution=exec:{id}:1")),
        "generation 1 recorded: {text}"
    );
    assert!(
        text.contains(&format!("execution=exec:{id}:2")),
        "generation 2 recorded: {text}"
    );
}

/// Missing and identity-mismatched operator protected inputs continue to refuse
/// repository work over the real `agalma work --once` path (SF.6 fail-closed):
/// an absent manifest never authorizes repository work, and a present manifest
/// whose task/repository/base identity does not match the fixture refuses.
#[test]
fn work_refuses_missing_or_mismatched_protected_inputs() {
    // Missing manifest: an absent operator manifest never authorizes work.
    let (missing, _id) = Scratch::new_raw("refuse-missing", "  - sh -c 'true'", 1);
    let out = missing.work(true);
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
    let (mismatch, id) = Scratch::new_raw("refuse-mismatch", "  - sh -c 'true'", 1);
    let wrong = protected_entry(&id, &mismatch.canonical_origin(), &"0".repeat(40));
    mismatch.write_manifest(vec![wrong]);
    let out = mismatch.work(true);
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

/// Deterministic generated-input property (seed `0x5f6_work`, case budget 48)
/// over the operator fixture identity/manifest contract, derived independently
/// of the implementation:
/// - a generated `ProtectedEntry` freezes and re-parses unchanged;
/// - its `ProtectedInputs/v1` manifest loads from the admission source;
/// - selection with the exact task/repository/base identity returns the frozen
///   entry;
/// - changing any single identity field refuses selection.
/// Boundary/invalid cases (empty identity, empty/broken baseline, bad digest or
/// mode, duplicate files, wrong format, empty manifest, absent file) all refuse.
#[test]
fn work_protected_fixture_seeding_property_over_generated_cases() {
    // Design seed label `0x5f6_work`; the non-hex letters are their ASCII bytes.
    const SEED: u64 = 0x5f6_776f_726b;
    const CASES: usize = 48;

    const TASKS: [&str; 4] = ["task.a", "task-b", "task.c", "t"];
    const REPOS: [&str; 4] = ["/repo/one", "/repo two", "/r", "/deep/nested/origin"];
    const ALLOWED: [&str; 4] = ["src", "crates", "a/b", "src/lib.rs"];
    const PROTECTED: [&str; 3] = ["tests", "docs/protected.md", "baseline"];

    let dir = fresh("work-fixture-property");
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

        let frozen = entry.freeze().expect("freeze generated entry");
        assert_eq!(
            ProtectedEntry::parse(&frozen).expect("re-parse frozen entry"),
            entry,
            "freeze must round-trip"
        );

        write_protected_inputs(&state_dir, vec![entry.clone()]);
        let inputs = ProtectedInputs::load(&state_dir).expect("generated manifest loads");
        let selected = inputs
            .select(&entry.task_id, &entry.repository_root, &entry.base_sha)
            .expect("exact identity selects");
        assert_eq!(selected, &entry);
        assert_eq!(selected.freeze().expect("freeze selected"), frozen);

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

    assert!(!entry_accepts(&good, |e| e.task_id = "  ".to_string()));
    assert!(!entry_accepts(&good, |e| e.repository_root = String::new()));
    assert!(!entry_accepts(&good, |e| e.base_sha = String::new()));

    assert!(!entry_accepts(&good, |e| e.baseline.files.clear()));
    assert!(!entry_accepts(&good, |e| e.baseline.commands.clear()));
    assert!(!entry_accepts(&good, |e| e.baseline.commands = vec!["  ".to_string()]));

    assert!(!entry_accepts(&good, |e| e.baseline.files[0].digest = sha256_digest(b"other")));
    assert!(!entry_accepts(&good, |e| e.baseline.files[0].mode = "100600".to_string()));
    assert!(!entry_accepts(&good, |e| {
        let dup = e.baseline.files[0].clone();
        e.baseline.files.push(dup);
    }));

    let absent = fresh("work-fixture-absent");
    assert!(ProtectedInputs::load(&absent).is_err());
}
