//! M1.3 verifier-phase integration tests: diagnosis handoff, bounded retry,
//! recorded retry decision, per-attempt cost, and postmortem on park.
//!
//! Each test runs against its own scratch origin repository under
//! `target/test-runs/verifier-*`: a real `git` origin with a real isolated `bd`
//! store. Model calls are replaced by the conductor's scripted activity mode
//! (`AGALMA_ACTIVITY_MODE=scripted`); the acceptance command list still runs
//! deterministically via the in-process runner. No `/tmp`, no network. Scratch
//! dirs auto-remove on success (kept on failure for forensics).

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
// SF.6 operator protected inputs (verifier-local fixture seeding)
//
// Repository work is fail-closed: `agalma work --once` refuses unless the
// operator `ProtectedInputs/v1` manifest (`<state_dir>/protected-inputs.json`)
// carries an entry whose task/repository/base identity matches the fixture and
// freezes a complete `ProtectedEntry` (including a valid baseline). The
// verifier fixture seeds that locally, without touching the shared
// `tests/common` helpers.
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
    fn new(name: &str, acceptance: &str, max_attempts: u32) -> (Scratch, String) {
        let (scratch, id) = Scratch::new_raw(name, acceptance, max_attempts);
        scratch.seed_protected_inputs(&id);
        (scratch, id)
    }

    /// A scratch fixture with NO operator manifest (for missing-input refusal
    /// coverage).
    fn new_raw(name: &str, acceptance: &str, max_attempts: u32) -> (Scratch, String) {
        require_bd();
        let dir = fresh(&format!("verifier-{name}"));
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
        let id = scratch.create("Verifier task", &description);
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

    fn run_root(&self, id: &str) -> PathBuf {
        self.state_dir.join("runs").join(format!("exec-{id}-1"))
    }

    fn ledger_path(&self) -> PathBuf {
        self.state_dir.join("ledger.sqlite")
    }

    /// Write an operator `ProtectedInputs/v1` manifest into the isolated state
    /// directory production reads.
    fn write_manifest(&self, entries: Vec<ProtectedEntry>) {
        write_protected_inputs(&self.state_dir, entries);
    }

    /// Seed a valid manifest whose identity matches the created task.
    fn seed_protected_inputs(&self, task_id: &str) {
        let origin = fs::canonicalize(&self.origin)
            .expect("canonical origin")
            .to_string_lossy()
            .into_owned();
        let base = Command::new("git")
            .args(["rev-parse", "main"])
            .current_dir(&self.origin)
            .output()
            .expect("git rev-parse");
        assert!(base.status.success(), "git rev-parse main: {}", stderr_of(&base));
        let base_sha = String::from_utf8_lossy(&base.stdout).trim().to_string();
        self.write_manifest(vec![protected_entry(task_id, &origin, &base_sha)]);
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
fn red_verify_produces_diagnosis_then_retries_then_parks() {
    let (scratch, id) = Scratch::new("red", "  - sh -c 'grep -q MAGIC src/lib.rs'", 2);

    let before = Command::new("git")
        .args(["rev-parse", "refs/heads/main"])
        .current_dir(&scratch.origin)
        .output()
        .expect("git rev-parse")
        .stdout;
    let before = String::from_utf8_lossy(&before).trim().to_string();

    let out = scratch.work_once();
    assert!(
        !out.status.success(),
        "--once exits nonzero on park: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        stderr_of(&out)
    );

    // Projection: parked with the attempt bound in the reason.
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

    let run_root = scratch.run_root(&id);
    let artifacts = run_root.join("artifacts");
    let prompts = run_root.join("prompts");

    // Per-attempt failure + diff artifacts (file-only handoff inputs).
    for n in 1..=2 {
        let failure = artifacts.join(format!("failure@{n}.txt"));
        assert!(failure.exists(), "failure artifact: {}", failure.display());
        let body = fs::read_to_string(&failure).expect("read failure");
        assert!(body.contains("MAGIC"), "captured failing command: {body}");

        let diff = artifacts.join(format!("diff@{n}.patch"));
        assert!(diff.exists(), "diff artifact: {}", diff.display());
        let patch = fs::read_to_string(&diff).expect("read diff");
        assert!(
            patch.contains("answer") || !patch.trim().is_empty(),
            "candidate diff is non-empty: {patch}"
        );

        let diag = artifacts.join(format!("diagnosis@{n}.md"));
        assert!(diag.exists(), "diagnosis artifact: {}", diag.display());
        let dtext = fs::read_to_string(&diag).expect("read diagnosis");
        for section in [
            "## cause",
            "## suggested fix",
            "## evidence",
            "## confidence",
        ] {
            assert!(dtext.contains(section), "diagnosis has {section}: {dtext}");
        }
    }

    // The retry builder prompt references the diagnosis artifact path (and the
    // captured failure text is still wired in).
    let builder2 = prompts.join("builder@2.md");
    assert!(builder2.exists(), "retry prompt: {}", builder2.display());
    let btext = fs::read_to_string(&builder2).expect("read retry prompt");
    assert!(
        btext.contains("diagnosis@1.md"),
        "retry prompt references the diagnosis path: {btext}"
    );
    assert!(
        btext.contains("failure@1.txt"),
        "retry prompt references the failure path: {btext}"
    );
    assert!(
        btext.contains("MAGIC"),
        "retry prompt carries failure: {btext}"
    );

    // The rendered verifier prompt names its file-only inputs.
    let verifier1 = prompts.join("verifier@1.md");
    assert!(
        verifier1.exists(),
        "verifier prompt: {}",
        verifier1.display()
    );
    let vtext = fs::read_to_string(&verifier1).expect("read verifier prompt");
    assert!(vtext.contains("diff@1.patch"), "verifier inputs: {vtext}");
    assert!(vtext.contains("failure@1.txt"), "verifier inputs: {vtext}");
    assert!(vtext.contains("diagnosis@1.md"), "verifier output: {vtext}");

    // Postmortem on park.
    let postmortem = artifacts.join("postmortem.md");
    assert!(postmortem.exists(), "postmortem: {}", postmortem.display());
    let pm = fs::read_to_string(&postmortem).expect("read postmortem");
    for needle in ["Attempts", "Verdicts", "Decisions", "Cost"] {
        assert!(pm.contains(needle), "postmortem has {needle}: {pm}");
    }

    // M1.4: verify commits the candidate working tree on the candidate branch
    // before running acceptance, so the candidate SHA is what the pristine
    // verification checkout (and integration) use. The terminal run reaps the
    // heavy checkout clone (`agalma-36y`); the digest is the surviving record of
    // the committed candidate and the source it touched.
    let digest: Value =
        serde_json::from_str(&fs::read_to_string(artifacts.join("digest.json")).expect("digest"))
            .expect("digest json");
    assert!(
        digest["candidate_sha"].as_str().is_some(),
        "candidate committed before verify: {digest}"
    );
    let touched: Vec<&str> = digest["files_touched"]
        .as_array()
        .map(|files| files.iter().filter_map(|f| f["path"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        touched.iter().any(|p| p.ends_with("src/lib.rs")),
        "the candidate commit carries the builder's source: {touched:?}"
    );
    assert!(
        !run_root.join("checkout").exists(),
        "terminal run reaps the checkout clone"
    );

    // No merge on red.
    let main = Command::new("git")
        .args(["rev-parse", "refs/heads/main"])
        .current_dir(&scratch.origin)
        .output()
        .expect("git rev-parse");
    assert_eq!(
        String::from_utf8_lossy(&main.stdout).trim(),
        before,
        "origin main unchanged on red"
    );
}

#[test]
fn retry_decisions_recorded_with_retry_then_park() {
    let (scratch, id) = Scratch::new("decisions", "  - sh -c 'grep -q MAGIC src/lib.rs'", 2);
    let out = scratch.work_once();
    assert!(!out.status.success(), "{}", stderr_of(&out));

    let conn = rusqlite::Connection::open(scratch.ledger_path()).expect("open ledger");
    let prefix = format!("op:retry:exec:{id}:1:");
    let rows: Vec<(String, String)> = conn
        .prepare("SELECT d.operation_id, o.outcome_json FROM decisions d JOIN decision_outcomes o ON o.operation_id = d.operation_id WHERE d.operation_id LIKE ?1 ORDER BY d.operation_id")
        .expect("prepare")
        .query_map(rusqlite::params![format!("{prefix}%")], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(rows.len(), 2, "one retry decision per attempt: {rows:?}");

    let attempt1 = rows
        .iter()
        .find(|(op, _)| op.ends_with(":1"))
        .map(|(_, o)| o)
        .expect("attempt 1 decision");
    let attempt2 = rows
        .iter()
        .find(|(op, _)| op.ends_with(":2"))
        .map(|(_, o)| o)
        .expect("attempt 2 decision");
    assert!(
        attempt1.contains("retry"),
        "attempt 1 chooses retry: {attempt1}"
    );
    assert!(
        attempt2.contains("park"),
        "attempt 2 (bound exhausted) chooses park: {attempt2}"
    );
}

#[test]
fn usage_events_attributed_per_attempt() {
    let (scratch, id) = Scratch::new("usage", "  - sh -c 'grep -q MAGIC src/lib.rs'", 2);
    let out = scratch.work_once();
    assert!(!out.status.success(), "{}", stderr_of(&out));

    let conn = rusqlite::Connection::open(scratch.ledger_path()).expect("open ledger");
    let exec = format!("exec:{id}:1");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM execution_events WHERE execution_id = ?1 AND kind = 'usage'",
            [&exec],
            |row| row.get(0),
        )
        .expect("count usage events");
    // build@1, verify@1 (acceptance + verifier), build@2, verify@2
    // (acceptance + verifier) => at least 6 attributed usage rows.
    assert!(count >= 6, "per-attempt usage events recorded: {count}");

    let attempts: Vec<i64> = conn
        .prepare(
            "SELECT DISTINCT CAST(json_extract(payload, '$.attempt') AS INTEGER) \
             FROM execution_events WHERE execution_id = ?1 AND kind = 'usage' ORDER BY 1",
        )
        .expect("prepare")
        .query_map(rusqlite::params![&exec], |row| row.get::<_, i64>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert!(
        attempts.contains(&1) && attempts.contains(&2),
        "attempts 1 and 2 attributed: {attempts:?}"
    );
}

/// Missing and identity-mismatched operator protected inputs continue to refuse
/// repository work over the real `agalma work --once` path (SF.6 fail-closed):
/// an absent manifest never authorizes repository work, and a present manifest
/// whose task/repository/base identity does not match the fixture refuses.
#[test]
fn verifier_work_refuses_missing_or_mismatched_protected_inputs() {
    // Missing manifest: an absent operator manifest never authorizes work.
    let (missing, _id) = Scratch::new_raw("refuse-missing", "  - sh -c 'true'", 1);
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
    let (mismatch, id) = Scratch::new_raw("refuse-mismatch", "  - sh -c 'true'", 1);
    let origin = fs::canonicalize(&mismatch.origin)
        .expect("canonical origin")
        .to_string_lossy()
        .into_owned();
    mismatch.write_manifest(vec![protected_entry(&id, &origin, &"0".repeat(40))]);
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

/// Deterministic generated-input property (seed `0x5f6_verif`, case budget 48)
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
fn verifier_protected_fixture_seeding_property_over_generated_cases() {
    // Design seed label `0x5f6_verif`; the non-hex letters are their ASCII bytes.
    const SEED: u64 = 0x5f6_7665_7269_66;
    const CASES: usize = 48;

    const TASKS: [&str; 4] = ["task.a", "task-b", "task.c", "t"];
    const REPOS: [&str; 4] = ["/repo/one", "/repo two", "/r", "/deep/nested/origin"];
    const ALLOWED: [&str; 4] = ["src", "crates", "a/b", "src/lib.rs"];
    const PROTECTED: [&str; 3] = ["tests", "docs/protected.md", "baseline"];

    let dir = fresh("verifier-fixture-property");
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

    let absent = fresh("verifier-fixture-absent");
    assert!(ProtectedInputs::load(&absent).is_err());
}
