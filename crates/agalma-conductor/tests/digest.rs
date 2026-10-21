//! M1.4 acceptance runner + digest integration tests.
//!
//! Each test runs against its own scratch origin repository under
//! `target/test-runs/digest-*`: a real `git` origin with a real isolated `bd`
//! store. Model calls are replaced by scripted activity mode; the acceptance
//! command list runs deterministically via the in-process runner. No `/tmp`,
//! no network. Scratch dirs auto-remove on success (kept on failure).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agalma_conductor::scope_guard::{
    BaselineFile, ProtectedBaseline, ProtectedEntry, ProtectedInputs, PROTECTED_BASELINE_FORMAT,
    PROTECTED_INPUTS_FORMAT,
};
use agalma_contracts::digest::sha256_digest;
use agalma_contracts::ExecutionId;
use agalma_testkit::ScratchDir;
use agalma_workspace::Workspace;
use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

/// Isolated XDG config base for a scratch fixture.
fn config_home(state_dir: &Path) -> PathBuf {
    state_dir.parent().expect("state parent").join("xdg-config")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
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
// SF.6 operator protected inputs
// ---------------------------------------------------------------------------

/// Immutable protected *behavioral* judge fixture: a scripted runner that
/// independently re-checks the candidate's fixed behavior against the pinned
/// candidate tree. It is frozen as a `ProtectedBaseline/v1` file and launched
/// through `$AGALMA_PROTECTED_DIR`, i.e. outside the candidate writable root, so
/// the candidate cannot substitute its own always-pass tests for the judge.
const PROTECTED_RUNNER: &str = "#!/bin/sh\n\
set -eu\n\
grep -q 42 \"$AGALMA_CANDIDATE_DIR/src/lib.rs\"\n";
/// The protected baseline command (runs the frozen runner through the sandbox).
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

/// A scratch origin repo (git + isolated bd store) with one task issue.
struct Scratch {
    origin: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Scratch {
    /// A scratch fixture with a matching operator manifest seeded (the normal
    /// case: repository work reaches verification).
    fn new(name: &str, acceptance: &str, max_attempts: u32) -> (Scratch, String) {
        let (scratch, id) = Scratch::new_raw(name, acceptance, max_attempts);
        scratch.seed_protected_inputs(&id);
        (scratch, id)
    }

    /// A scratch fixture with NO operator manifest (for refusal coverage).
    fn new_raw(name: &str, acceptance: &str, max_attempts: u32) -> (Scratch, String) {
        require_bd();
        let dir = fresh(&format!("digest-{name}"));
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

        let description = format!(
            "Task body.\n\n```yaml\ndirective: directive/v0\ntarget: agalma\nref: main\nacceptance:\n{acceptance}\npriority: normal\nfamily: fix\nmax_attempts: {max_attempts}\n```\n"
        );
        let state_dir = dir.join("state");
        let scratch = Scratch {
            origin,
            state_dir,
            _dir: dir,
        };
        let id = scratch.create("Digest task", &description);
        (scratch, id)
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

    /// The canonical origin path (production canonicalizes `--repo`).
    fn canonical_origin(&self) -> String {
        std::fs::canonicalize(&self.origin)
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

    /// Write the operator `ProtectedInputs/v1` manifest at the isolated XDG
    /// config path production reads.
    fn write_manifest(&self, entries: Vec<ProtectedEntry>) {
        let inputs = ProtectedInputs {
            format: PROTECTED_INPUTS_FORMAT.to_string(),
            entries,
        };
        let bytes = serde_json::to_vec(&inputs).expect("serialize protected inputs");
        let dir = config_home(&self.state_dir).join("agalma");
        fs::create_dir_all(&dir).expect("config dir");
        fs::write(dir.join("protected-inputs.json"), bytes).expect("write protected inputs");
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

    fn run_root(&self, id: &str) -> PathBuf {
        self.state_dir.join("runs").join(format!("exec-{id}-1"))
    }

    fn digest_json(&self, id: &str) -> Value {
        let path = self.run_root(id).join("artifacts").join("digest.json");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read digest {}: {e}", path.display()));
        serde_json::from_str(&text).expect("digest json")
    }

    fn ledger(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state_dir.join("ledger.sqlite")).expect("open ledger")
    }

    fn digest_cli(&self, execution: &str) -> Output {
        Command::new(bin())
            .args([
                "digest",
                "--execution",
                execution,
                "--state-dir",
                self.state_dir.to_str().expect("state utf8"),
            ])
            .output()
            .expect("run agalma digest")
    }
}

// ---------------------------------------------------------------------------

#[test]
fn second_command_red_records_failing_command_and_per_command_artifacts() {
    let (scratch, id) = Scratch::new(
        "second-red",
        "  - sh -c 'grep -q 42 src/lib.rs'\n  - sh -c 'grep -q MAGIC src/lib.rs'",
        1,
    );

    let out = scratch.work_once();
    assert!(
        !out.status.success(),
        "--once exits nonzero on red: {}",
        stderr_of(&out)
    );

    // Every command has its own artifact, including the passing first command
    // and the frozen protected judge command that runs last.
    let artifacts = scratch.run_root(&id).join("artifacts");
    let first = artifacts.join("accept@1-0.txt");
    let second = artifacts.join("accept@1-1.txt");
    let protected = artifacts.join("accept@1-2.txt");
    assert!(
        first.exists(),
        "first command artifact: {}",
        first.display()
    );
    assert!(
        second.exists(),
        "second command artifact: {}",
        second.display()
    );
    assert!(
        protected.exists(),
        "protected judge artifact: {}",
        protected.display()
    );
    assert!(
        fs::read_to_string(&first).unwrap().contains("exit: 0"),
        "first command passed"
    );
    assert!(
        fs::read_to_string(&second).unwrap().contains("MAGIC"),
        "second command captured"
    );
    assert!(
        fs::read_to_string(&protected)
            .unwrap()
            .contains("runner.sh"),
        "protected judge captured"
    );

    // Aggregate verdict is red with the failing command listed. The frozen task
    // acceptance (two commands) is followed by the frozen protected baseline
    // command; the protected judge passes, so the red verdict is the failing
    // task acceptance, never the protected judge.
    let digest = scratch.digest_json(&id);
    assert_eq!(digest["verdict"], "red");
    let commands = digest["commands"].as_array().expect("commands array");
    assert_eq!(commands.len(), 3, "two task + one protected command: {digest}");
    assert_eq!(commands[0]["exit_code"], 0);
    assert_eq!(commands[1]["exit_code"], 1);
    assert_eq!(commands[2]["exit_code"], 0);
    assert!(
        commands[1]["command"]
            .as_str()
            .expect("command text")
            .contains("MAGIC"),
        "failing command captured: {digest}"
    );
    assert!(
        commands[2]["command"]
            .as_str()
            .expect("protected command text")
            .contains("runner.sh"),
        "protected judge command recorded last: {digest}"
    );
}

#[test]
fn verification_checkout_is_separate_and_pristine() {
    let dir = fresh("digest-pristine");
    let origin = dir.join("origin");
    fs::create_dir_all(origin.join("src")).expect("origin src");
    fs::write(
        origin.join("src").join("lib.rs"),
        "//! Seed source.\npub fn answer() -> u32 { 0 }\n",
    )
    .expect("seed source");
    fs::write(origin.join(".gitignore"), "IGNORED.txt\n").expect("gitignore");
    run_ok("git", &origin, &["init", "-q", "-b", "main"]);
    run_ok(
        "git",
        &origin,
        &["config", "user.email", "agalma@localhost"],
    );
    run_ok("git", &origin, &["config", "user.name", "Agalma"]);
    run_ok("git", &origin, &["add", "-A"]);
    run_ok("git", &origin, &["commit", "-qm", "seed"]);

    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let exec = ExecutionId::new("exec:pristine:1");
    let mut ws = Workspace::new(state_dir.clone());
    let checkout = ws
        .prepare_repo(&origin, "main", &exec, &state_dir)
        .expect("prepare_repo");

    // Dirty candidate edit plus an ignored untracked file that must never reach
    // verify (`git add -A` stages all non-ignored changes).
    fs::write(
        Path::new(&checkout.path).join("src").join("lib.rs"),
        "//! Candidate.\npub fn answer() -> u32 { 42 }\n",
    )
    .expect("candidate edit");
    fs::write(Path::new(&checkout.path).join("IGNORED.txt"), "secret").expect("ignored file");

    let candidate_sha = ws.commit_candidate(&checkout).expect("commit candidate");
    let verify = ws
        .prepare_verify_checkout(&checkout, &state_dir, &exec, &candidate_sha)
        .expect("verify checkout");

    assert_ne!(
        verify,
        PathBuf::from(&checkout.path),
        "verification checkout is a separate directory"
    );
    assert!(
        !verify.join("IGNORED.txt").exists(),
        "candidate untracked files never reach verify"
    );
    assert!(
        fs::read_to_string(verify.join("src").join("lib.rs"))
            .unwrap()
            .contains("42"),
        "verify checkout contains the committed candidate"
    );
    // Own git metadata and a clean detached tree.
    assert!(
        verify.join(".git").exists(),
        "verify has its own git metadata"
    );
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&verify)
        .output()
        .expect("git status");
    assert!(
        String::from_utf8_lossy(&status.stdout).trim().is_empty(),
        "verify checkout is pristine"
    );
}

#[test]
fn digest_json_contains_required_fields_and_round_trips_ledger() {
    let (scratch, id) = Scratch::new("fields", "  - sh -c 'grep -q 42 src/lib.rs'", 2);
    let out = scratch.work_once();
    assert!(out.status.success(), "green run: {}", stderr_of(&out));

    let digest = scratch.digest_json(&id);
    for field in [
        "execution_id",
        "task_id",
        "model",
        "route",
        "cash_usd",
        "quota_usd",
        "base_sha",
        "candidate_sha",
        "result_sha",
        "files_touched",
        "commands",
        "repairs",
        "cost",
        "wall_ms",
        "verdict",
    ] {
        assert!(digest.get(field).is_some(), "digest has {field}: {digest}");
    }
    let execution = format!("exec:{id}:1");
    assert_eq!(digest["execution_id"], execution);
    assert_eq!(digest["task_id"], id);
    assert_eq!(digest["verdict"], "green");
    assert_eq!(digest["result_sha"], digest["candidate_sha"]);
    assert!(
        !digest["files_touched"]
            .as_array()
            .expect("files array")
            .is_empty(),
        "candidate touched files: {digest}"
    );
    assert_eq!(digest["repairs"]["attempts"], 1);

    // The digest round-trips through the schema v2 `digests` table.
    let conn = scratch.ledger();
    let (version, digest_json): (i64, String) = conn
        .query_row(
            "SELECT version, digest_json FROM digests WHERE execution_id=?1 \
             ORDER BY version DESC LIMIT 1",
            [&execution],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("digest row");
    assert_eq!(version, 1, "first digest version");
    let stored: Value = serde_json::from_str(&digest_json).expect("stored digest json");
    assert_eq!(
        stored["summary"], digest,
        "ledger digest summary matches the artifact"
    );
    assert_eq!(stored["artifact_ref"], "artifact:digest");
}

#[test]
fn digest_cost_sums_usage_events() {
    let (scratch, id) = Scratch::new("cost", "  - sh -c 'grep -q 42 src/lib.rs'", 2);
    let out = scratch.work_once();
    assert!(out.status.success(), "green run: {}", stderr_of(&out));

    let digest = scratch.digest_json(&id);
    let execution = format!("exec:{id}:1");
    let conn = scratch.ledger();
    let (tokens_in, tokens_out, usd): (i64, i64, f64) = conn
        .query_row(
            "SELECT \
                COALESCE(SUM(CAST(json_extract(payload,'$.tokens_in') AS INTEGER)),0), \
                COALESCE(SUM(CAST(json_extract(payload,'$.tokens_out') AS INTEGER)),0), \
                COALESCE(SUM(CAST(json_extract(payload,'$.cost_usd') AS REAL)),0) \
             FROM execution_events WHERE execution_id=?1 AND kind='usage'",
            [&execution],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("usage sums");

    assert_eq!(digest["cost"]["tokens_in"].as_i64().unwrap(), tokens_in);
    assert_eq!(digest["cost"]["tokens_out"].as_i64().unwrap(), tokens_out);
    let digest_usd = digest["cost"]["usd"].as_f64().unwrap();
    assert!((digest_usd - usd).abs() < 1e-9, "usd {digest_usd} vs {usd}");
}

#[test]
fn digest_cli_prints_json_and_summary() {
    let (scratch, id) = Scratch::new("cli", "  - sh -c 'grep -q 42 src/lib.rs'", 2);
    let out = scratch.work_once();
    assert!(out.status.success(), "green run: {}", stderr_of(&out));

    let execution = format!("exec:{id}:1");
    let cli = scratch.digest_cli(&execution);
    assert!(cli.status.success(), "digest cli: {}", stderr_of(&cli));
    let text = String::from_utf8_lossy(&cli.stdout);
    assert!(
        text.contains(&execution),
        "digest names the execution: {text}"
    );
    assert!(
        text.contains("\"verdict\": \"green\""),
        "digest prints JSON: {text}"
    );
    assert!(
        text.contains("digest execution="),
        "digest prints one-line summary: {text}"
    );
}

// ---------------------------------------------------------------------------
// SF.6 protected-input refusal + deterministic property coverage
// ---------------------------------------------------------------------------

/// Missing and identity-mismatched operator inputs continue to refuse
/// repository work (SF.6 fail-closed), over the real `agalma work --once` path.
#[test]
fn repository_work_refuses_missing_or_mismatched_protected_inputs() {
    // Missing manifest: an absent operator manifest never authorizes work.
    let (missing, _id) = Scratch::new_raw("refuse-missing", "  - sh -c 'grep -q 42 src/lib.rs'", 1);
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
    let (mismatch, id) = Scratch::new_raw("refuse-mismatch", "  - sh -c 'grep -q 42 src/lib.rs'", 1);
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

/// Concrete refusal and boundary coverage over the operator manifest contract:
/// malformed/wrong-format/empty-entry/duplicate/unknown-field manifests,
/// empty/broken baselines, integrity and mode boundaries, absent-manifest load,
/// and task/repository/base identity mismatch.
#[test]
fn protected_inputs_refusal_and_boundary_coverage() {
    let sha_a = "a".repeat(40);
    let good = protected_entry("task.good", "/repo/origin", &sha_a);
    let bytes = |entries: Vec<ProtectedEntry>| {
        serde_json::to_vec(&ProtectedInputs {
            format: PROTECTED_INPUTS_FORMAT.to_string(),
            entries,
        })
        .expect("manifest bytes")
    };

    // A valid manifest parses and selects on the exact identity.
    let parsed = ProtectedInputs::parse(&bytes(vec![good.clone()])).expect("valid manifest");
    let selected = parsed
        .select("task.good", "/repo/origin", &sha_a)
        .expect("select matches");
    assert_eq!(selected, &good);
    assert_eq!(
        selected.freeze().expect("freeze"),
        good.freeze().expect("freeze"),
        "the selected entry freezes to the same canonical oracle bytes"
    );

    // Malformed / wrong-format / empty-entry / duplicate manifests refuse.
    assert!(ProtectedInputs::parse(b"not json").is_err());
    assert!(ProtectedInputs::parse(b"{}").is_err());
    let wrong_format = serde_json::to_vec(&json!({
        "format": "ProtectedInputs/v2",
        "entries": [serde_json::to_value(&good).unwrap()],
    }))
    .expect("json");
    assert!(ProtectedInputs::parse(&wrong_format).is_err());
    assert!(ProtectedInputs::parse(&bytes(Vec::new())).is_err());
    assert!(ProtectedInputs::parse(&bytes(vec![good.clone(), good.clone()])).is_err());

    // Unknown fields refuse (deny_unknown_fields).
    let mut extra = serde_json::to_value(ProtectedInputs {
        format: PROTECTED_INPUTS_FORMAT.to_string(),
        entries: vec![good.clone()],
    })
    .expect("value");
    extra["extra"] = json!(1);
    assert!(ProtectedInputs::parse(&serde_json::to_vec(&extra).unwrap()).is_err());

    // Empty identity fields refuse.
    assert!(!entry_accepts(&good, |e| e.task_id = "  ".to_string()));
    assert!(!entry_accepts(&good, |e| e.repository_root = String::new()));
    assert!(!entry_accepts(&good, |e| e.base_sha = String::new()));

    // Baseline boundaries: at least one file and one non-blank command.
    assert!(!entry_accepts(&good, |e| e.baseline.files.clear()));
    assert!(!entry_accepts(&good, |e| e.baseline.commands.clear()));
    assert!(!entry_accepts(&good, |e| e.baseline.commands =
        vec!["  ".to_string()]));

    // Baseline integrity and mode boundaries.
    assert!(!entry_accepts(&good, |e| e.baseline.files[0].digest =
        sha256_digest(b"other")));
    assert!(!entry_accepts(&good, |e| e.baseline.files[0].mode =
        "100600".to_string()));
    assert!(!entry_accepts(&good, |e| {
        let dup = e.baseline.files[0].clone();
        e.baseline.files.push(dup);
    }));

    // An absent manifest refuses load (the admission source is missing).
    let dir = ScratchDir::new("digest-manifest-absent");
    assert!(ProtectedInputs::load(&dir).is_err());

    // Identity mismatch (task / repository / base) refuses selection.
    assert!(parsed.select("task.other", "/repo/origin", &sha_a).is_err());
    assert!(parsed.select("task.good", "/repo/other", &sha_a).is_err());
    assert!(parsed
        .select("task.good", "/repo/origin", &"b".repeat(40))
        .is_err());
}

/// Deterministic generated-input property (seed `0x5f6d_f1c7_2026`, case budget
/// 256) over the operator fixture identity contract, derived independently of
/// the implementation:
/// - a generated `ProtectedEntry` freezes and re-parses unchanged;
/// - its `ProtectedInputs/v1` manifest parses;
/// - selection with the exact task/repository/base identity returns the frozen
///   entry;
/// - changing any single identity field refuses selection.
#[test]
fn protected_fixture_selection_property_over_generated_cases() {
    const SEED: u64 = 0x5f6d_f1c7_2026;
    const CASES: usize = 256;

    const TASKS: [&str; 4] = ["task.a", "task-b", "task.c", "t"];
    const REPOS: [&str; 4] = ["/repo/one", "/repo two", "/r", "/deep/nested/origin"];
    const ALLOWED: [&str; 4] = ["src", "crates", "a/b", "src/lib.rs"];
    const PROTECTED: [&str; 3] = ["tests", "docs/protected.md", "baseline"];

    let mut rng = Rng(SEED);
    let mut checked = 0usize;
    for _ in 0..CASES {
        let task_id = format!("{}.{}", TASKS[rng.below(TASKS.len() as u64) as usize], rng.below(8));
        let repository_root = REPOS[rng.below(REPOS.len() as u64) as usize].to_string();
        let base_sha = rng_hex(&mut rng, 40);
        let allowed: Vec<String> = (0..1 + rng.below(3))
            .map(|_| ALLOWED[rng.below(ALLOWED.len() as u64) as usize].to_string())
            .collect();
        let protected: Vec<String> = (0..rng.below(2))
            .map(|_| PROTECTED[rng.below(PROTECTED.len() as u64) as usize].to_string())
            .collect();
        // Distinct, valid protected file paths (uniqueness is an invariant).
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
        let inputs = ProtectedInputs::parse(
            &serde_json::to_vec(&ProtectedInputs {
                format: PROTECTED_INPUTS_FORMAT.to_string(),
                entries: vec![entry.clone()],
            })
            .expect("manifest json"),
        )
        .expect("generated manifest parses");
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
}

/// Deterministic xorshift generator (shared style with `sf_scope_guard.rs`).
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
