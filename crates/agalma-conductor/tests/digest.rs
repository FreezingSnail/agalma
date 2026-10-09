//! M1.4 acceptance runner + digest integration tests.
//!
//! Each test runs against its own scratch origin repository under
//! `target/test-runs/digest-*`: a real `git` origin with a real isolated `bd`
//! store. Model calls are replaced by scripted activity mode; the acceptance
//! command list runs deterministically via the in-process runner. No `/tmp`,
//! no network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agalma_contracts::ExecutionId;
use agalma_workspace::Workspace;
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
        let scratch = Scratch {
            origin,
            state_dir: dir.join("state"),
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

    // Every command has its own artifact, including the passing first command.
    let artifacts = scratch.run_root(&id).join("artifacts");
    let first = artifacts.join("accept@1-0.txt");
    let second = artifacts.join("accept@1-1.txt");
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
        fs::read_to_string(&first).unwrap().contains("exit: 0"),
        "first command passed"
    );
    assert!(
        fs::read_to_string(&second).unwrap().contains("MAGIC"),
        "second command captured"
    );

    // Aggregate verdict is red with the failing command listed.
    let digest = scratch.digest_json(&id);
    assert_eq!(digest["verdict"], "red");
    let commands = digest["commands"].as_array().expect("commands array");
    assert_eq!(commands.len(), 2, "two commands recorded: {digest}");
    assert_eq!(commands[0]["exit_code"], 0);
    assert_eq!(commands[1]["exit_code"], 1);
    assert!(
        commands[1]["command"]
            .as_str()
            .expect("command text")
            .contains("MAGIC"),
        "failing command captured: {digest}"
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
