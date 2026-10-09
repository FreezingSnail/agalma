//! M1.3 verifier-phase integration tests: diagnosis handoff, bounded retry,
//! recorded retry decision, per-attempt cost, and postmortem on park.
//!
//! Each test runs against its own scratch origin repository under
//! `target/test-runs/verifier-*`: a real `git` origin with a real isolated `bd`
//! store. Model calls are replaced by the conductor's scripted activity mode
//! (`AGALMA_ACTIVITY_MODE=scripted`); the acceptance command list still runs
//! deterministically via the in-process runner. No `/tmp`, no network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_agalma")
}

fn fresh(name: &str) -> PathBuf {
    let dir = root().join("target").join("test-runs").join(name);
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create run dir");
    dir
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

/// A scratch origin repo (git + isolated bd store) with one task issue.
struct Scratch {
    origin: PathBuf,
    state_dir: PathBuf,
}

impl Scratch {
    fn new(name: &str, acceptance: &str, max_attempts: u32) -> (Scratch, String) {
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
        let scratch = Scratch {
            origin,
            state_dir: dir.join("state"),
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
    // before running acceptance, so the checkout is clean and the candidate SHA
    // is what the pristine verification checkout (and integration) use. No
    // artifact/prompt leaked into the tree.
    let checkout = run_root.join("checkout");
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&checkout)
        .output()
        .expect("git status");
    assert!(
        String::from_utf8_lossy(&status.stdout).trim().is_empty(),
        "candidate tree is clean after the pre-verify commit: {}",
        String::from_utf8_lossy(&status.stdout)
    );
    assert!(
        !checkout.join("artifacts").exists(),
        "no verifier artifacts inside the checkout"
    );

    // The builder's source edit is committed on the candidate branch: the
    // candidate HEAD is one commit ahead of the base and touches src/lib.rs.
    let changed = Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", "HEAD"])
        .current_dir(&checkout)
        .output()
        .expect("git show");
    let changed = String::from_utf8_lossy(&changed.stdout).trim().to_string();
    assert!(
        changed.lines().any(|line| line.ends_with("src/lib.rs")),
        "the candidate commit carries the builder's source: {changed:?}"
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
